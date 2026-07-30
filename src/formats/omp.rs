//! OMP session adapter.
//!
//! OMP (`@oh-my-pi/pi-coding-agent`) shares Pi's append-only JSONL tree
//! format — a `session` header followed by entries chained via `id`/
//! `parentId` — but diverges in two places this adapter honors:
//!
//! 1. An optional fixed-width **title slot** may occupy the first physical
//!    line: `{"type":"title","v":1,"title":…,"source?":"auto"|"user",
//!    "updatedAt":…,"pad":…}`, padded to 256 bytes. When present and valid
//!    its title is peeled and takes precedence over the header title for the
//!    session summary; otherwise the line is parsed as an ordinary record.
//! 2. The per-cwd encoded directory name uses OMP's home/tmp-relative scheme
//!    (`-`, `-Projects-x`, `-tmp-foo`) for paths under `$HOME`/`$TMPDIR`,
//!    falling back to the absolute `--…--` form elsewhere. Legacy `--…--`
//!    home directories (pre-migration) are accepted on discovery, not
//!    rejected.
//!
//! Parsing is intentionally lossy and lenient, matching the `al` contract:
//! malformed JSONL lines are skipped, the active branch is reconstructed from
//! the last-appended entry, and only `user`/`assistant` first text along that
//! branch is projected — using the outer entry timestamp, not the inner
//! message epoch. Unknown entry types still participate in the tree through
//! their `id`/`parentId` so the active-branch walk stays correct.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use serde_json::{Map, Value};

use crate::domain::{Message, Session, SourceTool};
use crate::formats::{normalize, read_jsonl_values, summarize_messages, tree};
use crate::formats::tree::TreeNode;

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
    let values = read_jsonl_values(path)?;
    let modified_epoch = file_mtime(path);

    // The optional title slot is the first physical record. Peel it only when
    // it validates as a v1 title slot; a title-typed record that fails
    // validation stays as the first logical record, which then fails the
    // strict session-header requirement below (unloadable).
    let mut slot_title: Option<String> = None;
    let mut start = 0;
    if let Some(first) = values.first().and_then(Value::as_object) {
        if first.get("type").and_then(Value::as_str) == Some("title") {
            if let Some(slot) = parse_title_slot(first) {
                slot_title = Some(slot.title);
                start = 1;
            }
        }
    }

    // Strict load (native OMP `loadEntriesFromFile`): the first logical
    // record after the optional title slot MUST be a valid `session` header
    // with a string id. A missing or invalid header makes the file
    // unloadable — surfaced as a typed `OmpParseError` so the listing layer
    // can distinguish a truly empty file from a nonempty headerless/corrupt
    // one, never papered over by scanning for a later header.
    //
    // `read_jsonl_values` silently drops malformed lines, so an empty `Vec`
    // alone is ambiguous: it can mean a 0-byte file or a nonempty file whose
    // every line is malformed. The physical file size disambiguates.
    let header_object = values
        .get(start)
        .and_then(Value::as_object)
        .filter(|object| object.get("type").and_then(Value::as_str) == Some("session"));
    let header_object = match header_object {
        Some(object) => object,
        None => {
            let err = if file_is_empty(path).unwrap_or(false) {
                OmpParseError::Empty
            } else {
                OmpParseError::NoSessionHeader
            };
            return Err(anyhow::Error::new(err));
        }
    };
    let Some(session_id) = header_object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        // A `session`-typed record with no string id: the file is nonempty
        // but its header is invalid → headerless, not empty.
        return Err(anyhow::Error::new(OmpParseError::NoSessionHeader));
    };
    let session_id = session_id.to_owned();
    let raw_cwd = header_object
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let raw_timestamp = header_object
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let header_title = header_object
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(str::to_owned);

    // Remaining records form the append-only tree. Every entry type —
    // `message`, `model_change`, `compaction`, unknown future types —
    // participates via id/parentId so the active-branch walk stays correct;
    // only `message` entries carry a conversation payload that projects.
    // Further `session` records are not entries and are skipped.
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

    let cwd = if raw_cwd.is_empty() {
        fallback_cwd(path)
    } else {
        PathBuf::from(raw_cwd)
    };
    let start_timestamp = if raw_timestamp.is_empty() {
        None
    } else {
        Some(raw_timestamp)
    };

    // Summary priority: title slot > header title > first user/assistant text.
    let summary = slot_title
        .as_deref()
        .or(header_title.as_deref())
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

/// A peeled OMP title slot, preserving all extension data via `raw`.
///
/// Validation mirrors OMP's `parseTitleSlotObject`: `type == "title"`,
/// `v == 1`, string `title`, string `updatedAt`, string `pad`, and an
/// optional `source` that must be `"auto"` or `"user"` when present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleSlot {
    pub title: String,
    #[serde(default)]
    pub source: Option<String>,
    pub updated_at: String,
    pub pad: String,
    /// The full record, kept verbatim for round-trip fidelity of any
    /// future/extension fields beyond the typed ones above.
    #[serde(skip)]
    pub raw: Value,
}

/// Validate and peel a title slot from a first-record object.
///
/// Returns `None` when the object does not validate as a title slot, in which
/// case the caller treats the record as an ordinary entry.
pub fn parse_title_slot(object: &Map<String, Value>) -> Option<TitleSlot> {
    let kind = object.get("type").and_then(Value::as_str)?;
    if kind != "title" {
        return None;
    }
    let v = object.get("v").and_then(Value::as_i64)?;
    if v != 1 {
        return None;
    }
    let title = object.get("title").and_then(Value::as_str)?.to_owned();
    let updated_at = object.get("updatedAt").and_then(Value::as_str)?.to_owned();
    let pad = object.get("pad").and_then(Value::as_str)?.to_owned();
    if let Some(source) = object.get("source").and_then(Value::as_str) {
        if source != "auto" && source != "user" {
            return None;
        }
    }
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(TitleSlot {
        title,
        source,
        updated_at,
        pad,
        raw: Value::Object(object.clone()),
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

/// Derive the working directory from the file's parent when no header cwd is
/// present (or it is empty).
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

    /// Build a 256-byte title-slot line (incl. trailing newline).
    fn title_slot_line(title: &str, source: &str) -> String {
        let core = format!(
            r#"{{"type":"title","v":1,"title":"{title}","source":"{source}","updatedAt":"2026-01-01T00:00:00.000Z","pad":""#
        );
        // total line incl. trailing newline == 256 bytes; pad fills the gap.
        let closer = "\"}";
        let without_pad = core.len() + closer.len();
        let pad_len = 256usize.saturating_sub(without_pad + 1);
        format!("{}{}{}\n", core, " ".repeat(pad_len), closer)
    }

    /// Downcast a parse error to its typed `OmpParseError` variant, if any.
    fn unloadable_kind(result: Result<Session>) -> Option<OmpParseError> {
        result
            .err()
            .and_then(|err| err.downcast_ref::<OmpParseError>().copied())
    }

    #[test]
    fn title_slot_is_peeled_and_wins_summary() {
        let slot = title_slot_line("Extract archive documents", "auto");
        assert_eq!(slot.len(), 256);
        let file = write_session(&[
            &slot,
            HEADER,
            &msg("a", None, "user", r#""first user text""#),
            &msg("b", Some("a"), "assistant", r#"[{"type":"text","text":"reply"}]"#),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.tool, SourceTool::Omp);
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.summary, "Extract archive documents");
        // Messages still project from the active branch.
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].text, "first user text");
        assert_eq!(session.messages[1].text, "reply");
    }

    #[test]
    fn header_title_used_when_no_slot() {
        let header = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp","title":"Header Title Here"}"#;
        let file = write_session(&[
            header,
            &msg("a", None, "user", r#""ignored for summary""#),
        ]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "Header Title Here");
    }

    #[test]
    fn slot_title_takes_precedence_over_header_title() {
        let slot = title_slot_line("Slot Title", "user");
        let header = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp","title":"Header Title"}"#;
        let file = write_session(&[&slot, header, &msg("a", None, "user", r#""x""#)]);
        let session = parse(file.path()).expect("parse");
        assert_eq!(session.summary, "Slot Title");
    }
    #[test]
    fn invalid_title_slot_makes_file_unloadable() {
        // type=="title" but missing `pad` → not a valid slot, so it stays as
        // the first logical record. Since it is not a `session` header the
        // file is unloadable (native returns []). The valid header on the
        // next line is NOT used as a fallback.
        let bad_slot = r#"{"type":"title","v":1,"title":"No Pad","updatedAt":"2026-01-01T00:00:00.000Z"}"#;
        let file = write_session(&[
            bad_slot,
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp","title":"Header Title"}"#,
            &msg("a", None, "user", r#""q""#),
        ]);
        assert!(parse(file.path()).is_err());
    }

    #[test]
    fn invalid_slot_source_makes_file_unloadable() {
        // `source` present but not auto/user → slot rejected, so the title
        // record is the first logical (non-session) entry → unloadable.
        let bad = r#"{"type":"title","v":1,"title":"Bad","source":"weird","updatedAt":"2026-01-01T00:00:00.000Z","pad":""}"#;
        let file = write_session(&[
            bad,
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp","title":"Header"}"#,
            &msg("a", None, "user", r#""q""#),
        ]);
        assert!(parse(file.path()).is_err());
    }

    #[test]
    fn active_branch_excludes_dead_branch() {
        // a then sibling branch b→c appended later; the active leaf c selects
        // the b→c branch and excludes the dead `a` branch.
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
    fn active_branch_cycle_guard_terminates() {
        // a.p = a (self-cycle) → walk stops, returning just [a].
        let file = write_session(&[
            HEADER,
            &msg("a", Some("a"), "user", r#""cyclic""#),
        ]);
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
            &msg("b", Some("u1"), "assistant", r#"[{"type":"text","text":"hey"}]"#),
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
            &msg("m2", Some("m1"), "assistant", r#"[{"type":"text","text":"ans"}]"#),
            &msg("m3", Some("m2"), "toolResult", r#"[{"type":"text","text":"tool"}]"#),
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
        assert_eq!(encode_omp_cwd_with(Path::new("/workspace/user"), home, tmp), "-");
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