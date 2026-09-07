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

use crate::domain::{ContentPart, Message, Role};

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
            if matches!(
                item_type,
                Some("text" | "input_text" | "output_text" | "summary_text")
            ) && text.is_some_and(|value| !value.is_empty())
            {
                return text;
            }
            object.get("content").and_then(first_text_from_content)
        }),
        _ => None,
    }
}

pub fn content_parts(content: &Value) -> Vec<ContentPart> {
    match content {
        Value::String(text) if !text.is_empty() => vec![ContentPart::Text(text.clone())],
        Value::Array(items) => items.iter().flat_map(part_from_value).collect(),
        Value::Object(_) => part_from_value(content),
        _ => Vec::new(),
    }
}

fn part_from_value(value: &Value) -> Vec<ContentPart> {
    let Some(object) = value.as_object() else {
        return match first_text_from_content(value) {
            Some(text) => vec![ContentPart::Text(text.to_owned())],
            None => Vec::new(),
        };
    };
    match object.get("type").and_then(Value::as_str) {
        Some("text" | "input_text" | "output_text" | "summary_text") => object
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| vec![ContentPart::Text(text.to_owned())])
            .unwrap_or_default(),
        Some("thinking" | "reasoning" | "agent_thought") => thinking_part(object)
            .map(|part| vec![part])
            .unwrap_or_default(),
        Some("tool_use" | "toolCall" | "tool_call" | "function_call") => {
            vec![ContentPart::ToolUse {
                id: string_field(object, &["id", "toolCallId", "tool_call_id", "call_id"]),
                name: string_field(object, &["name", "toolName", "tool_name"]),
                input: object_or_json(
                    object
                        .get("input")
                        .or_else(|| object.get("arguments"))
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                ),
            }]
        }
        Some("tool_result" | "toolResult" | "function_call_output") => {
            let content = object.get("content").or_else(|| object.get("output"));
            let mut parts = vec![ContentPart::ToolResult {
                tool_use_id: string_field(
                    object,
                    &["tool_use_id", "toolUseId", "toolCallId", "call_id", "id"],
                ),
                content: tool_result_text(content),
                is_error: object
                    .get("is_error")
                    .or_else(|| object.get("isError"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }];
            parts.extend(nested_images(content));
            parts
        }
        Some("image" | "input_image" | "output_image" | "image_url") => image_from_object(object)
            .map(|part| vec![part])
            .unwrap_or_default(),
        Some("note") => note_part(object, "note")
            .map(|part| vec![part])
            .unwrap_or_default(),
        Some("compaction") => object
            .get("summary")
            .or_else(|| object.get("shortSummary"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| {
                vec![ContentPart::Note {
                    kind: "compaction".to_owned(),
                    text: text.to_owned(),
                }]
            })
            .unwrap_or_default(),
        _ => object.get("content").map(content_parts).unwrap_or_default(),
    }
}

pub(crate) fn image_from_object(object: &serde_json::Map<String, Value>) -> Option<ContentPart> {
    let source = object.get("source").and_then(Value::as_object);
    let mime_type = [
        object.get("mimeType"),
        object.get("mime_type"),
        object.get("media_type"),
        object.get("mediaType"),
        source.and_then(|source| source.get("media_type")),
        source.and_then(|source| source.get("mediaType")),
        source.and_then(|source| source.get("mimeType")),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .map(str::to_owned);
    let data = [
        object.get("data"),
        object.get("url"),
        object.get("image_url"),
        source.and_then(|source| source.get("data")),
        source.and_then(|source| source.get("url")),
        source.and_then(|source| source.get("image_url")),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Object(nested) => nested
            .get("url")
            .or_else(|| nested.get("data"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        _ => None,
    })?;
    Some(ContentPart::Image { mime_type, data })
}

fn note_part(object: &serde_json::Map<String, Value>, default_kind: &str) -> Option<ContentPart> {
    let text = ["text", "content", "summary"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())?
        .to_owned();
    let kind = ["kind", "customType", "type"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(Value::as_str))
        .filter(|value| !value.is_empty() && *value != "note")
        .unwrap_or(default_kind)
        .to_owned();
    Some(ContentPart::Note { kind, text })
}

pub fn hints_from_tree_records(
    records: &[Value],
    path_ids: &[&str],
) -> crate::domain::SessionHints {
    use crate::domain::{SessionHints, ThinkingLevel};

    let ids: std::collections::HashSet<&str> = path_ids.iter().copied().collect();
    let mut hints = SessionHints::default();
    for record in records {
        let Some(object) = record.as_object() else {
            continue;
        };
        let id = object.get("id").and_then(Value::as_str).unwrap_or("");
        if !id.is_empty() && !ids.is_empty() && !ids.contains(id) {
            continue;
        }
        match object.get("type").and_then(Value::as_str) {
            Some("model_change") => {
                let model = object
                    .get("modelId")
                    .or_else(|| object.get("model"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty());
                if let Some(model) = model {
                    if let Some((provider, model_id)) = model.split_once('/') {
                        if hints.provider.is_none() && !provider.is_empty() {
                            hints.provider = Some(provider.to_owned());
                        }
                        hints.model = Some(model_id.to_owned());
                    } else {
                        hints.model = Some(model.to_owned());
                    }
                }
                if let Some(provider) = object
                    .get("provider")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    hints.provider = Some(provider.to_owned());
                }
            }
            Some("thinking_level_change") => {
                hints.thinking_level = object
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<ThinkingLevel>().ok());
            }
            Some("compaction") => {
                hints.compaction_summary = object
                    .get("summary")
                    .or_else(|| object.get("shortSummary"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
            }
            _ => {}
        }
    }
    hints
}

pub fn compaction_note(summary: Option<&str>) -> Option<Message> {
    let text = summary.filter(|value| !value.is_empty())?;
    Some(Message::from_parts(
        Role::Assistant,
        vec![ContentPart::Note {
            kind: "compaction".to_owned(),
            text: text.to_owned(),
        }],
        None,
    ))
}

fn thinking_part(object: &serde_json::Map<String, Value>) -> Option<ContentPart> {
    let text = ["thinking", "text", "summary"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())?;
    Some(ContentPart::Thinking {
        text: text.to_owned(),
        signature: object
            .get("signature")
            .or_else(|| object.get("thinkingSignature"))
            .or_else(|| object.get("thinking_signature"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Native Codex/Grok often store tool arguments as a JSON string. Object and
/// array payloads become typed values; anything else is left unchanged.
pub(crate) fn object_or_json(value: Value) -> Value {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return serde_json::json!({});
            }
            serde_json::from_str::<Value>(trimmed)
                .ok()
                .filter(|parsed| parsed.is_object() || parsed.is_array())
                .unwrap_or(Value::String(text))
        }
        other => other,
    }
}

fn nested_images(content: Option<&Value>) -> Vec<ContentPart> {
    match content {
        Some(Value::Array(items)) => items
            .iter()
            .flat_map(part_from_value)
            .filter(|part| matches!(part, ContentPart::Image { .. }))
            .collect(),
        Some(Value::Object(object)) => image_from_object(object).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn string_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => content_parts(value)
            .into_iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

pub fn parsed_message(
    role: Option<&str>,
    content: Option<&Value>,
    timestamp: Option<&str>,
) -> Option<Message> {
    let role = role?.parse::<Role>().ok()?;
    let parts = content_parts(content?);
    if parts.is_empty() {
        return None;
    }
    Some(Message::from_parts(
        role,
        parts,
        timestamp.map(str::to_owned),
    ))
}

pub fn normalize(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        return normalized;
    }
    let kept = normalized
        .chars()
        .take(limit.saturating_sub(3))
        .collect::<String>();
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
        Message::plain(role, text, None)
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
        assert_eq!(message.parts, vec![ContentPart::Text("hi".to_owned())]);
    }

    #[test]
    fn content_parts_keeps_thinking_tools_and_text() {
        let content = json!([
            {"type": "thinking", "thinking": "plan", "thinkingSignature": "sig"},
            {"type": "text", "text": "hello"},
            {"type": "toolCall", "id": "c1", "name": "read", "arguments": {"path": "a.rs"}},
            {"type": "tool_use", "id": "c2", "name": "Read", "arguments": "{\"path\":\"b.rs\"}"},
            {"type": "tool_result", "tool_use_id": "c1", "content": [
                {"type": "text", "text": "ok"},
                {"type": "image", "source": {"media_type": "image/png", "data": "nested"}}
            ], "is_error": false},
            {"type": "image", "source": {"media_type": "image/png", "data": "abc"}},
            {"type": "note", "kind": "compaction", "text": "prior"}
        ]);
        let parts = content_parts(&content);
        assert_eq!(
            parts,
            vec![
                ContentPart::Thinking {
                    text: "plan".to_owned(),
                    signature: Some("sig".to_owned()),
                },
                ContentPart::Text("hello".to_owned()),
                ContentPart::ToolUse {
                    id: "c1".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"path": "a.rs"}),
                },
                ContentPart::ToolUse {
                    id: "c2".to_owned(),
                    name: "Read".to_owned(),
                    input: json!({"path": "b.rs"}),
                },
                ContentPart::ToolResult {
                    tool_use_id: "c1".to_owned(),
                    content: "ok".to_owned(),
                    is_error: false,
                },
                ContentPart::Image {
                    mime_type: Some("image/png".to_owned()),
                    data: "nested".to_owned(),
                },
                ContentPart::Image {
                    mime_type: Some("image/png".to_owned()),
                    data: "abc".to_owned(),
                },
                ContentPart::Note {
                    kind: "compaction".to_owned(),
                    text: "prior".to_owned(),
                },
            ]
        );
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
