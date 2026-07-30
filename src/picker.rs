//! Row selection/deduplication, human formatting, ANSI coloring, TSV
//! emission, and fzf subprocess helpers for session browsing.
//!
//! All formatting contracts — output widths, color codes, `NO_COLOR`
//! handling, count/all truncation, dedupe keys, unsafe-field skip
//! diagnostics, target-tool ordering, and fzf args/status — match the
//! reference implementation in `scripts/sessions`.

use std::collections::HashMap;
use std::env;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use anyhow::{bail, Context, Result};

use crate::domain::{SessionRow, SourceTool, TargetTool};

// ── ANSI constants ────────────────────────────────────────────────────── //

const RESET: &str = "\x1b[0m";

// ── Row selection / dedupe ────────────────────────────────────────────── //

/// Keep the newest session for each tool / cwd / normalized-summary
/// combination by comparing `modified_epoch`. Input order does not
/// matter; the result is sorted newest-first.
///
/// When the cwd or normalized summary is empty the key falls back to
/// `(tool, session_id, "")` so sessions that lack a usable summary are
/// still deduplicated by identity rather than collapsing together.
pub fn dedupe_rows(rows: &[SessionRow]) -> Vec<SessionRow> {
    let mut best: HashMap<(SourceTool, String, String), SessionRow> = HashMap::new();
    for row in rows {
        let normalized_summary = normalize_summary(&row.summary);
        let cwd_empty = row.cwd.as_os_str().is_empty();
        let key = if cwd_empty || normalized_summary.is_empty() {
            (row.tool, row.session_id.clone(), String::new())
        } else {
            let normalized_cwd = normalize_cwd(&row.cwd);
            (row.tool, normalized_cwd, normalized_summary)
        };
        match best.get(&key) {
            Some(existing) if existing.modified_epoch >= row.modified_epoch => {}
            _ => {
                best.insert(key, row.clone());
            }
        }
    }
    let mut result: Vec<SessionRow> = best.into_values().collect();
    result.sort_by(|a, b| {
        b.modified_epoch
            .partial_cmp(&a.modified_epoch)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    result
}

/// Apply optional dedupe, then truncate to `count` (default 5) unless
/// `show_all` is true. `count` of 0 yields no rows; `show_all` always
/// wins over `count`.
pub fn select_rows(
    rows: Vec<SessionRow>,
    count: Option<usize>,
    show_all: bool,
    dedupe: bool,
) -> Vec<SessionRow> {
    let selected = if dedupe {
        dedupe_rows(&rows)
    } else {
        rows
    };
    if show_all {
        selected
    } else {
        let limit = count.unwrap_or(5);
        selected.into_iter().take(limit).collect()
    }
}

fn normalize_summary(summary: &str) -> String {
    summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Expand a leading `~` then canonicalize symlinks, falling back to
/// the expanded path when the target does not exist.
fn normalize_cwd(cwd: &Path) -> String {
    let expanded = expand_user(cwd);
    expanded
        .canonicalize()
        .unwrap_or(expanded)
        .to_string_lossy()
        .into_owned()
}

fn home_dir() -> Option<PathBuf> {
    if let Some(home) = env::var_os("HOME") {
        return Some(PathBuf::from(home));
    }
    #[cfg(windows)]
    {
        if let Some(profile) = env::var_os("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
    }
    None
}

fn expand_user(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

// ── Display helpers ───────────────────────────────────────────────────── //

/// Human label for a session source without changing its machine
/// identity. Grok sessions are shared with Hyper, so the display says
/// "grok/hyper" while the raw TSV tool field stays "grok".
pub fn display_tool_name(tool: SourceTool) -> &'static str {
    match tool {
        SourceTool::Grok => "grok/hyper",
        _ => tool.as_str(),
    }
}

/// Size-based ANSI color for list rows (gray / green / yellow / red).
pub fn size_color(size: u64) -> &'static str {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if size < 100 * KIB {
        "\x1b[90m"
    } else if size < MIB {
        "\x1b[32m"
    } else if size < 5 * MIB {
        "\x1b[33m"
    } else {
        "\x1b[91m"
    }
}

/// Tool-based ANSI color for picker display.
pub fn tool_color(tool: SourceTool) -> &'static str {
    match tool {
        SourceTool::Pi => "\x1b[94m",
        SourceTool::Omp => "\x1b[96m",
        SourceTool::Droid => "\x1b[91m",
        SourceTool::Codex => "\x1b[92m",
        SourceTool::Claude => "\x1b[93m",
        SourceTool::Grok => "\x1b[95m",
    }
}

/// Color decision for list output: requires a TTY and no `NO_COLOR`.
pub fn use_color_for_list() -> bool {
    std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

/// Color decision for picker/fzf output: always colored unless
/// `NO_COLOR` is set (the consumer is an ANSI-aware picker, not a TTY).
pub fn use_color_for_picker() -> bool {
    env::var_os("NO_COLOR").is_none()
}

// ── Human formatting ──────────────────────────────────────────────────── //

/// Format a single list row: tool (width 10) + 2 spaces + time (width
/// 19) + 2 spaces + session_id (width 36) + 2 spaces + summary.
/// When colored, the entire row is wrapped in a size-based ANSI code.
pub fn format_row(row: &SessionRow, use_color: bool) -> String {
    let display = format!(
        "{:<10}  {:<19}  {:<36}  {}",
        display_tool_name(row.tool),
        row.display_time,
        row.session_id,
        row.summary,
    );
    if use_color {
        format!("{}{}{}", size_color(row.size), display, RESET)
    } else {
        display
    }
}

// ── TSV sanitization ──────────────────────────────────────────────────── //

/// Collapse tabs, newlines, and carriage returns into spaces so a
/// field cannot corrupt a TSV row.
pub fn sanitize_tsv_field(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

// ── Unsafe field skip diagnostics ─────────────────────────────────────── //

/// Check tool, session id, and path for characters that would corrupt
/// a TSV row (`\t`, `\n`, `\r`, `\0`). Returns the human-readable name
/// of the first offending field, or `None` if all fields are safe.
pub fn unsafe_field_name(tool: &str, session_id: &str, path: &str) -> Option<&'static str> {
    unsafe_field_name_bytes(tool, session_id, path.as_bytes())
}

fn unsafe_field_name_bytes(
    tool: &str,
    session_id: &str,
    path: &[u8],
) -> Option<&'static str> {
    if has_unsafe_chars(tool) {
        Some("tool")
    } else if has_unsafe_chars(session_id) {
        Some("session id")
    } else if has_unsafe_bytes(path) {
        Some("path")
    } else {
        None
    }
}

fn has_unsafe_chars(s: &str) -> bool {
    has_unsafe_bytes(s.as_bytes())
}

fn has_unsafe_bytes(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|byte| matches!(byte, b'\t' | b'\n' | b'\r' | b'\0'))
}

#[cfg(unix)]
fn exact_path_bytes(path: &Path) -> Option<&[u8]> {
    Some(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn exact_path_bytes(path: &Path) -> Option<&[u8]> {
    path.to_str().map(str::as_bytes)
}

/// Emit a five-field safe TSV row: `tool\ttimestamp\tsession_id\tsummary\tpath`.
/// Returns `None` and prints a diagnostic to stderr when any structural
/// field contains characters that would corrupt the TSV or the platform
/// cannot represent the path exactly.
pub fn format_paths_line(row: &SessionRow) -> Option<Vec<u8>> {
    let tool = row.tool.as_str();
    let Some(path) = exact_path_bytes(&row.path) else {
        eprintln!(
            "skip {tool} session {}: path is not valid Unicode for --paths",
            row.path.display()
        );
        return None;
    };
    if let Some(field) = unsafe_field_name_bytes(tool, &row.session_id, path) {
        eprintln!(
            "skip {tool} session {}: unsafe {field} for --paths",
            row.path.display()
        );
        return None;
    }
    let mut line = format!(
        "{}\t{}\t{}\t{}\t",
        tool,
        row.display_time,
        row.session_id,
        sanitize_tsv_field(&row.summary),
    )
    .into_bytes();
    line.extend_from_slice(path);
    Some(line)
}

/// Emit a six-field picker TSV row: colored display + `tool\ttimestamp
/// \tsession_id\tsummary\tpath`. The display field is colored with the
/// tool color; the remaining five fields are raw and unmodified.
/// Returns `None` and prints a diagnostic to stderr when any structural
/// field contains characters that would corrupt the TSV or the platform
/// cannot represent the path exactly.
pub fn format_picker_line(row: &SessionRow, use_color: bool) -> Option<Vec<u8>> {
    let tool = row.tool.as_str();
    let Some(path) = exact_path_bytes(&row.path) else {
        eprintln!(
            "skip {tool} session {}: path is not valid Unicode for --picker",
            row.path.display()
        );
        return None;
    };
    if let Some(field) = unsafe_field_name_bytes(tool, &row.session_id, path) {
        eprintln!(
            "skip {tool} session {}: unsafe {field} for --picker",
            row.path.display()
        );
        return None;
    }
    let clean_summary = sanitize_tsv_field(&row.summary);
    let display = format!(
        "{:<11} {:<19}  {:<36}  {}",
        display_tool_name(row.tool),
        row.display_time,
        row.session_id,
        clean_summary,
    );
    let display = if use_color {
        format!("{}{}{}", tool_color(row.tool), display, RESET)
    } else {
        display
    };
    let mut line = format!(
        "{}\t{}\t{}\t{}\t{}\t",
        display, tool, row.display_time, row.session_id, clean_summary,
    )
    .into_bytes();
    line.extend_from_slice(path);
    Some(line)
}

// ── Target-tool ordering ──────────────────────────────────────────────── //

/// Build the ordered list of target tools for a source, preferring the
/// source's native tool (or `hyper` ahead of `grok` for shared Grok
/// storage) and appending the remaining tools in a fixed order.
///
/// Grok sessions are shared with Hyper; Hyper is listed first so the
/// picker defaults to the more capable target while keeping Grok
/// available. Every other source defaults to its own native tool.
/// The remaining candidates are appended in the order: `hyper, omp,
/// codex, claude, grok, pi, droid`.
pub fn target_tools_for_source(source: SourceTool) -> Vec<TargetTool> {
    let mut tools = match source {
        SourceTool::Grok => vec![TargetTool::Hyper, TargetTool::Grok],
        _ => vec![source_to_target(source)],
    };
    for candidate in [
        TargetTool::Hyper,
        TargetTool::Omp,
        TargetTool::Codex,
        TargetTool::Claude,
        TargetTool::Grok,
        TargetTool::Pi,
        TargetTool::Droid,
    ] {
        if !tools.contains(&candidate) {
            tools.push(candidate);
        }
    }
    tools
}

const fn source_to_target(source: SourceTool) -> TargetTool {
    match source {
        SourceTool::Pi => TargetTool::Pi,
        SourceTool::Omp => TargetTool::Omp,
        SourceTool::Droid => TargetTool::Droid,
        SourceTool::Codex => TargetTool::Codex,
        SourceTool::Claude => TargetTool::Claude,
        SourceTool::Grok => TargetTool::Grok,
    }
}

// ── fzf subprocess helpers ────────────────────────────────────────────── //

/// Outcome of an fzf session-list invocation. fzf's stdout is inherited
/// (the selected line reaches the terminal); the caller only needs to
/// know whether the command succeeded, was cancelled, or errored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FzfOutcome {
    /// fzf exited 0 (or was not invoked because there were no lines).
    Selected,
    /// fzf exited 1 (no match) or 130 (Ctrl-C) — user cancelled cleanly.
    Cancelled,
    /// fzf exited with an unexpected non-zero code.
    Error(i32),
}

/// Outcome of an fzf target-tool picker. Stdout is captured so the
/// selected tool is returned to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetOutcome {
    /// fzf exited 0 and the selection parsed to a valid `TargetTool`.
    Selected(TargetTool),
    /// fzf exited 1 (no match) or 130 (Ctrl-C) — user cancelled cleanly.
    Cancelled,
    /// fzf exited with an unexpected non-zero code.
    Error(i32),
}

/// Outcome of a session-picker fzf invocation. fzf's stdout is captured
/// so the selected six-field picker row is parsed into its source tool
/// and absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    /// fzf exited 0 and the six-field picker row was captured.
    Selected {
        source: SourceTool,
        path: PathBuf,
    },
    /// fzf exited 1 (no match) or 130 (Ctrl-C) — user cancelled cleanly.
    Cancelled,
    /// fzf exited with an unexpected non-zero code.
    Error(i32),
}

/// Classify a raw fzf exit code into an [`FzfOutcome`].
pub fn classify_fzf_status(code: i32) -> FzfOutcome {
    match code {
        0 => FzfOutcome::Selected,
        1 | 130 => FzfOutcome::Cancelled,
        c => FzfOutcome::Error(c),
    }
}

/// Run fzf with pre-built lines, inheriting stdout/stderr so the
/// selection (or `--filter` output) reaches the terminal.
///
/// Args: `--ansi --layout=reverse --height=80% --prompt=sessions>
/// --no-multi`. The `SESSIONS_FZF_FILTER` environment variable, when
/// set and non-empty, appends `--filter <value>` for non-interactive
/// filtering.
///
/// Returns [`FzfOutcome::Selected`] immediately when there are no
/// lines. Returns `Err` when `fzf` is not on `PATH` or fails to spawn.
pub fn run_fzf(lines: &[String]) -> Result<FzfOutcome> {
    if lines.is_empty() {
        return Ok(FzfOutcome::Selected);
    }
    let fzf = which_fzf()?;
    let mut cmd = Command::new(&fzf);
    cmd.args([
        "--ansi",
        "--layout=reverse",
        "--height=80%",
        "--prompt=sessions> ",
        "--no-multi",
    ]);
    if let Some(filter) = env::var_os("SESSIONS_FZF_FILTER") {
        if !filter.is_empty() {
            cmd.arg("--filter").arg(&filter);
        }
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().context("spawning fzf")?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .context("fzf stdin was not piped")?;
        for line in lines {
            stdin.write_all(line.as_bytes())?;
            stdin.write_all(b"\n")?;
        }
    }
    let status = child.wait().context("waiting for fzf")?;
    Ok(classify_fzf_status(status.code().unwrap_or(1)))
}

/// Pipe target tool names to fzf and return the selection.
///
/// Args: `--layout=reverse --height=40% --prompt=open with>
/// --no-multi`. Stdout is captured so the caller gets the selected
/// tool. Returns [`TargetOutcome::Cancelled`] when fzf exits 1 (no
/// match) or 130 (Ctrl-C), [`TargetOutcome::Selected`] on a valid
/// selection, or `Err` when fzf is missing, the selection is invalid,
/// or fzf exits with an unexpected code.
pub fn pick_target_tool(tools: &[TargetTool]) -> Result<TargetOutcome> {
    let fzf = which_fzf()?;
    let mut cmd = Command::new(&fzf);
    cmd.args([
        "--layout=reverse",
        "--height=40%",
        "--prompt=open with> ",
        "--no-multi",
    ]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().context("spawning fzf")?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .context("fzf stdin was not piped")?;
        for tool in tools {
            stdin.write_all(tool.as_str().as_bytes())?;
            stdin.write_all(b"\n")?;
        }
    }
    let output = child.wait_with_output().context("waiting for fzf")?;
    let code = output.status.code().unwrap_or(1);
    if code == 1 || code == 130 {
        return Ok(TargetOutcome::Cancelled);
    }
    if code != 0 {
        return Ok(TargetOutcome::Error(code));
    }
    let selection = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_owned();
    if selection.is_empty() {
        return Ok(TargetOutcome::Cancelled);
    }
    let tool: TargetTool = selection
        .parse()
        .with_context(|| format!("invalid target tool selection: {selection}"))?;
    if !tools.contains(&tool) {
        bail!("invalid target tool selection: {selection}");
    }
    Ok(TargetOutcome::Selected(tool))
}

/// Pipe pre-built picker TSV lines to fzf, capture the selected
/// six-field row, and parse out the source tool and absolute path.
///
/// Args: `--ansi --delimiter=\t --with-nth=1 --layout=reverse
/// --height=80% --prompt=<prompt> --no-multi`. fzf displays only the
/// first (colored) field and returns the full line on stdout. The
/// six tab-separated fields are: display, tool, timestamp,
/// session_id, summary, path.
///
/// Returns [`SessionOutcome::Selected`] with the parsed `SourceTool`
/// and `PathBuf` on success, [`SessionOutcome::Cancelled`] when fzf
/// exits 1 or 130, or `Err` when fzf is missing, the selection is
/// malformed, or fzf exits with an unexpected code.
pub fn pick_session(lines: &[Vec<u8>], prompt: &str) -> Result<SessionOutcome> {
    if lines.is_empty() {
        return Ok(SessionOutcome::Cancelled);
    }
    let fzf = which_fzf()?;
    pick_session_with_fzf(lines, prompt, &fzf)
}

fn pick_session_with_fzf(
    lines: &[Vec<u8>],
    prompt: &str,
    fzf: &Path,
) -> Result<SessionOutcome> {
    let mut cmd = Command::new(fzf);
    cmd.args([
        "--ansi",
        "--delimiter=\\t",
        "--with-nth=1",
        "--layout=reverse",
        "--height=80%",
        "--no-multi",
    ]);
    cmd.arg(format!("--prompt={prompt}"));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().context("spawning fzf")?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .context("fzf stdin was not piped")?;
        for line in lines {
            stdin.write_all(line)?;
            stdin.write_all(b"\n")?;
        }
    }
    let output = child.wait_with_output().context("waiting for fzf")?;
    let code = output.status.code().unwrap_or(1);
    if code == 1 || code == 130 {
        return Ok(SessionOutcome::Cancelled);
    }
    if code != 0 {
        return Ok(SessionOutcome::Error(code));
    }
    let selection = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let selection = selection.strip_suffix(b"\r").unwrap_or(selection);
    if selection.is_empty() {
        return Ok(SessionOutcome::Cancelled);
    }
    parse_picker_selection(selection)
        .map(|(source, path)| SessionOutcome::Selected { source, path })
}

/// High-level session picker: select rows, format picker TSV lines,
/// run fzf with the given prompt, and return the parsed outcome.
///
/// This is the primary entry point for `sks`/`skss` — the CLI builds
/// `SessionRow`s (via the sessions module), calls this, then chains
/// `target_tools_for_source` + `pick_target_tool` + `open`.
pub fn select_session(
    rows: Vec<SessionRow>,
    count: Option<usize>,
    show_all: bool,
    dedupe: bool,
    prompt: &str,
) -> Result<SessionOutcome> {
    let use_color = use_color_for_picker();
    let lines: Vec<Vec<u8>> = select_rows(rows, count, show_all, dedupe)
        .iter()
        .filter_map(|row| format_picker_line(row, use_color))
        .collect();
    pick_session(&lines, prompt)
}

/// Parse a six-field picker TSV selection into a source tool and path.
fn parse_picker_selection(selection: &[u8]) -> Result<(SourceTool, PathBuf)> {
    let fields: Vec<&[u8]> = selection.splitn(6, |byte| *byte == b'\t').collect();
    if fields.len() != 6 {
        bail!("malformed selection (expected 6 tab-separated fields)");
    }
    let tool = std::str::from_utf8(fields[1]).context("picker tool field is not UTF-8")?;
    let source: SourceTool = tool
        .parse()
        .with_context(|| format!("unknown tool: {tool}"))?;
    let path = path_from_picker_bytes(fields[5])?;
    Ok((source, path))
}

#[cfg(unix)]
fn path_from_picker_bytes(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_picker_bytes(bytes: &[u8]) -> Result<PathBuf> {
    let path = std::str::from_utf8(bytes).context("picker path field is not UTF-8")?;
    Ok(PathBuf::from(path))
}

/// High-level helper matching the `sessions list --fzf` command:
/// select rows, format with picker-color decision, pipe to fzf.
pub fn list_fzf(
    rows: Vec<SessionRow>,
    count: Option<usize>,
    show_all: bool,
    dedupe: bool,
) -> Result<FzfOutcome> {
    let use_color = use_color_for_picker();
    let lines: Vec<String> = select_rows(rows, count, show_all, dedupe)
        .iter()
        .map(|row| format_row(row, use_color))
        .collect();
    run_fzf(&lines)
}

fn which_fzf() -> Result<PathBuf> {
    let path_var = env::var_os("PATH").context("PATH is not set")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join("fzf");
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = candidate.metadata() {
                    if meta.permissions().mode() & 0o111 != 0 {
                        return Ok(candidate);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                return Ok(candidate);
            }
        }
    }
    bail!("fzf not found in PATH")
}

// ── Unit tests ───────────────────────────────────────────────────────── //

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn row(id: &str, tool: SourceTool, summary: &str, cwd: &str, epoch: f64) -> SessionRow {
        SessionRow {
            modified_epoch: epoch,
            tool,
            display_time: "2024-01-15 10:30:00".to_owned(),
            session_id: id.to_owned(),
            summary: summary.to_owned(),
            path: PathBuf::from(format!("/tmp/sessions/{id}.jsonl")),
            size: 1024,
            cwd: PathBuf::from(cwd),
        }
    }

    // -- dedupe --

    #[test]
    fn dedupe_keeps_newest_per_tool_cwd_summary() {
        let rows = vec![
            row("old", SourceTool::Omp, "Say OK", "/tmp/a", 10.0),
            row("new", SourceTool::Omp, "Say OK", "/tmp/a", 20.0),
            row("other", SourceTool::Omp, "Say OK", "/tmp/b", 15.0),
        ];
        let result = dedupe_rows(&rows);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].session_id, "new");
        assert_eq!(result[1].session_id, "other");
    }

    #[test]
    fn dedupe_normalizes_summary_whitespace_and_case() {
        let rows = vec![
            row("a", SourceTool::Omp, "Hello   World", "/tmp/x", 10.0),
            row("b", SourceTool::Omp, "hello world", "/tmp/x", 20.0),
        ];
        let result = dedupe_rows(&rows);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].session_id, "b"); // newest wins regardless of input order
    }

    #[test]
    fn dedupe_falls_back_to_session_id_when_cwd_empty() {
        let rows = vec![
            row("s1", SourceTool::Omp, "Summary", "", 10.0),
            row("s2", SourceTool::Omp, "Summary", "", 20.0),
        ];
        let result = dedupe_rows(&rows);
        assert_eq!(result.len(), 2); // different session ids → different keys
    }

    #[test]
    fn dedupe_falls_back_to_session_id_when_summary_empty() {
        let rows = vec![
            row("s1", SourceTool::Omp, "", "/tmp/a", 10.0),
            row("s2", SourceTool::Omp, "", "/tmp/a", 20.0),
        ];
        let result = dedupe_rows(&rows);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn dedupe_distinguishes_by_tool() {
        let rows = vec![
            row("p1", SourceTool::Pi, "Same", "/tmp/a", 10.0),
            row("o1", SourceTool::Omp, "Same", "/tmp/a", 10.0),
        ];
        let result = dedupe_rows(&rows);
        assert_eq!(result.len(), 2);
    }

    // -- select_rows --

    #[test]
    fn select_default_count_is_five() {
        let rows: Vec<SessionRow> = (0..10)
            .map(|i| row(&format!("s{i}"), SourceTool::Omp, "Sum", "/tmp", i as f64))
            .collect();
        let result = select_rows(rows, None, false, false);
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn select_count_limits_rows() {
        let rows: Vec<SessionRow> = (0..10)
            .map(|i| row(&format!("s{i}"), SourceTool::Omp, "Sum", "/tmp", i as f64))
            .collect();
        let result = select_rows(rows, Some(3), false, false);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn select_count_zero_yields_nothing() {
        let rows = vec![row("s1", SourceTool::Omp, "Sum", "/tmp", 1.0)];
        let result = select_rows(rows, Some(0), false, false);
        assert!(result.is_empty());
    }

    #[test]
    fn select_all_overrides_count() {
        let rows: Vec<SessionRow> = (0..10)
            .map(|i| row(&format!("s{i}"), SourceTool::Omp, "Sum", "/tmp", i as f64))
            .collect();
        let result = select_rows(rows, Some(3), true, false);
        assert_eq!(result.len(), 10);
    }

    #[test]
    fn select_dedupe_then_truncate() {
        let rows = vec![
            row("old", SourceTool::Omp, "Dup", "/tmp/a", 10.0),
            row("new", SourceTool::Omp, "Dup", "/tmp/a", 20.0),
            row("session-a", SourceTool::Pi, "Other", "/tmp/b", 15.0),
        ];
        let result = select_rows(rows, None, false, true);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].session_id, "new");
        assert_eq!(result[1].session_id, "session-a");
    }

    // -- display_tool_name --

    #[test]
    fn display_name_grok_shows_grok_hyper() {
        assert_eq!(display_tool_name(SourceTool::Grok), "grok/hyper");
    }

    #[test]
    fn display_name_other_tools_are_plain() {
        assert_eq!(display_tool_name(SourceTool::Omp), "omp");
        assert_eq!(display_tool_name(SourceTool::Pi), "pi");
        assert_eq!(display_tool_name(SourceTool::Claude), "claude");
    }

    // -- size_color --

    #[test]
    fn size_color_thresholds() {
        assert_eq!(size_color(0), "\x1b[90m");
        assert_eq!(size_color(100 * 1024 - 1), "\x1b[90m");
        assert_eq!(size_color(100 * 1024), "\x1b[32m");
        assert_eq!(size_color(1024 * 1024 - 1), "\x1b[32m");
        assert_eq!(size_color(1024 * 1024), "\x1b[33m");
        assert_eq!(size_color(5 * 1024 * 1024 - 1), "\x1b[33m");
        assert_eq!(size_color(5 * 1024 * 1024), "\x1b[91m");
    }

    // -- tool_color --

    #[test]
    fn tool_color_mapping() {
        assert_eq!(tool_color(SourceTool::Pi), "\x1b[94m");
        assert_eq!(tool_color(SourceTool::Omp), "\x1b[96m");
        assert_eq!(tool_color(SourceTool::Droid), "\x1b[91m");
        assert_eq!(tool_color(SourceTool::Codex), "\x1b[92m");
        assert_eq!(tool_color(SourceTool::Claude), "\x1b[93m");
        assert_eq!(tool_color(SourceTool::Grok), "\x1b[95m");
    }

    // -- sanitize_tsv_field --

    #[test]
    fn sanitize_replaces_tabs_newlines_cr() {
        assert_eq!(sanitize_tsv_field("a\tb"), "a b");
        assert_eq!(sanitize_tsv_field("a\nb"), "a b");
        assert_eq!(sanitize_tsv_field("a\rb"), "a b");
        assert_eq!(sanitize_tsv_field("a\tb\nc\rd"), "a b c d");
        assert_eq!(sanitize_tsv_field("clean"), "clean");
    }

    // -- unsafe_field_name --

    #[test]
    fn unsafe_field_name_detects_tab() {
        assert_eq!(
            unsafe_field_name("om\tp", "sid", "/tmp/p"),
            Some("tool")
        );
    }

    #[test]
    fn unsafe_field_name_detects_newline_in_session_id() {
        assert_eq!(
            unsafe_field_name("omp", "sid\nx", "/tmp/p"),
            Some("session id")
        );
    }

    #[test]
    fn unsafe_field_name_detects_null_in_path() {
        assert_eq!(
            unsafe_field_name("omp", "sid", "/tmp/\0p"),
            Some("path")
        );
    }

    #[test]
    fn unsafe_field_name_none_when_clean() {
        assert_eq!(unsafe_field_name("omp", "sid", "/tmp/p"), None);
    }

    #[test]
    fn unsafe_field_name_checks_tool_first() {
        assert_eq!(
            unsafe_field_name("om\tp", "si\nd", "/tm\0p"),
            Some("tool")
        );
    }

    // -- format_row --

    #[test]
    fn format_row_widths_and_content() {
        let r = row("abcdef-1234-5678-9012-abcdef123456", SourceTool::Omp, "Hello", "/tmp", 1.0);
        let line = format_row(&r, false);
        // tool (10) + 2 spaces + time (19) + 2 spaces + id (36) + 2 spaces + summary
        assert!(line.starts_with("omp         "));
        assert!(line.contains("2024-01-15 10:30:00"));
        assert!(line.contains("abcdef-1234-5678-9012-abcdef123456"));
        assert!(line.ends_with("Hello"));
    }

    #[test]
    fn format_row_with_color_wraps_in_size_color() {
        let r = row("sid", SourceTool::Omp, "Hello", "/tmp", 1.0);
        let line = format_row(&r, true);
        assert!(line.starts_with("\x1b[90m"));
        assert!(line.ends_with("\x1b[0m"));
    }

    #[test]
    fn format_row_grok_uses_display_name() {
        let r = row("sid", SourceTool::Grok, "Hello", "/tmp", 1.0);
        let line = format_row(&r, false);
        assert!(line.starts_with("grok/hyper"));
    }

    // -- format_paths_line --

    #[test]
    fn paths_line_has_five_fields() {
        let r = row("sid", SourceTool::Omp, "Hello World", "/tmp", 1.0);
        let line = format_paths_line(&r).unwrap();
        let fields: Vec<&[u8]> = line.split(|byte| *byte == b'\t').collect();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[0], b"omp");
        assert_eq!(fields[1], b"2024-01-15 10:30:00");
        assert_eq!(fields[2], b"sid");
        assert_eq!(fields[3], b"Hello World");
        assert_eq!(fields[4], b"/tmp/sessions/sid.jsonl");
    }

    #[test]
    fn paths_line_sanitizes_summary() {
        let mut r = row("sid", SourceTool::Omp, "a\tb\nc", "/tmp", 1.0);
        r.summary = "a\tb\nc".to_owned();
        let line = format_paths_line(&r).unwrap();
        let fields: Vec<&[u8]> = line.split(|byte| *byte == b'\t').collect();
        assert_eq!(fields[3], b"a b c");
    }

    #[test]
    fn paths_line_returns_none_for_unsafe_path() {
        let mut r = row("sid", SourceTool::Omp, "ok", "/tmp", 1.0);
        r.path = PathBuf::from("/tmp/seg\tment");
        assert!(format_paths_line(&r).is_none());
    }

    #[test]
    fn paths_line_returns_none_for_unsafe_session_id() {
        let mut r = row("sid", SourceTool::Omp, "ok", "/tmp", 1.0);
        r.session_id = "si\td".to_owned();
        assert!(format_paths_line(&r).is_none());
    }

    // -- format_picker_line --

    #[test]
    fn picker_line_has_six_fields() {
        let r = row("sid", SourceTool::Omp, "Hello", "/tmp", 1.0);
        let line = format_picker_line(&r, false).unwrap();
        let fields: Vec<&[u8]> = line.split(|byte| *byte == b'\t').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[1], b"omp"); // raw tool, not display name
        assert_eq!(fields[2], b"2024-01-15 10:30:00");
        assert_eq!(fields[3], b"sid");
        assert_eq!(fields[4], b"Hello");
        assert_eq!(fields[5], b"/tmp/sessions/sid.jsonl");
    }

    #[test]
    fn picker_line_display_has_tool_color_when_colored() {
        let r = row("sid", SourceTool::Omp, "Hello", "/tmp", 1.0);
        let line = format_picker_line(&r, true).unwrap();
        let display = line.split(|byte| *byte == b'\t').next().unwrap();
        assert!(display.starts_with(b"\x1b[96m")); // omp = bright cyan
        assert!(display.ends_with(b"\x1b[0m"));
    }

    #[test]
    fn picker_line_no_color_when_disabled() {
        let r = row("sid", SourceTool::Omp, "Hello", "/tmp", 1.0);
        let line = format_picker_line(&r, false).unwrap();
        let display = line.split(|byte| *byte == b'\t').next().unwrap();
        assert!(!display.contains(&b'\x1b'));
    }

    #[test]
    fn picker_line_grok_display_is_grok_hyper_raw_is_grok() {
        let r = row("sid", SourceTool::Grok, "Hello", "/tmp", 1.0);
        let line = format_picker_line(&r, false).unwrap();
        let fields: Vec<&[u8]> = line.split(|byte| *byte == b'\t').collect();
        assert!(fields[0].windows(b"grok/hyper".len()).any(|part| part == b"grok/hyper"));
        assert_eq!(fields[1], b"grok"); // raw tool unchanged
    }

    #[test]
    fn picker_line_display_width_matches_list() {
        // Both list and picker should produce a 12-char tool column.
        let r = row("sid", SourceTool::Omp, "Hi", "/tmp", 1.0);
        let list_line = format_row(&r, false);
        let picker_line = format_picker_line(&r, false).unwrap();
        let picker_display = picker_line.split(|byte| *byte == b'\t').next().unwrap();
        // The tool column (up to and including the separator) is 12 bytes.
        let list_tool_col = &list_line.as_bytes()[..12];
        let picker_tool_col = &picker_display[..12];
        assert_eq!(list_tool_col, picker_tool_col);
    }

    #[test]
    fn picker_line_sanitizes_summary_in_both_display_and_raw() {
        let mut r = row("sid", SourceTool::Omp, "ok", "/tmp", 1.0);
        r.summary = "a\tb".to_owned();
        let line = format_picker_line(&r, false).unwrap();
        let fields: Vec<&[u8]> = line.split(|byte| *byte == b'\t').collect();
        assert_eq!(fields[4], b"a b"); // raw summary sanitized
        // display also has sanitized summary
        assert!(fields[0].windows(3).any(|part| part == b"a b"));
    }

    #[test]
    fn picker_line_returns_none_for_unsafe_path() {
        let mut r = row("sid", SourceTool::Omp, "ok", "/tmp", 1.0);
        r.path = PathBuf::from("/tmp/seg\nment");
        assert!(format_picker_line(&r, false).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn byte_rows_preserve_non_utf8_path_exactly() {
        let path_bytes = b"/tmp/sessions/non-utf8-\xff.jsonl";
        let mut r = row("sid", SourceTool::Omp, "Hello", "/tmp", 1.0);
        r.path = PathBuf::from(std::ffi::OsString::from_vec(path_bytes.to_vec()));

        let paths_line = format_paths_line(&r).unwrap();
        let paths_fields: Vec<&[u8]> = paths_line.split(|byte| *byte == b'\t').collect();
        assert_eq!(paths_fields.len(), 5);
        assert_eq!(paths_fields[4], path_bytes);

        let picker_line = format_picker_line(&r, false).unwrap();
        let picker_fields: Vec<&[u8]> = picker_line.split(|byte| *byte == b'\t').collect();
        assert_eq!(picker_fields.len(), 6);
        assert_eq!(picker_fields[5], path_bytes);

        let (source, parsed_path) = parse_picker_selection(&picker_line).unwrap();
        assert_eq!(source, SourceTool::Omp);
        assert_eq!(parsed_path.as_os_str().as_bytes(), path_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn byte_rows_reject_structural_path_bytes() {
        for structural in [b'\t', b'\n', b'\r', b'\0'] {
            let mut path = b"/tmp/sessions/unsafe".to_vec();
            path.push(structural);
            path.extend_from_slice(b"path.jsonl");
            let mut r = row("sid", SourceTool::Omp, "Hello", "/tmp", 1.0);
            r.path = PathBuf::from(std::ffi::OsString::from_vec(path));
            assert!(format_paths_line(&r).is_none());
            assert!(format_picker_line(&r, false).is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn pick_session_round_trips_non_utf8_path_through_stub_fzf() {
        let temp = tempfile::tempdir().unwrap();
        let fzf = temp.path().join("fzf");
        fs::write(&fzf, b"#!/bin/sh\ncat\n").unwrap();
        let mut permissions = fs::metadata(&fzf).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fzf, permissions).unwrap();

        let path_bytes = b"/tmp/sessions/fzf-\xfe.jsonl";
        let mut r = row("sid", SourceTool::Omp, "Hello", "/tmp", 1.0);
        r.path = PathBuf::from(std::ffi::OsString::from_vec(path_bytes.to_vec()));
        let line = format_picker_line(&r, false).unwrap();

        let outcome = pick_session_with_fzf(&[line], "sessions> ", &fzf).unwrap();
        let SessionOutcome::Selected { source, path } = outcome else {
            panic!("expected selected session, got {outcome:?}");
        };
        assert_eq!(source, SourceTool::Omp);
        assert_eq!(path.as_os_str().as_bytes(), path_bytes);
    }

    // -- target_tools_for_source --

    #[test]
    fn target_tools_grok_prefers_hyper_first() {
        let tools = target_tools_for_source(SourceTool::Grok);
        assert_eq!(tools[0], TargetTool::Hyper);
        assert_eq!(tools[1], TargetTool::Grok);
        assert_eq!(tools.len(), 7);
    }

    #[test]
    fn target_tools_non_grok_starts_with_native() {
        let tools = target_tools_for_source(SourceTool::Pi);
        assert_eq!(tools[0], TargetTool::Pi);
        assert_eq!(tools.len(), 7);
    }

    // -- parse_picker_selection --

    #[test]
    fn parse_picker_selection_valid_six_fields() {
        let line = b"display\x1b[0m\tomp\t2024-01-15 10:30:00\tsid\tHello\t/tmp/session.jsonl";
        let (source, path) = parse_picker_selection(line).unwrap();
        assert_eq!(source, SourceTool::Omp);
        assert_eq!(path, PathBuf::from("/tmp/session.jsonl"));
    }

    #[test]
    fn parse_picker_selection_grok_raw_tool_stays_grok() {
        let line = b"grok/hyper display\tgrok\t2024-01-15 10:30:00\tsid\tHello\t/tmp/grok.jsonl";
        let (source, path) = parse_picker_selection(line).unwrap();
        assert_eq!(source, SourceTool::Grok);
        assert_eq!(path, PathBuf::from("/tmp/grok.jsonl"));
    }

    #[test]
    fn parse_picker_selection_rejects_five_fields() {
        let line = b"display\tomp\ttime\tsid\tsummary";
        assert!(parse_picker_selection(line).is_err());
    }

    #[test]
    fn parse_picker_selection_rejects_unknown_tool() {
        let line = b"display\tunknown\ttime\tsid\tsummary\t/path";
        assert!(parse_picker_selection(line).is_err());
    }

    // -- pick_session empty input --

    #[test]
    fn pick_session_empty_returns_cancelled() {
        let result = pick_session(&[], "sessions> ");
        assert_eq!(result.unwrap(), SessionOutcome::Cancelled);
    }

    #[test]
    fn target_tools_omp_ordering() {
        let tools = target_tools_for_source(SourceTool::Omp);
        assert_eq!(
            tools,
            vec![
                TargetTool::Omp,
                TargetTool::Hyper,
                TargetTool::Codex,
                TargetTool::Claude,
                TargetTool::Grok,
                TargetTool::Pi,
                TargetTool::Droid,
            ]
        );
    }

    #[test]
    fn target_tools_grok_ordering() {
        let tools = target_tools_for_source(SourceTool::Grok);
        assert_eq!(
            tools,
            vec![
                TargetTool::Hyper,
                TargetTool::Grok,
                TargetTool::Omp,
                TargetTool::Codex,
                TargetTool::Claude,
                TargetTool::Pi,
                TargetTool::Droid,
            ]
        );
    }

    #[test]
    fn target_tools_all_seven_present() {
        for source in SourceTool::ALL {
            let tools = target_tools_for_source(source);
            assert_eq!(tools.len(), 7, "wrong count for {source:?}");
            let set: HashSet<TargetTool> = tools.iter().copied().collect();
            assert!(set.contains(&TargetTool::Pi));
            assert!(set.contains(&TargetTool::Omp));
            assert!(set.contains(&TargetTool::Droid));
            assert!(set.contains(&TargetTool::Codex));
            assert!(set.contains(&TargetTool::Claude));
            assert!(set.contains(&TargetTool::Grok));
            assert!(set.contains(&TargetTool::Hyper));
        }
    }

    // -- use_color / NO_COLOR --

    #[test]
    fn use_color_for_picker_false_when_no_color_set() {
        // SAFETY: no other thread is running in this single-threaded test.
        unsafe {
            env::set_var("NO_COLOR", "1");
        }
        assert!(!use_color_for_picker());
        unsafe {
            env::remove_var("NO_COLOR");
        }
        assert!(use_color_for_picker());
    }

    // -- classify_fzf_status --

    #[test]
    fn classify_fzf_status_selected() {
        assert_eq!(classify_fzf_status(0), FzfOutcome::Selected);
    }

    #[test]
    fn classify_fzf_status_cancelled() {
        assert_eq!(classify_fzf_status(1), FzfOutcome::Cancelled);
        assert_eq!(classify_fzf_status(130), FzfOutcome::Cancelled);
    }

    #[test]
    fn classify_fzf_status_error() {
        assert_eq!(classify_fzf_status(2), FzfOutcome::Error(2));
        assert_eq!(classify_fzf_status(127), FzfOutcome::Error(127));
    }

    // -- run_fzf empty input --

    #[test]
    fn run_fzf_empty_returns_selected() {
        // No lines → fzf not invoked → Selected (matches Python return 0).
        let result = run_fzf(&[]);
        assert_eq!(result.unwrap(), FzfOutcome::Selected);
    }

    // -- normalize_summary / normalize_cwd --

    #[test]
    fn normalize_summary_collapses_whitespace_and_lowercases() {
        assert_eq!(normalize_summary("  Hello   World  "), "hello world");
        assert_eq!(normalize_summary("ABC"), "abc");
        assert_eq!(normalize_summary(""), "");
    }

    // -- expand_user --

    #[test]
    fn expand_user_tilde() {
        let home = env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = home {
            assert_eq!(expand_user(Path::new("~")), home);
            assert_eq!(expand_user(Path::new("~/foo")), home.join("foo"));
        }
        assert_eq!(expand_user(Path::new("/abs/path")), PathBuf::from("/abs/path"));
    }
}