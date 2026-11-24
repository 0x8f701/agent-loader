//! Source × target conversion matrix.
//!
//! Every convertible source, including Agent and Antigravity, is emitted to
//! every target except Agent. The portable conversation must keep user and
//! assistant text, thinking, tool ids, tool arguments, and tool results that
//! the target can represent. Fields a target cannot store are reported as
//! representability losses. Antigravity rows go through the production
//! SQLite/protobuf emitter and parser, not a hand-built database.
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
    let source = target
        .source()
        .unwrap_or_else(|| panic!("{target} is not a conversion target"));
    assert_ne!(source, SourceTool::Agent, "agent is not a conversion target");
    catalog
        .parse(source, path)
        .unwrap_or_else(|error| panic!("reparse {target} at {}: {error}", path.display()))
}

/// Non-Agent targets. Adding a tool without updating this count fails the
/// matrix instead of silently shrinking coverage.
fn convertible_targets() -> Vec<TargetTool> {
    let targets: Vec<TargetTool> = TargetTool::ALL
        .into_iter()
        .filter(|target| *target != TargetTool::Agent)
        .collect();
    assert_eq!(
        targets.len(),
        TargetTool::ALL.len() - 1,
        "exactly one target (Agent) is excluded from conversion"
    );
    assert_eq!(targets.len(), 8, "expected 8 non-Agent conversion targets");
    assert!(
        targets.contains(&TargetTool::Antigravity),
        "Antigravity must be a conversion target"
    );
    targets
}

#[test]
fn every_target_keeps_text_thinking_and_tools() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let cwd = home.join("workspace/project");
    fs::create_dir_all(&cwd).unwrap();
    let source = rich_session(&cwd);
    let catalog = Catalog::new(home);

    let targets = convertible_targets();
    assert_eq!(targets.len(), 8);
    for target in targets {
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
        if represents_user_image(target) {
            assert!(
                has_image(&parsed, "iVBORw0KGgo="),
                "{target} dropped user image: {:?}",
                parsed.messages
            );
        }
        assert!(
            has_note(&parsed, "prior context") || contains_text(&parsed, "prior context"),
            "{target} dropped compaction note: {:?}",
            parsed.messages
        );
        if represents_foreign_model(target) {
            assert_eq!(
                parsed.hints.model.as_deref(),
                Some("source-model"),
                "{target} dropped model hint"
            );
        } else if target == TargetTool::Antigravity {
            assert!(
                parsed.hints.model.is_none(),
                "{target} invented a native model enum from {:?}",
                parsed.hints.model
            );
        }
        if keeps_compaction_hint(target) {
            assert_eq!(
                parsed.hints.compaction_summary.as_deref(),
                Some("prior context"),
                "{target} dropped compaction hint"
            );
        }
        if target == TargetTool::Antigravity {
            assert_eq!(
                parsed.hints.thinking_level,
                None,
                "{target} invented a thinking level from a hint it cannot store"
            );
        } else {
            assert_eq!(
                parsed.hints.thinking_level,
                Some(ThinkingLevel::High),
                "{target} dropped thinking level"
            );
        }
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
        "00000000-0000-4000-8000-{:06x}{:06x}",
        source as u32, target as u32
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
            r#"{"type":"compaction","id":"c0","parentId":"tl1","timestamp":"2026-09-04T10:00:00.200Z","summary":"prior context","retainedTail":[{"role":"assistant","content":[{"type":"text","text":"kept tail"}]}]}"#,
            r#"{"type":"message","id":"u1","parentId":"c0","timestamp":"2026-09-04T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"list the file"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}],"timestamp":1}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-09-04T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"I should read it"},{"type":"text","text":"reading now"},{"type":"toolCall","id":"call-1","name":"Read","arguments":{"path":"src/lib.rs"}}],"api":"openai-completions","provider":"anthropic","model":"test","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
            r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-09-04T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"call-1","toolName":"Read","content":[{"type":"text","text":"fn main() {}"}],"isError":false,"timestamp":3}}"#,
            r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-09-04T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"the file starts with fn main"}],"api":"openai-completions","provider":"anthropic","model":"test","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":4}}"#,
            r#"{"type":"branch_summary","id":"b1","parentId":"a2","timestamp":"2026-09-04T10:00:05.000Z","fromId":"a2","summary":"other branch work"}"#,
            r#"{"type":"message","id":"x1","parentId":"b1","timestamp":"2026-09-04T10:00:06.000Z","message":{"role":"bashExecution","command":"ls","output":"src","timestamp":6}}"#,
        ],
    );
    let rpi = home.join(".rpi/sessions/--workspace-project--/2026-09-04T10-00-00_rpi-src.jsonl");
    if let Some(parent) = rpi.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::copy(&pi, &rpi).unwrap();

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
            r#"{"type":"user","uuid":"t1","parentUuid":"a1","timestamp":"2026-09-04T10:00:03.000Z","sessionId":"claude-src","cwd":"/workspace/project","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[{"type":"text","text":"fn main() {}"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"dG9vbEltZ0RhdGE="}}],"is_error":false}]}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"t1","timestamp":"2026-09-04T10:00:04.000Z","sessionId":"claude-src","cwd":"/workspace/project","message":{"role":"assistant","content":[{"type":"text","text":"the file starts with fn main"},{"type":"document","title":"spec.pdf","source":{"media_type":"application/pdf"}},{"type":"search_result","title":"Example","url":"https://example.test","snippet":"excerpt"}]}}"#,
            r#"{"type":"last-prompt","lastPrompt":"list the file","leafUuid":"a2","sessionId":"claude-src"}"#,
        ],
    );

    let droid = home.join(".factory/sessions/-workspace-project/droid-src.jsonl");
    write_lines(
        &droid,
        &[
            r#"{"type":"session_start","id":"droid-src","title":"read the file","cwd":"/workspace/project","version":2,"owner":"tester","model":"source-model","thinkingLevel":"high"}"#,
            r#"{"type":"compaction_state","id":"c1","summaryText":"prior context","summaryKind":"auto"}"#,
            r#"{"type":"todo_state","id":"td1","todos":{"todos":"1. [pending] inspect parser"}}"#,
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
            r#"{"timestamp":"2026-09-04T10:00:02.200Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"call-1","name":"Read","input":"{\"path\":\"src/lib.rs\"}"}}"#,
            r#"{"timestamp":"2026-09-04T10:00:02.250Z","type":"response_item","payload":{"type":"web_search_call","id":"ws_1","action":{"type":"search","query":"capybaras"}}}"#,
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
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"fn main() {}","images":[{"type":"image","url":"dG9vbEltZ0RhdGE="}]}"#,
            "\n",
            r#"{"type":"backend_tool_call","kind":{"tool_type":"web_search","id":"ws_1","action":{"type":"search","query":"capybaras"}}}"#,
            "\n",
            r#"{"type":"assistant","content":"the file starts with fn main"}"#,
            "\n",
        ),
    )
    .unwrap();
    let grok = grok_dir.join("summary.json");

    let agent = write_agent_store(home);
    let antigravity = write_antigravity_fixture(home);

    let sources = [
        (SourceTool::Pi, pi),
        (SourceTool::Rpi, rpi),
        (SourceTool::Omp, omp),
        (SourceTool::Claude, claude),
        (SourceTool::Droid, droid),
        (SourceTool::Codex, codex),
        (SourceTool::Grok, grok),
        (SourceTool::Agent, agent),
        (SourceTool::Antigravity, antigravity),
    ];
    assert_eq!(
        sources.len(),
        SourceTool::ALL.len(),
        "native matrix must cover every source, including Antigravity"
    );

    for (source, input) in sources {
        let targets = convertible_targets();
        for target in targets {
            let parsed = convert_native(home, source, &input, target);
            let before = catalog_parse(home, source, &input);
            assert!(
                contains_text(&parsed, "list the file"),
                "{source}->{target} dropped user text"
            );
            assert!(
                has_tool_use(&parsed, "Read"),
                "{source}->{target} dropped tool use"
            );
            assert!(
                has_tool_id(&parsed, "call-1"),
                "{source}->{target} dropped tool id"
            );
            assert!(
                has_tool_result(&parsed, "fn main()"),
                "{source}->{target} dropped tool result"
            );
            assert!(
                has_tool_result_id(&parsed, "call-1"),
                "{source}->{target} dropped tool result id"
            );
            assert!(
                has_thinking(&parsed, "I should read it"),
                "{source}->{target} dropped thinking"
            );
            if represents_user_image(target) {
                assert!(
                    has_image(&parsed, "iVBORw0KGgo="),
                    "{source}->{target} dropped user image"
                );
            }
            if let Some(model) = before.hints.model.as_deref() {
                if represents_foreign_model(target) {
                    assert_eq!(
                        parsed.hints.model.as_deref(),
                        Some(model),
                        "{source}->{target} dropped model hint"
                    );
                } else if target == TargetTool::Antigravity {
                    assert!(
                        parsed
                            .hints
                            .model
                            .as_deref()
                            .is_none_or(|value| value == model),
                        "{source}->{target} invented a native model enum from {:?}",
                        parsed.hints.model
                    );
                }
            } else if source == SourceTool::Antigravity {
                // No model hint on the AG source: each target applies its own
                // fallback — the configured conversion runtime for OMP/Codex/
                // Grok, the converted-from label for tree/Claude formats, and
                // nothing for formats with no model slot.
                let expected = match target {
                    TargetTool::Omp | TargetTool::Codex => Some("native-model"),
                    TargetTool::Grok => Some("grok-4.5"),
                    TargetTool::Droid | TargetTool::Antigravity => None,
                    _ => Some("converted-from-antigravity"),
                };
                assert_eq!(
                    parsed.hints.model.as_deref(),
                    expected,
                    "{source}->{target} drifted from its no-hint model contract"
                );
            }
            if matches!(
                source,
                SourceTool::Pi | SourceTool::Rpi | SourceTool::Omp | SourceTool::Codex
            ) {
                assert_eq!(
                    before.hints.provider.as_deref(),
                    Some("source-provider"),
                    "{source} parser dropped provider"
                );
            }
            if before.hints.provider.is_some()
                && matches!(
                    target,
                    TargetTool::Pi | TargetTool::Rpi | TargetTool::Omp | TargetTool::Codex
                )
            {
                assert_eq!(
                    parsed.hints.provider.as_deref(),
                    before.hints.provider.as_deref(),
                    "{source}->{target} dropped representable provider"
                );
            }
            if matches!(source, SourceTool::Agent | SourceTool::Antigravity) {
                assert_eq!(
                    before.hints.thinking_level,
                    None,
                    "{source} parser invented thinking level"
                );
            } else {
                assert_eq!(
                    before.hints.thinking_level,
                    Some(ThinkingLevel::High),
                    "{source} parser dropped thinking level"
                );
            }
            if before.hints.thinking_level.is_some() && target != TargetTool::Antigravity {
                assert_eq!(
                    parsed.hints.thinking_level,
                    before.hints.thinking_level,
                    "{source}->{target} dropped thinking level"
                );
            }
            if before.hints.thinking_level.is_some() && target == TargetTool::Antigravity {
                assert_eq!(
                    parsed.hints.thinking_level,
                    None,
                    "{source}->{target} invented a thinking level"
                );
            }
            assert!(
                has_object_tool_input(&parsed, "path", "src/lib.rs"),
                "{source}->{target} dropped object tool arguments"
            );
            if matches!(source, SourceTool::Claude | SourceTool::Codex)
                || (matches!(source, SourceTool::Pi | SourceTool::Rpi)
                    && (has_note(&before, "prior context")
                        || contains_text(&before, "prior context")))
            {
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
            if matches!(source, SourceTool::Claude | SourceTool::Grok | SourceTool::Agent) {
                assert!(
                    has_image(&before, "dG9vbEltZ0RhdGE="),
                    "{source} parser dropped nested tool-result image"
                );
                if represents_user_image(target) {
                    assert!(
                        has_image(&parsed, "dG9vbEltZ0RhdGE="),
                        "{source}->{target} dropped nested tool-result image"
                    );
                }
            }
            if source == SourceTool::Antigravity {
                assert!(
                    has_image(&before, "dG9vbEltZ0RhdGE="),
                    "{source} parser dropped tool-result image"
                );
                if represents_user_image(target) {
                    assert!(
                        has_image(&parsed, "dG9vbEltZ0RhdGE="),
                        "{source}->{target} dropped tool-result image"
                    );
                }
            }
            if represents_assistant_image(source) {
                assert!(
                    has_image(&before, "YVNzaXN0YW50SW1n"),
                    "{source} parser dropped assistant image"
                );
            }
            if represents_assistant_image(source) && represents_assistant_image_target(target) {
                assert!(
                    has_image(&parsed, "YVNzaXN0YW50SW1n"),
                    "{source}->{target} dropped representable assistant image"
                );
            }
            if matches!(source, SourceTool::Pi | SourceTool::Rpi) {
                assert!(
                    has_note(&parsed, "other branch work")
                        || contains_text(&parsed, "other branch work"),
                    "{source}->{target} dropped branch summary"
                );
                assert!(
                    has_note(&parsed, "ls") || contains_text(&parsed, "ls"),
                    "{source}->{target} dropped bash execution"
                );
                assert!(
                    contains_text(&parsed, "kept tail"),
                    "{source}->{target} dropped compaction retainedTail"
                );
            }
            if source == SourceTool::Grok {
                assert!(
                    has_tool_use(&parsed, "web_search"),
                    "{source}->{target} dropped backend tool call"
                );
            }
            if source == SourceTool::Codex {
                assert!(
                    has_tool_use(&parsed, "web_search"),
                    "{source}->{target} dropped web_search_call"
                );
            }
            if source == SourceTool::Droid {
                assert!(
                    has_note(&parsed, "inspect parser") || contains_text(&parsed, "inspect parser"),
                    "{source}->{target} dropped todo_state"
                );
            }
            if source == SourceTool::Claude {
                assert!(
                    has_note(&parsed, "spec.pdf") || contains_text(&parsed, "spec.pdf"),
                    "{source}->{target} dropped document block"
                );
                assert!(
                    has_note(&parsed, "excerpt") || contains_text(&parsed, "excerpt"),
                    "{source}->{target} dropped search_result block"
                );
            }
            if source == SourceTool::Antigravity {
                assert!(
                    has_thinking_signature(&before, "think-sig"),
                    "{source} parser dropped thinking signature"
                );
                if represents_thinking_signature(target) {
                    assert!(
                        has_thinking_signature(&parsed, "think-sig"),
                        "{source}->{target} dropped thinking signature"
                    );
                }
            }
        }

    }
}

fn catalog_parse(home: &Path, source: SourceTool, input: &Path) -> Session {
    Catalog::new(home)
        .parse(source, input)
        .unwrap_or_else(|error| panic!("parse {source}: {error}"))
}

fn write_agent_store(home: &Path) -> std::path::PathBuf {
    let directory = home.join(".cursor/chats/0123456789abcdef0123456789abcdef/agent-src");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("store.db");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT)", [])
        .unwrap();
    connection
        .execute("CREATE TABLE blobs(id TEXT PRIMARY KEY, data BLOB)", [])
        .unwrap();
    connection
        .execute(
            "INSERT INTO meta VALUES('0', ?1)",
            [r#"{"agentId":"agent-src","model":"source-model"}"#],
        )
        .unwrap();
    let blobs: [&[u8]; 4] = [
        br#"{"role":"user","content":[{"type":"text","text":"list the file"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}]}"#,
        br#"{"role":"assistant","content":[{"type":"redacted-reasoning","data":"opaque"},{"type":"thinking","thinking":"I should read it","signature":"think-sig"},{"type":"text","text":"reading now"},{"type":"tool-call","toolCallId":"call-1","toolName":"Read","args":{"path":"src/lib.rs"}}]}"#,
        br#"{"role":"tool","content":[{"type":"tool-result","toolCallId":"call-1","result":[{"type":"text","text":"fn main() {}"},{"type":"image","data":"dG9vbEltZ0RhdGE=","mimeType":"image/png"}],"isError":false}]}"#,
        br#"{"role":"assistant","content":"the file starts with fn main"}"#,
    ];
    for (index, blob) in blobs.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO blobs VALUES(?1, ?2)",
                rusqlite::params![index.to_string(), blob],
            )
            .unwrap();
    }
    drop(connection);
    fs::write(
        directory.join("meta.json"),
        r#"{"cwd":"/workspace/project","title":"read the file","createdAtMs":1756976400000}"#,
    )
    .unwrap();
    path
}

fn has_tool_id(session: &Session, id: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(part, ContentPart::ToolUse { id: tool, .. } if tool == id)
        })
    })
}

fn has_tool_result_id(session: &Session, id: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::ToolResult { tool_use_id, .. } if tool_use_id == id
            )
        })
    })
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

/// Native user-image slots exist on every current target, including
/// Antigravity user media. A future target that flattens images must be
/// listed here as a loss instead of being asserted as preserved.
fn represents_user_image(target: TargetTool) -> bool {
    !matches!(target, TargetTool::Agent)
}

/// Current Antigravity planner media stores assistant images. Older upstream
/// claims of no assistant slot are obsolete and must not hide a drop.
fn represents_assistant_image(source: SourceTool) -> bool {
    source == SourceTool::Antigravity
}

fn represents_assistant_image_target(target: TargetTool) -> bool {
    matches!(
        target,
        TargetTool::Pi
            | TargetTool::Rpi
            | TargetTool::Omp
            | TargetTool::Claude
            | TargetTool::Droid
            | TargetTool::Grok
            | TargetTool::Antigravity
    )
}

/// A foreign display name is not a native numeric model enum. Antigravity
/// must leave the hint unset rather than invent `model-N`.
fn represents_foreign_model(target: TargetTool) -> bool {
    target != TargetTool::Antigravity && target != TargetTool::Agent
}

fn represents_thinking_signature(target: TargetTool) -> bool {
    matches!(
        target,
        TargetTool::Pi
            | TargetTool::Rpi
            | TargetTool::Omp
            | TargetTool::Claude
            | TargetTool::Antigravity
    )
}

fn has_thinking_signature(session: &Session, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::Thinking {
                    signature: Some(signature),
                    ..
                } if signature.contains(needle)
            )
        })
    })
}

/// Emit one native Antigravity conversation through the production writer,
/// then hand the carrier path to the catalog parser. The schema is whatever
/// `emit` writes.
fn write_antigravity_fixture(home: &Path) -> std::path::PathBuf {
    let cwd = home.join("workspace/project");
    fs::create_dir_all(&cwd).unwrap();
    let session = Session {
        tool: SourceTool::Claude,
        session_id: "antigravity-source".to_owned(),
        cwd: cwd.clone(),
        start_timestamp: Some("2026-09-04T10:00:00.000Z".to_owned()),
        summary: "read the file".to_owned(),
        messages: vec![
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
                        signature: Some("think-sig".to_owned()),
                    },
                    ContentPart::Text("reading now".to_owned()),
                    ContentPart::Image {
                        mime_type: Some("image/png".to_owned()),
                        data: "YVNzaXN0YW50SW1n".to_owned(),
                    },
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
                vec![
                    ContentPart::ToolResult {
                        tool_use_id: "call-1".to_owned(),
                        content: "fn main() {}".to_owned(),
                        is_error: false,
                    },
                    ContentPart::Image {
                        mime_type: Some("image/png".to_owned()),
                        data: "dG9vbEltZ0RhdGE=".to_owned(),
                    },
                ],
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
            ..SessionHints::default()
        },
    };
    let context = emit_context(home).with_session_id(
        "00000000-0000-4000-8000-0000000000a9".to_owned(),
    );
    let emitted = emit(&session, TargetTool::Antigravity, &context)
        .expect("production Antigravity emitter");
    assert!(
        emitted
            .path
            .starts_with(home.join(".gemini/antigravity-cli/conversations")),
        "Antigravity fixture landed outside the native root: {}",
        emitted.path.display()
    );
    assert_eq!(emitted.path.extension().and_then(|ext| ext.to_str()), Some("db"));
    assert!(
        emitted
            .path
            .parent()
            .and_then(Path::parent)
            .is_some_and(|root| root.join("brain").join(&emitted.session_id).is_dir()),
        "production emitter did not create the sibling brain directory"
    );
    emitted.path
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

#[test]
fn codex_developer_note_round_trips_and_unknown_note_stays_text() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let input = home.join(
        ".codex/sessions/2026/09/04/rollout-2026-09-04T10-00-00-bbbbbbbb-0000-4000-8000-0000000000d0.jsonl",
    );
    write_lines(
        &input,
        &[
            r#"{"timestamp":"2026-09-04T10:00:00.000Z","type":"session_meta","payload":{"id":"bbbbbbbb-0000-4000-8000-0000000000d0","timestamp":"2026-09-04T10:00:00.000Z","cwd":"/workspace/project","originator":"codex","cli_version":"0.0.0","source":"cli","model_provider":"source-provider"}}"#,
            r#"{"timestamp":"2026-09-04T10:00:00.050Z","type":"turn_context","payload":{"cwd":"/workspace/project","model":"source-model"}}"#,
            r#"{"timestamp":"2026-09-04T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"native developer instruction"}]}}"#,
            r#"{"timestamp":"2026-09-04T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"list the file"}]}}"#,
            r#"{"timestamp":"2026-09-04T10:00:03.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reading now"}]}}"#,
        ],
    );
    let before = catalog_parse(home, SourceTool::Codex, &input);
    assert!(
        has_note_kind(&before, "developer", "native developer instruction"),
        "parser did not keep developer note"
    );
    let parsed = convert_native(home, SourceTool::Codex, &input, TargetTool::Codex);
    assert!(
        has_note_kind(&parsed, "developer", "native developer instruction"),
        "developer note was not preserved"
    );
    assert!(
        !has_note_kind(&parsed, "custom", "native developer instruction"),
        "developer note changed kind"
    );

    let mut unknown = before;
    unknown.messages.insert(
        0,
        Message::from_parts(
            Role::User,
            vec![ContentPart::Note {
                kind: "custom".to_owned(),
                text: "user custom content".to_owned(),
            }],
            None,
        ),
    );
    let context = emit_context(home).with_session_id(
        "cccccccc-0000-4000-8000-0000000000d1".to_owned(),
    );
    let emitted = emit(&unknown, TargetTool::Codex, &context).unwrap();
    let reparsed = parse_emitted(&Catalog::new(home), TargetTool::Codex, &emitted.path);
    assert!(
        !has_note_kind(&reparsed, "developer", "user custom content"),
        "unknown note was elevated to developer"
    );
    assert!(
        contains_text(&reparsed, "user custom content"),
        "unknown note text was dropped"
    );
}

#[test]
fn droid_todo_state_round_trips_as_todo_note() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let input = home.join(".factory/sessions/-workspace-project/droid-todo.jsonl");
    write_lines(
        &input,
        &[
            r#"{"type":"session_start","id":"droid-todo","title":"read the file","cwd":"/workspace/project","version":2,"owner":"tester","model":"source-model","thinkingLevel":"high"}"#,
            r#"{"type":"todo_state","id":"td1","timestamp":"2026-09-04T10:00:00.500Z","todos":{"todos":"1. [pending] inspect parser"}}"#,
            r#"{"type":"message","id":"u1","timestamp":"2026-09-04T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"list the file"}]}}"#,
            r#"{"type":"message","id":"a1","timestamp":"2026-09-04T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"reading now"}]}}"#,
        ],
    );
    let before = catalog_parse(home, SourceTool::Droid, &input);
    assert!(
        has_note_kind(&before, "todo", "1. [pending] inspect parser"),
        "parser did not keep todo note"
    );
    let parsed = convert_native(home, SourceTool::Droid, &input, TargetTool::Droid);
    assert!(
        has_note_kind(&parsed, "todo", "1. [pending] inspect parser"),
        "todo note was not preserved"
    );
    assert!(
        !parsed.messages.iter().any(|message| {
            message.role == Role::User && message.text.contains("inspect parser")
        }),
        "todo note was rewritten as user text"
    );
}

fn has_note_kind(session: &Session, kind: &str, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        message.parts.iter().any(|part| {
            matches!(
                part,
                ContentPart::Note { kind: note_kind, text }
                    if note_kind == kind && text.contains(needle)
            )
        })
    })
}
