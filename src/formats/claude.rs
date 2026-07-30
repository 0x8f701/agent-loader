//! Claude Code session adapter.
//!
//! Claude stores a session as an append-only JSONL log at
//! `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl`, one JSON object
//! per line. Every line is a JSON object keyed by `type`. Graph records
//! (`user`, `assistant`, `system`, `attachment`) carry a `uuid` and an
//! optional `parentUuid` forming a lineage forest; auxiliary/state records
//! (`last-prompt`, `mode`, `permission-mode`, `ai-title`,
//! `file-history-snapshot`, `queue-operation`) carry no lineage edges.
//!
//! The active conversation path is reconstructed exactly as the reference
//! implementation (`scripts/sessions`): the leaf is the most recent
//! `last-prompt.leafUuid`, falling back to the last `user`/`assistant` record
//! by file order; the chain is rebuilt by following `parentUuid` upward to
//! the root with a cycle guard, then reversed to chronological order. UUID
//! -bearing `system`/`attachment` nodes participate in the traversal as graph
//! hops, but only non-sidechain, non-meta `user`/`assistant` first-text is
//! projected.
//!
//! Parsing is typed and forward-compatible: known record types deserialize
//! into strongly typed structs with extension maps for unknown envelope
//! fields, and unknown record/attachment/block variants are retained
//! verbatim alongside their typed view. Malformed JSONL lines are skipped
//! without aborting the parse (delegated to the shared `read_jsonl_values`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::domain::{Message, Session, SourceTool};
use crate::formats::{normalize, parsed_message, read_jsonl_values, summarize_messages};

/// Parse a Claude Code session transcript into a lossy `Session`.
pub fn parse(path: &Path) -> Result<Session> {
    let values = read_jsonl_values(path)?;
    let modified_epoch = file_mtime(path);
    let records: Vec<Record> = values.iter().map(Record::from_value).collect();

    // Metadata scan — mirrors `parse_claude` (`scripts/sessions`): session id
    // and cwd default to filename/parent-dir and are overwritten by the last
    // non-empty in-record value; the start timestamp is the first non-empty
    // timestamp; the summary is the last record contributing any of
    // `aiTitle`/`summary`/`title` (priority order within that record).
    let mut session_id = fallback_id(path);
    let mut cwd = fallback_cwd(path);
    let mut start_timestamp: Option<String> = None;
    let mut summary: Option<String> = None;
    for record in &records {
        if let Some(sid) = record.session_id() {
            session_id = sid;
        }
        if let Some(value) = record.cwd() {
            cwd = PathBuf::from(value);
        }
        if start_timestamp.is_none() {
            if let Some(timestamp) = record.timestamp() {
                start_timestamp = Some(timestamp);
            }
        }
        if let Some(source) = record.summary_source() {
            summary = Some(normalize(&source, 100));
        }
    }

    let messages = project_active_messages(&records);
    let summary = summary.unwrap_or_else(|| summarize_messages(&messages));

    Ok(Session {
        tool: SourceTool::Claude,
        session_id,
        cwd,
        start_timestamp,
        summary,
        messages,
        path: path.to_path_buf(),
        modified_epoch,
    })
}

/// Project the active lineage chain into first-text `Message`s.
///
/// The leaf is the most recent `last-prompt.leafUuid` resolvable to a graph
/// node, else the last non-sidechain, non-meta `user`/`assistant` record by
/// file order. The chain is walked upward via `parentUuid` (cycle-guarded),
/// reversed to chronological order, then projected: sidechain/meta nodes and
/// non-`user`/`assistant` payloads are dropped, leaving only the lossy
/// first-text conversation.
fn project_active_messages(records: &[Record]) -> Vec<Message> {
    // Graph nodes indexed by uuid (last occurrence wins, matching the
    // reference dict comprehension).
    let mut by_uuid: HashMap<String, usize> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(uuid) = record.uuid() {
            by_uuid.insert(uuid, index);
        }
    }

    // Leaf = most recent `last-prompt.leafUuid`.
    let leaf_uuid = records.iter().rev().find_map(|record| match &record.kind {
        RecordKind::LastPrompt(last_prompt) => {
            last_prompt.leaf_uuid.clone().filter(|value| !value.is_empty())
        }
        _ => None,
    });

    // Fallback: last non-sidechain, non-meta user/assistant by file order.
    // (The reference fallback does not filter, but sidechain/meta records must
    // not anchor the resume chain — `filtered from /resume: isSidechain=true`.)
    let leaf_uuid = match leaf_uuid {
        Some(uuid) if by_uuid.contains_key(&uuid) => Some(uuid),
        _ => records.iter().rev().find_map(|record| {
            let is_message_record =
                matches!(record.type_tag(), Some("user") | Some("assistant"));
            if is_message_record && !record.is_sidechain() && !record.is_meta() {
                record.uuid()
            } else {
                None
            }
        }),
    };

    // Walk parentUuid upward to the root, cycle-guarded, then reverse.
    let mut path_indices: Vec<usize> = Vec::new();
    let mut visited: HashMap<String, ()> = HashMap::new();
    let mut cursor = leaf_uuid;
    while let Some(uuid) = cursor {
        let Some(&index) = by_uuid.get(&uuid) else { break };
        if visited.insert(uuid.clone(), ()).is_some() {
            break;
        }
        path_indices.push(index);
        let parent = records[index].parent_uuid();
        cursor = parent.filter(|value| by_uuid.contains_key(value));
    }
    path_indices.reverse();

    // Project: skip sidechain/meta, then keep recognized user/assistant
    // first-text via the shared `parsed_message` helper.
    path_indices
        .iter()
        .map(|&index| &records[index])
        .filter(|record| !record.is_sidechain() && !record.is_meta())
        .filter_map(|record| {
            let (role, content, timestamp) = record.message_payload()?;
            parsed_message(role, content, timestamp)
        })
        .collect()
}

/// Derive a session id from the file stem when no in-record id is present.
fn fallback_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .or_else(|| path.file_name().and_then(|value| value.to_str()))
        .unwrap_or("")
        .to_owned()
}

/// Derive the working directory from the file's parent when no in-record cwd
/// is present (or it is empty).
fn fallback_cwd(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

/// File mtime as a POSIX epoch second, when statable.
fn file_mtime(path: &Path) -> Option<f64> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0)
    }
    #[cfg(not(unix))]
    {
        metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_secs_f64())
    }
}

// ---------------------------------------------------------------------------
// Typed, forward-compatible record model.
// ---------------------------------------------------------------------------

/// One parsed JSONL line: a typed view (`kind`) paired with the exact raw
/// value. The raw value is authoritative for round-trip and for fields the
/// typed layer does not model; `kind` exposes the documented structure.
#[derive(Debug, Clone)]
pub struct Record {
    pub kind: RecordKind,
    pub raw: Value,
}

/// Typed record variants keyed by the `type` discriminator.
#[derive(Debug, Clone)]
pub enum RecordKind {
    User(UserRecord),
    Assistant(AssistantRecord),
    System(SystemRecord),
    Attachment(AttachmentRecord),
    FileHistorySnapshot(FileHistorySnapshot),
    QueueOperation(QueueOperation),
    LastPrompt(LastPrompt),
    Mode(Mode),
    PermissionMode(PermissionMode),
    AiTitle(AiTitle),
    /// Unknown record type — `raw` retains the original object verbatim.
    Unknown,
}

impl Record {
    /// Deserialize a JSON value into a typed record, retaining the raw value.
    /// A known type that fails to deserialize (e.g. an unexpected field type)
    /// degrades gracefully to `Unknown` so the line is never lost.
    pub fn from_value(value: &Value) -> Self {
        let type_tag = value
            .as_object()
            .and_then(|object| object.get("type"))
            .and_then(Value::as_str);
        let kind = match type_tag {
            Some("user") => from_value::<UserRecord>(value).map(RecordKind::User),
            Some("assistant") => from_value::<AssistantRecord>(value).map(RecordKind::Assistant),
            Some("system") => from_value::<SystemRecord>(value).map(RecordKind::System),
            Some("attachment") => from_value::<AttachmentRecord>(value).map(RecordKind::Attachment),
            Some("file-history-snapshot") => {
                from_value::<FileHistorySnapshot>(value).map(RecordKind::FileHistorySnapshot)
            }
            Some("queue-operation") => {
                from_value::<QueueOperation>(value).map(RecordKind::QueueOperation)
            }
            Some("last-prompt") => from_value::<LastPrompt>(value).map(RecordKind::LastPrompt),
            Some("mode") => from_value::<Mode>(value).map(RecordKind::Mode),
            Some("permission-mode") => {
                from_value::<PermissionMode>(value).map(RecordKind::PermissionMode)
            }
            Some("ai-title") => from_value::<AiTitle>(value).map(RecordKind::AiTitle),
            _ => Some(RecordKind::Unknown),
        }
        .unwrap_or(RecordKind::Unknown);
        Record {
            kind,
            raw: value.clone(),
        }
    }

    /// The `type` discriminator, read from the raw value.
    pub fn type_tag(&self) -> Option<&str> {
        self.raw
            .as_object()
            .and_then(|object| object.get("type"))
            .and_then(Value::as_str)
    }

    fn graph(&self) -> Option<&GraphEnvelope> {
        match &self.kind {
            RecordKind::User(record) => Some(&record.envelope),
            RecordKind::Assistant(record) => Some(&record.envelope),
            RecordKind::System(record) => Some(&record.envelope),
            RecordKind::Attachment(record) => Some(&record.envelope),
            _ => None,
        }
    }

    fn raw_str(&self, key: &str) -> Option<String> {
        self.raw
            .as_object()
            .and_then(|object| object.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    fn raw_bool(&self, key: &str) -> bool {
        self.raw
            .as_object()
            .and_then(|object| object.get(key))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Own node uuid (non-empty). Graph records read the typed envelope;
    /// unknown records read the raw value so uuid-bearing future types still
    /// participate in the lineage graph.
    pub fn uuid(&self) -> Option<String> {
        if let Some(envelope) = self.graph() {
            return envelope.uuid.clone().filter(|value| !value.is_empty());
        }
        self.raw_str("uuid")
    }

    /// Parent uuid link (non-empty string only). JSON `null` and absent both
    /// terminate the upward walk.
    pub fn parent_uuid(&self) -> Option<String> {
        if let Some(envelope) = self.graph() {
            return envelope.parent_uuid.clone().filter(|value| !value.is_empty());
        }
        self.raw_str("parentUuid")
    }

    /// Sidechain/subagent marker.
    pub fn is_sidechain(&self) -> bool {
        if let Some(envelope) = self.graph() {
            return envelope.is_sidechain.unwrap_or(false);
        }
        self.raw_bool("isSidechain")
    }

    /// Meta/injected-message marker.
    pub fn is_meta(&self) -> bool {
        if let Some(envelope) = self.graph() {
            return envelope.is_meta.unwrap_or(false);
        }
        self.raw_bool("isMeta")
    }

    /// Session id, preferring `sessionId` (camel) then `session_id` (snake).
    pub fn session_id(&self) -> Option<String> {
        if let Some(envelope) = self.graph() {
            return envelope.session_id();
        }
        match &self.kind {
            RecordKind::LastPrompt(record) => record.session_id.clone(),
            RecordKind::Mode(record) => record.session_id.clone(),
            RecordKind::PermissionMode(record) => record.session_id.clone(),
            RecordKind::AiTitle(record) => record.session_id.clone(),
            RecordKind::QueueOperation(record) => record.session_id.clone(),
            _ => self.raw_str("sessionId").or_else(|| self.raw_str("session_id")),
        }
        .filter(|value| !value.is_empty())
    }

    /// Canonical cwd (non-empty).
    pub fn cwd(&self) -> Option<String> {
        if let Some(envelope) = self.graph() {
            return envelope.cwd.clone().filter(|value| !value.is_empty());
        }
        self.raw_str("cwd")
    }

    /// Record timestamp (non-empty).
    pub fn timestamp(&self) -> Option<String> {
        if let Some(envelope) = self.graph() {
            return envelope.timestamp.clone().filter(|value| !value.is_empty());
        }
        self.raw_str("timestamp")
    }

    /// Summary source with `aiTitle` > `summary` > `title` priority (raw
    /// value, pre-normalization), read from any record.
    pub fn summary_source(&self) -> Option<String> {
        self.raw_str("aiTitle")
            .or_else(|| self.raw_str("summary"))
            .or_else(|| self.raw_str("title"))
    }

    /// Conversation payload `(role, content, timestamp)` for projection,
    /// read from the raw `message` object. Non-message records yield `None`.
    fn message_payload(&self) -> Option<(Option<&str>, Option<&Value>, Option<&str>)> {
        let message = self.raw.as_object()?.get("message")?.as_object()?;
        let role = message.get("role").and_then(Value::as_str);
        let content = message.get("content");
        let timestamp = self
            .raw
            .as_object()
            .and_then(|object| object.get("timestamp"))
            .and_then(Value::as_str);
        Some((role, content, timestamp))
    }
}

fn from_value<T: for<'de> Deserialize<'de>>(value: &Value) -> Option<T> {
    serde_json::from_value(value.clone()).ok()
}

/// Shared graph-envelope fields carried by `user`/`assistant`/`system`/
/// `attachment` records. Every field is optional and defaults silently so a
/// missing key never fails the parse.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEnvelope {
    #[serde(default, rename = "type")]
    pub r#type: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub parent_uuid: Option<String>,
    #[serde(default)]
    pub is_sidechain: Option<bool>,
    #[serde(default)]
    pub is_meta: Option<bool>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default, rename = "session_id")]
    pub session_id_snake: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub user_type: Option<String>,
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub git_branch: Option<String>,
}

impl GraphEnvelope {
    /// Session id with `sessionId` (camel) preferred over `session_id`
    /// (snake) — the assistant record carries both.
    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .clone()
            .filter(|value| !value.is_empty())
            .or_else(|| self.session_id_snake.clone().filter(|value| !value.is_empty()))
    }
}

/// `type: "user"` record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRecord {
    #[serde(flatten)]
    pub envelope: GraphEnvelope,
    #[serde(default)]
    pub message: Option<MessageBody>,
    #[serde(default)]
    pub prompt_id: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub origin: Option<Value>,
    #[serde(default)]
    pub prompt_source: Option<String>,
    #[serde(default)]
    pub is_compact_summary: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `type: "assistant"` record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantRecord {
    #[serde(flatten)]
    pub envelope: GraphEnvelope,
    #[serde(default)]
    pub message: Option<MessageBody>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub api_error_status: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub is_api_error_message: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `type: "system"` record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemRecord {
    #[serde(flatten)]
    pub envelope: GraphEnvelope,
    /// `turn_duration` | `local_command` | `informational` | `permission_retry`
    /// | `bridge_status` | …
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub message_count: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `type: "attachment"` record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRecord {
    #[serde(flatten)]
    pub envelope: GraphEnvelope,
    #[serde(default)]
    pub attachment: Option<AttachmentPayload>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Attachment payload: a typed view (`kind`) paired with the raw object so
/// unknown `attachment.type` values round-trip verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentPayload {
    pub kind: AttachmentKind,
    pub raw: Value,
}

/// Known `attachment.type` variants. Unknown tags collapse to `Unknown` while
/// the full object is retained on the enclosing `AttachmentPayload`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentKind {
    #[serde(rename_all = "camelCase")]
    AgentListingDelta {
        #[serde(default)]
        added_types: Vec<String>,
        #[serde(default)]
        added_lines: Vec<String>,
        #[serde(default)]
        removed_types: Option<Vec<String>>,
        #[serde(default)]
        is_initial: Option<bool>,
        #[serde(default)]
        show_concurrency_note: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    SkillListing {
        content: String,
        #[serde(default)]
        names: Option<Vec<String>>,
        #[serde(default)]
        skill_count: Option<u64>,
        #[serde(default)]
        is_initial: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    TaskReminder {
        content: Value,
        item_count: u64,
    },
    #[serde(rename_all = "camelCase")]
    QueuedCommand {
        #[serde(default)]
        command_mode: Option<String>,
        #[serde(default)]
        batched_relay_prompts: Option<Vec<String>>,
        #[serde(default)]
        origin: Option<Value>,
        #[serde(default)]
        rendered_by_batch_head: Option<bool>,
        #[serde(default)]
        is_meta: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    HookSuccess {
        hook_name: String,
        hook_event: String,
        command: String,
        duration_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    HookNonBlockingError {
        hook_name: String,
        hook_event: String,
        command: String,
        duration_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    HookErrorDuringExecution {
        hook_name: String,
        hook_event: String,
        command: String,
        duration_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    HookCancelled {
        hook_name: String,
        hook_event: String,
        command: String,
        duration_ms: u64,
        timed_out: bool,
        timeout_ms: u64,
    },
    McpResource {
        server: String,
        uri: String,
        #[serde(default)]
        contents: Vec<Value>,
    },
    CompactFileReference {
        filename: String,
    },
    #[serde(rename_all = "camelCase")]
    AudioTranscript {
        filename: String,
        #[serde(default)]
        error: Option<String>,
    },
    TodoReminder {
        #[serde(default)]
        content: Vec<Value>,
    },
    #[serde(rename_all = "camelCase")]
    ToolSearchUsageReminder {
        #[serde(default)]
        undiscovered_tool_names: Vec<String>,
        #[serde(default)]
        undiscovered_count: u64,
    },
    #[serde(rename_all = "camelCase")]
    AgentMention {
        agent_type: String,
    },
    #[serde(rename_all = "camelCase")]
    DynamicSkill {
        skill_dir: String,
        #[serde(default)]
        skill_names: Vec<String>,
    },
    ReadTruncationNotice {
        banner: String,
    },
    Notebook {
        file: Value,
    },
    RelevantMemories {
        content: Value,
    },
    /// Unknown `attachment.type` — payload retained on the enclosing struct.
    #[serde(other)]
    Unknown,
}

impl<'de> Deserialize<'de> for AttachmentPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        let kind = serde_json::from_value::<AttachmentKind>(raw.clone())
            .unwrap_or(AttachmentKind::Unknown);
        Ok(AttachmentPayload { kind, raw })
    }
}

impl Serialize for AttachmentPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.raw.serialize(serializer)
    }
}

/// `type: "last-prompt"` — the leaf pointer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastPrompt {
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub leaf_uuid: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `type: "mode"`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mode {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `type: "permission-mode"`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionMode {
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `type: "ai-title"` — AI-generated session title (summary source).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiTitle {
    #[serde(default)]
    pub ai_title: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `type: "file-history-snapshot"`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistorySnapshot {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub snapshot: Option<Snapshot>,
    #[serde(default)]
    pub is_snapshot_update: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Tracked-file snapshot embedded in `file-history-snapshot`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub tracked_file_backups: Value,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `type: "queue-operation"`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueOperation {
    #[serde(default)]
    pub operation: QueueOp,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Queue operation discriminator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueOp {
    #[default]
    Unknown,
    Enqueue,
    Dequeue,
}

// ---------------------------------------------------------------------------
// Message body and content blocks.
// ---------------------------------------------------------------------------

/// The `message` object on `user`/`assistant` records.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBody {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Message content: a bare string or an array of typed blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Blocks(Vec<Block>),
    Text(String),
}

/// One content block: a typed view (`kind`) paired with the raw object so
/// unknown block types round-trip verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: BlockKind,
    pub raw: Value,
}

/// Known content-block variants. `[UNVERIFIED-ON-DISK]` for `tool_use`/
/// `tool_result`/`thinking` (shapes recovered from the CLI binary); unknown
/// block types collapse to `Unknown` while the raw block is retained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockKind {
    Text { text: String },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(default)]
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    /// Unknown block type — raw block retained on the enclosing `Block`.
    #[serde(other)]
    Unknown,
}

/// `tool_result.content`: a bare string or an array of blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Blocks(Vec<Block>),
    Text(String),
}

impl<'de> Deserialize<'de> for Block {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        let kind = serde_json::from_value::<BlockKind>(raw.clone()).unwrap_or(BlockKind::Unknown);
        Ok(Block { kind, raw })
    }
}

impl Serialize for Block {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.raw.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;
    use crate::domain::Role;

    fn write_session(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("temp file");
        for line in lines {
            writeln!(file, "{line}").expect("write line");
        }
        file.flush().expect("flush");
        file
    }

    /// User record helper. `content` is a JSON fragment (string or array).
    fn user(uuid: &str, parent: Option<&str>, content: &str) -> String {
        let parent = match parent {
            Some(value) => format!("\"{value}\""),
            None => "null".to_owned(),
        };
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent},"timestamp":"2026-07-21T06:13:11.040Z","sessionId":"s1","cwd":"/workspace/project","message":{{"role":"user","content":{content}}}}}"#
        )
    }

    /// Assistant record helper.
    fn assistant(uuid: &str, parent: &str, content: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}","timestamp":"2026-07-21T06:13:12.040Z","sessionId":"s1","cwd":"/workspace/project","message":{{"role":"assistant","content":{content}}}}}"#
        )
    }

    /// UUID-bearing system record (a graph hop that projects no message).
    fn system(uuid: &str, parent: &str, subtype: &str) -> String {
        format!(
            r#"{{"type":"system","uuid":"{uuid}","parentUuid":"{parent}","timestamp":"2026-07-21T06:13:13.040Z","sessionId":"s1","cwd":"/workspace/project","subtype":"{subtype}","durationMs":2844,"messageCount":4}}"#
        )
    }

    fn last_prompt(leaf_uuid: &str) -> String {
        format!(
            r#"{{"type":"last-prompt","lastPrompt":"hi","leafUuid":"{leaf_uuid}","sessionId":"s1"}}"#
        )
    }

    #[test]
    fn parses_active_lineage_from_last_prompt_leaf() {
        // last-prompt points at the system/turn_duration node; the walk
        // traverses turn_duration -> assistant -> user, projecting the two
        // messages. The system node is a graph hop that projects nothing.
        let file = write_session(&[
            &user("u1", None, r#""hi""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"Hi there"}]"#),
            &system("t1", "a1", "turn_duration"),
            &last_prompt("t1"),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.tool, SourceTool::Claude);
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.cwd, PathBuf::from("/workspace/project"));
        assert_eq!(
            session.start_timestamp.as_deref(),
            Some("2026-07-21T06:13:11.040Z")
        );
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["hi", "Hi there"]);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.summary, "hi");
        assert!(session.modified_epoch.is_some());
    }

    #[test]
    fn fallback_leaf_uses_last_user_assistant_when_no_last_prompt() {
        // No last-prompt record: the leaf falls back to the last user/assistant
        // by file order (the assistant), then walks to its parent.
        let file = write_session(&[
            &user("u1", None, r#""first""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"reply"}]"#),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["first", "reply"]);
    }

    #[test]
    fn fallback_leaf_ignores_unresolvable_last_prompt() {
        // last-prompt points at a uuid absent from the graph: fall back.
        let file = write_session(&[
            &user("u1", None, r#""real""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"ans"}]"#),
            &last_prompt("does-not-exist"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["real", "ans"]);
    }

    #[test]
    fn sidechain_records_are_excluded_from_chain_and_messages() {
        // A trailing sidechain assistant must not anchor the resume chain nor
        // project a message.
        let sidechain = r#"{"type":"assistant","uuid":"sc1","parentUuid":"u1","isSidechain":true,"timestamp":"2026-07-21T06:13:14.040Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"secret"}]}}"#;
        let file = write_session(&[
            &user("u1", None, r#""hi""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"ok"}]"#),
            sidechain,
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["hi", "ok"]);
        assert!(!session.messages.iter().any(|m| m.text == "secret"));
    }

    #[test]
    fn meta_records_are_excluded_from_messages_but_traversed() {
        // A meta user record sits on the chain (links parent) but projects none.
        let meta_user = r#"{"type":"user","uuid":"m1","parentUuid":null,"isMeta":true,"timestamp":"2026-07-21T06:13:10.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"user","content":"injected"}}"#;
        let file = write_session(&[
            meta_user,
            &user("u1", Some("m1"), r#""real""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"ans"}]"#),
            &last_prompt("a1"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        // meta "injected" is dropped; the chain still reaches u1 via m1.
        assert_eq!(texts, ["real", "ans"]);
    }

    #[test]
    fn malformed_lines_are_isolated() {
        // A garbage line and a bare JSON array line are skipped without
        // aborting the parse (read_jsonl_values isolates them).
        let file = write_session(&[
            "this is not json at all",
            r#"["bare","array"]"#,
            &user("u1", None, r#""hi""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"hey"}]"#),
            "   ",
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["hi", "hey"]);
    }

    #[test]
    fn unknown_record_type_is_retained_and_round_trips() {
        let line = r#"{"type":"future-thing","x":1,"uuid":"f1","parentUuid":null,"sessionId":"s1","cwd":"/tmp"}"#;
        let value: Value = serde_json::from_str(line).expect("json");
        let record = Record::from_value(&value);
        assert!(matches!(record.kind, RecordKind::Unknown));
        assert_eq!(record.type_tag(), Some("future-thing"));
        // Raw value round-trips verbatim.
        let round_tripped = serde_json::to_value(&record.raw).expect("serialize");
        assert_eq!(round_tripped, value);
        // The unknown uuid-bearing record still participates in the graph.
        assert_eq!(record.uuid().as_deref(), Some("f1"));
        assert_eq!(record.session_id().as_deref(), Some("s1"));
    }

    #[test]
    fn unknown_record_type_does_not_corrupt_lineage() {
        // An unknown uuid-bearing record on the chain is traversed as a hop but
        // projects no message (it has no `message` object).
        let future = r#"{"type":"future-thing","uuid":"f1","parentUuid":"u1","timestamp":"2026-07-21T06:13:15.040Z","sessionId":"s1","cwd":"/tmp"}"#;
        let file = write_session(&[
            &user("u1", None, r#""hi""#),
            future,
            &assistant("a1", "f1", r#"[{"type":"text","text":"ans"}]"#),
            &last_prompt("a1"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["hi", "ans"]);
    }

    #[test]
    fn unknown_attachment_type_is_retained() {
        let line = r#"{"type":"attachment","uuid":"att1","parentUuid":"u1","timestamp":"2026-07-21T06:13:11.040Z","sessionId":"s1","cwd":"/tmp","attachment":{"type":"new-attachment","foo":1,"bar":[2,3]}}"#;
        let value: Value = serde_json::from_str(line).expect("json");
        let record = Record::from_value(&value);
        let RecordKind::Attachment(attachment) = &record.kind else {
            panic!("expected attachment variant");
        };
        let payload = attachment.attachment.as_ref().expect("payload");
        assert!(matches!(payload.kind, AttachmentKind::Unknown));
        // The raw attachment object is retained verbatim.
        assert_eq!(payload.raw["type"], "new-attachment");
        assert_eq!(payload.raw["foo"], 1);
        assert_eq!(payload.raw["bar"], serde_json::json!([2, 3]));
        // Round-trips back to the original attachment object.
        let round_tripped = serde_json::to_value(payload).expect("serialize");
        assert_eq!(round_tripped, value["attachment"]);
    }

    #[test]
    fn known_attachment_payload_is_typed() {
        let line = r#"{"type":"attachment","uuid":"att1","parentUuid":"u1","timestamp":"2026-07-21T06:13:11.040Z","sessionId":"s1","cwd":"/tmp","attachment":{"type":"skill_listing","content":"Skills available","skillCount":3,"isInitial":true}}"#;
        let value: Value = serde_json::from_str(line).expect("json");
        let record = Record::from_value(&value);
        let RecordKind::Attachment(attachment) = &record.kind else {
            panic!("expected attachment variant");
        };
        let payload = attachment.attachment.as_ref().expect("payload");
        match &payload.kind {
            AttachmentKind::SkillListing {
                content,
                skill_count,
                is_initial,
                ..
            } => {
                assert_eq!(content, "Skills available");
                assert_eq!(*skill_count, Some(3));
                assert_eq!(*is_initial, Some(true));
            }
            other => panic!("expected SkillListing, got {other:?}"),
        }
    }

    #[test]
    fn content_block_variants_round_trip() {
        // [UNVERIFIED-ON-DISK] shapes recovered from the CLI binary.
        let content = serde_json::json!([
            {"type":"text","text":"hello"},
            {"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}},
            {"type":"tool_result","tool_use_id":"tu1","content":"done","is_error":false},
            {"type":"thinking","thinking":"reasoning","signature":"sig"}
        ]);
        let line = format!(
            r#"{{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-07-21T06:13:12.040Z","sessionId":"s1","cwd":"/tmp","message":{{"role":"assistant","content":{}}}}}"#,
            content
        );
        let value: Value = serde_json::from_str(&line).expect("json");
        let record = Record::from_value(&value);
        let RecordKind::Assistant(assistant) = &record.kind else {
            panic!("expected assistant variant");
        };
        let body = assistant.message.as_ref().expect("message");
        let Content::Blocks(blocks) = body.content.as_ref().expect("content") else {
            panic!("expected blocks");
        };
        assert_eq!(blocks.len(), 4);
        assert!(matches!(blocks[0].kind, BlockKind::Text { .. }));
        assert!(matches!(blocks[1].kind, BlockKind::ToolUse { .. }));
        assert!(matches!(blocks[2].kind, BlockKind::ToolResult { .. }));
        assert!(matches!(blocks[3].kind, BlockKind::Thinking { .. }));
        // Each block round-trips to its original object.
        for (block, original) in blocks.iter().zip(content.as_array().unwrap()) {
            assert_eq!(serde_json::to_value(block).expect("serialize"), *original);
        }
    }

    #[test]
    fn first_text_extraction_variants() {
        // Bare string, text block, input_text block, mixed, empty, unknown block.
        let cases: &[(&str, Option<&str>)] = &[
            (r#""plain""#, Some("plain")),
            (r#"[{"type":"text","text":"a"}]"#, Some("a")),
            (r#"[{"type":"input_text","text":"b"}]"#, Some("b")),
            (
                r#"[{"type":"tool_use","id":"x","name":"Bash","input":{}},{"type":"text","text":"c"}]"#,
                Some("c"),
            ),
            (r#"[]"#, None),
            (r#"[{"type":"image","source":"data"}]"#, None),
        ];
        for (content, expected) in cases {
            let file = write_session(&[
                &user("u1", None, content),
                &assistant("a1", "u1", r#"[{"type":"text","text":"ok"}]"#),
            ]);
            let session = parse(file.path()).expect("parse");
            let user_text = session
                .messages
                .iter()
                .find(|m| m.role == Role::User)
                .map(|m| m.text.as_str());
            assert_eq!(user_text, *expected, "content = {content}");
        }
    }

    #[test]
    fn session_id_prefers_camel_over_snake() {
        // Assistant carries both session_id (snake) and sessionId (camel);
        // the camel value wins, and the snake value alone still resolves.
        let both = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-07-21T06:13:12.040Z","sessionId":"camel-id","session_id":"snake-id","cwd":"/tmp","message":{"role":"assistant","content":[{"type":"text","text":"x"}]}}"#;
        let file = write_session(&[&user("u1", None, r#""hi""#), both]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "camel-id");

        let snake_only = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-07-21T06:13:12.040Z","session_id":"snake-id","cwd":"/tmp","message":{"role":"assistant","content":[{"type":"text","text":"x"}]}}"#;
        let file = write_session(&[&user("u1", None, r#""hi""#), snake_only]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "snake-id");
    }

    #[test]
    fn summary_priority_ai_title_over_summary_over_title() {
        let ai_title = r#"{"type":"ai-title","aiTitle":"Generated Title","sessionId":"s1"}"#;
        let file = write_session(&[ai_title, &user("u1", None, r#""first prompt""#)]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "Generated Title");
    }

    #[test]
    fn summary_falls_back_to_first_user_text() {
        let file = write_session(&[&user("u1", None, r#""what is rust doing""#)]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "what is rust doing");
    }

    #[test]
    fn filename_fallback_when_no_session_id() {
        // No record carries a sessionId: fall back to the file stem.
        let file = write_session(&[&user_no_session_id("u1", r#""hi""#)]);
        let session = parse(file.path()).expect("parse");
        let stem = file
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap()
            .to_owned();
        assert_eq!(session.session_id, stem);
    }

    fn user_no_session_id(uuid: &str, content: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","parentUuid":null,"timestamp":"2026-07-21T06:13:11.040Z","cwd":"/tmp","message":{{"role":"user","content":{content}}}}}"#
        )
    }

    #[test]
    fn empty_file_yields_empty_session_with_fallbacks() {
        let file = write_session(&[]);
        let session = parse(file.path()).expect("parse");
        assert!(session.messages.is_empty());
        assert_eq!(session.session_id, fallback_id(file.path()));
        assert_eq!(session.cwd, fallback_cwd(file.path()));
        assert_eq!(session.summary, "(no summary)");
        assert_eq!(session.start_timestamp, None);
    }

    #[test]
    fn unknown_envelope_fields_are_retained_in_extension_map() {
        let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-07-21T06:13:11.040Z","sessionId":"s1","cwd":"/tmp","futureField":42,"message":{"role":"user","content":"hi"}}"#;
        let value: Value = serde_json::from_str(line).expect("json");
        let record = Record::from_value(&value);
        let RecordKind::User(user) = &record.kind else {
            panic!("expected user variant");
        };
        assert_eq!(user.extra.get("futureField"), Some(&serde_json::json!(42)));
        // Typed envelope fields are still accessible.
        assert_eq!(user.envelope.uuid.as_deref(), Some("u1"));
    }
}