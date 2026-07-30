use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::domain::{FlexibleRecord, Message, Session, SourceTool};
use crate::formats::{normalize, parsed_message, read_jsonl_values, summarize_messages};

/// Droid session-start record: strongly typed known fields with the remaining
/// fields preserved verbatim in `extra`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DroidSessionStart {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "sessionTitle")]
    pub session_title: Option<String>,
    /// All other session-start fields (owner, version, hostId, auto-title
    /// stage, etc.) are captured raw so the record round-trips losslessly.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The nested `message` payload of a Droid message record.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DroidMessagePayload {
    #[serde(default)]
    pub role: String,
    /// Kept as a raw `Value` so the shared `first_text_from_content` helper can
    /// project the lossy first text without losing structural fidelity.
    #[serde(default)]
    pub content: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A Droid message record: the timestamp lives at the top level while the
/// role/content live under the nested `message` object.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DroidMessage {
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub message: Option<DroidMessagePayload>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Enum-dispatched known Droid record kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum DroidRecord {
    SessionStart(DroidSessionStart),
    Message(DroidMessage),
}

/// The literal placeholder title Droid assigns before a real title is derived.
const PLACEHOLDER_TITLE: &str = "New Session";

/// Parse a Droid session `.jsonl` file into the lossy `Session` contract.
///
/// Known `session_start` and `message` records are strongly typed; every other
/// record kind (and any known-typed record that fails strict deserialization)
/// is preserved internally as a raw `FlexibleRecord::Unknown` and dropped from
/// the lossy output. Malformed lines are isolated by `read_jsonl_values`.
pub fn parse(path: &Path) -> Result<Session> {
    let values = read_jsonl_values(path)?;
    let modified_epoch = fs::metadata(path).ok().map(|metadata| metadata_epoch(&metadata));

    let mut session_start: Option<DroidSessionStart> = None;
    let mut messages: Vec<Message> = Vec::new();

    for value in values {
        match classify(value) {
            FlexibleRecord::Known(DroidRecord::SessionStart(start)) => {
                if session_start.is_none() {
                    session_start = Some(start);
                }
            }
            FlexibleRecord::Known(DroidRecord::Message(record)) => {
                if let Some(message) = project_message(&record) {
                    messages.push(message);
                }
            }
            // Unknown record kinds are preserved internally as raw values and
            // intentionally dropped from the lossy `Session` contract.
            FlexibleRecord::Unknown { .. } => {}
        }
    }

    let session_id = session_start
        .as_ref()
        .map(|start| start.id.clone())
        .filter(|id| !id.is_empty())
        .or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();

    let cwd = session_start
        .as_ref()
        .map(|start| start.cwd.clone())
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .or_else(|| path.parent().map(Path::to_path_buf))
        .unwrap_or_default();

    let start_timestamp = messages
        .first()
        .and_then(|message| message.timestamp.clone());

    let summary = summary_for(session_start.as_ref(), &messages);

    Ok(Session {
        tool: SourceTool::Droid,
        session_id,
        cwd,
        start_timestamp,
        summary,
        messages,
        path: path.to_path_buf(),
        modified_epoch,
    })
}

/// Classify a raw JSON record into a known typed Droid record or an unknown
/// raw record. A record whose `type` names a known kind but that fails strict
/// deserialization degrades to `Unknown` so the raw value is never lost.
fn classify(record: Value) -> FlexibleRecord<DroidRecord> {
    let type_tag = record
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match type_tag.as_deref() {
        Some("session_start") => serde_json::from_value::<DroidSessionStart>(record.clone())
            .map(|parsed| FlexibleRecord::Known(DroidRecord::SessionStart(parsed)))
            .unwrap_or(FlexibleRecord::Unknown { type_tag, raw: record }),
        Some("message") => serde_json::from_value::<DroidMessage>(record.clone())
            .map(|parsed| FlexibleRecord::Known(DroidRecord::Message(parsed)))
            .unwrap_or(FlexibleRecord::Unknown { type_tag, raw: record }),
        _ => FlexibleRecord::Unknown { type_tag, raw: record },
    }
}

/// Project a typed Droid message record into the lossy `Message` contract via
/// the shared `parsed_message` helper (role/content/timestamp first-text).
fn project_message(record: &DroidMessage) -> Option<Message> {
    let payload = record.message.as_ref()?;
    parsed_message(
        Some(&payload.role),
        Some(&payload.content),
        record.timestamp.as_deref(),
    )
}

/// Resolve the session title candidate. The top-level `title` is the
/// authoritative display title; `sessionTitle` is only consulted as a fallback
/// when `title` is absent or empty (in real Droid data `sessionTitle` only ever
/// holds the `New Session` placeholder, so this fallback rarely changes the
/// outcome but keeps the field load-bearing).
fn title_candidate(start: Option<&DroidSessionStart>) -> Option<&str> {
    let start = start?;
    let title = start.title.as_str();
    if !title.is_empty() {
        return Some(title);
    }
    start
        .session_title
        .as_deref()
        .filter(|title| !title.is_empty())
}

/// Build the lossy summary. A real title is normalized to the contract limit;
/// an empty or `New Session` placeholder title upgrades to the user-message
/// summary, finally falling back to `(no summary)` when there are no messages.
fn summary_for(start: Option<&DroidSessionStart>, messages: &[Message]) -> String {
    match title_candidate(start) {
        Some(title) if title != PLACEHOLDER_TITLE => normalize(title, 100),
        _ => summarize_messages(messages),
    }
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

    const SESSION_START: &str =
        r#"{"type":"session_start","id":"abc-123","title":"hello world","owner":"test-user","version":2,"cwd":"/tmp"}"#;
    const USER_MSG: &str = r#"{"type":"message","id":"m1","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"hello world"}]}}"#;
    const ASSISTANT_MSG: &str = r#"{"type":"message","id":"m2","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"}]}}"#;

    #[test]
    fn parses_session_start_and_messages() {
        let file = session_file(&[SESSION_START, USER_MSG, ASSISTANT_MSG]);
        let session = parse(file.path()).expect("parse");

        assert_eq!(session.tool, SourceTool::Droid);
        assert_eq!(session.session_id, "abc-123");
        assert_eq!(session.cwd, PathBuf::from("/tmp"));
        assert_eq!(session.start_timestamp.as_deref(), Some("2026-01-01T00:00:00.000Z"));
        assert_eq!(session.summary, "hello world");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text, "hello world");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].text, "Hi there!");
        assert_eq!(session.messages[1].timestamp.as_deref(), Some("2026-01-01T00:00:01.000Z"));
        assert_eq!(session.path, file.path());
        assert!(session.modified_epoch.is_some());
    }

    #[test]
    fn new_session_title_upgrades_to_user_summary() {
        let start = r#"{"type":"session_start","id":"ns-1","title":"New Session","sessionTitle":"New Session","cwd":"/tmp","version":2}"#;
        let user = r#"{"type":"message","id":"m1","timestamp":"2026-01-02T00:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Please fix the off-by-one bug in the parser"}]}}"#;
        let file = session_file(&[start, user]);
        let session = parse(file.path()).expect("parse");

        assert_eq!(session.session_id, "ns-1");
        assert_eq!(session.summary, "Please fix the off-by-one bug in the parser");
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn new_session_without_messages_falls_back_to_no_summary() {
        let start = r#"{"type":"session_start","id":"ns-2","title":"New Session","sessionTitle":"New Session","cwd":"/tmp","version":2}"#;
        let file = session_file(&[start]);
        let session = parse(file.path()).expect("parse");

        assert_eq!(session.summary, "(no summary)");
        assert!(session.messages.is_empty());
        assert!(session.start_timestamp.is_none());
    }

    #[test]
    fn title_used_when_session_title_absent() {
        let start = r#"{"type":"session_start","id":"t-1","title":"Real Title","cwd":"/tmp","version":2}"#;
        let file = session_file(&[start]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "Real Title");
    }

    #[test]
    fn title_takes_precedence_over_session_title() {
        let start = r#"{"type":"session_start","id":"t-2","title":"Preferred","sessionTitle":"ignored","cwd":"/tmp","version":2}"#;
        let file = session_file(&[start]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "Preferred");
    }

    #[test]
    fn session_title_used_when_title_absent() {
        let start = r#"{"type":"session_start","id":"t-2b","title":"","sessionTitle":"Fallback Title","cwd":"/tmp","version":2}"#;
        let file = session_file(&[start]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "Fallback Title");
    }

    #[test]
    fn long_title_is_truncated() {
        let long = "word ".repeat(40);
        let start = format!(
            r#"{{"type":"session_start","id":"t-3","title":"{long}","cwd":"/tmp","version":2}}"#
        );
        let file = session_file(&[&start]);
        let session = parse(file.path()).expect("parse");
        assert!(session.summary.ends_with("..."));
        assert!(session.summary.chars().count() <= 100);
    }

    #[test]
    fn malformed_lines_are_isolated() {
        let file = session_file(&[
            "this is not json",
            USER_MSG,
            "   ",
            "[1,2,3]",
            "null",
            ASSISTANT_MSG,
            "{not valid",
        ]);
        let session = parse(file.path()).expect("parse");

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].text, "hello world");
        assert_eq!(session.messages[1].text, "Hi there!");
    }

    #[test]
    fn unknown_records_are_preserved_internally_and_skipped() {
        let compaction = r#"{"type":"compaction_state","id":"c1","tokens":4096}"#;
        let todo = r#"{"type":"todo_state","items":[{"text":"do thing","done":false}]}"#;
        let outcome = r#"{"type":"agent_turn_outcome","id":"o1","ok":true}"#;
        let end = r#"{"type":"session_end","id":"e1"}"#;
        let file = session_file(&[SESSION_START, compaction, todo, outcome, USER_MSG, end]);
        let session = parse(file.path()).expect("parse");

        assert_eq!(session.summary, "hello world");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, Role::User);
    }

    #[test]
    fn known_type_failing_strict_deserialization_degrades_to_unknown() {
        // `id` is a number, not a string: strict deserialization of the typed
        // session-start fails, so the record is preserved as raw unknown and
        // contributes neither a session id nor a title. The file stem backs the
        // id and the summary upgrades to the user-message summary.
        let bad_start = r#"{"type":"session_start","id":12345,"title":"x","cwd":"/tmp","version":2}"#;
        let file = session_file(&[bad_start, USER_MSG]);
        let session = parse(file.path()).expect("parse");

        let stem = file
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap()
            .to_owned();
        assert_eq!(session.session_id, stem);
        assert_eq!(session.cwd, file.path().parent().unwrap().to_path_buf());
        assert_eq!(session.summary, "hello world");
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn message_without_payload_or_text_is_dropped() {
        let no_payload = r#"{"type":"message","id":"m0","timestamp":"2026-01-03T00:00:00.000Z"}"#;
        let empty_content = r#"{"type":"message","id":"m1","timestamp":"2026-01-03T00:00:01.000Z","message":{"role":"user","content":[]}}"#;
        let bad_role = r#"{"type":"message","id":"m2","timestamp":"2026-01-03T00:00:02.000Z","message":{"role":"system","content":[{"type":"text","text":"hi"}]}}"#;
        let file = session_file(&[SESSION_START, no_payload, empty_content, bad_role]);
        let session = parse(file.path()).expect("parse");

        assert!(session.messages.is_empty());
    }

    #[test]
    fn missing_session_start_falls_back_to_file_stem_and_parent_cwd() {
        let file = session_file(&[USER_MSG, ASSISTANT_MSG]);
        let session = parse(file.path()).expect("parse");

        let stem = file
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap()
            .to_owned();
        assert_eq!(session.session_id, stem);
        assert_eq!(session.cwd, file.path().parent().unwrap().to_path_buf());
        assert_eq!(session.start_timestamp.as_deref(), Some("2026-01-01T00:00:00.000Z"));
    }

    #[test]
    fn lossy_first_text_projection_picks_first_text_part() {
        let msg = r#"{"type":"message","id":"m1","timestamp":"2026-01-04T00:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"first answer"},{"type":"text","text":"second answer"}]}}"#;
        let file = session_file(&[SESSION_START, msg]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].text, "first answer");
    }

    #[test]
    fn string_content_is_projected() {
        let msg = r#"{"type":"message","id":"m1","timestamp":"2026-01-04T00:00:00.000Z","message":{"role":"user","content":"plain string body"}}"#;
        let file = session_file(&[SESSION_START, msg]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].text, "plain string body");
    }

    #[test]
    fn classify_routes_known_and_unknown() {
        let start = classify(serde_json::from_str(SESSION_START).unwrap());
        assert!(matches!(
            start,
            FlexibleRecord::Known(DroidRecord::SessionStart(_))
        ));

        let message = classify(serde_json::from_str(USER_MSG).unwrap());
        assert!(matches!(
            message,
            FlexibleRecord::Known(DroidRecord::Message(_))
        ));

        let other = classify(serde_json::from_str(r#"{"type":"compaction_state","x":1}"#).unwrap());
        assert!(matches!(other, FlexibleRecord::Unknown { .. }));

        let untagged = classify(serde_json::from_str(r#"{"foo":"bar"}"#).unwrap());
        assert!(matches!(
            untagged,
            FlexibleRecord::Unknown { type_tag: None, .. }
        ));
    }
}