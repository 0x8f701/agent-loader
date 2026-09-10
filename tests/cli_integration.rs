#![cfg(feature = "cli")]
//! Standalone binary-level integration tests for the `al` CLI.
//!
//! The suite drives the compiled binary against isolated temporary homes. It
//! covers list/search dispatch plus same-format legacy launch migration: OMP
//! open/fork and Codex open must launch newly emitted native copies while the
//! original source bytes remain unchanged, and print-only/native launches must
//! not materialize extra sessions.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

/// Path to the compiled `al` binary, resolved at compile time by cargo.
const AL: &str = env!("CARGO_BIN_EXE_al");

/// Build a native Pi v3 `session` header record.
fn header(session_id: &str, cwd: &str) -> String {
    format!(
        r#"{{"type":"session","version":3,"id":"{session_id}","timestamp":"2026-07-30T12:00:00.000Z","cwd":"{cwd}"}}"#,
    )
}

/// Build a native Pi v3 `message` entry. `parent` is `None` for the root
/// entry. The `text` must be free of quotes/backslashes/control characters so
/// the embedded JSON string literal stays valid without an escape layer.
fn message(id: &str, parent: Option<&str>, role: &str, text: &str) -> String {
    let parent = match parent {
        Some(value) => format!("\"{value}\""),
        None => "null".to_owned(),
    };
    format!(
        r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2026-07-30T12:00:00.000Z","message":{{"role":"{role}","content":"{text}"}}}}"#,
    )
}

/// Write a native Pi v3 session file one directory below the Pi sessions root
/// (`HOME/.pi/agent/sessions/<dir>/<file>.jsonl`) and return its absolute path.
///
/// The single intermediate directory matches the on-disk grammar enforced by
/// `is_tree_top_level_session` (`root/<project>/<session>.jsonl`); the file
/// name carries the native `timestamp_uuid.jsonl` shape.
fn write_pi_session(home: &Path, dir: &str, file: &str, lines: &[String]) -> PathBuf {
    write_jsonl_session(home, ".pi/agent/sessions", dir, file, lines)
}

fn write_rpi_session(home: &Path, dir: &str, file: &str, lines: &[String]) -> PathBuf {
    write_jsonl_session(home, ".rpi/sessions", dir, file, lines)
}

fn write_jsonl_session(
    home: &Path,
    root: &str,
    dir: &str,
    file: &str,
    lines: &[String],
) -> PathBuf {
    let session_dir = home.join(root).join(dir);
    fs::create_dir_all(&session_dir).unwrap_or_else(|e| panic!("creating session dir: {e}"));
    let path = session_dir.join(file);
    let mut file_handle =
        fs::File::create(&path).unwrap_or_else(|e| panic!("creating session file: {e}"));
    for line in lines {
        writeln!(file_handle, "{line}").unwrap_or_else(|e| panic!("writing session line: {e}"));
    }
    file_handle
        .flush()
        .unwrap_or_else(|e| panic!("flushing session file: {e}"));
    path
}

/// Run `al` with an isolated `HOME` and `SESSIONS_HOME`/`GROK_HOME`/`NO_COLOR`
/// removed, so the temp HOME is authoritative and no real environment leaks.
fn run(home: &Path, args: &[&str]) -> Output {
    run_with_env(home, args, &[])
}

fn run_with_env(home: &Path, args: &[&str], variables: &[(&str, &OsStr)]) -> Output {
    let mut command = Command::new(AL);
    command
        .args(args)
        .env("HOME", home)
        .env_remove("SESSIONS_HOME")
        .env_remove("GROK_HOME")
        .env_remove("NO_COLOR")
        .env_remove("AL_PROJECTS_HOME");
    for (name, value) in variables {
        command.env(name, value);
    }
    command
        .output()
        .unwrap_or_else(|e| panic!("spawning al: {e}"))
}

fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().expect("fixture parent")).unwrap();
    let mut file = fs::File::create(path).unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
    file.flush().unwrap();
}

#[cfg(unix)]
fn write_fake_tool(bin: &Path, name: &str, script: &str) {
    fs::create_dir_all(bin).unwrap();
    let path = bin.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn write_ssh_stub(bin: &Path) -> PathBuf {
    let invocations = bin
        .parent()
        .expect("stub bin parent")
        .join("ssh-invocations");
    write_fake_tool(
        bin,
        "ssh",
        r#"#!/bin/sh
printf '%s\n' "$#" >> "$SSH_INVOCATIONS"
for argument in "$@"; do
  printf '<%s>\n' "$argument" >> "$SSH_INVOCATIONS"
done
host=$6
case "$host" in
  host-a) printf 'remote-a\n' ;;
  host-b) printf 'remote-b\n' ;;
  host-c) printf 'private session body\n' >&2; exit 23 ;;
  *) printf 'literal:%s\n' "$host" ;;
esac
"#,
    );
    invocations
}

fn collect_jsonl(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_jsonl(&path, output);
        } else if path.extension() == Some(OsStr::new("jsonl")) {
            output.push(path);
        }
    }
}

fn first_json_id(path: &Path, pointer: &str) -> String {
    // OMP files begin with a native title-slot record (no `id`); scan for the
    // first line whose JSON object exposes the requested pointer.
    let contents = fs::read_to_string(path).unwrap();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(id) = value.pointer(pointer).and_then(Value::as_str) {
            return id.to_owned();
        }
    }
    panic!("no record with pointer {pointer} in {}", path.display());
}

fn write_omp_fixture(home: &Path, id: &str, legacy: bool) -> PathBuf {
    let path = home
        .join(".omp/agent/sessions/--workspace-project--")
        .join(format!("2026-07-30T12-00-00_{id}.jsonl"));
    let header = if legacy {
        format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-07-30T12:00:00.000Z","cwd":"{}","titleSource":"converted","convertedFrom":"pi"}}"#,
            home.display()
        )
    } else {
        format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-07-30T12:00:00.000Z","cwd":"{}","title":"native"}}"#,
            home.display()
        )
    };
    let model = if legacy {
        r#"{"type":"model_change","id":"model","parentId":null,"provider":"sessions-convert","modelId":"converted-from-pi"}"#.to_owned()
    } else {
        r#"{"type":"model_change","id":"model","parentId":null,"model":"openai/gpt-5"}"#.to_owned()
    };
    write_lines(
        &path,
        &[
            header,
            model,
            r#"{"type":"message","id":"user","parentId":"model","timestamp":"2026-07-30T12:00:01.000Z","message":{"role":"user","content":"hello"}}"#.to_owned(),
            r#"{"type":"message","id":"assistant","parentId":"user","timestamp":"2026-07-30T12:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#.to_owned(),
            r#"{"type":"appended_marker","id":"marker","parentId":"assistant","value":"keep-on-original"}"#.to_owned(),
        ],
    );
    path
}

fn write_codex_fixture(home: &Path, id: &str, legacy: bool) -> PathBuf {
    let path = home
        .join(".codex/sessions/2026/07/30")
        .join(format!("rollout-2026-07-30T12-00-00-{id}.jsonl"));
    let provider = if legacy {
        String::new()
    } else {
        r#","model_provider":"openai""#.to_owned()
    };
    let mut lines = vec![format!(
        r#"{{"timestamp":"2026-07-30T12:00:00.000Z","type":"session_meta","payload":{{"id":"{id}","timestamp":"2026-07-30T12:00:00.000Z","cwd":"{}","originator":"codex-tui","cli_version":"sessions-convert"{provider}}}}}"#,
        home.display()
    )];
    if !legacy {
        lines.push(format!(r#"{{"timestamp":"2026-07-30T12:00:00.100Z","type":"turn_context","payload":{{"cwd":"{}","model":"gpt-5"}}}}"#, home.display()));
    }
    lines.extend([
        r#"{"timestamp":"2026-07-30T12:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#.to_owned(),
        r#"{"timestamp":"2026-07-30T12:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}"#.to_owned(),
        r#"{"timestamp":"2026-07-30T12:00:03.000Z","type":"appended_marker","payload":{"value":"keep-on-original"}}"#.to_owned(),
    ]);
    write_lines(&path, &lines);
    path
}

/// Assert `fields` is a `YYYY-MM-DD HH:MM:SS` local timestamp — the locale
/// shape of `format_epoch`, independent of timezone. Defends the field is a
/// real numeric timestamp, not garbage.
fn assert_local_timestamp(field: &str) {
    let (date, time) = field
        .split_once(' ')
        .unwrap_or_else(|| panic!("timestamp field is not 'date time': {field:?}"));
    let date_parts: Vec<&str> = date.split('-').collect();
    assert_eq!(
        date_parts.len(),
        3,
        "timestamp date is not YYYY-MM-DD: {date:?}"
    );
    for part in &date_parts {
        assert!(
            !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
            "timestamp date component is non-numeric: {part:?}"
        );
    }
    let time_parts: Vec<&str> = time.split(':').collect();
    assert_eq!(
        time_parts.len(),
        3,
        "timestamp time is not HH:MM:SS: {time:?}"
    );
    for part in &time_parts {
        assert!(
            !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
            "timestamp time component is non-numeric: {part:?}"
        );
    }
}

#[test]
fn list_paths_all_emits_five_field_ansi_free_tsv_with_exact_tool_id_path() {
    let home = TempDir::new().unwrap();
    let dir = "--home-pi-int--";
    let file = "2026-07-30T12-00-00-aaaaaaaa-0000-4000-8000-000000000001.jsonl";
    let session_id = "aaaaaaaa-0000-4000-8000-000000000001";
    let cwd = "/workspace/project";
    let lines = vec![
        header(session_id, cwd),
        message("m1", None, "user", "Set up the integration test fixture"),
        message("m2", Some("m1"), "assistant", "Done"),
    ];
    let path = write_pi_session(home.path(), dir, file, &lines);

    let output = run(home.path(), &["sessions", "list", "--paths", "--all"]);
    assert!(
        output.status.success(),
        "list --paths --all failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains('\x1b'),
        "--paths output must be ANSI-free, found ESC in: {stdout:?}",
    );

    // Exactly one session row (one fixture) -> exactly one non-empty line.
    let nonempty: Vec<&str> = stdout.split('\n').filter(|line| !line.is_empty()).collect();
    assert_eq!(
        nonempty.len(),
        1,
        "expected exactly one session line, got {nonempty:?}",
    );

    let fields: Vec<&str> = nonempty[0].split('\t').collect();
    assert_eq!(
        fields.len(),
        5,
        "--paths line must have exactly 5 TSV fields, got {}: {fields:?}",
        fields.len(),
    );
    assert_eq!(fields[0], "pi", "tool field must be 'pi'");
    assert_local_timestamp(fields[1]);
    assert_eq!(
        fields[2], session_id,
        "session id field must be the exact header id",
    );
    assert_eq!(
        fields[3], "Set up the integration test fixture",
        "summary field must be the exact first-user-message text",
    );
    assert_eq!(
        fields[4],
        &*path.to_string_lossy(),
        "path field must be the exact on-disk session path",
    );
}

#[test]
fn list_paths_all_hides_workflow_children_until_requested() {
    let home = TempDir::new().unwrap();
    let dir = "--workspace-project--";
    let cwd = "/workspace/project";
    let parent_id = "aaaaaaaa-0000-4000-8000-00000000000a";
    let child_id = "aaaaaaaa-0000-4000-8000-00000000000b";
    write_pi_session(
        home.path(),
        dir,
        "2026-07-30T12-00-00-aaaaaaaa-0000-4000-8000-00000000000a.jsonl",
        &[
            header(parent_id, cwd),
            message("m1", None, "user", "parent prompt"),
        ],
    );
    write_pi_session(
        home.path(),
        dir,
        "2026-07-30T12-00-01-aaaaaaaa-0000-4000-8000-00000000000b.jsonl",
        &[
            format!(
                r#"{{"type":"session","version":3,"id":"{child_id}","timestamp":"2026-07-30T12:00:01.000Z","cwd":"{cwd}","sessionKind":"workflowWorker"}}"#
            ),
            message("m1", None, "user", "child worker prompt"),
        ],
    );

    let hidden = run(home.path(), &["sessions", "list", "--paths", "--all"]);
    assert!(
        hidden.status.success(),
        "list --paths --all failed: {}",
        String::from_utf8_lossy(&hidden.stderr),
    );
    let hidden_stdout = String::from_utf8(hidden.stdout).unwrap();
    assert!(
        hidden_stdout.contains(parent_id),
        "parent must stay listed: {hidden_stdout}"
    );
    assert!(
        !hidden_stdout.contains(child_id),
        "--all must not reveal children: {hidden_stdout}"
    );

    let shown = run(
        home.path(),
        &["sessions", "list", "--paths", "--all", "--children"],
    );
    assert!(
        shown.status.success(),
        "list --children failed: {}",
        String::from_utf8_lossy(&shown.stderr),
    );
    let shown_stdout = String::from_utf8(shown.stdout).unwrap();
    assert!(
        shown_stdout.contains(parent_id) && shown_stdout.contains(child_id),
        "--children must list parent and worker: {shown_stdout}"
    );
}

#[test]
fn search_hides_workflow_children_until_requested() {
    let home = TempDir::new().unwrap();
    let dir = "--workspace-project--";
    let cwd = "/workspace/project";
    let parent_id = "aaaaaaaa-0000-4000-8000-00000000000c";
    let child_id = "aaaaaaaa-0000-4000-8000-00000000000d";
    write_pi_session(
        home.path(),
        dir,
        "2026-07-30T12-00-00-aaaaaaaa-0000-4000-8000-00000000000c.jsonl",
        &[
            header(parent_id, cwd),
            message("m1", None, "user", "shared unique token in parent"),
        ],
    );
    write_pi_session(
        home.path(),
        dir,
        "2026-07-30T12-00-01-aaaaaaaa-0000-4000-8000-00000000000d.jsonl",
        &[
            format!(
                r#"{{"type":"session","version":3,"id":"{child_id}","timestamp":"2026-07-30T12:00:01.000Z","cwd":"{cwd}","sessionKind":"workflowPlanner"}}"#
            ),
            message("m1", None, "user", "shared unique token in child"),
        ],
    );

    let hidden = run(home.path(), &["sessions", "search", "unique token"]);
    assert!(
        hidden.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&hidden.stderr),
    );
    let hidden_stdout = String::from_utf8(hidden.stdout).unwrap();
    assert!(
        hidden_stdout.contains(parent_id),
        "parent must match search: {hidden_stdout}"
    );
    assert!(
        !hidden_stdout.contains(child_id),
        "default search must hide children: {hidden_stdout}"
    );

    let shown = run(
        home.path(),
        &["sessions", "search", "--children", "unique token"],
    );
    assert!(
        shown.status.success(),
        "search --children failed: {}",
        String::from_utf8_lossy(&shown.stderr),
    );
    let shown_stdout = String::from_utf8(shown.stdout).unwrap();
    assert!(
        shown_stdout.contains(parent_id) && shown_stdout.contains(child_id),
        "--children search must include the worker: {shown_stdout}"
    );
}

#[test]
fn list_paths_zero_emits_nothing_and_exits_zero() {
    let home = TempDir::new().unwrap();
    // A real fixture is present so the empty output is attributable to the
    // count of zero truncating to nothing — not to an empty catalog. A bug
    // that ignored `count=0` would emit a row and fail this assertion.
    let dir = "--home-pi-int--";
    let file = "2026-07-30T12-00-00-bbbbbbbb-0000-4000-8000-000000000002.jsonl";
    let lines = vec![
        header("bbbbbbbb-0000-4000-8000-000000000002", "/tmp"),
        message("m1", None, "user", "anything at all"),
    ];
    write_pi_session(home.path(), dir, file, &lines);

    let output = run(home.path(), &["sessions", "list", "--paths", "0"]);
    assert_eq!(output.status.code(), Some(0), "list --paths 0 must exit 0",);
    assert!(
        output.stdout.is_empty(),
        "list --paths 0 must emit no stdout, got: {:?}",
        String::from_utf8_lossy(&output.stdout),
    );
}

#[cfg(unix)]
#[test]
fn explicit_session_hosts_preserve_order_local_and_forward_only_list_flags() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let invocations = write_ssh_stub(&bin);
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run_with_env(
        home.path(),
        &[
            "sessions", "list", "4", "--all", "--dedupe", "--host", "host-a", "--host", "local",
            "--host", "host-b",
        ],
        &[
            ("PATH", path.as_os_str()),
            ("SSH_INVOCATIONS", invocations.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "multi-host list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "== host-a ==\nremote-a\n== local ==\n== host-b ==\nremote-b\n"
    );
    assert_eq!(
        fs::read_to_string(invocations).unwrap(),
        "7\n<-o>\n<ConnectTimeout=10>\n<-o>\n<ConnectionAttempts=1>\n<-->\n<host-a>\n<exec 'al' 'sessions' 'list' '4' '--all' '--dedupe'>\n7\n<-o>\n<ConnectTimeout=10>\n<-o>\n<ConnectionAttempts=1>\n<-->\n<host-b>\n<exec 'al' 'sessions' 'list' '4' '--all' '--dedupe'>\n"
    );
}

#[cfg(unix)]
#[test]
fn session_host_metacharacters_are_one_literal_ssh_argument() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let invocations = write_ssh_stub(&bin);
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let host = "host-a;printf-injected";
    let output = run_with_env(
        home.path(),
        &["sessions", "--host", host],
        &[
            ("PATH", path.as_os_str()),
            ("SSH_INVOCATIONS", invocations.as_os_str()),
        ],
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "== host-a;printf-injected ==\nliteral:host-a;printf-injected\n"
    );
    assert_eq!(
        fs::read_to_string(invocations).unwrap(),
        "7\n<-o>\n<ConnectTimeout=10>\n<-o>\n<ConnectionAttempts=1>\n<-->\n<host-a;printf-injected>\n<exec 'al' 'sessions' 'list'>\n"
    );
}

#[cfg(unix)]
#[test]
fn session_hosts_continue_after_failure_return_one_and_hide_remote_stderr() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let invocations = write_ssh_stub(&bin);
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "list", "--host", "host-c", "--host", "host-b"],
        &[
            ("PATH", path.as_os_str()),
            ("SSH_INVOCATIONS", invocations.as_os_str()),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "== host-c ==\n== host-b ==\nremote-b\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("host-c"));
    assert!(stderr.contains("ssh exited with exit status: 23"));
    assert!(!stderr.contains("private session body"));
    assert!(
        fs::read_to_string(invocations)
            .unwrap()
            .contains("<host-b>")
    );
}

#[cfg(unix)]
#[test]
fn session_host_forbidden_combinations_fail_before_ssh() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let invocations = write_ssh_stub(&bin);
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    for prefix in [&["sessions"][..], &["sessions", "list"][..]] {
        for mode in ["--paths", "--picker", "--fzf"] {
            let mut args = prefix.to_vec();
            args.extend(["--host", "host-a", mode]);
            let output = run_with_env(
                home.path(),
                &args,
                &[
                    ("PATH", path.as_os_str()),
                    ("SSH_INVOCATIONS", invocations.as_os_str()),
                ],
            );
            assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        }
    }
    assert!(!invocations.exists(), "ssh must not run on parser errors");
}

#[test]
fn removed_sessions_all_public_spellings_fail_to_parse() {
    let home = TempDir::new().unwrap();
    for args in [&["sessions-all"][..], &["sessions", "all"][..]] {
        assert_eq!(run(home.path(), args).status.code(), Some(2));
    }
}

#[test]
fn removed_sks_public_spellings_fail_to_parse() {
    let home = TempDir::new().unwrap();
    assert_eq!(run(home.path(), &["sks"]).status.code(), Some(2));
    assert_eq!(run(home.path(), &["skss", "needle"]).status.code(), Some(2));
}

#[test]
fn search_uppercase_query_finds_lowercase_message_body_and_summary() {
    let home = TempDir::new().unwrap();
    let dir = "--home-pi-int--";

    // Matching session: the lowercase term "off" appears in the first user
    // message (which is also the projected summary) and in the assistant
    // reply. An uppercase query must match case-insensitively.
    let match_id = "cccccccc-0000-4000-8000-000000000003";
    let match_lines = vec![
        header(match_id, "/tmp/match"),
        message(
            "u1",
            None,
            "user",
            "reproduce the off by one bug in the parser",
        ),
        message(
            "a1",
            Some("u1"),
            "assistant",
            "the off by one error is in tokenize",
        ),
    ];
    write_pi_session(
        home.path(),
        dir,
        "2026-07-30T12-00-00-cccccccc-0000-4000-8000-000000000003.jsonl",
        &match_lines,
    );

    // Non-matching session: the term never appears. It must be filtered out
    // of the result, defending that search selects rather than listing all.
    let other_id = "dddddddd-0000-4000-8000-000000000004";
    let other_lines = vec![
        header(other_id, "/tmp/other"),
        message("u1", None, "user", "completely unrelated topic about cats"),
    ];
    write_pi_session(
        home.path(),
        dir,
        "2026-07-30T12-01-00-dddddddd-0000-4000-8000-000000000004.jsonl",
        &other_lines,
    );

    let output = run(home.path(), &["sessions", "search", "OFF"]);
    assert!(
        output.status.success(),
        "search OFF failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains('\x1b'),
        "search output must be ANSI-free, found ESC in: {stdout:?}",
    );
    assert!(
        stdout.contains(match_id),
        "matching session id must appear in results: {stdout:?}",
    );
    assert!(
        !stdout.contains(other_id),
        "non-matching session id must be filtered out: {stdout:?}",
    );
    // The lowercase summary (first-user-message text) is surfaced verbatim,
    // proving the uppercase query found the lowercase body.
    assert!(
        stdout.contains("off by one"),
        "lowercase summary text must appear in output: {stdout:?}",
    );
}

#[test]
fn empty_or_whitespace_search_query_is_rejected_with_exit_2_and_readable_stderr() {
    let home = TempDir::new().unwrap();
    // A fixture is present so the rejection is from the query validator, not
    // from an empty catalog or a missing argument.
    let dir = "--home-pi-int--";
    let file = "2026-07-30T12-00-00-eeeeeeee-0000-4000-8000-000000000005.jsonl";
    let lines = vec![
        header("eeeeeeee-0000-4000-8000-000000000005", "/tmp"),
        message("u1", None, "user", "should not be searched"),
    ];
    write_pi_session(home.path(), dir, file, &lines);

    for query in ["", "   ", "\t"] {
        let output = run(home.path(), &["sessions", "search", query]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "query {query:?} must be rejected with exit code 2",
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.is_empty(),
            "query {query:?}: stderr must be non-empty/readable",
        );
        assert!(
            stderr.contains("empty"),
            "query {query:?}: stderr should state the query is empty, got: {stderr}",
        );
    }
}

#[cfg(unix)]
#[test]
fn legacy_omp_open_and_fork_launch_new_native_copies_without_touching_source() {
    for command in ["open", "fork"] {
        let home = TempDir::new().unwrap();
        let bin = home.path().join("bin");
        let invocations = home.path().join("omp-invocations");
        write_fake_tool(
            &bin,
            "omp",
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AL_INVOCATIONS\"\n",
        );
        let original_id = format!("legacy-omp-{command}");
        let source = write_omp_fixture(home.path(), &original_id, true);
        let before = fs::read(&source).unwrap();
        let output = run_with_env(
            home.path(),
            &["sessions", command, source.to_str().unwrap(), "omp"],
            &[
                ("PATH", bin.as_os_str()),
                ("AL_INVOCATIONS", invocations.as_os_str()),
                ("SESSIONS_OMP_MODEL", OsStr::new("openai/gpt-5:high")),
            ],
        );
        assert!(
            output.status.success(),
            "OMP {command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let invocation = fs::read_to_string(&invocations).unwrap();
        let prefix = if command == "open" {
            "--resume "
        } else {
            "--fork "
        };
        let launched_path = PathBuf::from(invocation.trim().strip_prefix(prefix).unwrap());
        assert_ne!(launched_path, source);
        assert!(launched_path.is_file());
        assert_ne!(first_json_id(&launched_path, "/id"), original_id);
        assert_eq!(fs::read(&source).unwrap(), before);
        assert!(
            String::from_utf8(before)
                .unwrap()
                .contains("keep-on-original")
        );
    }
}

#[cfg(unix)]
#[test]
fn legacy_codex_open_launches_new_native_id_without_touching_source() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let invocations = home.path().join("codex-invocations");
    write_fake_tool(
        &bin,
        "codex",
        r#"#!/bin/sh
case "$1 $2" in
  "doctor --json") printf '%s\n' '{"checks":{"config.load":{"details":{"model provider":"openai","model":"gpt-5"}}}}' ;;
  "debug models") printf '%s\n' '{"models":[{"slug":"gpt-5"}]}' ;;
  *) printf '%s\n' "$*" >> "$AL_INVOCATIONS" ;;
esac
"#,
    );
    let original_id = "11111111-1111-4111-8111-111111111111";
    let source = write_codex_fixture(home.path(), original_id, true);
    let before = fs::read(&source).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "open", source.to_str().unwrap(), "codex"],
        &[
            ("PATH", bin.as_os_str()),
            ("AL_INVOCATIONS", invocations.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "Codex open failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let invocation = fs::read_to_string(&invocations).unwrap();
    let launched_id = invocation.trim().strip_prefix("resume ").unwrap();
    assert_ne!(launched_id, original_id);
    let mut outputs = Vec::new();
    collect_jsonl(&home.path().join(".codex/sessions"), &mut outputs);
    outputs.retain(|path| path != &source);
    assert_eq!(outputs.len(), 1);
    assert_ne!(outputs[0], source);
    assert_eq!(first_json_id(&outputs[0], "/payload/id"), launched_id);
    assert_eq!(fs::read(&source).unwrap(), before);
    assert!(
        String::from_utf8(before)
            .unwrap()
            .contains("keep-on-original")
    );
}

#[cfg(unix)]
#[test]
fn native_same_format_sessions_launch_original_without_materializing() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let omp_invocations = home.path().join("omp-invocations");
    let codex_invocations = home.path().join("codex-invocations");
    write_fake_tool(
        &bin,
        "omp",
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$OMP_INVOCATIONS\"\n",
    );
    write_fake_tool(
        &bin,
        "codex",
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$CODEX_INVOCATIONS\"\n",
    );

    let omp = write_omp_fixture(home.path(), "native-omp", false);
    let omp_before = fs::read(&omp).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "open", omp.to_str().unwrap(), "omp"],
        &[
            ("PATH", bin.as_os_str()),
            ("OMP_INVOCATIONS", omp_invocations.as_os_str()),
        ],
    );
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(&omp_invocations).unwrap().trim(),
        format!("--resume {}", omp.display())
    );
    assert_eq!(fs::read(&omp).unwrap(), omp_before);

    let codex_id = "22222222-2222-4222-8222-222222222222";
    let codex = write_codex_fixture(home.path(), codex_id, false);
    let codex_before = fs::read(&codex).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "open", codex.to_str().unwrap(), "codex"],
        &[
            ("PATH", bin.as_os_str()),
            ("CODEX_INVOCATIONS", codex_invocations.as_os_str()),
        ],
    );
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(&codex_invocations).unwrap().trim(),
        format!("resume {codex_id}")
    );
    assert_eq!(fs::read(&codex).unwrap(), codex_before);

    let mut omp_files = Vec::new();
    collect_jsonl(&home.path().join(".omp/agent/sessions"), &mut omp_files);
    assert_eq!(omp_files, [omp]);
    let mut codex_files = Vec::new();
    collect_jsonl(&home.path().join(".codex/sessions"), &mut codex_files);
    assert_eq!(codex_files, [codex]);
}

#[cfg(unix)]
#[test]
fn print_command_never_materializes_or_mutates_legacy_same_format_sessions() {
    let home = TempDir::new().unwrap();
    let omp = write_omp_fixture(home.path(), "legacy-print-omp", true);
    let codex = write_codex_fixture(home.path(), "33333333-3333-4333-8333-333333333333", true);
    let omp_before = fs::read(&omp).unwrap();
    let codex_before = fs::read(&codex).unwrap();

    for (source, target) in [(&omp, "omp"), (&codex, "codex")] {
        let output = run_with_env(
            home.path(),
            &[
                "sessions",
                "open",
                "--print-command",
                source.to_str().unwrap(),
                target,
            ],
            &[("PATH", OsStr::new(""))],
        );
        assert!(
            output.status.success(),
            "print-command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert_eq!(fs::read(&omp).unwrap(), omp_before);
    assert_eq!(fs::read(&codex).unwrap(), codex_before);
    let mut omp_files = Vec::new();
    collect_jsonl(&home.path().join(".omp/agent/sessions"), &mut omp_files);
    assert_eq!(omp_files, [omp]);
    let mut codex_files = Vec::new();
    collect_jsonl(&home.path().join(".codex/sessions"), &mut codex_files);
    assert_eq!(codex_files, [codex]);
}

#[cfg(unix)]
#[test]
fn agentlo_default_continues_existing_chat_without_fallback() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let invocations = home.path().join("agent-invocations");
    write_fake_tool(
        &bin,
        "agent",
        "#!/bin/sh\nfor a in \"$@\"; do printf '<%s>\\n' \"$a\" >> \"$AL_AGENT_INVOCATIONS\"; done\n",
    );
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run_with_env(
        home.path(),
        &["agentlo"],
        &[
            ("PATH", path.as_os_str()),
            ("AL_AGENT_INVOCATIONS", invocations.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "agentlo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&invocations).unwrap(),
        "<--force>\n<--trust>\n<--approve-mcps>\n<--continue>\n"
    );
}

#[cfg(unix)]
#[test]
fn agentlo_default_starts_new_chat_when_continue_fails() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let invocations = home.path().join("agent-invocations");
    write_fake_tool(
        &bin,
        "agent",
        "#!/bin/sh\nfor a in \"$@\"; do printf '<%s>\\n' \"$a\" >> \"$AL_AGENT_INVOCATIONS\"; done\ncase \" $* \" in *' --continue '*) exit 1;; esac\n",
    );
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run_with_env(
        home.path(),
        &["agentlo"],
        &[
            ("PATH", path.as_os_str()),
            ("AL_AGENT_INVOCATIONS", invocations.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "agentlo fallback failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&invocations).unwrap(),
        "<--force>\n<--trust>\n<--approve-mcps>\n<--continue>\n<--force>\n<--trust>\n<--approve-mcps>\n"
    );
}

#[cfg(unix)]
#[test]
fn agentlo_protected_tail_survives_without_shell_reparsing() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    let invocations = home.path().join("agent-invocations");
    write_fake_tool(
        &bin,
        "agent",
        "#!/bin/sh\nfor a in \"$@\"; do printf '<%s>\\n' \"$a\" >> \"$AL_AGENT_INVOCATIONS\"; done\n",
    );
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    // A literal `--` protects the tail from launcher flag parsing; the
    // positional tail becomes the resume id. Shell metacharacters must
    // survive byte-for-byte because `al` execs structured argv, never a
    // shell string (no $HOME expansion, no `;` command splitting).
    let payload = "arg with $HOME; echo pwned --continue";
    let output = run_with_env(
        home.path(),
        &["agentlo", "--", payload],
        &[
            ("PATH", path.as_os_str()),
            ("AL_AGENT_INVOCATIONS", invocations.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "agentlo -- failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&invocations).unwrap();
    assert_eq!(
        recorded,
        format!("<--force>\n<--trust>\n<--approve-mcps>\n<--resume>\n<{payload}>\n")
    );
}

#[cfg(unix)]
fn write_picker_stubs(home: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let bin = home.join("bin");
    let fzf_state = home.join("fzf-state");
    let pi_invocations = home.join("pi-invocations");
    write_fake_tool(
        &bin,
        "fzf",
        r#"#!/bin/sh
state="${AL_FZF_STATE:?}"
n=0
if [ -f "$state" ]; then
    n=$(cat "$state")
fi
n=$((n + 1))
printf '%s\n' "$n" > "$state"
if [ "$n" -eq 1 ]; then
    IFS= read -r line || true
    cat >/dev/null
    printf '%s\n' "$line"
    exit 0
fi
while IFS= read -r line; do
    if [ "$line" = pi ]; then
        printf '%s\n' "$line"
        exit 0
    fi
done
exit 1
"#,
    );
    write_fake_tool(
        &bin,
        "pi",
        r#"#!/bin/sh
for a in "$@"; do
    printf '<%s>\n' "$a" >> "${AL_PI_INVOCATIONS:?}"
done
"#,
    );
    (bin, fzf_state, pi_invocations)
}

#[cfg(unix)]
#[test]
fn sessions_fzf_picks_session_and_opens_native_pi() {
    let home = TempDir::new().unwrap();
    let cwd = home.path().to_str().expect("utf8 temp home");
    let session_id = "aaaaaaaa-0000-4000-8000-000000000011";
    let path = write_pi_session(
        home.path(),
        "--home-pi-int--",
        "2026-07-30T12-00-00-aaaaaaaa-0000-4000-8000-000000000011.jsonl",
        &[
            header(session_id, cwd),
            message("u1", None, "user", "open this fixture from fzf"),
        ],
    );
    let (bin, fzf_state, pi_invocations) = write_picker_stubs(home.path());
    let path_var =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "--fzf"],
        &[
            ("PATH", path_var.as_os_str()),
            ("AL_FZF_STATE", fzf_state.as_os_str()),
            ("AL_PI_INVOCATIONS", pi_invocations.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "sessions --fzf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&fzf_state).unwrap().trim(), "2");
    assert_eq!(
        fs::read_to_string(&pi_invocations).unwrap(),
        format!("<--session>\n<{}>\n", path.display())
    );
}

#[cfg(unix)]
#[test]
fn sessions_list_fzf_matches_bare_fzf() {
    let home = TempDir::new().unwrap();
    let cwd = home.path().to_str().expect("utf8 temp home");
    let path = write_pi_session(
        home.path(),
        "--home-pi-int--",
        "2026-07-30T12-00-00-bbbbbbbb-0000-4000-8000-000000000012.jsonl",
        &[
            header("bbbbbbbb-0000-4000-8000-000000000012", cwd),
            message("u1", None, "user", "explicit list fzf"),
        ],
    );
    let (bin, fzf_state, pi_invocations) = write_picker_stubs(home.path());
    let path_var =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "list", "--fzf"],
        &[
            ("PATH", path_var.as_os_str()),
            ("AL_FZF_STATE", fzf_state.as_os_str()),
            ("AL_PI_INVOCATIONS", pi_invocations.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "sessions list --fzf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&pi_invocations).unwrap(),
        format!("<--session>\n<{}>\n", path.display())
    );
}

#[cfg(unix)]
#[test]
fn sessions_query_searches_then_opens_only_the_match() {
    let home = TempDir::new().unwrap();
    let cwd = home.path().to_str().expect("utf8 temp home");
    let match_id = "cccccccc-0000-4000-8000-000000000013";
    let match_path = write_pi_session(
        home.path(),
        "--home-pi-int--",
        "2026-07-30T12-00-00-cccccccc-0000-4000-8000-000000000013.jsonl",
        &[
            header(match_id, cwd),
            message("u1", None, "user", "unique needle in the body"),
        ],
    );
    write_pi_session(
        home.path(),
        "--home-pi-int--",
        "2026-07-30T12-01-00-dddddddd-0000-4000-8000-000000000014.jsonl",
        &[
            header("dddddddd-0000-4000-8000-000000000014", cwd),
            message("u1", None, "user", "completely unrelated cats"),
        ],
    );
    let (bin, fzf_state, pi_invocations) = write_picker_stubs(home.path());
    let path_var =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "query", "UNIQUE", "needle"],
        &[
            ("PATH", path_var.as_os_str()),
            ("AL_FZF_STATE", fzf_state.as_os_str()),
            ("AL_PI_INVOCATIONS", pi_invocations.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "sessions query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&pi_invocations).unwrap(),
        format!("<--session>\n<{}>\n", match_path.display())
    );
}

#[cfg(unix)]
#[test]
fn sessions_query_without_match_cancels_without_launching() {
    let home = TempDir::new().unwrap();
    let cwd = home.path().to_str().expect("utf8 temp home");
    write_pi_session(
        home.path(),
        "--home-pi-int--",
        "2026-07-30T12-00-00-eeeeeeee-0000-4000-8000-000000000015.jsonl",
        &[
            header("eeeeeeee-0000-4000-8000-000000000015", cwd),
            message("u1", None, "user", "no such term here"),
        ],
    );
    let (bin, fzf_state, pi_invocations) = write_picker_stubs(home.path());
    let path_var =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "query", "missing-term-xyz"],
        &[
            ("PATH", path_var.as_os_str()),
            ("AL_FZF_STATE", fzf_state.as_os_str()),
            ("AL_PI_INVOCATIONS", pi_invocations.as_os_str()),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        !fzf_state.exists(),
        "empty query results must not spawn fzf"
    );
    assert!(
        !pi_invocations.exists(),
        "empty query results must not launch"
    );
}

#[test]
fn sessions_fzf_on_empty_catalog_cancels() {
    let home = TempDir::new().unwrap();
    let output = run(home.path(), &["sessions", "--fzf"]);
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn sessions_query_requires_a_nonempty_query() {
    let home = TempDir::new().unwrap();
    assert_eq!(
        run(home.path(), &["sessions", "query"]).status.code(),
        Some(2)
    );
    assert_eq!(
        run(home.path(), &["sessions", "query", "   "])
            .status
            .code(),
        Some(2)
    );
}

#[cfg(unix)]
#[test]
fn sessions_fzf_without_fzf_on_path_fails() {
    let home = TempDir::new().unwrap();
    let cwd = home.path().to_str().expect("utf8 temp home");
    write_pi_session(
        home.path(),
        "--home-pi-int--",
        "2026-07-30T12-00-00-ffffffff-0000-4000-8000-000000000016.jsonl",
        &[
            header("ffffffff-0000-4000-8000-000000000016", cwd),
            message("u1", None, "user", "needs fzf"),
        ],
    );
    let empty = home.path().join("empty-bin");
    fs::create_dir_all(&empty).unwrap();
    let path = std::env::join_paths([empty.as_path()]).unwrap();
    let output = run_with_env(
        home.path(),
        &["sessions", "--fzf"],
        &[("PATH", path.as_os_str())],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("fzf"),
        "missing fzf must be reported: {stderr}"
    );
}

#[test]
fn sessions_move_rehomes_pi_workspace_directory() {
    let home = TempDir::new().unwrap();
    let session_id = "aaaaaaaa-0000-4000-8000-000000000020";
    let file = "2026-07-30T12-00-00-aaaaaaaa-0000-4000-8000-000000000020.jsonl";
    let source = write_pi_session(
        home.path(),
        "--workspace-project--",
        file,
        &[
            header(session_id, "/workspace/project"),
            message("m1", None, "user", "move this session"),
            message("m2", Some("m1"), "assistant", "moved"),
        ],
    );

    let output = run(
        home.path(),
        &["sessions", "move", "/workspace/project", "/workspace/moved"],
    );
    assert!(
        output.status.success(),
        "move failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let dest = String::from_utf8(output.stdout).unwrap();
    let dest = dest.trim();
    assert!(
        dest.contains("/.pi/agent/sessions/--workspace-moved--/"),
        "expected re-homed Pi path, got {dest:?}"
    );
    assert!(
        Path::new(dest).is_file(),
        "moved destination is missing: {dest}"
    );
    assert!(
        !source.exists(),
        "source session should be deleted after move: {}",
        source.display()
    );
    let text = fs::read_to_string(dest).unwrap();
    assert!(text.contains("/workspace/moved"), "{text}");
    assert!(!text.contains("/workspace/project"), "{text}");
}

#[test]
fn sessions_move_copies_catalog_folder_without_rewriting_cwd() {
    let home = TempDir::new().unwrap();
    let session_id = "bbbbbbbb-0000-4000-8000-000000000021";
    let file = "2026-07-30T12-00-00-bbbbbbbb-0000-4000-8000-000000000021.jsonl";
    let source = write_pi_session(
        home.path(),
        "--home-pi-int--",
        file,
        &[
            header(session_id, "/workspace/project"),
            message("m1", None, "user", "folder move"),
        ],
    );
    let from = source.parent().unwrap();
    let to = home.path().join(".pi/agent/sessions/--home-pi-dst--");

    let output = run(
        home.path(),
        &[
            "sessions",
            "move",
            from.to_str().expect("utf8 from"),
            to.to_str().expect("utf8 to"),
        ],
    );
    assert!(
        output.status.success(),
        "catalog move failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let dest = to.join(file);
    assert!(
        !source.exists(),
        "source should be gone: {}",
        source.display()
    );
    assert!(dest.is_file(), "destination missing: {}", dest.display());
    let text = fs::read_to_string(&dest).unwrap();
    assert!(
        text.contains("/workspace/project"),
        "folder move must keep recorded cwd: {text}"
    );
}

#[test]
fn sessions_move_dry_run_prints_destination_without_moving() {
    let home = TempDir::new().unwrap();
    let session_id = "cccccccc-0000-4000-8000-000000000022";
    let source = write_pi_session(
        home.path(),
        "--workspace-project--",
        "2026-07-30T12-00-00-cccccccc-0000-4000-8000-000000000022.jsonl",
        &[
            header(session_id, "/workspace/project"),
            message("m1", None, "user", "dry run"),
        ],
    );
    let output = run(
        home.path(),
        &[
            "sessions",
            "move",
            "/workspace/project",
            "/workspace/moved",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let dest = String::from_utf8(output.stdout).unwrap();
    assert!(
        dest.contains("--workspace-moved--"),
        "dry-run should print the destination: {dest:?}"
    );
    assert!(source.exists(), "dry-run must not delete the source");
    assert!(!Path::new(dest.trim()).exists(), "dry-run must not write");
}

#[test]
fn sessions_move_feat_a_project_x_to_feat_b() {
    let home = TempDir::new().unwrap();
    let from = "/workspace/feat-a/projectX";
    let to = "/workspace/feat-b/projectX";
    let file = "2026-07-30T12-00-00-dddddddd-0000-4000-8000-000000000023.jsonl";
    let source = write_pi_session(
        home.path(),
        "--workspace-feat-a-projectX--",
        file,
        &[
            header("dddddddd-0000-4000-8000-000000000023", from),
            message("m1", None, "user", "feat a session"),
        ],
    );
    let sibling = write_pi_session(
        home.path(),
        "--workspace-feat-a-projectX-extra--",
        "2026-07-30T12-00-00-eeeeeeee-0000-4000-8000-000000000024.jsonl",
        &[
            header(
                "eeeeeeee-0000-4000-8000-000000000024",
                "/workspace/feat-a/projectX-extra",
            ),
            message("m1", None, "user", "should stay"),
        ],
    );

    let output = run(home.path(), &["sessions", "move", from, to]);
    assert!(
        output.status.success(),
        "feat move failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let dest = String::from_utf8(output.stdout).unwrap();
    let dest = dest.trim();
    assert!(
        dest.ends_with(&format!(
            "/.pi/agent/sessions/--workspace-feat-b-projectX--/{file}"
        )),
        "unexpected dest {dest:?}"
    );
    assert!(Path::new(dest).is_file());
    assert!(!source.exists());
    assert!(sibling.exists(), "sibling projectX-extra must stay");
    let text = fs::read_to_string(dest).unwrap();
    assert!(text.contains(to), "{text}");
    assert!(!text.contains(from), "{text}");
}

#[test]
fn sessions_move_rpi_workspace() {
    let home = TempDir::new().unwrap();
    let from = "/workspace/old-project";
    let to = "/workspace/new-project";
    let file = "2026-09-03T00-00-00-aaaaaaaa-0000-4000-8000-000000000001.jsonl";
    let source = write_rpi_session(
        home.path(),
        "--workspace-old-project--",
        file,
        &[
            header("aaaaaaaa-0000-4000-8000-000000000001", from),
            message("m1", None, "user", "rpi session"),
        ],
    );
    let pi_twin = write_pi_session(
        home.path(),
        "--workspace-old-project--",
        file,
        &[
            header("aaaaaaaa-0000-4000-8000-000000000001", from),
            message("m1", None, "user", "pi twin stays unless asked"),
        ],
    );

    let output = run(
        home.path(),
        &["sessions", "move", from, to, "--tool", "rpi"],
    );
    assert!(
        output.status.success(),
        "rpi move failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let dest = String::from_utf8(output.stdout).unwrap();
    let dest = dest.trim();
    assert!(
        dest.ends_with(&format!("/.rpi/sessions/--workspace-new-project--/{file}")),
        "unexpected dest {dest:?}"
    );
    assert!(Path::new(dest).is_file());
    assert!(!source.exists());
    assert!(pi_twin.exists(), "pi catalog must stay when --tool rpi");
    let text = fs::read_to_string(dest).unwrap();
    assert!(text.contains(to), "{text}");
    assert!(!text.contains(from), "{text}");
}

#[test]
fn new_project_creates_updates_and_worktree() {
    let home = TempDir::new().unwrap();
    let destination = home.path().join("Projects").join("sample-app");
    let created = run(home.path(), &["new", "sample-app", "ship the parser"]);
    assert!(
        created.status.success(),
        "al new create failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&created.stdout).trim(),
        destination.to_str().expect("utf-8 dest")
    );
    let goal = fs::read_to_string(destination.join("GOAL.md")).expect("GOAL.md");
    assert!(goal.contains("ship the parser"), "{goal}");
    let log = Command::new("git")
        .args(["log", "-1", "--pretty=%s"])
        .current_dir(&destination)
        .output()
        .expect("git log");
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).trim(),
        "Initial commit"
    );

    let updated = run(home.path(), &["new", "sample-app", "next goal"]);
    assert!(
        updated.status.success(),
        "al new update failed: {}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let goal = fs::read_to_string(destination.join("GOAL.md")).expect("GOAL.md");
    assert!(goal.contains("next goal"), "{goal}");

    let planned = run(
        home.path(),
        &[
            "new",
            "x3",
            "~/Projects/pi-zig",
            "完成zig版本的pi-coding-agent",
            "--executor",
            "omp",
            "--worktree",
            "--print-command",
        ],
    );
    assert!(
        planned.status.success(),
        "al new --print-command failed: {}",
        String::from_utf8_lossy(&planned.stderr)
    );
    assert!(!home.path().join("Projects").join("pi-zig").exists());
    let stdout = String::from_utf8_lossy(&planned.stdout);
    assert!(stdout.contains("x3"), "{stdout}");
    assert!(stdout.contains("$HOME/Projects/pi-zig-wt"), "{stdout}");
    assert!(stdout.contains("omlo-pi-zig-wt"), "{stdout}");
    assert!(stdout.contains("omp --auto-approve"), "{stdout}");
    assert!(
        stdout.contains("goal\tomlo-pi-zig-wt\t/goal then 完成zig版本的pi-coding-agent"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("You are the executor"),
        "startup argv prompt should be gone: {stdout}"
    );

    let grok_plan = run(
        home.path(),
        &[
            "new",
            "~/Projects/sample-app",
            "ship the parser",
            "--executor",
            "grok",
            "--print-command",
        ],
    );
    assert!(
        grok_plan.status.success(),
        "al new grok --print-command failed: {}",
        String::from_utf8_lossy(&grok_plan.stderr)
    );
    let grok_out = String::from_utf8_lossy(&grok_plan.stdout);
    assert!(
        grok_out.contains("goal\tgrolo-sample-app\t/goal ship the parser"),
        "{grok_out}"
    );

    let agent_plan = run(
        home.path(),
        &[
            "new",
            "~/Projects/sample-app",
            "ship the parser",
            "--executor",
            "agent",
            "--print-command",
        ],
    );
    assert!(agent_plan.status.success());
    let agent_out = String::from_utf8_lossy(&agent_plan.stdout);
    assert!(
        agent_out.contains("goal\tagentlo-sample-app\tGOAL.md only (agentlo has no /goal)"),
        "{agent_out}"
    );

    let no_tmux_fleet = run(
        home.path(),
        &[
            "new",
            "sample-app",
            "ship the parser",
            "--orchestrator",
            "pi",
            "--executor",
            "omp",
            "--no-tmux",
        ],
    );
    assert!(
        !no_tmux_fleet.status.success(),
        "multiple roles + --no-tmux should fail"
    );
    let no_tmux_err = String::from_utf8_lossy(&no_tmux_fleet.stderr);
    assert!(
        no_tmux_err.contains("multiple roles require tmux"),
        "{no_tmux_err}"
    );

    let worktree = run(
        home.path(),
        &[
            "new",
            destination.to_str().expect("utf-8 repo"),
            "use a worktree",
            "--worktree",
            "feat",
        ],
    );
    assert!(
        worktree.status.success(),
        "al new --worktree failed: {}",
        String::from_utf8_lossy(&worktree.stderr)
    );
    let dest = home.path().join("Projects").join("sample-app-feat");
    assert_eq!(
        String::from_utf8_lossy(&worktree.stdout).trim(),
        dest.to_str().expect("utf-8 dest")
    );
    let goal = fs::read_to_string(dest.join("GOAL.md")).expect("worktree GOAL.md");
    assert!(goal.contains("use a worktree"), "{goal}");
    assert!(dest.join(".git").exists());
}

#[test]
fn new_project_clones_git_urls() {
    let home = TempDir::new().unwrap();
    let alpha = seed_git_upstream(home.path().join("upstreams").join("alpha"));
    let beta = seed_git_upstream(home.path().join("upstreams").join("beta"));

    let missing_worktree = run(
        home.path(),
        &[
            "new",
            "bundle",
            "ship both",
            "--git",
            alpha.to_str().expect("utf-8 alpha"),
            "--git",
            beta.to_str().expect("utf-8 beta"),
        ],
    );
    assert!(
        !missing_worktree.status.success(),
        "multiple --git without --worktree should fail"
    );
    let err = String::from_utf8_lossy(&missing_worktree.stderr);
    assert!(err.contains("multiple --git requires --worktree"), "{err}");

    let cloned = run(
        home.path(),
        &[
            "new",
            "cloned-app",
            "ship the parser",
            "--git",
            alpha.to_str().expect("utf-8 alpha"),
        ],
    );
    assert!(
        cloned.status.success(),
        "al new --git failed: {}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    let dest = home.path().join("Projects").join("cloned-app");
    assert_eq!(
        String::from_utf8_lossy(&cloned.stdout).trim(),
        dest.to_str().expect("utf-8 dest")
    );
    assert!(dest.join(".git").exists());
    assert!(dest.join("README.md").exists());
    let goal = fs::read_to_string(dest.join("GOAL.md")).expect("GOAL.md");
    assert!(goal.contains("ship the parser"), "{goal}");

    let planned = run(
        home.path(),
        &[
            "new",
            "bundle",
            "ship both",
            "--git",
            "https://example.test/org/alpha.git",
            "--git",
            "https://example.test/org/beta.git",
            "--worktree",
            "--print-command",
        ],
    );
    assert!(
        planned.status.success(),
        "al new --git --print-command failed: {}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let stdout = String::from_utf8_lossy(&planned.stdout);
    assert!(
        stdout.contains("clone\thttps://example.test/org/alpha.git"),
        "{stdout}"
    );
    assert!(stdout.contains("worktree/alpha"), "{stdout}");
    assert!(stdout.contains("worktree/beta"), "{stdout}");
    assert!(!stdout.contains("git worktree add"), "{stdout}");
    assert!(!home.path().join("Projects").join("worktree").exists());

    let multi = run(
        home.path(),
        &[
            "new",
            "bundle",
            "ship both",
            "--git",
            alpha.to_str().expect("utf-8 alpha"),
            "--git",
            beta.to_str().expect("utf-8 beta"),
            "--worktree",
        ],
    );
    assert!(
        multi.status.success(),
        "al new --git --worktree failed: {}",
        String::from_utf8_lossy(&multi.stderr)
    );
    let bundle = home.path().join("Projects").join("worktree");
    assert_eq!(
        String::from_utf8_lossy(&multi.stdout).trim(),
        bundle.to_str().expect("utf-8 dest")
    );
    assert!(bundle.join("alpha").join(".git").exists());
    assert!(bundle.join("beta").join(".git").exists());
    assert!(bundle.join("GOAL.md").exists());
    assert!(!bundle.join(".git").exists());
}

fn seed_git_upstream(dir: PathBuf) -> PathBuf {
    fs::create_dir_all(&dir).unwrap();
    let status = Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .status()
        .expect("git init");
    assert!(status.success(), "git init failed");
    fs::write(dir.join("README.md"), "# upstream\n").expect("README");
    let status = Command::new("git")
        .args(["add", "README.md"])
        .current_dir(&dir)
        .status()
        .expect("git add");
    assert!(status.success(), "git add failed");
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=al",
            "-c",
            "user.email=al@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "upstream",
        ])
        .current_dir(&dir)
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit failed");
    dir
}

#[cfg(unix)]
fn path_with(bin: &Path) -> std::ffi::OsString {
    let mut path = bin.as_os_str().to_os_string();
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
}

#[cfg(unix)]
fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_tmux_session(session: &str) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", session])
        .status();
}

#[cfg(unix)]
#[test]
fn new_project_no_tmux_warns_that_goal_cannot_be_sent() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    write_fake_tool(&bin, "grok", "#!/bin/sh\nexit 0\n");
    let path = path_with(&bin);
    let output = run_with_env(
        home.path(),
        &[
            "new",
            "sample-app",
            "ship the parser",
            "--executor",
            "grok",
            "--no-tmux",
        ],
        &[("PATH", path.as_os_str())],
    );
    assert!(
        output.status.success(),
        "al new --no-tmux failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot send /goal"),
        "expected /goal warning, got {stderr}"
    );
    let dest = home.path().join("Projects").join("sample-app");
    let goal = fs::read_to_string(dest.join("GOAL.md")).expect("GOAL.md");
    assert!(goal.contains("ship the parser"), "{goal}");
    assert!(dest.join(".al").join("executor.md").exists());
}

#[cfg(unix)]
#[test]
fn new_project_sends_native_goal_via_tmux() {
    if !tmux_available() {
        eprintln!("skipping new_project_sends_native_goal_via_tmux: tmux not on PATH");
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let grok_name = format!("altest-grok-{}-{stamp}", std::process::id());
    let omp_name = format!("altest-omp-{}-{stamp}", std::process::id());
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    write_fake_tool(
        &bin,
        "grok",
        "#!/bin/sh\nprintf 'ready\\nNo goal\\n>\\n'\nexec cat\n",
    );
    write_fake_tool(
        &bin,
        "omp",
        "#!/bin/sh\nprintf 'ready\\nNo goal\\n>\\n'\nexec cat\n",
    );
    let path = path_with(&bin);
    let wait = std::ffi::OsString::from("2000");
    let _grok_guard = scopeguard_kill(&grok_name);
    let grok = run_with_env(
        home.path(),
        &["new", &grok_name, "ship the parser", "--executor", "grok"],
        &[
            ("PATH", path.as_os_str()),
            ("AL_GOAL_WAIT_MS", wait.as_os_str()),
        ],
    );
    assert!(
        grok.status.success(),
        "al new --executor grok failed: {}",
        String::from_utf8_lossy(&grok.stderr)
    );
    let grok_pane = Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-t",
            &format!("{grok_name}:grolo-{grok_name}"),
        ])
        .output()
        .expect("tmux capture grok");
    assert!(grok_pane.status.success(), "grok capture failed");
    let grok_text = String::from_utf8_lossy(&grok_pane.stdout);
    assert!(
        grok_text.contains("/goal ship the parser"),
        "grok pane missing /goal: {grok_text:?}"
    );

    let _omp_guard = scopeguard_kill(&omp_name);
    let omp = run_with_env(
        home.path(),
        &["new", &omp_name, "ship the parser", "--executor", "omp"],
        &[
            ("PATH", path.as_os_str()),
            ("AL_GOAL_WAIT_MS", wait.as_os_str()),
        ],
    );
    assert!(
        omp.status.success(),
        "al new --executor omp failed: {}",
        String::from_utf8_lossy(&omp.stderr)
    );
    let omp_pane = Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-t",
            &format!("{omp_name}:omlo-{omp_name}"),
        ])
        .output()
        .expect("tmux capture omp");
    assert!(omp_pane.status.success(), "omp capture failed");
    let omp_text = String::from_utf8_lossy(&omp_pane.stdout);
    assert!(
        omp_text.contains("/goal"),
        "omp pane missing /goal: {omp_text:?}"
    );
    assert!(
        omp_text.contains("ship the parser"),
        "omp pane missing goal text: {omp_text:?}"
    );
}

#[cfg(unix)]
fn scopeguard_kill(session: &str) -> TmuxKillOnDrop {
    TmuxKillOnDrop {
        session: session.to_owned(),
    }
}

#[cfg(unix)]
struct TmuxKillOnDrop {
    session: String,
}

#[cfg(unix)]
impl Drop for TmuxKillOnDrop {
    fn drop(&mut self) {
        kill_tmux_session(&self.session);
    }
}

#[test]
fn live_list_attach_watch_and_supervise_surface() {
    let home = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let help = run(home.path(), &["list", "--help"]);
    assert!(
        help.status.success(),
        "list --help failed: {}",
        String::from_utf8_lossy(&help.stderr)
    );
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(
        help_text.contains("not saved sessions"),
        "list help should distinguish live panes from the catalog: {help_text}"
    );
    assert!(
        !help_text.contains("--fzf"),
        "list help should not mention fzf after the move to attach: {help_text}"
    );

    let attach_help = run(home.path(), &["attach", "--help"]);
    assert!(
        attach_help.status.success(),
        "attach --help failed: {}",
        String::from_utf8_lossy(&attach_help.stderr)
    );
    let attach_text = String::from_utf8_lossy(&attach_help.stdout);
    assert!(
        attach_text.contains("--target") && attach_text.contains("fzf"),
        "{attach_text}"
    );

    let watch_help = run(home.path(), &["watch", "--help"]);
    assert!(
        watch_help.status.success(),
        "watch --help failed: {}",
        String::from_utf8_lossy(&watch_help.stderr)
    );
    let watch_text = String::from_utf8_lossy(&watch_help.stdout);
    assert!(
        watch_text.contains("Seconds between table refreshes") && watch_text.contains("--no-git"),
        "{watch_text}"
    );

    let sessions = run(home.path(), &["sessions", "list", "--help"]);
    assert!(
        sessions.status.success(),
        "sessions list --help failed: {}",
        String::from_utf8_lossy(&sessions.stderr)
    );

    let bad_host = run(home.path(), &["list", "--host", ""]);
    assert!(!bad_host.status.success(), "empty --host must be rejected");

    let list = run_with_env(
        home.path(),
        &["list", "--json"],
        &[("AL_LIVE_STATE_DIR", state.path().as_os_str())],
    );
    assert!(
        list.status.success(),
        "al list --json failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    for line in String::from_utf8_lossy(&list.stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let row: Value =
            serde_json::from_str(line).unwrap_or_else(|error| panic!("json row {line}: {error}"));
        assert!(row.get("agent").and_then(Value::as_str).is_some());
        assert!(row.get("cwd").and_then(Value::as_str).is_some());
        assert!(row.get("diff_summary").and_then(Value::as_str).is_some());
        assert!(row.get("state").and_then(Value::as_str).is_some());
        assert!(row.get("pane").and_then(Value::as_str).is_some());
    }

    let plain = run_with_env(
        home.path(),
        &["list"],
        &[("AL_LIVE_STATE_DIR", state.path().as_os_str())],
    );
    assert!(
        plain.status.success(),
        "al list failed: {}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let plain_out = String::from_utf8_lossy(&plain.stdout);
    if plain_out.trim().is_empty() {
        panic!("al list should print a table or 'no live agents'");
    }
    assert!(
        plain_out.contains("no live agents")
            || plain_out.contains("asking")
            || plain_out.contains("working")
            || plain_out.contains("idle")
            || plain_out.contains("blocked"),
        "unexpected list output: {plain_out}"
    );

    let supervise_help = run(home.path(), &["supervise", "--help"]);
    assert!(
        supervise_help.status.success(),
        "supervise --help failed: {}",
        String::from_utf8_lossy(&supervise_help.stderr)
    );
    let supervise_text = String::from_utf8_lossy(&supervise_help.stdout);
    assert!(
        supervise_text.contains("Seconds between table refreshes"),
        "{supervise_text}"
    );
    let send_help = run(home.path(), &["supervise", "send", "--help"]);
    assert!(
        send_help.status.success(),
        "supervise send --help failed: {}",
        String::from_utf8_lossy(&send_help.stderr)
    );
    let missing_message = run(home.path(), &["supervise", "send"]);
    assert!(
        !missing_message.status.success(),
        "supervise send without --message must fail"
    );
}
