pub mod agent;
pub mod claude;
pub mod codex;
pub mod droid;
pub mod grok;
pub mod omp;
pub mod pi;
pub mod tree;

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::domain::{Message, Role};

pub fn read_jsonl_values(path: &Path) -> Result<Vec<Value>> {
    let file = File::open(path).with_context(|| format!("opening session {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if record.is_object() {
            records.push(record);
        }
    }
    Ok(records)
}

pub fn first_text_from_content(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) if !text.is_empty() => Some(text),
        Value::Array(items) => items.iter().find_map(|item| {
            let object = item.as_object()?;
            let item_type = object.get("type").and_then(Value::as_str);
            let text = object.get("text").and_then(Value::as_str);
            if matches!(item_type, Some("text" | "input_text" | "output_text"))
                && text.is_some_and(|value| !value.is_empty())
            {
                return text;
            }
            object.get("content").and_then(first_text_from_content)
        }),
        _ => None,
    }
}

pub fn parsed_message(
    role: Option<&str>,
    content: Option<&Value>,
    timestamp: Option<&str>,
) -> Option<Message> {
    let role = role?.parse::<Role>().ok()?;
    let text = first_text_from_content(content?)?;
    Some(Message {
        role,
        text: text.to_owned(),
        timestamp: timestamp.map(str::to_owned),
    })
}

pub fn normalize(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        return normalized;
    }
    let kept = normalized.chars().take(limit.saturating_sub(3)).collect::<String>();
    format!("{kept}...")
}

pub fn summarize_messages(messages: &[Message]) -> String {
    if let Some(message) = messages
        .iter()
        .find(|message| message.role == Role::User && !message.text.trim_start().starts_with('<'))
    {
        return normalize(&message.text, 100);
    }
    messages
        .iter()
        .find(|message| !message.text.is_empty())
        .map(|message| normalize(&message.text, 100))
        .unwrap_or_else(|| "(no summary)".to_owned())
}
