//! Claude Code session adapter.
//!
//! Claude stores a session as an append-only JSONL graph at
//! `~/.claude/projects/<sanitized-cwd>/<session-uuid>.jsonl`. This adapter
//! follows the native 2.1.220 loader's active-leaf, compaction, parent recovery,
//! and parallel-response rules before projecting the intentionally lossy
//! user/assistant text contract.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::domain::{Message, Role, Session, SourceTool};
use crate::formats::{normalize, read_jsonl_values, summarize_messages};

const PARENT_TIMESTAMP_FALLBACK_MS: i64 = 5_000;

/// Parse a Claude Code session transcript into a lossy `Session`.
pub fn parse(path: &Path) -> Result<Session> {
    let values = read_jsonl_values(path)?;
    let modified_epoch = file_mtime(path);
    let records: Vec<Record> = values.iter().map(Record::from_value).collect();

    let mut session_id = fallback_id(path);
    let mut cwd = fallback_cwd(path);
    let mut summary: Option<String> = None;
    for record in &records {
        if let Some(value) = record.session_id() {
            session_id = value;
        }
        if let Some(value) = record.cwd() {
            cwd = PathBuf::from(value);
        }
        if let Some(source) = record.summary_source() {
            summary = Some(normalize(&source, 100));
        }
    }

    let active_path = reconstruct_active_path(&records);
    let start_timestamp = active_path
        .first()
        .and_then(|index| records[*index].timestamp());
    let messages = project_messages(&records, &active_path);
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

#[derive(Debug, Clone)]
struct GraphNode {
    record_index: usize,
    parent_uuid: Option<String>,
    is_sidechain: bool,
    timestamp_ms: Option<i64>,
}

#[derive(Debug, Default)]
struct GraphIndex {
    nodes: HashMap<String, GraphNode>,
    order: Vec<String>,
}

impl GraphIndex {
    fn from_records(records: &[Record]) -> Self {
        let mut graph = Self::default();
        for (record_index, record) in records.iter().enumerate() {
            let Some(uuid) = record.uuid() else { continue };
            if !graph.nodes.contains_key(&uuid) {
                graph.order.push(uuid.clone());
            }
            graph.nodes.insert(
                uuid,
                GraphNode {
                    record_index,
                    parent_uuid: record.parent_uuid(),
                    is_sidechain: record.is_sidechain(),
                    timestamp_ms: record.timestamp().as_deref().and_then(parse_timestamp_ms),
                },
            );
        }
        graph
    }

    fn foreground(&self, uuid: &str) -> bool {
        self.nodes.get(uuid).is_some_and(|node| !node.is_sidechain)
    }
}

#[derive(Debug, Default)]
struct LeafState {
    preferred_uuid: Option<String>,
    latest_foreground_uuid: Option<String>,
    is_preferred_explicit: bool,
    is_cleared: bool,
}

fn reconstruct_active_path(records: &[Record]) -> Vec<usize> {
    let leaf_state = scan_leaf_state(records);
    if leaf_state.is_cleared {
        return Vec::new();
    }

    let mut graph = GraphIndex::from_records(records);
    let compacted_tail = apply_latest_compaction(records, &mut graph);
    let Some(leaf_uuid) = select_leaf(&leaf_state, compacted_tail, &graph) else {
        return Vec::new();
    };

    let mut path = walk_parent_chain(records, &graph, &leaf_uuid);
    recover_parallel_responses(records, &graph, &mut path);
    append_non_message_descendants(records, &graph, &leaf_uuid, &mut path);
    path
}

fn scan_leaf_state(records: &[Record]) -> LeafState {
    let mut state = LeafState::default();
    for record in records {
        if let Some(uuid) = record.uuid().filter(|_| !record.is_sidechain()) {
            state.latest_foreground_uuid = Some(uuid);
            state.is_preferred_explicit = false;
            state.is_cleared = false;
        }
        if record.type_tag() != Some("last-prompt") {
            continue;
        }
        let explicit = record.raw_bool("explicit");
        match record.raw.get("leafUuid") {
            Some(Value::String(uuid)) if !uuid.is_empty() => {
                state.is_preferred_explicit = explicit
                    || (state.is_preferred_explicit
                        && state.preferred_uuid.as_deref() == Some(uuid));
                state.preferred_uuid = Some(uuid.clone());
                state.is_cleared = false;
            }
            Some(Value::Null) if explicit => {
                state.preferred_uuid = None;
                state.is_preferred_explicit = false;
                state.is_cleared = true;
            }
            _ => {}
        }
    }
    state
}

fn select_leaf(
    state: &LeafState,
    compacted_tail: Option<String>,
    graph: &GraphIndex,
) -> Option<String> {
    let preferred = state
        .preferred_uuid
        .as_ref()
        .filter(|uuid| graph.foreground(uuid));
    if state.is_preferred_explicit && preferred.is_some() {
        return preferred.cloned();
    }
    if let Some(preferred) = preferred {
        if let Some(latest) = state
            .latest_foreground_uuid
            .as_ref()
            .filter(|uuid| graph.foreground(uuid))
        {
            if latest != preferred && is_descendant(graph, latest, preferred) {
                return Some(latest.clone());
            }
        }
        return Some(preferred.clone());
    }
    state
        .latest_foreground_uuid
        .as_ref()
        .filter(|uuid| graph.foreground(uuid))
        .cloned()
        .or_else(|| compacted_tail.filter(|uuid| graph.foreground(uuid)))
        .or_else(|| {
            graph
                .order
                .iter()
                .rev()
                .find(|uuid| graph.foreground(uuid))
                .cloned()
        })
}

fn is_descendant(graph: &GraphIndex, descendant: &str, ancestor: &str) -> bool {
    let mut cursor = Some(descendant);
    let mut visited = HashSet::new();
    while let Some(uuid) = cursor {
        if uuid == ancestor {
            return true;
        }
        if !visited.insert(uuid.to_owned()) {
            return false;
        }
        cursor = graph
            .nodes
            .get(uuid)
            .and_then(|node| node.parent_uuid.as_deref());
    }
    false
}

#[derive(Debug)]
struct PreservedMessages {
    anchor_uuid: String,
    uuids: Vec<String>,
}

fn apply_latest_compaction(records: &[Record], graph: &mut GraphIndex) -> Option<String> {
    let (boundary_index, boundary) = records
        .iter()
        .enumerate()
        .rev()
        .find(|(_, record)| record.is_compact_boundary())?;
    let preserved = compact_preserved_messages(boundary, graph).filter(|preserved| {
        preserved
            .uuids
            .iter()
            .all(|uuid| graph.nodes.contains_key(uuid))
    });
    if let Some(preserved) = preserved.as_ref() {
        relink_preserved_messages(graph, preserved);
    }
    remove_pre_compaction_nodes(records, graph, boundary_index, preserved.as_ref())
}

fn compact_preserved_messages(boundary: &Record, graph: &GraphIndex) -> Option<PreservedMessages> {
    let metadata = boundary.raw.get("compactMetadata")?.as_object()?;
    if let Some(preserved) = metadata.get("preservedMessages").and_then(Value::as_object) {
        let anchor_uuid = preserved.get("anchorUuid")?.as_str()?.to_owned();
        let uuids = preserved
            .get("uuids")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        return Some(PreservedMessages { anchor_uuid, uuids });
    }
    let segment = metadata.get("preservedSegment")?.as_object()?;
    preserved_segment(segment, graph)
}

fn preserved_segment(
    segment: &Map<String, Value>,
    graph: &GraphIndex,
) -> Option<PreservedMessages> {
    let anchor_uuid = segment.get("anchorUuid")?.as_str()?.to_owned();
    let head_uuid = segment.get("headUuid")?.as_str()?;
    let mut cursor = segment.get("tailUuid")?.as_str()?;
    let mut visited = HashSet::new();
    let mut uuids = Vec::new();
    loop {
        if !visited.insert(cursor.to_owned()) {
            return None;
        }
        uuids.push(cursor.to_owned());
        if cursor == head_uuid {
            uuids.reverse();
            return Some(PreservedMessages { anchor_uuid, uuids });
        }
        cursor = graph.nodes.get(cursor)?.parent_uuid.as_deref()?;
    }
}

fn relink_preserved_messages(graph: &mut GraphIndex, preserved: &PreservedMessages) {
    let mut parent_uuid = preserved.anchor_uuid.clone();
    for uuid in &preserved.uuids {
        if let Some(node) = graph.nodes.get_mut(uuid) {
            node.parent_uuid = Some(parent_uuid);
        }
        parent_uuid = uuid.clone();
    }
    let (Some(first), Some(last)) = (preserved.uuids.first(), preserved.uuids.last()) else {
        return;
    };
    for uuid in graph.order.clone() {
        let Some(node) = graph.nodes.get_mut(&uuid) else {
            continue;
        };
        if node.parent_uuid.as_deref() == Some(&preserved.anchor_uuid) && &uuid != first {
            node.parent_uuid = Some(last.clone());
        }
    }
}

fn remove_pre_compaction_nodes(
    records: &[Record],
    graph: &mut GraphIndex,
    boundary_index: usize,
    preserved: Option<&PreservedMessages>,
) -> Option<String> {
    let preserved_uuids: HashSet<&str> = preserved
        .into_iter()
        .flat_map(|messages| messages.uuids.iter().map(String::as_str))
        .collect();
    let deleted: HashSet<String> = graph
        .order
        .iter()
        .filter(|uuid| {
            graph.nodes.get(*uuid).is_some_and(|node| {
                node.record_index < boundary_index && !preserved_uuids.contains(uuid.as_str())
            })
        })
        .cloned()
        .collect();
    for uuid in &deleted {
        graph.nodes.remove(uuid);
    }

    let tail = preserved
        .and_then(|messages| messages.uuids.last())
        .cloned()
        .or_else(|| records[boundary_index].uuid())?;
    for uuid in graph.order.clone() {
        let Some(node) = graph.nodes.get_mut(&uuid) else {
            continue;
        };
        if node.record_index <= boundary_index
            || !records[node.record_index].is_conversation_record()
        {
            continue;
        }
        if node
            .parent_uuid
            .as_ref()
            .is_some_and(|parent| deleted.contains(parent))
        {
            node.parent_uuid = Some(tail.clone());
        }
    }
    Some(tail)
}

fn walk_parent_chain(records: &[Record], graph: &GraphIndex, leaf_uuid: &str) -> Vec<usize> {
    let mut reversed = Vec::new();
    let mut visited = HashSet::new();
    let mut cursor = Some(leaf_uuid.to_owned());
    while let Some(uuid) = cursor {
        let Some(node) = graph.nodes.get(&uuid) else {
            break;
        };
        if !visited.insert(uuid.clone()) {
            break;
        }
        reversed.push(node.record_index);
        let Some(parent_uuid) = node.parent_uuid.as_ref() else {
            break;
        };
        cursor = if graph.nodes.contains_key(parent_uuid) && !visited.contains(parent_uuid) {
            Some(parent_uuid.clone())
        } else {
            nearest_timestamp_parent(graph, node, &visited)
        };
    }
    reversed.reverse();
    let _ = records;
    reversed
}

fn nearest_timestamp_parent(
    graph: &GraphIndex,
    current: &GraphNode,
    visited: &HashSet<String>,
) -> Option<String> {
    let current_timestamp = current.timestamp_ms?;
    let mut best: Option<(i64, String)> = None;
    for uuid in &graph.order {
        let Some(candidate) = graph.nodes.get(uuid) else {
            continue;
        };
        if visited.contains(uuid) || candidate.is_sidechain != current.is_sidechain {
            continue;
        }
        let Some(candidate_timestamp) = candidate.timestamp_ms else {
            continue;
        };
        let delta = current_timestamp - candidate_timestamp;
        if !(0..=PARENT_TIMESTAMP_FALLBACK_MS).contains(&delta) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(best_delta, _)| delta < *best_delta)
        {
            best = Some((delta, uuid.clone()));
        }
    }
    best.map(|(_, uuid)| uuid)
}

fn recover_parallel_responses(records: &[Record], graph: &GraphIndex, path: &mut Vec<usize>) {
    let mut canonical_by_message_id = HashMap::new();
    for index in path.iter().copied() {
        let record = &records[index];
        if record.type_tag() == Some("assistant") {
            if let Some(message_id) = record.message_id() {
                canonical_by_message_id.insert(message_id, record.uuid().unwrap_or_default());
            }
        }
    }
    if canonical_by_message_id.is_empty() {
        return;
    }

    let mut assistants = HashMap::<String, Vec<String>>::new();
    let mut tool_results = HashMap::<String, Vec<String>>::new();
    for uuid in &graph.order {
        let Some(node) = graph.nodes.get(uuid) else {
            continue;
        };
        let record = &records[node.record_index];
        if record.type_tag() == Some("assistant") {
            if let Some(message_id) = record.message_id() {
                assistants.entry(message_id).or_default().push(uuid.clone());
            }
        } else if record.is_user_tool_result() {
            if let Some(parent_uuid) = node.parent_uuid.as_ref() {
                tool_results
                    .entry(parent_uuid.clone())
                    .or_default()
                    .push(uuid.clone());
            }
        }
    }
    insert_parallel_records(
        records,
        graph,
        path,
        canonical_by_message_id,
        assistants,
        tool_results,
    );
}

fn insert_parallel_records(
    records: &[Record],
    graph: &GraphIndex,
    path: &mut Vec<usize>,
    canonical: HashMap<String, String>,
    assistants: HashMap<String, Vec<String>>,
    tool_results: HashMap<String, Vec<String>>,
) {
    let mut visited: HashSet<String> = path
        .iter()
        .filter_map(|index| records[*index].uuid())
        .collect();
    let mut insertions = HashMap::<String, Vec<usize>>::new();
    for (message_id, canonical_uuid) in canonical {
        let fragments = assistants.get(&message_id).cloned().unwrap_or_default();
        let mut assistant_indices = Vec::new();
        let mut tool_result_indices = Vec::new();
        for fragment_uuid in fragments {
            if visited.insert(fragment_uuid.clone()) {
                assistant_indices.push(graph.nodes[&fragment_uuid].record_index);
            }
            for tool_uuid in tool_results.get(&fragment_uuid).into_iter().flatten() {
                if visited.insert(tool_uuid.clone()) {
                    tool_result_indices.push(graph.nodes[tool_uuid].record_index);
                }
            }
        }
        sort_record_indices(records, &mut assistant_indices);
        sort_record_indices(records, &mut tool_result_indices);
        assistant_indices.extend(tool_result_indices);
        if !assistant_indices.is_empty() {
            insertions.insert(canonical_uuid, assistant_indices);
        }
    }

    let mut expanded =
        Vec::with_capacity(path.len() + insertions.values().map(Vec::len).sum::<usize>());
    for index in path.iter().copied() {
        expanded.push(index);
        if let Some(uuid) = records[index].uuid() {
            if let Some(extra) = insertions.get(&uuid) {
                expanded.extend(extra.iter().copied());
            }
        }
    }
    *path = expanded;
}

fn append_non_message_descendants(
    records: &[Record],
    graph: &GraphIndex,
    leaf_uuid: &str,
    path: &mut Vec<usize>,
) {
    let mut children = HashMap::<String, Vec<String>>::new();
    for uuid in &graph.order {
        let Some(node) = graph.nodes.get(uuid) else {
            continue;
        };
        if records[node.record_index].is_conversation_record() {
            continue;
        }
        if let Some(parent_uuid) = node.parent_uuid.as_ref() {
            children
                .entry(parent_uuid.clone())
                .or_default()
                .push(uuid.clone());
        }
    }

    let mut visited: HashSet<String> = path
        .iter()
        .filter_map(|index| records[*index].uuid())
        .collect();
    let mut queue = VecDeque::from([leaf_uuid.to_owned()]);
    let mut descendants = Vec::new();
    while let Some(parent_uuid) = queue.pop_front() {
        for uuid in children.get(&parent_uuid).into_iter().flatten() {
            if !visited.insert(uuid.clone()) {
                continue;
            }
            descendants.push(graph.nodes[uuid].record_index);
            queue.push_back(uuid.clone());
        }
    }
    sort_record_indices(records, &mut descendants);
    path.extend(descendants);
}

fn sort_record_indices(records: &[Record], indices: &mut [usize]) {
    indices.sort_by(|left, right| records[*left].timestamp().cmp(&records[*right].timestamp()));
}

fn project_messages(records: &[Record], path: &[usize]) -> Vec<Message> {
    path.iter()
        .map(|index| &records[*index])
        .filter(|record| record.is_conversation_record())
        .filter(|record| !record.is_sidechain() && !record.is_meta())
        .filter_map(project_message)
        .collect()
}

fn project_message(record: &Record) -> Option<Message> {
    let (role, content, timestamp) = record.message_payload()?;
    let role = role?.parse::<Role>().ok()?;
    let text = first_direct_text(content?)?;
    Some(Message {
        role,
        text: text.to_owned(),
        timestamp: timestamp.map(str::to_owned),
    })
}

fn first_direct_text(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) if !text.is_empty() => Some(text),
        Value::Array(items) => items.iter().find_map(|item| {
            let object = item.as_object()?;
            let item_type = object.get("type").and_then(Value::as_str);
            let text = object.get("text").and_then(Value::as_str);
            matches!(item_type, Some("text" | "input_text" | "output_text"))
                .then_some(text)
                .flatten()
                .filter(|value| !value.is_empty())
        }),
        _ => None,
    }
}

fn parse_timestamp_ms(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
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

    fn is_conversation_record(&self) -> bool {
        matches!(self.type_tag(), Some("user" | "assistant"))
    }

    fn is_compact_boundary(&self) -> bool {
        self.type_tag() == Some("system")
            && self.raw_str("subtype").as_deref() == Some("compact_boundary")
    }

    fn message_id(&self) -> Option<String> {
        self.raw
            .get("message")
            .and_then(Value::as_object)
            .and_then(|message| message.get("id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    fn is_user_tool_result(&self) -> bool {
        if self.type_tag() != Some("user") {
            return false;
        }
        self.raw
            .get("message")
            .and_then(Value::as_object)
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            })
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
            return envelope
                .parent_uuid
                .clone()
                .filter(|value| !value.is_empty());
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
            _ => self
                .raw_str("sessionId")
                .or_else(|| self.raw_str("session_id")),
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
            .or_else(|| {
                self.session_id_snake
                    .clone()
                    .filter(|value| !value.is_empty())
            })
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
    Text {
        text: String,
    },
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
    fn explicit_null_leaf_clears_conversation() {
        let cleared = r#"{"type":"last-prompt","leafUuid":null,"explicit":true,"sessionId":"s1"}"#;
        let file = write_session(&[
            &user("u1", None, r#""prompt""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"answer"}]"#),
            &last_prompt("a1"),
            cleared,
        ]);
        let session = parse(file.path()).expect("parse");
        assert!(session.messages.is_empty());
        assert_eq!(session.start_timestamp, None);
    }

    #[test]
    fn missing_explicit_leaf_falls_back_to_latest_foreground_record() {
        let missing =
            r#"{"type":"last-prompt","leafUuid":"missing","explicit":true,"sessionId":"s1"}"#;
        let file = write_session(&[
            &user("u1", None, r#""prompt""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"answer"}]"#),
            missing,
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["prompt", "answer"]);
    }

    #[test]
    fn preferred_leaf_advances_to_later_foreground_descendant() {
        let preferred = r#"{"type":"last-prompt","leafUuid":"a1","sessionId":"s1"}"#;
        let file = write_session(&[
            &user("u1", None, r#""prompt""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"first"}]"#),
            preferred,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","timestamp":"2026-07-21T06:13:14.040Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"later"}]}}"#,
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["prompt", "first", "later"]);
    }

    #[test]
    fn explicit_leaf_is_authoritative_over_later_foreground_descendant() {
        // `explicit: true` pins the leaf to the resolvable preferred uuid: a
        // later foreground descendant must not advance past it.
        let explicit = r#"{"type":"last-prompt","leafUuid":"a1","explicit":true,"sessionId":"s1"}"#;
        let file = write_session(&[
            &user("u1", None, r#""prompt""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"first"}]"#),
            r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","timestamp":"2026-07-21T06:13:14.040Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"later"}]}}"#,
            explicit,
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["prompt", "first"]);
    }

    #[test]
    fn preserved_segment_chain_is_relinked_in_tail_to_head_order() {
        // compactMetadata.preservedSegment names headUuid/tailUuid of a chain
        // whose parent pointers span pre-compaction nodes: the walk must
        // collect tailUuid..headUuid, reverse into head-first order, and
        // relink so the segment reads forward from the anchor.
        let boundary = r#"{"type":"system","subtype":"compact_boundary","uuid":"c1","parentUuid":"old-b","timestamp":"2026-07-21T06:13:12.500Z","sessionId":"s1","cwd":"/workspace/project","compactMetadata":{"preservedSegment":{"anchorUuid":"old-b","headUuid":"p1","tailUuid":"p2"}}}"#;
        // p1's parent pointer is a pre-compaction node removed later; the
        // walk must reach it through the graph regardless.
        let preserved_user = r#"{"type":"user","uuid":"p1","parentUuid":"old-a","timestamp":"2026-07-21T06:13:13.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"user","content":"preserved"}}"#;
        let preserved_assistant = r#"{"type":"assistant","uuid":"p2","parentUuid":"p1","timestamp":"2026-07-21T06:13:14.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"preserved answer"}]}}"#;
        let final_answer = r#"{"type":"assistant","uuid":"a2","parentUuid":"c1","timestamp":"2026-07-21T06:13:15.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"final"}]}}"#;
        let file = write_session(&[
            &user("old-a", None, r#""dropped older context""#),
            &assistant("old-b", "old-a", r#"[{"type":"text","text":"dropped answer"}]"#),
            boundary,
            preserved_user,
            preserved_assistant,
            final_answer,
            &last_prompt("a2"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        // Old context is dropped; the preserved segment projects in
        // head-first order followed by the post-compaction answer.
        assert_eq!(texts, ["preserved", "preserved answer", "final"]);
    }

    #[test]
    fn missing_parent_recovers_nearest_same_sidechain_record() {
        let file = write_session(&[
            r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-07-21T06:13:10.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"user","content":"prompt"}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"missing","timestamp":"2026-07-21T06:13:13.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
            &last_prompt("a1"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["prompt", "answer"]);
    }

    #[test]
    fn tool_result_content_is_not_projected_as_human_text() {
        let tool_result = r#"{"type":"user","uuid":"tr1","parentUuid":"a1","timestamp":"2026-07-21T06:13:13.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool1","content":[{"type":"text","text":"tool output"}]}]}}"#;
        let final_answer = r#"{"type":"assistant","uuid":"a2","parentUuid":"tr1","timestamp":"2026-07-21T06:13:14.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
        let file = write_session(&[
            &user("u1", None, r#""prompt""#),
            &assistant(
                "a1",
                "u1",
                r#"[{"type":"tool_use","id":"tool1","name":"Read","input":{}}]"#,
            ),
            tool_result,
            final_answer,
            &last_prompt("a2"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["prompt", "done"]);
    }

    #[test]
    fn start_timestamp_comes_from_reconstructed_chain_root() {
        let queued = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-07-21T05:00:00.000Z","sessionId":"s1"}"#;
        let file = write_session(&[
            queued,
            &user("u1", None, r#""prompt""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"answer"}]"#),
            &last_prompt("a1"),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(
            session.start_timestamp.as_deref(),
            Some("2026-07-21T06:13:11.040Z")
        );
    }

    #[test]
    fn compact_preserved_messages_relink_to_boundary_anchor() {
        let boundary = r#"{"type":"system","subtype":"compact_boundary","uuid":"c1","parentUuid":"u1","timestamp":"2026-07-21T06:13:12.500Z","sessionId":"s1","cwd":"/workspace/project","compactMetadata":{"preservedMessages":{"anchorUuid":"u1","uuids":["p1","p2"]}}}"#;
        let preserved_user = r#"{"type":"user","uuid":"p1","parentUuid":"missing-a","timestamp":"2026-07-21T06:13:13.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"user","content":"preserved"}}"#;
        let preserved_assistant = r#"{"type":"assistant","uuid":"p2","parentUuid":"missing-b","timestamp":"2026-07-21T06:13:14.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"preserved answer"}]}}"#;
        let final_answer = r#"{"type":"assistant","uuid":"a2","parentUuid":"c1","timestamp":"2026-07-21T06:13:15.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"final"}]}}"#;
        let file = write_session(&[
            &user("u1", None, r#""old root""#),
            boundary,
            preserved_user,
            preserved_assistant,
            final_answer,
            &last_prompt("a2"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["preserved", "preserved answer", "final"]);
    }

    #[test]
    fn latest_compaction_without_preservation_drops_older_context() {
        let first_boundary = r#"{"type":"system","subtype":"compact_boundary","uuid":"c1","parentUuid":"u0","timestamp":"2026-07-21T06:13:12.500Z","sessionId":"s1","cwd":"/workspace/project","compactMetadata":{"preservedMessages":{"anchorUuid":"c1","uuids":["u0"]}}}"#;
        let middle = r#"{"type":"user","uuid":"u1","parentUuid":"c1","timestamp":"2026-07-21T06:13:13.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"user","content":"middle"}}"#;
        let latest_boundary = r#"{"type":"system","subtype":"compact_boundary","uuid":"c2","parentUuid":"u1","timestamp":"2026-07-21T06:13:14.000Z","sessionId":"s1","cwd":"/workspace/project"}"#;
        let final_answer = r#"{"type":"assistant","uuid":"a2","parentUuid":"c2","timestamp":"2026-07-21T06:13:15.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"final"}]}}"#;
        let file = write_session(&[
            &user("u0", None, r#""old""#),
            first_boundary,
            middle,
            latest_boundary,
            final_answer,
            &last_prompt("a2"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["final"]);
        assert_eq!(
            session.start_timestamp.as_deref(),
            Some("2026-07-21T06:13:14.000Z")
        );
    }

    #[test]
    fn non_conversation_descendant_message_payload_is_not_projected() {
        let metadata = r#"{"type":"future-record","uuid":"x1","parentUuid":"a1","timestamp":"2026-07-21T06:13:13.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"user","content":"metadata text"}}"#;
        let file = write_session(&[
            &user("u1", None, r#""prompt""#),
            &assistant("a1", "u1", r#"[{"type":"text","text":"answer"}]"#),
            metadata,
            &last_prompt("a1"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["prompt", "answer"]);
    }

    #[test]
    fn parallel_assistant_fragments_are_recovered_without_tool_output() {
        let first = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-07-21T06:13:12.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"id":"api1","role":"assistant","content":[{"type":"text","text":"first"}]}}"#;
        let second = r#"{"type":"assistant","uuid":"a2","parentUuid":"u1","timestamp":"2026-07-21T06:13:13.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"id":"api1","role":"assistant","content":[{"type":"text","text":"second"}]}}"#;
        let tool_result = r#"{"type":"user","uuid":"tr1","parentUuid":"a2","timestamp":"2026-07-21T06:13:14.000Z","sessionId":"s1","cwd":"/workspace/project","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool1","content":"tool output"}]}}"#;
        let file = write_session(&[
            &user("u1", None, r#""prompt""#),
            first,
            second,
            tool_result,
            &last_prompt("a1"),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["prompt", "first", "second"]);
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
