use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::domain::{Message, Role, Session, SourceTool};
use crate::formats::{content_parts, normalize, parsed_message, summarize_messages};
use crate::fs::{open_directory_under_root, open_regular_file_at};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrokSummary {
    pub info: GrokSummaryInfo,
    #[serde(default)]
    pub session_summary: String,
    #[serde(default)]
    pub generated_title: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub last_active_at: String,
    #[serde(default)]
    pub num_messages: u64,
    #[serde(default)]
    pub num_chat_messages: u64,
    #[serde(default)]
    pub current_model_id: String,
    #[serde(default)]
    pub chat_format_version: u32,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub sandbox_profile: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub session_kind: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub forked_at: Option<String>,
    #[serde(default)]
    pub fork_context_source: Option<String>,
    #[serde(default)]
    pub fork_parent_prompt_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GrokSummaryInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrokPromptContext {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub prompt_mode: String,
    #[serde(default)]
    pub audience: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub working_directory: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub fn parse(path: &Path, grok_root: &Path) -> Result<Session> {
    let session_directory = path
        .parent()
        .with_context(|| format!("Grok summary has no parent: {}", path.display()))?;
    let directory = open_directory_under_root(session_directory, grok_root).with_context(|| {
        format!(
            "opening Grok session directory {}",
            session_directory.display()
        )
    })?;
    let (mut summary_file, summary_metadata) = open_regular_file_at(&directory, "summary.json")
        .with_context(|| format!("opening Grok summary {}", path.display()))?;
    let mut summary_json = String::new();
    summary_file
        .read_to_string(&mut summary_json)
        .with_context(|| format!("reading Grok summary {}", path.display()))?;
    let summary: GrokSummary = serde_json::from_str(&summary_json)
        .with_context(|| format!("parsing Grok summary {}", path.display()))?;

    let mut messages = Vec::new();
    if let Some((chat_file, _)) = open_regular_file_at(&directory, "chat_history.jsonl") {
        parse_chat_history(chat_file, &mut messages);
    }
    if messages.is_empty() {
        if let Some((updates_file, _)) = open_regular_file_at(&directory, "updates.jsonl") {
            parse_updates(updates_file, &mut messages);
        }
    }

    let session_id = if summary.info.id.is_empty() {
        session_directory
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_owned()
    } else {
        summary.info.id.clone()
    };
    let cwd: PathBuf = if summary.info.cwd.is_empty() {
        decode_cwd_fallback(session_directory, grok_root)
            .unwrap_or_default()
            .into()
    } else {
        summary.info.cwd.clone().into()
    };
    let start_timestamp = [
        summary.created_at.as_str(),
        summary.updated_at.as_str(),
        summary.last_active_at.as_str(),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .map(str::to_owned);
    let summary_text = [
        summary.generated_title.as_str(),
        summary.session_summary.as_str(),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .map(|value| normalize(value, 100))
    .unwrap_or_else(|| summarize_messages(&messages));

    Ok(Session {
        tool: SourceTool::Grok,
        session_id,
        cwd,
        start_timestamp,
        summary: summary_text,
        messages,
        path: path.to_path_buf(),
        modified_epoch: Some(metadata_epoch(&summary_metadata)),
        hints: crate::domain::SessionHints {
            model: (!summary.current_model_id.is_empty()).then(|| summary.current_model_id.clone()),
            thinking_level: summary
                .reasoning_effort
                .as_deref()
                .and_then(|value| value.parse().ok()),
            session_kind: summary.session_kind.clone().filter(|kind| !kind.is_empty()),
            hidden: summary.hidden,
            ..crate::domain::SessionHints::default()
        },
    })
}

fn grok_history_message(
    object: &serde_json::Map<String, Value>,
    record_type: Option<&str>,
    timestamp: Option<String>,
) -> Option<Message> {
    let content = object.get("content");
    match record_type {
        Some("reasoning") => {
            let text = ["content", "summary"].into_iter().find_map(|key| {
                object.get(key).and_then(|value| {
                    first_text_like(value).or_else(|| {
                        let joined = crate::domain::joined_text(&content_parts(value));
                        (!joined.is_empty()).then_some(joined)
                    })
                })
            })?;
            Some(Message::from_parts(
                Role::Assistant,
                vec![crate::domain::ContentPart::Thinking {
                    text,
                    signature: None,
                }],
                timestamp,
            ))
        }
        Some("tool" | "tool_call") => Some(Message::from_parts(
            Role::Assistant,
            vec![crate::domain::ContentPart::ToolUse {
                id: object
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                name: object
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                input: crate::formats::object_or_json(
                    object
                        .get("arguments")
                        .or_else(|| object.get("input"))
                        .or(content)
                        .cloned()
                        .unwrap_or(serde_json::json!({})),
                ),
            }],
            timestamp,
        )),
        Some("tool_result") => {
            let mut parts = vec![crate::domain::ContentPart::ToolResult {
                tool_use_id: object
                    .get("tool_call_id")
                    .or_else(|| object.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                content: content.and_then(first_text_like).unwrap_or_default(),
                is_error: object
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }];
            if let Some(content) = content {
                parts.extend(
                    content_parts(content)
                        .into_iter()
                        .filter(|part| matches!(part, crate::domain::ContentPart::Image { .. })),
                );
            }
            if let Some(Value::Array(images)) = object.get("images") {
                parts.extend(
                    images
                        .iter()
                        .flat_map(content_parts)
                        .filter(|part| matches!(part, crate::domain::ContentPart::Image { .. })),
                );
            }
            Some(Message::from_parts(Role::Tool, parts, timestamp))
        }
        Some("backend_tool_call") => backend_tool_call(object, timestamp),
        _ => {
            let role = object.get("role").and_then(Value::as_str).or(record_type);
            let mut message = match content
                .and_then(|value| parsed_message(role, Some(value), timestamp.as_deref()))
            {
                Some(message) => message,
                None => Message::from_parts(
                    role.and_then(|value| value.parse().ok())?,
                    Vec::new(),
                    timestamp,
                ),
            };
            if let Some(Value::Array(calls)) = object.get("tool_calls") {
                for call in calls {
                    message.parts.push(crate::domain::ContentPart::ToolUse {
                        id: call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        name: call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        input: crate::formats::object_or_json(
                            call.get("arguments")
                                .or_else(|| call.get("input"))
                                .cloned()
                                .unwrap_or(serde_json::json!({})),
                        ),
                    });
                }
                message.text = crate::domain::joined_text(&message.parts);
            }
            (!message.parts.is_empty()).then_some(message)
        }
    }
}

fn backend_tool_call(
    object: &serde_json::Map<String, Value>,
    timestamp: Option<String>,
) -> Option<Message> {
    let kind = object.get("kind").and_then(Value::as_object);
    let name = kind
        .and_then(|kind| kind.get("tool_type"))
        .or_else(|| object.get("name"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("backend_tool");
    let id = kind
        .and_then(|kind| kind.get("id"))
        .or_else(|| object.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let input = kind
        .and_then(|kind| kind.get("action"))
        .cloned()
        .or_else(|| object.get("kind").cloned())
        .unwrap_or(serde_json::json!({}));
    Some(Message::from_parts(
        Role::Assistant,
        vec![crate::domain::ContentPart::ToolUse {
            id,
            name: name.to_owned(),
            input: crate::formats::object_or_json(input),
        }],
        timestamp,
    ))
}

fn first_text_like(content: &Value) -> Option<String> {
    match content {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => {
            let joined = crate::domain::joined_text(&content_parts(content));
            if joined.is_empty() {
                crate::formats::first_text_from_content(content).map(str::to_owned)
            } else {
                Some(joined)
            }
        }
    }
}

fn parse_updates(file: File, messages: &mut Vec<Message>) {
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(update) = record.pointer("/params/update") else {
            continue;
        };
        if let Some(message) = grok_update_message(update) {
            messages.push(message);
        }
    }
}

fn grok_update_message(update: &Value) -> Option<Message> {
    let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
    let timestamp = update
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match kind {
        "user_message_chunk" => {
            parsed_message(Some("user"), update.get("content"), timestamp.as_deref())
        }
        "agent_message_chunk" => parsed_message(
            Some("assistant"),
            update.get("content"),
            timestamp.as_deref(),
        ),
        "agent_thought_chunk" => first_text_like(update.get("content")?).map(|text| {
            Message::from_parts(
                Role::Assistant,
                vec![crate::domain::ContentPart::Thinking {
                    text,
                    signature: None,
                }],
                timestamp,
            )
        }),
        "tool_call" => {
            let id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = update.get("title").and_then(Value::as_str).unwrap_or("");
            let status = update.get("status").and_then(Value::as_str);
            if matches!(status, Some("completed" | "failed")) {
                Some(Message::from_parts(
                    Role::Tool,
                    vec![crate::domain::ContentPart::ToolResult {
                        tool_use_id: id.to_owned(),
                        content: update
                            .get("content")
                            .and_then(first_text_like)
                            .unwrap_or_default(),
                        is_error: status == Some("failed"),
                    }],
                    timestamp,
                ))
            } else {
                Some(Message::from_parts(
                    Role::Assistant,
                    vec![crate::domain::ContentPart::ToolUse {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        input: update
                            .get("rawInput")
                            .cloned()
                            .unwrap_or(serde_json::json!({})),
                    }],
                    timestamp,
                ))
            }
        }
        _ => None,
    }
}

fn decode_cwd_fallback(session_directory: &Path, grok_root: &Path) -> Option<String> {
    let cwd_directory = session_directory.parent()?;
    let encoded = cwd_directory.file_name()?.to_str()?;
    let decoded = percent_decode_str(encoded).decode_utf8().ok()?.into_owned();
    if decoded.starts_with('/') || (cfg!(windows) && decoded.chars().nth(1) == Some(':')) {
        return Some(decoded);
    }
    let directory = open_directory_under_root(cwd_directory, grok_root).ok()?;
    let (mut cwd_file, _) = open_regular_file_at(&directory, ".cwd")?;
    let mut cwd = String::new();
    cwd_file.read_to_string(&mut cwd).ok()?;
    let cwd = cwd.trim();
    (!cwd.is_empty()).then(|| cwd.to_owned())
}

fn parse_chat_history(file: File, messages: &mut Vec<Message>) {
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(object) = record.as_object() else {
            continue;
        };
        let record_type = object.get("type").and_then(Value::as_str);
        if record_type == Some("system") {
            continue;
        }
        let timestamp = ["timestamp", "created_at", "updated_at", "time"]
            .into_iter()
            .find_map(|key| object.get(key).and_then(Value::as_str))
            .map(str::to_owned);
        if let Some(message) = grok_history_message(object, record_type, timestamp) {
            messages.push(message);
        }
    }
}

#[cfg(unix)]
fn metadata_epoch(metadata: &std::fs::Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0
}

#[cfg(not(unix))]
fn metadata_epoch(metadata: &std::fs::Metadata) -> f64 {
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
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use crate::domain::Role;

    /// Build `<root>/<encoded>/<id>/summary.json` (+ chat_history.jsonl) and return the
    /// summary path. `root` must be absolute; `encoded` must be a single path component.
    fn write_session(root: &Path, encoded: &str, id: &str, summary: &str, chat: &str) -> PathBuf {
        let dir = root.join(encoded).join(id);
        fs::create_dir_all(&dir).expect("create session dir");
        let summary_path = dir.join("summary.json");
        fs::write(&summary_path, summary).expect("write summary");
        if !chat.is_empty() {
            fs::write(dir.join("chat_history.jsonl"), chat).expect("write chat history");
        }
        summary_path
    }

    #[test]
    fn info_cwd_is_authoritative_over_encoded_dir_name() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        let summary = r#"{"info":{"id":"sid","cwd":"/tmp/right-cwd"},"session_summary":"t","created_at":"2026-01-01T00:00:00Z"}"#;
        // Encoded dir name intentionally disagrees with info.cwd.
        let path = write_session(&root, "%2Ftmp%2Fwrong-cwd", "sid", summary, "");
        let session = parse(&path, &root).expect("parse");
        assert_eq!(session.cwd, PathBuf::from("/tmp/right-cwd"));
        assert_eq!(session.session_id, "sid");
    }

    #[test]
    fn updates_are_used_when_chat_history_is_empty() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        let summary = r#"{"info":{"id":"sid","cwd":"/tmp/grok"},"session_summary":"t","created_at":"2026-01-01T00:00:00Z"}"#;
        let path = write_session(&root, "%2Ftmp%2Fgrok", "sid", summary, "");
        fs::write(
            path.parent().unwrap().join("updates.jsonl"),
            concat!(
                r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"list the file"}}}}"#,
                "\n",
                r#"{"method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"I should read it"}}}}"#,
                "\n",
                r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"Read","status":"pending","rawInput":{"path":"src/lib.rs"}}}}"#,
                "\n",
                r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"call-1","status":"completed","content":{"type":"text","text":"fn main() {}"}}}}"#,
                "\n",
            ),
        )
        .expect("write updates");
        let session = parse(&path, &root).expect("parse");
        assert!(
            session
                .messages
                .iter()
                .any(|message| message.text.contains("list the file")),
            "{:?}",
            session
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
        );
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(part, crate::domain::ContentPart::Thinking { text, .. } if text.contains("I should read it"))
            })
        }));
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(part, crate::domain::ContentPart::ToolUse { name, .. } if name == "Read")
            })
        }));
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(part, crate::domain::ContentPart::ToolResult { content, .. } if content.contains("fn main()"))
            })
        }));
    }

    #[test]
    fn empty_info_cwd_falls_back_to_percent_decoded_dir_name() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        // No info.cwd -> must reverse percent-decode the encoded-cwd component.
        let summary =
            r#"{"info":{"id":"sid"},"session_summary":"t","created_at":"2026-01-01T00:00:00Z"}"#;
        let path = write_session(&root, "%2Ftmp%2Fdecoded-cwd", "sid", summary, "");
        let session = parse(&path, &root).expect("parse");
        assert_eq!(session.cwd, PathBuf::from("/tmp/decoded-cwd"));
    }

    #[test]
    fn slug_directory_falls_back_to_cwd_sidecar() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        let summary =
            r#"{"info":{"id":"sid"},"session_summary":"t","created_at":"2026-01-01T00:00:00Z"}"#;
        let path = write_session(&root, "workspace-1234567890abcdef", "sid", summary, "");
        fs::write(
            path.parent().unwrap().parent().unwrap().join(".cwd"),
            "/real/long/workspace",
        )
        .expect("write cwd sidecar");
        let session = parse(&path, &root).expect("parse");
        assert_eq!(session.cwd, PathBuf::from("/real/long/workspace"));
    }

    #[test]
    fn parse_rejects_path_outside_grok_root() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        let outside = TempDir::new().expect("outside root");
        let dir = outside.path().join("enc/sid");
        fs::create_dir_all(&dir).expect("create outside session dir");
        let path = dir.join("summary.json");
        fs::write(&path, r#"{"info":{"id":"sid","cwd":"/tmp"}}"#).expect("write summary");
        assert!(parse(&path, &root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_summary_file_fails_closed() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        let dir = root.join("enc/sid");
        fs::create_dir_all(&dir).expect("create session dir");
        // Replace summary.json with a symlink to a regular file outside the dir.
        let target = home.path().join("payload.txt");
        fs::write(&target, "payload").expect("write payload");
        let path = dir.join("summary.json");
        symlink(&target, &path).expect("symlink summary");
        assert!(parse(&path, &root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_path_component_fails_closed() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        // Real session dir living outside root, linked in by a symlink component.
        let real = home.path().join("real-enc/sid");
        fs::create_dir_all(&real).expect("create real session dir");
        fs::write(
            real.join("summary.json"),
            r#"{"info":{"id":"sid","cwd":"/tmp"}}"#,
        )
        .expect("write summary");
        let link = root.join("link-enc");
        symlink(home.path().join("real-enc"), &link).expect("symlink component");
        let path = link.join("sid/summary.json");
        assert!(parse(&path, &root).is_err());
    }

    #[test]
    fn chat_history_keeps_only_user_and_assistant_first_text() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        let summary = r#"{"info":{"id":"sid","cwd":"/tmp/grok"},"session_summary":"t","created_at":"2026-01-01T00:00:00Z"}"#;
        let chat = concat!(
            r#"{"type":"system","content":"system prompt"}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"hello user"}]}"#,
            "\n",
            r#"{"type":"assistant","content":"hi assistant","tool_calls":[{"id":"call-1","name":"WebSearch","arguments":"{}"}],"model_id":"m"}"#,
            "\n",
            r#"{"type":"reasoning","id":"rs_1","status":"completed","summary":[{"type":"summary_text","text":"thinking"}],"encrypted_content":"abc"}"#,
            "\n",
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"result"}"#,
            "\n",
            r#"{not valid json"#,
            "\n",
            r#"{"type":"user","content":[{"type":"image","url":"x"},{"type":"text","text":"second text"}]}"#,
            "\n",
            r#"{"type":"assistant","content":""}"#,
            "\n",
        );
        let path = write_session(&root, "%2Ftmp%2Fgrok", "sid", summary, chat);
        let session = parse(&path, &root).expect("parse");
        assert_eq!(
            session
                .messages
                .iter()
                .map(|m| (m.role, m.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (Role::User, "hello user"),
                (Role::Assistant, "hi assistant\nWebSearch"),
                (Role::Assistant, "thinking"),
                (Role::Tool, "result"),
                (Role::User, "[Image]\nsecond text"),
            ]
        );
    }

    #[test]
    fn backend_tool_call_and_tool_result_images_are_projected() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        let summary = r#"{"info":{"id":"sid","cwd":"/tmp/grok"},"session_summary":"t","created_at":"2026-01-01T00:00:00Z"}"#;
        let chat = concat!(
            r#"{"type":"user","content":[{"type":"text","text":"search it"}]}"#,
            "\n",
            r#"{"type":"backend_tool_call","kind":{"tool_type":"web_search","id":"ws_1","action":{"type":"search","query":"capybaras"}}}"#,
            "\n",
            r#"{"type":"tool_result","tool_call_id":"ws_1","content":"found","images":[{"type":"image","url":"iVBORw0KGgo="}]}"#,
            "\n",
        );
        let path = write_session(&root, "%2Ftmp%2Fgrok", "sid", summary, chat);
        let session = parse(&path, &root).expect("parse");
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(part, crate::domain::ContentPart::ToolUse { id, name, input } if id == "ws_1" && name == "web_search" && input.get("query").and_then(Value::as_str) == Some("capybaras"))
            })
        }));
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(part, crate::domain::ContentPart::ToolResult { content, .. } if content == "found")
            }) && message.parts.iter().any(|part| {
                matches!(part, crate::domain::ContentPart::Image { data, .. } if data == "iVBORw0KGgo=")
            })
        }));
    }

    #[test]
    fn unknown_summary_fields_are_retained_and_round_trip() {
        let home = TempDir::new().expect("temp home");
        let root = home.path().join(".grok/sessions");
        fs::create_dir_all(&root).expect("create grok root");
        let raw = r#"{"info":{"id":"sid","cwd":"/tmp/grok","future_info":"x"},"session_summary":"t","created_at":"2026-01-01T00:00:00Z","current_model_id":"m","chat_format_version":1,"agent_name":"researcher","sandbox_profile":"off","session_kind":"subagent_resume","parent_session_id":"p","forked_at":"2026-01-01T00:00:00Z","fork_context_source":"resumed","reasoning_effort":"high","future_field":{"nested":true}}"#;
        let path = write_session(&root, "%2Ftmp%2Fgrok", "sid", raw, "");
        // parse must succeed with unknown fields present.
        let session = parse(&path, &root).expect("parse");
        assert_eq!(session.session_id, "sid");
        assert_eq!(session.cwd, PathBuf::from("/tmp/grok"));
        assert_eq!(
            session.hints.session_kind.as_deref(),
            Some("subagent_resume")
        );
        assert!(session.is_catalog_child());

        // Re-deserialize the same bytes through the typed struct to prove the
        // flatten maps capture unknown fields verbatim and typed fields parse.
        let parsed: GrokSummary = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(parsed.agent_name.as_deref(), Some("researcher"));
        assert_eq!(parsed.sandbox_profile.as_deref(), Some("off"));
        assert_eq!(parsed.session_kind.as_deref(), Some("subagent_resume"));
        assert_eq!(parsed.fork_context_source.as_deref(), Some("resumed"));
        assert_eq!(parsed.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(parsed.parent_session_id.as_deref(), Some("p"));
        assert_eq!(
            parsed.info.extra.get("future_info"),
            Some(&Value::String("x".to_owned()))
        );
        assert_eq!(
            parsed.extra.get("future_field"),
            Some(&serde_json::json!({"nested": true}))
        );
        // Round-trip: re-serialize and re-parse; the unknown field must survive.
        let reserialized = serde_json::to_string(&parsed).expect("serialize");
        let reparsed: GrokSummary = serde_json::from_str(&reserialized).expect("re-deserialize");
        assert_eq!(
            reparsed.extra.get("future_field"),
            Some(&serde_json::json!({"nested": true}))
        );
    }
}
