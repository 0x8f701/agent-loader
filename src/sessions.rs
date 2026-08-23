//! Local session roots, discovery, catalog queries, and resolution.
//!
//! This module deliberately stops at the local filesystem boundary. It discovers
//! native session files, delegates parsing to the format adapters, and turns
//! successfully parsed sessions into rows suitable for list/search workflows.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::Metadata;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{Local, TimeZone};
use walkdir::WalkDir;

use crate::domain::{Session, SessionRow, SourceTool};
use crate::formats::{agent, claude, codex, droid, grok, omp, pi};
use crate::fs::{is_agent_store, is_grok_summary, is_tree_top_level_session, path_under_root};
use crate::picker::{dedupe_rows, format_paths_line, select_rows};

pub const DEFAULT_RECENT_COUNT: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoot {
    pub tool: SourceTool,
    pub path: PathBuf,
    pub pattern: &'static str,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    sessions_home: PathBuf,
    user_home: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOptions {
    pub count: Option<usize>,
    pub show_all: bool,
    pub dedupe: bool,
    /// An empty list selects every source. Repeated values are harmless.
    pub tools: Vec<SourceTool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub dedupe: bool,
    /// An empty list selects every source. Repeated values are harmless.
    pub tools: Vec<SourceTool>,
}

impl Catalog {
    /// Build a local catalog rooted at `home`.
    ///
    /// This is the hermetic entry point for callers and tests. In normal CLI
    /// operation, use `from_env` so `SESSIONS_HOME` can override `HOME`.
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let home = make_absolute(home.into());
        Self {
            sessions_home: home.clone(),
            user_home: home,
        }
    }

    pub fn from_env() -> Result<Self> {
        Self::from_environment(
            env::var_os("SESSIONS_HOME"),
            env::var_os("HOME"),
            user_home_fallback(),
        )
    }

    pub fn with_homes(
        sessions_home: impl Into<PathBuf>,
        user_home: impl Into<PathBuf>,
    ) -> Self {
        let user_home = make_absolute(user_home.into());
        let sessions_home = expand_tilde(&sessions_home.into(), &user_home);
        Self {
            sessions_home: make_absolute(sessions_home),
            user_home,
        }
    }

    fn from_environment(
        sessions_home: Option<OsString>,
        home: Option<OsString>,
        home_fallback: Option<OsString>,
    ) -> Result<Self> {
        let home = nonempty_os_path(home)
            .or_else(|| nonempty_os_path(home_fallback))
            .ok_or_else(|| anyhow::anyhow!(missing_user_home_message()))?;
        let sessions_home = nonempty_os_path(sessions_home).unwrap_or_else(|| home.clone());
        Ok(Self::with_homes(sessions_home, home))
    }

    pub fn sessions_home(&self) -> &Path {
        &self.sessions_home
    }

    pub fn user_home(&self) -> &Path {
        &self.user_home
    }

    pub fn root_for_tool(&self, tool: SourceTool) -> SessionRoot {
        let (relative, pattern) = root_spec(tool);
        SessionRoot {
            tool,
            path: self.sessions_home.join(relative),
            pattern,
        }
    }

    pub fn roots(&self) -> Vec<SessionRoot> {
        SourceTool::ALL
            .into_iter()
            .map(|tool| self.root_for_tool(tool))
            .collect()
    }

    /// Discover valid native session files for one source.
    ///
    /// Missing roots and unreadable entries are isolated. Pi/OMP and Grok use
    /// the exact-depth validators in `fs`; every source rejects symlinks,
    /// special files, escapes, and rsync partial state.
    pub fn discover(&self, tool: SourceTool) -> Vec<PathBuf> {
        let root = self.root_for_tool(tool);
        let mut paths = Vec::new();
        if !root.path.is_dir() {
            return paths;
        }

        for entry in WalkDir::new(&root.path).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!(
                        "skip {} session discovery under {}: {error}",
                        tool,
                        root.path.display()
                    );
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !matches_pattern(tool, path) || contains_rsync_partial(path, &root.path) {
                continue;
            }
            if !self.is_session_path(tool, path) {
                continue;
            }
            paths.push(path.to_path_buf());
        }
        paths.sort();
        paths
    }

    pub fn is_session_path(&self, tool: SourceTool, path: &Path) -> bool {
        let root = self.root_for_tool(tool).path;
        if contains_rsync_partial(path, &root) || !path_under_root(path, &root) {
            return false;
        }
        match tool {
            SourceTool::Pi | SourceTool::Omp => is_tree_top_level_session(path, &root),
            SourceTool::Grok => is_grok_summary(path, &root),
            SourceTool::Agent => is_agent_store(path, &root),
            SourceTool::Droid | SourceTool::Codex | SourceTool::Claude => true,
        }
    }

    pub fn parse(&self, tool: SourceTool, path: &Path) -> Result<Session> {
        let mut session = match tool {
            SourceTool::Pi => pi::parse(path),
            SourceTool::Omp => omp::parse(path),
            SourceTool::Droid => droid::parse(path),
            SourceTool::Codex => codex::parse(path),
            SourceTool::Claude => claude::parse(path),
            SourceTool::Grok => grok::parse(path, &self.root_for_tool(SourceTool::Grok).path),
            SourceTool::Agent => agent::parse(path),
        }
        .with_context(|| format!("parsing {tool} session {}", path.display()))?;
        session.tool = tool;
        session.path = path.to_path_buf();
        Ok(session)
    }

    /// Scan selected tools and return newest-first rows.
    ///
    /// A malformed, unreadable, or concurrently removed file only removes that
    /// file from the result; the remaining catalog is still returned.
    pub fn scan(&self, tools: &[SourceTool]) -> Vec<SessionRow> {
        self.scan_matching(tools, |_| true)
    }

    pub fn list(&self, options: &ListOptions) -> Vec<SessionRow> {
        let rows = self.scan(&options.tools);
        select_rows(rows, options.count, options.show_all, options.dedupe)
    }

    pub fn search(&self, query: &str, options: &SearchOptions) -> Result<Vec<SessionRow>> {
        if query.trim().is_empty() {
            bail!("search query must not be empty");
        }
        let needle = query.to_lowercase();
        let rows = self.scan_matching(&options.tools, |session| {
            session
                .messages
                .iter()
                .any(|message| message.text.to_lowercase().contains(&needle))
        });
        Ok(if options.dedupe {
            dedupe_rows(&rows)
        } else {
            rows
        })
    }

    pub fn resolve_path_for_tool(
        &self,
        tool: SourceTool,
        input: impl AsRef<OsStr>,
    ) -> Result<PathBuf> {
        let input = input.as_ref();
        let candidate = self.input_path(input);
        if candidate.is_file() {
            if self.is_session_path(tool, &candidate) {
                return Ok(candidate);
            }
            bail!("invalid {tool} session path: {}", candidate.display());
        }

        let input = input
            .to_str()
            .context("session id is not valid UTF-8 and is not an existing path")?;
        let matches = self.matching_paths(tool, input);
        match matches.as_slice() {
            [] => bail!("session not found for {tool}: {input}"),
            [path] => Ok(path.clone()),
            _ => bail!(
                "session id is ambiguous for {tool}: {input}\n{}",
                display_paths(matches.iter().take(10).map(|path| (None, path)))
            ),
        }
    }

    pub fn resolve_for_tool(
        &self,
        tool: SourceTool,
        input: impl AsRef<OsStr>,
    ) -> Result<Session> {
        let path = self.resolve_path_for_tool(tool, input)?;
        self.parse(tool, &path)
    }

    pub fn resolve_any_path(&self, input: impl AsRef<OsStr>) -> Result<(SourceTool, PathBuf)> {
        let input = input.as_ref();
        let candidate = self.input_path(input);
        if candidate.is_file() {
            for tool in SourceTool::ALL {
                if self.is_session_path(tool, &candidate) {
                    return Ok((tool, candidate));
                }
            }
            bail!("cannot infer source tool from path: {}", candidate.display());
        }

        let input = input
            .to_str()
            .context("session id is not valid UTF-8 and is not an existing path")?;
        let mut matches = Vec::new();
        for tool in SourceTool::ALL {
            matches.extend(
                self.matching_paths(tool, input)
                    .into_iter()
                    .map(|path| (tool, path)),
            );
        }
        match matches.as_slice() {
            [] => bail!("session not found: {input}"),
            [(tool, path)] => Ok((*tool, path.clone())),
            _ => bail!(
                "session id is ambiguous across tools: {input}\n{}",
                display_paths(
                    matches
                        .iter()
                        .take(10)
                        .map(|(tool, path)| (Some(*tool), path))
                )
            ),
        }
    }

    pub fn resolve_any(&self, input: impl AsRef<OsStr>) -> Result<Session> {
        let (tool, path) = self.resolve_any_path(input)?;
        self.parse(tool, &path)
    }

    fn scan_matching<F>(&self, tools: &[SourceTool], mut predicate: F) -> Vec<SessionRow>
    where
        F: FnMut(&Session) -> bool,
    {
        let mut rows = Vec::new();
        for tool in selected_tools(tools) {
            for path in self.discover(tool) {
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_file() => metadata,
                    Ok(_) => continue,
                    Err(error) => {
                        eprintln!("skip {tool} session {}: {error}", path.display());
                        continue;
                    }
                };
                let (modified_epoch, size) = session_file_stats(tool, &path, &metadata);
                let session = match self.parse(tool, &path) {
                    Ok(session) => session,
                    Err(error) => {
                        eprintln!("skip {tool} session {}: {error:#}", path.display());
                        continue;
                    }
                };
                if session.session_id.is_empty() {
                    eprintln!("skip {tool} session {}: missing session id", path.display());
                    continue;
                }
                if !predicate(&session) {
                    continue;
                }
                rows.push(SessionRow {
                    modified_epoch,
                    tool,
                    display_time: format_epoch(modified_epoch),
                    session_id: session.session_id,
                    summary: session.summary,
                    path,
                    size,
                    cwd: session.cwd,
                });
            }
        }
        rows.sort_by(|left, right| {
            right
                .modified_epoch
                .partial_cmp(&left.modified_epoch)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.path.cmp(&right.path))
        });
        rows
    }

    fn matching_paths(&self, tool: SourceTool, input: &str) -> Vec<PathBuf> {
        let mut matches = Vec::new();
        for path in self.discover(tool) {
            let Some(file_stem) = path.file_stem().and_then(OsStr::to_str) else {
                continue;
            };
            let stem_without_rollout = file_stem.strip_prefix("rollout-").unwrap_or(file_stem);
            let session = match self.parse(tool, &path) {
                Ok(session) => session,
                Err(_) => continue,
            };
            if session.session_id.is_empty() {
                continue;
            }
            if input == session.session_id
                || input == file_stem
                || input == stem_without_rollout
                || session.session_id.starts_with(input)
                || file_stem.starts_with(input)
                || stem_without_rollout.starts_with(input)
            {
                matches.push(path);
            }
        }
        matches
    }

    fn input_path(&self, input: &OsStr) -> PathBuf {
        let expanded = expand_tilde(Path::new(input), &self.user_home);
        make_absolute(expanded)
    }
}

pub fn list_rows(options: &ListOptions) -> Result<Vec<SessionRow>> {
    Ok(Catalog::from_env()?.list(options))
}

pub fn search_rows(query: &str, options: &SearchOptions) -> Result<Vec<SessionRow>> {
    Catalog::from_env()?.search(query, options)
}

pub fn resolve_input_path(tool: SourceTool, input: impl AsRef<OsStr>) -> Result<PathBuf> {
    Catalog::from_env()?.resolve_path_for_tool(tool, input)
}

pub fn resolve_input_session(tool: SourceTool, input: impl AsRef<OsStr>) -> Result<Session> {
    Catalog::from_env()?.resolve_for_tool(tool, input)
}

pub fn resolve_any_path(input: impl AsRef<OsStr>) -> Result<(SourceTool, PathBuf)> {
    Catalog::from_env()?.resolve_any_path(input)
}

pub fn resolve_any_session(input: impl AsRef<OsStr>) -> Result<Session> {
    Catalog::from_env()?.resolve_any(input)
}

/// Render the `--paths` data contract without ANSI escapes.
///
/// Each emitted line has exactly five TSV fields:
/// `tool`, local timestamp, session id, sanitized summary, and exact path.
/// Unsafe structural fields are skipped by the shared row formatter.
pub fn render_paths_tsv(rows: &[SessionRow]) -> Vec<u8> {
    let mut rendered = Vec::new();
    for line in rows.iter().filter_map(format_paths_line) {
        if !rendered.is_empty() {
            rendered.push(b'\n');
        }
        rendered.extend_from_slice(&line);
    }
    rendered
}

fn root_spec(tool: SourceTool) -> (&'static Path, &'static str) {
    match tool {
        SourceTool::Pi => (Path::new(".pi/agent/sessions"), "*.jsonl"),
        SourceTool::Omp => (Path::new(".omp/agent/sessions"), "*.jsonl"),
        SourceTool::Droid => (Path::new(".factory/sessions"), "*.jsonl"),
        SourceTool::Codex => (Path::new(".codex/sessions"), "rollout-*.jsonl"),
        SourceTool::Claude => (Path::new(".claude/projects"), "*.jsonl"),
        SourceTool::Grok => (Path::new(".grok/sessions"), "summary.json"),
        SourceTool::Agent => (Path::new(".cursor/chats"), "store.db"),
    }
}

fn selected_tools(filters: &[SourceTool]) -> Vec<SourceTool> {
    if filters.is_empty() {
        return SourceTool::ALL.to_vec();
    }
    let selected: HashSet<_> = filters.iter().copied().collect();
    SourceTool::ALL
        .into_iter()
        .filter(|tool| selected.contains(tool))
        .collect()
}

fn matches_pattern(tool: SourceTool, path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    match tool {
        SourceTool::Pi | SourceTool::Omp | SourceTool::Droid | SourceTool::Claude => {
            path.extension() == Some(OsStr::new("jsonl"))
        }
        SourceTool::Codex => {
            path.extension() == Some(OsStr::new("jsonl"))
                && file_name
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-"))
        }
        SourceTool::Grok => file_name == OsStr::new("summary.json"),
        SourceTool::Agent => file_name == OsStr::new("store.db"),
    }
}

fn contains_rsync_partial(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).ok().is_some_and(|relative| {
        relative
            .components()
            .any(|component| component == Component::Normal(OsStr::new(".rsync-partial")))
    })
}

fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    if path == Path::new("~") {
        return home.to_path_buf();
    }
    match path.strip_prefix("~") {
        Ok(remainder) => home.join(remainder),
        Err(_) => path.to_path_buf(),
    }
}

fn make_absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        env::current_dir().map_or(path.clone(), |cwd| cwd.join(path))
    }
}

fn nonempty_os_path(value: Option<OsString>) -> Option<PathBuf> {
    value
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn user_home_fallback() -> Option<OsString> {
    env::var_os("USERPROFILE")
}

#[cfg(not(windows))]
fn user_home_fallback() -> Option<OsString> {
    None
}

fn missing_user_home_message() -> &'static str {
    if cfg!(windows) {
        "HOME or USERPROFILE is not set"
    } else {
        "HOME is not set"
    }
}

fn session_file_stats(tool: SourceTool, path: &Path, metadata: &Metadata) -> (f64, u64) {
    if tool != SourceTool::Agent {
        return (metadata_epoch(metadata), metadata.len());
    }
    let mut modified = metadata_epoch(metadata);
    let mut size = metadata.len();
    let Some(directory) = path.parent() else {
        return (modified, size);
    };
    for name in ["store.db-wal", "store.db-shm", "meta.json"] {
        if let Ok(metadata) = std::fs::symlink_metadata(directory.join(name)) {
            if metadata.is_file() && !metadata.file_type().is_symlink() {
                modified = modified.max(metadata_epoch(&metadata));
                size = size.saturating_add(metadata.len());
            }
        }
    }
    (modified, size)
}

#[cfg(unix)]
fn metadata_epoch(metadata: &Metadata) -> f64 {
    use std::os::unix::fs::MetadataExt;
    metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1_000_000_000.0
}

#[cfg(not(unix))]
fn metadata_epoch(metadata: &Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

fn format_epoch(epoch: f64) -> String {
    let seconds = epoch.floor() as i64;
    let nanos = ((epoch - seconds as f64) * 1_000_000_000.0)
        .round()
        .clamp(0.0, 999_999_999.0) as u32;
    Local
        .timestamp_opt(seconds, nanos)
        .single()
        .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn display_paths<'a>(paths: impl Iterator<Item = (Option<SourceTool>, &'a PathBuf)>) -> String {
    paths
        .map(|(tool, path)| match tool {
            Some(tool) => format!("{tool}: {}", path.display()),
            None => path.display().to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, FileTimes};
    use std::io::Write;
    use std::time::{Duration, UNIX_EPOCH};
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    use tempfile::TempDir;

    use super::*;

    fn catalog() -> (TempDir, Catalog) {
        let home = TempDir::new().expect("temporary home");
        let catalog = Catalog::new(home.path());
        (home, catalog)
    }

    fn write_pi(
        catalog: &Catalog,
        project: &str,
        file_name: &str,
        session_id: &str,
        cwd: &str,
        messages: &[(&str, &str)],
    ) -> PathBuf {
        let path = catalog
            .root_for_tool(SourceTool::Pi)
            .path
            .join(project)
            .join(file_name);
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        let mut file = File::create(&path).expect("create Pi session");
        writeln!(
            file,
            r#"{{"type":"session","id":"{session_id}","cwd":"{cwd}","timestamp":"2026-01-01T00:00:00Z"}}"#
        )
        .expect("write header");
        let mut parent: Option<String> = None;
        for (index, (role, text)) in messages.iter().enumerate() {
            let id = format!("message-{index}");
            let parent_json = parent
                .as_ref()
                .map(|value| format!(r#""{value}""#))
                .unwrap_or_else(|| "null".to_owned());
            writeln!(
                file,
                r#"{{"type":"message","id":"{id}","parentId":{parent_json},"message":{{"role":"{role}","content":{}}}}}"#,
                serde_json::to_string(text).expect("text JSON")
            )
            .expect("write message");
            parent = Some(id);
        }
        file.flush().expect("flush Pi session");
        path
    }

    fn set_modified(path: &Path, seconds: u64) {
        let file = File::options().write(true).open(path).expect("open for times");
        file.set_times(
            FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
        )
        .expect("set modified");
    }

    fn write_grok(catalog: &Catalog, encoded: &str, id: &str, valid: bool) -> PathBuf {
        let directory = catalog
            .root_for_tool(SourceTool::Grok)
            .path
            .join(encoded)
            .join(id);
        fs::create_dir_all(&directory).expect("create Grok directory");
        let path = directory.join("summary.json");
        if valid {
            fs::write(
                &path,
                format!(
                    r#"{{"info":{{"id":"{id}","cwd":"/tmp/grok"}},"generated_title":"Grok session","created_at":"2026-01-01T00:00:00Z"}}"#
                ),
            )
            .expect("write Grok summary");
        } else {
            fs::write(&path, "not JSON").expect("write corrupt Grok summary");
        }
        path
    }

    #[test]
    fn roots_honor_sessions_home_without_duplicating_grok_for_hyper() {
        let catalog = Catalog::from_environment(
            Some(OsString::from("~/catalog")),
            Some(OsString::from("/workspace/user")),
            None,
        )
        .expect("catalog");
        assert_eq!(catalog.sessions_home(), Path::new("/workspace/user/catalog"));
        let roots = catalog.roots();
        assert_eq!(roots.len(), SourceTool::ALL.len());
        assert_eq!(
            roots
                .iter()
                .filter(|root| root.tool == SourceTool::Grok)
                .count(),
            1
        );
        assert_eq!(
            catalog.root_for_tool(SourceTool::Grok).path,
            Path::new("/workspace/user/catalog/.grok/sessions")
        );
    }

    #[test]
    fn from_environment_prefers_nonempty_home_over_fallback() {
        let catalog = Catalog::from_environment(
            None,
            Some(OsString::from("/workspace/preferred")),
            Some(OsString::from("/workspace/fallback")),
        )
        .expect("catalog");
        assert_eq!(catalog.user_home(), Path::new("/workspace/preferred"));
        assert_eq!(catalog.sessions_home(), Path::new("/workspace/preferred"));
    }

    #[test]
    fn from_environment_uses_fallback_when_home_missing_or_empty() {
        let missing_home = Catalog::from_environment(
            None,
            None,
            Some(OsString::from("/workspace/fallback")),
        )
        .expect("catalog from fallback");
        assert_eq!(missing_home.user_home(), Path::new("/workspace/fallback"));

        let empty_home = Catalog::from_environment(
            Some(OsString::from("~/sessions")),
            Some(OsString::from("")),
            Some(OsString::from("/workspace/fallback")),
        )
        .expect("catalog from empty HOME");
        assert_eq!(empty_home.user_home(), Path::new("/workspace/fallback"));
        assert_eq!(
            empty_home.sessions_home(),
            Path::new("/workspace/fallback/sessions")
        );
    }

    #[test]
    fn from_environment_rejects_missing_home_with_platform_message() {
        let err = Catalog::from_environment(None, None, None)
            .expect_err("missing home should fail");
        assert_eq!(err.to_string(), missing_user_home_message());

        let err = Catalog::from_environment(
            Some(OsString::from("/sessions")),
            Some(OsString::from("")),
            Some(OsString::from("")),
        )
        .expect_err("empty homes should fail");
        assert_eq!(err.to_string(), missing_user_home_message());
    }

    #[test]
    fn discovery_enforces_exact_depths_containment_and_partial_exclusion() {
        let (_home, catalog) = catalog();
        let top = write_pi(&catalog, "project", "top.jsonl", "top", "/tmp", &[]);
        let nested = write_pi(
            &catalog,
            "project/top/subagent",
            "nested.jsonl",
            "nested",
            "/tmp",
            &[],
        );
        let partial = write_pi(
            &catalog,
            ".rsync-partial",
            "partial.jsonl",
            "partial",
            "/tmp",
            &[],
        );
        let shallow = catalog
            .root_for_tool(SourceTool::Pi)
            .path
            .join("shallow.jsonl");
        fs::create_dir_all(shallow.parent().expect("parent")).expect("create Pi root");
        fs::write(&shallow, "{}\n").expect("write shallow Pi file");
        let grok_top = write_grok(&catalog, "encoded", "grok-top", true);
        let grok_deep = catalog
            .root_for_tool(SourceTool::Grok)
            .path
            .join("encoded/grok-deep/subagent/summary.json");
        fs::create_dir_all(grok_deep.parent().expect("parent")).expect("create deep parent");
        fs::write(&grok_deep, "{}").expect("write deep summary");

        let grok_shallow = catalog
            .root_for_tool(SourceTool::Grok)
            .path
            .join("encoded/summary.json");
        fs::create_dir_all(grok_shallow.parent().expect("parent"))
            .expect("create shallow Grok parent");
        fs::write(&grok_shallow, "{}").expect("write shallow summary");
        assert_eq!(catalog.discover(SourceTool::Pi), vec![top]);
        assert!(!catalog.is_session_path(SourceTool::Pi, &nested));
        assert!(!catalog.is_session_path(SourceTool::Pi, &partial));
        assert!(!catalog.is_session_path(SourceTool::Pi, &shallow));
        assert_eq!(catalog.discover(SourceTool::Grok), vec![grok_top]);
        assert!(!catalog.is_session_path(SourceTool::Grok, &grok_deep));
        assert!(!catalog.is_session_path(SourceTool::Grok, &grok_shallow));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let external = TempDir::new().expect("external directory");
            let external_path = external.path().join("outside.jsonl");
            fs::write(&external_path, "{}").expect("write external");
            let linked = catalog
                .root_for_tool(SourceTool::Pi)
                .path
                .join("project/linked.jsonl");
            symlink(&external_path, &linked).expect("create symlink");
            assert!(!catalog.discover(SourceTool::Pi).contains(&linked));
        }
    }

    #[test]
    fn scan_isolates_corrupt_files_and_uses_exact_size_and_mtime_order() {
        let (_home, catalog) = catalog();
        let old = write_grok(&catalog, "one", "old", true);
        let corrupt = write_grok(&catalog, "two", "corrupt", false);
        let new = write_grok(&catalog, "three", "new", true);
        set_modified(&old, 10);
        set_modified(&corrupt, 20);
        set_modified(&new, 30);

        let rows = catalog.scan(&[SourceTool::Grok]);
        assert_eq!(
            rows.iter().map(|row| row.session_id.as_str()).collect::<Vec<_>>(),
            vec!["new", "old"]
        );
        assert_eq!(rows[0].path, new);
        assert_eq!(rows[0].modified_epoch, 30.0);
        assert_eq!(rows[0].size, fs::metadata(&rows[0].path).expect("metadata").len());
        assert!(rows.iter().all(|row| row.path != corrupt));
    }

    #[test]
    fn list_preserves_zero_default_all_filters_and_dedupe_semantics() {
        let (_home, catalog) = catalog();
        for index in 0..6 {
            let (cwd, summary) = if index == 0 || index == 5 {
                ("/tmp/same", "Duplicate summary")
            } else {
                ("/tmp/other", match index {
                    1 => "one",
                    2 => "two",
                    3 => "three",
                    _ => "four",
                })
            };
            let path = write_pi(
                &catalog,
                &format!("project-{index}"),
                &format!("session-{index}.jsonl"),
                &format!("session-{index}"),
                cwd,
                &[("user", summary)],
            );
            set_modified(&path, index as u64 + 1);
        }
        let droid_path = catalog
            .root_for_tool(SourceTool::Droid)
            .path
            .join("project/droid.jsonl");
        fs::create_dir_all(droid_path.parent().expect("parent")).expect("create Droid parent");
        fs::write(&droid_path, "{}\n").expect("write Droid fixture");
        set_modified(&droid_path, 100);

        let filtered = ListOptions {
            tools: vec![SourceTool::Pi, SourceTool::Pi],
            ..ListOptions::default()
        };
        let rows = catalog.list(&filtered);
        assert_eq!(rows.len(), DEFAULT_RECENT_COUNT);
        assert!(rows.iter().all(|row| row.tool == SourceTool::Pi));
        assert_eq!(rows[0].session_id, "session-5");

        assert!(catalog
            .list(&ListOptions {
                count: Some(0),
                tools: vec![SourceTool::Pi],
                ..ListOptions::default()
            })
            .is_empty());
        assert_eq!(
            catalog
                .list(&ListOptions {
                    count: Some(0),
                    show_all: true,
                    tools: vec![SourceTool::Pi],
                    ..ListOptions::default()
                })
                .len(),
            6
        );
        let deduped = catalog.list(&ListOptions {
            show_all: true,
            dedupe: true,
            tools: vec![SourceTool::Pi],
            ..ListOptions::default()
        });
        assert_eq!(deduped.len(), 5);
        assert!(deduped.iter().any(|row| row.session_id == "session-5"));
        assert!(!deduped.iter().any(|row| row.session_id == "session-0"));
    }

    #[test]
    fn search_is_case_insensitive_and_checks_non_summary_messages() {
        let (_home, catalog) = catalog();
        write_pi(
            &catalog,
            "project",
            "match.jsonl",
            "match",
            "/tmp",
            &[("user", "ordinary summary"), ("assistant", "Hidden MiXeD Needle")],
        );
        write_pi(
            &catalog,
            "other",
            "miss.jsonl",
            "miss",
            "/tmp",
            &[("user", "unrelated")],
        );

        let rows = catalog
            .search("mixed needle", &SearchOptions::default())
            .expect("search");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "match");
        assert!(catalog.search("  ", &SearchOptions::default()).is_err());
    }

    #[test]
    fn duplicate_uuid_is_ambiguous_but_an_exact_path_selects_one_file() {
        let (_home, catalog) = catalog();
        let first = write_pi(
            &catalog,
            "one",
            "first.jsonl",
            "duplicate-uuid",
            "/tmp/one",
            &[("user", "one")],
        );
        let second = write_pi(
            &catalog,
            "two",
            "second.jsonl",
            "duplicate-uuid",
            "/tmp/two",
            &[("user", "two")],
        );

        let error = catalog
            .resolve_path_for_tool(SourceTool::Pi, OsStr::new("duplicate-uuid"))
            .expect_err("duplicate id must be ambiguous");
        let message = format!("{error:#}");
        assert!(message.contains("ambiguous"));
        assert!(message.contains(&first.display().to_string()));
        assert!(message.contains(&second.display().to_string()));

        let resolved = catalog
            .resolve_for_tool(SourceTool::Pi, first.as_os_str())
            .expect("resolve exact path");
        assert_eq!(resolved.path, first);
        assert_eq!(resolved.cwd, Path::new("/tmp/one"));

        assert!(catalog
            .resolve_any_path(OsStr::new("duplicate-uuid"))
            .expect_err("duplicate id across catalog must be ambiguous")
            .to_string()
            .contains("ambiguous"));
    }

    #[test]
    fn paths_renderer_has_five_ansi_free_fields_and_sanitizes_summary() {
        let (_home, catalog) = catalog();
        let path = write_pi(
            &catalog,
            "project",
            "row.jsonl",
            "row-id",
            "/tmp",
            &[("user", "summary")],
        );
        let mut row = catalog.scan(&[SourceTool::Pi]).remove(0);
        row.summary = "first\tsecond\nthird\rfourth".to_owned();

        let rendered = render_paths_tsv(&[row]);
        assert!(!rendered.windows(2).any(|part| part == b"\x1b["));
        let fields = rendered.split(|byte| *byte == b'\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[0], b"pi");
        assert_eq!(fields[2], b"row-id");
        assert_eq!(fields[3], b"first second third fourth");
        assert_eq!(fields[4], path.as_os_str().as_encoded_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn paths_renderer_preserves_non_utf8_path_bytes_and_skips_unsafe_paths() {
        let exact_path = b"/tmp/sessions/render-\xff.jsonl";
        let mut exact = SessionRow {
            modified_epoch: 1.0,
            tool: SourceTool::Omp,
            display_time: "2024-01-15 10:30:00".to_owned(),
            session_id: "exact".to_owned(),
            summary: "summary".to_owned(),
            path: PathBuf::from(OsString::from_vec(exact_path.to_vec())),
            size: 1,
            cwd: PathBuf::from("/tmp"),
        };
        let rendered = render_paths_tsv(&[exact.clone()]);
        let fields = rendered.split(|byte| *byte == b'\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[4], exact_path);

        exact.path = PathBuf::from(OsString::from_vec(b"/tmp/unsafe\npath".to_vec()));
        assert!(render_paths_tsv(&[exact]).is_empty());
    }

    fn write_agent(catalog: &Catalog, id: &str, message: &str) -> PathBuf {
        let directory = catalog
            .root_for_tool(SourceTool::Agent)
            .path
            .join("0123456789abcdef0123456789abcdef")
            .join(id);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("store.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT)", []).unwrap();
        connection.execute("CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB)", []).unwrap();
        connection.execute("INSERT INTO meta VALUES('0', ?1)", [format!(r#"{{"agentId":"{id}","name":"New Agent"}}"#)]).unwrap();
        connection.execute("INSERT INTO blobs VALUES('message', ?1)", [serde_json::to_vec(&serde_json::json!({"role":"user","content":message})).unwrap()]).unwrap();
        drop(connection);
        fs::write(directory.join("meta.json"), r#"{"cwd":"/workspace/agent","title":"Agent title","updatedAtMs":1767225660000}"#).unwrap();
        fs::write(directory.join("store.db-wal"), b"wal").unwrap();
        path
    }

    #[test]
    fn agent_discovery_search_and_row_stats_use_native_artifacts() {
        let (_home, catalog) = catalog();
        let path = write_agent(&catalog, "agent-session", "Needle in Agent conversation");
        let nested = path.parent().unwrap().join("nested/store.db");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, b"ignored").unwrap();
        assert_eq!(catalog.discover(SourceTool::Agent), [path.clone()]);
        let rows = catalog.search("needle", &SearchOptions { dedupe: false, tools: vec![SourceTool::Agent] }).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool, SourceTool::Agent);
        assert_eq!(rows[0].session_id, "agent-session");
        assert_eq!(rows[0].summary, "Agent title");
        assert_eq!(rows[0].cwd, Path::new("/workspace/agent"));
        assert!(rows[0].size > fs::metadata(&path).unwrap().len());
        assert_eq!(catalog.resolve_for_tool(SourceTool::Agent, "agent-session").unwrap().path, path);
    }

    #[test]
    fn agent_subagents_are_excluded_from_catalog_rows() {
        let (_home, catalog) = catalog();
        let path = write_agent(&catalog, "subagent-session", "internal work");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute(
            "UPDATE meta SET value = ?1 WHERE key = '0'",
            [r#"{"agentId":"subagent-session","subagentInfo":{"parentAgentId":"parent"}}"#],
        ).unwrap();
        assert!(catalog.scan(&[SourceTool::Agent]).is_empty());
        assert!(catalog.resolve_for_tool(SourceTool::Agent, "subagent-session").is_err());
    }
}
