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

use crate::domain::{ContentPart, Message, Role, Session, TargetTool, ThinkingLevel};
use crate::fs::atomic_write_jsonl;

const MAX_FILESYSTEM_COMPONENT_BYTES: usize = 255;
const CLAUDE_VERSION: &str = "2.1.263";
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
    pub rpi: PathBuf,
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
            rpi: home.join(".rpi/sessions"),
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
    if let Some(selector) =
        nonempty_env("SESSIONS_OMP_MODEL").or_else(|| nonempty_env("OMP_DEFAULT_MODEL"))
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
    if let (Some(provider), Some(model)) = (
        nonempty_env("SESSIONS_CODEX_PROVIDER"),
        nonempty_env("SESSIONS_CODEX_MODEL"),
    ) {
        return Ok(CodexRuntime { provider, model });
    }
    let doctor =
        command_output_with_timeout("codex", &["doctor", "--json"], Duration::from_secs(15))
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

    let catalog =
        command_output_with_timeout("codex", &["debug", "models"], Duration::from_secs(15))
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
        bail!("cannot resolve Codex runtime state: configured model {model:?} is not available");
    }
    Ok(CodexRuntime {
        provider: provider.to_owned(),
        model: model.to_owned(),
    })
}

pub fn emit_default(session: &Session, target: TargetTool, home: &Path) -> Result<EmittedSession> {
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
    if target == TargetTool::Agent {
        bail!("Agent sessions cannot be emitted or converted");
    }
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
    let grok_cwd = target.uses_grok_storage().then(|| encode_grok_cwd(cwd));
    let output = match &context.output {
        Some(output) => normalize_output_path(target, output),
        None => target_path(
            target,
            cwd,
            grok_cwd.as_deref(),
            &session_id,
            start,
            context,
        )?,
    };

    match target {
        TargetTool::Pi | TargetTool::Rpi => {
            let records = emit_pi(
                session,
                cwd,
                &session_id,
                start,
                target == TargetTool::Rpi,
                defaults,
            );
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
            let model = session
                .hints
                .model
                .clone()
                .or_else(|| context.grok_model.clone())
                .unwrap_or_else(|| defaults.grok_model());
            let bundle = emit_grok(session, cwd, &session_id, start, &model);
            write_grok_bundle(&output, cwd, &bundle, defaults)?;
        }
        TargetTool::Agent => unreachable!("Agent emission rejected before materialization"),
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
    split_user_tool_results: bool,
    defaults: &mut D,
) -> Vec<Value> {
    let provider = session
        .hints
        .provider
        .clone()
        .unwrap_or_else(|| "sessions-convert".to_owned());
    let model = session
        .hints
        .model
        .clone()
        .unwrap_or_else(|| format!("converted-from-{}", session.tool));
    let thinking_level = session.hints.thinking_level.unwrap_or(ThinkingLevel::Off);
    let model_id = short_id(defaults.next_uuid());
    let thinking_id = short_id(defaults.next_uuid());
    let mut header = json!({
        "type": "session",
        "version": 3,
        "id": session_id,
        "timestamp": fmt_iso(start),
        "cwd": cwd,
    });
    if let Some(kind) = session
        .hints
        .session_kind
        .as_deref()
        .filter(|kind| !kind.is_empty())
    {
        header
            .as_object_mut()
            .expect("JSON object")
            .insert("sessionKind".to_owned(), json!(kind));
    }
    let mut records = vec![
        header,
        json!({
            "type": "model_change",
            "id": model_id,
            "parentId": null,
            "timestamp": fmt_iso(start + TimeDelta::milliseconds(100)),
            "provider": provider,
            "modelId": model,
        }),
        json!({
            "type": "thinking_level_change",
            "id": thinking_id,
            "parentId": model_id,
            "timestamp": fmt_iso(start + TimeDelta::milliseconds(200)),
            "thinkingLevel": thinking_level.as_str(),
        }),
    ];
    let mut parent_id = thinking_id;
    push_tree_compaction_hint(
        session,
        &mut records,
        &mut parent_id,
        start + TimeDelta::milliseconds(300),
        defaults,
    );
    for (index, message) in session.messages.iter().enumerate() {
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let (notes, rest) = split_notes(message);
        push_tree_notes(&mut records, &mut parent_id, notes, timestamp, defaults);
        if rest.is_empty() {
            continue;
        }
        let fragments = if split_user_tool_results && message.role == Role::User {
            rpi_user_fragments(rest, message.timestamp.clone())
        } else {
            vec![Message::from_parts(
                message.role,
                rest,
                message.timestamp.clone(),
            )]
        };
        for fragment in fragments {
            push_pi_message_record(
                &mut records,
                &mut parent_id,
                &fragment,
                timestamp,
                defaults,
                &provider,
                &model,
            );
        }
    }
    records
}

fn rpi_user_fragments(parts: Vec<ContentPart>, timestamp: Option<String>) -> Vec<Message> {
    let mut fragments = Vec::new();
    let mut user_parts = Vec::new();
    for part in parts {
        if matches!(part, ContentPart::ToolResult { .. }) {
            if !user_parts.is_empty() {
                fragments.push(Message::from_parts(
                    Role::User,
                    std::mem::take(&mut user_parts),
                    timestamp.clone(),
                ));
            }
            fragments.push(Message::from_parts(
                Role::Tool,
                vec![part],
                timestamp.clone(),
            ));
        } else {
            user_parts.push(part);
        }
    }
    if !user_parts.is_empty() {
        fragments.push(Message::from_parts(Role::User, user_parts, timestamp));
    }
    fragments
}

fn push_pi_message_record<D: EmitDefaults>(
    records: &mut Vec<Value>,
    parent_id: &mut String,
    message: &Message,
    timestamp: DateTime<Utc>,
    defaults: &mut D,
    provider: &str,
    model: &str,
) {
    let message_id = short_id(defaults.next_uuid());
    records.push(json!({
        "type": "message",
        "id": message_id,
        "parentId": parent_id.clone(),
        "timestamp": fmt_iso(timestamp),
        "message": pi_message_payload(message, timestamp, defaults, provider, model),
    }));
    *parent_id = message_id;
}

fn emit_omp<D: EmitDefaults>(
    session: &Session,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    runtime: &OmpRuntime,
    defaults: &mut D,
) -> Result<Vec<Value>> {
    let (runtime_provider, runtime_model) = runtime.provider_and_model()?;
    let provider = session
        .hints
        .provider
        .as_deref()
        .unwrap_or(runtime_provider);
    let model = session.hints.model.as_deref().unwrap_or(runtime_model);
    let model_selector = match (&session.hints.provider, &session.hints.model) {
        (Some(hint_provider), Some(hint_model)) => format!("{hint_provider}/{hint_model}"),
        (None, Some(hint_model)) if hint_model.contains('/') => hint_model.clone(),
        (None, Some(hint_model)) => format!("{provider}/{hint_model}"),
        _ => runtime.model.clone(),
    };
    let thinking_level = session
        .hints
        .thinking_level
        .unwrap_or(runtime.thinking_level);
    let model_id = short_id(defaults.next_uuid());
    let thinking_id = short_id(defaults.next_uuid());
    // Native OMP files begin with a fixed-width title-slot record. The native
    // reader splits the first line as the slot regardless of byte width, so a
    // plain JSONL record with an empty `pad` is native-readable; the slot's
    // non-empty title overrides the `session` header title on load.
    let title_slot = omp_title_slot(session.summary.as_str(), fmt_iso(start));
    let mut header = json!({
        "type": "session",
        "version": 3,
        "id": session_id,
        "timestamp": fmt_iso(start),
        "cwd": cwd,
        "title": session.summary,
    });
    if let Some(kind) = session
        .hints
        .session_kind
        .as_deref()
        .filter(|kind| !kind.is_empty())
    {
        header
            .as_object_mut()
            .expect("JSON object")
            .insert("sessionKind".to_owned(), json!(kind));
    }
    let mut records = vec![
        title_slot,
        header,
        json!({
            "type": "model_change",
            "id": model_id,
            "parentId": null,
            "timestamp": fmt_iso(start + TimeDelta::milliseconds(100)),
            "model": model_selector,
        }),
        json!({
            "type": "thinking_level_change",
            "id": thinking_id,
            "parentId": model_id,
            "timestamp": fmt_iso(start + TimeDelta::milliseconds(200)),
            "thinkingLevel": thinking_level.as_str(),
        }),
    ];
    let mut parent_id = thinking_id;
    push_tree_compaction_hint(
        session,
        &mut records,
        &mut parent_id,
        start + TimeDelta::milliseconds(300),
        defaults,
    );
    for (index, message) in session.messages.iter().enumerate() {
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let (notes, rest) = split_notes(message);
        push_tree_notes(&mut records, &mut parent_id, notes, timestamp, defaults);
        if rest.is_empty() {
            continue;
        }
        let rest_message = Message::from_parts(message.role, rest, message.timestamp.clone());
        let message_id = short_id(defaults.next_uuid());
        records.push(json!({
            "type": "message",
            "id": message_id,
            "parentId": parent_id,
            "timestamp": fmt_iso(timestamp),
            "message": pi_message_payload(
                &rest_message,
                timestamp,
                defaults,
                provider,
                model,
            ),
        }));
        parent_id = message_id;
    }
    Ok(records)
}

/// Build the native OMP leading title-slot record.
///
/// Mirrors `src/session/title-slot.ts::TitleSlotObject`: `type:"title"`,
/// `v:1`, a non-empty `title`, an ISO `updatedAt`, and a `pad` string. The
/// native reader validates only that these are strings, so an empty `pad`
/// is accepted; the fixed 256-byte width is a filesystem optimization, not a
/// parse requirement, and is intentionally not reproduced here.
fn omp_title_slot(title: &str, updated_at: String) -> Value {
    json!({
        "type": "title",
        "v": 1,
        "title": title,
        "updatedAt": updated_at,
        "pad": "",
    })
}

fn emit_droid<D: EmitDefaults>(
    session: &Session,
    cwd: &str,
    session_id: &str,
    start: DateTime<Utc>,
    owner: &str,
    defaults: &mut D,
) -> Vec<Value> {
    let mut start_record = json!({
        "type": "session_start",
        "id": session_id,
        "title": session.summary,
        "sessionTitle": session.summary,
        "owner": owner,
        "version": 2,
        "cwd": cwd,
        "isSessionTitleManuallySet": false,
        "sessionTitleAutoStage": "first_message",
    });
    if let Some(model) = &session.hints.model {
        start_record
            .as_object_mut()
            .expect("JSON object")
            .insert("model".to_owned(), json!(model));
    }
    if let Some(level) = session.hints.thinking_level {
        start_record
            .as_object_mut()
            .expect("JSON object")
            .insert("thinkingLevel".to_owned(), json!(level.as_str()));
    }
    let mut records = vec![start_record];
    if !has_compaction_note(session) {
        if let Some(text) = session
            .hints
            .compaction_summary
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            records.push(json!({
                "type": "compaction_state",
                "id": defaults.next_uuid().to_string(),
                "summary": text,
                "summaryText": text,
            }));
        }
    }
    let mut parent_id: Option<String> = None;
    for (index, message) in session.messages.iter().enumerate() {
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let (notes, rest) = split_notes(message);
        for (kind, text) in notes {
            let note_id = defaults.next_uuid().to_string();
            if kind == "compaction" {
                records.push(json!({
                    "type": "compaction_state",
                    "id": note_id,
                    "summary": text,
                    "summaryText": text,
                }));
            } else {
                let mut record = json!({
                    "type": "message",
                    "id": note_id,
                    "timestamp": fmt_iso(timestamp),
                    "message": {
                        "role": "user",
                        "content": [{ "type": "text", "text": text }],
                    },
                });
                if let Some(parent_id) = &parent_id {
                    record
                        .as_object_mut()
                        .expect("JSON object")
                        .insert("parentId".to_owned(), json!(parent_id));
                }
                records.push(record);
                parent_id = Some(note_id);
            }
        }
        if rest.is_empty() {
            continue;
        }
        let rest_message = Message::from_parts(message.role, rest, message.timestamp.clone());
        let message_id = defaults.next_uuid().to_string();
        let mut record = json!({
            "type": "message",
            "id": message_id,
            "timestamp": fmt_iso(timestamp),
            "message": {
                "role": droid_role(&rest_message),
                "content": claude_content_blocks(&rest_message),
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
                "timestamp": timestamp,
                "cwd": cwd,
                "originator": "codex",
                "source": "cli",
                "cli_version": env!("CARGO_PKG_VERSION"),
                "model_provider": session
                    .hints
                    .provider
                    .as_deref()
                    .unwrap_or(&runtime.provider),
            },
        }),
        json!({
            "timestamp": timestamp,
            "type": "turn_context",
            "payload": {
                "cwd": cwd,
                "approval_policy": "never",
                "sandbox_policy": { "type": "read-only" },
                "model": session.hints.model.as_deref().unwrap_or(&runtime.model),
                "summary": "auto",
                "reasoning_effort": session
                    .hints
                    .thinking_level
                    .map(|level| level.as_str())
                    .unwrap_or("auto"),
            },
        }),
    ];
    if !has_compaction_note(session) {
        if let Some(text) = session
            .hints
            .compaction_summary
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            records.push(json!({
                "timestamp": timestamp,
                "type": "compacted",
                "payload": { "message": text },
            }));
        }
    }
    for (index, message) in session.messages.iter().enumerate() {
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let timestamp_text = fmt_iso(timestamp);
        let (notes, rest) = split_notes(message);
        for (kind, text) in notes {
            if kind == "compaction" {
                records.push(json!({
                    "timestamp": timestamp_text,
                    "type": "compacted",
                    "payload": { "message": text },
                }));
            } else {
                let note = Message::from_parts(
                    message.role,
                    vec![ContentPart::Text(text)],
                    message.timestamp.clone(),
                );
                records.extend(codex_records(&note, &timestamp_text));
            }
        }
        if rest.is_empty() {
            continue;
        }
        let rest_message = Message::from_parts(message.role, rest, message.timestamp.clone());
        records.extend(codex_records(&rest_message, &timestamp_text));
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
    let mut records =
        Vec::with_capacity(session.messages.len() + usize::from(!session.messages.is_empty()));
    let mut parent_uuid: Option<String> = None;
    let mut last_user_text = String::new();
    let model = session
        .hints
        .model
        .clone()
        .unwrap_or_else(|| format!("converted-from-{}", session.tool));
    if !has_compaction_note(session) {
        if let Some(text) = session
            .hints
            .compaction_summary
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            let message_uuid = defaults.next_uuid().to_string();
            records.push(json!({
                "parentUuid": parent_uuid,
                "isSidechain": false,
                "userType": "external",
                "cwd": cwd,
                "sessionId": session_id,
                "version": CLAUDE_VERSION,
                "gitBranch": "",
                "type": "user",
                "isCompactSummary": true,
                "uuid": message_uuid,
                "timestamp": fmt_iso(start),
                "message": { "role": "user", "content": text },
                "permissionMode": "default",
            }));
            parent_uuid = Some(message_uuid);
        }
    }
    for (index, message) in session.messages.iter().enumerate() {
        let timestamp = message_time(session, message.timestamp.as_deref(), index, start);
        let (notes, rest) = split_notes(message);
        for (kind, text) in notes {
            let message_uuid = defaults.next_uuid().to_string();
            if kind == "compaction" {
                records.push(json!({
                    "parentUuid": parent_uuid,
                    "isSidechain": false,
                    "userType": "external",
                    "cwd": cwd,
                    "sessionId": session_id,
                    "version": CLAUDE_VERSION,
                    "gitBranch": "",
                    "type": "user",
                    "isCompactSummary": true,
                    "uuid": message_uuid,
                    "timestamp": fmt_iso(timestamp),
                    "message": { "role": "user", "content": text },
                    "permissionMode": "default",
                }));
            } else {
                records.push(json!({
                    "parentUuid": parent_uuid,
                    "isSidechain": false,
                    "userType": "external",
                    "cwd": cwd,
                    "sessionId": session_id,
                    "version": CLAUDE_VERSION,
                    "gitBranch": "",
                    "type": "attachment",
                    "uuid": message_uuid,
                    "timestamp": fmt_iso(timestamp),
                    "attachment": {
                        "type": kind,
                        "content": text,
                    },
                }));
            }
            parent_uuid = Some(message_uuid);
        }
        if rest.is_empty() {
            continue;
        }
        let rest_message = Message::from_parts(message.role, rest, message.timestamp.clone());
        let message_uuid = defaults.next_uuid().to_string();
        let record_type = claude_record_type(&rest_message);
        let mut record = json!({
            "parentUuid": parent_uuid,
            "isSidechain": false,
            "userType": "external",
            "cwd": cwd,
            "sessionId": session_id,
            "version": CLAUDE_VERSION,
            "gitBranch": "",
            "type": record_type,
            "uuid": message_uuid,
            "timestamp": fmt_iso(timestamp),
        });
        let object = record.as_object_mut().expect("JSON object");
        if record_type == "user" {
            if rest_message.role == Role::User {
                last_user_text = rest_message.text.clone();
            }
            object.insert(
                "message".to_owned(),
                json!({ "role": "user", "content": claude_user_content(&rest_message) }),
            );
            object.insert("permissionMode".to_owned(), json!("default"));
        } else {
            let mut assistant = json!({
                "model": model,
                "id": format!("msg_converted_{}", defaults.next_uuid().simple()),
                "type": "message",
                "role": "assistant",
                "content": claude_content_blocks(&rest_message),
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            });
            if let Some(level) = session.hints.thinking_level {
                object.insert("effort".to_owned(), json!(level.as_str()));
                assistant
                    .as_object_mut()
                    .expect("JSON object")
                    .insert("effort".to_owned(), json!(level.as_str()));
            }
            object.insert("message".to_owned(), assistant);
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
        let provenance = model.to_owned();
        let (chat_records, update_records) = grok_records(
            message,
            session_id,
            timestamp,
            &provenance,
            &mut prompt_index,
            index,
        );
        chat.extend(chat_records);
        updates.extend(update_records);
    }
    GrokBundle {
        summary: grok_summary(
            session,
            cwd,
            session_id,
            model,
            start,
            end,
            chat.len(),
            updates.len(),
        ),
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
    grok_cwd: Option<&str>,
    session_id: &str,
    start: DateTime<Utc>,
    context: &EmitContext,
) -> Result<PathBuf> {
    let path = match target {
        TargetTool::Pi => context
            .roots
            .pi
            .join(crate::formats::pi::encode_cwd(Path::new(cwd))?)
            .join(format!(
                "{}_{}.jsonl",
                file_safe_timestamp(start),
                session_id
            )),
        TargetTool::Rpi => context
            .roots
            .rpi
            .join(crate::formats::pi::encode_cwd(Path::new(cwd))?)
            .join(format!(
                "{}_{}.jsonl",
                file_safe_timestamp(start),
                session_id
            )),
        TargetTool::Omp => context
            .roots
            .omp
            .join(crate::formats::omp::encode_omp_cwd_with(
                Path::new(cwd),
                &context.home,
                &env::temp_dir(),
            ))
            .join(format!(
                "{}_{}.jsonl",
                file_safe_timestamp(start),
                session_id
            )),
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
            .join(grok_cwd.expect("Grok cwd computed before target path"))
            .join(session_id)
            .join("summary.json"),
        TargetTool::Agent => unreachable!("Agent emission rejected before path selection"),
    };
    Ok(path)
}

fn split_notes(message: &Message) -> (Vec<(String, String)>, Vec<ContentPart>) {
    let mut notes = Vec::new();
    let mut rest = Vec::new();
    for part in message.effective_parts() {
        match part {
            ContentPart::Note { kind, text } => notes.push((kind, text)),
            other => rest.push(other),
        }
    }
    (notes, rest)
}

fn has_compaction_note(session: &Session) -> bool {
    session.messages.iter().any(|message| {
        message
            .effective_parts()
            .iter()
            .any(|part| matches!(part, ContentPart::Note { kind, .. } if kind == "compaction"))
    })
}

fn push_tree_compaction_hint<D: EmitDefaults>(
    session: &Session,
    records: &mut Vec<Value>,
    parent_id: &mut String,
    timestamp: DateTime<Utc>,
    defaults: &mut D,
) {
    if has_compaction_note(session) {
        return;
    }
    let Some(text) = session
        .hints
        .compaction_summary
        .as_deref()
        .filter(|text| !text.is_empty())
    else {
        return;
    };
    push_tree_notes(
        records,
        parent_id,
        vec![("compaction".to_owned(), text.to_owned())],
        timestamp,
        defaults,
    );
}

fn push_tree_notes<D: EmitDefaults>(
    records: &mut Vec<Value>,
    parent_id: &mut String,
    notes: Vec<(String, String)>,
    timestamp: DateTime<Utc>,
    defaults: &mut D,
) {
    for (kind, text) in notes {
        let message_id = short_id(defaults.next_uuid());
        if kind == "compaction" {
            records.push(json!({
                "type": "compaction",
                "id": message_id,
                "parentId": parent_id,
                "timestamp": fmt_iso(timestamp),
                "summary": text,
            }));
        } else if kind == "branch_summary" {
            records.push(json!({
                "type": "branch_summary",
                "id": message_id,
                "parentId": parent_id,
                "timestamp": fmt_iso(timestamp),
                "fromId": parent_id,
                "summary": text,
            }));
        } else {
            records.push(json!({
                "type": "custom_message",
                "id": message_id,
                "parentId": parent_id,
                "timestamp": fmt_iso(timestamp),
                "customType": kind,
                "content": text,
                "display": true,
            }));
        }
        *parent_id = message_id;
    }
}

fn tool_arguments(input: &Value) -> Value {
    match input {
        Value::String(text) => json!(text),
        other => json!(other.to_string()),
    }
}

fn image_ref(mime_type: Option<&str>, data: impl AsRef<str>) -> String {
    let data = data.as_ref();
    if data.starts_with("data:") || data.contains("://") || data.starts_with('/') {
        data.to_owned()
    } else {
        format!("data:{};base64,{data}", mime_type.unwrap_or("image/png"))
    }
}

fn pi_message_payload<D: EmitDefaults>(
    message: &Message,
    timestamp: DateTime<Utc>,
    defaults: &mut D,
    provider: &str,
    model: &str,
) -> Value {
    let parts = message.effective_parts();
    let mut payload = serde_json::Map::new();
    payload.insert("timestamp".to_owned(), json!(timestamp.timestamp_millis()));
    payload.insert("usage".to_owned(), zero_usage());
    if message.role == Role::Tool {
        let result = parts.iter().find_map(|part| match part {
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), content.as_str(), *is_error)),
            _ => None,
        });
        payload.insert("role".to_owned(), json!("toolResult"));
        if let Some((tool_use_id, content, is_error)) = result {
            payload.insert("toolCallId".to_owned(), json!(tool_use_id));
            payload.insert("toolName".to_owned(), json!(""));
            payload.insert(
                "content".to_owned(),
                tool_result_content(content, &parts, false),
            );
            payload.insert("isError".to_owned(), json!(is_error));
        } else {
            payload.insert("content".to_owned(), json!(pi_content_blocks(&parts)));
        }
        return Value::Object(payload);
    }
    payload.insert("role".to_owned(), json!(message.role.as_str()));
    payload.insert("content".to_owned(), json!(pi_content_blocks(&parts)));
    if message.role == Role::Assistant {
        payload.insert("api".to_owned(), json!("openai-completions"));
        payload.insert("provider".to_owned(), json!(provider));
        payload.insert("model".to_owned(), json!(model));
        payload.insert("stopReason".to_owned(), json!("stop"));
        payload.insert(
            "responseId".to_owned(),
            json!(format!(
                "converted-{}",
                compact_id(defaults.next_uuid(), 12)
            )),
        );
    }
    Value::Object(payload)
}

fn pi_content_blocks(parts: &[ContentPart]) -> Vec<Value> {
    if parts.is_empty() {
        return Vec::new();
    }
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => json!({ "type": "text", "text": text }),
            ContentPart::Thinking { text, signature } => {
                let mut block = json!({ "type": "thinking", "thinking": text });
                if let Some(signature) = signature {
                    block
                        .as_object_mut()
                        .expect("object")
                        .insert("thinkingSignature".to_owned(), json!(signature));
                }
                block
            }
            ContentPart::ToolUse { id, name, input } => json!({
                "type": "toolCall",
                "id": id,
                "name": name,
                "arguments": input,
            }),
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => json!({
                "type": "tool_result",
                "toolCallId": tool_use_id,
                "content": content,
                "isError": is_error,
            }),
            ContentPart::Image { mime_type, data } => json!({
                "type": "image",
                "data": data,
                "mimeType": mime_type.clone().unwrap_or_else(|| "image/png".to_owned()),
            }),
            ContentPart::Note { text, .. } => json!({ "type": "text", "text": text }),
        })
        .collect()
}

fn droid_role(message: &Message) -> &'static str {
    match message.role {
        Role::Tool => "user",
        other => other.as_str(),
    }
}

fn claude_record_type(message: &Message) -> &'static str {
    match message.role {
        Role::Assistant => "assistant",
        Role::User | Role::Tool => "user",
    }
}

fn claude_user_content(message: &Message) -> Value {
    let parts = message.effective_parts();
    if parts.len() == 1 {
        if let ContentPart::Text(text) = &parts[0] {
            return json!(text);
        }
    }
    Value::Array(claude_content_blocks(message))
}

fn claude_content_blocks(message: &Message) -> Vec<Value> {
    let parts = message.effective_parts();
    if parts.is_empty() {
        return vec![json!({ "type": "text", "text": message.text })];
    }
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(json!({ "type": "text", "text": text })),
            ContentPart::Thinking { text, signature } => Some(json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature.clone().unwrap_or_default(),
            })),
            ContentPart::ToolUse { id, name, input } => Some(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": tool_result_content(content, &parts, true),
                "is_error": is_error,
            })),
            ContentPart::Image { mime_type, data } if message.role != Role::Tool => Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type.clone().unwrap_or_else(|| "image/png".to_owned()),
                    "data": data,
                }
            })),
            ContentPart::Image { .. } => None,
            ContentPart::Note { text, .. } => Some(json!({ "type": "text", "text": text })),
        })
        .collect()
}

fn tool_result_content(content: &str, parts: &[ContentPart], claude_style: bool) -> Value {
    let images: Vec<&ContentPart> = parts
        .iter()
        .filter(|part| matches!(part, ContentPart::Image { .. }))
        .collect();
    if images.is_empty() {
        return json!(content);
    }
    let mut blocks = vec![json!({ "type": "text", "text": content })];
    for part in images {
        let ContentPart::Image { mime_type, data } = part else {
            continue;
        };
        if claude_style {
            blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type.clone().unwrap_or_else(|| "image/png".to_owned()),
                    "data": data,
                }
            }));
        } else {
            blocks.push(json!({
                "type": "image",
                "data": data,
                "mimeType": mime_type.clone().unwrap_or_else(|| "image/png".to_owned()),
            }));
        }
    }
    json!(blocks)
}

fn codex_records(message: &Message, timestamp_text: &str) -> Vec<Value> {
    let mut records = Vec::new();
    let mut grouped = Vec::new();
    for part in message.effective_parts() {
        if matches!(message.role, Role::User | Role::Assistant)
            && matches!(
                part,
                ContentPart::Text(_) | ContentPart::Image { .. } | ContentPart::Note { .. }
            )
        {
            grouped.push(part);
            continue;
        }
        records.extend(codex_grouped_message(
            message.role,
            &grouped,
            timestamp_text,
        ));
        grouped.clear();
        records.extend(codex_special_part(&part, timestamp_text));
    }
    records.extend(codex_grouped_message(
        message.role,
        &grouped,
        timestamp_text,
    ));
    records
}

fn codex_grouped_message(role: Role, parts: &[ContentPart], timestamp_text: &str) -> Vec<Value> {
    if parts.is_empty() {
        return Vec::new();
    }
    let mut content = Vec::new();
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text(text) | ContentPart::Note { text, .. } => {
                let content_type = if role == Role::User {
                    "input_text"
                } else {
                    "output_text"
                };
                content.push(json!({ "type": content_type, "text": text }));
                if !text.is_empty() {
                    texts.push(text.as_str());
                }
            }
            ContentPart::Image { mime_type, data } => {
                let url = image_ref(mime_type.as_deref(), data);
                let content_type = if role == Role::User {
                    "input_image"
                } else {
                    "output_image"
                };
                content.push(json!({ "type": content_type, "image_url": url }));
                images.push(url);
            }
            _ => {}
        }
    }
    let record_role = if role == Role::Tool {
        "user"
    } else {
        role.as_str()
    };
    let response_item = json!({
        "timestamp": timestamp_text,
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": record_role,
            "content": content,
        },
    });
    let text = texts.join("\n");
    match role {
        Role::User => vec![
            response_item,
            json!({
                "timestamp": timestamp_text,
                "type": "event_msg",
                "payload": {
                    "type": "user_message",
                    "message": text,
                    "images": images,
                    "local_images": [],
                    "text_elements": [],
                },
            }),
        ],
        Role::Assistant if !text.is_empty() => vec![
            json!({
                "timestamp": timestamp_text,
                "type": "event_msg",
                "payload": {
                    "type": "agent_message",
                    "message": text,
                    "phase": null,
                    "memory_citation": null,
                },
            }),
            response_item,
        ],
        _ => vec![response_item],
    }
}

fn codex_special_part(part: &ContentPart, timestamp_text: &str) -> Vec<Value> {
    match part {
        ContentPart::Thinking { text, .. } => vec![json!({
            "timestamp": timestamp_text,
            "type": "response_item",
            "payload": {
                "type": "reasoning",
                "content": [{ "type": "output_text", "text": text }],
            },
        })],
        ContentPart::ToolUse { id, name, input } => vec![json!({
            "timestamp": timestamp_text,
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": id,
                "name": name,
                "arguments": tool_arguments(input),
            },
        })],
        ContentPart::ToolResult {
            tool_use_id,
            content,
            ..
        } => vec![json!({
            "timestamp": timestamp_text,
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": tool_use_id,
                "output": content,
            },
        })],
        ContentPart::Image { mime_type, data } => {
            let url = image_ref(mime_type.as_deref(), data);
            vec![json!({
                "timestamp": timestamp_text,
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_image", "image_url": url }],
                },
            })]
        }
        _ => Vec::new(),
    }
}

fn grok_summary(
    session: &Session,
    cwd: &str,
    session_id: &str,
    model: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    chat_len: usize,
    updates_len: usize,
) -> Value {
    let mut summary = json!({
        "info": { "id": session_id, "cwd": cwd },
        "session_summary": session.summary,
        "generated_title": session.summary,
        "created_at": fmt_iso(start),
        "updated_at": fmt_iso(end),
        "last_active_at": fmt_iso(end),
        "num_messages": updates_len,
        "num_chat_messages": chat_len,
        "current_model_id": model,
        "chat_format_version": 1,
    });
    let object = summary.as_object_mut().expect("JSON object");
    if let Some(level) = session.hints.thinking_level {
        object.insert("reasoning_effort".to_owned(), json!(level.as_str()));
    }
    if let Some(kind) = session
        .hints
        .session_kind
        .as_deref()
        .filter(|kind| !kind.is_empty())
    {
        object.insert("session_kind".to_owned(), json!(kind));
    }
    if session.hints.hidden {
        object.insert("hidden".to_owned(), json!(true));
    }
    summary
}

fn grok_records(
    message: &Message,
    session_id: &str,
    timestamp: DateTime<Utc>,
    provenance: &str,
    prompt_index: &mut u64,
    index: usize,
) -> (Vec<Value>, Vec<Value>) {
    let mut chat = Vec::new();
    let mut updates = Vec::new();
    let mut parts = message.effective_parts();
    if message.role == Role::User {
        let visible: Vec<ContentPart> = parts
            .iter()
            .filter(|part| {
                matches!(
                    part,
                    ContentPart::Text(_) | ContentPart::Image { .. } | ContentPart::Note { .. }
                )
            })
            .cloned()
            .collect();
        if !visible.is_empty() {
            let current_prompt = *prompt_index;
            *prompt_index += 1;
            let content: Vec<Value> = visible
                .iter()
                .map(|part| match part {
                    ContentPart::Image { mime_type, data } => {
                        json!({ "type": "image", "url": image_ref(mime_type.as_deref(), data) })
                    }
                    ContentPart::Text(text) | ContentPart::Note { text, .. } => {
                        json!({ "type": "text", "text": text })
                    }
                    _ => json!({ "type": "text", "text": "" }),
                })
                .collect();
            let text = visible
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) | ContentPart::Note { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            chat.push(json!({
                "type": "user",
                "content": content,
                "prompt_index": current_prompt,
            }));
            push_grok_update(
                &mut updates,
                session_id,
                timestamp,
                index,
                "user_message_chunk",
                json!({ "type": "text", "text": text }),
                json!({ "modelId": provenance, "promptIndex": current_prompt }),
            );
        }
        parts.retain(|part| {
            !matches!(
                part,
                ContentPart::Text(_) | ContentPart::Image { .. } | ContentPart::Note { .. }
            )
        });
    }
    if message.role == Role::Assistant {
        let thinking: Vec<ContentPart> = parts
            .iter()
            .filter(|part| matches!(part, ContentPart::Thinking { .. }))
            .cloned()
            .collect();
        let grouped: Vec<ContentPart> = parts
            .iter()
            .filter(|part| {
                matches!(
                    part,
                    ContentPart::Text(_)
                        | ContentPart::Image { .. }
                        | ContentPart::Note { .. }
                        | ContentPart::ToolUse { .. }
                )
            })
            .cloned()
            .collect();
        for part in thinking {
            let ContentPart::Thinking { text, .. } = part else {
                continue;
            };
            chat.push(json!({
                "type": "reasoning",
                "content": text,
                "summary": [{ "type": "summary_text", "text": text }],
                "model_id": provenance,
            }));
            push_grok_update(
                &mut updates,
                session_id,
                timestamp,
                index,
                "agent_thought_chunk",
                json!({ "type": "text", "text": text }),
                json!({ "modelId": provenance }),
            );
        }
        if !grouped.is_empty() {
            let content: Vec<Value> = grouped
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) | ContentPart::Note { text, .. } => {
                        Some(json!({ "type": "text", "text": text }))
                    }
                    ContentPart::Image { mime_type, data } => Some(
                        json!({ "type": "image", "url": image_ref(mime_type.as_deref(), data) }),
                    ),
                    _ => None,
                })
                .collect();
            let tool_calls: Vec<Value> = grouped
                .iter()
                .filter_map(|part| match part {
                    ContentPart::ToolUse { id, name, input } => Some(json!({
                        "id": id,
                        "name": name,
                        "arguments": input,
                    })),
                    _ => None,
                })
                .collect();
            let text = grouped
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) | ContentPart::Note { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let content_value = if content.len() == 1
                && content[0].get("type").and_then(Value::as_str) == Some("text")
            {
                json!(text)
            } else if content.is_empty() {
                json!(text)
            } else {
                json!(content)
            };
            let mut record = json!({
                "type": "assistant",
                "content": content_value,
                "model_id": provenance,
            });
            if !tool_calls.is_empty() {
                record
                    .as_object_mut()
                    .expect("JSON object")
                    .insert("tool_calls".to_owned(), json!(tool_calls));
            }
            chat.push(record);
            push_grok_update(
                &mut updates,
                session_id,
                timestamp,
                index,
                "agent_message_chunk",
                json!({ "type": "text", "text": text }),
                json!({ "modelId": provenance }),
            );
        }
        parts.retain(|part| matches!(part, ContentPart::ToolResult { .. }));
    }
    if message.role == Role::Tool {
        let images: Vec<Value> = parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Image { mime_type, data } => {
                    Some(json!({ "type": "image", "url": image_ref(mime_type.as_deref(), data) }))
                }
                _ => None,
            })
            .collect();
        parts.retain(|part| !matches!(part, ContentPart::Image { .. }));
        if let Some((tool_use_id, content, is_error)) = parts.iter().find_map(|part| match part {
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.clone(), content.clone(), *is_error)),
            _ => None,
        }) {
            let mut record = json!({
                "type": "tool_result",
                "id": tool_use_id,
                "tool_call_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            });
            if !images.is_empty() {
                record
                    .as_object_mut()
                    .expect("JSON object")
                    .insert("images".to_owned(), json!(images));
            }
            chat.push(record);
            push_grok_update(
                &mut updates,
                session_id,
                timestamp,
                index,
                "tool_call",
                json!({ "type": "text", "text": content }),
                json!({
                    "toolCallId": tool_use_id,
                    "status": if is_error { "failed" } else { "completed" }
                }),
            );
            parts.retain(|part| !matches!(part, ContentPart::ToolResult { .. }));
        }
    }
    for part in parts {
        let (chat_record, update_type, content, extra_update) = match &part {
            ContentPart::Text(text) if message.role == Role::User => {
                let current_prompt = *prompt_index;
                *prompt_index += 1;
                (
                    json!({
                        "type": "user",
                        "content": [{ "type": "text", "text": text }],
                        "prompt_index": current_prompt,
                    }),
                    "user_message_chunk",
                    json!({ "type": "text", "text": text }),
                    json!({ "modelId": provenance, "promptIndex": current_prompt }),
                )
            }
            ContentPart::Text(text) => (
                json!({
                    "type": "assistant",
                    "content": text,
                    "model_id": provenance,
                }),
                "agent_message_chunk",
                json!({ "type": "text", "text": text }),
                json!({ "modelId": provenance }),
            ),
            ContentPart::Thinking { text, .. } => (
                json!({
                    "type": "reasoning",
                    "content": text,
                    "summary": [{ "type": "summary_text", "text": text }],
                    "model_id": provenance,
                }),
                "agent_thought_chunk",
                json!({ "type": "text", "text": text }),
                json!({ "modelId": provenance }),
            ),
            ContentPart::ToolUse { id, name, input } => (
                json!({
                    "type": "tool_call",
                    "id": id,
                    "name": name,
                    "arguments": input,
                }),
                "tool_call",
                json!({ "type": "text", "text": name }),
                json!({ "toolCallId": id, "title": name, "kind": "other", "status": "pending", "rawInput": input }),
            ),
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => (
                json!({
                    "type": "tool_result",
                    "id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                }),
                "tool_call",
                json!({ "type": "text", "text": content }),
                json!({ "toolCallId": tool_use_id, "status": if *is_error { "failed" } else { "completed" } }),
            ),
            ContentPart::Note { text, .. } => (
                json!({
                    "type": if message.role == Role::User { "user" } else { "assistant" },
                    "content": [{ "type": "text", "text": text }],
                    "model_id": provenance,
                }),
                if message.role == Role::User {
                    "user_message_chunk"
                } else {
                    "agent_message_chunk"
                },
                json!({ "type": "text", "text": text }),
                json!({ "modelId": provenance }),
            ),
            ContentPart::Image { mime_type, data } => {
                let url = image_ref(mime_type.as_deref(), data);
                (
                    json!({
                        "type": if message.role == Role::User { "user" } else { "assistant" },
                        "content": [{ "type": "image", "url": url }],
                    }),
                    if message.role == Role::User {
                        "user_message_chunk"
                    } else {
                        "agent_message_chunk"
                    },
                    json!({ "type": "image", "url": url }),
                    json!({ "modelId": provenance }),
                )
            }
        };
        chat.push(chat_record);
        push_grok_update(
            &mut updates,
            session_id,
            timestamp,
            index,
            update_type,
            content,
            extra_update,
        );
    }
    (chat, updates)
}

fn push_grok_update(
    updates: &mut Vec<Value>,
    session_id: &str,
    timestamp: DateTime<Utc>,
    index: usize,
    update_type: &str,
    content: Value,
    extra_update: Value,
) {
    let mut update = json!({
        "sessionUpdate": update_type,
        "content": content,
        "_meta": extra_update,
    });
    if let Some(object) = extra_update.as_object() {
        if let Some(update_object) = update.as_object_mut() {
            for (key, value) in object {
                if key != "modelId" && key != "promptIndex" {
                    update_object.insert(key.clone(), value.clone());
                }
            }
        }
    }
    updates.push(json!({
        "timestamp": timestamp.timestamp(),
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update,
            "_meta": {
                "eventId": format!("{session_id}-{}", index + 1),
                "agentTimestampMs": timestamp.timestamp_millis(),
            },
        },
    }));
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
    cwd: &str,
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
        bail!(
            "refusing to replace existing Grok session directory {}",
            session_dir.display()
        );
    }
    let staging = cwd_dir.join(format!(
        ".{}.{}.tmp",
        session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session"),
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
        atomic_write_jsonl(
            &staging.join("summary.json"),
            std::slice::from_ref(&bundle.summary),
        )?;
        atomic_write_jsonl(&staging.join("chat_history.jsonl"), &bundle.chat)?;
        atomic_write_jsonl(&staging.join("updates.jsonl"), &bundle.updates)?;
        fs::rename(&staging, session_dir).with_context(|| {
            format!(
                "publishing Grok session {} as {}",
                staging.display(),
                session_dir.display()
            )
        })?;
        let encoded = utf8_percent_encode(cwd, URL_PATH_ENCODE_SET).to_string();
        if encoded != encode_grok_cwd(cwd) {
            let cwd_sidecar = cwd_dir.join(".cwd");
            if !cwd_sidecar.exists() {
                fs::write(&cwd_sidecar, cwd).with_context(|| {
                    format!("writing Grok cwd sidecar {}", cwd_sidecar.display())
                })?;
            }
        }
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

pub(crate) fn encode_single_dash_cwd(cwd: &str) -> String {
    let stripped = cwd.trim_matches('/');
    if stripped.is_empty() {
        "-".to_owned()
    } else {
        format!("-{}", stripped.replace('/', "-"))
    }
}

pub(crate) fn encode_grok_cwd(cwd: &str) -> String {
    let encoded = utf8_percent_encode(cwd, URL_PATH_ENCODE_SET).to_string();
    if encoded.len() <= MAX_FILESYSTEM_COMPONENT_BYTES {
        return encoded;
    }
    let leaf = Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let slug = grok_slug(leaf, 40);
    let slug = if slug.is_empty() { "workspace" } else { &slug };
    let hash = blake3::hash(cwd.as_bytes()).to_hex();
    format!("{slug}-{}", &hash[..16])
}

fn grok_slug(value: &str, limit: usize) -> String {
    let mut slug = String::with_capacity(value.len());
    let mut previous_dash = false;
    for character in value.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }
    slug.trim_matches('-').chars().take(limit).collect()
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
    use crate::domain::{ContentPart, Message, Role, SourceTool};

    struct FixedDefaults {
        uuids: VecDeque<Uuid>,
        now: DateTime<Utc>,
    }

    impl FixedDefaults {
        fn new() -> Self {
            let uuids = (1_u128..=64).map(Uuid::from_u128).collect::<VecDeque<_>>();
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
                Message::plain(
                    Role::User,
                    "hello",
                    Some("2026-07-30T10:11:13.000Z".to_owned()),
                ),
                Message::plain(
                    Role::Assistant,
                    "world",
                    Some("2026-07-30T10:11:14.000Z".to_owned()),
                ),
            ],
            path: home.join("source.jsonl"),
            modified_epoch: None,
            hints: crate::domain::SessionHints::default(),
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
        for target in TargetTool::ALL
            .into_iter()
            .filter(|target| *target != TargetTool::Agent)
        {
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
            } else if target.uses_pi_storage() || matches!(target, TargetTool::Omp) {
                assert!(name.ends_with(&format!("_{}.jsonl", emitted.session_id)));
            } else {
                assert!(name.starts_with("rollout-"));
                assert!(name.ends_with(&format!("-{}.jsonl", emitted.session_id)));
            }
        }
    }

    #[test]
    fn agent_emission_is_rejected_without_creating_store_files() {
        let temporary = TempDir::new().unwrap();
        let context = EmitContext::new(temporary.path());
        let error = emit_with_defaults(
            &fixture(temporary.path()),
            TargetTool::Agent,
            &context,
            &mut FixedDefaults::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be emitted"));
        assert!(!temporary.path().join(".cursor").exists());
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
        let native_dir =
            crate::formats::omp::encode_omp_cwd_with(&session.cwd, &real_home, &env::temp_dir());
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
        let native_dir =
            crate::formats::omp::encode_omp_cwd_with(&session.cwd, &real_home, &env::temp_dir());
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
    fn rpi_emits_into_rpi_sessions_not_pi() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let session = fixture(home);
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Rpi,
            &EmitContext::new(home).with_session_id("rpi-session"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        assert!(
            emitted.path.starts_with(home.join(".rpi/sessions")),
            "{}",
            emitted.path.display()
        );
        assert!(!emitted.path.starts_with(home.join(".pi/agent/sessions")));
    }

    #[test]
    fn rpi_splits_user_tool_results_into_native_records() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.messages = vec![
            Message::from_parts(
                Role::User,
                vec![
                    ContentPart::Text("list the file".to_owned()),
                    ContentPart::ToolResult {
                        tool_use_id: "call-1".to_owned(),
                        content: "fn main() {}".to_owned(),
                        is_error: false,
                    },
                    ContentPart::Image {
                        mime_type: Some("image/png".to_owned()),
                        data: "toolImgData".to_owned(),
                    },
                    ContentPart::ToolResult {
                        tool_use_id: "call-2".to_owned(),
                        content: "second result".to_owned(),
                        is_error: true,
                    },
                ],
                Some("2026-07-30T10:11:13.000Z".to_owned()),
            ),
            Message::plain(
                Role::Assistant,
                "done",
                Some("2026-07-30T10:11:14.000Z".to_owned()),
            ),
        ];
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Rpi,
            &EmitContext::new(home).with_session_id("rpi-tool-result"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        let messages: Vec<&Value> = records
            .iter()
            .filter(|record| record.get("type").and_then(Value::as_str) == Some("message"))
            .collect();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["message"]["role"], "user");
        assert_eq!(
            messages[0]["message"]["content"][0]["text"],
            "list the file"
        );
        assert!(
            messages[0]["message"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|block| block.get("type").and_then(Value::as_str) != Some("tool_result"))
        );
        assert_eq!(messages[1]["message"]["role"], "toolResult");
        assert_eq!(messages[1]["message"]["toolCallId"], "call-1");
        assert_eq!(messages[1]["message"]["content"], "fn main() {}");
        assert_eq!(messages[1]["message"]["isError"], false);
        assert_eq!(messages[2]["message"]["role"], "user");
        assert_eq!(messages[2]["message"]["content"][0]["type"], "image");
        assert_eq!(messages[2]["message"]["content"][0]["data"], "toolImgData");
        assert_eq!(messages[3]["message"]["role"], "toolResult");
        assert_eq!(messages[3]["message"]["toolCallId"], "call-2");
        assert_eq!(messages[3]["message"]["content"], "second result");
        assert_eq!(messages[3]["message"]["isError"], true);
        assert_eq!(messages[4]["message"]["role"], "assistant");
        assert!(
            records
                .iter()
                .filter(|record| record.get("type").and_then(Value::as_str) == Some("message"))
                .all(|record| {
                    record["message"]["role"].as_str() == Some("toolResult")
                        || record["message"]["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .all(|block| {
                                block.get("type").and_then(Value::as_str) != Some("tool_result")
                            })
                })
        );

        let pi = emit_with_defaults(
            &session,
            TargetTool::Pi,
            &EmitContext::new(home).with_session_id("pi-tool-result"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let pi_messages: Vec<Value> = read_jsonl(&pi.path)
            .into_iter()
            .filter(|record| record.get("type").and_then(Value::as_str) == Some("message"))
            .collect();
        assert_eq!(pi_messages[0]["message"]["role"], "user");
        assert!(
            pi_messages[0]["message"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |block| block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && block.get("toolCallId").and_then(Value::as_str) == Some("call-1")
                )
        );
    }

    #[test]
    fn rpi_omits_empty_user_after_splitting_only_tool_results() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.messages = vec![Message::from_parts(
            Role::User,
            vec![ContentPart::ToolResult {
                tool_use_id: "call-1".to_owned(),
                content: "fn main() {}".to_owned(),
                is_error: false,
            }],
            Some("2026-07-30T10:11:13.000Z".to_owned()),
        )];
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Rpi,
            &EmitContext::new(home).with_session_id("rpi-only-tool"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let messages: Vec<Value> = read_jsonl(&emitted.path)
            .into_iter()
            .filter(|record| record.get("type").and_then(Value::as_str) == Some("message"))
            .collect();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["message"]["role"], "toolResult");
        assert_eq!(messages[0]["message"]["toolCallId"], "call-1");
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
        // Record 0 is the native leading title slot; record 1 is the session
        // header; record 2 is the model-change bootstrap.
        assert_eq!(omp_records[0]["type"], "title");
        assert_eq!(omp_records[0]["v"], 1);
        assert!(omp_records[1].get("convertedFrom").is_none());
        assert_eq!(omp_records[2]["model"], "provider/model");
        assert_eq!(omp_records[3]["thinkingLevel"], "xhigh");
        assert_eq!(omp_records[5]["message"]["provider"], "provider");
        assert_eq!(omp_records[5]["message"]["model"], "model");
    }

    #[test]
    fn pi_emits_native_compaction_for_compaction_notes() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.messages.insert(
            0,
            Message::from_parts(
                Role::Assistant,
                vec![ContentPart::Note {
                    kind: "compaction".to_owned(),
                    text: "prior context".to_owned(),
                }],
                None,
            ),
        );
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Pi,
            &EmitContext::new(home).with_session_id("pi-compact"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        assert!(records.iter().any(|record| {
            record.get("type").and_then(Value::as_str) == Some("compaction")
                && record.get("summary").and_then(Value::as_str) == Some("prior context")
        }));
        assert!(!records.iter().any(|record| {
            record.get("type").and_then(Value::as_str) == Some("custom_message")
                && record.get("customType").and_then(Value::as_str) == Some("compaction")
        }));
    }

    #[test]
    fn pi_emits_native_branch_summary_for_branch_notes() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.messages.push(Message::from_parts(
            Role::Assistant,
            vec![ContentPart::Note {
                kind: "branch_summary".to_owned(),
                text: "other branch work".to_owned(),
            }],
            None,
        ));
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Pi,
            &EmitContext::new(home).with_session_id("pi-branch"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        assert!(records.iter().any(|record| {
            record.get("type").and_then(Value::as_str) == Some("branch_summary")
                && record.get("summary").and_then(Value::as_str) == Some("other branch work")
        }));
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
        assert!(records[0]["payload"].get("session_id").is_none());
        assert!(records[0]["payload"].get("thread_source").is_none());
        assert!(records[0]["payload"].get("history_mode").is_none());
        assert!(records[1]["payload"].get("workspace_roots").is_none());
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
    fn codex_and_grok_group_user_text_with_images() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.messages[0] = Message::from_parts(
            Role::User,
            vec![
                ContentPart::Text("hello".to_owned()),
                ContentPart::Image {
                    mime_type: Some("image/png".to_owned()),
                    data: "iVBORw0KGgo=".to_owned(),
                },
            ],
            Some("2026-07-30T10:11:13.000Z".to_owned()),
        );

        let codex = emit_with_defaults(
            &session,
            TargetTool::Codex,
            &EmitContext::new(home)
                .with_session_id("codex-image")
                .with_codex_runtime(CodexRuntime {
                    provider: "provider".to_owned(),
                    model: "model".to_owned(),
                }),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&codex.path);
        let user = records
            .iter()
            .find(|record| {
                record.get("type").and_then(Value::as_str) == Some("response_item")
                    && record["payload"].get("role").and_then(Value::as_str) == Some("user")
            })
            .expect("user response");
        let content = user["payload"]["content"].as_array().expect("content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(
            records
                .iter()
                .filter(
                    |record| record["payload"].get("type").and_then(Value::as_str)
                        == Some("user_message")
                )
                .count(),
            1
        );

        let grok = emit_with_defaults(
            &session,
            TargetTool::Grok,
            &EmitContext::new(home).with_session_id("grok-image"),
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let chat: Vec<Value> = fs::read_to_string(grok.path.with_file_name("chat_history.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let users: Vec<&Value> = chat
            .iter()
            .filter(|record| record.get("type").and_then(Value::as_str) == Some("user"))
            .collect();
        assert_eq!(users.len(), 1);
        let content = users[0]["content"].as_array().expect("content");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
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
    fn grok_and_hyper_emit_shared_native_summary() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let session = fixture(home);
        for (target, id) in [
            (TargetTool::Grok, "grok-session"),
            (TargetTool::Hyper, "hyper-session"),
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
            let summary: Value =
                serde_json::from_str(&fs::read_to_string(&emitted.path).unwrap()).unwrap();
            assert!(summary.get("agent_name").is_none());
            assert_eq!(summary["current_model_id"], "grok-runtime-model");
            let chat = read_jsonl(&emitted.path.with_file_name("chat_history.jsonl"));
            assert!(chat[0]["content"].is_array());
            assert!(chat[1]["content"].is_string());
            assert_eq!(chat[0]["prompt_index"], 0);
            let updates = read_jsonl(&emitted.path.with_file_name("updates.jsonl"));
            assert_eq!(updates[0]["params"]["update"]["_meta"]["promptIndex"], 0);
            assert!(
                updates[1]["params"]["update"]["_meta"]
                    .get("promptIndex")
                    .is_none()
            );
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
    fn long_grok_cwd_uses_slug_and_sidecar() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        let cwd = format!("/{}", "é".repeat(100));
        session.cwd = PathBuf::from(&cwd);
        let context = EmitContext::new(home).with_session_id("grok-session");
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Grok,
            &context,
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let cwd_directory = emitted.path.parent().unwrap().parent().unwrap();
        assert!(cwd_directory.file_name().unwrap().len() <= MAX_FILESYSTEM_COMPONENT_BYTES);
        assert_eq!(fs::read_to_string(cwd_directory.join(".cwd")).unwrap(), cwd);
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
    #[test]
    fn unsafe_session_ids_are_rejected_by_component_guard() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let long_id = "x".repeat(256);
        for (session_id, expected) in [
            ("a/b", "unsafe session id"),
            ("a\\b", "unsafe session id"),
            ("a\0b", "unsafe session id"),
            (".", "unsafe session id"),
            ("..", "unsafe session id"),
            ("", "unsafe session id"),
            (long_id.as_str(), "exceeding filesystem limit"),
        ] {
            let context = EmitContext::new(home).with_session_id(session_id);
            let error = emit_with_defaults(
                &fixture(home),
                TargetTool::Droid,
                &context,
                &mut FixedDefaults::new(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "session_id {session_id:?} should fail with {expected:?}, got: {error}"
            );
        }
    }

    #[test]
    fn valid_session_id_passes_the_component_guard() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let context = EmitContext::new(home).with_session_id("valid-session-id");
        let emitted = emit_with_defaults(
            &fixture(home),
            TargetTool::Droid,
            &context,
            &mut FixedDefaults::new(),
        )
        .unwrap();
        assert_eq!(emitted.session_id, "valid-session-id");
    }

    #[test]
    fn fallback_time_prefers_modified_epoch_over_context_and_clock() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.start_timestamp = None;
        session.modified_epoch = Some(0.0);
        let context = EmitContext::new(home)
            .with_session_id("epoch-session")
            .with_fallback_time(Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap());
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Pi,
            &context,
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        assert_eq!(records[0]["timestamp"], "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn fallback_time_context_wins_over_default_clock() {
        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.start_timestamp = None;
        session.modified_epoch = None;
        let fallback = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        let context = EmitContext::new(home)
            .with_session_id("fallback-session")
            .with_fallback_time(fallback);
        let emitted = emit_with_defaults(
            &session,
            TargetTool::Pi,
            &context,
            &mut FixedDefaults::new(),
        )
        .unwrap();
        let records = read_jsonl(&emitted.path);
        assert_eq!(records[0]["timestamp"], "2025-06-01T00:00:00.000Z");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_cwd_is_rejected_before_any_write() {
        use std::os::unix::ffi::OsStringExt;

        let temporary = TempDir::new().unwrap();
        let home = temporary.path();
        let mut session = fixture(home);
        session.cwd = PathBuf::from(std::ffi::OsString::from_vec(vec![0x62, 0xff, 0x63]));
        let error = emit_with_defaults(
            &session,
            TargetTool::Droid,
            &EmitContext::new(home).with_session_id("non-utf8-cwd"),
            &mut FixedDefaults::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"));
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
