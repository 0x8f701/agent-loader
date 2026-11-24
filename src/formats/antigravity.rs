//! Antigravity CLI conversation carrier.
//!
//! Each conversation is `conversations/<uuid>.db`: SQLite rows of protobuf
//! trajectory steps, plus a sibling `brain/<uuid>/` directory required by
//! native resume. Parsing accepts any readable carrier. Writing into a
//! directory named `conversations` also registers that one conversation in
//! the parent summary database and continue-cache; any other parent is a
//! standalone export and only receives the adjacent brain directory.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Map, Value};

use super::antigravity_transfer::{brain_path, check_path};
use super::antigravity_wire::{self as wire, Field};
use crate::domain::{ContentPart, Message, Role, Session, SessionHints, SourceTool, is_canonical_uuid};
use crate::formats::{normalize, summarize_messages};

const SCHEMA: &str = "\
PRAGMA user_version = 1;
\
CREATE TABLE trajectory_meta (trajectory_id text,cascade_id text,trajectory_type integer,source integer,PRIMARY KEY (trajectory_id));
\
CREATE TABLE steps (idx integer,step_type integer NOT NULL DEFAULT 0,status integer NOT NULL DEFAULT 0,has_subtrajectory numeric NOT NULL DEFAULT false,metadata blob,error_details blob,permissions blob,task_details blob,render_info blob,step_payload blob,step_format integer NOT NULL DEFAULT 0,PRIMARY KEY (idx));
\
CREATE INDEX idx_steps_status ON steps(status);
\
CREATE INDEX idx_steps_step_type ON steps(step_type);
\
CREATE TABLE gen_metadata (idx integer,data blob,size integer NOT NULL DEFAULT 0,PRIMARY KEY (idx));
\
CREATE TABLE executor_metadata (idx integer,data blob,PRIMARY KEY (idx));
\
CREATE TABLE parent_references (idx integer,data blob,PRIMARY KEY (idx));
\
CREATE TABLE trajectory_metadata_blob (id text DEFAULT \"main\",data blob,PRIMARY KEY (id));
\
CREATE TABLE battle_mode_infos (idx integer,data blob,PRIMARY KEY (idx));";

const SUMMARY_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS conversation_summaries (
\
 conversation_id text, title text NOT NULL DEFAULT \"\", preview text NOT NULL DEFAULT \"\",
\
 step_count integer NOT NULL DEFAULT 0, last_modified_time datetime NOT NULL,
\
 workspace_uris text NOT NULL, status text NOT NULL DEFAULT \"\", source text NOT NULL DEFAULT \"\",
\
 project_id text NOT NULL DEFAULT \"\", agent_name text NOT NULL DEFAULT \"\",
\
 parent_conversation_id text NOT NULL DEFAULT \"\", nesting_depth integer NOT NULL DEFAULT 0,
\
 battle_id text NOT NULL DEFAULT \"\", winning_conversation_id text NOT NULL DEFAULT \"\",
\
 not_fully_idle numeric NOT NULL DEFAULT false, killed numeric NOT NULL DEFAULT false,
\
 last_user_input_time datetime NOT NULL, last_user_input_step_index integer NOT NULL DEFAULT -1,
\
 app_data_dir text NOT NULL DEFAULT \"\", raw_summary blob, group_id text NOT NULL DEFAULT \"\",
\
 PRIMARY KEY (conversation_id));";

const STEP_CODE_ACTION: i64 = 5;
const STEP_GREP: i64 = 7;
const STEP_VIEW_FILE: i64 = 8;
const STEP_LIST_DIR: i64 = 9;
const STEP_USER: i64 = 14;
const STEP_PLANNER: i64 = 15;
const STEP_COMMAND: i64 = 21;
const STEP_CHECKPOINT: i64 = 23;
const STEP_MCP: i64 = 38;
const STEP_EPHEMERAL: i64 = 90;
const STEP_HISTORY: i64 = 98;
const STEP_SYSTEM: i64 = 101;
const STEP_GENERIC: i64 = 132;
const STATUS_PENDING: i64 = 1;
const STATUS_DONE: i64 = 3;
const STATUS_CANCELED: i64 = 6;
const STATUS_ERROR: i64 = 7;
const SOURCE_MODEL: u64 = 2;
const SOURCE_USER: u64 = 4;
const PROJECT_ID: &str = "default-cli-project";
const IDLE_STATUS: &str = "CASCADE_RUN_STATUS_IDLE";

struct Step {
    index: i64,
    kind: i64,
    status: i64,
    metadata: Vec<u8>,
    error: Vec<u8>,
    payload: Vec<u8>,
}

/// Read one conversation database, including its committed WAL.
pub fn parse(path: &Path) -> Result<Session> {
    check_path(path)?;
    let metadata = fs::symlink_metadata(path).with_context(|| format!("reading Antigravity carrier {}", path.display()))?;
    if !metadata.is_file() {
        bail!("Antigravity carrier is not a regular file: {}", path.display());
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening Antigravity carrier {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let steps = read_steps(&connection, path)?;
    let main = read_blob(&connection, "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'")?.unwrap_or_default();
    let main_fields = wire::decode(&main).context("decoding Antigravity trajectory metadata")?;
    let (_trajectory_id, cascade_id) = connection
        .query_row("SELECT trajectory_id, cascade_id FROM trajectory_meta LIMIT 1", [], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .optional()
        .with_context(|| format!("reading Antigravity identity {}", path.display()))?
        .unwrap_or_default();
    let stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or_default();
    let session_id = (!cascade_id.is_empty()).then_some(cascade_id).unwrap_or_else(|| stem.to_owned());
    let cwd = workspace_paths(&main_fields)?.into_iter().next().unwrap_or_default();
    let mut messages = Vec::new();
    let mut earliest = None;
    let mut checkpoint = None;
    let mut model = None;
    for step in &steps {
        let payload = wire::decode(&step.payload).with_context(|| format!("decoding Antigravity step {}", step.index))?;
        let meta = match wire::bytes(&payload, 5).filter(|bytes| !bytes.is_empty()) {
            Some(bytes) => wire::decode(bytes).with_context(|| format!("decoding Antigravity step {} metadata", step.index))?,
            None => wire::decode(&step.metadata).with_context(|| format!("decoding Antigravity step {} metadata column", step.index))?,
        };
        let timestamp = timestamp_text(wire::bytes(&meta, 1));
        if earliest.is_none() { earliest = timestamp.clone(); }

        match step.kind {
            STEP_USER => messages.extend(user_message(&payload, timestamp)?),
            STEP_PLANNER => {
                if let Some(value) = wire::varint(&meta, 11) { model = Some(format!("model-{value}")); }
                messages.extend(planner_message(&payload, timestamp)?);
            }
            STEP_CHECKPOINT => checkpoint = checkpoint_title(&payload)?.or(checkpoint),
            STEP_HISTORY | STEP_SYSTEM | STEP_EPHEMERAL => {}
            STEP_CODE_ACTION => messages.extend(code_action_message(step, &payload, &meta, timestamp)?),
            _ => messages.extend(tool_message(step, &payload, &meta, timestamp)?),
        }
    }
    let start = timestamp_text(wire::bytes(&main_fields, 2)).or(earliest);
    let summary = stored_title(path, &session_id)?.or(checkpoint).filter(|value| !value.trim().is_empty()).map(|value| normalize(&value, 100)).unwrap_or_else(|| summarize_messages(&messages));
    let session_kind = child_kind(path, &session_id)?;
    let modified_epoch = messages.iter().filter_map(|message| message.timestamp.as_deref().and_then(parse_time)).max().map(|time| time.timestamp() as f64).or(Some(file_epoch(&metadata)));
    Ok(Session {
        tool: SourceTool::Antigravity,
        session_id,
        cwd: PathBuf::from(cwd),
        start_timestamp: start.clone(),
        summary,
        messages,
        path: path.to_path_buf(),
        modified_epoch,
        hints: SessionHints { model, session_kind, ..SessionHints::default() },
    })
}

/// Write a new native carrier. `path` is the database path and `session_id`
/// its conversation UUID. An existing database is never replaced.
pub fn write(session: &Session, path: &Path, session_id: &str) -> Result<()> {
    check_path(path)?;
    validate_id(session_id)?;
    let native = native_layout(path);
    if native && path.file_stem().and_then(|value| value.to_str()) != Some(session_id) {
        bail!("Antigravity database name must be {session_id}.db");
    }
    if path.try_exists().with_context(|| format!("checking {}", path.display()))? {
        bail!("refusing to overwrite Antigravity carrier {}", path.display());
    }
    let cwd = session.cwd.to_str().with_context(|| format!("session cwd is not UTF-8: {}", session.cwd.display()))?;
    let parent = path.parent().context("Antigravity output has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let staging = tempfile::NamedTempFile::new_in(parent).context("creating Antigravity staging database")?;
    {
        let connection = Connection::open(staging.path()).context("opening Antigravity staging database")?;
        connection.execute_batch(SCHEMA).context("creating Antigravity schema")?;
        let transaction = connection.unchecked_transaction().context("starting Antigravity write")?;
        write_body(&transaction, session, session_id, cwd)?;
        transaction.commit().context("committing Antigravity carrier")?;
    }
    staging.persist_noclobber(path).with_context(|| format!("installing Antigravity carrier {}", path.display()))?;
    fs::create_dir_all(brain_for(path, session_id)?).context("creating Antigravity brain directory")?;
    if native { register_native(path)?; }
    Ok(())
}

/// Patch the primary workspace URI. Extra workspaces stay unchanged.
/// Unknown protobuf fields and every step stay byte-for-byte intact.
/// A `-wal` or `-shm` sidecar means a native writer may still be open.
pub(crate) fn rewrite_workspace(path: &Path, cwd: &Path) -> Result<()> {
    check_path(path)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        let sidecar = PathBuf::from(sidecar);
        if sidecar.try_exists()? { bail!("refusing workspace rewrite while SQLite sidecar exists: {}", sidecar.display()); }
    }
    let cwd = cwd.to_str().context("replacement cwd is not UTF-8")?;
    let connection = Connection::open(path).with_context(|| format!("opening {} for workspace rewrite", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let transaction = connection.unchecked_transaction()?;
    let main = read_blob(&transaction, "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'")?.context("Antigravity carrier has no main metadata")?;
    let fields = wire::decode(&main)?;
    let previous = workspace_paths(&fields)?;
    let primary = previous.first().cloned().into_iter().collect::<Vec<_>>();
    let updated = patch_metadata(&fields, &primary, cwd)?;
    transaction.execute("UPDATE trajectory_metadata_blob SET data = ?1 WHERE id = 'main'", params![updated])?;
    transaction.commit()?;
    if native_layout(path) {
        let id = path.file_stem().and_then(|value| value.to_str()).context("missing conversation id")?;
        patch_summary(path, &primary, cwd)?;
        retarget_cache(path.parent().and_then(Path::parent).context("missing Antigravity root")?, &primary, cwd, id)?;
    }
    Ok(())
}

/// Register one already-written native carrier. Rejects standalone exports.
pub(crate) fn register_native(path: &Path) -> Result<()> {
    check_path(path)?;
    if !native_layout(path) {
        bail!("Antigravity registration requires conversations/<uuid>.db, got {}", path.display());
    }
    let id = path.file_stem().and_then(|value| value.to_str()).context("missing conversation id")?;
    validate_id(id)?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let (trajectory, cascade, trajectory_type, source) = connection.query_row(
        "SELECT trajectory_id, cascade_id, trajectory_type, source FROM trajectory_meta LIMIT 1",
        [],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?)),
    ).context("reading Antigravity identity for registration")?;
    if cascade != id {
        bail!("Antigravity cascade id {cascade} does not match database {id}");
    }
    let main = read_blob(&connection, "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'")?.unwrap_or_default();
    let fields = wire::decode(&main)?;
    let workspaces = workspace_paths(&fields)?;
    let uris: Vec<String> = wire::all_bytes(&fields, 1).into_iter().filter_map(|workspace| wire::decode(workspace).ok()).filter_map(|workspace| wire::text(&workspace, 1).ok().flatten()).collect();
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM steps", [], |row| row.get(0))?;
    let last_user: i64 = connection.query_row("SELECT COALESCE(MAX(idx), -1) FROM steps WHERE step_type = 14", [], |row| row.get(0))?;
    let created = wire::bytes(&fields, 2).unwrap_or_default();
    let now = Utc::now();
    let title = stored_title(path, id)?.or(summary_title(&connection)?).or(first_user_text(&connection)?).filter(|value| !value.trim().is_empty()).unwrap_or_else(|| "Imported conversation".to_owned());
    let raw = summary_proto(&title, count, &trajectory, trajectory_type, source, created, &wire::all_bytes(&fields, 1).into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>(), last_user, &main, now);
    let root = path.parent().and_then(Path::parent).context("missing Antigravity root")?;
    upsert_summary(root, id, &title, count, &uris, last_user, &raw, now)?;
    if let Some(cwd) = workspaces.first() { merge_cache(root, cwd, id)?; }
    Ok(())
}

fn write_body(connection: &Connection, session: &Session, session_id: &str, cwd: &str) -> Result<()> {
    let trajectory_id = uuid::Uuid::new_v4().to_string();
    connection.execute("INSERT INTO trajectory_meta (trajectory_id, cascade_id, trajectory_type, source) VALUES (?1, ?2, 4, 17)", params![trajectory_id, session_id])?;
    let created = session.start_timestamp.as_deref().and_then(parse_time).unwrap_or_else(Utc::now);
    connection.execute("INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)", params![trajectory_metadata(cwd, session_id, created)])?;
    let mut index = 0_i64;
    let mut turn = 0_u64;
    let mut execution = String::new();
    let mut results = std::collections::HashMap::<String, (String, bool, Option<String>)>::new();
    for message in &session.messages {
        for part in message.effective_parts() {
            if let ContentPart::ToolResult { tool_use_id, content, is_error } = part {
                results.insert(tool_use_id, (content, is_error, message.timestamp.clone()));
            }
        }
    }
    let known = tool_ids(session);
    for message in &session.messages {
        let time = message.timestamp.as_deref().and_then(parse_time).unwrap_or(created);
        match message.role {
            Role::User | Role::Tool => {
                let parts = message.effective_parts();
                let orphan = orphan_text(&parts, &known);
                let (text, images) = user_content(&parts, &orphan)?;
                if text.is_empty() && images.is_empty() { continue; }
                turn += 1;
                execution = uuid::Uuid::new_v4().to_string();
                insert_step(connection, index, STEP_USER, STATUS_DONE, &user_step(session_id, &trajectory_id, index, turn, &execution, &text, &images, time)?)?;
                index += 1;
            }
            Role::Assistant => {
                if execution.is_empty() { execution = uuid::Uuid::new_v4().to_string(); }
                let calls = tool_calls(&message.effective_parts())?;
                insert_step(connection, index, STEP_PLANNER, STATUS_DONE, &planner_step(session, session_id, &trajectory_id, index, turn, &execution, message, &calls, time)?)?;
                index += 1;
                for (id, name, arguments) in calls {
                    let result = results.get(&id);
                    let status = match result { Some((_, true, _)) => STATUS_ERROR, Some(_) => STATUS_DONE, None => STATUS_PENDING };
                    insert_step(connection, index, STEP_GENERIC, status, &generic_step(session_id, &trajectory_id, index, turn, &execution, &id, &name, &arguments, result, time)?)?;
                    index += 1;
                }
            }
        }
    }
    if !session.summary.trim().is_empty() {
        if execution.is_empty() { execution = uuid::Uuid::new_v4().to_string(); }
        let latest = session.messages.iter().filter_map(|message| message.timestamp.as_deref().and_then(parse_time)).max().unwrap_or(created);
        let metadata = metadata_proto(session_id, &trajectory_id, index, turn, SOURCE_MODEL, &execution, latest, None, None);
        let mut body = Vec::new();
        wire::string_field(&mut body, 10, session.summary.trim());
        insert_step(connection, index, STEP_CHECKPOINT, STATUS_DONE, &envelope(STEP_CHECKPOINT, STATUS_DONE, &metadata, 30, &body))?;
    }
    Ok(())
}

fn tool_ids(session: &Session) -> std::collections::HashSet<String> {
    session.messages.iter().flat_map(|message| message.effective_parts()).filter_map(|part| match part {
        ContentPart::ToolUse { id, .. } if !id.is_empty() => Some(id),
        _ => None,
    }).collect()
}

fn user_content(parts: &[ContentPart], orphan: &str) -> Result<(String, Vec<Media>)> {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text(text) => texts.push(text.clone()),
            ContentPart::Note { kind, text } => texts.push(format!("[{kind}] {text}")),
            ContentPart::Image { mime_type, data } => images.push(media_from_part(mime_type.as_deref(), data)?),
            ContentPart::ToolResult { .. } | ContentPart::ToolUse { .. } | ContentPart::Thinking { .. } => {}
        }
    }
    if !orphan.is_empty() { texts.push(orphan.to_owned()); }
    Ok((texts.join("\n\n"), images))
}

fn orphan_text(parts: &[ContentPart], known: &std::collections::HashSet<String>) -> String {
    parts.iter().filter_map(|part| match part {
        ContentPart::ToolResult { tool_use_id, .. } if known.contains(tool_use_id) => None,
        ContentPart::ToolResult { tool_use_id, content, is_error } => {
            Some(format!("Tool {} for {tool_use_id}:\n{content}", if *is_error { "error" } else { "result" }))
        }
        _ => None,
    }).collect::<Vec<_>>().join("\n\n")
}

fn tool_calls(parts: &[ContentPart]) -> Result<Vec<(String, String, String)>> {
    let mut calls = Vec::new();
    for part in parts {
        if let ContentPart::ToolUse { id, name, input } = part {
            if id.is_empty() || name.is_empty() { bail!("Antigravity tool call requires an id and name"); }
            calls.push((id.clone(), name.clone(), input.to_string()));
        }
    }
    Ok(calls)
}

fn user_step(session: &str, trajectory: &str, index: i64, turn: u64, execution: &str, text: &str, images: &[Media], time: DateTime<Utc>) -> Result<Vec<u8>> {
    let metadata = metadata_proto(session, trajectory, index, turn, SOURCE_USER, execution, time, None, None);
    let mut body = Vec::new();
    wire::string_field(&mut body, 2, text);
    if !text.is_empty() {
        let mut item = Vec::new();
        wire::string_field(&mut item, 1, text);
        wire::bytes_field(&mut body, 3, &item);
    }
    for image in images { wire::bytes_field(&mut body, 9, &encode_media(image)); }
    Ok(envelope(STEP_USER, STATUS_DONE, &metadata, 19, &body))
}

fn planner_step(session: &Session, id: &str, trajectory: &str, index: i64, turn: u64, execution: &str, message: &Message, calls: &[(String, String, String)], time: DateTime<Utc>) -> Result<Vec<u8>> {
    let parts = message.effective_parts();
    let mut body = Vec::new();
    let text = parts.iter().filter_map(|part| match part {
        ContentPart::Text(text) => Some(text.clone()),
        ContentPart::Note { kind, text } => Some(format!("[{kind}] {text}")),
        _ => None,
    }).collect::<Vec<_>>().join("\n\n");
    wire::string_field(&mut body, 1, &text);
    let (thinking, signature) = combined_thinking(&parts);
    if !thinking.is_empty() {
        wire::string_field(&mut body, 3, &thinking);
        if let Some(signature) = signature {
            if let Some(binary) = signature.strip_prefix(wire::BASE64_PREFIX) {
                wire::bytes_field(&mut body, 14, &wire::base64_decode(binary)?);
            } else {
                wire::string_field(&mut body, 4, &signature);
            }
        }
    }
    for image in media_from_parts(&parts)? { wire::bytes_field(&mut body, 20, &encode_media(&image)); }
    wire::string_field(&mut body, 6, &format!("bot-{}", uuid::Uuid::new_v4()));
    for (call_id, name, arguments) in calls {
        wire::bytes_field(&mut body, 7, &chat_call(call_id, name, arguments));
    }
    wire::string_field(&mut body, 8, &text);
    wire::varint_field(&mut body, 12, if calls.is_empty() { 2 } else { 10 });
    let model = session.hints.model.as_deref().and_then(|value| value.strip_prefix("model-")).and_then(|value| value.parse().ok());
    let metadata = metadata_proto(id, trajectory, index, turn, SOURCE_MODEL, execution, time, None, model);
    Ok(envelope(STEP_PLANNER, STATUS_DONE, &metadata, 20, &body))
}

fn media_from_parts(parts: &[ContentPart]) -> Result<Vec<Media>> {
    parts.iter().filter_map(|part| match part {
        ContentPart::Image { mime_type, data } => Some(media_from_part(mime_type.as_deref(), data)),
        _ => None,
    }).collect()
}
struct Media {
    mime: String,
    inline: Option<Vec<u8>>,
    uri: Option<String>,
}

fn media_from_part(mime: Option<&str>, data: &str) -> Result<Media> {
    let mime = mime.unwrap_or("image/png").to_owned();
    if let Some(uri) = data.strip_prefix("data:") {
        let (header, payload) = uri.split_once(",").context("invalid data URL image")?;
        let mime = header.split(';').next().filter(|value| !value.is_empty()).unwrap_or(mime.as_str()).to_owned();
        return Ok(Media { mime, inline: Some(wire::base64_decode(payload).context("invalid data URL image")?), uri: None });
    }
    if data.contains("://") { return Ok(Media { mime, inline: None, uri: Some(data.to_owned()) }); }
    Ok(Media { mime, inline: Some(wire::base64_decode(data).context("image data is neither base64 nor a URI")?), uri: None })
}

fn encode_media(media: &Media) -> Vec<u8> {
    let mut out = Vec::new();
    wire::string_field(&mut out, 1, &media.mime);
    if let Some(bytes) = &media.inline { wire::bytes_field(&mut out, 2, bytes); }
    if let Some(uri) = &media.uri { wire::string_field(&mut out, 5, uri); }
    out
}

/// Native planner responses have one thinking slot. Multiple blocks are joined,
/// and only the first signature can occupy field 4 or binary field 14.
fn combined_thinking(parts: &[ContentPart]) -> (String, Option<String>) {
    let mut texts = Vec::new();
    let mut signature = None;
    for part in parts {
        if let ContentPart::Thinking { text, signature: found } = part {
            if !text.is_empty() { texts.push(text.clone()); }
            if signature.is_none() { signature = found.clone(); }
        }
    }
    (texts.join("\n\n"), signature)
}

fn generic_step(session: &str, trajectory: &str, index: i64, turn: u64, execution: &str, id: &str, name: &str, arguments: &str, result: Option<&(String, bool, Option<String>)>, call_time: DateTime<Utc>) -> Result<Vec<u8>> {
    let (status, time) = match result {
        Some((_, true, timestamp)) => (STATUS_ERROR, timestamp.as_deref().and_then(parse_time).unwrap_or(call_time)),
        Some((_, false, timestamp)) => (STATUS_DONE, timestamp.as_deref().and_then(parse_time).unwrap_or(call_time)),
        None => (STATUS_PENDING, call_time),
    };
    let call = chat_call(id, name, arguments);
    let metadata = metadata_proto(session, trajectory, index, turn, SOURCE_MODEL, execution, time, Some(&call), None);
    let mut result_body = Vec::new();
    if let Some((content, _, _)) = result { wire::string_field(&mut result_body, 1, content); }
    let mut body = Vec::new();
    wire::message_field(&mut body, 2, &result_body);
    Ok(envelope(STEP_GENERIC, status, &metadata, 140, &body))
}

fn chat_call(id: &str, name: &str, arguments: &str) -> Vec<u8> {
    let mut out = Vec::new();
    wire::string_field(&mut out, 1, id);
    wire::string_field(&mut out, 2, name);
    wire::string_field(&mut out, 3, arguments);
    wire::string_field(&mut out, 9, name);
    out
}

fn envelope(kind: i64, status: i64, metadata: &[u8], field: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    wire::varint_field(&mut out, 1, kind as u64);
    wire::varint_field(&mut out, 4, status as u64);
    wire::bytes_field(&mut out, 5, metadata);
    wire::bytes_field(&mut out, field, body);
    out
}

fn metadata_proto(session: &str, trajectory: &str, index: i64, turn: u64, source: u64, execution: &str, time: DateTime<Utc>, call: Option<&[u8]>, model: Option<u64>) -> Vec<u8> {
    let mut out = Vec::new();
    wire::bytes_field(&mut out, 1, &timestamp_proto(time));
    wire::varint_field(&mut out, 3, source);
    if let Some(call) = call { wire::bytes_field(&mut out, 4, call); }
    if let Some(model) = model { wire::varint_field(&mut out, 11, model); }
    wire::string_field(&mut out, 12, execution);
    let mut info = Vec::new();
    wire::string_field(&mut info, 1, trajectory);
    if index > 0 { wire::varint_field(&mut info, 2, index as u64); }
    if turn > 0 { wire::varint_field(&mut info, 3, turn - 1); }
    wire::string_field(&mut info, 4, session);
    wire::bytes_field(&mut out, 20, &info);
    out
}

fn trajectory_metadata(cwd: &str, session_id: &str, created: DateTime<Utc>) -> Vec<u8> {
    let uri = file_uri(cwd);
    let mut workspace = Vec::new();
    wire::string_field(&mut workspace, 1, &uri);
    wire::string_field(&mut workspace, 2, &uri);
    let mut out = Vec::new();
    wire::bytes_field(&mut out, 1, &workspace);
    wire::bytes_field(&mut out, 2, &timestamp_proto(created));
    wire::string_field(&mut out, 6, session_id);
    wire::string_field(&mut out, 7, &uri);
    wire::string_field(&mut out, 18, PROJECT_ID);

    out
}

fn timestamp_proto(time: DateTime<Utc>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Ok(seconds) = u64::try_from(time.timestamp()) {
        if seconds > 0 { wire::varint_field(&mut out, 1, seconds); }
    }
    if time.timestamp_subsec_nanos() > 0 { wire::varint_field(&mut out, 2, u64::from(time.timestamp_subsec_nanos())); }
    out
}

fn insert_step(connection: &Connection, index: i64, kind: i64, status: i64, payload: &[u8]) -> Result<()> {
    let fields = wire::decode(payload)?;
    let metadata = wire::bytes(&fields, 5).unwrap_or_default();
    connection.execute(
        "INSERT INTO steps (idx, step_type, status, has_subtrajectory, metadata, step_payload, step_format) VALUES (?1, ?2, ?3, 0, ?4, ?5, 0)",
        params![index, kind, status, metadata, payload],
    )?;
    Ok(())
}

fn user_message(payload: &[Field], timestamp: Option<String>) -> Result<Option<Message>> {
    let input = wire::message(payload, 19)?;
    let mut parts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(text) = wire::text(&input, 2)? { push_unique(&mut parts, &mut seen, text); }
    if parts.is_empty() {
        for item in wire::all_bytes(&input, 3) {
            if let Some(text) = wire::text(&wire::decode(item)?, 1)? { push_unique(&mut parts, &mut seen, text); }
        }
    }
    if parts.is_empty() {
        if let Some(text) = wire::text(&input, 1)? { push_unique(&mut parts, &mut seen, text); }
    }
    for image in wire::all_bytes(&input, 5) {
        let fields = wire::decode(image)?;
        if let Some(data) = wire::text(&fields, 1)? {
            parts.push(ContentPart::Image { mime_type: wire::text(&fields, 2)?, data });
        }
    }
    for media in wire::all_bytes(&input, 9) { parts.extend(media_part(&wire::decode(media)?)?); }
    Ok((!parts.is_empty()).then(|| Message::from_parts(Role::User, parts, timestamp)))
}

fn push_unique(parts: &mut Vec<ContentPart>, seen: &mut std::collections::HashSet<String>, text: String) {
    if !text.trim().is_empty() && seen.insert(text.clone()) { parts.push(ContentPart::Text(text)); }
}

fn planner_message(payload: &[Field], timestamp: Option<String>) -> Result<Option<Message>> {
    let planner = wire::message(payload, 20)?;
    let mut parts = Vec::new();
    let thinking = wire::text(&planner, 3)?.or(wire::text(&planner, 16)?).filter(|text| !text.trim().is_empty());
    if let Some(text) = thinking {
        let signature = match wire::bytes(&planner, 14) {
            Some(bytes) => Some(format!("{}{}", wire::BASE64_PREFIX, wire::base64_encode(bytes))),
            None => wire::text(&planner, 4)?,
        };
        parts.push(ContentPart::Thinking { text, signature });
    }
    if let Some(text) = wire::text(&planner, 1)?.or(wire::text(&planner, 8)?).filter(|text| !text.trim().is_empty()) {
        parts.push(ContentPart::Text(text));
    }
    for call in wire::all_bytes(&planner, 7) {
        let fields = wire::decode(call)?;
        let id = wire::text(&fields, 1)?.unwrap_or_default();
        let name = wire::text(&fields, 2)?.unwrap_or_default();
        if id.is_empty() || name.is_empty() { continue; }
        let input = match wire::text(&fields, 3)?.filter(|value| !value.trim().is_empty()) {
            Some(value) => serde_json::from_str(&value).with_context(|| format!("Antigravity tool arguments for {id} are not JSON"))?,
            None => Value::Object(Map::new()),
        };
        parts.push(ContentPart::ToolUse { id, name, input });
    }
    for media in wire::all_bytes(&planner, 20) { parts.extend(media_part(&wire::decode(media)?)?); }
    Ok((!parts.is_empty()).then(|| Message::from_parts(Role::Assistant, parts, timestamp)))
}

fn media_part(fields: &[Field]) -> Result<Vec<ContentPart>> {
    let mime = wire::text(fields, 1)?;
    let image = mime.as_deref().is_none_or(|value| value.starts_with("image/"));
    if let Some(bytes) = wire::bytes(fields, 2) {
        let data = wire::base64_encode(bytes);
        return Ok(vec![if image { ContentPart::Image { mime_type: mime, data } } else { ContentPart::Note { kind: "media".to_owned(), text: format!("{}: {data}", mime.as_deref().unwrap_or("application/octet-stream")) } }]);
    }
    if let Some(uri) = wire::text(fields, 5)? {
        return Ok(vec![if image { ContentPart::Image { mime_type: mime, data: uri } } else { ContentPart::Note { kind: "media".to_owned(), text: uri } }]);
    }
    Ok(wire::text(fields, 4)?.filter(|text| !text.is_empty()).map(|text| ContentPart::Note { kind: "media".to_owned(), text }).into_iter().collect())
}

fn tool_message(step: &Step, payload: &[Field], meta: &[Field], timestamp: Option<String>) -> Result<Option<Message>> {
    if !matches!(step.status, STATUS_DONE | STATUS_ERROR | STATUS_CANCELED) { return Ok(None); }
    let call = wire::message(meta, 4)?;
    let id = wire::text(&call, 1)?.filter(|value| !value.is_empty());
    let Some(id) = id else { return Ok(None); };
    let (content, failed) = tool_output(step.kind, payload)?;
    let error = error_text(&step.error);
    let is_error = matches!(step.status, STATUS_ERROR | STATUS_CANCELED) || error.is_some() || failed;
    let content = error.unwrap_or(content);
    Ok(Some(Message::from_parts(Role::Tool, vec![ContentPart::ToolResult { tool_use_id: id, content, is_error }], timestamp)))
}

fn tool_output(kind: i64, payload: &[Field]) -> Result<(String, bool)> {
    let field = match kind {
        STEP_VIEW_FILE => 14, STEP_LIST_DIR => 15, STEP_COMMAND => 28, STEP_GREP => 13, STEP_MCP => 47, STEP_GENERIC => 140, _ => 0,
    };
    let body = if field == 0 { Vec::new() } else { wire::message(payload, field)? };
    Ok(match kind {
        STEP_VIEW_FILE => (wire::text(&body, 4)?.or(wire::text(&body, 9)?).unwrap_or_default(), false),
        STEP_COMMAND => {
            let combined = wire::message(&body, 21)?;
            let text = wire::text(&combined, 1)?.or(wire::text(&body, 4)?).unwrap_or_default();
            (text, wire::varint(&body, 6).unwrap_or(0) != 0)
        }
        STEP_LIST_DIR => {
            let path = wire::text(&body, 1)?.as_deref().map(decode_file_uri).transpose()?.flatten().unwrap_or_default();
            let entries = wire::all_bytes(&body, 3).into_iter().filter_map(|entry| directory_entry(entry).ok().flatten()).collect::<Vec<_>>();
            (serde_json::json!({"path": path, "entries": entries}).to_string(), false)
        }
        STEP_GREP => match wire::text(&body, 5)?.filter(|text| !text.trim().is_empty()) {
            Some(error) => (error, true),
            None => (wire::text(&body, 3)?.unwrap_or_default(), false),
        },
        STEP_MCP => (wire::text(&body, 3)?.unwrap_or_default(), false),
        STEP_GENERIC => {
            let result = wire::message(&body, 2)?;
            (wire::text(&result, 1)?.unwrap_or_default(), false)
        }
        _ => (String::new(), false),
    })
}

fn directory_entry(bytes: &[u8]) -> Result<Option<Value>> {
    let fields = wire::decode(bytes)?;
    let Some(name) = wire::text(&fields, 1)? else { return Ok(None); };
    let mut entry = serde_json::json!({"name": name});
    if wire::varint(&fields, 2) == Some(1) { entry["is_dir"] = Value::Bool(true); }
    if let Some(size) = wire::varint(&fields, 4) { entry["size_bytes"] = Value::from(size); }
    Ok(Some(entry))
}

fn code_action_message(step: &Step, payload: &[Field], meta: &[Field], timestamp: Option<String>) -> Result<Option<Message>> {
    let Some(message) = tool_message(step, payload, meta, timestamp.clone())? else { return Ok(None); };
    let Some(ContentPart::ToolResult { tool_use_id, content, is_error }) = message.effective_parts().into_iter().next() else { return Ok(Some(message)); };
    if is_error || !content.is_empty() { return Ok(Some(message)); }
    let body = wire::message(payload, 10)?;
    let spec = wire::message(&body, 1)?;
    let created = wire::bytes(&spec, 2).is_some();
    let uri = action_uri(&spec)?;
    let key = if created { "created" } else { "edited" };
    let derived = serde_json::json!({"file": uri, key: true}).to_string();
    Ok(Some(Message::from_parts(Role::Tool, vec![ContentPart::ToolResult { tool_use_id, content: derived, is_error: false }], timestamp)))
}

fn action_uri(spec: &[Field]) -> Result<String> {
    let path_of = |bytes: &[u8]| wire::text(&wire::decode(bytes)?, 5);
    let create = wire::message(spec, 2)?;
    if let Some(path) = wire::bytes(&create, 2).map(path_of).transpose()?.flatten() { return decode_file_uri(&path).map(|value| value.unwrap_or(path)); }
    let command = wire::message(spec, 1)?;
    let path = wire::bytes(&command, 4).map(path_of).transpose()?.flatten().unwrap_or_default();
    decode_file_uri(&path).map(|value| value.unwrap_or(path))
}

fn error_text(bytes: &[u8]) -> Option<String> {
    let fields = wire::decode(bytes).ok()?;
    [9, 1, 2].into_iter().find_map(|field| wire::text(&fields, field).ok().flatten()).filter(|text| !text.trim().is_empty())
}

fn checkpoint_title(payload: &[Field]) -> Result<Option<String>> {
    let checkpoint = wire::message(payload, 30)?;
    Ok(wire::text(&checkpoint, 10)?.or(wire::text(&checkpoint, 4)?).filter(|text| !text.trim().is_empty()))
}

fn summary_title(connection: &Connection) -> Result<Option<String>> {
    let mut statement = connection.prepare("SELECT step_payload FROM steps WHERE step_type = 23 ORDER BY idx DESC LIMIT 1")?;
    let payload: Option<Vec<u8>> = statement.query_row([], |row| row.get(0)).optional()?;
    Ok(payload.and_then(|payload| wire::decode(&payload).ok()).and_then(|payload| checkpoint_title(&payload).ok()).flatten())
}
fn first_user_text(connection: &Connection) -> Result<Option<String>> {
    let mut statement = connection.prepare("SELECT step_payload FROM steps WHERE step_type = 14 ORDER BY idx LIMIT 1")?;
    let payload: Option<Vec<u8>> = statement.query_row([], |row| row.get(0)).optional()?;
    let Some(payload) = payload else { return Ok(None); };
    let input = wire::message(&wire::decode(&payload)?, 19)?;
    Ok(wire::text(&input, 2)?.filter(|text| !text.trim().is_empty()))
}
fn stored_title(path: &Path, id: &str) -> Result<Option<String>> {
    let database = path.parent().and_then(Path::parent).context("missing root")?.join("conversation_summaries.db");
    check_path(&database)?;
    if !database.try_exists()? { return Ok(None); }
    let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    connection.query_row("SELECT title FROM conversation_summaries WHERE conversation_id = ?1", params![id], |row| row.get(0)).optional().map_err(Into::into)
}

/// A parent id alone is a user fork. Only nesting depth above zero with an
/// agent name marks the internal subagents hidden from the default catalog.
fn child_kind(path: &Path, id: &str) -> Result<Option<String>> {
    if !native_layout(path) { return Ok(None); }
    let database = path.parent().and_then(Path::parent).context("missing root")?.join("conversation_summaries.db");
    check_path(&database)?;
    if !database.try_exists()? { return Ok(None); }
    let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let row: Option<(String, i64, String)> = connection.query_row(
        "SELECT parent_conversation_id, nesting_depth, agent_name FROM conversation_summaries WHERE conversation_id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    Ok(row.and_then(|(parent, depth, agent)| (!parent.trim().is_empty() && depth > 0 && !agent.trim().is_empty()).then(|| format!("subagent:{agent}"))))
}

fn summary_proto(title: &str, count: i64, trajectory: &str, trajectory_type: i64, source: i64, created: &[u8], workspaces: &[Vec<u8>], last_user: i64, main: &[u8], now: DateTime<Utc>) -> Vec<u8> {
    let mut out = Vec::new();
    wire::string_field(&mut out, 1, title);
    wire::varint_field(&mut out, 2, count.max(0) as u64);
    let stamp = timestamp_proto(now);
    wire::bytes_field(&mut out, 3, &stamp);
    wire::string_field(&mut out, 4, trajectory);
    wire::varint_field(&mut out, 5, 1);
    if !created.is_empty() { wire::bytes_field(&mut out, 7, created); }
    for workspace in workspaces { wire::bytes_field(&mut out, 9, workspace); }
    wire::bytes_field(&mut out, 10, &stamp);
    let mut annotations = Vec::new();
    wire::string_field(&mut annotations, 1, title);
    wire::message_field(&mut out, 15, &annotations);
    if last_user >= 0 { wire::varint_field(&mut out, 16, last_user as u64); }
    wire::bytes_field(&mut out, 17, main);
    if source > 0 { wire::varint_field(&mut out, 20, source as u64); }
    wire::varint_field(&mut out, 22, trajectory_type.max(0) as u64);
    out
}

fn upsert_summary(root: &Path, id: &str, title: &str, count: i64, uris: &[String], last_user: i64, raw: &[u8], now: DateTime<Utc>) -> Result<()> {
    let database = root.join("conversation_summaries.db");
    check_path(&database)?;
    let connection = Connection::open(&database).with_context(|| format!("opening {}", database.display()))?;
    connection.execute_batch(SUMMARY_SCHEMA)?;
    let stamp = now.to_rfc3339_opts(SecondsFormat::Nanos, true);
    connection.execute(
        "INSERT INTO conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,status,project_id,last_user_input_time,last_user_input_step_index,app_data_dir,raw_summary) VALUES (?1,?2,'',?3,?4,?5,?6,?7,?4,?8,'antigravity-cli',?9) ON CONFLICT(conversation_id) DO UPDATE SET title=excluded.title,step_count=excluded.step_count,last_modified_time=excluded.last_modified_time,workspace_uris=excluded.workspace_uris,status=excluded.status,last_user_input_time=excluded.last_user_input_time,last_user_input_step_index=excluded.last_user_input_step_index,raw_summary=excluded.raw_summary",
        params![id, title, count, stamp, serde_json::to_string(uris)?, IDLE_STATUS, PROJECT_ID, last_user, raw],
    )?;
    Ok(())
}

fn merge_cache(root: &Path, cwd: &str, id: &str) -> Result<()> {
    let cache = root.join("cache").join("last_conversations.json");
    check_path(&cache)?;
    if let Some(parent) = cache.parent() { fs::create_dir_all(parent)?; }
    let _lock = RoutingLock::acquire(&root.join("cache"))?;
    let mut routing = if cache.try_exists()? {
        let value: Value = serde_json::from_reader(File::open(&cache)?).context("reading Antigravity continue cache")?;
        value.as_object().cloned().context("Antigravity continue cache is not an object")?
    } else {
        Map::new()
    };
    routing.insert(cwd.to_owned(), Value::String(id.to_owned()));
    let parent = cache.parent().context("continue cache has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(serde_json::to_vec(&Value::Object(routing))?.as_slice())?;
    temporary.flush()?;
    temporary.persist(&cache).context("replacing Antigravity continue cache")?;
    Ok(())
}
fn retarget_cache(root: &Path, previous: &[String], cwd: &str, id: &str) -> Result<()> {
    let cache = root.join("cache").join("last_conversations.json");
    check_path(&cache)?;
    if !cache.try_exists()? { return Ok(()); }
    let _lock = RoutingLock::acquire(&root.join("cache"))?;
    let value: Value = serde_json::from_reader(File::open(&cache)?).context("reading Antigravity continue cache")?;
    let mut routing = value.as_object().cloned().context("Antigravity continue cache is not an object")?;
    routing.retain(|key, value| !(previous.iter().any(|old| old == key) && value.as_str() == Some(id)));
    routing.entry(cwd.to_owned()).or_insert_with(|| Value::String(id.to_owned()));
    let parent = cache.parent().context("continue cache has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(serde_json::to_vec(&Value::Object(routing))?.as_slice())?;
    temporary.as_file().sync_all()?;
    temporary.persist(&cache).context("replacing Antigravity continue cache")?;
    Ok(())
}

fn retarget_uri(bytes: &[u8], previous: &[String], cwd: &str) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes).unwrap_or_default();
    let decoded = decode_file_uri(text)?.unwrap_or_default();
    Ok(if previous.iter().any(|old| old == &decoded) { file_uri(cwd).into_bytes() } else { bytes.to_vec() })
}

fn patch_summary(path: &Path, previous: &[String], cwd: &str) -> Result<()> {
    let id = path.file_stem().and_then(|value| value.to_str()).context("missing id")?;
    let root = path.parent().and_then(Path::parent).context("missing root")?;
    let database = root.join("conversation_summaries.db");
    if !database.try_exists()? { return Ok(()); }
    let connection = Connection::open(&database)?;
    let transaction = connection.unchecked_transaction()?;
    let row: Option<(String, Vec<u8>)> = transaction.query_row("SELECT workspace_uris, raw_summary FROM conversation_summaries WHERE conversation_id = ?1", params![id], |row| Ok((row.get(0)?, row.get::<_, Vec<u8>>(1).unwrap_or_default()))).optional()?;
    let Some((uris, raw)) = row else { return Ok(()) };
    let mut values: Vec<String> = serde_json::from_str(&uris).unwrap_or_default();
    let next = file_uri(cwd);
    let old = previous.iter().map(|path| file_uri(path)).collect::<Vec<_>>();
    for value in &mut values {
        if old.iter().any(|candidate| candidate == value) { *value = next.clone(); }
    }
    if values.is_empty() { values.push(next.clone()); }
    let patched = patch_summary_blob(&raw, previous, cwd)?;
    transaction.execute("UPDATE conversation_summaries SET workspace_uris = ?1, raw_summary = ?2 WHERE conversation_id = ?3", params![serde_json::to_string(&values)?, patched, id])?;
    transaction.commit()?;
    Ok(())
}

fn patch_summary_blob(raw: &[u8], previous: &[String], cwd: &str) -> Result<Vec<u8>> {
    if raw.is_empty() { return Ok(Vec::new()); }
    let fields = wire::decode(raw)?;
    let workspaces = wire::all_bytes(&fields, 9).into_iter().map(|workspace| patch_workspace_message(workspace, previous, cwd)).collect::<Result<Vec<_>>>()?;
    let nested = wire::all_bytes(&fields, 17).into_iter().map(|metadata| patch_metadata(&wire::decode(metadata)?, previous, cwd)).collect::<Result<Vec<_>>>()?;
    let mut patched = wire::replace_bytes(&fields, 9, &workspaces);
    patched = wire::replace_bytes(&patched, 17, &nested);
    Ok(wire::encode(&patched))
}

fn patch_metadata(fields: &[Field], previous: &[String], cwd: &str) -> Result<Vec<u8>> {
    let workspaces = wire::all_bytes(fields, 1).into_iter().map(|workspace| patch_workspace_message(workspace, previous, cwd)).collect::<Result<Vec<_>>>()?;
    let mut patched = if workspaces.is_empty() { fields.to_vec() } else { wire::replace_bytes(fields, 1, &workspaces) };
    let listed = wire::all_bytes(fields, 7).into_iter().map(|uri| retarget_uri(uri, previous, cwd)).collect::<Result<Vec<_>>>()?;
    patched = wire::replace_bytes(&patched, 7, &listed);
    Ok(wire::encode(&patched))
}

fn patch_workspace_message(bytes: &[u8], previous: &[String], cwd: &str) -> Result<Vec<u8>> {
    let fields = wire::decode(bytes)?;

    Ok(wire::encode(&wire::replace_bytes(&fields, 1, &[retarget_uri(wire::bytes(&fields, 1).unwrap_or_default(), previous, cwd)?])))
}

fn workspace_paths(fields: &[Field]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for workspace in wire::all_bytes(fields, 1) {
        if let Some(uri) = wire::text(&wire::decode(workspace)?, 1)? {
            if let Some(path) = decode_file_uri(&uri)? { paths.push(path); }
        }
    }
    if paths.is_empty() {
        for uri in wire::all_bytes(fields, 7) {
            if let Some(path) = decode_file_uri(std::str::from_utf8(uri).context("workspace URI is not UTF-8")?)? { paths.push(path); }
        }
    }
    Ok(paths)
}

fn file_uri(path: &str) -> String {
    if path.starts_with("file://") { return path.to_owned(); }
    let mut encoded = String::from("file://");
    for (index, component) in path.split('/').enumerate() {
        if index > 0 { encoded.push('/'); }
        encoded.push_str(&percent_encoding::utf8_percent_encode(component, percent_encoding::NON_ALPHANUMERIC).to_string());
    }
    encoded
}

fn decode_file_uri(uri: &str) -> Result<Option<String>> {
    let Some(path) = uri.strip_prefix("file://") else { return Ok(Some(uri.to_owned())); };
    let path = path.strip_prefix("localhost").unwrap_or(path);
    let decoded = percent_encoding::percent_decode_str(path).decode_utf8().context("workspace URI contains invalid percent escapes")?;
    Ok((!decoded.is_empty()).then(|| decoded.into_owned()))
}

fn read_steps(connection: &Connection, path: &Path) -> Result<Vec<Step>> {
    let mut statement = connection.prepare("SELECT idx, step_type, status, metadata, error_details, step_payload FROM steps ORDER BY idx")
        .with_context(|| format!("reading Antigravity steps {}", path.display()))?;
    let rows = statement.query_map([], |row| Ok(Step {
        index: row.get(0)?, kind: row.get(1)?, status: row.get(2)?, metadata: row.get::<_, Vec<u8>>(3).unwrap_or_default(), error: row.get::<_, Vec<u8>>(4).unwrap_or_default(), payload: row.get::<_, Vec<u8>>(5).unwrap_or_default(),
    }))?;
    rows.collect::<Result<Vec<_>, _>>().context("reading an Antigravity step")
}

fn read_blob(connection: &Connection, sql: &str) -> Result<Option<Vec<u8>>> {
    connection.query_row(sql, [], |row| row.get(0)).optional().map_err(Into::into)
}

fn timestamp_text(bytes: Option<&[u8]>) -> Option<String> {
    let fields = wire::decode(bytes?).ok()?;
    let seconds = i64::try_from(wire::varint(&fields, 1).unwrap_or(0)).ok()?;
    let nanos = u32::try_from(wire::varint(&fields, 2).unwrap_or(0)).ok()?;
    Utc.timestamp_opt(seconds, nanos).single().map(|time| time.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value).ok().map(|time| time.with_timezone(&Utc))
}

fn native_layout(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()) == Some("db")
        && path.parent().and_then(Path::file_name).and_then(|value| value.to_str()) == Some("conversations")
        && path.file_stem().and_then(|value| value.to_str()).is_some_and(is_canonical_uuid)
}

/// Brain beside `conversations`, or `parent/brain/<id>` for a standalone export.
fn brain_for(path: &Path, session_id: &str) -> Result<PathBuf> {
    if native_layout(path) { brain_path(path) } else {
        Ok(path.parent().context("missing output parent")?.join("brain").join(session_id))
    }
}

fn validate_id(id: &str) -> Result<()> {
    if !is_canonical_uuid(id) { bail!("Antigravity session id must be a canonical UUID: {id}"); }
    Ok(())
}

fn file_epoch(metadata: &fs::Metadata) -> f64 {
    metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|time| time.as_secs_f64()).unwrap_or(0.0)
}

struct RoutingLock(PathBuf);

impl RoutingLock {
    fn acquire(cache_dir: &Path) -> Result<Self> {
        fs::create_dir_all(cache_dir)?;
        let path = cache_dir.join(".al-routing.lock");
        OpenOptions::new().write(true).create_new(true).open(&path).with_context(|| format!("Antigravity continue cache is busy: {}", path.display()))?;
        #[cfg(unix)]
        { use std::os::unix::fs::PermissionsExt; fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?; }
        Ok(Self(path))
    }
}

impl Drop for RoutingLock {
    fn drop(&mut self) { let _ = fs::remove_file(&self.0); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn session(text: &str) -> Session {
        Session {
            tool: SourceTool::Antigravity,
            session_id: "source".to_owned(),
            cwd: PathBuf::from("/work/project"),
            start_timestamp: Some("2026-01-02T03:04:05Z".to_owned()),
            summary: String::new(),
            messages: vec![
                Message::plain(Role::User, text, Some("2026-01-02T03:04:05Z".to_owned())),
                Message::from_parts(Role::Assistant, vec![
                    ContentPart::Thinking { text: "consider".to_owned(), signature: Some("sig".to_owned()) },
                    ContentPart::Text("done".to_owned()),
                    ContentPart::ToolUse { id: "call-1".to_owned(), name: "run_command".to_owned(), input: serde_json::json!({"CommandLine": "true"}) },
                    ContentPart::Image { mime_type: Some("image/png".to_owned()), data: wire::base64_encode(&[1, 2, 3]) },
                ], Some("2026-01-02T03:04:06Z".to_owned())),
                Message::from_parts(Role::Tool, vec![ContentPart::ToolResult { tool_use_id: "call-1".to_owned(), content: "ok".to_owned(), is_error: false }], None),
            ],
            path: PathBuf::new(),
            modified_epoch: None,
            hints: SessionHints { model: Some("model-1526".to_owned()), ..SessionHints::default() },
        }
    }

    #[test]
    fn round_trip_preserves_conversation_and_registers_native_root() {
        let root = TempDir::new().unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let path = root.path().join("conversations").join(format!("{id}.db"));
        write(&session("hello project"), &path, id).unwrap();
        assert!(root.path().join("brain").join(id).is_dir());
        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.session_id, id);
        assert_eq!(parsed.cwd, PathBuf::from("/work/project"));
        assert_eq!(parsed.hints.model.as_deref(), Some("model-1526"));
        assert!(parsed.messages.iter().any(|message| message.text.contains("hello project")));
        assert!(parsed.messages.iter().any(|message| message.parts.iter().any(|part| matches!(part, ContentPart::Thinking { signature: Some(value), .. } if value == "sig"))));
        assert!(parsed.messages.iter().any(|message| message.parts.iter().any(|part| matches!(part, ContentPart::ToolResult { content, .. } if content == "ok"))));
        assert!(parsed.messages.iter().any(|message| message.parts.iter().any(|part| matches!(part, ContentPart::Image { .. }))));
        let summaries = Connection::open(root.path().join("conversation_summaries.db")).unwrap();
        let count: i64 = summaries.query_row("SELECT COUNT(*) FROM conversation_summaries WHERE conversation_id = ?1", params![id], |row| row.get(0)).unwrap();
        assert_eq!(count, 1);
        let cache: Value = serde_json::from_str(&fs::read_to_string(root.path().join("cache/last_conversations.json")).unwrap()).unwrap();
        assert_eq!(cache["/work/project"], id);
    }

    #[test]
    fn standalone_export_skips_registration_and_second_write_refuses_overwrite() {
        let root = TempDir::new().unwrap();
        let id = "22222222-2222-4222-8222-222222222222";
        let path = root.path().join("exports").join(format!("{id}.db"));
        write(&session("exported"), &path, id).unwrap();
        assert!(root.path().join("exports/brain").join(id).is_dir());
        assert!(!root.path().join("conversation_summaries.db").exists());
        assert!(write(&session("exported"), &path, id).is_err());
        assert!(register_native(&path).is_err());
    }

    #[test]
    fn truncated_payload_and_parent_path_are_rejected() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("broken.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection.execute("INSERT INTO steps (idx, step_type, status, step_payload) VALUES (0, 14, 3, ?1)", params![vec![0x0a_u8]]).unwrap();
        assert!(parse(&path).is_err());
        assert!(parse(Path::new("/tmp/../etc/passwd")).is_err());
    }

    #[test]
    fn rewrite_preserves_unknown_metadata_and_steps() {
        let root = TempDir::new().unwrap();
        let id = "33333333-3333-4333-8333-333333333333";
        let path = root.path().join("conversations").join(format!("{id}.db"));
        write(&session("keep me"), &path, id).unwrap();
        let connection = Connection::open(&path).unwrap();
        let mut main: Vec<u8> = connection.query_row("SELECT data FROM trajectory_metadata_blob WHERE id='main'", [], |row| row.get(0)).unwrap();
        wire::bytes_field(&mut main, 15, &[0xab, 0xcd]);
        let payload: Vec<u8> = connection.query_row("SELECT step_payload FROM steps WHERE idx=0", [], |row| row.get(0)).unwrap();
        connection.execute("UPDATE trajectory_metadata_blob SET data=?1 WHERE id='main'", params![main]).unwrap();
        drop(connection);
        rewrite_workspace(&path, Path::new("/moved/project")).unwrap();
        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.cwd, PathBuf::from("/moved/project"));
        assert!(parsed.messages[0].text.contains("keep me"));
        let connection = Connection::open(&path).unwrap();
        let updated: Vec<u8> = connection.query_row("SELECT data FROM trajectory_metadata_blob WHERE id='main'", [], |row| row.get(0)).unwrap();
        let fields = wire::decode(&updated).unwrap();
        assert_eq!(wire::bytes(&fields, 15), Some([0xab, 0xcd].as_slice()));
        let same: Vec<u8> = connection.query_row("SELECT step_payload FROM steps WHERE idx=0", [], |row| row.get(0)).unwrap();
        assert_eq!(same, payload);
    }
    fn envelope_step(connection: &Connection, index: i64, kind: i64, status: i64, field: u32, body: &[u8], error: &[u8]) {
        let mut payload = Vec::new();
        wire::varint_field(&mut payload, 1, kind as u64);
        wire::varint_field(&mut payload, 4, status as u64);
        let mut call = Vec::new();
        wire::string_field(&mut call, 1, "edit-1");
        wire::string_field(&mut call, 2, "replace_file_content");
        let mut meta = Vec::new();
        wire::bytes_field(&mut meta, 4, &call);
        wire::bytes_field(&mut payload, 5, &meta);
        wire::bytes_field(&mut payload, field, body);
        connection.execute("INSERT INTO steps (idx,step_type,status,metadata,error_details,step_payload) VALUES (?1,?2,?3,?4,?5,?6)", params![index, kind, status, meta, error, payload]).unwrap();
    }

    #[test]
    fn failed_code_action_keeps_error_text() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("failed.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        let mut spec = Vec::new();
        wire::string_field(&mut spec, 5, "file:///work/file.rs");
        let mut body = Vec::new();
        wire::bytes_field(&mut body, 1, &spec);
        let mut error = Vec::new();
        wire::string_field(&mut error, 9, "permission denied sentinel");
        for status in [STATUS_ERROR, STATUS_CANCELED] {
            envelope_step(&connection, status, STEP_CODE_ACTION, status, 10, &body, &error);
        }
        drop(connection);
        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.messages.len(), 2);
        for message in &parsed.messages {
            assert!(message.parts.iter().any(|part| matches!(part, ContentPart::ToolResult { content, is_error: true, .. } if content == "permission denied sentinel")));
            assert!(!message.text.contains("edited"));
        }
    }

    #[test]
    fn system_bookkeeping_is_not_user_text() {
        let root = TempDir::new().unwrap();
        let id = "44444444-4444-4444-8444-444444444444";
        let path = root.path().join("conversations").join(format!("{id}.db"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection.execute("INSERT INTO trajectory_meta VALUES (?1,?2,4,17)", params![id, id]).unwrap();
        connection.execute("INSERT INTO trajectory_metadata_blob VALUES ('main', ?1)", params![trajectory_metadata("/work", id, Utc::now())]).unwrap();
        let mut user = Vec::new();
        wire::string_field(&mut user, 2, "real user");
        envelope_step(&connection, 0, STEP_USER, STATUS_DONE, 19, &user, &[]);
        let mut system = Vec::new();
        wire::string_field(&mut system, 1, "system sentinel");
        envelope_step(&connection, 1, STEP_SYSTEM, STATUS_DONE, 114, &system, &[]);
        let mut answer = Vec::new();
        wire::string_field(&mut answer, 1, "answer");
        envelope_step(&connection, 2, STEP_PLANNER, STATUS_DONE, 20, &answer, &[]);
        drop(connection);
        let parsed = parse(&path).unwrap();
        assert!(!parsed.messages.iter().any(|message| message.role == Role::User && message.text.contains("system sentinel")));
        let exported = root.path().join("conversations").join("55555555-5555-4555-8555-555555555555.db");
        write(&parsed, &exported, "55555555-5555-4555-8555-555555555555").unwrap();
        let again = parse(&exported).unwrap();
        assert!(again.messages.iter().any(|message| message.role == Role::User && message.text.contains("real user")));
        assert!(!again.messages.iter().any(|message| message.text.contains("system sentinel")));
    }

    #[test]
    fn workspace_uri_percent_encodes_reserved_characters() {
        let root = TempDir::new().unwrap();
        let id = "66666666-6666-4666-8666-666666666666";
        let path = root.path().join("exports").join(format!("{id}.db"));
        let mut source = session("path");
        source.cwd = PathBuf::from("/work/literal%20dir/#hash?query space/\u{4e2d}\u{6587}");
        write(&source, &path, id).unwrap();
        let connection = Connection::open(&path).unwrap();
        let main: Vec<u8> = connection.query_row("SELECT data FROM trajectory_metadata_blob WHERE id='main'", [], |row| row.get(0)).unwrap();
        let fields = wire::decode(&main).unwrap();
        let workspace = wire::decode(wire::bytes(&fields, 1).unwrap()).unwrap();
        let folder = wire::text(&workspace, 1).unwrap().unwrap();
        let listed = wire::text(&fields, 7).unwrap().unwrap();
        assert!(folder.contains("%2520") && folder.contains("%23") && folder.contains("%3F") && folder.contains("%20"));
        assert_eq!(folder, listed);
        assert_eq!(parse(&path).unwrap().cwd, source.cwd);
    }

    #[test]
    fn title_checkpoint_keeps_latest_message_time() {
        let root = TempDir::new().unwrap();
        let id = "88888888-8888-4888-8888-888888888888";
        let path = root.path().join("exports").join(format!("{id}.db"));
        let mut source = session("timed");
        source.start_timestamp = Some("2026-01-02T10:00:00Z".to_owned());
        source.messages[0].timestamp = Some("2026-01-02T10:00:00Z".to_owned());
        source.messages[1].timestamp = Some("2026-01-02T10:05:00Z".to_owned());
        source.summary = "kept title".to_owned();
        write(&source, &path, id).unwrap();
        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.summary, "kept title");
        assert_eq!(parsed.modified_epoch, Some(parse_time("2026-01-02T10:05:00Z").unwrap().timestamp() as f64));
    }


    #[test]
    fn rewrite_changes_only_matching_primary_workspace() {
        let root = TempDir::new().unwrap();
        let id = "77777777-7777-4777-8777-777777777777";
        let path = root.path().join("conversations").join(format!("{id}.db"));
        write(&session("workspace"), &path, id).unwrap();
        let connection = Connection::open(&path).unwrap();
        let mut extra = Vec::new();
        wire::string_field(&mut extra, 1, "file:///extra");
        wire::bytes_field(&mut extra, 99, &[9, 9]);
        let original: Vec<u8> = connection.query_row("SELECT data FROM trajectory_metadata_blob WHERE id='main'", [], |row| row.get(0)).unwrap();
        let fields = wire::decode(&original).unwrap();
        let primary = wire::bytes(&fields, 1).unwrap().to_vec();
        let patched = wire::encode(&wire::replace_bytes(&wire::replace_bytes(&fields, 1, &[primary, extra.clone()]), 7, &[b"file:///work/project".to_vec(), b"file:///extra".to_vec()]));
        connection.execute("UPDATE trajectory_metadata_blob SET data=?1 WHERE id='main'", params![patched]).unwrap();
        drop(connection);
        rewrite_workspace(&path, Path::new("/moved")).unwrap();
        let connection = Connection::open(&path).unwrap();
        let updated: Vec<u8> = connection.query_row("SELECT data FROM trajectory_metadata_blob WHERE id='main'", [], |row| row.get(0)).unwrap();
        let fields = wire::decode(&updated).unwrap();
        let workspaces = wire::all_bytes(&fields, 1);
        assert_eq!(workspaces[1], extra.as_slice());
        assert!(std::str::from_utf8(workspaces[0]).unwrap().contains("/moved"));
        assert_eq!(wire::all_bytes(&fields, 7)[1], b"file:///extra".as_slice());
    }

}
