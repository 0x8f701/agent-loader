//! OpenAI Codex (`codex-rs`) rollout JSONL adapter.
//!
//! Source baseline: `openai/codex` commit `35aaa5d9` (2026-05-01). Each rollout
//! file is a JSONL of `RolloutLine` records: a top-level `timestamp` plus a
//! flattened `RolloutItem` tagged as `{"type": ..., "payload": ...}`. The five
//! on-disk item kinds are `session_meta`, `response_item`, `compacted`,
//! `turn_context`, and `event_msg` (`protocol/src/protocol.rs:2775-2781`).
//!
//! The adapter is intentionally **forward-compatible**: known item kinds and
//! the few event/response sub-variants it cares about are strongly typed, while
//! every unrecognized field is preserved verbatim in an `extra` bag and every
//! unknown item kind degrades to an opaque `CodexItem::Unknown` that retains the
//! raw line. This keeps conversion lossless even when on-disk payloads carry
//! fields beyond the audited source (verified drift: `session_meta.payload`
//! ships a `session_id` alias absent from `SessionMeta`).
//!
//! Only the lossy `Session` contract is produced: id (meta then filename UUID),
//! cwd (last `turn_context` else meta), start timestamp, user/assistant messages
//! from `response_item` first text, and a summary/title derived from
//! `thread_name_updated` or the first user content — mirroring
//! `state/src/extract.rs` title/preview precedence.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::domain::{Message, Role, Session, SourceTool};
use crate::formats::{first_text_from_content, normalize, read_jsonl_values, summarize_messages};

/// Sentinel Codex prepends to a synthesized user message body
/// (`protocol/src/protocol.rs:108`). Stripped when deriving a lossy preview,
/// matching `state/src/extract.rs:113-118`.
const USER_MESSAGE_BEGIN: &str = "## My request for Codex:";

/// Image-only placeholder used by the native preview extractor
/// (`state/src/extract.rs:131`, `IMAGE_ONLY_USER_MESSAGE_PLACEHOLDER`).
const IMAGE_ONLY_PLACEHOLDER: &str = "[Image]";

/// A classified Codex rollout line item. Known kinds are strongly typed; the
/// `Unknown` arm preserves the raw line so forward-compat drift never loses
/// data and never aborts the parse.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexItem {
    SessionMeta(CodexSessionMeta),
    ResponseMessage(CodexResponseMessage),
    UserMessage(CodexUserMessageEvent),
    ThreadNameUpdated(CodexThreadNameUpdatedEvent),
    TurnContext(CodexTurnContext),
    Unknown {
        type_tag: Option<String>,
        raw: Value,
    },
}

/// `SessionMeta` (flattened by `SessionMetaLine`). Known required fields are
/// typed; unrecognized fields (e.g. on-disk `session_id` alias, `git`,
/// `source`, `agent_*`) are preserved in `extra`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CodexSessionMeta {
    pub id: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default)]
    pub originator: String,
    #[serde(default)]
    pub cli_version: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `TurnContextItem`. Only `cwd` is load-bearing for the adapter; the remaining
/// required/optional fields (`approval_policy`, `sandbox_policy`, `model`,
/// `summary`, `turn_id`, …) are preserved in `extra`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CodexTurnContext {
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `ResponseItem::Message`. `content` is kept opaque so the shared
/// `first_text_from_content` helper can pull the first `input_text`/
/// `output_text` block regardless of wire shape; unknown fields (`id`, `phase`)
/// land in `extra`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CodexResponseMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `EventMsg::UserMessage` — the native source of the thread preview
/// (`state/src/extract.rs:89-99`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CodexUserMessageEvent {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub images: Vec<Value>,
    #[serde(default)]
    pub local_images: Vec<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `EventMsg::ThreadNameUpdated` — overrides the thread title
/// (`state/src/extract.rs:100-106`). `thread_id` is left to `extra` so a
/// missing/renamed id never drops the title.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CodexThreadNameUpdatedEvent {
    #[serde(default)]
    pub thread_name: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Parse a Codex rollout `.jsonl` file into the lossy `Session` contract.
///
/// Known `session_meta`, `turn_context`, `response_item` message, and
/// `event_msg` `user_message`/`thread_name_updated` payloads are strongly
/// typed; all other item kinds and any known-typed payload that fails strict
/// deserialization are preserved internally as `CodexItem::Unknown` and dropped
/// from the lossy output. Malformed JSON lines are isolated by
/// `read_jsonl_values`. Archived rollout paths and empty/non-session files
/// return errors so the caller can isolate them rather than crash a listing.
pub fn parse(path: &Path) -> Result<Session> {
    // Archive scoping (`rollout/src/helpers.rs:55-60`): any path component named
    // `archived_sessions` marks an archived rollout the catalog excludes.
    if path
        .components()
        .any(|component| component.as_os_str() == "archived_sessions")
    {
        bail!("codex rollout under archived_sessions: {}", path.display());
    }

    let metadata = fs::metadata(path)
        .with_context(|| format!("reading codex rollout {}", path.display()))?;
    if !metadata.is_file() {
        bail!("codex rollout is not a regular file: {}", path.display());
    }

    let values = read_jsonl_values(path)?;
    if values.is_empty() {
        // Distinguish a genuinely empty file (`recorder.rs:860-861`) from a file
        // whose lines are all malformed/non-object (which simply yields nothing).
        let content = fs::read_to_string(path).unwrap_or_default();
        if content.trim().is_empty() {
            bail!("empty codex session file: {}", path.display());
        }
    }

    let mut session_meta: Option<CodexSessionMeta> = None;
    let mut messages: Vec<Message> = Vec::new();
    let mut last_turn_cwd: Option<PathBuf> = None;
    let mut first_user_message: Option<String> = None;
    let mut thread_title: Option<String> = None;
    let mut first_line_timestamp: Option<String> = None;

    for value in values {
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if first_line_timestamp.is_none() {
            first_line_timestamp.clone_from(&timestamp);
        }
        match classify_line(value) {
            CodexItem::SessionMeta(meta) => {
                // First SessionMeta sets the canonical thread id; later
                // forked-embedded metas with a different id are ignored
                // (`state/src/extract.rs:62-66`).
                if session_meta.is_none() {
                    session_meta = Some(meta);
                }
            }
            CodexItem::ResponseMessage(record) => {
                if let Some(message) = project_message(&record, timestamp.as_deref()) {
                    messages.push(message);
                }
            }
            CodexItem::UserMessage(user) => {
                if first_user_message.is_none() {
                    if let Some(preview) = user_message_preview(&user) {
                        first_user_message = Some(preview);
                    }
                }
            }
            CodexItem::ThreadNameUpdated(updated) => {
                if let Some(name) = updated.thread_name.as_deref() {
                    let trimmed = name.trim();
                    if !trimmed.is_empty() {
                        thread_title = Some(trimmed.to_owned());
                    }
                }
            }
            CodexItem::TurnContext(ctx) => {
                if !ctx.cwd.as_os_str().is_empty() {
                    last_turn_cwd = Some(ctx.cwd);
                }
            }
            // Unknown item kinds and known kinds that failed strict
            // deserialization are preserved as raw values and intentionally
            // dropped from the lossy `Session` contract.
            CodexItem::Unknown { .. } => {}
        }
    }

    let session_id = session_meta
        .as_ref()
        .map(|meta| meta.id.clone())
        .filter(|id| !id.is_empty())
        .or_else(|| filename_uuid(path))
        .with_context(|| {
            format!("codex rollout missing session id: {}", path.display())
        })?;

    // cwd precedence (`rollout/src/recorder.rs:1880-1903`): last TurnContext.cwd
    // wins, else SessionMeta.cwd, else the file's parent directory.
    let cwd = last_turn_cwd
        .or_else(|| session_meta.as_ref().and_then(|meta| nonempty_path(&meta.cwd)))
        .or_else(|| path.parent().map(Path::to_path_buf))
        .unwrap_or_default();

    let start_timestamp = session_meta
        .as_ref()
        .map(|meta| meta.timestamp.clone())
        .filter(|timestamp| !timestamp.is_empty())
        .or(first_line_timestamp);

    let summary = summary_for(thread_title.as_deref(), first_user_message.as_deref(), &messages);
    let modified_epoch = Some(metadata_epoch(&metadata));

    Ok(Session {
        tool: SourceTool::Codex,
        session_id,
        cwd,
        start_timestamp,
        summary,
        messages,
        path: path.to_path_buf(),
        modified_epoch,
    })
}

/// Classify a raw rollout line into a known typed item or an opaque unknown.
/// A line whose `type` names a known kind but whose payload fails strict
/// deserialization degrades to `Unknown` carrying the raw line, so the value is
/// never lost and the parse never aborts.
fn classify_line(record: Value) -> CodexItem {
    let type_tag = record
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(payload) = record.get("payload").cloned() else {
        return CodexItem::Unknown {
            type_tag,
            raw: record,
        };
    };
    match type_tag.as_deref() {
        Some("session_meta") => serde_json::from_value::<CodexSessionMeta>(payload)
            .map(CodexItem::SessionMeta)
            .unwrap_or(CodexItem::Unknown {
                type_tag,
                raw: record,
            }),
        Some("turn_context") => serde_json::from_value::<CodexTurnContext>(payload)
            .map(CodexItem::TurnContext)
            .unwrap_or(CodexItem::Unknown {
                type_tag,
                raw: record,
            }),
        Some("response_item") => classify_response_item(payload, type_tag, record),
        Some("event_msg") => classify_event_msg(payload, type_tag, record),
        _ => CodexItem::Unknown {
            type_tag,
            raw: record,
        },
    }
}

fn classify_response_item(payload: Value, type_tag: Option<String>, record: Value) -> CodexItem {
    let inner_type = payload
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match inner_type.as_deref() {
        Some("message") => serde_json::from_value::<CodexResponseMessage>(payload)
            .map(CodexItem::ResponseMessage)
            .unwrap_or(CodexItem::Unknown {
                type_tag,
                raw: record,
            }),
        _ => CodexItem::Unknown {
            type_tag,
            raw: record,
        },
    }
}

fn classify_event_msg(payload: Value, type_tag: Option<String>, record: Value) -> CodexItem {
    let inner_type = payload
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match inner_type.as_deref() {
        Some("user_message") => serde_json::from_value::<CodexUserMessageEvent>(payload)
            .map(CodexItem::UserMessage)
            .unwrap_or(CodexItem::Unknown {
                type_tag,
                raw: record,
            }),
        Some("thread_name_updated") => {
            serde_json::from_value::<CodexThreadNameUpdatedEvent>(payload)
                .map(CodexItem::ThreadNameUpdated)
                .unwrap_or(CodexItem::Unknown {
                    type_tag,
                    raw: record,
                })
        }
        _ => CodexItem::Unknown {
            type_tag,
            raw: record,
        },
    }
}

/// Project a typed response-item message into the lossy `Message` contract via
/// the shared first-text helper. Non-user/assistant roles and content without a
/// text block yield `None`.
fn project_message(record: &CodexResponseMessage, timestamp: Option<&str>) -> Option<Message> {
    let role = record.role.parse::<Role>().ok()?;
    let text = first_text_from_content(&record.content)?.to_owned();
    Some(Message {
        role,
        text,
        timestamp: timestamp.map(str::to_owned),
    })
}

/// Native lossy preview for an `EventMsg::UserMessage`
/// (`state/src/extract.rs:120-133`): strip the `USER_MESSAGE_BEGIN` sentinel,
/// fall back to the image-only placeholder when the body is empty but images
/// are present, else `None`.
fn user_message_preview(user: &CodexUserMessageEvent) -> Option<String> {
    let stripped = strip_user_message_prefix(&user.message);
    if !stripped.is_empty() {
        return Some(stripped.to_owned());
    }
    if !user.images.is_empty() || !user.local_images.is_empty() {
        return Some(IMAGE_ONLY_PLACEHOLDER.to_owned());
    }
    None
}

fn strip_user_message_prefix(text: &str) -> &str {
    match text.find(USER_MESSAGE_BEGIN) {
        Some(idx) => text[idx + USER_MESSAGE_BEGIN.len()..].trim(),
        None => text.trim(),
    }
}

/// Build the lossy summary. Precedence mirrors `state/src/extract.rs`:
/// `thread_name_updated` overrides, else the first `user_message` preview, else
/// the shared message summary, finally `(no summary)`.
fn summary_for(
    thread_title: Option<&str>,
    first_user_message: Option<&str>,
    messages: &[Message],
) -> String {
    if let Some(title) = thread_title.filter(|value| !value.trim().is_empty()) {
        return normalize(title, 100);
    }
    if let Some(preview) = first_user_message.filter(|value| !value.is_empty()) {
        return normalize(preview, 100);
    }
    summarize_messages(messages)
}

fn nonempty_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path.to_path_buf())
    }
}

/// Parse the trailing UUID out of a `rollout-<YYYY-MM-DD>T<hh-mm-ss>-<uuid>.jsonl`
/// filename by scanning `-` indices from the right and probing each growing
/// suffix as a UUID (`rollout/src/list.rs:926-937`).
fn filename_uuid(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let mut search_end = stem.len();
    while let Some(dash) = stem[..search_end].rfind('-') {
        let suffix = &stem[dash + 1..];
        if Uuid::parse_str(suffix).is_ok() {
            return Some(suffix.to_owned());
        }
        search_end = dash;
    }
    None
}

#[cfg(unix)]
fn metadata_epoch(metadata: &fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0
}

#[cfg(not(unix))]
fn metadata_epoch(metadata: &fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Role;
    use std::fs as stdfs;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn session_file(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("temp file");
        for line in lines {
            writeln!(file, "{line}").expect("write line");
        }
        file.flush().expect("flush");
        file
    }

    /// Name a temp file like a real rollout so filename-UUID extraction is
    /// exercised against the on-disk grammar. The returned `TempDir` owns the
    /// file's parent directory and must be kept alive (bound to `_dir`) for the
    /// test's duration so the path stays valid.
    fn rollout_named_file(uuid: &str, lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = format!("rollout-2026-07-29T14-04-37-{uuid}.jsonl");
        let path = dir.path().join(name);
        let mut file = stdfs::File::create(&path).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write line");
        }
        file.flush().expect("flush");
        drop(file);
        (dir, path)
    }

    const META: &str = r#"{"timestamp":"2026-07-29T06:04:37.826Z","type":"session_meta","payload":{"id":"11111111-1111-4111-8111-111111111111","session_id":"11111111-1111-4111-8111-111111111111","timestamp":"2026-07-29T06:04:37.826Z","cwd":"/workspace/project","originator":"codex-tui","source":"cli","cli_version":"sessions-convert","model_provider":"cliproxy"}}"#;
    const USER_RESPONSE: &str = r#"{"timestamp":"2026-07-29T06:04:38.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#;
    const ASSISTANT_RESPONSE: &str = r#"{"timestamp":"2026-07-29T06:04:39.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello there!"}]}}"#;

    #[test]
    fn session_id_alias_drift_preserved_and_id_used() {
        // On-disk session_meta carries a `session_id` alias absent from the
        // audited SessionMeta struct. The typed `id` is the canonical thread id;
        // the alias is preserved in `extra` rather than silently dropped.
        let meta: CodexSessionMeta = serde_json::from_str(
            r#"{"id":"11111111-1111-4111-8111-111111111111","session_id":"11111111-1111-4111-8111-111111111111","timestamp":"2026-07-29T06:04:37.826Z","cwd":"/tmp","originator":"codex-tui","cli_version":"x"}"#,
        )
        .expect("deserialize meta");
        assert_eq!(meta.id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(
            meta.extra.get("session_id").and_then(Value::as_str),
            Some("11111111-1111-4111-8111-111111111111")
        );

        // parse() must surface the typed `id`, never the alias, as session_id.
        let file = session_file(&[META, USER_RESPONSE]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.tool, SourceTool::Codex);
        assert_eq!(session.session_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(session.cwd, PathBuf::from("/workspace/project"));
        assert_eq!(
            session.start_timestamp.as_deref(),
            Some("2026-07-29T06:04:37.826Z")
        );
        assert_eq!(session.path, file.path());
    }

    #[test]
    fn malformed_lines_are_isolated() {
        let file = session_file(&[
            "this is not json",
            META,
            "   ",
            "[1,2,3]",
            "null",
            USER_RESPONSE,
            "{not valid",
            ASSISTANT_RESPONSE,
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text, "hi");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].text, "Hello there!");
        assert_eq!(
            session.messages[0].timestamp.as_deref(),
            Some("2026-07-29T06:04:38.000Z")
        );
    }

    #[test]
    fn unknown_top_level_item_kind_is_skipped() {
        let future = r#"{"timestamp":"2026-07-29T06:04:40.000Z","type":"future_kind","payload":{"anything":true}}"#;
        let file = session_file(&[META, future, USER_RESPONSE]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].text, "hi");
    }

    #[test]
    fn unknown_event_msg_variant_is_skipped() {
        let unknown_event = r#"{"timestamp":"2026-07-29T06:04:41.000Z","type":"event_msg","payload":{"type":"future_event","detail":"opaque"}}"#;
        let file = session_file(&[META, unknown_event, USER_RESPONSE]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.summary, "hi");
    }

    #[test]
    fn filename_uuid_extracted_when_no_meta() {
        let uuid = "01900000-0000-7000-8000-000000000001";
        let (_dir, path) = rollout_named_file(uuid, &[USER_RESPONSE, ASSISTANT_RESPONSE]);
        let session = parse(&path).expect("parse");
        assert_eq!(session.session_id, uuid);
        assert_eq!(session.messages.len(), 2);
        // No meta and no turn_context: cwd falls back to the file's parent.
        assert_eq!(session.cwd, path.parent().unwrap().to_path_buf());
        assert_eq!(
            session.start_timestamp.as_deref(),
            Some("2026-07-29T06:04:38.000Z")
        );
    }

    #[test]
    fn filename_uuid_right_scan_parses_v7() {
        // Right-scan must skip the date/time dash segments and land on the full
        // UUID suffix (`rollout/src/list.rs:926`).
        let uuid = "01900000-0000-7000-8000-000000000001";
        let (_dir, path) = rollout_named_file(uuid, &[]);
        assert_eq!(filename_uuid(&path), Some(uuid.to_owned()));
    }

    #[test]
    fn content_first_text_block_extracted() {
        let msg = r#"{"timestamp":"2026-07-29T06:04:42.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"first answer"},{"type":"output_text","text":"second answer"}]}}"#;
        let file = session_file(&[META, msg]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].text, "first answer");
    }

    #[test]
    fn cwd_last_turn_context_wins_over_meta() {
        let turn = r#"{"timestamp":"2026-07-29T06:04:38.000Z","type":"turn_context","payload":{"cwd":"/srv/late","approval_policy":"never","sandbox_policy":"read-only","model":"gpt-5","summary":"auto"}}"#;
        let file = session_file(&[META, turn, USER_RESPONSE]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.cwd, PathBuf::from("/srv/late"));
    }

    #[test]
    fn thread_name_update_overrides_summary() {
        let user_event = r#"{"timestamp":"2026-07-29T06:04:38.000Z","type":"event_msg","payload":{"type":"user_message","message":"do thing","images":[],"local_images":[],"text_elements":[]}}"#;
        let title = r#"{"timestamp":"2026-07-29T06:04:45.000Z","type":"event_msg","payload":{"type":"thread_name_updated","thread_id":"11111111-1111-4111-8111-111111111111","thread_name":"Refactor parser"}}"#;
        let file = session_file(&[META, user_event, title, USER_RESPONSE]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "Refactor parser");
    }

    #[test]
    fn summary_falls_back_to_first_user_message_event() {
        // No thread_name_updated: the first user_message preview drives the
        // summary, with the sentinel stripped (`extract.rs:113-127`).
        let user_event = r###"{"timestamp":"2026-07-29T06:04:38.000Z","type":"event_msg","payload":{"type":"user_message","message":"## My request for Codex: actual user request","images":[],"local_images":[],"text_elements":[]}}"###;
        let file = session_file(&[META, user_event, USER_RESPONSE]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "actual user request");
    }

    #[test]
    fn summary_falls_back_to_response_message_when_no_events() {
        let file = session_file(&[META, USER_RESPONSE, ASSISTANT_RESPONSE]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "hi");
    }

    #[test]
    fn empty_file_errors() {
        let file = NamedTempFile::new().expect("temp file");
        let error = parse(file.path()).expect_err("empty file should error");
        let msg = format!("{error:#}");
        assert!(msg.contains("empty codex session file"), "got: {msg}");
    }

    #[test]
    fn archived_path_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archived = dir.path().join("archived_sessions").join("2026");
        stdfs::create_dir_all(&archived).expect("mkdir");
        let path = archived.join("rollout-2026-07-29T14-04-37-deadbeef.jsonl");
        stdfs::write(&path, META).expect("write");
        let error = parse(&path).expect_err("archived should error");
        let msg = format!("{error:#}");
        assert!(msg.contains("archived_sessions"), "got: {msg}");
    }

    #[test]
    fn forked_embedded_meta_with_different_id_ignored() {
        let forked = r#"{"timestamp":"2026-07-29T06:05:00.000Z","type":"session_meta","payload":{"id":"deadbeef-0000-0000-0000-000000000000","timestamp":"2026-07-29T06:05:00.000Z","cwd":"/elsewhere","originator":"codex-tui","cli_version":"x"}}"#;
        let file = session_file(&[META, forked, USER_RESPONSE]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(session.cwd, PathBuf::from("/workspace/project"));
    }

    #[test]
    fn image_only_user_message_preview() {
        let user = CodexUserMessageEvent {
            message: String::new(),
            images: vec![Value::String("https://img".to_owned())],
            local_images: Vec::new(),
            extra: Map::new(),
        };
        assert_eq!(user_message_preview(&user).as_deref(), Some("[Image]"));
    }

    #[test]
    fn classify_routes_known_and_unknown() {
        let meta = classify_line(serde_json::from_str(META).unwrap());
        assert!(matches!(meta, CodexItem::SessionMeta(_)));

        let turn = classify_line(
            serde_json::from_str(
                r#"{"timestamp":"t","type":"turn_context","payload":{"cwd":"/x"}}"#,
            )
            .unwrap(),
        );
        assert!(matches!(turn, CodexItem::TurnContext(_)));

        let event = classify_line(
            serde_json::from_str(
                r#"{"timestamp":"t","type":"event_msg","payload":{"type":"future_event"}}"#,
            )
            .unwrap(),
        );
        assert!(matches!(event, CodexItem::Unknown { .. }));

        let top = classify_line(
            serde_json::from_str(r#"{"timestamp":"t","type":"brand_new","payload":{}}"#).unwrap(),
        );
        assert!(matches!(
            top,
            CodexItem::Unknown { type_tag, .. } if type_tag.as_deref() == Some("brand_new")
        ));
    }
}