//! Pi session adapter.
//!
//! Pi (and OMP) stores a session as an append-only JSONL tree. The first
//! record is a `session` header (`{type:"session", id, cwd, timestamp, …}`);
//! every subsequent record is an entry (`{type, id, parentId, timestamp, …}`)
//! forming a tree via `parentId`. The file holds every branch ever appended;
//! the active branch is the last-appended non-header entry and its ancestor
//! chain. Only `user`/`assistant` messages along that chain are projected.
//!
//! Parsing follows the native load contract: the first logical line (the
//! first valid JSON object) MUST be a `session` header with a string id; a
//! nonempty, headerless file is unloadable and rejected (returned as `Err`)
//! so the catalog skips it. An empty file is tolerated with filename/parent
//! fallbacks so a freshly created (not-yet-flushed) session still resolves.
//! Parsing is otherwise intentionally lossy: malformed JSONL lines are skipped,
//! unrecognized entry roles are dropped, and only the active branch's
//! `user`/`assistant` first text is projected.

use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};

use crate::domain::{Message, Session, SourceTool};
use crate::formats::{read_jsonl_values, summarize_messages, tree};
use crate::formats::tree::TreeNode;

/// Parse a Pi/OMP session export into a lossy `Session`.
///
/// Returns `Err` for a nonempty file whose first logical line is not a valid
/// `session` header (missing `type:"session"` or a non-empty string `id`), so
/// the caller can skip it from the catalog. An empty file resolves with
/// filename/parent-directory fallbacks.
pub fn parse(path: &Path) -> Result<Session> {
    let modified_epoch = file_mtime(path);

    // An empty (or whitespace-only) file is tolerated with filename/parent
    // fallbacks so the catalog can represent a freshly created, not-yet-flushed
    // session. Crucially, this checks the file's *actual content*: a nonempty
    // file whose lines are all malformed (dropped by the reader, yielding no
    // values) is still "nonempty" and must be rejected, not fallen back from.
    if is_logically_empty(path)? {
        return Ok(empty_session(path, modified_epoch));
    }

    let values = read_jsonl_values(path)?;

    // Native contract: the first logical line (first valid JSON object) must
    // be a `session` header carrying a non-empty string id. A nonempty,
    // headerless file — including one whose content is entirely malformed, so
    // no valid JSON object exists — is unloadable and rejected so the catalog
    // skips it. `values.first()` is used (not indexing) so an all-malformed
    // file yields `None` here rather than panicking.
    let (header_object, session_id) = values
        .first()
        .and_then(Value::as_object)
        .filter(|object| object.get("type").and_then(Value::as_str) == Some("session"))
        .and_then(|object| {
            object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| (object, id))
        })
        .ok_or_else(|| {
            anyhow!(
                "unloadable Pi session (first line is not a valid session header): {}",
                path.display()
            )
        })?;

    let raw_cwd = header_object
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("");
    let raw_timestamp = header_object
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Remaining records form the append-only entry tree. Every entry type
    // (message, model_change, compaction, …) participates via id/parentId;
    // only `message`-type entries carry a conversation payload nested under
    // `message`, so the rest shape the tree without emitting a message.
    let mut nodes: Vec<TreeNode<'_>> = Vec::with_capacity(values.len().saturating_sub(1));
    for record in &values[1..] {
        let Some(object) = record.as_object() else {
            continue;
        };
        let id = object.get("id").and_then(Value::as_str).unwrap_or("");
        let parent_id = object.get("parentId").and_then(Value::as_str);
        let entry_timestamp = object.get("timestamp").and_then(Value::as_str);
        let (role, content) = message_payload(object);
        nodes.push(TreeNode {
            id,
            parent_id: parent_id.filter(|value| !value.is_empty()),
            role,
            content,
            timestamp: entry_timestamp,
        });
    }

    let messages: Vec<Message> = tree::project_messages(&tree::active_path(&nodes));
    let summary = summarize_messages(&messages);

    // The header's cwd is used verbatim — even when empty (ancient v1 files) —
    // rather than inventing a parent-dir fallback. Only the empty-file branch
    // below falls back to the parent directory.
    Ok(Session {
        tool: SourceTool::Pi,
        session_id: session_id.to_owned(),
        cwd: PathBuf::from(raw_cwd),
        start_timestamp: if raw_timestamp.is_empty() {
            None
        } else {
            Some(raw_timestamp.to_owned())
        },
        summary,
        messages,
        path: path.to_path_buf(),
        modified_epoch,
    })
}

/// Encode a working directory the way Pi names session directories: path
/// components joined by `-` and wrapped in `--…--`, with the leading root
/// dropped. Native Pi `resolve()`s the path to absolute first; this mirrors
/// that by absolutizing relative inputs against the process cwd (lexically —
/// no symlink resolution) and normalizing `.`/`..` components, so the result
/// is always absolute and never contains `..`.
///
/// Returns `Err` only when a relative input cannot be absolutized because the
/// process cwd is unavailable. `..` is folded away, never emitted. On Windows
/// the drive prefix's `:` and any `\\`/`/` are replaced with `-`. Within a
/// normal component, special characters (`.` `_`) are kept verbatim — only
/// separators are replaced. `<workspace>/user/Projects/dotfiles` →
/// `--workspace-user-Projects-dotfiles--`.
pub fn encode_cwd(cwd: &Path) -> Result<String> {
    // Absolutize relative inputs against the process cwd (lexical join, no
    // filesystem resolution), mirroring native `resolve()` for the path shape.
    let absolute: PathBuf = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| anyhow!("encode_cwd: current_dir unavailable: {error}"))?
            .join(cwd)
    };

    // Lexical normalization: drop CurDir, fold ParentDir against the last
    // normal component. RootDir/Prefix anchor the path and are never popped,
    // so ParentDir at the root is a no-op (it can never escape the root of an
    // absolute path).
    let mut parts: Vec<String> = Vec::new();
    let mut anchored = false;
    for component in absolute.components() {
        match component {
            Component::Prefix(_) => {
                anchored = true;
                parts.push(replace_separators(component.as_os_str()));
            }
            Component::RootDir => {
                anchored = true;
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                parts.push(name.to_string_lossy().into_owned());
            }
            Component::ParentDir => {
                if parts.last().is_some() {
                    parts.pop();
                } else if !anchored {
                    // Unreachable for an absolutized path, but defensive: a
                    // relative path escaping its root is rejected outright.
                    bail!("encode_cwd: path escapes root: {}", cwd.display());
                }
            }
        }
    }

    // Defensive: no ParentDir may survive normalization in an emitted path.
    if parts.iter().any(|part| part == "..") {
        bail!("encode_cwd: unresolved parent reference: {}", cwd.display());
    }
    if !anchored {
        bail!("encode_cwd: path is not absolute: {}", cwd.display());
    }

    let inner = parts.join("-");
    Ok(format!("--{inner}--"))
}

/// Replace path separators (and, on Windows, the drive-letter `:`) with `-`.
/// Applied only to prefix strings; normal component names already exclude
/// separators and are kept verbatim.
fn replace_separators(value: &std::ffi::OsStr) -> String {
    value
        .to_string_lossy()
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' => '-',
            _ => character,
        })
        .collect()
}

/// Extract the conversation payload from a `message`-type entry.
///
/// The role/content live nested under a `message` object; other entry types
/// have no such payload and yield `(None, None)`, so they never project.
fn message_payload(object: &Map<String, Value>) -> (Option<&str>, Option<&Value>) {
    let Some(message) = object.get("message") else {
        return (None, None);
    };
    let Some(message_obj) = message.as_object() else {
        return (None, None);
    };
    let role = message_obj.get("role").and_then(Value::as_str);
    let content = message_obj.get("content");
    (role, content)
}

/// Build a fallback `Session` for an empty (not-yet-flushed) file: id from the
/// file name, cwd from the parent directory, no messages.
fn empty_session(path: &Path, modified_epoch: Option<f64>) -> Session {
    Session {
        tool: SourceTool::Pi,
        session_id: fallback_id(path),
        cwd: fallback_cwd(path),
        start_timestamp: None,
        summary: summarize_messages(&[]),
        messages: Vec::new(),
        path: path.to_path_buf(),
        modified_epoch,
    }
}

/// Derive a session id from the file name (empty-file fallback only).
fn fallback_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .or_else(|| path.file_name().and_then(|value| value.to_str()))
        .unwrap_or("")
        .to_owned()
}

/// Derive the working directory from the file's parent (empty-file fallback
/// only; nonempty files use the header cwd verbatim).
fn fallback_cwd(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

/// File mtime as a POSIX epoch second, when statable.
fn file_mtime(path: &Path) -> Option<f64> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0)
    }
    #[cfg(not(unix))]
    {
        metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_secs_f64())
    }
}

/// Whether a session file is logically empty: zero bytes, or only whitespace.
///
/// This inspects the file's *actual content* (streaming, early-exit on the
/// first non-whitespace byte) rather than the parsed record count, so a
/// nonempty file whose lines are all malformed — and thus yields no values
/// from the reader — is correctly treated as nonempty (and must therefore be
/// loadable), not as an empty-file fallback.
fn is_logically_empty(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening session {}", path.display()))?;
    let mut buffer = [0u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading session {}", path.display()))?;
        if read == 0 {
            return Ok(true);
        }
        if buffer[..read]
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
        {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;
    use crate::domain::Role;

    fn write_session(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("temp file");
        for line in lines {
            writeln!(file, "{line}").expect("write line");
        }
        file.flush().expect("flush");
        file
    }

    const HEADER: &str =
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#;

    fn msg(id: &str, parent_id: Option<&str>, role: &str, content: &str) -> String {
        let parent = match parent_id {
            Some(value) => format!("\"{value}\""),
            None => "null".to_owned(),
        };
        format!(
            r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2026-01-01T00:00:0{id}.000Z","message":{{"role":"{role}","content":{content}}}}}"#
        )
    }

    #[test]
    fn parses_header_and_active_branch_messages() {
        let file = write_session(&[
            HEADER,
            &msg("a", None, "user", r#""hi""#),
            &msg("b", Some("a"), "assistant", r#"[{"type":"text","text":"hello"}]"#),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.tool, SourceTool::Pi);
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.cwd, PathBuf::from("/tmp"));
        assert_eq!(
            session.start_timestamp.as_deref(),
            Some("2026-01-01T00:00:00.000Z")
        );
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text, "hi");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].text, "hello");
        assert_eq!(session.summary, "hi");
        assert_eq!(session.path, file.path());
        assert!(session.modified_epoch.is_some());
    }

    #[test]
    fn malformed_lines_after_header_are_skipped() {
        // A garbage non-JSON line and a valid-JSON object lacking the entry
        // shape must be skipped without aborting the parse. The header remains
        // the first logical line.
        let file = write_session(&[
            HEADER,
            "this is not json at all",
            r#"{"orphan":"no type or id"}"#,
            &msg("a", None, "user", r#""real""#),
            "   ",
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].text, "real");
    }

    #[test]
    fn nonempty_file_without_session_header_returns_err() {
        // First logical line is a message, not a header → unloadable.
        let file = write_session(&[&msg("a", None, "user", r#""q""#)]);
        let err = parse(file.path()).expect_err("should be unloadable");
        assert!(
            err.to_string().contains("not a valid session header"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn header_missing_string_id_returns_err() {
        // A `session` record without a non-empty string id is not a valid
        // header → unloadable (no fallback).
        let file = write_session(&[
            r#"{"type":"session","cwd":"/tmp","timestamp":"2026-01-01T00:00:00.000Z"}"#,
            &msg("a", None, "user", r#""q""#),
        ]);
        parse(file.path()).expect_err("should be unloadable");
    }

    #[test]
    fn non_object_first_line_is_dropped_and_header_loads() {
        // The reader keeps only JSON objects, so a leading array is dropped and
        // the first valid JSON *object* becomes the header-bearing logical line.
        let file = write_session(&[
            r#"[1, 2, 3]"#, // dropped by reader → values[0] is the header
            HEADER,
            &msg("a", None, "user", r#""q""#),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "s1");
    }

    #[test]
    fn empty_file_falls_back_to_filename_and_parent_dir() {
        let file = write_session(&[]);
        let session = parse(file.path()).expect("parse");
        assert!(session.messages.is_empty());
        assert_eq!(session.session_id, fallback_id(file.path()));
        assert_eq!(session.cwd, fallback_cwd(file.path()));
        assert_eq!(session.start_timestamp, None);
        assert_eq!(session.summary, "(no summary)");
    }

    #[test]
    fn nonempty_all_malformed_file_returns_err() {
        // The file has real content but no parseable JSON object, so the reader
        // yields no values. This is a nonempty headerless file → Err, not the
        // empty-file fallback.
        let file = write_session(&["totally not json", "neither is this {{"]);
        let err = parse(file.path()).expect_err("all-malformed should be unloadable");
        assert!(
            err.to_string().contains("not a valid session header"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn whitespace_only_file_falls_back() {
        // A file with only blank lines is logically empty (no real content) →
        // empty-file fallback, not an error.
        let file = write_session(&["", "   ", "\t"]);
        let session = parse(file.path()).expect("parse");
        assert!(session.messages.is_empty());
        assert_eq!(session.session_id, fallback_id(file.path()));
        assert_eq!(session.cwd, fallback_cwd(file.path()));
    }

    #[test]
    fn valid_header_with_empty_cwd_uses_empty_cwd_verbatim() {
        // A valid header with an empty cwd (ancient v1) uses the cwd verbatim
        // rather than inventing a parent-dir fallback.
        let file = write_session(&[
            r#"{"type":"session","id":"s1","cwd":"","timestamp":"2026-01-01T00:00:00.000Z"}"#,
            &msg("a", None, "user", r#""x""#),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.cwd, PathBuf::new());
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn later_session_record_is_treated_as_entry_not_header() {
        // Only the first logical line is the header; a second `session`
        // record later in the file is just a (message-less) tree entry.
        let file = write_session(&[
            HEADER,
            &msg("a", None, "user", r#""q""#),
            r#"{"type":"session","id":"s2","parentId":"a","timestamp":"2026-01-01T00:00:02.000Z","cwd":"/other"}"#,
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.cwd, PathBuf::from("/tmp"));
        // The s2 record is on the active path (it is the last entry) but has no
        // nested `message`, so it projects nothing.
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].text, "q");
    }

    #[test]
    fn active_branch_excludes_dead_branch() {
        // a (user) then sibling branch b→c appended later; the active leaf c
        // selects the b→c branch and excludes the dead `a` branch.
        let file = write_session(&[
            HEADER,
            &msg("a", None, "user", r#""branch-a""#),
            &msg("b", None, "user", r#""branch-b""#),
            &msg("c", Some("b"), "assistant", r#"[{"type":"text","text":"c"}]"#),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["branch-b", "c"]);
    }

    #[test]
    fn only_user_and_assistant_roles_are_projected() {
        // toolResult / custom / developer roles sit on the active path but
        // must not appear in the projected messages.
        let file = write_session(&[
            HEADER,
            &msg("m1", None, "user", r#""q""#),
            &msg("m2", Some("m1"), "assistant", r#"[{"type":"text","text":"ans"}]"#),
            &msg("m3", Some("m2"), "toolResult", r#"[{"type":"text","text":"tool"}]"#),
            &msg("m4", Some("m3"), "custom", r#""custom""#),
            &msg("m5", Some("m4"), "developer", r#""dev""#),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text, "q");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].text, "ans");
    }

    #[test]
    fn non_message_entries_shape_tree_without_emitting() {
        // model_change and thinking_level_change entries are on the active
        // chain between the two messages; they link the path but project none.
        let file = write_session(&[
            HEADER,
            &msg("m1", None, "user", r#""hi""#),
            r#"{"type":"model_change","id":"c1","parentId":"m1","timestamp":"2026-01-01T00:00:02.000Z","provider":"local","modelId":"x"}"#,
            r#"{"type":"thinking_level_change","id":"c2","parentId":"c1","timestamp":"2026-01-01T00:00:03.000Z","thinkingLevel":"off"}"#,
            &msg("m2", Some("c2"), "assistant", r#"[{"type":"text","text":"hey"}]"#),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["hi", "hey"]);
    }

    #[test]
    fn encode_cwd_matches_real_pi_directory_names() {
        assert_eq!(encode_cwd(Path::new("/workspace/user")).unwrap(), "--workspace-user--");
        assert_eq!(
            encode_cwd(Path::new("/workspace/user/Projects/dotfiles")).unwrap(),
            "--workspace-user-Projects-dotfiles--"
        );
        // Special characters within a component are preserved verbatim; only
        // separators become dashes.
        assert_eq!(
            encode_cwd(Path::new("/workspace/user/Projects/llama.cpp")).unwrap(),
            "--workspace-user-Projects-llama.cpp--"
        );
        assert_eq!(
            encode_cwd(Path::new("/workspace/user/Projects/parth-generic-v1/client_prover")).unwrap(),
            "--workspace-user-Projects-parth-generic-v1-client_prover--"
        );
        assert_eq!(encode_cwd(Path::new("/tmp")).unwrap(), "--tmp--");
    }

    #[test]
    fn encode_cwd_drops_leading_root_and_handles_trailing_slash() {
        assert_eq!(encode_cwd(Path::new("/")).unwrap(), "----");
        assert_eq!(
            encode_cwd(Path::new("/workspace/user/Projects/dotfiles/")).unwrap(),
            "--workspace-user-Projects-dotfiles--"
        );
    }

    #[test]
    fn encode_cwd_normalizes_parent_and_current_components() {
        // `..` is folded against the last normal component, never emitted.
        assert_eq!(encode_cwd(Path::new("/a/b/../c")).unwrap(), "--a-c--");
        assert_eq!(encode_cwd(Path::new("/a/./b/c")).unwrap(), "--a-b-c--");
        // ParentDir past the root is a no-op for an absolute path.
        assert_eq!(encode_cwd(Path::new("/a/../../..")).unwrap(), "----");
        assert_eq!(encode_cwd(Path::new("/..")).unwrap(), "----");
    }

    #[test]
    fn encode_cwd_output_never_contains_dotdot() {
        for cwd in [
            "/a/../b",
            "/a/b/../../..",
            "/../..",
            "/workspace/user/../user/Projects",
        ] {
            let encoded = encode_cwd(Path::new(cwd)).expect("absolute encodes");
            assert!(
                !encoded.contains(".."),
                "encoded {encoded:?} for {cwd} must not contain `..`"
            );
        }
    }

    #[test]
    fn encode_cwd_relative_absolutizes_against_process_cwd() {
        // A relative input is absolutized against the process cwd, so it
        // encodes identically to its joined-absolute form and stays absolute.
        let here = std::env::current_dir().expect("current_dir");
        let relative = Path::new("foo/bar");
        assert_eq!(
            encode_cwd(relative).unwrap(),
            encode_cwd(&here.join(relative)).unwrap()
        );
        let encoded = encode_cwd(relative).unwrap();
        assert!(encoded.starts_with("--"));
        assert!(encoded.ends_with("--"));
        assert!(!encoded.contains(".."));
    }
}