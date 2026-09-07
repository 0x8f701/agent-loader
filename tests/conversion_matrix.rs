//! Source × target conversion matrix.
//!
//! Every convertible source is emitted to every target except Agent. The
//! portable conversation must keep user/assistant text, thinking, tool calls,
//! and tool results that the target format can represent.

use std::fs;
use std::path::Path;

use agent_loader::domain::{
    ContentPart, Message, Role, Session, SessionHints, SourceTool, TargetTool, ThinkingLevel,
};
use agent_loader::emit::{CodexRuntime, EmitContext, OmpRuntime, emit};
use agent_loader::sessions::Catalog;
use serde_json::json;
use tempfile::TempDir;

fn rich_session(cwd: &Path) -> Session {
    Session {
        tool: SourceTool::Claude,
        session_id: "source".to_owned(),
        cwd: cwd.to_path_buf(),
        start_timestamp: Some("2026-09-04T10:00:00.000Z".to_owned()),
        summary: "read the file".to_owned(),
        messages: vec![
            Message::from_parts(
                Role::Assistant,
                vec![ContentPart::Note {
                    kind: "compaction".to_owned(),
                    text: "prior context".to_owned(),
                }],
                Some("2026-09-04T10:00:00.500Z".to_owned()),
            ),
            Message::from_parts(
                Role::User,
                vec![
                    ContentPart::Text("list the file".to_owned()),
                    ContentPart::Image {
                        mime_type: Some("image/png".to_owned()),
                        data: "iVBORw0KGgo=".to_owned(),
                    },
                ],
                Some("2026-09-04T10:00:01.000Z".to_owned()),
            ),
            Message::from_parts(
                Role::Assistant,
                vec![
                    ContentPart::Thinking {
                        text: "I should read it".to_owned(),
                        signature: None,
                    },
                    ContentPart::Text("reading now".to_owned()),
                    ContentPart::ToolUse {
                        id: "call-1".to_owned(),
                        name: "Read".to_owned(),
                        input: json!({"path": "src/lib.rs"}),
                    },
                ],
                Some("2026-09-04T10:00:02.000Z".to_owned()),
            ),
            Message::from_parts(
                Role::Tool,
                vec![ContentPart::ToolResult {
                    tool_use_id: "call-1".to_owned(),
                    content: "fn main() {}".to_owned(),
                    is_error: false,
                }],
                Some("2026-09-04T10:00:03.000Z".to_owned()),
            ),
            Message::plain(
                Role::Assistant,
                "the file starts with fn main",
                Some("2026-09-04T10:00:04.000Z".to_owned()),
            ),
        ],
        path: cwd.join("source.jsonl"),
        modified_epoch: None,
        hints: SessionHints {
            model: Some("source-model".to_owned()),
            provider: Some("source-provider".to_owned()),
            thinking_level: Some(ThinkingLevel::High),
            compaction_summary: Some("prior context".to_owned()),
            ..SessionHints::default()
        },
    }
}

fn contains_text(session: &Session, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        message.text.contains(needle)
            || message.parts.iter().any(|part| {
                part.searchable_text()
                    .is_some_and(|text| text.contains(needle))
            })
    })
}

fn has_tool_use(session: &Session, name: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::ToolUse { name: tool, .. } if tool == name
            )
        })
    })
}

fn has_thinking(session: &Session, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::Thinking { text, .. } if text.contains(needle)
            )
        })
    })
}

fn has_tool_result(session: &Session, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::ToolResult { content, .. } if content.contains(needle)
            )
        })
    })
}

fn has_image(session: &Session, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::Image { data, .. } if data.contains(needle)
            )
        })
    })
}

fn has_note(session: &Session, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::Note { text, .. } if text.contains(needle)
            )
        })
    })
}

fn emit_context(home: &Path) -> EmitContext {
    EmitContext::new(home)
        .with_omp_runtime(OmpRuntime::parse("provider/native-model:off").unwrap())
        .with_codex_runtime(CodexRuntime {
            provider: "native-provider".to_owned(),
            model: "native-model".to_owned(),
        })
}

fn parse_emitted(catalog: &Catalog, target: TargetTool, path: &Path) -> Session {
    let source = match target {
        TargetTool::Pi => SourceTool::Pi,
        TargetTool::Rpi => SourceTool::Rpi,
        TargetTool::Omp => SourceTool::Omp,
        TargetTool::Droid => SourceTool::Droid,
        TargetTool::Codex => SourceTool::Codex,
        TargetTool::Claude => SourceTool::Claude,
        TargetTool::Grok | TargetTool::Hyper => SourceTool::Grok,
        TargetTool::Agent => panic!("agent is not a conversion target"),
    };
    catalog
        .parse(source, path)
        .unwrap_or_else(|error| panic!("reparse {target} at {}: {error}", path.display()))
}

#[test]
fn every_target_keeps_text_thinking_and_tools() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let cwd = home.join("workspace/project");
    fs::create_dir_all(&cwd).unwrap();
    let source = rich_session(&cwd);
    let catalog = Catalog::new(home);

    for target in TargetTool::ALL
        .into_iter()
        .filter(|target| *target != TargetTool::Agent)
    {
        let context = emit_context(home).with_session_id(format!(
            "00000000-0000-4000-8000-0000000000{:02}",
            target as u8
        ));
        let emitted = emit(&source, target, &context)
            .unwrap_or_else(|error| panic!("emit {target}: {error}"));
        let parsed = parse_emitted(&catalog, target, &emitted.path);
        assert!(
            contains_text(&parsed, "list the file"),
            "{target} dropped user text: {:?}",
            parsed.messages
        );
        assert!(
            contains_text(&parsed, "reading now"),
            "{target} dropped assistant text: {:?}",
            parsed.messages
        );
        assert!(
            contains_text(&parsed, "the file starts with fn main"),
            "{target} dropped final assistant text: {:?}",
            parsed.messages
        );
        assert!(
            has_tool_use(&parsed, "Read"),
            "{target} dropped tool use: {:?}",
            parsed.messages
        );
        assert!(
            has_tool_result(&parsed, "fn main()"),
            "{target} dropped tool result: {:?}",
            parsed.messages
        );
        assert!(
            has_thinking(&parsed, "I should read it"),
            "{target} dropped thinking: {:?}",
            parsed.messages
        );
        assert!(
            has_image(&parsed, "iVBORw0KGgo="),
            "{target} dropped image: {:?}",
            parsed.messages
        );
        assert!(
            has_note(&parsed, "prior context") || contains_text(&parsed, "prior context"),
            "{target} dropped compaction note: {:?}",
            parsed.messages
        );
        assert_eq!(
            parsed.hints.model.as_deref(),
            Some("source-model"),
            "{target} dropped model hint"
        );
        if keeps_compaction_hint(target) {
            assert_eq!(
                parsed.hints.compaction_summary.as_deref(),
                Some("prior context"),
                "{target} dropped compaction hint"
            );
        }
        assert_eq!(
            parsed.hints.thinking_level,
            Some(ThinkingLevel::High),
            "{target} dropped thinking level"
        );
    }
}

fn write_lines(path: &Path, lines: &[&str]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn convert_native(home: &Path, source: SourceTool, input: &Path, target: TargetTool) -> Session {
    let catalog = Catalog::new(home);
    let session = catalog
        .parse(source, input)
        .unwrap_or_else(|error| panic!("parse {source}: {error}"));
    let context = emit_context(home).with_session_id(format!(
        "{source}-{target}-00000000-0000-4000-8000-000000000099"
    ));
    let emitted = emit(&session, target, &context)
        .unwrap_or_else(|error| panic!("emit {source}->{target}: {error}"));
    parse_emitted(&catalog, target, &emitted.path)
}

#[test]
fn native_sources_convert_to_every_target() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();

    let pi = home.join(".pi/agent/sessions/--workspace-project--/2026-09-04T10-00-00_pi-src.jsonl");
    write_lines(
        &pi,
        &[
            r#"{"type":"session","version":3,"id":"pi-src","timestamp":"2026-09-04T10:00:00.000Z","cwd":"/workspace/project"}"#,
            r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-09-04T10:00:00.100Z","provider":"source-provider","modelId":"source-model"}"#,
            r#"{"type":"thinking_level_change","id":"tl1","parentId":"m1","timestamp":"2026-09-04T10:00:00.150Z","thinkingLevel":"high"}"#,
            r#"{"type":"message","id":"u1","parentId":"tl1","timestamp":"2026-09-04T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"list the file"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}],"timestamp":1}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-04T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"I should read it"},{"type":"text","text":"reading now"},{"type":"toolCall","id":"call-1","name":"Read","arguments":{"path":"src/lib.rs"}}],"api":"openai-completions","provider":"anthropic","model":"test","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
            r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-09-04T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"call-1","toolName":"Read","content":[{"type":"text","text":"fn main() {}"}],"isError":false,"timestamp":3}}"#,
            r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-09-04T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"the file starts with fn main"}],"api":"openai-completions","provider":"anthropic","model":"test","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":4}}"#,
            r#"{"type":"branch_summary","id":"b1","parentId":"a2","timestamp":"2026-09-04T10:00:05.000Z","fromId":"a2","summary":"other branch work"}"#,
            r#"{"type":"message","id":"x1","parentId":"b1","timestamp":"2026-09-04T10:00:06.000Z","message":{"role":"bashExecution","command":"ls","output":"src","timestamp":6}}"#,
        ],
    );

    let omp =
        home.join(".omp/agent/sessions/--workspace-project--/2026-09-04T10-00-00_omp-src.jsonl");
    write_lines(
        &omp,
        &[
            r#"{"type":"title","v":1,"title":"read the file","updatedAt":"2026-09-04T10:00:00.000Z","pad":"","source":"converted"}"#,
            r#"{"type":"session","version":3,"id":"omp-src","timestamp":"2026-09-04T10:00:00.000Z","cwd":"/workspace/project"}"#,
            r#"{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-09-04T10:00:00.100Z","provider":"source-provider","modelId":"source-model"}"#,
            r#"{"type":"thinking_level_change","id":"tl1","parentId":"m1","timestamp":"2026-09-04T10:00:00.150Z","thinkingLevel":"high"}"#,
            r#"{"type":"message","id":"u1","parentId":"tl1","timestamp":"2026-09-04T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"list the file"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}],"timestamp":1}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-04T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"I should read it"},{"type":"text","text":"reading now"},{"type":"toolCall","id":"call-1","name":"Read","arguments":{"path":"src/lib.rs"}}],"timestamp":2}}"#,
            r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-09-04T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"call-1","toolName":"Read","content":[{"type":"text","text":"fn main() {}"}],"isError":false,"timestamp":3}}"#,
            r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-09-04T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"the file starts with fn main"}],"timestamp":4}}"#,
        ],
    );

    let claude = home.join(".claude/projects/-workspace-project/claude-src.jsonl");
    write_lines(
        &claude,
        &[
            r#"{"type":"system","subtype":"compact_boundary","uuid":"cb","parentUuid":null,"timestamp":"2026-09-04T10:00:00.000Z","sessionId":"claude-src","cwd":"/workspace/project","compactMetadata":{"compactSummary":"prior context"}}"#,
            r#"{"type":"user","uuid":"c0","parentUuid":"cb","timestamp":"2026-09-04T10:00:00.500Z","sessionId":"claude-src","cwd":"/workspace/project","isCompactSummary":true,"message":{"role":"user","content":"prior context"}}"#,
            r#"{"type":"user","uuid":"u1","parentUuid":"c0","timestamp":"2026-09-04T10:00:01.000Z","sessionId":"claude-src","cwd":"/workspace/project","message":{"role":"user","content":[{"type":"text","text":"list the file"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-09-04T10:00:02.000Z","sessionId":"claude-src","cwd":"/workspace/project","effort":"high","message":{"role":"assistant","model":"source-model","content":[{"type":"thinking","thinking":"I should read it","signature":""},{"type":"text","text":"reading now"},{"type":"tool_use","id":"call-1","name":"Read","input":{"path":"src/lib.rs"}}]}}"#,
            r#"{"type":"user","uuid":"t1","parentUuid":"a1","timestamp":"2026-09-04T10:00:03.000Z","sessionId":"claude-src","cwd":"/workspace/project","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[{"type":"text","text":"fn main() {}"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"toolImgData"}}],"is_error":false}]}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"t1","timestamp":"2026-09-04T10:00:04.000Z","sessionId":"claude-src","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"the file starts with fn main"}]}}"#,
            r#"{"type":"last-prompt","lastPrompt":"list the file","leafUuid":"a2","sessionId":"claude-src"}"#,
        ],
    );

    let droid = home.join(".factory/sessions/-workspace-project/droid-src.jsonl");
    write_lines(
        &droid,
        &[
            r#"{"type":"session_start","id":"droid-src","title":"read the file","cwd":"/workspace/project","version":2,"owner":"tester","model":"source-model","thinkingLevel":"high"}"#,
            r#"{"type":"message","id":"u1","timestamp":"2026-09-04T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"list the file"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-04T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"I should read it"},{"type":"text","text":"reading now"},{"type":"tool_use","id":"call-1","name":"Read","input":{"path":"src/lib.rs"}}]}}"#,
            r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-09-04T10:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"fn main() {}"}]}}"#,
            r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-09-04T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"the file starts with fn main"}]}}"#,
        ],
    );

    let codex = home.join(
        ".codex/sessions/2026/09/04/rollout-2026-09-04T10-00-00-aaaaaaaa-0000-4000-8000-0000000000c0.jsonl",
    );
    write_lines(
        &codex,
        &[
            r#"{"timestamp":"2026-09-04T10:00:00.000Z","type":"session_meta","payload":{"id":"aaaaaaaa-0000-4000-8000-0000000000c0","timestamp":"2026-09-04T10:00:00.000Z","cwd":"/workspace/project","originator":"codex","cli_version":"0.0.0","source":"cli","model_provider":"source-provider"}}"#,
            r#"{"timestamp":"2026-09-04T10:00:00.050Z","type":"turn_context","payload":{"cwd":"/workspace/project","model":"source-model","reasoning_effort":"high"}}"#,
            r#"{"timestamp":"2026-09-04T10:00:00.080Z","type":"compacted","payload":{"message":"prior context"}}"#,
            r#"{"timestamp":"2026-09-04T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"list the file"},{"type":"input_image","image_url":"iVBORw0KGgo="}]}}"#,
            r#"{"timestamp":"2026-09-04T10:00:02.000Z","type":"response_item","payload":{"type":"reasoning","content":[{"type":"output_text","text":"I should read it"}]}}"#,
            r#"{"timestamp":"2026-09-04T10:00:02.100Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reading now"}]}}"#,
            r#"{"timestamp":"2026-09-04T10:00:02.200Z","type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"Read","arguments":"{\"path\":\"src/lib.rs\"}"}}"#,
            r#"{"timestamp":"2026-09-04T10:00:03.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"fn main() {}"}}"#,
            r#"{"timestamp":"2026-09-04T10:00:04.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"the file starts with fn main"}]}}"#,
        ],
    );

    let grok_dir = home.join(".grok/sessions/%2Fworkspace%2Fproject/grok-src");
    fs::create_dir_all(&grok_dir).unwrap();
    fs::write(
        grok_dir.join("summary.json"),
        r#"{"info":{"id":"grok-src","cwd":"/workspace/project"},"session_summary":"read the file","created_at":"2026-09-04T10:00:00Z","current_model_id":"source-model","reasoning_effort":"high"}"#,
    )
    .unwrap();
    fs::write(
        grok_dir.join("chat_history.jsonl"),
        concat!(
            r#"{"type":"user","content":[{"type":"text","text":"list the file"},{"type":"image","url":"iVBORw0KGgo="}]}"#,
            "\n",
            r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"I should read it"}]}"#,
            "\n",
            r#"{"type":"assistant","content":"reading now","tool_calls":[{"id":"call-1","name":"Read","arguments":"{\"path\":\"src/lib.rs\"}"}]}"#,
            "\n",
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"fn main() {}","images":[{"type":"image","url":"toolImgData"}]}"#,
            "\n",
            r#"{"type":"backend_tool_call","kind":{"tool_type":"web_search","id":"ws_1","action":{"type":"search","query":"capybaras"}}}"#,
            "\n",
            r#"{"type":"assistant","content":"the file starts with fn main"}"#,
            "\n",
        ),
    )
    .unwrap();
    let grok = grok_dir.join("summary.json");

    let sources = [
        (SourceTool::Pi, pi),
        (SourceTool::Omp, omp),
        (SourceTool::Claude, claude),
        (SourceTool::Droid, droid),
        (SourceTool::Codex, codex),
        (SourceTool::Grok, grok),
    ];

    for (source, input) in sources {
        for target in TargetTool::ALL
            .into_iter()
            .filter(|target| *target != TargetTool::Agent)
        {
            let parsed = convert_native(home, source, &input, target);
            assert!(
                contains_text(&parsed, "list the file"),
                "{source}->{target} dropped user text"
            );
            assert!(
                has_tool_use(&parsed, "Read"),
                "{source}->{target} dropped tool use"
            );
            assert!(
                has_tool_result(&parsed, "fn main()"),
                "{source}->{target} dropped tool result"
            );
            assert!(
                has_thinking(&parsed, "I should read it"),
                "{source}->{target} dropped thinking"
            );
            assert!(
                has_image(&parsed, "iVBORw0KGgo="),
                "{source}->{target} dropped image"
            );
            assert_eq!(
                parsed.hints.model.as_deref(),
                Some("source-model"),
                "{source}->{target} dropped model hint"
            );
            assert_eq!(
                parsed.hints.thinking_level,
                Some(ThinkingLevel::High),
                "{source}->{target} dropped thinking level"
            );
            assert!(
                has_object_tool_input(&parsed, "path", "src/lib.rs"),
                "{source}->{target} dropped object tool arguments"
            );
            if matches!(source, SourceTool::Claude | SourceTool::Codex) {
                assert!(
                    has_note(&parsed, "prior context") || contains_text(&parsed, "prior context"),
                    "{source}->{target} dropped compact summary"
                );
                if keeps_compaction_hint(target) {
                    assert_eq!(
                        parsed.hints.compaction_summary.as_deref(),
                        Some("prior context"),
                        "{source}->{target} dropped compaction hint"
                    );
                }
            }
            if matches!(source, SourceTool::Claude | SourceTool::Grok) {
                assert!(
                    has_image(&parsed, "toolImgData"),
                    "{source}->{target} dropped nested tool-result image"
                );
            }
            if source == SourceTool::Pi {
                assert!(
                    has_note(&parsed, "other branch work")
                        || contains_text(&parsed, "other branch work"),
                    "{source}->{target} dropped branch summary"
                );
                assert!(
                    has_note(&parsed, "ls") || contains_text(&parsed, "ls"),
                    "{source}->{target} dropped bash execution"
                );
            }
            if source == SourceTool::Grok {
                assert!(
                    has_tool_use(&parsed, "web_search"),
                    "{source}->{target} dropped backend tool call"
                );
            }
        }
    }
}

fn keeps_compaction_hint(target: TargetTool) -> bool {
    matches!(
        target,
        TargetTool::Pi
            | TargetTool::Rpi
            | TargetTool::Omp
            | TargetTool::Claude
            | TargetTool::Codex
            | TargetTool::Droid
    )
}

fn has_object_tool_input(session: &Session, key: &str, value: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::ToolUse { input, .. }
                    if input.get(key).and_then(serde_json::Value::as_str) == Some(value)
            )
        })
    })
}
