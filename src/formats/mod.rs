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
    let first_user = messages.iter().find(|message| {
        message.role == Role::User && !message.text.is_empty() && !message.text.starts_with('<')
    });
    let first_nonempty = messages.iter().find(|message| !message.text.is_empty());
    first_user
        .or(first_nonempty)
        .map(|message| message.text.clone())
        .unwrap_or_else(|| "(no summary)".to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;
    use tempfile::NamedTempFile;

    use super::*;

    fn write_lines(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("temp file");
        for line in lines {
            writeln!(file, "{line}").expect("write line");
        }
        file.flush().expect("flush");
        file
    }

    fn message(role: Role, text: &str) -> Message {
        Message {
            role,
            text: text.to_owned(),
            timestamp: None,
        }
    }

    #[test]
    fn read_jsonl_values_parses_valid_records() {
        let file = write_lines(&[r#"{"id":1}"#, r#"{"id":2}"#]);
        let records = read_jsonl_values(file.path()).expect("read");
        assert_eq!(records, vec![json!({"id": 1}), json!({"id": 2})]);
    }

    #[test]
    fn read_jsonl_values_skips_malformed_lines() {
        let file = write_lines(&[r#"{"id":1}"#, "not json", r#"{"id":3}"#]);
        let records = read_jsonl_values(file.path()).expect("read");
        assert_eq!(records, vec![json!({"id": 1}), json!({"id": 3})]);
    }

    #[test]
    fn read_jsonl_values_empty_file_yields_empty() {
        let file = NamedTempFile::new().expect("temp file");
        let records = read_jsonl_values(file.path()).expect("read");
        assert!(records.is_empty());
    }

    #[test]
    fn read_jsonl_values_skips_non_utf8_line() {
        let mut file = NamedTempFile::new().expect("temp file");
        file.write_all(&[0xFF, 0xFE, b'\n']).expect("write bytes");
        writeln!(file, r#"{{"id":1}}"#).expect("write line");
        file.flush().expect("flush");
        let records = read_jsonl_values(file.path()).expect("read");
        assert_eq!(records, vec![json!({"id": 1})]);
    }

    #[test]
    fn first_text_from_content_plain_string() {
        let content = json!("hello");
        assert_eq!(first_text_from_content(&content), Some("hello"));
    }

    #[test]
    fn first_text_from_content_text_block_in_array() {
        let content = json!([{"type": "text", "text": "hello"}]);
        assert_eq!(first_text_from_content(&content), Some("hello"));
    }

    #[test]
    fn first_text_from_content_recurses_into_nested_content() {
        let content = json!([{"content": [{"type": "text", "text": "nested"}]}]);
        assert_eq!(first_text_from_content(&content), Some("nested"));
    }

    #[test]
    fn first_text_from_content_empty_array_is_none() {
        let content = json!([]);
        assert_eq!(first_text_from_content(&content), None);
    }

    #[test]
    fn first_text_from_content_no_text_is_none() {
        let content = json!([{"type": "image", "url": "x.png"}]);
        assert_eq!(first_text_from_content(&content), None);
        let empty = json!("");
        assert_eq!(first_text_from_content(&empty), None);
    }

    #[test]
    fn parsed_message_builds_message_from_valid_record() {
        let content = json!("hi");
        let message = parsed_message(Some("user"), Some(&content), Some("t1")).expect("message");
        assert_eq!(message.role, Role::User);
        assert_eq!(message.text, "hi");
        assert_eq!(message.timestamp.as_deref(), Some("t1"));
    }

    #[test]
    fn parsed_message_invalid_role_is_none() {
        let content = json!("hi");
        assert!(parsed_message(Some("admin"), Some(&content), None).is_none());
    }

    #[test]
    fn parsed_message_missing_content_is_none() {
        assert!(parsed_message(Some("user"), None, None).is_none());
    }

    #[test]
    fn normalize_truncates_ascii_at_limit_with_ellipsis() {
        assert_eq!(normalize("hello world", 8), "hello...");
    }

    #[test]
    fn normalize_counts_multibyte_chars_not_bytes() {
        // "héllo wörld" is 11 chars; a byte-based cut would split "é".
        assert_eq!(normalize("héllo wörld", 8), "héllo...");
    }

    #[test]
    fn normalize_empty_string_stays_empty() {
        assert_eq!(normalize("", 5), "");
    }

    #[test]
    fn normalize_at_exact_limit_is_unchanged() {
        assert_eq!(normalize("abc def", 7), "abc def");
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize("  a  \n\t b ", 10), "a b");
    }

    #[test]
    fn summarize_uses_first_user_text() {
        let messages = vec![
            message(Role::User, "first user message"),
            message(Role::Assistant, "assistant reply"),
        ];
        assert_eq!(summarize_messages(&messages), "first user message");
    }

    #[test]
    fn summarize_skips_angle_prefixed_user_text() {
        let messages = vec![
            message(Role::User, "<ignored prefix"),
            message(Role::User, "actual question"),
        ];
        assert_eq!(summarize_messages(&messages), "actual question");
    }

    #[test]
    fn summarize_empty_messages_yields_no_summary() {
        assert_eq!(summarize_messages(&[]), "(no summary)");
    }

    #[test]
    fn summarize_no_user_messages_falls_back_to_non_empty() {
        let messages = vec![message(Role::Assistant, "assistant-only text")];
        assert_eq!(summarize_messages(&messages), "assistant-only text");
    }

    #[test]
    fn summarize_no_user_messages_without_text_yields_no_summary() {
        let messages = vec![message(Role::Assistant, "")];
        assert_eq!(summarize_messages(&messages), "(no summary)");
    }
}
