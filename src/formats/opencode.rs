//! OpenCode 1.19.6 portable exports and read-only native session selectors.
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::domain::{ContentPart, Message, Role, Session, SessionHints, SourceTool};
use crate::fs::{create_jsonl, open_directory_under_root, open_regular_file_at, path_under_root};

const RAW_PART: &str = "opencode.part";

fn safe_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix) && id.len() > prefix.len() && id.len() <= 128
        && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Splits a virtual selector before any filesystem operation on that selector.
pub fn native_reference(path: &Path) -> Result<Option<(PathBuf, String)>> {
    let Some(id) = path.file_name().and_then(|s| s.to_str()) else { return Ok(None) };
    if !id.starts_with("ses") { return Ok(None) }
    if !safe_id(id, "ses") {
        if path.extension().is_some_and(|extension| extension == "json") { return Ok(None) }
        bail!("invalid OpenCode native selector");
    }
    if path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        bail!("invalid OpenCode native selector");
    }
    let database = path.parent().ok_or_else(|| anyhow!("invalid OpenCode native selector"))?;
    if database.as_os_str().is_empty() { bail!("OpenCode selector has no database") }
    validate_database(database)?;
    Ok(Some((database.to_owned(), id.to_owned())))
}

pub fn database_path(reference: &Path) -> Result<PathBuf> {
    native_reference(reference)?.map(|(database, _)| database)
        .ok_or_else(|| anyhow!("expected an OpenCode native selector"))
}

fn validate_database(database: &Path) -> Result<()> {
    let parent = database.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !path_under_root(database, parent) || !database.is_file() {
        bail!("OpenCode database is not a safe regular file");
    }
    Ok(())
}

fn open_store(database: &Path) -> Result<Connection> {
    validate_database(database)?;
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| anyhow!("opening OpenCode database failed"))?;
    connection.busy_timeout(Duration::from_secs(2)).map_err(|_| anyhow!("configuring OpenCode database timeout failed"))?;
    connection.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED;")
        .map_err(|_| anyhow!("starting OpenCode read snapshot failed"))?;
    for sql in ["SELECT id,directory,title,time_created,time_updated FROM session LIMIT 0",
        "SELECT id,session_id,time_created,data FROM message LIMIT 0",
        "SELECT id,session_id,message_id,data FROM part LIMIT 0"] {
        connection.prepare(sql).map_err(|_| anyhow!("unsupported OpenCode database schema"))?;
    }
    Ok(connection)
}

pub fn discover(database: &Path) -> Result<Vec<PathBuf>> {
    if !database.try_exists().context("checking OpenCode database")? { return Ok(Vec::new()) }
    let connection = open_store(database)?;
    let mut statement = connection.prepare("SELECT id FROM session ORDER BY time_updated DESC,id ASC")
        .map_err(|_| anyhow!("enumerating OpenCode sessions failed"))?;
    let ids = statement.query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| anyhow!("enumerating OpenCode sessions failed"))?;
    let mut paths = Vec::new();
    for id in ids {
        let id = id.map_err(|_| anyhow!("reading OpenCode session identity failed"))?;
        if !safe_id(&id, "ses") { bail!("unsafe OpenCode session identity") }
        paths.push(database.join(id));
    }
    Ok(paths)
}

fn read_export(path: &Path) -> Result<Value> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let directory = open_directory_under_root(parent, parent).map_err(|_| anyhow!("opening OpenCode export directory failed"))?;
    let name = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| anyhow!("invalid OpenCode export filename"))?;
    let (file, _) = open_regular_file_at(&directory, name).ok_or_else(|| anyhow!("opening OpenCode export failed"))?;
    serde_json::from_reader(file).map_err(|_| anyhow!("invalid OpenCode export JSON"))
}

pub fn is_export(path: &Path) -> bool {
    read_export(path).is_ok_and(|v| v.get("info").is_some_and(Value::is_object)
        && v.get("messages").is_some_and(Value::is_array)
        && v["info"]["id"].as_str().is_some_and(|s| safe_id(s, "ses")))
}

pub fn parse(path: &Path) -> Result<Session> {
    let export = match native_reference(path)? {
        Some((database, id)) => hydrate(&open_store(&database)?, &id)?,
        None => read_export(path)?,
    };
    decode(&export, path)
}

fn json_column(row: &rusqlite::Row<'_>, name: &str) -> Result<Option<Value>> {
    let value: Option<String> = row.get(name).map_err(|_| anyhow!("reading OpenCode session metadata failed"))?;
    value.map(|s| serde_json::from_str(&s).map_err(|_| anyhow!("invalid OpenCode metadata JSON"))).transpose()
}

fn hydrate(connection: &Connection, id: &str) -> Result<Value> {
    let mut statement = connection.prepare("SELECT * FROM session WHERE id=?1")
        .map_err(|_| anyhow!("reading OpenCode session failed"))?;
    let mut rows = statement.query([id]).map_err(|_| anyhow!("reading OpenCode session failed"))?;
    let row = rows.next().map_err(|_| anyhow!("reading OpenCode session failed"))?
        .ok_or_else(|| anyhow!("OpenCode session not found"))?;
    let mut info = json!({"id":id});
    for (column, field) in [("slug","slug"),("directory","directory"),("title","title"),
        ("version","version"),("project_id","projectID"),("parent_id","parentID"),
        ("workspace_id","workspaceID"),("path","path"),("agent","agent")] {
        if let Ok(Some(value)) = row.get::<_, Option<String>>(column) { info[field] = json!(value); }
    }
    for field in ["model", "revert", "metadata"] {
        if row.as_ref().column_index(field).is_ok() {
            if let Some(value) = json_column(row, field)? { info[field] = value; }
        }
    }
    let created: i64 = row.get("time_created").map_err(|_| anyhow!("invalid OpenCode creation time"))?;
    let updated: i64 = row.get("time_updated").map_err(|_| anyhow!("invalid OpenCode update time"))?;
    info["time"] = json!({"created":created,"updated":updated});
    let mut messages = Vec::new();
    let mut statement = connection.prepare("SELECT id,data FROM message WHERE session_id=?1 ORDER BY time_created ASC,id ASC")
        .map_err(|_| anyhow!("reading OpenCode messages failed"))?;
    let rows = statement.query_map([id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))
        .map_err(|_| anyhow!("reading OpenCode messages failed"))?;
    let mut parts_query = connection.prepare("SELECT id,message_id,data FROM part WHERE session_id=?1 AND message_id=?2 ORDER BY id ASC")
        .map_err(|_| anyhow!("reading OpenCode parts failed"))?;
    for row in rows {
        let (message_id, data) = row.map_err(|_| anyhow!("reading OpenCode message failed"))?;
        let mut message = object_json(&data)?;
        inject(&mut message, "id", &message_id)?;
        inject(&mut message, "sessionID", id)?;
        let rows = parts_query.query_map([id, &message_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))
            .map_err(|_| anyhow!("reading OpenCode parts failed"))?;
        let mut parts = Vec::new();
        for row in rows {
            let (part_id, parent_id, data) = row.map_err(|_| anyhow!("reading OpenCode part failed"))?;
            let mut part = object_json(&data)?;
            inject(&mut part, "id", &part_id)?;
            inject(&mut part, "sessionID", id)?;
            inject(&mut part, "messageID", &parent_id)?;
            parts.push(part);
        }
        messages.push(json!({"info":message,"parts":parts}));
    }
    Ok(json!({"info":info,"messages":messages}))
}

fn object_json(data: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(data).map_err(|_| anyhow!("invalid OpenCode row JSON"))?;
    if !value.is_object() { bail!("OpenCode row is not an object") }
    Ok(value)
}

fn inject(value: &mut Value, field: &str, authoritative: &str) -> Result<()> {
    if value.get(field).is_some_and(|v| v.as_str() != Some(authoritative)) {
        bail!("inconsistent OpenCode relational identity");
    }
    value[field] = json!(authoritative);
    Ok(())
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field].as_str().ok_or_else(|| anyhow!("missing or invalid OpenCode {field}"))
}

fn timestamp(value: &Value) -> Option<String> {
    value.as_i64().and_then(DateTime::<Utc>::from_timestamp_millis).map(|v| v.to_rfc3339())
}

fn decode(export: &Value, path: &Path) -> Result<Session> {
    validate_links(export)?;
    let info = &export["info"];
    let mut hints = SessionHints::default();
    hints.model = info["model"]["id"].as_str().map(str::to_owned);
    hints.provider = info["model"]["providerID"].as_str().map(str::to_owned);
    let mut messages = Vec::new();
    let revert = &info["revert"];
    for entry in export["messages"].as_array().ok_or_else(|| anyhow!("missing OpenCode messages"))? {
        let message = &entry["info"];
        let at_revert = message["id"] == revert["messageID"] && !revert["messageID"].is_null();
        if at_revert && revert["partID"].is_null() { break }
        let role = match text(message, "role")? { "user" => Role::User, "assistant" => Role::Assistant, _ => bail!("unsupported OpenCode message role") };
        if hints.model.is_none() {
            hints.model = message["modelID"].as_str().or_else(|| message["model"]["modelID"].as_str()).map(str::to_owned);
        }
        if hints.provider.is_none() {
            hints.provider = message["providerID"].as_str().or_else(|| message["model"]["providerID"].as_str()).map(str::to_owned);
        }
        let mut parts = Vec::new();
        if let Some(system) = message["system"].as_str() {
            parts.push(ContentPart::Note { kind: "system".into(), text: system.into() });
        }
        for part in entry["parts"].as_array().ok_or_else(|| anyhow!("missing OpenCode parts"))? {
            if at_revert && part["id"] == revert["partID"] { break }
            decode_part(part, &mut parts)?;
        }
        if let Some(error) = message.get("error") {
            parts.push(ContentPart::Note { kind: "opencode.error".into(), text: error.to_string() });
        }
        if message["summary"] == true {
            hints.compaction_summary = Some(crate::domain::joined_text(&parts));
        }
        messages.push(Message::from_parts(role, parts, timestamp(&message["time"]["created"])));
        if at_revert { break }
    }
    Ok(Session { tool: SourceTool::OpenCode, session_id: text(info,"id")?.into(),
        cwd: PathBuf::from(text(info,"directory")?), start_timestamp: timestamp(&info["time"]["created"]),
        summary: text(info,"title")?.into(), messages, path: path.to_owned(),
        modified_epoch: info["time"]["updated"].as_f64().map(|v| v / 1000.0), hints })
}

fn decode_part(part: &Value, parts: &mut Vec<ContentPart>) -> Result<()> {
    match text(part,"type")? {
        "text" => parts.push(ContentPart::Text(text(part,"text")?.into())),
        "reasoning" => parts.push(ContentPart::Thinking { text: text(part,"text")?.into(),
            signature: None }),
        "file" if text(part,"mime")?.starts_with("image/") => parts.push(ContentPart::Image {
            mime_type: Some(text(part,"mime")?.into()), data: text(part,"url")?.into() }),
        "tool" => {
            let id = text(part,"callID")?.to_owned();
            parts.push(ContentPart::ToolUse { id: id.clone(), name: text(part,"tool")?.into(), input: part["state"]["input"].clone() });
            let state = &part["state"];
            match text(state,"status")? {
                "completed" | "error" => {
                    let error = state["status"] == "error";
                    parts.push(ContentPart::ToolResult { tool_use_id: id, content: text(state,if error {"error"} else {"output"})?.into(), is_error: error });
                    if let Some(attachments) = state["attachments"].as_array() {
                        for attachment in attachments { decode_part(attachment, parts)?; }
                    }
                }
                "pending" | "running" => (),
                _ => bail!("unsupported OpenCode tool status"),
            }
        }
        _ => parts.push(ContentPart::Note { kind: RAW_PART.into(), text: part.to_string() }),
    }
    if part["type"] == "reasoning" && part.get("metadata").is_some() {
        parts.push(ContentPart::Note { kind: "opencode.part-metadata".into(), text: part.to_string() });
    }
    Ok(())
}

fn validate_links(export: &Value) -> Result<()> {
    let session_id = text(&export["info"],"id")?;
    if !safe_id(session_id,"ses") { bail!("invalid OpenCode session identity") }
    let entries = export["messages"].as_array().ok_or_else(|| anyhow!("missing OpenCode messages"))?;
    let mut message_ids = HashSet::new();
    let mut user_ids = HashSet::new();
    let mut part_ids = HashSet::new();
    for entry in entries {
        let info = &entry["info"];
        let id = text(info,"id")?;
        if !safe_id(id,"msg") || !message_ids.insert(id) || text(info,"sessionID")? != session_id {
            bail!("invalid OpenCode message relationship");
        }
        match text(info,"role")? {
            "user" => { user_ids.insert(id); }
            "assistant" => {
                if !user_ids.contains(text(info,"parentID")?) { bail!("invalid OpenCode assistant parent") }
            }
            _ => bail!("unsupported OpenCode message role"),
        }
        for part in entry["parts"].as_array().ok_or_else(|| anyhow!("missing OpenCode parts"))? {
            validate_part_links(part, session_id, id, &mut part_ids)?;
        }
    }
    if let Some(revert) = export["info"].get("revert") {
        if !user_ids.contains(revert["messageID"].as_str().unwrap_or(""))
            && !message_ids.contains(revert["messageID"].as_str().unwrap_or("")) {
            bail!("invalid OpenCode revert message reference");
        }
        if let Some(part_id) = revert["partID"].as_str() {
            let valid = entries.iter().any(|entry| entry["info"]["id"] == revert["messageID"]
                && entry["parts"].as_array().is_some_and(|parts| parts.iter().any(|part| part["id"] == part_id)));
            if !valid { bail!("invalid OpenCode revert part reference") }
        }
    }
    Ok(())
}

fn reject_row_collisions(connection: &Connection, document: &Value) -> Result<()> {
    for entry in document["messages"].as_array().ok_or_else(|| anyhow!("missing OpenCode messages"))? {
        let message = text(&entry["info"],"id")?;
        let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM message WHERE id=?1)",[message],|r| r.get(0))
            .map_err(|_| anyhow!("checking OpenCode message collision failed"))?;
        if exists { bail!("OpenCode import message identity already exists") }
        for part in entry["parts"].as_array().ok_or_else(|| anyhow!("missing OpenCode parts"))? {
            reject_part_collision(connection,part)?;
        }
    }
    Ok(())
}

fn reject_part_collision(connection: &Connection, part: &Value) -> Result<()> {
    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM part WHERE id=?1)",[text(part,"id")?],|r| r.get(0))
        .map_err(|_| anyhow!("checking OpenCode part collision failed"))?;
    if exists { bail!("OpenCode import part identity already exists") }
    if let Some(attachments) = part["state"]["attachments"].as_array() {
        for attachment in attachments { reject_part_collision(connection,attachment)?; }
    }
    Ok(())
}

fn validate_part_links<'a>(part: &'a Value, session: &str, message: &str, ids: &mut HashSet<&'a str>) -> Result<()> {
    let id = text(part,"id")?;
    if !safe_id(id,"prt") || !ids.insert(id) || text(part,"sessionID")? != session || text(part,"messageID")? != message {
        bail!("invalid OpenCode part relationship");
    }
    if let Some(attachments) = part["state"]["attachments"].as_array() {
        for attachment in attachments { validate_part_links(attachment, session, message, ids)?; }
    }
    Ok(())
}

/// Counter prefix orders every generated message/part even within one millisecond.
struct Ids { sequence: u64 }
impl Ids {
    fn next(&mut self, prefix: &str) -> String {
        self.sequence += 1;
        let random = uuid::Uuid::new_v4().simple().to_string();
        format!("{prefix}_{:012x}{}", self.sequence, &random[..14])
    }
}

fn millis(timestamp: Option<&str>) -> i64 {
    timestamp.and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp_millis().max(0)).unwrap_or(0)
}

pub fn write_export(session: &Session, path: &Path, id: &str) -> Result<()> {
    if !safe_id(id,"ses") { bail!("invalid OpenCode export identity") }
    let export = encode(session,id)?;
    validate_links(&export)?;
    validate_import_schema(&export)?;
    create_jsonl(path, &[export])
}

fn encode(session: &Session, id: &str) -> Result<Value> {
    let mut ids = Ids { sequence: 0 };
    let mut entries: Vec<Value> = Vec::new();
    let mut parent = None;
    let mut calls: HashMap<String,(usize,usize)> = HashMap::new();
    let model = session.hints.model.as_deref().unwrap_or("unknown");
    let provider = session.hints.provider.as_deref().unwrap_or("unknown");
    for message in &session.messages {
        let time = millis(message.timestamp.as_deref());
        let parts = message.effective_parts();
        if message.role == Role::Tool {
            let mut last_call = None;
            for part in &parts {
                if let ContentPart::ToolResult { tool_use_id, .. } = part {
                    fold_result(part,&mut entries,&calls,time)?;
                    last_call = Some(tool_use_id.as_str());
                } else if let Some(call) = last_call {
                    let &(owner, position) = calls.get(call).ok_or_else(|| anyhow!("orphan OpenCode tool attachment"))?;
                    let owner_id = text(&entries[owner]["info"],"id")?.to_owned();
                    let mut attachment = encode_part(part,time)?;
                    if attachment["type"] != "file" { bail!("unsupported OpenCode tool result attachment") }
                    remap_part(&mut attachment,id,&owner_id,&mut ids)?;
                    let state = &mut entries[owner]["parts"][position]["state"];
                    if state.get("attachments").is_none() { state["attachments"] = json!([]); }
                    state["attachments"].as_array_mut().ok_or_else(|| anyhow!("invalid tool attachments"))?.push(attachment);
                } else { bail!("OpenCode tool content has no result") }
            }
            continue;
        }
        let role = if message.role == Role::User { "user" } else { "assistant" };
        if role == "assistant" && parent.is_none() {
            let seed = ids.next("msg");
            entries.push(json!({"info":{"id":seed,"sessionID":id,"role":"user","time":{"created":time},
                "agent":"build","model":{"modelID":model,"providerID":provider}},"parts":[]}));
            parent = Some(seed);
        }
        let message_id = ids.next("msg");
        let info = if role == "user" {
            parent = Some(message_id.clone());
            json!({"id":message_id,"sessionID":id,"role":role,"time":{"created":time},"agent":"build",
                "model":{"modelID":model,"providerID":provider}})
        } else {
            json!({"id":message_id,"sessionID":id,"role":role,"time":{"created":time},"parentID":parent,
                "modelID":model,"providerID":provider,"mode":"build","agent":"build",
                "path":{"cwd":session.cwd,"root":session.cwd},"cost":0,
                "tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}})
        };
        let index = entries.len();
        entries.push(json!({"info":info,"parts":[]}));
        for part in parts {
            if let ContentPart::Note { kind, text: raw } = &part {
                if kind == "opencode.part-metadata" {
                    let mut original = object_json(raw)?;
                    let array = entries[index]["parts"].as_array_mut().ok_or_else(|| anyhow!("invalid OpenCode parts"))?;
                    let previous = array.last_mut().ok_or_else(|| anyhow!("orphan OpenCode part metadata"))?;
                    let old_id = previous["id"].clone();
                    remap_part(&mut original,id,&message_id,&mut ids)?;
                    original["id"] = old_id;
                    *previous = original;
                    continue;
                }
            }
            if matches!(part,ContentPart::ToolResult {..}) { fold_result(&part,&mut entries,&calls,time)?; continue }
            let mut native = encode_part(&part,time)?;
            remap_part(&mut native,id,&message_id,&mut ids)?;
            let array = entries[index]["parts"].as_array_mut().ok_or_else(|| anyhow!("invalid encoded OpenCode parts"))?;
            if let ContentPart::ToolUse { id: call, .. } = &part {
                calls.insert(call.clone(),(index,array.len()));
            }
            array.push(native);
        }
    }
    let created = millis(session.start_timestamp.as_deref());
    let updated = session.modified_epoch.filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| (v * 1000.0) as i64).unwrap_or(created);
    Ok(json!({"info":{"id":id,"slug":"imported-session","projectID":"global","directory":session.cwd,
        "title":session.summary,"version":"1.19.6","time":{"created":created,"updated":updated}},"messages":entries}))
}

fn encode_part(part: &ContentPart, time: i64) -> Result<Value> {
    Ok(match part {
        ContentPart::Text(text) => json!({"type":"text","text":text}),
        ContentPart::Thinking { text, .. } => json!({"type":"reasoning","text":text,"time":{"start":time,"end":time}}),
        ContentPart::ToolUse { id, name, input } => {
            if !input.is_object() { bail!("OpenCode tool input must be an object") }
            json!({"type":"tool","callID":id,"tool":name,"state":{"status":"pending","input":input,"raw":input.to_string()}})
        }
        ContentPart::Image { mime_type, data } => {
            let mime = mime_type.as_deref().unwrap_or("image/png");
            let url = if data.starts_with("data:") || data.contains("://") { data.clone() }
                else { format!("data:{mime};base64,{data}") };
            json!({"type":"file","mime":mime,"url":url})
        }
        ContentPart::Note {kind,text} if kind == RAW_PART => {
            let mut value = object_json(text)?;
            if value["type"] == "compaction" {
                value.as_object_mut().ok_or_else(|| anyhow!("invalid compaction part"))?.remove("tail_start_id");
            }
            value
        }
        ContentPart::Note {kind,text} => json!({"type":"text","text":format!("[{kind}]\n{text}"),"synthetic":true}),
        ContentPart::ToolResult {..} => bail!("unpaired OpenCode tool result"),
    })
}

fn remap_part(part: &mut Value, session: &str, message: &str, ids: &mut Ids) -> Result<()> {
    if !part.is_object() { bail!("invalid OpenCode part") }
    part["id"] = json!(ids.next("prt"));
    part["sessionID"] = json!(session);
    part["messageID"] = json!(message);
    if let Some(attachments) = part.get_mut("state").and_then(|s| s.get_mut("attachments")).and_then(Value::as_array_mut) {
        for attachment in attachments { remap_part(attachment,session,message,ids)?; }
    }
    Ok(())
}

fn fold_result(part: &ContentPart, entries: &mut [Value], calls: &HashMap<String,(usize,usize)>, time: i64) -> Result<()> {
    let ContentPart::ToolResult {tool_use_id,content,is_error} = part else { return Ok(()) };
    let &(message,index) = calls.get(tool_use_id).ok_or_else(|| anyhow!("OpenCode tool result has no prior invocation"))?;
    let state = &mut entries[message]["parts"][index]["state"];
    let input = state["input"].clone();
    *state = if *is_error {
        json!({"status":"error","input":input,"error":content,"time":{"start":time,"end":time}})
    } else {
        json!({"status":"completed","input":input,"output":content,"title":"","metadata":{},"time":{"start":time,"end":time}})
    };
    Ok(())
}
fn validate_import_schema(export: &Value) -> Result<()> {
    let info = &export["info"];
    for field in ["slug","projectID","directory","title","version"] { text(info,field)?; }
    for field in ["created","updated"] {
        if info["time"][field].as_u64().is_none() { bail!("invalid OpenCode session time") }
    }
    for entry in export["messages"].as_array().ok_or_else(|| anyhow!("missing OpenCode messages"))? {
        let message = &entry["info"];
        text(message,"agent")?;
        if message["time"]["created"].as_f64().is_none_or(|n| !n.is_finite() || n < 0.0) { bail!("invalid OpenCode message time") }
        if message["role"] == "user" {
            text(&message["model"],"modelID")?;
            text(&message["model"],"providerID")?;
        } else {
            for field in ["modelID","providerID","mode"] { text(message,field)?; }
            for field in ["cwd","root"] { text(&message["path"],field)?; }
            for field in ["input","output","reasoning"] {
                if !message["tokens"][field].is_number() { bail!("invalid OpenCode assistant tokens") }
            }
            for field in ["read","write"] {
                if !message["tokens"]["cache"][field].is_number() { bail!("invalid OpenCode cache tokens") }
            }
            if !message["cost"].is_number() { bail!("invalid OpenCode assistant cost") }
        }
        for part in entry["parts"].as_array().ok_or_else(|| anyhow!("missing OpenCode parts"))? {
            validate_import_part(part)?;
        }
    }
    Ok(())
}

fn validate_import_part(part: &Value) -> Result<()> {
    match text(part,"type")? {
        "text" => { text(part,"text")?; }
        "reasoning" => {
            text(part,"text")?;
            if part["time"]["start"].as_u64().is_none() { bail!("invalid OpenCode reasoning time") }
        }
        "file" => { text(part,"mime")?; text(part,"url")?; }
        "tool" => {
            text(part,"callID")?; text(part,"tool")?;
            let state = &part["state"];
            if !state["input"].is_object() { bail!("invalid OpenCode tool input") }
            match text(state,"status")? {
                "pending" => { text(state,"raw")?; }
                "running" => { if state["time"]["start"].as_u64().is_none() { bail!("invalid OpenCode tool time") } }
                "completed" | "error" => {
                    text(state,if state["status"] == "error" {"error"} else {"output"})?;
                    if state["time"]["start"].as_u64().is_none() || state["time"]["end"].as_u64().is_none() { bail!("invalid OpenCode tool time") }
                    if state["status"] == "completed" {
                        text(state,"title")?;
                        if !state["metadata"].is_object() { bail!("invalid OpenCode tool metadata") }
                    }
                    if let Some(attachments) = state["attachments"].as_array() {
                        for attachment in attachments { validate_import_part(attachment)?; }
                    }
                }
                _ => bail!("unsupported OpenCode tool state"),
            }
        }
        "compaction" if part["auto"].is_boolean() => (),
        "step-start" => (),
        "step-finish" if part["reason"].is_string() && part["cost"].is_number() && part["tokens"].is_object() => (),
        "snapshot" if part["snapshot"].is_string() => (),
        "patch" if part["hash"].is_string() && part["files"].is_array() => (),
        "agent" if part["name"].is_string() => (),
        "retry" if part["attempt"].is_number() && part["error"].is_object() && part["time"]["created"].is_number() => (),
        "subtask" if part["prompt"].is_string() && part["description"].is_string() && part["agent"].is_string() => (),
        _ => bail!("unsupported or invalid OpenCode native part"),
    }
    Ok(())
}

fn ensure_database_directory(directory: &Path) -> Result<()> {
    if directory.components().any(|c| matches!(c,Component::ParentDir | Component::CurDir)) {
        bail!("unsafe OpenCode import database directory");
    }
    if std::fs::symlink_metadata(directory).is_ok() {
        open_directory_under_root(directory,directory).map_err(|_| anyhow!("unsafe OpenCode import database directory"))?;
        return Ok(());
    }
    let parent = directory.parent().ok_or_else(|| anyhow!("invalid OpenCode database directory"))?;
    ensure_database_directory(parent)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let trusted = open_directory_under_root(parent,parent).map_err(|_| anyhow!("unsafe OpenCode database ancestor"))?;
        let name = directory.file_name().ok_or_else(|| anyhow!("invalid OpenCode database directory"))?;
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| anyhow!("invalid OpenCode database directory"))?;
        // The trusted parent descriptor prevents symlink traversal during creation.
        let created = unsafe { libc::mkdirat(trusted.as_raw_fd(),name.as_ptr(),0o700) };
        if created != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
            bail!("creating OpenCode database directory failed");
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir(directory).map_err(|_| anyhow!("creating OpenCode database directory failed"))?;
    open_directory_under_root(directory,directory).map_err(|_| anyhow!("unsafe created OpenCode database directory"))?;
    Ok(())
}

/// Native import can leave a partial fresh session on failure; never delete or retry it.
pub fn import_export(export: &Path, cwd: &Path, database: &Path) -> Result<PathBuf> {
    let document = read_export(export)?;
    validate_import_schema(&document)?;
    validate_links(&document)?;
    decode(&document,export)?;
    let id = text(&document["info"],"id")?;
    let database = if database.is_absolute() { database.to_owned() }
        else { std::env::current_dir().context("resolving OpenCode import database")?.join(database) };
    let parent = database.parent().ok_or_else(|| anyhow!("OpenCode import database has no parent"))?;
    ensure_database_directory(parent)?;
    if database.try_exists().context("checking OpenCode import target")? {
        let connection = open_store(&database)?;
        let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM session WHERE id=?1)",[id],|r| r.get(0))
            .map_err(|_| anyhow!("checking OpenCode import collision failed"))?;
        if exists { bail!("OpenCode import session identity already exists") }
        reject_row_collisions(&connection,&document)?;
    } else if std::fs::symlink_metadata(&database).is_ok() { bail!("unsafe OpenCode import target") }
    open_directory_under_root(cwd,cwd).map_err(|_| anyhow!("unsafe OpenCode import working directory"))?;
    // Copy the validated document to a private regular file: no mutable external input at spawn.
    let private = tempfile::tempdir().context("creating private OpenCode import directory")?;
    let input = private.path().join("session.json");
    create_jsonl(&input,&[&document])?;
    let stdout = tempfile::tempfile().context("creating private OpenCode stdout capture")?;
    let stderr = tempfile::tempfile().context("creating private OpenCode stderr capture")?;
    let mut child = Command::new("opencode").arg("--pure").arg("import").arg(&input)
        .current_dir(cwd).env("OPENCODE_DB",&database).stdin(Stdio::null())
        .stdout(Stdio::from(stdout)).stderr(Stdio::from(stderr)).spawn()
        .map_err(|_| anyhow!("starting native OpenCode import failed"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                if outcome.is_err() { bail!("waiting for native OpenCode import failed; a partial session may remain") }
                bail!("native OpenCode import timed out; a partial session may remain");
            }
        }
    };
    if !status.success() { bail!("native OpenCode import failed; a partial session may remain") }
    let connection = open_store(&database)?;
    let imported = hydrate(&connection,id)?;
    validate_links(&imported)?;
    let count = |value: &Value| -> (usize,usize) {
        value["messages"].as_array().map_or((0,0), |entries|
            (entries.len(),entries.iter().map(|e| e["parts"].as_array().map_or(0,Vec::len)).sum()))
    };
    if count(&document) != count(&imported) { bail!("native OpenCode import incomplete; a partial session may remain") }
    Ok(database.join(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Session {
        Session { tool: SourceTool::OpenCode, session_id: "ses_synthetic".into(), cwd: PathBuf::from("/synthetic"),
            start_timestamp: None, summary: "Synthetic".into(), messages: vec![Message::plain(Role::User,"Hello",None)],
            path: PathBuf::new(), modified_epoch: None, hints: SessionHints::default() }
    }

    #[test]
    fn portable_roundtrip_and_exclusive_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("export.json");
        write_export(&sample(),&path,"ses_synthetic").unwrap();
        assert!(is_export(&path));
        assert_eq!(parse(&path).unwrap().messages[0].text,"Hello");
        assert!(write_export(&sample(),&path,"ses_synthetic").is_err());
    }

    #[test]
    fn monotonic_parts_and_tool_results() {
        let mut session = sample();
        session.messages.push(Message::from_parts(Role::Assistant,vec![
            ContentPart::Text("Before".into()), ContentPart::ToolUse { id:"call".into(),name:"read".into(),input:json!({}) },
            ContentPart::Text("After".into())],None));
        session.messages.push(Message::from_parts(Role::Tool,vec![ContentPart::ToolResult {
            tool_use_id:"call".into(),content:"Result".into(),is_error:false }],None));
        let export = encode(&session,"ses_synthetic").unwrap();
        validate_links(&export).unwrap();
        let parts = export["messages"][1]["parts"].as_array().unwrap();
        assert!(parts.windows(2).all(|w| w[0]["id"].as_str() < w[1]["id"].as_str()));
        assert_eq!(parts[1]["state"]["output"],"Result");
    }

    #[test]
    fn invalid_message_reference_and_revert() {
        let mut export = encode(&sample(),"ses_synthetic").unwrap();
        export["info"]["revert"] = json!({"messageID":export["messages"][0]["info"]["id"]});
        assert!(decode(&export,Path::new("synthetic.json")).unwrap().messages.is_empty());
        export["messages"][0]["parts"][0]["messageID"] = json!("msg_other");
        assert!(validate_links(&export).is_err());
    }

    #[test]
    fn session_named_export_is_not_a_native_selector() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.json");
        write_export(&sample(),&path,"ses_synthetic").unwrap();
        assert!(native_reference(&path).unwrap().is_none());
        assert_eq!(parse(&path).unwrap().messages[0].text,"Hello");
    }

    #[test]
    fn sqlite_hydration_orders_and_injects_identity() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE session(id TEXT,directory TEXT,title TEXT,time_created INTEGER,time_updated INTEGER);\
            CREATE TABLE message(id TEXT,session_id TEXT,time_created INTEGER,data TEXT);\
            CREATE TABLE part(id TEXT,session_id TEXT,message_id TEXT,data TEXT);\
            INSERT INTO session VALUES('ses_synthetic','/synthetic','Synthetic',0,1000);\
            INSERT INTO message VALUES('msg_b','ses_synthetic',2,'{\"role\":\"user\",\"time\":{\"created\":2}}');\
            INSERT INTO message VALUES('msg_a','ses_synthetic',1,'{\"role\":\"user\",\"time\":{\"created\":1}}');\
            INSERT INTO part VALUES('prt_b','ses_synthetic','msg_a','{\"type\":\"text\",\"text\":\"B\"}');\
            INSERT INTO part VALUES('prt_a','ses_synthetic','msg_a','{\"type\":\"text\",\"text\":\"A\"}');").unwrap();
        let export = hydrate(&connection,"ses_synthetic").unwrap();
        assert_eq!(export["messages"][0]["info"]["id"],"msg_a");
        assert_eq!(export["messages"][0]["parts"][0]["id"],"prt_a");
        assert_eq!(decode(&export,Path::new("synthetic")).unwrap().messages[0].text,"A\nB");
        assert!(reject_row_collisions(&connection,&export).is_err());
    }
}

#[test]
fn opencode_creates_missing_database_ancestors() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("data/opencode");
    ensure_database_directory(&nested).unwrap();
    assert!(nested.is_dir());
    #[cfg(unix)]
    {
        let link = temporary.path().join("linked");
        std::os::unix::fs::symlink(&nested,&link).unwrap();
        assert!(ensure_database_directory(&link.join("child")).is_err());
    }
}
