use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, types::ValueRef};
use serde::Deserialize;
use serde_json::Value;

use crate::domain::{Message, Role, Session, SourceTool};
use crate::formats::{normalize, summarize_messages};
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

pub fn parse(path: &Path) -> Result<Session> {
    let metadata = fs::metadata(path).with_context(|| format!("reading Agent store metadata {}", path.display()))?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening Agent store {} read-only", path.display()))?;
    let meta = read_meta(&connection, path)?;
    let sidecar = read_sidecar(path)?;
    if meta
        .as_ref()
        .and_then(|value| value.get("subagentInfo"))
        .is_some_and(Value::is_object)
    {
        anyhow::bail!("Agent subagent session is not resumable: {}", path.display());
    }
    let messages = read_messages(&connection, path)?;
    let directory = path.parent().with_context(|| format!("Agent store has no parent: {}", path.display()))?;
    let session_id = meta.as_ref().and_then(|value| value.get("agentId")).and_then(Value::as_str)
        .filter(|value| !value.is_empty()).map(str::to_owned)
        .or_else(|| directory.file_name()?.to_str().map(str::to_owned)).unwrap_or_default();
    let cwd = sidecar.as_ref().map(|value| value.cwd.clone()).filter(|value| !value.as_os_str().is_empty()).unwrap_or_default();
    let start_timestamp = sidecar.as_ref().and_then(|value| value.created_at_ms.or(value.updated_at_ms)).and_then(timestamp_from_millis);
    let summary = sidecar.as_ref().map(|value| value.title.trim()).filter(|value| !value.is_empty()).map(|value| normalize(value, 100))
        .or_else(|| meta.as_ref().and_then(meaningful_meta_name)).unwrap_or_else(|| summarize_messages(&messages));
    let modified_epoch = sidecar.as_ref().and_then(|value| value.updated_at_ms.or(value.created_at_ms))
        .map(|value| value as f64 / 1000.0).or_else(|| Some(metadata_epoch(&metadata)));
    Ok(Session { tool: SourceTool::Agent, session_id, cwd, start_timestamp, summary, messages, path: path.to_path_buf(), modified_epoch })
}

fn read_meta(connection: &Connection, path: &Path) -> Result<Option<Value>> {
    let value = match connection.query_row("SELECT value FROM meta WHERE key = '0'", [], |row| row.get::<_, String>(0)).optional() {
        Ok(value) => value,
        Err(error) if is_missing_table(&error) => None,
        Err(error) => return Err(error).with_context(|| format!("reading Agent metadata {}", path.display())),
    };
    value.map(|value| decode_meta(&value).with_context(|| format!("decoding Agent metadata {}", path.display()))).transpose()
}

fn decode_meta(value: &str) -> Result<Value> {
    if let Ok(bytes) = decode_hex(value) {
        if let Ok(decoded) = serde_json::from_slice(&bytes) { return Ok(decoded); }
    }
    serde_json::from_str(value).context("metadata is neither hex JSON nor plain JSON")
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(value.len() % 2 == 0, "odd-length hex metadata");
    value.as_bytes().chunks_exact(2).map(|pair| {
        let high = hex_digit(pair[0]).context("invalid hex metadata")?;
        let low = hex_digit(pair[1]).context("invalid hex metadata")?;
        Ok((high << 4) | low)
    }).collect()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value { b'0'..=b'9' => Some(value - b'0'), b'a'..=b'f' => Some(value - b'a' + 10), b'A'..=b'F' => Some(value - b'A' + 10), _ => None }
}

fn read_sidecar(path: &Path) -> Result<Option<Sidecar>> {
    let directory = path.parent().with_context(|| format!("Agent store has no parent: {}", path.display()))?;
    let chats_root = directory
        .parent()
        .and_then(Path::parent)
        .with_context(|| format!("Agent store is not beneath a chats root: {}", path.display()))?;
    let directory = open_directory_under_root(directory, chats_root)
        .with_context(|| format!("opening Agent session directory {}", path.display()))?;
    let Some((mut file, _)) = open_regular_file_at(&directory, "meta.json") else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).with_context(|| format!("reading Agent sidecar {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes).ok())
}

fn read_messages(connection: &Connection, path: &Path) -> Result<Vec<Message>> {
    let mut statement = match connection.prepare("SELECT data FROM blobs ORDER BY rowid") {
        Ok(statement) => statement,
        Err(error) if is_missing_table(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading Agent blobs {}", path.display())),
    };
    let rows = statement.query_map([], |row| match row.get_ref(0)? {
        ValueRef::Blob(value) | ValueRef::Text(value) => Ok(Some(value.to_vec())),
        _ => Ok(None),
    }).with_context(|| format!("querying Agent blobs {}", path.display()))?;
    let mut messages = Vec::new();
    for row in rows {
        let Some(blob) = row.with_context(|| format!("reading Agent blob {}", path.display()))? else { continue };
        for value in json_values(&blob) {
            if let Some(message) = project_message(&value) { messages.push(message); }
        }
    }
    Ok(messages)
}

fn is_missing_table(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(_, Some(message)) if message.contains("no such table"))
}

fn json_values(data: &[u8]) -> Vec<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(data) { return vec![value]; }
    balanced_json_objects(data).filter_map(|object| serde_json::from_slice(object).ok()).collect()
}

fn balanced_json_objects(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut objects = Vec::new();
    let (mut start, mut depth, mut in_string, mut escaped) = (None, 0usize, false, false);
    for (index, byte) in data.iter().copied().enumerate() {
        if let Some(object_start) = start {
            if in_string {
                if escaped { escaped = false; } else if byte == b'\\' { escaped = true; } else if byte == b'"' { in_string = false; }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => { depth -= 1; if depth == 0 { objects.push(&data[object_start..=index]); start = None; } }
                _ => {}
            }
        } else if byte == b'{' { start = Some(index); depth = 1; }
    }
    objects.into_iter()
}

fn project_message(value: &Value) -> Option<Message> {
    let object = value.as_object()?;
    let role = match object.get("role").and_then(Value::as_str)? { "user" => Role::User, "assistant" => Role::Assistant, _ => return None };
    if role == Role::User && is_synthesized_user_record(object) { return None; }
    let raw = first_agent_text(object.get("content")?)?;
    let text = if role == Role::User { clean_user_text(raw)? } else { raw.trim().to_owned() };
    if text.is_empty() { return None; }
    let timestamp = ["timestamp", "createdAt", "created_at", "updatedAt", "updated_at"].into_iter()
        .find_map(|key| object.get(key).and_then(Value::as_str)).map(str::to_owned);
    Some(Message { role, text, timestamp })
}

fn is_synthesized_user_record(object: &serde_json::Map<String, Value>) -> bool {
    object.get("providerOptions").and_then(|value| value.get("cursor"))
        .and_then(|cursor| cursor.get("isSummary")).and_then(Value::as_bool) == Some(true)
}

fn first_agent_text(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) if !text.is_empty() => Some(text),
        Value::Array(items) => items.iter().find_map(|item| {
            let object = item.as_object()?;
            (object.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| object.get("text").and_then(Value::as_str))
                .flatten()
                .filter(|text| !text.is_empty())
        }),
        _ => None,
    }
}

fn clean_user_text(text: &str) -> Option<String> {
    let queries = tagged_contents(text, "user_query");
    if !queries.is_empty() {
        let joined = queries.into_iter().map(str::trim).filter(|value| !value.is_empty()).collect::<Vec<_>>().join("\n");
        return (!joined.is_empty()).then_some(joined);
    }
    let cleaned = ["user_info", "system_reminder", "attached_files", "timestamp", "git_status", "agent_transcripts"]
        .into_iter().fold(text.to_owned(), |text, tag| remove_tagged_sections(&text, tag)).trim().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn tagged_contents<'a>(text: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let (mut contents, mut remainder) = (Vec::new(), text);
    while let Some(open_index) = remainder.find(&open) {
        let content_start = open_index + open.len();
        let Some(close_offset) = remainder[content_start..].find(&close) else { break };
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
        let Some(close_offset) = remainder[content_start..].find(&close) else { remainder = &remainder[open_index..]; break; };
        remainder = &remainder[content_start + close_offset + close.len()..];
    }
    cleaned.push_str(remainder);
    cleaned
}

fn meaningful_meta_name(meta: &Value) -> Option<String> {
    ["name", "title"].into_iter().find_map(|key| meta.get(key).and_then(Value::as_str)).map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("New Agent")).map(|value| normalize(value, 100))
}

fn timestamp_from_millis(value: i64) -> Option<String> { Utc.timestamp_millis_opt(value).single().map(|value| value.to_rfc3339()) }

#[cfg(unix)]
fn metadata_epoch(metadata: &fs::Metadata) -> f64 { use std::os::unix::fs::MetadataExt; metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0 }
#[cfg(not(unix))]
fn metadata_epoch(metadata: &fs::Metadata) -> f64 { metadata.modified().ok().and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok()).map(|value| value.as_secs_f64()).unwrap_or(0.0) }

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};
    use tempfile::TempDir;

    fn fixture(meta: Option<&str>, blobs: &[&[u8]], sidecar: Option<&str>) -> (TempDir, PathBuf) {
        let home = TempDir::new().unwrap();
        let directory = home.path().join(".cursor/chats/0123456789abcdef0123456789abcdef/fallback-id");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("store.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT)", []).unwrap();
        connection.execute("CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB)", []).unwrap();
        if let Some(meta) = meta { connection.execute("INSERT INTO meta VALUES('0', ?1)", [meta]).unwrap(); }
        for (index, blob) in blobs.iter().enumerate() { connection.execute("INSERT INTO blobs VALUES(?1, ?2)", params![index.to_string(), blob]).unwrap(); }
        drop(connection);
        if let Some(sidecar) = sidecar { fs::write(directory.join("meta.json"), sidecar).unwrap(); }
        (home, path)
    }
    fn hex(value: &str) -> String { value.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect() }

    #[test]
    fn parses_hex_meta_and_sidecar_fields() {
        let meta = hex(r#"{"agentId":"canonical-id","name":"Meta name"}"#);
        let (_home, path) = fixture(Some(&meta), &[br#"{"role":"user","content":"hello"}"#], Some(r#"{"cwd":"/workspace/demo","title":"Sidecar title","createdAtMs":1767225600000,"updatedAtMs":1767225660000}"#));
        let session = parse(&path).unwrap();
        assert_eq!(session.session_id, "canonical-id");
        assert_eq!(session.cwd, Path::new("/workspace/demo"));
        assert_eq!(session.summary, "Sidecar title");
        assert_eq!(session.start_timestamp.as_deref(), Some("2026-01-01T00:00:00+00:00"));
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
    fn extracts_string_and_first_text_block_without_tool_output() {
        let wrapped = b"binary-prefix\0{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"answer\"},{\"type\":\"text\",\"text\":\"ignored\"}]}binary-suffix";
        let (_home, path) = fixture(Some(r#"{"agentId":"id","name":"New Agent"}"#), &[br#"{"role":"user","content":"question"}"#, wrapped, br#"{"role":"tool","content":"secret tool output"}"#, br#"{"type":"tool_result","content":{"role":"assistant","content":"nested output"}}"#], None);
        let session = parse(&path).unwrap();
        assert_eq!(session.messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(), ["question", "answer"]);
        assert_eq!(session.summary, "question");
    }
    #[test]
    fn cleans_user_wrappers_and_skips_injected_only_records() {
        let injected_context = br#"{"role":"user","content":"<user_info>ignore</user_info><agent_transcripts>ignore</agent_transcripts>","providerOptions":{"cursor":{"requestContextCompleteness":{"rules":true}}}}"#;
        let real_context = br#"{"role":"user","content":"<user_info>ignore</user_info>plain real request","providerOptions":{"cursor":{"requestContextCompleteness":{"rules":true}}}}"#;
        let synthesized_summary = br#"{"role":"user","content":"summary-only term","providerOptions":{"cursor":{"isSummary":true}}}"#;
        let (_home, path) = fixture(Some(r#"{"agentId":"id"}"#), &[br#"{"role":"user","content":"<user_info>ignore</user_info><user_query>tagged real request</user_query><system_reminder>ignore</system_reminder>"}"#, br#"{"role":"user","content":"<attached_files>ignore</attached_files><timestamp>ignore</timestamp>"}"#, injected_context, real_context, synthesized_summary], None);
        let session = parse(&path).unwrap();
        assert_eq!(session.messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(), ["tagged real request", "plain real request"]);
    }
    #[test]
    fn malformed_blob_storage_classes_do_not_hide_valid_messages() {
        let (_home, path) = fixture(Some(r#"{"agentId":"id"}"#), &[br#"{"role":"user","content":"valid message"}"#], None);
        let connection = Connection::open(&path).unwrap();
        connection.execute("INSERT INTO blobs VALUES('null', NULL)", []).unwrap();
        connection.execute("INSERT INTO blobs VALUES('integer', 7)", []).unwrap();
        connection.execute("INSERT INTO blobs VALUES('text', ?1)", [r#"{"role":"assistant","content":"text message"}"#]).unwrap();
        drop(connection);
        let session = parse(&path).unwrap();
        assert_eq!(session.messages.iter().map(|message| message.text.as_str()).collect::<Vec<_>>(), ["valid message", "text message"]);
    }
    #[test]
    fn subagents_are_rejected_as_non_resumable() {
        let (_home, path) = fixture(
            Some(r#"{"agentId":"subagent","subagentInfo":{"parentAgentId":"parent"}}"#),
            &[],
            None,
        );
        assert!(parse(&path).unwrap_err().to_string().contains("subagent"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_sidecar_is_ignored() {
        use std::os::unix::fs::symlink;
        let (_home, path) = fixture(Some(r#"{"agentId":"id","name":"Safe meta"}"#), &[], None);
        let outside = path.parent().unwrap().parent().unwrap().join("outside.json");
        fs::write(&outside, r#"{"cwd":"/unsafe","title":"Unsafe"}"#).unwrap();
        symlink(&outside, path.parent().unwrap().join("meta.json")).unwrap();
        let session = parse(&path).unwrap();
        assert_eq!(session.summary, "Safe meta");
        assert!(session.cwd.as_os_str().is_empty());
    }
    #[test]
    fn missing_tables_are_tolerated_but_invalid_database_is_contextual() {
        let home = TempDir::new().unwrap();
        let directory = home.path().join("workspace/session");
        fs::create_dir_all(&directory).unwrap();
        let empty_db = directory.join("store.db");
        Connection::open(&empty_db).unwrap();
        assert_eq!(parse(&empty_db).unwrap().session_id, "session");
        let invalid = directory.join("invalid.db");
        fs::write(&invalid, b"not sqlite").unwrap();
        assert!(!parse(&invalid).unwrap_err().to_string().is_empty());
    }
}
