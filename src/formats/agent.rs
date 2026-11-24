use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, types::ValueRef};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::domain::{ContentPart, Message, Role, Session, SourceTool};
use crate::formats::{content_parts, normalize, summarize_messages};
use crate::fs::{open_directory_under_root, open_regular_file_at};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sidecar {
    #[serde(default)]
    cwd: PathBuf,
    #[serde(default)]
    title: String,
    created_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
}

/// Why an Agent store is unloadable. Catalog listing treats [`Self::Subagent`]
/// as an expected skip rather than a corrupt file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AgentParseError {
    #[error("Agent subagent session is not resumable")]
    Subagent,
}

pub fn parse(path: &Path) -> Result<Session> {
    // Native roots select the current context only; archived compaction history is not exported.
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading Agent store metadata {}", path.display()))?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening Agent store {} read-only", path.display()))?;
    let meta = read_meta(&connection, path)?;
    let sidecar = read_sidecar(path)?;
    if meta
        .as_ref()
        .and_then(|value| value.get("subagentInfo"))
        .is_some_and(Value::is_object)
    {
        return Err(AgentParseError::Subagent.into());
    }
    let messages = read_messages(&connection, path, meta.as_ref())?;
    let directory = path
        .parent()
        .with_context(|| format!("Agent store has no parent: {}", path.display()))?;
    let session_id = meta
        .as_ref()
        .and_then(|value| value.get("agentId"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| directory.file_name()?.to_str().map(str::to_owned))
        .unwrap_or_default();
    let cwd = sidecar
        .as_ref()
        .map(|value| value.cwd.clone())
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_default();
    let start_timestamp = sidecar
        .as_ref()
        .and_then(|value| value.created_at_ms.or(value.updated_at_ms))
        .or_else(|| meta.as_ref()?.get("createdAt")?.as_i64())
        .and_then(timestamp_from_millis);
    let summary = sidecar
        .as_ref()
        .map(|value| value.title.trim())
        .filter(|value| !value.is_empty())
        .map(|value| normalize(value, 100))
        .or_else(|| meta.as_ref().and_then(meaningful_meta_name))
        .unwrap_or_else(|| summarize_messages(&messages));
    let modified_epoch = sidecar
        .as_ref()
        .and_then(|value| value.updated_at_ms.or(value.created_at_ms))
        .map(|value| value as f64 / 1000.0)
        .or_else(|| Some(metadata_epoch(&metadata)));
    let hints = crate::domain::SessionHints {
        model: meta.as_ref().and_then(|value| {
            ["model", "modelId", "currentModel", "lastUsedModel"]
                .into_iter()
                .find_map(|key| {
                    value
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                })
                .map(str::to_owned)
        }),
        ..crate::domain::SessionHints::default()
    };
    Ok(Session {
        tool: SourceTool::Agent,
        session_id,
        cwd,
        start_timestamp,
        summary,
        messages,
        path: path.to_path_buf(),
        modified_epoch,
        hints,
    })
}

fn read_meta(connection: &Connection, path: &Path) -> Result<Option<Value>> {
    let value = match connection
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()
    {
        Ok(value) => value,
        Err(error) if is_missing_table(&error) => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading Agent metadata {}", path.display()));
        }
    };
    value
        .map(|value| {
            decode_meta(&value)
                .with_context(|| format!("decoding Agent metadata {}", path.display()))
        })
        .transpose()
}

fn decode_meta(value: &str) -> Result<Value> {
    if let Ok(bytes) = decode_hex(value) {
        if let Ok(decoded) = serde_json::from_slice(&bytes) {
            return Ok(decoded);
        }
    }
    serde_json::from_str(value).context("metadata is neither hex JSON nor plain JSON")
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(value.len() % 2 == 0, "odd-length hex metadata");
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0]).context("invalid hex metadata")?;
            let low = hex_digit(pair[1]).context("invalid hex metadata")?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn read_sidecar(path: &Path) -> Result<Option<Sidecar>> {
    let directory = path
        .parent()
        .with_context(|| format!("Agent store has no parent: {}", path.display()))?;
    let chats_root = directory.parent().and_then(Path::parent).with_context(|| {
        format!(
            "Agent store is not beneath a chats root: {}",
            path.display()
        )
    })?;
    let directory = open_directory_under_root(directory, chats_root)
        .with_context(|| format!("opening Agent session directory {}", path.display()))?;
    let Some((mut file, _)) = open_regular_file_at(&directory, "meta.json") else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading Agent sidecar {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes).ok())
}

fn read_messages(connection: &Connection, path: &Path, meta: Option<&Value>) -> Result<Vec<Message>> {
    if let Some(root) = meta.and_then(|value| value.get("latestRootBlobId")) {
        let root = root.as_str().context("Agent root blob ID is not a string")?;
        let blob = read_blob(connection, root)?.context("Agent root blob is missing")?;
        let references = root_message_references(&blob).context("decoding Agent active context")?;
        let mut messages = Vec::new();
        for reference in references {
            let blob = read_blob(connection, &reference)?
                .context("Agent active context message blob is missing")?;
            let value: Value = serde_json::from_slice(&blob)
                .context("decoding Agent active context message")?;
            if let Some(message) = project_message(&value, true) {
                messages.push(message);
            }
        }
        return Ok(messages);
    }
    let mut statement = match connection.prepare("SELECT data FROM blobs ORDER BY rowid") {
        Ok(statement) => statement,
        Err(error) if is_missing_table(&error) => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading Agent blobs {}", path.display()));
        }
    };
    let rows = statement
        .query_map([], |row| match row.get_ref(0)? {
            ValueRef::Blob(value) | ValueRef::Text(value) => Ok(Some(value.to_vec())),
            _ => Ok(None),
        })
        .with_context(|| format!("querying Agent blobs {}", path.display()))?;
    let mut messages = Vec::new();
    for row in rows {
        let Some(blob) = row.with_context(|| format!("reading Agent blob {}", path.display()))?
        else {
            continue;
        };
        for value in json_values(&blob) {
            if let Some(message) = project_message(&value, false) {
                messages.push(message);
            }
        }
    }
    Ok(messages)
}

fn read_blob(connection: &Connection, id: &str) -> Result<Option<Vec<u8>>> {
    connection
        .query_row("SELECT data FROM blobs WHERE id = ?1", [id], |row| {
            match row.get_ref(0)? {
                ValueRef::Blob(value) | ValueRef::Text(value) => Ok(value.to_vec()),
                _ => Err(rusqlite::Error::InvalidColumnType(
                    0,
                    "data".to_owned(),
                    row.get_ref(0)?.data_type(),
                )),
            }
        })
        .optional()
        .context("reading Agent context blob")
}

fn root_message_references(mut bytes: &[u8]) -> Result<Vec<String>> {
    let mut references = Vec::new();
    while !bytes.is_empty() {
        let tag = read_varint(&mut bytes)?;
        anyhow::ensure!(tag >> 3 != 0, "invalid Agent root field number");
        anyhow::ensure!(tag >> 3 != 1 || tag & 7 == 2, "invalid Agent message reference wire type");
        let length = match tag & 7 {
            0 => {
                read_varint(&mut bytes)?;
                continue;
            }
            1 => 8,
            2 => usize::try_from(read_varint(&mut bytes)?)?,
            5 => 4,
            _ => anyhow::bail!("unsupported Agent root wire type"),
        };
        anyhow::ensure!(length <= bytes.len(), "truncated Agent root field");
        let (value, remainder) = bytes.split_at(length);
        if tag >> 3 == 1 {
            anyhow::ensure!(tag & 7 == 2 && length == 32, "invalid Agent message reference");
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut reference = String::with_capacity(64);
            for byte in value {
                reference.push(HEX[(byte >> 4) as usize] as char);
                reference.push(HEX[(byte & 15) as usize] as char);
            }
            references.push(reference);
        }
        bytes = remainder;
    }
    Ok(references)
}

fn read_varint(bytes: &mut &[u8]) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, remainder) = bytes.split_first().context("truncated Agent root varint")?;
        *bytes = remainder;
        anyhow::ensure!(shift < 63 || byte <= 1, "overflowing Agent root varint");
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("overflowing Agent root varint")
}

fn is_missing_table(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(_, Some(message)) if message.contains("no such table"))
}

fn json_values(data: &[u8]) -> Vec<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(data) {
        return vec![value];
    }
    balanced_json_objects(data)
        .filter_map(|object| serde_json::from_slice(object).ok())
        .collect()
}

fn balanced_json_objects(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut objects = Vec::new();
    let (mut start, mut depth, mut in_string, mut escaped) = (None, 0usize, false, false);
    for (index, byte) in data.iter().copied().enumerate() {
        if let Some(object_start) = start {
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        objects.push(&data[object_start..=index]);
                        start = None;
                    }
                }
                _ => {}
            }
        } else if byte == b'{' {
            start = Some(index);
            depth = 1;
        }
    }
    objects.into_iter()
}

fn project_message(value: &Value, include_summary: bool) -> Option<Message> {
    let object = value.as_object()?;
    let role = match object.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => return None,
    };
    if !include_summary && role == Role::User && is_synthesized_user_record(object) {
        return None;
    }
    let mut parts = object.get("content").map(content_parts).unwrap_or_default();
    if let Some(Value::Array(content)) = object.get("content") {
        for part in content {
            if part.get("type").and_then(Value::as_str) == Some("tool-result") {
                if let Some(content) = part.get("experimental_content") {
                    parts.extend(
                        content_parts(content)
                            .into_iter()
                            .filter(|part| matches!(part, ContentPart::Image { .. })),
                    );
                }
            }
        }
    }
    if let Some(Value::Array(calls)) = object.get("tool_calls") {
        for call in calls {
            parts.push(ContentPart::ToolUse {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                name: call
                    .get("name")
                    .or_else(|| {
                        call.get("function")
                            .and_then(|function| function.get("name"))
                    })
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                input: crate::formats::object_or_json(
                    call.get("arguments")
                        .or_else(|| {
                            call.get("function")
                                .and_then(|function| function.get("arguments"))
                        })
                        .or_else(|| call.get("input"))
                        .cloned()
                        .unwrap_or(serde_json::json!({})),
                ),
            });
        }
    }
    if role == Role::User {
        parts = parts
            .into_iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => clean_user_text(&text).map(ContentPart::Text),
                other => Some(other),
            })
            .collect();
    }
    if parts.is_empty() {
        return None;
    }
    let timestamp = [
        "timestamp",
        "createdAt",
        "created_at",
        "updatedAt",
        "updated_at",
    ]
    .into_iter()
    .find_map(|key| object.get(key).and_then(Value::as_str))
    .map(str::to_owned);
    Some(Message::from_parts(role, parts, timestamp))
}

fn is_synthesized_user_record(object: &serde_json::Map<String, Value>) -> bool {
    object
        .get("providerOptions")
        .and_then(|value| value.get("cursor"))
        .and_then(|cursor| cursor.get("isSummary"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn clean_user_text(text: &str) -> Option<String> {
    let queries = tagged_contents(text, "user_query");
    if !queries.is_empty() {
        let joined = queries
            .into_iter()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return (!joined.is_empty()).then_some(joined);
    }
    let cleaned = [
        "user_info",
        "system_reminder",
        "attached_files",
        "timestamp",
        "git_status",
        "agent_transcripts",
    ]
    .into_iter()
    .fold(text.to_owned(), |text, tag| {
        remove_tagged_sections(&text, tag)
    })
    .trim()
    .to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn tagged_contents<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let (mut contents, mut remainder) = (Vec::new(), text);
    while let Some(open_index) = remainder.find(&open) {
        let content_start = open_index + open.len();
        let Some(close_offset) = remainder[content_start..].find(&close) else {
            break;
        };
        let content_end = content_start + close_offset;
        contents.push(&remainder[content_start..content_end]);
        remainder = &remainder[content_end + close.len()..];
    }
    contents
}

fn remove_tagged_sections(text: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let (mut cleaned, mut remainder) = (String::with_capacity(text.len()), text);
    while let Some(open_index) = remainder.find(&open) {
        cleaned.push_str(&remainder[..open_index]);
        let content_start = open_index + open.len();
        let Some(close_offset) = remainder[content_start..].find(&close) else {
            remainder = &remainder[open_index..];
            break;
        };
        remainder = &remainder[content_start + close_offset + close.len()..];
    }
    cleaned.push_str(remainder);
    cleaned
}

fn meaningful_meta_name(meta: &Value) -> Option<String> {
    ["name", "title"]
        .into_iter()
        .find_map(|key| meta.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("New Agent"))
        .map(|value| normalize(value, 100))
}

fn timestamp_from_millis(value: i64) -> Option<String> {
    Utc.timestamp_millis_opt(value)
        .single()
        .map(|value| value.to_rfc3339())
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
    #[test]
    fn active_root_orders_messages_and_excludes_unreferenced_blobs() {
        let (_home, path) = fixture(
            Some(r#"{"latestRootBlobId":"synthetic-root"}"#),
            &[br#"{"role":"user","content":"unreferenced sentinel"}"#],
            None,
        );
        let connection = Connection::open(&path).unwrap();
        let mut root = Vec::new();
        for byte in [1u8, 2, 3] {
            root.extend_from_slice(&[10, 32]);
            root.extend_from_slice(&[byte; 32]);
        }
        for (byte, message) in [
            (3u8, r#"{"role":"assistant","content":"answer"}"#),
            (2, r#"{"role":"user","content":"question"}"#),
            (1, r#"{"role":"user","content":"context summary","providerOptions":{"cursor":{"isSummary":true}}}"#),
        ] {
            let id = hex(&String::from_utf8(vec![byte; 32]).unwrap());
            connection.execute("INSERT INTO blobs VALUES(?1, ?2)", params![id, message]).unwrap();
        }
        connection.execute("INSERT INTO blobs VALUES('synthetic-root', ?1)", [root]).unwrap();
        drop(connection);
        let session = parse(&path).unwrap();
        assert_eq!(
            session.messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(),
            ["context summary", "question", "answer"]
        );
    }

    #[test]
    fn invalid_active_roots_do_not_fall_back_to_all_blobs() {
        for root in [&b"\x0a\x20short"[..], &b"\x00"[..], &b"\x0a\x01x"[..]] {
            let (_home, path) = fixture(
                Some(r#"{"latestRootBlobId":"synthetic-root"}"#),
                &[br#"{"role":"user","content":"unreferenced sentinel"}"#],
                None,
            );
            let connection = Connection::open(&path).unwrap();
            connection.execute("INSERT INTO blobs VALUES('synthetic-root', ?1)", [root]).unwrap();
            drop(connection);
            assert!(parse(&path).is_err());
        }
        let (_home, path) = fixture(Some(r#"{"latestRootBlobId":"missing-root"}"#), &[], None);
        assert!(parse(&path).is_err());
        assert!(root_message_references(&[10, 32].into_iter().chain([7; 32]).collect::<Vec<_>>()).is_ok());
        assert!(read_varint(&mut &[255; 10][..]).is_err());
    }
    use super::*;
    use rusqlite::{Connection, params};
    use tempfile::TempDir;

    fn fixture(meta: Option<&str>, blobs: &[&[u8]], sidecar: Option<&str>) -> (TempDir, PathBuf) {
        let home = TempDir::new().unwrap();
        let directory = home
            .path()
            .join(".cursor/chats/0123456789abcdef0123456789abcdef/fallback-id");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("store.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT)", [])
            .unwrap();
        connection
            .execute("CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB)", [])
            .unwrap();
        if let Some(meta) = meta {
            connection
                .execute("INSERT INTO meta VALUES('0', ?1)", [meta])
                .unwrap();
        }
        for (index, blob) in blobs.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO blobs VALUES(?1, ?2)",
                    params![index.to_string(), blob],
                )
                .unwrap();
        }
        drop(connection);
        if let Some(sidecar) = sidecar {
            fs::write(directory.join("meta.json"), sidecar).unwrap();
        }
        (home, path)
    }
    fn hex(value: &str) -> String {
        value
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn parses_hex_meta_and_sidecar_fields() {
        let meta = hex(r#"{"agentId":"canonical-id","name":"Meta name"}"#);
        let (_home, path) = fixture(
            Some(&meta),
            &[br#"{"role":"user","content":"hello"}"#],
            Some(
                r#"{"cwd":"/workspace/demo","title":"Sidecar title","createdAtMs":1767225600000,"updatedAtMs":1767225660000}"#,
            ),
        );
        let session = parse(&path).unwrap();
        assert_eq!(session.session_id, "canonical-id");
        assert_eq!(session.cwd, Path::new("/workspace/demo"));
        assert_eq!(session.summary, "Sidecar title");
        assert_eq!(
            session.start_timestamp.as_deref(),
            Some("2026-01-01T00:00:00+00:00")
        );
        assert_eq!(session.modified_epoch, Some(1_767_225_660.0));
    }
    #[test]
    fn plain_meta_falls_back_to_parent_id_and_meaningful_name() {
        let (_home, path) = fixture(Some(r#"{"name":"Useful name"}"#), &[], None);
        let session = parse(&path).unwrap();
        assert_eq!(session.session_id, "fallback-id");
        assert_eq!(session.summary, "Useful name");
    }
    #[test]
    fn hyphenated_tool_blocks_and_redacted_reasoning_are_projected() {
        let assistant = br#"{"role":"assistant","content":[{"type":"redacted-reasoning","data":"opaque"},{"type":"text","text":"calling"},{"type":"tool-call","toolCallId":"c1","toolName":"Read","args":{"path":"a.rs"}}]}"#;
        let tool = br#"{"role":"tool","content":[{"type":"tool-result","toolCallId":"c1","toolName":"Read","result":"fn main() {}"}]}"#;
        let (_home, path) = fixture(Some(r#"{"agentId":"id"}"#), &[assistant, tool], None);
        let session = parse(&path).unwrap();
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(
                    part,
                    ContentPart::Thinking { text, signature }
                        if text == "[redacted]" && signature.as_deref() == Some("opaque")
                )
            })
        }));
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(
                    part,
                    ContentPart::ToolUse { name, input, .. }
                        if name == "Read" && input.get("path").and_then(Value::as_str) == Some("a.rs")
                )
            })
        }));
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(
                    part,
                    ContentPart::ToolResult { content, .. } if content.contains("fn main")
                )
            })
        }));
    }

    #[test]
    fn tool_calls_and_model_are_projected() {
        let blob = br#"{"role":"assistant","content":"calling","tool_calls":[{"id":"c1","name":"Read","arguments":"{\"path\":\"a.rs\"}"}]}"#;
        let (_home, path) = fixture(
            Some(r#"{"agentId":"id","model":"source-model"}"#),
            &[blob],
            None,
        );
        let session = parse(&path).unwrap();
        assert_eq!(session.hints.model.as_deref(), Some("source-model"));
        assert!(session.messages.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(part, ContentPart::ToolUse { name, input, .. } if name == "Read" && input.get("path").and_then(Value::as_str) == Some("a.rs"))
            })
        }));
    }

    #[test]
    fn last_used_model_is_projected_when_model_keys_are_absent() {
        let (_home, path) = fixture(
            Some(r#"{"agentId":"id","lastUsedModel":"cursor-model"}"#),
            &[br#"{"role":"user","content":"hello"}"#],
            None,
        );
        let session = parse(&path).unwrap();
        assert_eq!(session.hints.model.as_deref(), Some("cursor-model"));
    }

    #[test]
    fn last_used_model_wins_over_empty_model_keys() {
        let (_home, path) = fixture(
            Some(r#"{"agentId":"id","model":"","modelId":"","lastUsedModel":"cursor-model"}"#),
            &[br#"{"role":"user","content":"hello"}"#],
            None,
        );
        let session = parse(&path).unwrap();
        assert_eq!(session.hints.model.as_deref(), Some("cursor-model"));
    }

    #[test]
    fn native_metadata_creation_time_is_used_without_sidecar() {
        let (_home, path) = fixture(
            Some(r#"{"agentId":"id","createdAt":1767225600000}"#),
            &[],
            None,
        );
        let session = parse(&path).unwrap();
        assert_eq!(
            session.start_timestamp.as_deref(),
            Some("2026-01-01T00:00:00+00:00")
        );
    }

    #[test]
    fn structured_tool_result_images_are_preserved_without_duplicate_text() {
        let blob = br#"{"role":"tool","content":[{"type":"tool-result","toolCallId":"call","result":"image description","experimental_content":[{"type":"text","text":"image description"},{"type":"image","mimeType":"image/png","data":"c3ludGhldGlj"}]}]}"#;
        let (_home, path) = fixture(None, &[blob], None);
        let session = parse(&path).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].parts.len(), 2);
        assert!(matches!(
            &session.messages[0].parts[0],
            ContentPart::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "call" && content == "image description"
        ));
        assert!(matches!(
            &session.messages[0].parts[1],
            ContentPart::Image { mime_type, data }
                if mime_type.as_deref() == Some("image/png") && data == "c3ludGhldGlj"
        ));
    }

    #[test]
    fn extracts_string_and_first_text_block_without_tool_output() {
        let wrapped = b"binary-prefix\0{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"answer\"},{\"type\":\"text\",\"text\":\"ignored\"}]}binary-suffix";
        let (_home, path) = fixture(Some(r#"{"agentId":"id","name":"New Agent"}"#), &[br#"{"role":"user","content":"question"}"#, wrapped, br#"{"role":"tool","content":"secret tool output"}"#, br#"{"type":"tool_result","content":{"role":"assistant","content":"nested output"}}"#], None);
        let session = parse(&path).unwrap();
        assert_eq!(
            session
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["question", "answer\nignored", "secret tool output"]
        );
        assert_eq!(session.summary, "question");
    }
    #[test]
    fn cleans_user_wrappers_and_skips_injected_only_records() {
        let injected_context = br#"{"role":"user","content":"<user_info>ignore</user_info><agent_transcripts>ignore</agent_transcripts>","providerOptions":{"cursor":{"requestContextCompleteness":{"rules":true}}}}"#;
        let real_context = br#"{"role":"user","content":"<user_info>ignore</user_info>plain real request","providerOptions":{"cursor":{"requestContextCompleteness":{"rules":true}}}}"#;
        let synthesized_summary = br#"{"role":"user","content":"summary-only term","providerOptions":{"cursor":{"isSummary":true}}}"#;
        let (_home, path) = fixture(Some(r#"{"agentId":"id"}"#), &[br#"{"role":"user","content":"<user_info>ignore</user_info><user_query>tagged real request</user_query><system_reminder>ignore</system_reminder>"}"#, br#"{"role":"user","content":"<attached_files>ignore</attached_files><timestamp>ignore</timestamp>"}"#, injected_context, real_context, synthesized_summary], None);
        let session = parse(&path).unwrap();
        assert_eq!(
            session
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["tagged real request", "plain real request"]
        );
    }
    #[test]
    fn malformed_blob_storage_classes_do_not_hide_valid_messages() {
        let (_home, path) = fixture(
            Some(r#"{"agentId":"id"}"#),
            &[br#"{"role":"user","content":"valid message"}"#],
            None,
        );
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("INSERT INTO blobs VALUES('null', NULL)", [])
            .unwrap();
        connection
            .execute("INSERT INTO blobs VALUES('integer', 7)", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO blobs VALUES('text', ?1)",
                [r#"{"role":"assistant","content":"text message"}"#],
            )
            .unwrap();
        drop(connection);
        let session = parse(&path).unwrap();
        assert_eq!(
            session
                .messages
                .iter()
                .map(|message| message.text.as_str())
                .collect::<Vec<_>>(),
            ["valid message", "text message"]
        );
    }
    #[test]
    fn subagents_are_rejected_as_non_resumable() {
        let (_home, path) = fixture(
            Some(r#"{"agentId":"subagent","subagentInfo":{"parentAgentId":"parent"}}"#),
            &[],
            None,
        );
        let error = parse(&path).unwrap_err();
        assert_eq!(
            error.downcast_ref::<AgentParseError>().copied(),
            Some(AgentParseError::Subagent)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_sidecar_is_ignored() {
        use std::os::unix::fs::symlink;
        let (_home, path) = fixture(Some(r#"{"agentId":"id","name":"Safe meta"}"#), &[], None);
        let outside = path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("outside.json");
        fs::write(&outside, r#"{"cwd":"/unsafe","title":"Unsafe"}"#).unwrap();
        symlink(&outside, path.parent().unwrap().join("meta.json")).unwrap();
        let session = parse(&path).unwrap();
        assert_eq!(session.summary, "Safe meta");
        assert!(session.cwd.as_os_str().is_empty());
    }
    #[test]
    fn odd_length_hex_is_rejected() {
        let error = decode_hex("abc").unwrap_err().to_string();
        assert!(error.contains("odd-length hex metadata"), "{error}");
    }
    #[test]
    fn invalid_hex_digit_is_rejected() {
        let error = decode_hex("z0").unwrap_err().to_string();
        assert!(error.contains("invalid hex metadata"), "{error}");
    }
    #[test]
    fn hex_decoding_to_non_json_falls_back_to_plain_parse() {
        // "1234" decodes to bytes [0x12, 0x34], which are not JSON, so decode_meta
        // falls back to parsing the hex string itself as plain JSON: the number 1234.
        assert_eq!(decode_meta("1234").unwrap().as_i64(), Some(1234));
    }
}
