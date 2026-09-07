use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceTool {
    Pi,
    Rpi,
    Omp,
    Droid,
    Codex,
    Claude,
    Grok,
    Agent,
}

impl SourceTool {
    pub const ALL: [Self; 8] = [
        Self::Pi,
        Self::Rpi,
        Self::Omp,
        Self::Droid,
        Self::Codex,
        Self::Claude,
        Self::Grok,
        Self::Agent,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Rpi => "rpi",
            Self::Omp => "omp",
            Self::Droid => "droid",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok",
            Self::Agent => "agent",
        }
    }

    /// Pi and Rpi use the same JSONL tree on disk, in different catalog roots.
    pub const fn uses_pi_jsonl(self) -> bool {
        matches!(self, Self::Pi | Self::Rpi)
    }
}

impl fmt::Display for SourceTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SourceTool {
    type Err = ToolParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pi" => Ok(Self::Pi),
            "rpi" => Ok(Self::Rpi),
            "omp" => Ok(Self::Omp),
            "droid" => Ok(Self::Droid),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "grok" => Ok(Self::Grok),
            "agent" => Ok(Self::Agent),
            _ => Err(ToolParseError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetTool {
    Pi,
    Omp,
    Droid,
    Codex,
    Claude,
    Grok,
    Hyper,
    Rpi,
    Agent,
}

impl TargetTool {
    pub const ALL: [Self; 9] = [
        Self::Pi,
        Self::Omp,
        Self::Droid,
        Self::Codex,
        Self::Claude,
        Self::Grok,
        Self::Hyper,
        Self::Rpi,
        Self::Agent,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Droid => "droid",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok",
            Self::Hyper => "hyper",
            Self::Rpi => "rpi",
            Self::Agent => "agent",
        }
    }

    pub const fn source(self) -> Option<SourceTool> {
        match self {
            Self::Pi => Some(SourceTool::Pi),
            Self::Omp => Some(SourceTool::Omp),
            Self::Droid => Some(SourceTool::Droid),
            Self::Codex => Some(SourceTool::Codex),
            Self::Claude => Some(SourceTool::Claude),
            Self::Grok => Some(SourceTool::Grok),
            Self::Agent => Some(SourceTool::Agent),
            Self::Hyper => None,
            Self::Rpi => Some(SourceTool::Rpi),
        }
    }

    pub const fn uses_grok_storage(self) -> bool {
        matches!(self, Self::Grok | Self::Hyper)
    }

    pub const fn uses_pi_storage(self) -> bool {
        matches!(self, Self::Pi | Self::Rpi)
    }
}

impl fmt::Display for TargetTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for TargetTool {
    type Err = ToolParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pi" => Ok(Self::Pi),
            "omp" => Ok(Self::Omp),
            "droid" => Ok(Self::Droid),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "grok" => Ok(Self::Grok),
            "hyper" => Ok(Self::Hyper),
            "rpi" => Ok(Self::Rpi),
            "agent" => Ok(Self::Agent),
            _ => Err(ToolParseError(value.to_owned())),
        }
    }
}

#[derive(Debug, Error)]
#[error("unsupported tool: {0}")]
pub struct ToolParseError(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    pub const fn is_conversation(self) -> bool {
        matches!(self, Self::User | Self::Assistant)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = RoleParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" | "toolResult" | "tool_result" => Ok(Self::Tool),
            _ => Err(RoleParseError(value.to_owned())),
        }
    }
}

#[derive(Debug, Error)]
#[error("unsupported message role: {0}")]
pub struct RoleParseError(String);

#[derive(Debug, Error)]
#[error("unsupported thinking level: {0}")]
pub struct ThinkingLevelParseError(String);

/// One portable content block shared across source and target formats.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    Image {
        mime_type: Option<String>,
        data: String,
    },
    Note {
        kind: String,
        text: String,
    },
}

impl ContentPart {
    pub fn searchable_text(&self) -> Option<&str> {
        match self {
            Self::Text(text)
            | Self::Thinking { text, .. }
            | Self::ToolResult { content: text, .. }
            | Self::Note { text, .. } => (!text.is_empty()).then_some(text.as_str()),
            Self::ToolUse { name, .. } => (!name.is_empty()).then_some(name.as_str()),
            Self::Image { .. } => Some("[Image]"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub timestamp: Option<String>,
    pub parts: Vec<ContentPart>,
}

impl Message {
    pub fn plain(role: Role, text: impl Into<String>, timestamp: Option<String>) -> Self {
        let text = text.into();
        let parts = if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::Text(text.clone())]
        };
        Self {
            role,
            text,
            timestamp,
            parts,
        }
    }

    pub fn from_parts(role: Role, parts: Vec<ContentPart>, timestamp: Option<String>) -> Self {
        let text = joined_text(&parts);
        Self {
            role,
            text,
            timestamp,
            parts,
        }
    }

    pub fn effective_parts(&self) -> Vec<ContentPart> {
        if self.parts.is_empty() && !self.text.is_empty() {
            vec![ContentPart::Text(self.text.clone())]
        } else {
            self.parts.clone()
        }
    }
}

pub fn joined_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(ContentPart::searchable_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionHints {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub compaction_summary: Option<String>,
    pub session_kind: Option<String>,
    pub hidden: bool,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub tool: SourceTool,
    pub session_id: String,
    pub cwd: PathBuf,
    pub start_timestamp: Option<String>,
    pub summary: String,
    pub messages: Vec<Message>,
    pub path: PathBuf,
    pub modified_epoch: Option<f64>,
    pub hints: SessionHints,
}

impl Session {
    /// Workflow/subagent children that should stay out of the default catalog.
    ///
    /// Absent `sessionKind` is a user session. `parentSession` alone is a user
    /// fork or rotation and stays visible. Grok `hidden: true` is also hidden.
    pub fn is_catalog_child(&self) -> bool {
        self.hints.hidden || is_internal_session_kind(self.hints.session_kind.as_deref())
    }
}

/// Native workflow/subagent kinds. Missing or `user` is a first-class session.
pub fn is_internal_session_kind(kind: Option<&str>) -> bool {
    let Some(kind) = kind.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    !kind.eq_ignore_ascii_case("user")
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub modified_epoch: f64,
    pub tool: SourceTool,
    pub display_time: String,
    pub session_id: String,
    pub summary: String,
    pub path: PathBuf,
    pub size: u64,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FlexibleRecord<T> {
    Known(T),
    Unknown {
        type_tag: Option<String>,
        raw: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Extensions {
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Auto,
}

impl ThinkingLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Auto => "auto",
        }
    }
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ThinkingLevel {
    type Err = ThinkingLevelParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_', ' '], "");
        match normalized.as_str() {
            "off" => Ok(Self::Off),
            "minimal" | "min" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" | "med" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "extrahigh" => Ok(Self::XHigh),
            "max" | "maximum" => Ok(Self::Max),
            "auto" => Ok(Self::Auto),
            _ => Err(ThinkingLevelParseError(value.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_tool_from_str_accepts_all_spellings() {
        for tool in SourceTool::ALL {
            let parsed: SourceTool = tool.as_str().parse().expect("valid spelling");
            assert_eq!(parsed, tool);
        }
        let error = "hyper".parse::<SourceTool>().expect_err("invalid tool");
        assert_eq!(error.to_string(), "unsupported tool: hyper");
    }

    #[test]
    fn target_tool_from_str_accepts_all_spellings() {
        for tool in TargetTool::ALL {
            let parsed: TargetTool = tool.as_str().parse().expect("valid spelling");
            assert_eq!(parsed, tool);
        }
        let error = "hyperx".parse::<TargetTool>().expect_err("invalid tool");
        assert_eq!(error.to_string(), "unsupported tool: hyperx");
    }

    #[test]
    fn role_from_str_accepts_user_assistant_and_tool() {
        assert_eq!("user".parse::<Role>().unwrap(), Role::User);
        assert_eq!("assistant".parse::<Role>().unwrap(), Role::Assistant);
        assert_eq!("tool".parse::<Role>().unwrap(), Role::Tool);
        assert_eq!("toolResult".parse::<Role>().unwrap(), Role::Tool);
        assert_eq!("tool_result".parse::<Role>().unwrap(), Role::Tool);
        let error = "system".parse::<Role>().expect_err("invalid role");
        assert_eq!(error.to_string(), "unsupported message role: system");
    }

    #[test]
    fn message_from_parts_joins_searchable_text() {
        let message = Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::Thinking {
                    text: "plan".to_owned(),
                    signature: None,
                },
                ContentPart::Text("answer".to_owned()),
                ContentPart::ToolUse {
                    id: "c1".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"path": "a.rs"}),
                },
            ],
            None,
        );
        assert_eq!(message.text, "plan\nanswer\nread");
        assert_eq!(message.effective_parts().len(), 3);
    }

    #[test]
    fn thinking_level_from_str_accepts_all_levels() {
        let levels = [
            ("off", ThinkingLevel::Off),
            ("minimal", ThinkingLevel::Minimal),
            ("low", ThinkingLevel::Low),
            ("medium", ThinkingLevel::Medium),
            ("high", ThinkingLevel::High),
            ("xhigh", ThinkingLevel::XHigh),
            ("max", ThinkingLevel::Max),
            ("auto", ThinkingLevel::Auto),
        ];
        for (spelling, expected) in levels {
            assert_eq!(spelling.parse::<ThinkingLevel>().unwrap(), expected);
        }
        assert_eq!(
            "High".parse::<ThinkingLevel>().unwrap(),
            ThinkingLevel::High
        );
        assert_eq!(
            "x-high".parse::<ThinkingLevel>().unwrap(),
            ThinkingLevel::XHigh
        );
        assert_eq!(
            "extra_high".parse::<ThinkingLevel>().unwrap(),
            ThinkingLevel::XHigh
        );
        let error = "turbo".parse::<ThinkingLevel>().expect_err("invalid level");
        assert_eq!(error.to_string(), "unsupported thinking level: turbo");
    }

    #[test]
    fn target_tool_source_maps_to_origin_tool() {
        assert_eq!(TargetTool::Pi.source(), Some(SourceTool::Pi));
        assert_eq!(TargetTool::Omp.source(), Some(SourceTool::Omp));
        assert_eq!(TargetTool::Droid.source(), Some(SourceTool::Droid));
        assert_eq!(TargetTool::Codex.source(), Some(SourceTool::Codex));
        assert_eq!(TargetTool::Claude.source(), Some(SourceTool::Claude));
        assert_eq!(TargetTool::Grok.source(), Some(SourceTool::Grok));
        assert_eq!(TargetTool::Agent.source(), Some(SourceTool::Agent));
        assert_eq!(TargetTool::Hyper.source(), None);
        assert_eq!(TargetTool::Rpi.source(), Some(SourceTool::Rpi));
    }

    #[test]
    fn target_tool_uses_grok_storage_only_for_grok_and_hyper() {
        for tool in TargetTool::ALL {
            assert_eq!(
                tool.uses_grok_storage(),
                matches!(tool, TargetTool::Grok | TargetTool::Hyper),
                "{tool}"
            );
        }
    }

    #[test]
    fn target_tool_uses_pi_storage_only_for_pi_and_rpi() {
        for tool in TargetTool::ALL {
            assert_eq!(
                tool.uses_pi_storage(),
                matches!(tool, TargetTool::Pi | TargetTool::Rpi),
                "{tool}"
            );
        }
    }

    #[test]
    fn flexible_record_unknown_raw_round_trips_with_extra_fields() {
        let raw: Value =
            serde_json::from_str(r#"{"type":"mystery","id":"x","extra":{"n":7},"flag":true}"#)
                .unwrap();
        let record: FlexibleRecord<u8> = FlexibleRecord::Unknown {
            type_tag: Some("mystery".to_owned()),
            raw: raw.clone(),
        };
        let FlexibleRecord::Unknown {
            type_tag,
            raw: record_raw,
        } = &record
        else {
            panic!("expected Unknown");
        };
        assert_eq!(type_tag.as_deref(), Some("mystery"));
        assert_eq!(record_raw["extra"]["n"], 7);
        let serialized = serde_json::to_string(record_raw).unwrap();
        let round_tripped: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(round_tripped, raw);
        // A rebuilt record compares equal, so the extra fields survive the trip.
        let rebuilt: FlexibleRecord<u8> = FlexibleRecord::Unknown {
            type_tag: Some("mystery".to_owned()),
            raw: round_tripped,
        };
        assert_eq!(record, rebuilt);
    }

    #[test]
    fn catalog_child_hides_internal_kinds_but_not_user_forks() {
        let mut session = Session {
            tool: SourceTool::Pi,
            session_id: "s".to_owned(),
            cwd: PathBuf::from("/workspace/project"),
            start_timestamp: None,
            summary: String::new(),
            messages: Vec::new(),
            path: PathBuf::from("/tmp/s.jsonl"),
            modified_epoch: None,
            hints: SessionHints::default(),
        };
        assert!(!session.is_catalog_child());
        session.hints.session_kind = Some("user".to_owned());
        assert!(!session.is_catalog_child());
        session.hints.session_kind = Some("workflowWorker".to_owned());
        assert!(session.is_catalog_child());
        session.hints.session_kind = Some("subagent_resume".to_owned());
        assert!(session.is_catalog_child());
        session.hints.session_kind = Some(String::new());
        assert!(!session.is_catalog_child());
        session.hints.session_kind = Some("  ".to_owned());
        assert!(!session.is_catalog_child());
        session.hints.session_kind = Some("disposable".to_owned());
        assert!(session.is_catalog_child());
        session.hints.session_kind = None;
        session.hints.hidden = true;
        assert!(session.is_catalog_child());
    }

    #[test]
    fn extensions_flatten_round_trips_extra_fields() {
        let mut extensions = Extensions::default();
        extensions
            .fields
            .insert("future_field".to_owned(), json!({"nested": [1, 2]}));
        extensions.fields.insert("count".to_owned(), json!(3));
        let value = serde_json::to_value(&extensions).unwrap();
        assert_eq!(value["count"], 3);
        assert_eq!(value["future_field"]["nested"][1], 2);
        let reparsed: Extensions = serde_json::from_value(value).unwrap();
        assert_eq!(reparsed, extensions);
    }
}
