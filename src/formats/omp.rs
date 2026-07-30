//! OMP session adapter.
//!
//! OMP shares Pi's append-only JSONL conversation tree, adding a fixed-width
//! leading title-slot record and home/tmp-relative session directory encoding.
//! The first logical record of the body must be the native `session` header;
//! legacy v1 entries are migrated to a linear tree before active-branch
//! resolution; the latest compaction bounds projected user/assistant text.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::domain::{Message, Session, SourceTool};
use crate::formats::tree::TreeNode;
use crate::formats::{normalize, read_jsonl_values, summarize_messages, tree};

/// Why an OMP session file is unloadable, surfaced through `parse`'s
/// `anyhow::Result` (downcastable via `error.downcast_ref::<OmpParseError>()`)
/// so the listing layer can tell a truly empty file from a nonempty
/// headerless/corrupt one without string-matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OmpParseError {
    /// The file is physically empty (0 bytes) — an empty draft, skipped but
    /// not flagged corrupt.
    #[error("OMP session file is empty")]
    Empty,
    /// A nonempty file with no parseable `session` header (all lines
    /// malformed, first logical record not a header, or a header missing a
    /// string id).
    #[error("OMP session is unloadable: no session header in nonempty file")]
    NoSessionHeader,
}

/// Parse an OMP session export into a lossy `Session`.
pub fn parse(path: &Path) -> Result<Session> {
    let mut values = read_jsonl_values(path)?;
    let modified_epoch = file_mtime(path);

    // Native OMP session files begin with a fixed-width title-slot record:
    //   {"type":"title","v":1,"title":"…","updatedAt":"…","pad":"…"[,"source":"auto"|"user"]}
    // (`src/session/title-slot.ts::parseTitleSlotObject`). The slot is split off
    // before the JSONL body is parsed; its non-empty title overrides the
    // `session` header title, and an empty slot title deletes it. The session
    // header must then be the first logical record of the remaining body.
    let mut start = 0;
    let mut slot_title: Option<String> = None;
    let mut slot_present = false;
    if let Some(object) = values.first().and_then(Value::as_object) {
        if object.get("type").and_then(Value::as_str) == Some("title")
            && object.get("v").and_then(Value::as_i64) == Some(1)
            && object.get("title").and_then(Value::as_str).is_some()
        {
            // `updatedAt` and `pad` are required strings in the native schema;
            // validate them so an unrelated `{"type":"title"}` object is not
            // mistaken for a slot.
            let valid = object.get("updatedAt").and_then(Value::as_str).is_some()
                && object.get("pad").and_then(Value::as_str).is_some()
                && object
                    .get("source")
                    .is_none_or(|src| src.as_str().is_some_and(|s| s == "auto" || s == "user"));
            if valid {
                slot_present = true;
                slot_title = object
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned);
                start = 1;
            }
        }
    }

    // Strict load (native OMP `loadEntriesFromFile`): the first logical
    // record MUST be a valid `session` header with a string id. An invalid
    // header is unloadable; the typed error distinguishes a truly empty file
    // from a nonempty headerless or corrupt one without scanning later lines.
    //
    // `read_jsonl_values` silently drops malformed lines, so an empty `Vec`
    // alone is ambiguous: it can mean a 0-byte file or a nonempty file whose
    // every line is malformed. The physical file size disambiguates.
    let (session_id, raw_cwd, raw_timestamp, header_title) = values
        .get(start)
        .and_then(Value::as_object)
        .filter(|object| object.get("type").and_then(Value::as_str) == Some("session"))
        .and_then(|object| {
            object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| {
                    (
                        id.to_owned(),
                        object
                            .get("cwd")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        object
                            .get("timestamp")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        object
                            .get("title")
                            .and_then(Value::as_str)
                            .filter(|title| !title.is_empty())
                            .map(str::to_owned),
                    )
                })
        })
        .ok_or_else(|| {
            anyhow::Error::new(if file_is_empty(path).unwrap_or(false) {
                OmpParseError::Empty
            } else {
                OmpParseError::NoSessionHeader
            })
        })?;

    // Native `applyTitleSlot`: a present slot title overrides the header title;
    // a present but empty slot title deletes the header title. Absent slot
    // leaves the header title in place.
    let effective_title = if slot_present {
        slot_title
    } else {
        header_title
    };

    crate::formats::pi::migrate_legacy_entries(&mut values[start..]);

    // Remaining records form the append-only tree. Every entry type participates
    // via id/parentId; only native user/assistant message payloads project.
    let mut nodes: Vec<TreeNode<'_>> = Vec::with_capacity(values.len().saturating_sub(start + 1));
    for record in &values[start + 1..] {
        let Some(object) = record.as_object() else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) == Some("session") {
            continue;
        }
        let id = object.get("id").and_then(Value::as_str).unwrap_or("");
        let parent_id = object.get("parentId").and_then(Value::as_str);
        let entry_timestamp = object.get("timestamp").and_then(Value::as_str);
        let entry_type = object.get("type").and_then(Value::as_str);
        let (role, content) = message_payload(object);
        nodes.push(TreeNode {
            id,
            parent_id: parent_id.filter(|value| !value.is_empty()),
            entry_type,
            role,
            content,
            timestamp: entry_timestamp,
            summary: object.get("summary").and_then(Value::as_str),
            short_summary: object.get("shortSummary").and_then(Value::as_str),
            first_kept_entry_id: object.get("firstKeptEntryId").and_then(Value::as_str),
        });
    }

    let active_path = tree::active_path(&nodes);
    let messages: Vec<Message> = tree::project_native_messages(&active_path);
    let compaction_title = active_path
        .iter()
        .filter(|node| node.entry_type == Some("compaction"))
        .filter_map(|node| node.short_summary)
        .next_back();

    let cwd = PathBuf::from(raw_cwd);
    let start_timestamp = if raw_timestamp.is_empty() {
        None
    } else {
        Some(raw_timestamp)
    };

    // Summary priority: effective title slot/header title, latest compaction
    // short summary, then first projected user/assistant text.
    let summary = effective_title
        .as_deref()
        .or(compaction_title)
        .map(|title| normalize(title, 100))
        .unwrap_or_else(|| summarize_messages(&messages));

    Ok(Session {
        tool: SourceTool::Omp,
        session_id,
        cwd,
        start_timestamp,
        summary,
        messages,
        path: path.to_path_buf(),
        modified_epoch,
    })
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

/// Whether the file is physically empty (0 bytes), when statable.
///
/// Disambiguates a truly empty file from a nonempty file whose lines were
/// all dropped by the lenient JSONL reader — both yield an empty record vec,
/// but only the former is a 0-byte draft.
fn file_is_empty(path: &Path) -> Option<bool> {
    Some(std::fs::metadata(path).ok()?.len() == 0)
}

/// Encode a working directory into OMP's per-cwd session directory name.
///
/// Resolves `cwd` (and `$HOME`/`$TMPDIR`) symlink-awarely when the paths
/// exist, then applies OMP's `P51` rule: under `$HOME` → `-` or
/// `-<home-relative>`; under `$TMPDIR` → `-tmp` or `-tmp-<tmp-relative>`;
/// otherwise the absolute `--<encoded>--` form shared with Pi.
pub fn encode_omp_cwd(cwd: &Path) -> String {
    let home = std::env::var_os("HOME")
        .or({
            #[cfg(windows)]
            {
                std::env::var_os("USERPROFILE")
            }
            #[cfg(not(windows))]
            {
                None
            }
        })
        .map(PathBuf::from)
        .unwrap_or_default();
    let tmp = std::env::temp_dir();
    encode_omp_cwd_with(cwd, &home, &tmp)
}

/// Encode a working directory with explicit home/tmp roots (testable core).
///
/// Canonicalization degrades gracefully to the literal path when a path does
/// not exist, so synthetic roots work without filesystem preparation.
pub fn encode_omp_cwd_with(cwd: &Path, home: &Path, tmp: &Path) -> String {
    let resolved = canonicalish(cwd);
    let home = canonicalish(home);
    let tmp = canonicalish(tmp);

    if !home.as_os_str().is_empty() {
        if let Ok(rel) = resolved.strip_prefix(&home) {
            return encode_relative("-", rel);
        }
    }
    if !tmp.as_os_str().is_empty() {
        if let Ok(rel) = resolved.strip_prefix(&tmp) {
            return encode_relative("-tmp", rel);
        }
    }
    encode_absolute(&resolved)
}

/// Classify an encoded OMP session directory name, accepting legacy forms.
///
/// Discovery must not reject pre-migration `--<home-encoded>--` directories,
/// so both the modern home/tmp-relative forms and the legacy/absolute
/// `--…--` form are recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmpDirKind {
    Home,
    Tmp,
    Absolute,
    Unknown,
}

pub fn classify_omp_dir(name: &str) -> OmpDirKind {
    if name.starts_with("--") && name.ends_with("--") && name.len() >= 4 {
        OmpDirKind::Absolute
    } else if name == "-tmp" || name.starts_with("-tmp-") {
        OmpDirKind::Tmp
    } else if name == "-" || (name.starts_with('-') && !name.starts_with("--")) {
        OmpDirKind::Home
    } else {
        OmpDirKind::Unknown
    }
}

fn canonicalish(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Home/tmp-relative encoder mirroring OMP's `x51(prefix, rel)`: replace
/// `/`, `\`, `:` with `-`; if empty, yield the prefix; else join with a `-`
/// unless the prefix already ends in `-` (the home prefix `-` does).
fn encode_relative(prefix: &str, rel: &Path) -> String {
    let encoded: String = rel.to_string_lossy().replace(['/', '\\', ':'], "-");
    if encoded.is_empty() {
        prefix.to_owned()
    } else if prefix.ends_with('-') {
        format!("{prefix}{encoded}")
    } else {
        format!("{prefix}-{encoded}")
    }
}

/// Absolute encoder mirroring OMP's `bkn(cwd)`: strip one leading separator,
/// replace `/`, `\`, `:` with `-`, wrap in `--…--`.
fn encode_absolute(resolved: &Path) -> String {
    let s = resolved.to_string_lossy();
    let stripped = if s.starts_with('/') || s.starts_with('\\') {
        &s[1..]
    } else {
        s.as_ref()
    };
    let inner = stripped.replace(['/', '\\', ':'], "-");
    format!("--{inner}--")
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

    const HEADER: &str = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#;

    fn msg(id: &str, parent_id: Option<&str>, role: &str, content: &str) -> String {
        let parent = match parent_id {
            Some(value) => format!("\"{value}\""),
            None => "null".to_owned(),
        };
        format!(
            r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2026-01-01T00:00:0{id}.000Z","message":{{"role":"{role}","content":{content}}}}}"#
        )
    }

    /// Downcast a parse error to its typed `OmpParseError` variant, if any.
    fn unloadable_kind(result: Result<Session>) -> Option<OmpParseError> {
        result
            .err()
            .and_then(|err| err.downcast_ref::<OmpParseError>().copied())
    }
    #[test]
    fn leading_title_slot_is_native_and_overrides_header_title() {
        let header = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp","title":"Header Title"}"#;
        let file = write_session(&[
            r#"{"type":"title","v":1,"title":"Slot Title","updatedAt":"2026-01-01T00:00:00.000Z","pad":""}"#,
            header,
            &msg("a", None, "user", r#""first user text""#),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.summary, "Slot Title");
    }

    #[test]
    fn empty_title_slot_deletes_header_title() {
        let header = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp","title":"Header Title"}"#;
        let file = write_session(&[
            r#"{"type":"title","v":1,"title":"","updatedAt":"2026-01-01T00:00:00.000Z","pad":""}"#,
            header,
            &msg("a", None, "user", r#""first user text""#),
        ]);
        let session = parse(file.path()).expect("parse");
        // No slot/header title and no compaction: summary falls back to text.
        assert_eq!(session.summary, "first user text");
    }

    #[test]
    fn non_title_typed_first_record_is_rejected() {
        // An unrelated leading {"type":"title"} object without the required
        // updatedAt/pad fields is not a native title slot, so it must not be
        // consumed and the missing session header stays unloadable.
        let file = write_session(&[
            r#"{"type":"title","v":1,"title":"X"}"#,
            &msg("a", None, "user", r#""first user text""#),
        ]);
        assert_eq!(
            unloadable_kind(parse(file.path())),
            Some(OmpParseError::NoSessionHeader)
        );
    }

    #[test]
    fn header_title_used_when_no_slot() {
        let header = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp","title":"Header Title Here"}"#;
        let file = write_session(&[header, &msg("a", None, "user", r#""ignored for summary""#)]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "Header Title Here");
    }

    #[test]
    fn active_branch_excludes_dead_branch() {
        // a then sibling branch b→c appended later; the active leaf c selects
        // the b→c branch and excludes the dead `a` branch.
        let file = write_session(&[
            HEADER,
            &msg("a", None, "user", r#""branch-a""#),
            &msg("b", None, "user", r#""branch-b""#),
            &msg(
                "c",
                Some("b"),
                "assistant",
                r#"[{"type":"text","text":"c"}]"#,
            ),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["branch-b", "c"]);
    }

    #[test]
    fn active_branch_cycle_guard_terminates() {
        // a.p = a (self-cycle) → walk stops, returning just [a].
        let file = write_session(&[HEADER, &msg("a", Some("a"), "user", r#""cyclic""#)]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].text, "cyclic");
    }

    #[test]
    fn malformed_lines_are_skipped() {
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
    fn unknown_entry_type_participates_in_tree() {
        // A future entry type sits between two messages on the active chain;
        // it links the path (via id/parentId) without projecting a message.
        let file = write_session(&[
            HEADER,
            &msg("a", None, "user", r#""hi""#),
            r#"{"type":"future_thing","id":"u1","parentId":"a","timestamp":"2026-01-01T00:00:02.000Z","weird":1}"#,
            &msg(
                "b",
                Some("u1"),
                "assistant",
                r#"[{"type":"text","text":"hey"}]"#,
            ),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["hi", "hey"]);
    }

    #[test]
    fn only_user_and_assistant_roles_project() {
        let file = write_session(&[
            HEADER,
            &msg("m1", None, "user", r#""q""#),
            &msg(
                "m2",
                Some("m1"),
                "assistant",
                r#"[{"type":"text","text":"ans"}]"#,
            ),
            &msg(
                "m3",
                Some("m2"),
                "toolResult",
                r#"[{"type":"text","text":"tool"}]"#,
            ),
            &msg("m4", Some("m3"), "developer", r#""dev""#),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[1].role, Role::Assistant);
    }

    #[test]
    fn outer_entry_timestamp_used_for_messages() {
        // Inner message.timestamp is a ms epoch number; the emitted timestamp
        // must be the outer entry ISO string.
        let line = r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-07-14T06:49:26.401Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1784011766355}}"#;
        let file = write_session(&[HEADER, line]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.messages.len(), 1);
        assert_eq!(
            session.messages[0].timestamp.as_deref(),
            Some("2026-07-14T06:49:26.401Z")
        );
    }

    #[test]
    fn compaction_drops_older_messages_and_uses_short_summary() {
        let file = write_session(&[
            HEADER,
            &msg("a", None, "user", r#""old""#),
            &msg(
                "b",
                Some("a"),
                "assistant",
                r#"[{"type":"text","text":"kept"}]"#,
            ),
            r#"{"type":"compaction","id":"c","parentId":"b","timestamp":"2026-01-01T00:00:03.000Z","summary":"full summary","shortSummary":"compact title","firstKeptEntryId":"b","tokensBefore":100}"#,
            &msg(
                "d",
                Some("c"),
                "assistant",
                r#"[{"type":"text","text":"done"}]"#,
            ),
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["kept", "done"]);
        assert_eq!(session.summary, "compact title");
    }

    #[test]
    fn version_one_entries_are_migrated_to_linear_tree() {
        let file = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"message","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        ]);
        let session = parse(file.path()).expect("parse");
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(texts, ["hi", "hello"]);
    }

    #[test]
    fn empty_file_is_unloadable_as_empty() {
        // A truly empty (0-byte) file is an empty draft, distinct from a
        // nonempty headerless file. Both unloadable, but typed differently.
        let file = write_session(&[]);
        assert_eq!(
            unloadable_kind(parse(file.path())),
            Some(OmpParseError::Empty)
        );
    }

    #[test]
    fn missing_header_is_unloadable_as_no_session_header() {
        // First logical record is a message, not a `session` header →
        // nonempty headerless, not empty.
        let file = write_session(&[&msg("a", None, "user", r#""only""#)]);
        assert_eq!(
            unloadable_kind(parse(file.path())),
            Some(OmpParseError::NoSessionHeader)
        );
    }

    #[test]
    fn nonempty_all_malformed_is_headerless_not_empty() {
        // A nonempty file whose every line is malformed yields an empty
        // record vec from the lenient reader — but it is NOT a 0-byte file,
        // so it must be classified as headerless, not empty.
        let file = write_session(&["this is not json", "{ also not valid", "   "]);
        assert_eq!(
            unloadable_kind(parse(file.path())),
            Some(OmpParseError::NoSessionHeader)
        );
    }

    #[test]
    fn header_only_file_is_listable() {
        // A header with no entries is a valid (resumable) draft session.
        let file = write_session(&[HEADER]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.session_id, "s1");
        assert!(session.messages.is_empty());
        assert_eq!(session.summary, "(no summary)");
    }

    #[test]
    fn encode_home_relative() {
        let home = Path::new("/workspace/user");
        let tmp = Path::new("/tmp");
        assert_eq!(
            encode_omp_cwd_with(Path::new("/workspace/user"), home, tmp),
            "-"
        );
        assert_eq!(
            encode_omp_cwd_with(Path::new("/workspace/user/Projects/x"), home, tmp),
            "-Projects-x"
        );
    }

    #[test]
    fn encode_tmp_relative() {
        let home = Path::new("/workspace/user");
        let tmp = Path::new("/tmp");
        assert_eq!(
            encode_omp_cwd_with(Path::new("/tmp/foo"), home, tmp),
            "-tmp-foo"
        );
        assert_eq!(encode_omp_cwd_with(Path::new("/tmp"), home, tmp), "-tmp");
    }

    #[test]
    fn encode_absolute_elsewhere() {
        let home = Path::new("/workspace/user");
        let tmp = Path::new("/tmp");
        assert_eq!(
            encode_omp_cwd_with(Path::new("/opt/workspace/memory"), home, tmp),
            "--opt-workspace-memory--"
        );
    }

    #[test]
    fn classify_accepts_modern_and_legacy_dirs() {
        // Modern home/tmp-relative forms.
        assert_eq!(classify_omp_dir("-"), OmpDirKind::Home);
        assert_eq!(classify_omp_dir("-workspace-project"), OmpDirKind::Home);
        assert_eq!(classify_omp_dir("-tmp"), OmpDirKind::Tmp);
        assert_eq!(classify_omp_dir("-tmp-foo"), OmpDirKind::Tmp);
        // Absolute (non-home) and legacy home `--…--` forms are accepted.
        assert_eq!(
            classify_omp_dir("--opt-workspace-memory--"),
            OmpDirKind::Absolute
        );
        assert_eq!(
            classify_omp_dir("--workspace-user-project--"),
            OmpDirKind::Absolute
        );
        assert_eq!(classify_omp_dir("random"), OmpDirKind::Unknown);
    }
}
