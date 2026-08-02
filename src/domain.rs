use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SourceTool {
    Pi,
    Omp,
    Droid,
    Codex,
    Claude,
    Grok,
}

impl SourceTool {
    pub const ALL: [Self; 6] = [
        Self::Pi,
        Self::Omp,
        Self::Droid,
        Self::Codex,
        Self::Claude,
        Self::Grok,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Droid => "droid",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok",
        }
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
            "omp" => Ok(Self::Omp),
            "droid" => Ok(Self::Droid),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "grok" => Ok(Self::Grok),
            _ => Err(ToolParseError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
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
}

impl TargetTool {
    pub const ALL: [Self; 8] = [
        Self::Pi,
        Self::Omp,
        Self::Droid,
        Self::Codex,
        Self::Claude,
        Self::Grok,
        Self::Hyper,
        Self::Rpi,
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
            Self::Hyper => None,
            Self::Rpi => None,
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
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
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
            _ => Err(RoleParseError(value.to_owned())),
        }
    }
}

#[derive(Debug, Error)]
#[error("unsupported message role: {0}")]
pub struct RoleParseError(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
    pub timestamp: Option<String>,
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
    Unknown { type_tag: Option<String>, raw: Value },
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
        match value {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            "auto" => Ok(Self::Auto),
            _ => Err(ThinkingLevelParseError(value.to_owned())),
        }
    }
}

#[derive(Debug, Error)]
#[error("unsupported thinking level: {0}")]
pub struct ThinkingLevelParseError(String);
