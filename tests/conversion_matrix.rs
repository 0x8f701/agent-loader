//! Source × target conversion matrix.
//!
//! Every convertible source, including Agent and Antigravity, is emitted to
//! every target except Agent. The portable conversation must keep user and
//! assistant text, thinking, tool ids, tool arguments, and tool results that
//! the target can represent. Fields a target cannot store are reported as
//! representability losses. Antigravity rows go through the production
//! SQLite/protobuf emitter and parser, not a hand-built database. OpenCode
//! rows use the production portable export writer/parser with an explicit
//! output path, so the matrix never invokes the native import CLI.
use std::fs;
use std::path::Path;

use agent_loader::domain::{
    ContentPart, Message, Role, Session, SessionHints, SourceTool, TargetTool, ThinkingLevel,
};
use agent_loader::emit::{CodexRuntime, EmitContext, OmpRuntime, emit};
use agent_loader::formats::opencode;
use agent_loader::sessions::Catalog;
use serde_json::json;
use tempfile::TempDir;

/// Synthetic 1x1 PNGs (red, green, blue). Distinct valid image bytes let the
/// matrix prove that user, assistant, and tool-result images survive
/// independently instead of matching one shared placeholder.
const OPENCODE_USER_IMAGE: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
const OPENCODE_ASSISTANT_IMAGE: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mNg+M8AAAICAQBF9FLUAAAAAElFTkSuQmCC";
const OPENCODE_TOOL_IMAGE: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mNgYPgPAAEDAQA2dBFAAAAAAElFTkSuQmCC";

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

fn has_image_in_role(session: &Session, role: Role, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        message.role == role
            && message.parts.iter().any(|part| {
                matches!(
                    part,
                    ContentPart::Image { data, .. } if data.contains(needle)
                )
            })
    })
}

fn has_image_in_tool_result(session: &Session, needle: &str) -> bool {
    session.messages.iter().any(|message| {
        let has_result = message
            .parts
            .iter()
            .any(|part| matches!(part, ContentPart::ToolResult { .. }));
        has_result
            && message.parts.iter().any(|part| {
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
    assert_eq!(targets.len(), 9, "expected 9 non-Agent conversion targets");
    assert!(
        targets.contains(&TargetTool::Antigravity),
        "Antigravity must be a conversion target"
    );
    assert!(
        targets.contains(&TargetTool::OpenCode),
        "OpenCode must be a conversion target"
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
    assert_eq!(targets.len(), 9);
    for target in targets {
        let mut context = emit_context(home).with_session_id(format!(
            "00000000-0000-4000-8000-0000000000{:02}",
            target as u8
        ));
        if target == TargetTool::OpenCode {
            // The native OpenCode target imports through the installed CLI.
            // The conversion matrix must stay hermetic and exercise the pure
            // portable export path instead. OpenCode identifies sessions with
            // `ses_` + 26 characters, and an explicit export filename must be
            // the session id.
            let id = "ses_00000000000000000000000009";
            context = context
                .with_session_id(id)
                .with_output(home.join(format!("{id}.json")));
        }
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
        if target == TargetTool::OpenCode {
            assert_eq!(
                parsed.hints.provider.as_deref(),
                Some("source-provider"),
                "{target} dropped provider hint"
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
        } else if target == TargetTool::OpenCode {
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
    let mut context = emit_context(home).with_session_id(format!(
        "00000000-0000-4000-8000-{:06x}{:06x}",
        source as u32, target as u32
    ));
    if target == TargetTool::OpenCode {
        // Keep the conversion matrix hermetic: OpenCode default emission would
        // invoke the installed native import, while an explicit output is the
        // pure portable export. Native IDs are `ses_` + 26 characters and the
        // explicit filename must be that id.
        let id = format!("ses_{:026x}", (source as u32) << 8 | target as u32);
        context = context
            .with_session_id(id.clone())
            .with_output(home.join(format!("{id}.json")));
    }
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
    let opencode = write_opencode_export_fixture(home);

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
        (SourceTool::OpenCode, opencode),
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
                    has_image(&parsed, source_user_image(source)),
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
                // Grok, OpenCode's required unknown label, the converted-from
                // label for tree/Claude formats, and nothing for formats with
                // no model slot.
                let expected = match target {
                    TargetTool::Omp | TargetTool::Codex => Some("native-model"),
                    TargetTool::Grok => Some("grok-4.5"),
                    TargetTool::OpenCode => Some("unknown"),
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
                SourceTool::Pi
                    | SourceTool::Rpi
                    | SourceTool::Omp
                    | SourceTool::Codex
                    | SourceTool::OpenCode
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
                    TargetTool::Pi
                        | TargetTool::Rpi
                        | TargetTool::Omp
                        | TargetTool::Codex
                        | TargetTool::OpenCode
                )
            {
                assert_eq!(
                    parsed.hints.provider.as_deref(),
                    before.hints.provider.as_deref(),
                    "{source}->{target} dropped representable provider"
                );
            }
            if matches!(
                source,
                SourceTool::Agent | SourceTool::Antigravity | SourceTool::OpenCode
            ) {
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
            if before.hints.thinking_level.is_some()
                && !matches!(target, TargetTool::Antigravity | TargetTool::OpenCode)
            {
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
            if before.hints.thinking_level.is_some() && target == TargetTool::OpenCode {
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
            if matches!(
                source,
                SourceTool::Claude | SourceTool::Codex | SourceTool::OpenCode
            ) || (matches!(source, SourceTool::Pi | SourceTool::Rpi)
                && (has_note(&before, "prior context")
                    || contains_text(&before, "prior context")))
            {
                assert!(
                    has_note(&parsed, "prior context") || contains_text(&parsed, "prior context"),
                    "{source}->{target} dropped compact summary"
                );
                if keeps_compaction_hint(target)
                    && before.hints.compaction_summary.is_some()
                {
                    assert_eq!(
                        parsed.hints.compaction_summary.as_deref(),
                        Some("prior context"),
                        "{source}->{target} dropped compaction hint"
                    );
                }
            }
            if matches!(
                source,
                SourceTool::Claude | SourceTool::Grok | SourceTool::Agent | SourceTool::OpenCode
            ) {
                assert!(
                    has_image(&before, source_tool_image(source)),
                    "{source} parser dropped nested tool-result image"
                );
                if represents_user_image(target) {
                    assert!(
                        has_image(&parsed, source_tool_image(source)),
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
                    has_image(&before, source_assistant_image(source)),
                    "{source} parser dropped assistant image"
                );
            }
            if represents_assistant_image(source) && represents_assistant_image_target(target) {
                assert!(
                    has_image(&parsed, source_assistant_image(source)),
                    "{source}->{target} dropped representable assistant image"
                );
            }
            if target == TargetTool::OpenCode
                && has_thinking_signature(&before, "think-sig")
            {
                assert!(
                    !has_thinking_signature(&parsed, "think-sig"),
                    "{source}->{target} fabricated a foreign thinking signature"
                );
            }
            if source == SourceTool::OpenCode {
                assert!(
                    has_image_in_role(&before, Role::User, source_user_image(source)),
                    "{source} parser moved or dropped the user image"
                );
                assert!(
                    has_image_in_role(&before, Role::Assistant, source_assistant_image(source)),
                    "{source} parser moved or dropped the assistant image"
                );
                assert!(
                    has_image_in_tool_result(&before, source_tool_image(source)),
                    "{source} parser moved or dropped the tool-result image"
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
                } else if target == TargetTool::OpenCode {
                    assert!(
                        !has_thinking_signature(&parsed, "think-sig"),
                        "{source}->{target} fabricated a foreign thinking signature"
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

/// Existing native fixtures use their historical image payloads; the new
/// OpenCode fixture carries distinct valid PNG bytes so role mix-ups cannot
/// pass by matching one shared placeholder.
fn source_user_image(source: SourceTool) -> &'static str {
    if source == SourceTool::OpenCode {
        OPENCODE_USER_IMAGE
    } else {
        "iVBORw0KGgo="
    }
}

fn source_assistant_image(source: SourceTool) -> &'static str {
    if source == SourceTool::OpenCode {
        OPENCODE_ASSISTANT_IMAGE
    } else {
        "YVNzaXN0YW50SW1n"
    }
}

fn source_tool_image(source: SourceTool) -> &'static str {
    if source == SourceTool::OpenCode {
        OPENCODE_TOOL_IMAGE
    } else {
        "dG9vbEltZ0RhdGE="
    }
}

/// Current Antigravity planner media stores assistant images, and OpenCode
/// file parts are role-independent. Older upstream claims of no assistant slot
/// are obsolete and must not hide a drop.
fn represents_assistant_image(source: SourceTool) -> bool {
    matches!(source, SourceTool::Antigravity | SourceTool::OpenCode)
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
            | TargetTool::OpenCode
    )
}

/// A foreign display name is not a native numeric model enum. Antigravity
/// must leave the hint unset rather than invent `model-N`.
fn represents_foreign_model(target: TargetTool) -> bool {
    target != TargetTool::Antigravity && target != TargetTool::Agent
}

/// OpenCode drops foreign reasoning signatures instead of fabricating native
/// replay metadata; only exact native metadata is retained, opaquely.
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

/// A native-shaped synthetic OpenCode conversation. IDs use the `ses_` +
/// 26-character grammar rather than UUIDs, because OpenCode accepts its own
/// identifier shape and does not persist UUID-looking session IDs verbatim.
fn opencode_rich_session(cwd: &Path) -> Session {
    Session {
        tool: SourceTool::OpenCode,
        session_id: "ses_00000000000000000000000001".to_owned(),
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
                        data: OPENCODE_USER_IMAGE.to_owned(),
                    },
                ],
                Some("2026-09-04T10:00:01.000Z".to_owned()),
            ),
            Message::from_parts(
                Role::Assistant,
                vec![
                    ContentPart::Thinking {
                        text: "I should read it".to_owned(),
                        signature: Some("foreign-signature".to_owned()),
                    },
                    ContentPart::Text("reading now".to_owned()),
                    ContentPart::Image {
                        mime_type: Some("image/png".to_owned()),
                        data: OPENCODE_ASSISTANT_IMAGE.to_owned(),
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
                        data: OPENCODE_TOOL_IMAGE.to_owned(),
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
        path: cwd.join("opencode-source.json"),
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

/// Write an official portable OpenCode export through the production adapter,
/// not a hand-built JSON document. This is the source-side fixture contract.
fn write_opencode_export_fixture(home: &Path) -> std::path::PathBuf {
    let cwd = home.join("workspace/project");
    fs::create_dir_all(&cwd).unwrap();
    let session = opencode_rich_session(&cwd);
    let path = home.join("opencode-source-export.json");
    opencode::write_export(&session, &path, &session.session_id)
        .expect("production OpenCode export writer");
    path
}

fn json_contains(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains(needle),
        serde_json::Value::Array(items) => items.iter().any(|item| json_contains(item, needle)),
        serde_json::Value::Object(object) => {
            object.iter().any(|(key, value)| {
                key.contains(needle) || json_contains(value, needle)
            })
        }
        _ => false,
    }
}

fn opencode_part<'a>(
    document: &'a serde_json::Value,
    part_type: &str,
) -> Vec<&'a serde_json::Value> {
    document["messages"]
        .as_array()
        .expect("export messages array")
        .iter()
        .flat_map(|message| {
            message["parts"]
                .as_array()
                .expect("export parts array")
                .iter()
        })
        .filter(|part| part["type"].as_str() == Some(part_type))
        .collect()
}

#[test]
fn opencode_source_export_round_trips_semantic_parts() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let path = write_opencode_export_fixture(home);
    let parsed = Catalog::new(home)
        .parse(SourceTool::OpenCode, &path)
        .unwrap_or_else(|error| panic!("parse OpenCode export: {error}"));

    assert_eq!(parsed.tool, SourceTool::OpenCode);
    assert!(
        contains_text(&parsed, "list the file")
            && contains_text(&parsed, "reading now")
            && contains_text(&parsed, "the file starts with fn main"),
        "OpenCode export dropped message text: {:?}",
        parsed.messages
    );
    assert!(
        has_thinking(&parsed, "I should read it"),
        "OpenCode export dropped reasoning: {:?}",
        parsed.messages
    );
    assert!(
        !has_thinking_signature(&parsed, "foreign-signature"),
        "foreign conversions must not fabricate an OpenCode reasoning signature: {:?}",
        parsed.messages
    );
    assert!(
        has_tool_use(&parsed, "Read") && has_tool_id(&parsed, "call-1"),
        "OpenCode export dropped the tool call identity: {:?}",
        parsed.messages
    );
    assert!(
        has_object_tool_input(&parsed, "path", "src/lib.rs"),
        "OpenCode export dropped typed tool arguments: {:?}",
        parsed.messages
    );
    assert!(
        has_tool_result(&parsed, "fn main()") && has_tool_result_id(&parsed, "call-1"),
        "OpenCode export dropped the tool result pairing: {:?}",
        parsed.messages
    );
    assert!(
        has_image_in_role(&parsed, Role::User, OPENCODE_USER_IMAGE),
        "OpenCode export dropped or moved the user image: {:?}",
        parsed.messages
    );
    assert!(
        has_image_in_role(&parsed, Role::Assistant, OPENCODE_ASSISTANT_IMAGE),
        "OpenCode export dropped or moved the assistant image: {:?}",
        parsed.messages
    );
    assert!(
        has_image_in_tool_result(&parsed, OPENCODE_TOOL_IMAGE),
        "OpenCode export dropped the tool-result image attachment: {:?}",
        parsed.messages
    );
    assert!(
        has_note(&parsed, "prior context") || contains_text(&parsed, "prior context"),
        "OpenCode export dropped compaction content: {:?}",
        parsed.messages
    );
    assert_eq!(
        parsed.hints.model.as_deref(),
        Some("source-model"),
        "OpenCode export dropped the model hint"
    );
    assert_eq!(
        parsed.hints.provider.as_deref(),
        Some("source-provider"),
        "OpenCode export dropped the provider hint"
    );
    assert_eq!(parsed.summary, "read the file");
    assert_eq!(parsed.cwd, home.join("workspace/project"));
}

/// Native OpenCode reasoning metadata is an exact native fact and may be
/// retained by the parser. Foreign writers must still drop foreign
/// signatures instead of turning them into native metadata.
#[test]
fn opencode_native_reasoning_metadata_is_retained() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let session = "ses_00000000000000000000000003";
    let user = "msg_00000000000000000000000003";
    let assistant = "msg_00000000000000000000000004";
    let path = home.join("native-reasoning.json");
    let document = json!({
        "info": {
            "id": session,
            "slug": "native-reasoning",
            "projectID": "global",
            "directory": home.to_str().unwrap(),
            "title": "native metadata",
            "version": "1.19.6",
            "time": {"created": 0, "updated": 1},
        },
        "messages": [
            {
                "info": {
                    "id": user,
                    "sessionID": session,
                    "role": "user",
                    "time": {"created": 0},
                    "agent": "build",
                    "model": {"providerID": "native-provider", "modelID": "native-model"},
                },
                "parts": [{
                    "id": "prt_00000000000000000000000003",
                    "sessionID": session,
                    "messageID": user,
                    "type": "text",
                    "text": "native prompt",
                }],
            },
            {
                "info": {
                    "id": assistant,
                    "sessionID": session,
                    "role": "assistant",
                    "time": {"created": 1},
                    "parentID": user,
                    "modelID": "native-model",
                    "providerID": "native-provider",
                    "mode": "build",
                    "agent": "build",
                    "path": {"cwd": home.to_str().unwrap(), "root": home.to_str().unwrap()},
                    "cost": 0,
                    "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                },
                "parts": [{
                    "id": "prt_00000000000000000000000004",
                    "sessionID": session,
                    "messageID": assistant,
                    "type": "reasoning",
                    "text": "native thought",
                    "time": {"start": 1, "end": 1},
                    "metadata": {"signature": "native-signature"},
                }],
            },
        ],
    });
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let parsed = Catalog::new(home)
        .parse(SourceTool::OpenCode, &path)
        .expect("parse native reasoning metadata");
    assert!(
        has_thinking(&parsed, "native thought"),
        "native reasoning text was dropped: {:?}",
        parsed.messages
    );
    assert!(
        has_note(&parsed, "native-signature")
            || has_thinking_signature(&parsed, "native-signature"),
        "native reasoning metadata was dropped instead of retained opaquely: {:?}",
        parsed.messages
    );

    let roundtrip = home.join("native-reasoning-roundtrip.json");
    opencode::write_export(
        &parsed,
        &roundtrip,
        "ses_00000000000000000000000005",
    )
    .expect("re-export native reasoning metadata");
    let restored: serde_json::Value =
        serde_json::from_slice(&fs::read(&roundtrip).unwrap()).unwrap();
    assert!(
        json_contains(&restored, "native-signature"),
        "native reasoning metadata was not restored on re-export: {restored}"
    );
}

#[test]
fn opencode_explicit_output_is_portable_json_without_native_import() {
    let temporary = TempDir::new().unwrap();
    let home = temporary.path();
    let cwd = home.join("workspace/project");
    fs::create_dir_all(&cwd).unwrap();
    let source = opencode_rich_session(&cwd);
    let id = "ses_00000000000000000000000002";
    // Explicit output paths are arbitrary: the native id lives in the export
    // document, not in the filename.
    let output = home.join("converted-opencode.json");
    let context = emit_context(home)
        .with_session_id(id)
        .with_output(&output);
    let emitted = emit(&source, TargetTool::OpenCode, &context)
        .unwrap_or_else(|error| panic!("emit OpenCode with explicit output: {error}"));

    assert_eq!(emitted.path, output, "explicit output path was not honored");
    assert_eq!(emitted.session_id, id);
    assert!(
        !home
            .join(".local/share/opencode/opencode.db")
            .exists(),
        "explicit output must not import into the native store"
    );
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(&output).expect("read explicit export"))
            .expect("explicit export must be one JSON document");

    assert_eq!(document["info"]["id"], emitted.session_id.as_str());
    assert_eq!(document["info"]["directory"], cwd.to_str().unwrap());
    assert_eq!(document["info"]["title"], "read the file");

    let messages = document["messages"].as_array().expect("messages array");
    assert!(!messages.is_empty(), "export must contain messages");
    assert!(
        messages.iter().any(|message| {
            message["info"]["role"].as_str() == Some("user")
                && message["info"]["model"]["modelID"].as_str() == Some("source-model")
                && message["info"]["model"]["providerID"].as_str() == Some("source-provider")
        }),
        "export dropped the model/provider carried by user messages: {document}"
    );
    for message in messages {
        let id = message["info"]["id"].as_str().expect("message id");
        assert!(
            id.starts_with("msg_"),
            "native message id must start with msg_: {id:?}"
        );
        for part in message["parts"].as_array().expect("parts array") {
            let id = part["id"].as_str().expect("part id");
            assert!(
                id.starts_with("prt_"),
                "native part id must start with prt_: {id:?}"
            );
        }
    }

    let reasoning = opencode_part(&document, "reasoning");
    assert!(
        reasoning
            .iter()
            .any(|part| part["text"].as_str() == Some("I should read it")),
        "export dropped the reasoning part: {document}"
    );
    let tools = opencode_part(&document, "tool");
    let tool = tools
        .iter()
        .find(|part| part["callID"].as_str() == Some("call-1"))
        .unwrap_or_else(|| panic!("export dropped the tool part: {document}"));
    assert_eq!(tool["tool"], "Read");
    assert_eq!(tool["state"]["status"], "completed");
    assert_eq!(tool["state"]["input"]["path"], "src/lib.rs");
    assert_eq!(tool["state"]["output"], "fn main() {}");
    let file_in_role = |role: &str, needle: &str| {
        messages.iter().any(|message| {
            message["info"]["role"].as_str() == Some(role)
                && message["parts"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part["type"].as_str() == Some("file")
                            && part["mime"].as_str() == Some("image/png")
                            && part["url"]
                                .as_str()
                                .is_some_and(|url| url.contains(needle))
                    })
                })
        })
    };
    assert!(
        file_in_role("user", OPENCODE_USER_IMAGE),
        "export dropped or moved the user image file part: {document}"
    );
    assert!(
        file_in_role("assistant", OPENCODE_ASSISTANT_IMAGE),
        "export dropped or moved the assistant image file part: {document}"
    );
    let attachments = tool["state"]["attachments"]
        .as_array()
        .unwrap_or_else(|| panic!("tool result has no attachment array: {document}"));
    assert!(
        attachments.iter().any(|attachment| {
            attachment["type"].as_str() == Some("file")
                && attachment["mime"].as_str() == Some("image/png")
                && attachment["url"]
                    .as_str()
                    .is_some_and(|url| url.contains(OPENCODE_TOOL_IMAGE))
        }),
        "explicit export dropped the tool-result image attachment: {document}"
    );
    assert!(
        json_contains(&document, "prior context"),
        "explicit export dropped compaction content: {document}"
    );

    let reparsed = Catalog::new(home)
        .parse(SourceTool::OpenCode, &emitted.path)
        .unwrap_or_else(|error| panic!("reparse explicit OpenCode export: {error}"));
    assert!(
        contains_text(&reparsed, "list the file") && contains_text(&reparsed, "reading now"),
        "explicit export lost text on reparse: {:?}",
        reparsed.messages
    );
    assert!(
        has_tool_result(&reparsed, "fn main()") && has_tool_result_id(&reparsed, "call-1"),
        "explicit export lost tool results on reparse: {:?}",
        reparsed.messages
    );
    assert!(
        !has_thinking_signature(&reparsed, "foreign-signature"),
        "explicit foreign conversion must not fabricate an OpenCode reasoning signature: {:?}",
        reparsed.messages
    );
    assert!(
        has_image_in_role(&reparsed, Role::User, OPENCODE_USER_IMAGE),
        "reparse moved or dropped the user image: {:?}",
        reparsed.messages
    );
    assert!(
        has_image_in_role(&reparsed, Role::Assistant, OPENCODE_ASSISTANT_IMAGE),
        "reparse moved or dropped the assistant image: {:?}",
        reparsed.messages
    );
    assert!(
        has_image_in_tool_result(&reparsed, OPENCODE_TOOL_IMAGE),
        "reparse dropped the tool-result image attachment: {:?}",
        reparsed.messages
    );
    assert_eq!(
        reparsed.hints.model.as_deref(),
        Some("source-model"),
        "explicit export lost the model hint on reparse"
    );
    assert_eq!(
        reparsed.hints.provider.as_deref(),
        Some("source-provider"),
        "explicit export lost the provider hint on reparse"
    );
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
