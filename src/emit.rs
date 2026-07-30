use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::domain::{Role, Session, TargetTool, ThinkingLevel};
use crate::fs::atomic_write_jsonl;

const MAX_FILESYSTEM_COMPONENT_BYTES: usize = 255;
const CLAUDE_VERSION: &str = "2.1.220";
const DEFAULT_GROK_MODEL: &str = "grok-4.5";

const URL_PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedSession {
    pub path: PathBuf,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmpRuntime {
    pub model: String,
    pub thinking_level: ThinkingLevel,
}

impl OmpRuntime {
    pub fn parse(selector: &str) -> Result<Self> {
        let selector = selector.trim();
        if selector.is_empty() {
            bail!("OMP conversion model is empty");
        }
        let (model, thinking_level) = match selector.rsplit_once(':') {
            Some((model, thinking)) if thinking.parse::<ThinkingLevel>().is_ok() => {
                (model, thinking.parse::<ThinkingLevel>()?)
            }
            _ => (selector, ThinkingLevel::Off),
        };
        let Some((provider, model_id)) = model.split_once('/') else {
            bail!(
                "OMP conversion model must use provider/model format; set SESSIONS_OMP_MODEL or configure OMP modelRoles.default"
            );
        };
        if provider.is_empty() || model_id.is_empty() {
            bail!(
                "OMP conversion model must use provider/model format; set SESSIONS_OMP_MODEL or configure OMP modelRoles.default"
            );
        }
        Ok(Self {
            model: model.to_owned(),
            thinking_level,
        })
    }

    fn provider_and_model(&self) -> Result<(&str, &str)> {
        let Some((provider, model)) = self.model.split_once('/') else {
            bail!("OMP conversion model must use provider/model format");
        };
        if provider.is_empty() || model.is_empty() {
            bail!("OMP conversion model must use provider/model format");
        }
        Ok((provider, model))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRuntime {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRoots {
    pub pi: PathBuf,
    pub omp: PathBuf,
    pub droid: PathBuf,
    pub codex: PathBuf,
    pub claude: PathBuf,
    pub grok: PathBuf,
}

impl TargetRoots {
    pub fn from_home(home: &Path) -> Self {
        Self {
            pi: home.join(".pi/agent/sessions"),
            omp: home.join(".omp/agent/sessions"),
            droid: home.join(".factory/sessions"),
            codex: home.join(".codex/sessions"),
            claude: home.join(".claude/projects"),
            grok: home.join(".grok/sessions"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmitContext {
    pub home: PathBuf,
    pub roots: TargetRoots,
    pub session_id: Option<String>,
    pub fallback_time: Option<DateTime<Utc>>,
    pub owner: Option<String>,
    pub omp_runtime: Option<OmpRuntime>,
    pub codex_runtime: Option<CodexRuntime>,
    pub grok_model: Option<String>,
    pub output: Option<PathBuf>,
}

impl EmitContext {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let roots = TargetRoots::from_home(&home);
        Self {
            home,
            roots,
            session_id: None,
            fallback_time: None,
            owner: None,
            omp_runtime: None,
            codex_runtime: None,
            grok_model: None,
            output: None,
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_fallback_time(mut self, fallback_time: DateTime<Utc>) -> Self {
        self.fallback_time = Some(fallback_time);
        self
    }

    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    pub fn with_omp_runtime(mut self, runtime: OmpRuntime) -> Self {
        self.omp_runtime = Some(runtime);
        self
    }

    pub fn with_codex_runtime(mut self, runtime: CodexRuntime) -> Self {
        self.codex_runtime = Some(runtime);
        self
    }

    pub fn with_grok_model(mut self, model: impl Into<String>) -> Self {
        self.grok_model = Some(model.into());
        self
    }

    pub fn with_output(mut self, output: impl Into<PathBuf>) -> Self {
        self.output = Some(output.into());
        self
    }
}

pub trait EmitDefaults {
    fn next_uuid(&mut self) -> Uuid;
    fn now(&mut self) -> DateTime<Utc>;
    fn owner(&mut self) -> String;
    fn resolve_omp_runtime(&mut self) -> Result<OmpRuntime>;
    fn resolve_codex_runtime(&mut self) -> Result<CodexRuntime>;
    fn grok_model(&mut self) -> String;
}

#[derive(Debug, Default)]
pub struct SystemEmitDefaults;

impl EmitDefaults for SystemEmitDefaults {
    fn next_uuid(&mut self) -> Uuid {
        Uuid::new_v4()
    }

    fn now(&mut self) -> DateTime<Utc> {
        Utc::now()
    }

    fn owner(&mut self) -> String {
        env::var("USER").unwrap_or_else(|_| "user".to_owned())
    }

    fn resolve_omp_runtime(&mut self) -> Result<OmpRuntime> {
        resolve_omp_runtime_default()
    }

    fn resolve_codex_runtime(&mut self) -> Result<CodexRuntime> {
        resolve_codex_runtime_default()
    }
    fn grok_model(&mut self) -> String {
        nonempty_env("SESSIONS_GROK_MODEL")
            .or_else(|| nonempty_env("GROK_DEFAULT_MODEL"))
            .unwrap_or_else(|| DEFAULT_GROK_MODEL.to_owned())
    }
}

pub fn resolve_omp_runtime_default() -> Result<OmpRuntime> {
    if let Some(selector) = nonempty_env("SESSIONS_OMP_MODEL")
        .or_else(|| nonempty_env("OMP_DEFAULT_MODEL"))
    {
        return OmpRuntime::parse(&selector);
    }
    let output = Command::new("omp")
        .args(["config", "get", "modelRoles", "--json"])
        .output()
        .context("running omp config get modelRoles --json")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if detail.is_empty() {
            bail!("cannot read OMP modelRoles");
        }
        bail!("cannot read OMP modelRoles: {detail}");
    }
    let payload: Value =
        serde_json::from_slice(&output.stdout).context("parsing omp modelRoles response")?;
    let selector = payload
        .pointer("/value/default")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow!("OMP modelRoles.default is not configured; set SESSIONS_OMP_MODEL")
        })?;
    OmpRuntime::parse(selector)
}

pub fn resolve_codex_runtime_default() -> Result<CodexRuntime> {
    let doctor = command_output_with_timeout(
        "codex",
        &["doctor", "--json"],
        Duration::from_secs(15),
    )
    .context("running codex doctor --json")?;
    if !doctor.status.success() {
        let detail = String::from_utf8_lossy(&doctor.stderr).trim().to_owned();
        if detail.is_empty() {
            bail!("cannot read Codex effective configuration");
        }
        bail!("cannot read Codex effective configuration: {detail}");
    }
    let report: Value =
        serde_json::from_slice(&doctor.stdout).context("parsing codex doctor response")?;
    let details = report
        .pointer("/checks/config.load/details")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow!("cannot resolve Codex runtime state: doctor report missing config.load details")
        })?;
    let provider = details
        .get("model provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("cannot resolve Codex runtime state: doctor report missing model provider")
        })?;
    let model = details
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("cannot resolve Codex runtime state: doctor report missing model")
        })?;

    let catalog = command_output_with_timeout(
        "codex",
        &["debug", "models"],
        Duration::from_secs(15),
    )
    .context("running codex debug models")?;
    if !catalog.status.success() {
        let detail = String::from_utf8_lossy(&catalog.stderr).trim().to_owned();
        if detail.is_empty() {
            bail!("cannot read Codex model catalog");
        }
        bail!("cannot read Codex model catalog: {detail}");
    }
    let catalog_payload: Value =
        serde_json::from_slice(&catalog.stdout).context("parsing codex model catalog")?;
    let available = catalog_payload
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("cannot resolve Codex runtime state: model catalog is malformed"))?;
    if !available
        .iter()
        .any(|entry| entry.get("slug").and_then(Value::as_str) == Some(model))
    {
        bail!(
            "cannot resolve Codex runtime state: configured model {model:?} is not available"
        );
    }
    Ok(CodexRuntime {
        provider: provider.to_owned(),
        model: model.to_owned(),
    })
}

pub fn emit_default(
    session: &Session,
    target: TargetTool,
    home: &Path,
) -> Result<EmittedSession> {
    emit(session, target, &EmitContext::new(home))
}

pub fn emit_to_default(
    session: &Session,
    target: TargetTool,
    home: &Path,
    output: &Path,
) -> Result<EmittedSession> {
    emit(session, target, &EmitContext::new(home).with_output(output))
}

pub fn emit(
    session: &Session,
    target: TargetTool,
    context: &EmitContext,
) -> Result<EmittedSession> {
    emit_with_defaults(session, target, context, &mut SystemEmitDefaults)
}

pub fn emit_with_defaults<D: EmitDefaults>(
    session: &Session,
    target: TargetTool,
    context: &EmitContext,
    defaults: &mut D,
) -> Result<EmittedSession> {
    if session.messages.is_empty() {
        bail!(
            "input session has no convertible user/assistant text messages: {}",
            session.path.display()
        );
    }
    let session_id = context
        .session_id
        .clone()
        .unwrap_or_else(|| defaults.next_uuid().to_string());
    validate_component("session id", &session_id)?;
    let start = fallback_time(session, context, defaults);
    let cwd = session
        .cwd
        .to_str()
        .ok_or_else(|| anyhow!("session cwd is not valid UTF-8: {}", session.cwd.display()))?;
    if target.uses_grok_storage() {
        validate_grok_cwd_component(cwd)?;
    }
    let output = match &context.output {
        Some(output) => normalize_output_path(target, output),
        None => target_path(target, cwd, &session_id, start, context)?,
    };

    match target {
        TargetTool::Pi => {
            let records = emit_pi(session, cwd, &session_id, start, defaults);
            write_jsonl(&output, &records)?;
        }
        TargetTool::Omp => {
            let runtime = match &context.omp_runtime {
                Some(runtime) => runtime.clone(),
                None => defaults.resolve_omp_runtime()?,
            };
            let records = emit_omp(session, cwd, &session_id, start, &runtime, defaults)?;
            write_jsonl(&output, &records)?;
        }
        TargetTool::Droid => {
            let owner = context.owner.clone().unwrap_or_else(|| defaults.owner());
            let records = emit_droid(session, cwd, &session_id, start, &owner, defaults);
            write_jsonl(&output, &records)?;
        }
        TargetTool::Codex => {
            let runtime = match &context.codex_runtime {
                Some(runtime) => runtime.clone(),
                None => defaults.resolve_codex_runtime()?,
            };
            let records = emit_codex(session, cwd, &session_id, start, &runtime);
            write_jsonl(&output, &records)?;
        }
        TargetTool::Claude => {
            let records = emit_claude(session, cwd, &session_id, start, defaults);
            write_jsonl(&output, &records)?;
        }
        TargetTool::Grok | TargetTool::Hyper => {
            let model = context
                .grok_model
                .clone()
                .unwrap_or_else(|| defaults.grok_model());
            let bundle = emit_grok(session, target, cwd, &session_id, start, &model);
            write_grok_bundle(&output, &bundle, defaults)?;
        }
    }

    Ok(EmittedSession {
        path: output,
        session_id,
    })
}

fn emit_pi<D: EmitDefaults>(
    session: &Session,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    defaults: &mut D,
) -> Vec<Value> {
    let model_id = short_id(defaults.next_uuid());
    let thinking_id = short_id(defaults.next_uuid());
    let mut records = vec![
        json!({
            "type": "session",
            "version": 3,
            "id": session_id,
            "timestamp": fmt_iso(start),
            "cwd": cwd,
        }),
        json!({
            "type": "model_change",
            "id": model_id,
            "parentId": null,
            "timestamp": fmt_iso(start + TimeDelta::milliseconds(100)),
            "provider": "sessions-convert",
            "modelId": format!("converted-from-{}", session.tool),
        }),
        json!({
            "type": "thinking_level_change",
            "id": thinking_id,
            "parentId": model_id,
            "timestamp": fmt_iso(start + TimeDelta::milliseconds(200)),
            "thinkingLevel": "off",
        }),
    ];
    let mut parent_id = thinking_id;
    for (index, message) in session.messages.iter().enumerate() {
        let message_id = short_id(defaults.next_uuid());
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let mut payload = serde_json::Map::new();
        payload.insert("role".to_owned(), json!(message.role.as_str()));
        payload.insert(
            "content".to_owned(),
            json!([{ "type": "text", "text": message.text }]),
        );
        payload.insert("timestamp".to_owned(), json!(timestamp.timestamp_millis()));
        payload.insert("usage".to_owned(), zero_usage());
        if message.role == Role::Assistant {
            payload.insert("api".to_owned(), json!("openai-completions"));
            payload.insert("provider".to_owned(), json!("sessions-convert"));
            payload.insert(
                "model".to_owned(),
                json!(format!("converted-from-{}", session.tool)),
            );
            payload.insert("stopReason".to_owned(), json!("stop"));
            payload.insert(
                "responseId".to_owned(),
                json!(format!("converted-{}", compact_id(defaults.next_uuid(), 12))),
            );
        }
        records.push(json!({
            "type": "message",
            "id": message_id,
            "parentId": parent_id,
            "timestamp": fmt_iso(timestamp),
            "message": payload,
        }));
        parent_id = message_id;
    }
    records
}

fn emit_omp<D: EmitDefaults>(
    session: &Session,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    runtime: &OmpRuntime,
    defaults: &mut D,
) -> Result<Vec<Value>> {
    let (provider, model) = runtime.provider_and_model()?;
    let model_id = short_id(defaults.next_uuid());
    let thinking_id = short_id(defaults.next_uuid());
    let mut records = vec![
        json!({
            "type": "session",
            "version": 3,
            "id": session_id,
            "timestamp": fmt_iso(start),
            "cwd": cwd,
            "title": session.summary,
            "convertedFrom": session.tool.as_str(),
        }),
        json!({
            "type": "model_change",
            "id": model_id,
            "parentId": null,
            "timestamp": fmt_iso(start + TimeDelta::milliseconds(100)),
            "model": runtime.model,
        }),
        json!({
            "type": "thinking_level_change",
            "id": thinking_id,
            "parentId": model_id,
            "timestamp": fmt_iso(start + TimeDelta::milliseconds(200)),
            "thinkingLevel": runtime.thinking_level.as_str(),
        }),
    ];
    let mut parent_id = thinking_id;
    for (index, message) in session.messages.iter().enumerate() {
        let message_id = short_id(defaults.next_uuid());
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let mut payload = serde_json::Map::new();
        payload.insert("role".to_owned(), json!(message.role.as_str()));
        payload.insert(
            "content".to_owned(),
            json!([{ "type": "text", "text": message.text }]),
        );
        payload.insert("timestamp".to_owned(), json!(timestamp.timestamp_millis()));
        payload.insert("usage".to_owned(), zero_usage());
        if message.role == Role::Assistant {
            payload.insert("api".to_owned(), json!("openai-completions"));
            payload.insert("provider".to_owned(), json!(provider));
            payload.insert("model".to_owned(), json!(model));
            payload.insert("stopReason".to_owned(), json!("stop"));
            payload.insert(
                "responseId".to_owned(),
                json!(format!("converted-{}", compact_id(defaults.next_uuid(), 12))),
            );
        }
        records.push(json!({
            "type": "message",
            "id": message_id,
            "parentId": parent_id,
            "timestamp": fmt_iso(timestamp),
            "message": payload,
        }));
        parent_id = message_id;
    }
    Ok(records)
}

fn emit_droid<D: EmitDefaults>(
    session: &Session,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    owner: &str,
    defaults: &mut D,
) -> Vec<Value> {
    let mut records = vec![json!({
        "type": "session_start",
        "id": session_id,
        "title": session.summary,
        "sessionTitle": session.summary,
        "owner": owner,
        "version": 2,
        "cwd": cwd,
        "isSessionTitleManuallySet": false,
        "sessionTitleAutoStage": "first_message",
    })];
    let mut parent_id: Option<String> = None;
    for (index, message) in session.messages.iter().enumerate() {
        let message_id = defaults.next_uuid().to_string();
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let mut record = json!({
            "type": "message",
            "id": message_id,
            "timestamp": fmt_iso(timestamp),
            "message": {
                "role": message.role.as_str(),
                "content": [{ "type": "text", "text": message.text }],
            },
        });
        if let Some(parent_id) = &parent_id {
            record
                .as_object_mut()
                .expect("JSON object")
                .insert("parentId".to_owned(), json!(parent_id));
        }
        records.push(record);
        parent_id = Some(message_id);
    }
    records
}

fn emit_codex(
    session: &Session,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    runtime: &CodexRuntime,
) -> Vec<Value> {
    let timestamp = fmt_iso(start);
    let mut records = vec![
        json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "session_id": session_id,
                "timestamp": timestamp,
                "cwd": cwd,
                "originator": "codex-tui",
                "source": "cli",
                "thread_source": "user",
                "cli_version": "sessions-convert",
                "model_provider": runtime.provider,
                "history_mode": "legacy",
            },
        }),
        json!({
            "timestamp": timestamp,
            "type": "turn_context",
            "payload": {
                "cwd": cwd,
                "workspace_roots": [cwd],
                "approval_policy": "never",
                "sandbox_policy": { "type": "read-only" },
                "model": runtime.model,
                "summary": "auto",
            },
        }),
    ];
    for (index, message) in session.messages.iter().enumerate() {
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let timestamp_text = fmt_iso(timestamp);
        let content_type = if message.role == Role::User {
            "input_text"
        } else {
            "output_text"
        };
        let response_item = json!({
            "timestamp": timestamp_text,
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": message.role.as_str(),
                "content": [{ "type": content_type, "text": message.text }],
            },
        });
        let event = if message.role == Role::User {
            json!({
                "timestamp": timestamp_text,
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": message.text,
                    "images": [],
                    "local_images": [],
                    "text_elements": [],
                },
            })
        } else {
            json!({
                "timestamp": timestamp_text,
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": message.text,
                    "phase": null,
                    "memory_citation": null,
                },
            })
        };
        if message.role == Role::User {
            records.push(response_item);
            records.push(event);
        } else {
            records.push(event);
            records.push(response_item);
        }
    }
    records
}

fn emit_claude<D: EmitDefaults>(
    session: &Session,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    defaults: &mut D,
) -> Vec<Value> {
    let mut records = Vec::with_capacity(session.messages.len() + usize::from(!session.messages.is_empty()));
    let mut parent_uuid: Option<String> = None;
    let mut last_user_text = "";
    for (index, message) in session.messages.iter().enumerate() {
        let message_uuid = defaults.next_uuid().to_string();
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let mut record = json!({
            "parentUuid": parent_uuid,
            "isSidechain": false,
            "userType": "external",
            "cwd": cwd,
            "sessionId": session_id,
            "version": CLAUDE_VERSION,
            "gitBranch": "",
            "type": message.role.as_str(),
            "uuid": message_uuid,
            "timestamp": fmt_iso(timestamp),
        });
        let object = record.as_object_mut().expect("JSON object");
        if message.role == Role::User {
            last_user_text = &message.text;
            object.insert(
                "message".to_owned(),
                json!({ "role": "user", "content": message.text }),
            );
            object.insert("permissionMode".to_owned(), json!("default"));
        } else {
            object.insert(
                "message".to_owned(),
                json!({
                    "model": format!("converted-from-{}", session.tool),
                    "id": format!("msg_converted_{}", defaults.next_uuid().simple()),
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "text", "text": message.text }],
                    "stop_reason": "end_turn",
                    "stop_sequence": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                }),
            );
        }
        records.push(record);
        parent_uuid = Some(message_uuid);
    }
    if let Some(leaf_uuid) = parent_uuid {
        records.push(json!({
            "type": "last-prompt",
            "lastPrompt": last_user_text,
            "leafUuid": leaf_uuid,
            "sessionId": session_id,
        }));
    }
    records
}

struct GrokBundle {
    summary: Value,
    chat: Vec<Value>,
    updates: Vec<Value>,
}

fn emit_grok(
    session: &Session,
    target: TargetTool,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    model: &str,
) -> GrokBundle {
    let mut chat = Vec::with_capacity(session.messages.len());
    let mut updates = Vec::with_capacity(session.messages.len());
    let mut end = start;
    let mut prompt_index = 0_u64;
    for (index, message) in session.messages.iter().enumerate() {
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        end = timestamp;
        let provenance = format!("converted-from-{}", session.tool);
        let (chat_record, update_type, chunk_meta) = if message.role == Role::User {
            let current_prompt = prompt_index;
            prompt_index += 1;
            (
                json!({
                    "type": "user",
                    "content": [{ "type": "text", "text": message.text }],
                    "prompt_index": current_prompt,
                }),
                "user_message_chunk",
                json!({ "modelId": provenance, "promptIndex": current_prompt }),
            )
        } else {
            (
                json!({
                    "type": "assistant",
                    "content": message.text,
                    "model_id": provenance,
                }),
                "agent_message_chunk",
                json!({ "modelId": provenance }),
            )
        };
        chat.push(chat_record);
        updates.push(json!({
            "timestamp": timestamp.timestamp(),
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": update_type,
                    "content": { "type": "text", "text": message.text },
                    "_meta": chunk_meta,
                },
                "_meta": {
                    "eventId": format!("{session_id}-{}", index + 1),
                    "agentTimestampMs": timestamp.timestamp_millis(),
                },
            },
        }));
    }
    GrokBundle {
        summary: json!({
            "info": { "id": session_id, "cwd": cwd },
            "session_summary": session.summary,
            "generated_title": session.summary,
            "created_at": fmt_iso(start),
            "updated_at": fmt_iso(end),
            "last_active_at": fmt_iso(end),
            "num_messages": updates.len(),
            "num_chat_messages": chat.len(),
            "current_model_id": model,
            "chat_format_version": 1,
            "agent_name": target.as_str(),
        }),
        chat,
        updates,
    }
}

fn normalize_output_path(target: TargetTool, output: &Path) -> PathBuf {
    if target.uses_grok_storage() && output.file_name().is_none_or(|name| name != "summary.json") {
        output.join("summary.json")
    } else {
        output.to_path_buf()
    }
}

fn target_path(
    target: TargetTool,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    context: &EmitContext,
) -> Result<PathBuf> {
    let path = match target {
        TargetTool::Pi => context
            .roots
            .pi
            .join(crate::formats::pi::encode_cwd(Path::new(cwd))?)
            .join(format!("{}_{}.jsonl", file_safe_timestamp(start), session_id)),
        TargetTool::Omp => context
            .roots
            .omp
            .join(crate::formats::omp::encode_omp_cwd_with(
                Path::new(cwd),
                &context.home,
                &env::temp_dir(),
            ))
            .join(format!("{}_{}.jsonl", file_safe_timestamp(start), session_id)),
        TargetTool::Droid => context
            .roots
            .droid
            .join(encode_single_dash_cwd(cwd))
            .join(format!("{session_id}.jsonl")),
        TargetTool::Codex => {
            let local = start.with_timezone(&Local);
            context
                .roots
                .codex
                .join(local.format("%Y").to_string())
                .join(local.format("%m").to_string())
                .join(local.format("%d").to_string())
                .join(format!(
                    "rollout-{}-{session_id}.jsonl",
                    local.format("%Y-%m-%dT%H-%M-%S")
                ))
        }
        TargetTool::Claude => context
            .roots
            .claude
            .join(encode_single_dash_cwd(cwd))
            .join(format!("{session_id}.jsonl")),
        TargetTool::Grok | TargetTool::Hyper => context
            .roots
            .grok
            .join(encode_grok_cwd(cwd))
            .join(session_id)
            .join("summary.json"),
    };
    Ok(path)
}

fn write_jsonl(path: &Path, records: &[Value]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("output path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating output directory {}", parent.display()))?;
    atomic_write_jsonl(path, records)
}

fn write_grok_bundle<D: EmitDefaults>(
    summary_path: &Path,
    bundle: &GrokBundle,
    defaults: &mut D,
) -> Result<()> {
    let session_dir = summary_path
        .parent()
        .ok_or_else(|| anyhow!("Grok summary path has no session directory"))?;
    let cwd_dir = session_dir
        .parent()
        .ok_or_else(|| anyhow!("Grok session path has no cwd directory"))?;
    fs::create_dir_all(cwd_dir)
        .with_context(|| format!("creating Grok cwd directory {}", cwd_dir.display()))?;
    if session_dir.exists() {
        bail!("refusing to replace existing Grok session directory {}", session_dir.display());
    }
    let staging = cwd_dir.join(format!(
        ".{}.{}.tmp",
        session_dir.file_name().and_then(|name| name.to_str()).unwrap_or("session"),
        defaults.next_uuid()
    ));
    validate_component(
        "Grok staging directory",
        staging
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("Grok staging directory is not valid UTF-8"))?,
    )?;
    fs::create_dir(&staging)
        .with_context(|| format!("creating Grok staging directory {}", staging.display()))?;
    let result = (|| -> Result<()> {
        atomic_write_jsonl(&staging.join("summary.json"), std::slice::from_ref(&bundle.summary))?;
        atomic_write_jsonl(&staging.join("chat_history.jsonl"), &bundle.chat)?;
        atomic_write_jsonl(&staging.join("updates.jsonl"), &bundle.updates)?;
        fs::rename(&staging, session_dir).with_context(|| {
            format!(
                "publishing Grok session {} as {}",
                staging.display(),
                session_dir.display()
            )
        })?;
        fs::File::open(cwd_dir)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing Grok cwd directory {}", cwd_dir.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn fallback_time<D: EmitDefaults>(
    session: &Session,
    context: &EmitContext,
    defaults: &mut D,
) -> DateTime<Utc> {
    session
        .start_timestamp
        .as_deref()
        .and_then(parse_timestamp)
        .or_else(|| session.modified_epoch.and_then(datetime_from_epoch))
        .or(context.fallback_time)
        .unwrap_or_else(|| defaults.now())
}

fn message_time(
    _session: &Session,
    timestamp: Option<&str>,
    index: usize,
    fallback: DateTime<Utc>,
) -> DateTime<Utc> {
    timestamp
        .and_then(parse_timestamp)
        .unwrap_or_else(|| fallback + TimeDelta::seconds(index as i64 + 1))
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn datetime_from_epoch(epoch: f64) -> Option<DateTime<Utc>> {
    if !epoch.is_finite() {
        return None;
    }
    let seconds = epoch.floor();
    if seconds < i64::MIN as f64 || seconds > i64::MAX as f64 {
        return None;
    }
    let mut nanos = ((epoch - seconds) * 1_000_000_000.0).round() as u32;
    let mut seconds = seconds as i64;
    if nanos == 1_000_000_000 {
        seconds = seconds.checked_add(1)?;
        nanos = 0;
    }
    DateTime::from_timestamp(seconds, nanos)
}

fn fmt_iso(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn file_safe_timestamp(timestamp: DateTime<Utc>) -> String {
    fmt_iso(timestamp).replace([':', '.'], "-")
}

fn short_id(uuid: Uuid) -> String {
    compact_id(uuid, 8)
}

fn compact_id(uuid: Uuid, length: usize) -> String {
    uuid.simple().to_string().chars().take(length).collect()
}

fn zero_usage() -> Value {
    json!({
        "input": 0,
        "output": 0,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": 0,
        "cost": {
            "input": 0,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0,
            "total": 0,
        },
    })
}

fn encode_single_dash_cwd(cwd: &str) -> String {
    let stripped = cwd.trim_matches('/');
    if stripped.is_empty() {
        "-".to_owned()
    } else {
        format!("-{}", stripped.replace('/', "-"))
    }
}

fn encode_grok_cwd(cwd: &str) -> String {
    utf8_percent_encode(cwd, URL_PATH_ENCODE_SET).to_string()
}

fn validate_grok_cwd_component(cwd: &str) -> Result<()> {
    let encoded = encode_grok_cwd(cwd);
    if encoded.len() > MAX_FILESYSTEM_COMPONENT_BYTES {
        bail!(
            "percent-encoded cwd component is {} bytes, exceeding filesystem limit of {} bytes; native slug fallback is intentionally not guessed",
            encoded.len(),
            MAX_FILESYSTEM_COMPONENT_BYTES
        );
    }
    Ok(())
}

fn validate_component(label: &str, component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains(['/', '\\', '\0'])
    {
        bail!("unsafe {label}: {component:?}");
    }
    if component.len() > MAX_FILESYSTEM_COMPONENT_BYTES {
        bail!(
            "{label} is {} bytes, exceeding filesystem limit of {} bytes",
            component.len(),
            MAX_FILESYSTEM_COMPONENT_BYTES
        );
    }
    Ok(())
}

fn command_output_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("child stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("child stderr was not piped"))?;
    let stdout_reader = thread::spawn(move || read_pipe(stdout));
    let stderr_reader = thread::spawn(move || read_pipe(stderr));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{program} {} timed out after {}s",
                    args.join(" "),
                    timeout.as_secs()
                ),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = join_pipe_reader(stdout_reader)?;
    let stderr = join_pipe_reader(stderr_reader)?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_pipe_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> std::io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| std::io::Error::other("command output reader thread panicked"))?
}
fn nonempty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;

    use chrono::TimeZone;
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{Message, SourceTool};

    struct FixedDefaults {
        uuids: VecDeque<Uuid>,
        now: DateTime<Utc>,
    }

    impl FixedDefaults {
        fn new() -> Self {
            let uuids = (1_u128..=64)
                .map(Uuid::from_u128)
                .collect::<VecDeque<_>>();
            Self {
                uuids,
                now: Utc.with_ymd_and_hms(2026, 7, 30, 1, 2, 3).unwrap(),
            }
        }
    }

    impl EmitDefaults for FixedDefaults {
        fn next_uuid(&mut self) -> Uuid {
            self.uuids.pop_front().expect("enough deterministic UUIDs")
        }

        fn now(&mut self) -> DateTime<Utc> {
            self.now
        }

        fn owner(&mut self) -> String {
            "tester".to_owned()
        }

        fn resolve_omp_runtime(&mut self) -> Result<OmpRuntime> {
            OmpRuntime::parse("provider/native-model:high")
        }

        fn resolve_codex_runtime(&mut self) -> Result<CodexRuntime> {
            Ok(CodexRuntime {
                provider: "native-provider".to_owned(),
                model: "native-model".to_owned(),
            })
        }

        fn grok_model(&mut self) -> String {
            "native-grok-model".to_owned()
        }
    }

    fn fixture(home: &Path) -> Session {
        Session {
            tool: SourceTool::Claude,
            session_id: "source".to_owned(),
            cwd: home.join("Projects/demo"),
            start_timestamp: Some("2026-07-30T10:11:12.123Z".to_owned()),
            summary: "Converted session".to_owned(),
            messages: vec![
                Message {
                    role: Role::User,
                    text: "hello".to_owned(),
                    timestamp: Some("2026-07-30T10:11:13.000Z".to_owned()),
                },
                Message {
                    role: Role::Assistant,
                    text: "world".to_owned(),
                    timestamp: Some("2026-07-30T10:11:14.000Z".to_owned()),
                },
            ],
            path: home.join("source.jsonl"),
            modified_epoch: None,
        }
    }

    fn read_jsonl(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn emits_every_target_to_its_native_path() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let session = fixture(home);
        for target in TargetTool::ALL {
            let context = EmitContext::new(home).with_session_id(format!(
                "00000000-0000-4000-8000-0000000000{}",
                target as u8
            ));
            let emitted =
                emit_with_defaults(&session, target, &context, &mut FixedDefaults::new()).unwrap();
            assert!(
                emitted.path.is_file(),
                "{target} did not emit {}",
                emitted.path.display()
            );
            assert!(emitted.path.starts_with(home));
            let name = emitted.path.file_name().unwrap().to_str().unwrap();
            if target.uses_grok_storage() {
                assert_eq!(name, "summary.json");
            } else if matches!(target, TargetTool::Droid | TargetTool::Claude) {
                assert_eq!(name, format!("{}.jsonl", emitted.session_id));
            } else if matches!(target, TargetTool::Pi | TargetTool::Omp) {
                assert!(name.ends_with(&format!("_{}.jsonl", emitted.session_id)));
            } else {
                assert!(name.starts_with("rollout-"));
                assert!(name.ends_with(&format!("-{}.jsonl", emitted.session_id)));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn omp_emits_symlinked_cwds_under_native_project_directories() {
        use std::os::unix::fs::symlink;

        let current = env::current_dir().unwrap();
        let aliases = tempfile::tempdir_in(current).unwrap();
        let real_home = aliases.path().join("real-home");
        fs::create_dir_all(real_home.join("project")).unwrap();
        let home_alias = aliases.path().join("home-alias");
        symlink(&real_home, &home_alias).unwrap();

        let mut session = fixture(&real_home);
        session.cwd = home_alias.join("project");
        let context = EmitContext::new(&real_home)
            .with_session_id("omp-home-symlink")
            .with_omp_runtime(OmpRuntime::parse("provider/model:high").unwrap());
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Omp,
            &context,
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let native_dir = crate::formats::omp::encode_omp_cwd_with(
            &session.cwd,
            &real_home,
            &env::temp_dir(),
        );
        assert_eq!(native_dir, "-project");
        assert_eq!(
            emitted.path.parent().unwrap(),
            context.roots.omp.join(&native_dir)
        );
        assert_ne!(
            native_dir,
            crate::formats::pi::encode_cwd(&session.cwd).unwrap()
        );

        let real_temp = TempDir::new().unwrap();
        fs::create_dir(real_temp.path().join("project")).unwrap();
        let temp_alias = aliases.path().join("temp-alias");
        symlink(real_temp.path(), &temp_alias).unwrap();
        session.cwd = temp_alias.join("project");
        let context = EmitContext::new(&real_home)
            .with_session_id("omp-temp-symlink")
            .with_omp_runtime(OmpRuntime::parse("provider/model:high").unwrap());
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Omp,
            &context,
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let native_dir = crate::formats::omp::encode_omp_cwd_with(
            &session.cwd,
            &real_home,
            &env::temp_dir(),
        );
        assert!(native_dir.starts_with("-tmp-"));
        assert_eq!(
            emitted.path.parent().unwrap(),
            context.roots.omp.join(&native_dir)
        );
        assert_ne!(
            native_dir,
            crate::formats::pi::encode_cwd(&session.cwd).unwrap()
        );
    }

    #[test]
    fn pi_and_omp_emit_v3_tree_headers_and_native_bootstrap() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let session = fixture(home);

        let pi = emit_with_defaults(
            &session,
            TargetTool::Pi,
            &EmitContext::new(home).with_session_id("pi-session"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let pi_records = read_jsonl(&pi.path);
        assert_eq!(pi_records[0]["version"], 3);
        assert_eq!(pi_records[1]["provider"], "sessions-convert");
        assert_eq!(pi_records[2]["thinkingLevel"], "off");
        assert_eq!(pi_records[4]["message"]["content"][0]["text"], "world");

        let omp = emit_with_defaults(
            &session,
            TargetTool::Omp,
            &EmitContext::new(home)
                .with_session_id("omp-session")
                .with_omp_runtime(OmpRuntime::parse("provider/model:xhigh").unwrap()),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let omp_records = read_jsonl(&omp.path);
        assert_eq!(omp_records[0]["convertedFrom"], "claude");
        assert_eq!(omp_records[1]["model"], "provider/model");
        assert_eq!(omp_records[2]["thinkingLevel"], "xhigh");
        assert_eq!(omp_records[4]["message"]["provider"], "provider");
        assert_eq!(omp_records[4]["message"]["model"], "model");
    }

    #[test]
    fn codex_rollout_has_native_context_and_event_order() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let session = fixture(home);
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Codex,
            &EmitContext::new(home)
                .with_session_id("codex-session")
                .with_codex_runtime(CodexRuntime {
                    provider: "provider".to_owned(),
                    model: "model".to_owned(),
                }),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        assert_eq!(records[0]["payload"]["model_provider"], "provider");
        assert_eq!(records[1]["payload"]["workspace_roots"][0], session.cwd.to_str().unwrap());
        assert_eq!(records[1]["payload"]["sandbox_policy"]["type"], "read-only");
        assert_eq!(records[2]["type"], "response_item");
        assert_eq!(records[3]["payload"]["type"], "user_message");
        assert_eq!(records[4]["payload"]["type"], "agent_message");
        assert_eq!(records[5]["type"], "response_item");
    }

    #[test]
    fn claude_is_strict_chain_with_last_prompt_leaf() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let emitted = emit_with_defaults(
            &fixture(home),
            TargetTool::Claude,
            &EmitContext::new(home).with_session_id("claude-session"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        assert!(records[0]["parentUuid"].is_null());
        assert_eq!(records[1]["parentUuid"], records[0]["uuid"]);
        assert_eq!(records[1]["message"]["content"][0]["text"], "world");
        assert_eq!(records[2]["type"], "last-prompt");
        assert_eq!(records[2]["lastPrompt"], "hello");
        assert_eq!(records[2]["leafUuid"], records[1]["uuid"]);
    }

    #[test]
    fn droid_emits_native_session_start_and_parent_chain() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let emitted = emit_with_defaults(
            &fixture(home),
            TargetTool::Droid,
            &EmitContext::new(home)
                .with_session_id("droid-session")
                .with_owner("test-user"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        assert_eq!(records[0]["type"], "session_start");
        assert_eq!(records[0]["owner"], "test-user");
        assert_eq!(records[0]["version"], 2);
        assert!(records[1].get("parentId").is_none());
        assert_eq!(records[2]["parentId"], records[1]["id"]);
    }

    #[test]
    fn grok_and_hyper_share_storage_but_stamp_distinct_agent_names() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let session = fixture(home);
        for (target, id, expected_agent) in [
            (TargetTool::Grok, "grok-session", "grok"),
            (TargetTool::Hyper, "hyper-session", "hyper"),
        ] {
            let emitted = emit_with_defaults(
                &session,
                target,
                &EmitContext::new(home)
                    .with_session_id(id)
                    .with_grok_model("grok-runtime-model"),
                &mut FixedDefaults::new(),
            )
            .unwrap();
            assert!(emitted.path.starts_with(home.join(".grok/sessions")));
            let summary: Value = serde_json::from_str(&fs::read_to_string(&emitted.path).unwrap()).unwrap();
            assert_eq!(summary["agent_name"], expected_agent);
            assert_eq!(summary["current_model_id"], "grok-runtime-model");
            let chat = read_jsonl(&emitted.path.with_file_name("chat_history.jsonl"));
            assert!(chat[0]["content"].is_array());
            assert!(chat[1]["content"].is_string());
            assert_eq!(chat[0]["prompt_index"], 0);
            let updates = read_jsonl(&emitted.path.with_file_name("updates.jsonl"));
            assert_eq!(updates[0]["params"]["update"]["_meta"]["promptIndex"], 0);
            assert!(updates[1]["params"]["update"]["_meta"].get("promptIndex").is_none());
        }
    }

    #[test]
    fn explicit_output_path_is_honored_for_jsonl_and_grok() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let session = fixture(home);

        let jsonl_output = home.join("chosen/custom.jsonl");
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Droid,
            &EmitContext::new(home)
                .with_session_id("custom-droid")
                .with_output(&jsonl_output),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        assert_eq!(emitted.path, jsonl_output);
        assert!(emitted.path.is_file());

        let grok_output_dir = home.join("chosen/grok/custom");
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Hyper,
            &EmitContext::new(home)
                .with_session_id("custom-hyper")
                .with_grok_model("model")
                .with_output(&grok_output_dir),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let grok_output = grok_output_dir.join("summary.json");
        assert_eq!(emitted.path, grok_output);
        assert!(emitted.path.is_file());
        assert!(grok_output.with_file_name("chat_history.jsonl").is_file());
        assert!(grok_output.with_file_name("updates.jsonl").is_file());
    }

    #[test]
    fn rejects_oversized_percent_encoded_cwd_before_writing() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.cwd = PathBuf::from(format!("/{}", "界".repeat(100)));
        let context = EmitContext::new(home).with_session_id("grok-session");
        let error = emit_with_defaults(
            &session,
            TargetTool::Grok,
            &context,
            &mut FixedDefaults::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exceeding filesystem limit"));
        assert!(!home.join(".grok").exists());
    }

    #[test]
    fn empty_conversation_is_rejected_before_any_write() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.messages.clear();
        let error = emit_with_defaults(
            &session,
            TargetTool::Droid,
            &EmitContext::new(home).with_session_id("empty-session"),
            &mut FixedDefaults::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("no convertible"));
        assert!(!home.join(".factory").exists());
    }

    #[test]
    fn invalid_message_timestamps_use_deterministic_offsets() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.start_timestamp = None;
        session.modified_epoch = None;
        session.messages[0].timestamp = Some("bad".to_owned());
        session.messages[1].timestamp = None;
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Droid,
            &EmitContext::new(home).with_session_id("time-session"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        assert_eq!(records[1]["timestamp"], "2026-07-30T01:02:04.000Z");
        assert_eq!(records[2]["timestamp"], "2026-07-30T01:02:05.000Z");
    }
    #[cfg(unix)]
    #[test]
    fn timed_command_drains_output_larger_than_pipe_capacity() {
        let output = command_output_with_timeout(
            "/bin/sh",
            &["-c", "yes x | head -c 131072"],
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 131_072);
    }

}
