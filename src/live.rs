//! Live tmux coding-agent discovery, status, and pane delivery.
//!
//! `al list` prints a one-shot snapshot. `al attach` picks a pane with fzf
//! (or a direct `--target`) and attaches. `al watch` refreshes the table in
//! place across local and remote hosts. `al supervise TARGET` pins one pane
//! and nudges it when idle; other agents (even with `/goal` started) are
//! never sent to. Bare `al supervise` is still a watch. `send`/`diff` stay
//! one-shot. Identification prefers the pane process tree, then `al`
//! session/window names. State comes from the captured screen.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const SNIPPET_CHARS: usize = 56;
const SNIPPET_SCAN: usize = 20;
const STRUCT_LINES: usize = 12;
const WORD_LINES: usize = 2;
const CWD_CHARS: usize = 24;
const SESSION_CHARS: usize = 16;
const DEFAULT_INTERVAL: u64 = 3;
const DUMP_VERSION: &str = "AL_LIVE_1";
static PANE_MEMORY_LOCK: Mutex<()> = Mutex::new(());
#[cfg(all(test, unix))]
pub(crate) static TMUX_TEST_LOCK: Mutex<()> = Mutex::new(());
const KEEP_SUBMIT_RETRIES: u32 = 3;
const KEEP_SUBMIT_WAIT_MS: u64 = 400;

const AGENT_ALIASES: &[(&str, &str)] = &[
    ("agent", "agent"),
    ("agentlo", "agent"),
    ("cclo", "claude"),
    ("claude", "claude"),
    ("claude-code", "claude"),
    ("codex", "codex"),
    ("colo", "codex"),
    ("dolo", "droid"),
    ("droid", "droid"),
    ("grok", "grok"),
    ("grok-build", "grok"),
    ("grolo", "grok"),
    ("hyper", "hyper"),
    ("hyperlo", "hyper"),
    ("omp", "omp"),
    ("omlo", "omp"),
    ("open-code", "opencode"),
    ("opencode", "opencode"),
    ("opencode2", "opencode"),
    ("pi", "pi"),
    ("pilo", "pi"),
    ("rpi", "rpi"),
    ("rpilo", "rpi"),
];

const NAME_PREFIXES: &[(&str, &str)] = &[
    ("agentlo-", "agent"),
    ("cclo-", "claude"),
    ("colo-", "codex"),
    ("dolo-", "droid"),
    ("grolo-", "grok"),
    ("hyperlo-", "hyper"),
    ("omlo-", "omp"),
    ("pilo-", "pi"),
    ("rpilo-", "rpi"),
];

const RUNTIMES: &[&str] = &[
    "node", "bun", "python", "python3", "sh", "bash", "zsh", "fish",
];
const SPINNERS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⣾⣽⣻⢿⡿⣟⣯⣷";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Blocked,
    Asking,
    Working,
    Idle,
    Unknown,
}

impl AgentState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Asking => "asking",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Unknown => "unknown",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Blocked => "\x1b[91m",
            Self::Asking => "\x1b[93m",
            Self::Working => "\x1b[96m",
            Self::Idle => "\x1b[92m",
            Self::Unknown => "\x1b[90m",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveAgent {
    pub host: String,
    pub session: String,
    pub window: String,
    pub pane: String,
    pub target: String,
    pub cwd: String,
    pub agent: String,
    pub branch: String,
    pub diff_summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    pub state: AgentState,
    pub idle_secs: u64,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    pub tpgid: i32,
    pub command: String,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pane {
    pane_id: String,
    session: String,
    window: String,
    window_index: String,
    pane_index: String,
    pid: i32,
    cwd: String,
    activity: u64,
    title: String,
}

#[derive(Debug, Clone, Default)]
struct Snapshot {
    panes: Vec<Pane>,
    processes: Vec<Process>,
    captures: HashMap<String, String>,
    proc_cwds: HashMap<i32, String>,
    gits: HashMap<String, GitInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GitInfo {
    branch: String,
    files: u32,
    insertions: u32,
    deletions: u32,
    untracked: u32,
    inside: bool,
    diff: Option<String>,
}

impl GitInfo {
    fn summary(&self) -> String {
        if !self.inside {
            return "-".to_owned();
        }
        let mut parts = Vec::new();
        if self.insertions > 0 || self.deletions > 0 {
            parts.push(format!("+{}/-{}", self.insertions, self.deletions));
        } else if self.files > 0 {
            parts.push(format!("{} files", self.files));
        }
        if self.untracked > 0 {
            parts.push(format!("?{}", self.untracked));
        }
        if parts.is_empty() {
            String::new()
        } else {
            parts.join(" ")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitMode {
    Off,
    Status,
    Diff,
}

impl GitMode {
    fn want_status(self) -> bool {
        matches!(self, Self::Status | Self::Diff)
    }

    fn want_diff(self) -> bool {
        matches!(self, Self::Diff)
    }

    fn remote_arg(self) -> &'static str {
        match self {
            Self::Off => "none",
            Self::Status => "status",
            Self::Diff => "diff",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PaneMemory {
    hash: u64,
    changed_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListOptions {
    pub hosts: Vec<String>,
    pub json: bool,
    pub diff: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachOptions {
    pub hosts: Vec<String>,
    pub query: String,
    pub target: Option<String>,
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    pub hosts: Vec<String>,
    pub interval: u64,
    pub no_git: bool,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOptions {
    pub host: Option<String>,
    pub target: Option<String>,
    pub message: String,
    pub submit: bool,
}

pub const DEFAULT_KEEP_MESSAGE: &str = "Continue from GOAL.md. Do not idle-wait.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepOptions {
    pub hosts: Vec<String>,
    pub target: String,
    pub message: String,
    pub interval: u64,
    pub max_ticks: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepAction {
    Nudge,
    SkipWorking,
    SkipAsking,
    SkipPicker,
}

impl KeepAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Nudge => "nudged",
            Self::SkipWorking => "skip working",
            Self::SkipAsking => "skip asking",
            Self::SkipPicker => "skip picker",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOptions {
    pub host: Option<String>,
    pub target: Option<String>,
}

pub fn run_list(options: &ListOptions) -> Result<()> {
    let mode = if options.diff {
        GitMode::Diff
    } else {
        GitMode::Status
    };
    let scan = collect_agents(&options.hosts, mode);
    if options.json {
        for agent in &scan.agents {
            println!("{}", serde_json::to_string(agent)?);
        }
    } else if scan.agents.is_empty() {
        println!("no live agents");
    } else {
        print!("{}", format_table(&scan.agents, use_color()));
        if options.diff {
            print_diffs(&scan.agents);
        }
    }
    if scan.failed {
        bail!("one or more hosts failed");
    }
    Ok(())
}

pub fn run_attach(options: &AttachOptions) -> Result<()> {
    let scan = collect_agents(&options.hosts, GitMode::Status);
    if scan.agents.is_empty() {
        if scan.failed {
            bail!("one or more hosts failed");
        }
        bail!("no live agents");
    }
    if options.all {
        return attach_all(&scan.agents);
    }
    if let Some(target) = options.target.as_deref() {
        let agent = resolve_agent(&scan.agents, Some(target))?;
        attach_agent(agent)
    } else {
        pick_and_attach(&scan.agents, &options.query)
    }
}

pub fn run_watch(options: &WatchOptions) -> Result<()> {
    let interval = options.interval.max(1);
    let tty = io::stdout().is_terminal();
    let mode = if options.no_git {
        GitMode::Off
    } else {
        GitMode::Status
    };
    loop {
        let scan = collect_agents(&options.hosts, mode);
        let frame = render_watch_frame(
            &options.label,
            &scan.agents,
            interval,
            &scan.failed_hosts,
            use_color(),
        );
        if tty {
            print!("\x1b[2J\x1b[H");
        }
        print!("{frame}");
        if !tty {
            println!("---");
        }
        let _ = io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn render_watch_frame(
    label: &str,
    agents: &[LiveAgent],
    interval: u64,
    failed_hosts: &[String],
    color: bool,
) -> String {
    let mut out = format_summary(label, agents, interval, failed_hosts, color);
    out.push('\n');
    if agents.is_empty() {
        out.push_str("no live agents\n");
    } else {
        out.push_str(&format_table(agents, color));
    }
    out
}

pub fn run_send(options: &SendOptions) -> Result<()> {
    let host = options
        .host
        .as_deref()
        .filter(|host| *host != "local")
        .unwrap_or("local");
    let agents = scan_host(host, GitMode::Off)?;
    let agent = resolve_agent(&agents, options.target.as_deref())?;
    deliver(
        host,
        &agent.pane,
        options.message.as_bytes(),
        options.submit,
    )
}

pub fn run_keep(options: &KeepOptions) -> Result<()> {
    if options.target.trim().is_empty() {
        bail!("al supervise keep requires TARGET");
    }
    if options.hosts.len() > 1 {
        bail!(
            "al supervise TARGET uses one --host (got {})",
            options.hosts.len()
        );
    }
    let interval = options.interval.max(1);
    let max_ticks = options.max_ticks.or_else(keep_max_ticks_from_env);
    let mut ticks = 0u64;
    loop {
        keep_once(options)?;
        ticks += 1;
        if max_ticks.is_some_and(|max| ticks >= max) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

fn keep_max_ticks_from_env() -> Option<u64> {
    env::var("AL_SUPERVISE_MAX_TICKS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|ticks: &u64| *ticks > 0)
}

fn keep_once(options: &KeepOptions) -> Result<()> {
    let host = options
        .hosts
        .first()
        .map(String::as_str)
        .filter(|host| *host != "local")
        .unwrap_or("local");
    let agents = scan_host(host, GitMode::Off)?;
    let agent = resolve_agent(&agents, Some(options.target.as_str()))?;
    let screen = capture_on(host, &agent.pane);
    let action = keep_action(agent.state, &screen);
    println!(
        "al supervise  keep {}  {} {}  {}",
        options.target,
        agent.pane,
        agent.state.as_str(),
        action.as_str()
    );
    if action != KeepAction::Nudge {
        return Ok(());
    }
    if keep_composer_dirty(&screen, &options.message) {
        send_key(host, &agent.pane, "C-u")?;
    }
    deliver(host, &agent.pane, options.message.as_bytes(), true)?;
    for attempt in 1..=KEEP_SUBMIT_RETRIES {
        std::thread::sleep(std::time::Duration::from_millis(KEEP_SUBMIT_WAIT_MS));
        let after = capture_on(host, &agent.pane);
        if keep_submit_landed(&after, &options.message) {
            println!(
                "al supervise  keep {}  {} submitted",
                options.target, agent.pane
            );
            return Ok(());
        }
        if attempt < KEEP_SUBMIT_RETRIES {
            send_key(host, &agent.pane, "C-m")?;
        }
    }
    println!(
        "al supervise  keep {}  {} submit unconfirmed",
        options.target, agent.pane
    );
    Ok(())
}

fn keep_action(state: AgentState, screen: &str) -> KeepAction {
    if is_picker(&last_nonempty(screen, STRUCT_LINES)) {
        return KeepAction::SkipPicker;
    }
    match state {
        AgentState::Working => KeepAction::SkipWorking,
        AgentState::Asking => KeepAction::SkipAsking,
        AgentState::Idle | AgentState::Blocked | AgentState::Unknown => KeepAction::Nudge,
    }
}

fn composer_has_unknown_paste(text: &str) -> bool {
    text.contains("[Pasted text")
}

fn looks_like_idle_followup(screen: &str) -> bool {
    let last = last_nonempty(screen, STRUCT_LINES).to_ascii_lowercase();
    last.contains("add a follow-up")
        || last.contains("add a followup")
        || composer_has_unknown_paste(screen)
}

fn keep_composer_dirty(screen: &str, message: &str) -> bool {
    composer_has_unknown_paste(screen)
        || (!message.is_empty() && looks_like_idle_followup(screen) && screen.contains(message))
}

fn keep_still_unsent(screen: &str, message: &str) -> bool {
    composer_has_unknown_paste(screen)
        || (looks_like_idle_followup(screen) && screen.contains(message))
}

fn keep_submit_landed(screen: &str, message: &str) -> bool {
    match classify_screen(screen, "", false) {
        AgentState::Working | AgentState::Asking | AgentState::Blocked => true,
        AgentState::Idle | AgentState::Unknown => !keep_still_unsent(screen, message),
    }
}

fn capture_on(host: &str, pane: &str) -> String {
    match tmux_on(
        host,
        &[
            "capture-pane",
            "-p",
            "-J",
            "-S",
            "-30",
            "-E",
            "-",
            "-t",
            pane,
        ],
    ) {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => String::new(),
    }
}

fn send_key(host: &str, pane: &str, key: &str) -> Result<()> {
    let sent = tmux_on(host, &["send-keys", "-t", pane, key])?;
    if !sent.status.success() {
        bail!(
            "tmux send-keys {key} failed: {}",
            String::from_utf8_lossy(&sent.stderr)
        );
    }
    Ok(())
}

pub fn run_diff(options: &DiffOptions) -> Result<()> {
    let host = options
        .host
        .as_deref()
        .filter(|host| *host != "local")
        .unwrap_or("local");
    let agents = scan_host(host, GitMode::Diff)?;
    let agent = resolve_agent(&agents, options.target.as_deref())?;
    let cwd = if agent.cwd.is_empty() {
        "-"
    } else {
        agent.cwd.as_str()
    };
    if agent.diff_summary.is_empty() {
        println!("== {} {cwd} ==", agent.host);
    } else {
        println!("== {} {cwd} {} ==", agent.host, agent.diff_summary);
    }
    match agent
        .diff
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(diff) => println!("{diff}"),
        None if agent.diff_summary == "-" => {
            bail!("not a git worktree: {cwd}")
        }
        None if agent.diff_summary.is_empty() => {}
        None => println!("(no textual diff)"),
    }
    Ok(())
}

struct HostScan {
    agents: Vec<LiveAgent>,
    failed: bool,
    failed_hosts: Vec<String>,
}

fn collect_agents(hosts: &[String], mode: GitMode) -> HostScan {
    if hosts.is_empty() {
        return match scan_host("local", mode) {
            Ok(agents) => HostScan {
                agents,
                failed: false,
                failed_hosts: Vec::new(),
            },
            Err(error) => {
                eprintln!("al: scan failed for host \"local\": {error}");
                HostScan {
                    agents: Vec::new(),
                    failed: true,
                    failed_hosts: vec!["local".to_owned()],
                }
            }
        };
    }
    if hosts.len() == 1 {
        let host = &hosts[0];
        return match scan_host(host, mode) {
            Ok(agents) => HostScan {
                agents,
                failed: false,
                failed_hosts: Vec::new(),
            },
            Err(error) => {
                eprintln!("al: scan failed for host {host:?}: {error}");
                HostScan {
                    agents: Vec::new(),
                    failed: true,
                    failed_hosts: vec![host.clone()],
                }
            }
        };
    }

    let mut agents = Vec::new();
    let mut failed_hosts = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|host| {
                scope.spawn(|| match scan_host(host, mode) {
                    Ok(rows) => Ok(rows),
                    Err(error) => Err((host.clone(), error)),
                })
            })
            .collect();
        for handle in handles {
            match handle.join().expect("host scan thread") {
                Ok(rows) => agents.extend(rows),
                Err((host, error)) => {
                    eprintln!("al: scan failed for host {host:?}: {error}");
                    failed_hosts.push(host);
                }
            }
        }
    });
    HostScan {
        agents,
        failed: !failed_hosts.is_empty(),
        failed_hosts,
    }
}

fn scan_host(host: &str, mode: GitMode) -> Result<Vec<LiveAgent>> {
    let mut snapshot = if host == "local" {
        collect_local_snapshot()
    } else {
        collect_remote_snapshot(host, mode)?
    };
    let mut agents = agents_from_snapshot(host, &snapshot);
    if host == "local" && mode.want_status() {
        snapshot.gits = local_gits_for(&agents, mode.want_diff());
    }
    attach_git(&mut agents, &snapshot, mode);
    Ok(agents)
}

fn collect_local_snapshot() -> Snapshot {
    let mut snapshot = Snapshot {
        panes: list_panes().unwrap_or_default(),
        processes: list_processes().unwrap_or_default(),
        ..Snapshot::default()
    };
    let mut pids: Vec<i32> = snapshot.panes.iter().map(|pane| pane.pid).collect();
    for pane in &snapshot.panes {
        for process in foreground_job(pane.pid, &snapshot.processes) {
            pids.push(process.pid);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    for pid in pids {
        if let Some(cwd) = process_cwd(pid) {
            snapshot.proc_cwds.insert(pid, cwd);
        }
    }
    for pane in &snapshot.panes {
        snapshot
            .captures
            .insert(pane.pane_id.clone(), capture_pane(&pane.pane_id));
    }
    snapshot
}

fn collect_remote_snapshot(host: &str, mode: GitMode) -> Result<Snapshot> {
    let output = crate::remote::spawn_with(
        host,
        &["sh", "-s", "--", mode.remote_arg()],
        false,
        |command| {
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        },
    )
    .and_then(|mut child| {
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(REMOTE_DUMP_SCRIPT.as_bytes())?;
        }
        child.wait_with_output()
    })
    .with_context(|| {
        format!(
            "could not run {} to {host}",
            crate::remote::preferred_for(host).as_str()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} {host} exited with {}{}",
            crate::remote::preferred_for(host).as_str(),
            output.status,
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        );
    }
    parse_snapshot(&String::from_utf8_lossy(&output.stdout))
}

fn agents_from_snapshot(host: &str, snapshot: &Snapshot) -> Vec<LiveAgent> {
    let now = unix_now();
    let _lock = PANE_MEMORY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut memory = load_memory();
    let mut agents = Vec::new();
    for pane in &snapshot.panes {
        let job = foreground_job(pane.pid, &snapshot.processes);
        let Some(agent) = identify_agent(&job, group_leader(&job)) else {
            continue;
        };
        let capture = snapshot
            .captures
            .get(&pane.pane_id)
            .cloned()
            .unwrap_or_default();
        let key = format!("{host}:{}", pane.pane_id);
        let hash = content_hash(&pane_activity_key(&capture, &pane.title));
        let previous = memory.get(&key).cloned().unwrap_or_default();
        let changed = previous.hash != 0 && previous.hash != hash;
        let changed_at = if previous.hash == 0 || changed {
            now
        } else {
            previous.changed_at
        };
        memory.insert(key, PaneMemory { hash, changed_at });
        let cwd = agent_cwd(pane, &job, &snapshot.proc_cwds);
        let state = classify_screen(&capture, &pane.title, changed);
        agents.push(LiveAgent {
            host: host.to_owned(),
            session: pane.session.clone(),
            window: pane.window.clone(),
            pane: pane.pane_id.clone(),
            target: format!("{}:{}.{}", pane.session, pane.window_index, pane.pane_index),
            cwd,
            agent,
            branch: String::new(),
            diff_summary: "-".to_owned(),
            diff: None,
            state,
            idle_secs: idle_since(now, pane.activity),
            snippet: snippet(&capture),
        });
    }
    save_memory(&memory);
    agents.sort_by(|left, right| {
        left.state
            .cmp(&right.state)
            .then_with(|| left.cwd.cmp(&right.cwd))
            .then_with(|| left.agent.cmp(&right.agent))
            .then_with(|| left.pane.cmp(&right.pane))
    });
    agents
}

fn agent_cwd(pane: &Pane, job: &[Process], proc_cwds: &HashMap<i32, String>) -> String {
    for process in job {
        if identify_process(process).is_some() {
            if let Some(cwd) = proc_cwds.get(&process.pid) {
                if !cwd.is_empty() {
                    return cwd.clone();
                }
            }
        }
    }
    proc_cwds
        .get(&pane.pid)
        .filter(|cwd| !cwd.is_empty())
        .cloned()
        .unwrap_or_else(|| pane.cwd.clone())
}

fn attach_git(agents: &mut [LiveAgent], snapshot: &Snapshot, mode: GitMode) {
    if !mode.want_status() {
        for agent in agents {
            agent.branch.clear();
            agent.diff_summary.clear();
            agent.diff = None;
        }
        return;
    }
    let pane_cwd: HashMap<&str, &str> = snapshot
        .panes
        .iter()
        .map(|pane| (pane.pane_id.as_str(), pane.cwd.as_str()))
        .collect();
    for agent in agents {
        let git = snapshot.gits.get(&agent.cwd).or_else(|| {
            pane_cwd
                .get(agent.pane.as_str())
                .and_then(|cwd| snapshot.gits.get(*cwd))
        });
        let Some(git) = git else {
            continue;
        };
        agent.branch = git.branch.clone();
        agent.diff_summary = git.summary();
        if mode.want_diff() {
            agent.diff = git.diff.clone();
        }
    }
}

fn local_gits_for(agents: &[LiveAgent], want_diff: bool) -> HashMap<String, GitInfo> {
    let mut gits = HashMap::new();
    for agent in agents {
        if agent.cwd.is_empty() || gits.contains_key(&agent.cwd) {
            continue;
        }
        gits.insert(agent.cwd.clone(), inspect_git(&agent.cwd, want_diff));
    }
    gits
}

fn inspect_git(cwd: &str, want_diff: bool) -> GitInfo {
    if cwd.is_empty() {
        return GitInfo::default();
    }
    let Ok(inside) = git_stdout(cwd, &["rev-parse", "--is-inside-work-tree"]) else {
        return GitInfo::default();
    };
    if inside.trim() != "true" {
        return GitInfo::default();
    }
    let status = git_stdout(cwd, &["status", "--porcelain=v1", "-b"]).unwrap_or_default();
    let stat = git_stdout(cwd, &["diff", "--shortstat", "HEAD"]).unwrap_or_default();
    let mut body = status;
    body.push_str("\n--STAT--\n");
    body.push_str(&stat);
    if want_diff {
        if let Ok(untracked) = git_stdout(cwd, &["ls-files", "--others", "--exclude-standard"]) {
            if !untracked.trim().is_empty() {
                body.push_str("\n--UNTRACKED--\n");
                body.push_str(&untracked);
            }
        }
        let diff = git_stdout(cwd, &["diff", "--no-color", "HEAD"]).unwrap_or_default();
        body.push_str("\n--DIFF--\n");
        body.push_str(&diff);
    }
    parse_git_body(&body)
}

fn git_stdout(cwd: &str, args: &[&str]) -> Result<String> {
    let cwd = if cwd.starts_with('-') {
        format!("./{cwd}")
    } else {
        cwd.to_owned()
    };
    let output = Command::new("git")
        .args(["--no-pager", "-C", &cwd])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        bail!("git exited {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum GitPart {
    Status,
    Stat,
    Untracked,
    Diff,
}

fn parse_git_body(body: &str) -> GitInfo {
    if body.trim().is_empty() {
        return GitInfo::default();
    }
    let mut info = GitInfo {
        inside: true,
        ..GitInfo::default()
    };
    let mut part = GitPart::Status;
    let mut status = String::new();
    let mut stat = String::new();
    let mut diff = String::new();
    let mut untracked = String::new();
    for line in body.lines() {
        if part != GitPart::Diff {
            if line == "--STAT--" && part == GitPart::Status {
                part = GitPart::Stat;
                continue;
            }
            if line == "--UNTRACKED--" && part < GitPart::Untracked {
                part = GitPart::Untracked;
                continue;
            }
            if line == "--DIFF--" {
                part = GitPart::Diff;
                continue;
            }
        }
        match part {
            GitPart::Stat => {
                stat.push_str(line);
                stat.push('\n');
            }
            GitPart::Diff => {
                diff.push_str(line);
                diff.push('\n');
            }
            GitPart::Untracked => {
                untracked.push_str(line);
                untracked.push('\n');
            }
            GitPart::Status => {
                status.push_str(line);
                status.push('\n');
            }
        }
    }
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            info.branch = rest
                .split("...")
                .next()
                .unwrap_or(rest)
                .split_whitespace()
                .next()
                .unwrap_or(rest)
                .to_owned();
            continue;
        }
        if line.len() < 2 {
            continue;
        }
        let code = &line[..2];
        if code == "??" {
            info.untracked += 1;
        } else if code != "!!" && !line.trim().is_empty() {
            info.files += 1;
        }
    }
    let (stat_files, insertions, deletions) = parse_shortstat(stat.trim());
    if stat_files > 0 {
        info.files = info.files.max(stat_files);
    }
    info.insertions = insertions;
    info.deletions = deletions;
    if !diff.trim().is_empty() || !untracked.trim().is_empty() {
        let mut text = diff;
        if !untracked.trim().is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("Untracked:\n");
            text.push_str(untracked.trim_end());
            text.push('\n');
        }
        info.diff = Some(text);
    }
    info
}

fn parse_shortstat(text: &str) -> (u32, u32, u32) {
    let mut files = 0;
    let mut insertions = 0;
    let mut deletions = 0;
    for part in text.split(',') {
        let part = part.trim();
        let count = part
            .split_whitespace()
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if part.contains("file") {
            files = count;
        } else if part.contains("insertion") {
            insertions = count;
        } else if part.contains("deletion") {
            deletions = count;
        }
    }
    (files, insertions, deletions)
}

fn resolve_agent<'a>(agents: &'a [LiveAgent], target: Option<&str>) -> Result<&'a LiveAgent> {
    let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
        return attention_agent(agents);
    };
    if target == "attention" {
        return attention_agent(agents);
    }
    let matches: Vec<&LiveAgent> = agents
        .iter()
        .filter(|agent| {
            agent.pane == target
                || agent.target == target
                || agent.agent == target
                || agent.session == target
                || agent.window == target
                || agent.cwd == target
                || short_cwd(&agent.cwd) == target
        })
        .collect();
    match matches.as_slice() {
        [agent] => Ok(agent),
        [] => bail!("no live agent matches {target:?}"),
        _ => bail!(
            "ambiguous target {target:?}: {}",
            matches
                .iter()
                .map(|agent| format!("{} {} {}", agent.agent, agent.cwd, agent.pane))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn attention_agent(agents: &[LiveAgent]) -> Result<&LiveAgent> {
    if agents.is_empty() {
        bail!("no live agents on this host");
    }
    agents
        .iter()
        .find(|agent| agent.state == AgentState::Blocked)
        .or_else(|| {
            agents
                .iter()
                .find(|agent| agent.state == AgentState::Asking)
        })
        .ok_or_else(|| {
            let summary = agents
                .iter()
                .map(|agent| format!("{}:{}", agent.agent, agent.state.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!("no blocked or asking agent on this host ({summary})")
        })
}

fn deliver(host: &str, pane: &str, message: &[u8], submit: bool) -> Result<()> {
    if message.contains(&0) {
        bail!("message contains a NUL byte");
    }
    let buffer = format!("al-live-{}", std::process::id());
    let mut load = if host == "local" {
        Command::new("tmux")
            .args(["load-buffer", "-b", &buffer, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    } else {
        crate::remote::spawn_with(
            host,
            &["tmux", "load-buffer", "-b", &buffer, "-"],
            false,
            |command| {
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped());
            },
        )
    }
    .context("could not start tmux load-buffer")?;
    {
        let stdin = load.stdin.as_mut().context("tmux load-buffer stdin")?;
        stdin.write_all(message)?;
    }
    let loaded = load.wait_with_output()?;
    if !loaded.status.success() {
        bail!(
            "tmux load-buffer failed: {}",
            String::from_utf8_lossy(&loaded.stderr)
        );
    }
    let paste = tmux_on(host, &["paste-buffer", "-d", "-b", &buffer, "-t", pane])?;
    if !paste.status.success() {
        let _ = tmux_on(host, &["delete-buffer", "-b", &buffer]);
        bail!(
            "tmux paste-buffer failed: {}",
            String::from_utf8_lossy(&paste.stderr)
        );
    }
    if submit {
        let sent = tmux_on(host, &["send-keys", "-t", pane, "C-m", "C-m"])?;
        if !sent.status.success() {
            bail!(
                "tmux send-keys failed: {}",
                String::from_utf8_lossy(&sent.stderr)
            );
        }
    }
    Ok(())
}

fn tmux_on(host: &str, args: &[&str]) -> Result<std::process::Output> {
    if host == "local" {
        Command::new("tmux")
            .args(args)
            .output()
            .context("could not run tmux")
    } else {
        let mut remote = Vec::with_capacity(args.len() + 1);
        remote.push("tmux");
        remote.extend(args.iter().copied());
        crate::remote::output(host, &remote, false)
            .with_context(|| format!("could not run tmux on {host}"))
    }
}

const REMOTE_DUMP_SCRIPT: &str = r#"
TOKEN=$(od -An -N8 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')
[ "${#TOKEN}" -ge 8 ] || TOKEN="t$$"
printf '%s %s\n' 'AL_LIVE_1' "$TOKEN"
printf '%s\n' 'PANES'
tmux list-panes -a -F '#{pane_id}	#{session_name}	#{window_name}	#{window_index}	#{pane_index}	#{pane_pid}	#{pane_current_path}	#{pane_activity}	#{pane_title}' 2>/dev/null || true
printf '%s\n' 'PS'
ps -axo pid=,ppid=,pgid=,tpgid=,comm=,args= 2>/dev/null || true
PANE_PIDS=$(tmux list-panes -a -F '#{pane_pid}' 2>/dev/null || true)
TREE_PIDS=$(ps -axo pid=,ppid= 2>/dev/null | awk -v panes="$PANE_PIDS" '
BEGIN {
  n = split(panes, a, /[[:space:]]+/)
  for (i = 1; i <= n; i++) if (a[i] != "") keep[a[i]] = 1
}
{
  pid = $1 + 0
  ppid = $2 + 0
  if (pid == 0) next
  parent[pid] = ppid
  list[++count] = pid
}
END {
  changed = 1
  while (changed) {
    changed = 0
    for (i = 1; i <= count; i++) {
      pid = list[i]
      if (keep[pid]) continue
      if (parent[pid] in keep) { keep[pid] = 1; changed = 1 }
    }
  }
  for (pid in keep) print pid
}')
printf '%s\n' 'CWDS'
printf '%s\n' "$TREE_PIDS" | while read -r pid; do
  [ -n "$pid" ] || continue
  cwd=$(readlink "/proc/$pid/cwd" 2>/dev/null) || continue
  printf '%s\t%s\n' "$pid" "$cwd"
done
tmux list-panes -a -F '#{pane_id}' 2>/dev/null | while read -r pane; do
  printf '%s\n' "CAPTURE $TOKEN $pane"
  tmux capture-pane -p -J -S - -E - -t "$pane" 2>/dev/null || true
  printf '\n%s\n' "ENDCAPTURE $TOKEN $pane"
done
MODE=${1:-status}
if [ "$MODE" = status ] || [ "$MODE" = diff ]; then
  printf '%s\n' 'GITS'
  {
    tmux list-panes -a -F '#{pane_current_path}' 2>/dev/null || true
    printf '%s\n' "$TREE_PIDS" | while read -r pid; do
      [ -n "$pid" ] || continue
      readlink "/proc/$pid/cwd" 2>/dev/null || true
    done
  } | awk 'NF && !seen[$0]++' | while IFS= read -r cwd; do
    case $cwd in
      -*) cwd="./$cwd" ;;
    esac
    printf '%s\n' "GIT $TOKEN $cwd"
    if git -C "$cwd" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
      GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 git -C "$cwd" --no-pager status --porcelain=v1 -b 2>/dev/null || true
      printf '%s\n' '--STAT--'
      GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 git -C "$cwd" --no-pager diff --shortstat HEAD 2>/dev/null || true
      if [ "$MODE" = diff ]; then
        untracked=$(GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 git -C "$cwd" --no-pager ls-files --others --exclude-standard 2>/dev/null || true)
        if [ -n "$untracked" ]; then
          printf '%s\n' '--UNTRACKED--'
          printf '%s\n' "$untracked"
        fi
        printf '%s\n' '--DIFF--'
        GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 git -C "$cwd" --no-pager diff --no-color HEAD 2>/dev/null || true
      fi
    fi
    printf '\n%s\n' "ENDGIT $TOKEN $cwd"
  done
fi
"#;

fn parse_dump_header(line: &str) -> Result<Option<String>> {
    let Some(rest) = line.strip_prefix(DUMP_VERSION) else {
        bail!("unexpected live dump from remote host");
    };
    let rest = rest.trim();
    if rest.is_empty() {
        Ok(None)
    } else if rest.chars().any(char::is_whitespace) {
        bail!("unexpected live dump from remote host");
    } else {
        Ok(Some(rest.to_owned()))
    }
}

fn frame_id<'a>(line: &'a str, prefix: &str, token: Option<&str>) -> Option<&'a str> {
    let rest = line.strip_prefix(prefix)?;
    match token {
        Some(token) => rest.strip_prefix(token)?.strip_prefix(' '),
        None => Some(rest),
    }
}

fn parse_snapshot(text: &str) -> Result<Snapshot> {
    let mut lines = text.lines();
    let token = parse_dump_header(lines.next().unwrap_or_default())?;
    let token = token.as_deref();
    let mut snapshot = Snapshot::default();
    let mut section = "";
    let mut capture_pane = None;
    let mut capture = String::new();
    let mut git_cwd = None;
    let mut git_body = String::new();
    for line in lines {
        if capture_pane.is_some() {
            if let Some(pane) = frame_id(line, "ENDCAPTURE ", token) {
                if Some(pane) == capture_pane.as_deref() {
                    snapshot
                        .captures
                        .insert(pane.to_owned(), capture.trim_end_matches('\n').to_owned());
                    capture_pane = None;
                    capture.clear();
                    continue;
                }
            }
            if !capture.is_empty() {
                capture.push('\n');
            }
            capture.push_str(line);
            continue;
        }
        if git_cwd.is_some() {
            if let Some(cwd) = frame_id(line, "ENDGIT ", token) {
                if Some(cwd) == git_cwd.as_deref() {
                    snapshot
                        .gits
                        .insert(cwd.to_owned(), parse_git_body(&git_body));
                    git_cwd = None;
                    git_body.clear();
                    continue;
                }
            }
            if !git_body.is_empty() {
                git_body.push('\n');
            }
            git_body.push_str(line);
            continue;
        }
        if line == "PANES" || line == "PS" || line == "CWDS" || line == "GITS" {
            section = line;
            continue;
        }
        if let Some(pane) = frame_id(line, "CAPTURE ", token) {
            capture_pane = Some(pane.to_owned());
            capture.clear();
            continue;
        }
        if let Some(cwd) = frame_id(line, "GIT ", token) {
            git_cwd = Some(cwd.to_owned());
            git_body.clear();
            continue;
        }
        match section {
            "PANES" => {
                if let Some(pane) = parse_pane_line(line) {
                    snapshot.panes.push(pane);
                }
            }
            "PS" => snapshot
                .processes
                .extend(parse_processes(&format!("{line}\n"))),
            "CWDS" => {
                let mut fields = line.splitn(2, '\t');
                if let (Some(pid), Some(cwd)) = (fields.next(), fields.next()) {
                    if let Ok(pid) = pid.trim().parse() {
                        snapshot.proc_cwds.insert(pid, cwd.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    Ok(snapshot)
}

fn parse_panes(text: &str) -> Vec<Pane> {
    text.lines().filter_map(parse_pane_line).collect()
}

fn parse_pane_line(line: &str) -> Option<Pane> {
    let mut fields = line.splitn(9, '\t');
    let pane_id = fields.next()?;
    if !pane_id.starts_with('%') {
        return None;
    }
    let session = fields.next()?;
    let window = fields.next()?;
    let window_index = fields.next()?;
    let pane_index = fields.next()?;
    let pid = fields.next()?.parse().ok()?;
    let cwd = fields.next().unwrap_or("").to_owned();
    let eighth = fields.next().unwrap_or("");
    let (activity, title) = match fields.next() {
        Some(title) => (eighth.parse().unwrap_or(0), title.to_owned()),
        None => (0, eighth.to_owned()),
    };
    Some(Pane {
        pane_id: pane_id.to_owned(),
        session: session.to_owned(),
        window: window.to_owned(),
        window_index: window_index.to_owned(),
        pane_index: pane_index.to_owned(),
        pid,
        cwd,
        activity,
        title,
    })
}

fn list_panes() -> Result<Vec<Pane>> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{session_name}\t#{window_name}\t#{window_index}\t#{pane_index}\t#{pane_pid}\t#{pane_current_path}\t#{pane_activity}\t#{pane_title}",
        ])
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        _ => return Ok(Vec::new()),
    };
    Ok(parse_panes(&String::from_utf8_lossy(&output.stdout)))
}

fn capture_pane(pane_id: &str) -> String {
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-p",
            "-J",
            "-S",
            "-",
            "-E",
            "-",
            "-t",
            pane_id,
        ])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => String::new(),
    }
}

fn process_cwd(pid: i32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
}

fn list_processes() -> Result<Vec<Process>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,tpgid=,comm=,args="])
        .output()
        .context("could not list processes")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_processes(&String::from_utf8_lossy(&output.stdout)))
}

pub fn parse_processes(text: &str) -> Vec<Process> {
    let mut processes = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(pgid), Some(tpgid), Some(command)) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            continue;
        };
        let argv_text = parts.collect::<Vec<_>>().join(" ");
        let argv = shlex_split(if argv_text.is_empty() {
            command
        } else {
            &argv_text
        });
        let Ok(pid) = pid.parse() else { continue };
        let Ok(ppid) = ppid.parse() else { continue };
        let Ok(pgid) = pgid.parse() else { continue };
        let Ok(tpgid) = tpgid.parse() else { continue };
        processes.push(Process {
            pid,
            ppid,
            pgid,
            tpgid,
            command: command.to_owned(),
            argv,
        });
    }
    processes
}

fn foreground_job(pane_pid: i32, processes: &[Process]) -> Vec<Process> {
    let mut descendants = vec![pane_pid];
    let mut changed = true;
    while changed {
        changed = false;
        for process in processes {
            if descendants.contains(&process.ppid) && !descendants.contains(&process.pid) {
                descendants.push(process.pid);
                changed = true;
            }
        }
    }
    let shell = processes.iter().find(|process| process.pid == pane_pid);
    let tpgid = shell.and_then(|process| (process.tpgid > 0).then_some(process.tpgid));
    let job: Vec<Process> = processes
        .iter()
        .filter(|process| {
            descendants.contains(&process.pid) && tpgid.is_none_or(|leader| process.pgid == leader)
        })
        .cloned()
        .collect();
    if job.is_empty() {
        processes
            .iter()
            .filter(|process| descendants.contains(&process.pid) && process.pid != pane_pid)
            .cloned()
            .collect()
    } else {
        job
    }
}

fn group_leader(job: &[Process]) -> Option<i32> {
    job.first()
        .map(|process| process.pgid)
        .filter(|pgid| *pgid > 0)
}

pub fn identify_agent(processes: &[Process], group_leader: Option<i32>) -> Option<String> {
    if let Some(leader) = group_leader {
        if let Some(process) = processes.iter().find(|process| process.pid == leader) {
            if let Some(agent) = identify_process(process) {
                return Some(agent);
            }
        }
    }
    let mut best: Option<(u8, String)> = None;
    for process in processes {
        let Some(agent) = identify_process(process) else {
            continue;
        };
        let score = if canonical_agent(&process.command).is_none() {
            3
        } else {
            2
        };
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, agent));
        }
    }
    best.map(|(_, agent)| agent)
}

fn identify_process(process: &Process) -> Option<String> {
    if let Some(agent) = canonical_agent(&process.command) {
        return Some(agent.to_owned());
    }
    let runtime = normalize_program(&process.command);
    if RUNTIMES.contains(&runtime.as_str()) || runtime.starts_with("python") {
        if let Some(agent) = wrapped_agent(&runtime, &process.argv) {
            return Some(agent.to_owned());
        }
    }
    process
        .argv
        .first()
        .and_then(|value| agent_from_path(value).map(str::to_owned))
}

fn wrapped_agent(runtime: &str, argv: &[String]) -> Option<&'static str> {
    if argv.is_empty() {
        return None;
    }
    let args = &argv[1..];
    let value_options = [
        "-r",
        "--require",
        "--loader",
        "--import",
        "--experimental-loader",
        "--inspect-port",
        "-W",
        "-X",
        "-S",
        "-L",
        "-o",
    ];
    let eval_options: &[&str] = if matches!(runtime, "node" | "bun") {
        &["-e", "--eval", "-p", "--print"]
    } else {
        &["-c"]
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return args.get(index + 1).and_then(|value| agent_from_path(value));
        }
        if matches!(runtime, "sh" | "bash" | "zsh" | "fish") && arg == "-c" {
            let token = args
                .get(index + 1)
                .and_then(|command| first_command_token(command))?;
            return agent_from_path(&token);
        }
        if eval_options.contains(&arg.as_str())
            || eval_options
                .iter()
                .any(|flag| flag.starts_with("--") && arg.starts_with(&format!("{flag}=")))
        {
            return None;
        }
        if runtime.starts_with("python") && arg == "-m" {
            return None;
        }
        if arg.starts_with('-') {
            index += if value_options.contains(&arg.as_str()) {
                2
            } else {
                1
            };
            continue;
        }
        return agent_from_path(arg);
    }
    None
}

fn first_command_token(command: &str) -> Option<String> {
    let mut tokens = shlex_split(command);
    while tokens
        .first()
        .is_some_and(|token| matches!(token.as_str(), "&" | "." | "call" | "exec" | "command"))
    {
        tokens.remove(0);
    }
    tokens.into_iter().next()
}

fn agent_from_path(value: &str) -> Option<&'static str> {
    let cleaned = value.trim_matches(|ch| ch == '"' || ch == '\'');
    if let Some(agent) = canonical_agent(cleaned) {
        return Some(agent);
    }
    let components: Vec<String> = cleaned
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .map(normalize_program)
        .collect();
    let package = [
        "node_modules",
        "@earendil-works",
        "pi-coding-agent",
        "dist",
        "cli",
    ];
    if components
        .windows(package.len())
        .any(|window| window == package)
        || components.iter().any(|part| part == "pi-coding-agent")
    {
        return Some("pi");
    }
    None
}

fn canonical_agent(value: &str) -> Option<&'static str> {
    let name = normalize_program(value);
    AGENT_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, agent)| *agent)
}

fn agent_from_tmux_name(name: &str) -> Option<&'static str> {
    NAME_PREFIXES
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix))
        .map(|(_, agent)| *agent)
}

fn normalize_program(value: &str) -> String {
    let name = value
        .trim_matches(|ch| ch == '"' || ch == '\'')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1", ".js"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped.to_owned();
        }
    }
    name
}

pub fn classify_screen(text: &str, title: &str, hash_changed: bool) -> AgentState {
    let structural = last_nonempty(text, STRUCT_LINES);
    let words = last_nonempty(text, WORD_LINES);
    if is_picker(&structural) || is_asking(text) {
        return AgentState::Asking;
    }
    if is_blocked(&words) {
        return AgentState::Blocked;
    }
    if is_working_now(&structural, &words) || is_working_now(title, title) {
        return AgentState::Working;
    }
    if hash_changed {
        return AgentState::Working;
    }
    AgentState::Idle
}

fn is_working_now(structural: &str, words: &str) -> bool {
    is_working_struct(structural) || is_working_words(words)
}

fn is_working_struct(text: &str) -> bool {
    if text
        .chars()
        .any(|ch| SPINNERS.contains(ch) || ('\u{2800}'..='\u{28FF}').contains(&ch))
    {
        return true;
    }
    let folded = text.to_ascii_lowercase();
    folded.contains("working...")
        || folded.contains("esc to interrupt")
        || folded.contains("ctrl+c to stop")
}

fn is_working_words(text: &str) -> bool {
    let folded = text.to_ascii_lowercase();
    folded.contains("thinking") || folded.contains("generating")
}

fn is_blocked(text: &str) -> bool {
    text.contains("FAILED")
        || has_word(text, "blocked")
        || has_word(text, "eacces")
        || text.to_ascii_lowercase().contains("needs attention")
        || text.to_ascii_lowercase().contains("permission denied")
        || text.to_ascii_lowercase().contains("panic!")
}

fn has_word(text: &str, word: &str) -> bool {
    text.to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| token == word)
}

fn is_asking(text: &str) -> bool {
    let last = last_nonempty(text, 1);
    let trimmed = last.trim();
    let folded = trimmed.to_ascii_lowercase();
    trimmed.ends_with('?')
        || trimmed.contains('❓')
        || folded.contains("which ")
        || folded.contains("how should")
        || folded.contains("please confirm")
        || folded.contains("confirm?")
        || folded
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .next_back()
            == Some("confirm")
}

fn is_picker(text: &str) -> bool {
    text.contains('⎋') || text.lines().any(|line| line.trim_start().starts_with("❯ "))
}

fn last_nonempty(text: &str, count: usize) -> String {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

fn snippet(text: &str) -> String {
    let lines = last_nonempty_lines(text, SNIPPET_SCAN);
    let line = lines
        .iter()
        .rev()
        .map(|line| clean_visible(line))
        .find(|line| !is_chrome_line(line))
        .unwrap_or_default();
    ellipsize(&line, SNIPPET_CHARS)
}

fn last_nonempty_lines(text: &str, count: usize) -> Vec<String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(str::to_owned)
        .collect()
}

fn clean_visible(line: &str) -> String {
    let cleaned: String = line
        .chars()
        .filter(|ch| *ch >= ' ' && *ch != '\u{7f}')
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_chrome_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    let letters = trimmed.chars().filter(|ch| ch.is_alphabetic()).count();
    let boxes = trimmed.chars().filter(|ch| is_box_char(*ch)).count();
    if boxes > 0 && boxes >= letters {
        return true;
    }
    if trimmed.starts_with(['╰', '╭', '┌', '└', '│', '┃', '├', '┤']) {
        return true;
    }
    if looks_like_path_status(trimmed)
        || looks_like_shell_prompt(trimmed)
        || looks_like_usage_bar(trimmed)
    {
        return true;
    }
    if trimmed.contains("Add a follow-up") || trimmed.contains("ctrl+c to stop") {
        return true;
    }
    if trimmed.starts_with("Goal active") {
        return true;
    }
    if trimmed.eq_ignore_ascii_case("1 task")
        || trimmed
            .strip_suffix(" tasks")
            .is_some_and(|count| count.chars().all(|ch| ch.is_ascii_digit()))
    {
        return true;
    }
    if trimmed.starts_with("Cursor ")
        && (trimmed.contains('%')
            || trimmed.contains("files edited")
            || trimmed.contains("Run Everything"))
    {
        return true;
    }
    if looks_like_resume_command(trimmed) || trimmed.starts_with("Resume this session with") {
        return true;
    }
    false
}

fn is_box_char(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{259F}')
}

fn looks_like_path_status(line: &str) -> bool {
    if !(line.starts_with("~/") || line.starts_with('/')) {
        return false;
    }
    line.contains('·') || line.contains(" • ") || (line.contains(" (") && line.ends_with(')'))
}

fn looks_like_shell_prompt(line: &str) -> bool {
    (line.contains('@')
        && (line.ends_with('>') || line.ends_with('$') || line.ends_with('%'))
        && (line.contains("~/") || line.contains(" ~") || line.contains('(')))
        || (line.starts_with('(') && line.contains('@') && line.ends_with('>'))
}

fn looks_like_resume_command(line: &str) -> bool {
    let Some(id) = line
        .split_whitespace()
        .skip_while(|token| *token != "--resume")
        .nth(1)
    else {
        return false;
    };
    id.len() >= 16 && id.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

fn looks_like_usage_bar(line: &str) -> bool {
    let arrows = line.bytes().filter(|byte| *byte == b'>').count();
    (line.contains('↑') && line.contains('↓'))
        || (arrows >= 2
            && (line.contains('$')
                || line.contains('%')
                || line.contains('📁')
                || line.contains("Goal")))
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{kept}...")
}

fn idle_since(now: u64, activity: u64) -> u64 {
    if activity == 0 || activity > now {
        0
    } else {
        now.saturating_sub(activity)
    }
}

fn format_idle(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn pick_and_attach(agents: &[LiveAgent], query: &str) -> Result<()> {
    if agents.is_empty() {
        bail!("no live agents");
    }
    let color = crate::picker::use_color_for_picker();
    let lines: Vec<String> = agents
        .iter()
        .map(|agent| format_live_picker_line(agent, color))
        .collect();
    match crate::picker::pick_tsv_line(&lines, "agents> ", Some(query))? {
        crate::picker::LineOutcome::Cancelled => bail!("no live agent selected"),
        crate::picker::LineOutcome::Error(code) => bail!("fzf exited {code}"),
        crate::picker::LineOutcome::Selected(line) => {
            let key = parse_live_picker_line(&line)?;
            let agent = agents
                .iter()
                .find(|agent| agent.host == key.host && agent.pane == key.pane)
                .ok_or_else(|| anyhow::anyhow!("selected pane is no longer live"))?;
            attach_agent(agent)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PickerKey {
    host: String,
    pane: String,
    session: String,
    target: String,
}

fn format_live_picker_line(agent: &LiveAgent, color: bool) -> String {
    let state = agent.state.as_str();
    let cwd = short_cwd(&agent.cwd);
    let branch = if agent.branch.is_empty() {
        "-"
    } else {
        agent.branch.as_str()
    };
    let host = if agent.host == "local" {
        String::new()
    } else {
        format!("{}  ", agent.host)
    };
    let state_cell = if color {
        format!("{}{state:<8}\x1b[0m", agent.state.color())
    } else {
        format!("{state:<8}")
    };
    let session = if agent.session.is_empty() || numeric_session(&agent.session) {
        "-"
    } else {
        agent.session.as_str()
    };
    let idle = if agent.idle_secs == 0 {
        "-".to_owned()
    } else {
        format_idle(agent.idle_secs)
    };
    let snippet = if agent.snippet.is_empty() {
        "-"
    } else {
        agent.snippet.as_str()
    };
    let display = sanitize_picker(&format!(
        "{state_cell} {:<6} {host}{cwd} · {branch}  {}  {}  {session}  {idle}  {snippet}",
        agent.agent, agent.diff_summary, agent.pane
    ));
    format!(
        "{display}\t{}\t{}\t{}\t{}",
        sanitize_picker(&agent.host),
        sanitize_picker(&agent.pane),
        sanitize_picker(&agent.session),
        sanitize_picker(&attach_target(agent))
    )
}

fn parse_live_picker_line(line: &str) -> Result<PickerKey> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < 5 {
        bail!("invalid live picker selection");
    }
    Ok(PickerKey {
        host: fields[1].to_owned(),
        pane: fields[2].to_owned(),
        session: fields[3].to_owned(),
        target: fields[4].to_owned(),
    })
}

fn sanitize_picker(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\t' | '\n' | '\r' => ' ',
            other => other,
        })
        .collect()
}

fn attach_all(agents: &[LiveAgent]) -> Result<()> {
    let plan = attach_all_plan(agents);
    if plan.is_empty() {
        bail!("no live agents");
    }
    for (host, sessions) in &plan {
        create_host_aggregator(host, sessions)?;
    }
    attach_local_session(&plan[0].0)
}

fn attach_all_plan(agents: &[LiveAgent]) -> Vec<(String, Vec<String>)> {
    let mut hosts: Vec<String> = Vec::new();
    for agent in agents {
        if !hosts.iter().any(|host| host == &agent.host) {
            hosts.push(agent.host.clone());
        }
    }
    hosts.sort_by(|left, right| host_rank(left).cmp(&host_rank(right)).then(left.cmp(right)));
    hosts
        .into_iter()
        .map(|host| {
            let mut sessions = Vec::new();
            for agent in agents {
                if agent.host == host && !sessions.iter().any(|session| session == &agent.session) {
                    sessions.push(agent.session.clone());
                }
            }
            sessions.sort();
            (host, sessions)
        })
        .filter(|(_, sessions)| !sessions.is_empty())
        .collect()
}

fn create_host_aggregator(host: &str, sessions: &[String]) -> Result<()> {
    let target = format!("={host}");
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", &target])
        .output();
    for (index, session) in sessions.iter().enumerate() {
        let window = sanitize_tmux_name(session);
        let mut command = Command::new("tmux");
        if index == 0 {
            command.args(["new-session", "-d", "-s", host, "-n", &window, "--"]);
        } else {
            command.args(["new-window", "-t", &target, "-n", &window, "--"]);
        }
        let status = command
            .args(session_attach_command(host, session))
            .status()
            .with_context(|| format!("could not create tmux window for {host} {session}"))?;
        if !status.success() {
            bail!(
                "could not create tmux window for {host} {session} (exit {})",
                status.code().unwrap_or(1)
            );
        }
    }
    Ok(())
}

fn session_attach_command(host: &str, session: &str) -> Vec<String> {
    let target = format!("={}", session.trim_start_matches('='));
    if host == "local" {
        vec![
            "env".into(),
            "-u".into(),
            "TMUX".into(),
            "tmux".into(),
            "attach-session".into(),
            "-t".into(),
            target,
        ]
    } else {
        crate::remote::argv(host, &["tmux", "attach-session", "-t", &target], true)
    }
}

fn sanitize_tmux_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| match ch {
            '.' | ':' | '\n' | '\r' | '\t' => '-',
            other => other,
        })
        .collect();
    if cleaned.is_empty() {
        "session".to_owned()
    } else {
        cleaned
    }
}

fn attach_local_session(name: &str) -> Result<()> {
    let target = format!("={name}");
    let argv = if env::var_os("TMUX").is_some() {
        vec!["tmux", "switch-client", "-t", &target]
    } else {
        vec!["tmux", "attach-session", "-t", &target]
    };
    let status = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("could not attach to host session")?;
    if status.success() {
        Ok(())
    } else {
        bail!("attach exited {}", status.code().unwrap_or(1))
    }
}

fn attach_target(agent: &LiveAgent) -> String {
    if agent.target.is_empty() {
        format!("={}", agent.session.trim_start_matches('='))
    } else {
        format!("={}", agent.target.trim_start_matches('='))
    }
}

fn attach_agent(agent: &LiveAgent) -> Result<()> {
    let argv = attach_argv(agent, env::var_os("TMUX").is_some());
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("could not run {}", argv[0]))?;
    if status.success() {
        Ok(())
    } else {
        bail!("attach exited {}", status.code().unwrap_or(1))
    }
}

fn attach_argv(agent: &LiveAgent, inside_tmux: bool) -> Vec<String> {
    let target = attach_target(agent);
    if agent.host != "local" {
        crate::remote::argv(
            &agent.host,
            &["tmux", "attach-session", "-t", &target],
            true,
        )
    } else if inside_tmux {
        vec!["tmux".into(), "switch-client".into(), "-t".into(), target]
    } else {
        vec!["tmux".into(), "attach-session".into(), "-t".into(), target]
    }
}

fn pane_activity_key(capture: &str, title: &str) -> String {
    format!("{title}\n{}", last_nonempty(capture, STRUCT_LINES))
}

fn content_hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn format_summary(
    label: &str,
    agents: &[LiveAgent],
    interval: u64,
    failed_hosts: &[String],
    color: bool,
) -> String {
    let mut blocked = 0;
    let mut asking = 0;
    let mut working = 0;
    let mut idle = 0;
    for agent in agents {
        match agent.state {
            AgentState::Blocked => blocked += 1,
            AgentState::Asking => asking += 1,
            AgentState::Working => working += 1,
            AgentState::Idle | AgentState::Unknown => idle += 1,
        }
    }
    let mut line = format!(
        "al {label}  {} agents  {blocked} blocked  {asking} asking  {working} working  {idle} idle  {interval}s",
        agents.len()
    );
    if !failed_hosts.is_empty() {
        line.push_str("  failed: ");
        line.push_str(&failed_hosts.join(","));
    }
    if color {
        format!("\x1b[1m{line}\x1b[0m")
    } else {
        line
    }
}

fn format_table(agents: &[LiveAgent], color: bool) -> String {
    let groups = grouped_agents(agents);
    if groups.is_empty() {
        return String::new();
    }
    let multi_host = groups
        .iter()
        .map(|group| group.host)
        .collect::<HashSet<_>>()
        .len()
        > 1;
    let mut rows: Vec<Vec<String>> = groups
        .iter()
        .flat_map(|group| group.agents.iter().copied().map(pane_cells))
        .collect();
    let snippet_idx = rows.first().map(|row| row.len() - 1).unwrap_or(0);
    let idle_idx = snippet_idx.saturating_sub(1);
    let widths: Vec<usize> = (0..snippet_idx)
        .map(|index| {
            rows.iter()
                .map(|row| display_width(&row[index]))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let indent = if multi_host { "    " } else { "  " };
    let used = indent.len() + widths.iter().sum::<usize>() + widths.len().saturating_mul(2);
    let snippet_budget = terminal_columns().saturating_sub(used).max(24);
    for row in &mut rows {
        if let Some(snippet) = row.get_mut(snippet_idx) {
            *snippet = ellipsize_width(snippet, snippet_budget);
        }
    }
    let mut out = String::new();
    let mut current_host = "";
    let mut row_index = 0;
    for group in &groups {
        if multi_host && group.host != current_host {
            if !out.is_empty() {
                out.push('\n');
            }
            if color {
                out.push_str("\x1b[1m");
                out.push_str(group.host);
                out.push_str("\x1b[0m\n");
            } else {
                out.push_str(group.host);
                out.push('\n');
            }
            current_host = group.host;
        } else if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&group_header(group, multi_host, color));
        out.push('\n');
        for agent in &group.agents {
            let mut cells = rows[row_index].clone();
            row_index += 1;
            if color {
                let padded = pad_cell(&cells[0], widths[0]);
                cells[0] = format!("{}{padded}\x1b[0m", agent.state.color());
            }
            out.push_str(indent);
            out.push_str(&join_row(&cells, &widths, idle_idx));
            out.push('\n');
        }
    }
    out
}

struct AgentGroup<'a> {
    host: &'a str,
    cwd: &'a str,
    agents: Vec<&'a LiveAgent>,
}

fn grouped_agents<'a>(agents: &'a [LiveAgent]) -> Vec<AgentGroup<'a>> {
    let mut groups: Vec<AgentGroup<'a>> = Vec::new();
    for agent in agents {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.host == agent.host && group.cwd == agent.cwd)
        {
            group.agents.push(agent);
            continue;
        }
        groups.push(AgentGroup {
            host: &agent.host,
            cwd: &agent.cwd,
            agents: vec![agent],
        });
    }
    let multi_host = groups
        .iter()
        .map(|group| group.host)
        .collect::<HashSet<_>>()
        .len()
        > 1;
    for group in &mut groups {
        group.agents.sort_by(|left, right| {
            left.state
                .cmp(&right.state)
                .then_with(|| left.agent.cmp(&right.agent))
                .then_with(|| left.pane.cmp(&right.pane))
        });
    }
    groups.sort_by(|left, right| {
        let left_state = left
            .agents
            .iter()
            .map(|agent| agent.state)
            .min()
            .unwrap_or(AgentState::Unknown);
        let right_state = right
            .agents
            .iter()
            .map(|agent| agent.state)
            .min()
            .unwrap_or(AgentState::Unknown);
        if multi_host {
            host_rank(left.host)
                .cmp(&host_rank(right.host))
                .then(left.host.cmp(right.host))
                .then(left_state.cmp(&right_state))
                .then(left.cwd.cmp(right.cwd))
        } else {
            left_state.cmp(&right_state).then(left.cwd.cmp(right.cwd))
        }
    });
    groups
}

fn host_rank(host: &str) -> u8 {
    if host == "local" {
        0
    } else {
        1
    }
}

fn group_header(group: &AgentGroup<'_>, multi_host: bool, color: bool) -> String {
    let cwd = short_cwd(group.cwd);
    let first = group.agents[0];
    let title = if first.branch.is_empty() {
        cwd.clone()
    } else {
        format!("{cwd} · {}", first.branch)
    };
    let diff = first.diff_summary.as_str();
    let indent = if multi_host { "  " } else { "" };
    if !color {
        return if diff.is_empty() {
            format!("{indent}{title}")
        } else {
            format!("{indent}{title}  {diff}")
        };
    }
    let title = format!("\x1b[1m{title}\x1b[0m");
    if diff.is_empty() {
        return format!("{indent}{title}");
    }
    let diff = if diff == "-" {
        format!("\x1b[90m{diff}\x1b[0m")
    } else {
        format!("\x1b[33m{diff}\x1b[0m")
    };
    format!("{indent}{title}  {diff}")
}

fn pane_cells(agent: &LiveAgent) -> Vec<String> {
    vec![
        agent.state.as_str().to_owned(),
        agent.agent.clone(),
        agent.pane.clone(),
        if agent.session.is_empty() || numeric_session(&agent.session) {
            "-".to_owned()
        } else {
            ellipsize(&agent.session, SESSION_CHARS)
        },
        if agent.idle_secs == 0 {
            "-".to_owned()
        } else {
            format_idle(agent.idle_secs)
        },
        if agent.snippet.is_empty() {
            "-".to_owned()
        } else {
            agent.snippet.clone()
        },
    ]
}

fn numeric_session(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|character| character.is_ascii_digit())
}

fn join_row(cells: &[String], widths: &[usize], right_align_idx: usize) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| match widths.get(index).copied() {
            None | Some(0) => cell.clone(),
            Some(width) if index == right_align_idx => pad_cell_left(cell, width),
            Some(width) => pad_cell(cell, width),
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn pad_cell(text: &str, width: usize) -> String {
    let len = display_width(text);
    if len >= width {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

fn pad_cell_left(text: &str, width: usize) -> String {
    let len = display_width(text);
    if len >= width {
        text.to_owned()
    } else {
        format!("{}{text}", " ".repeat(width - len))
    }
}

fn display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

fn char_display_width(ch: char) -> usize {
    if ch < ' ' || ch == '\u{7f}' {
        0
    } else if is_wide(ch) {
        2
    } else {
        1
    }
}

fn is_wide(ch: char) -> bool {
    matches!(
        ch,
        '\u{1100}'..='\u{115F}'
            | '\u{2329}'
            | '\u{232A}'
            | '\u{2E80}'..='\u{A4CF}'
            | '\u{AC00}'..='\u{D7A3}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FE10}'..='\u{FE19}'
            | '\u{FE30}'..='\u{FE6F}'
            | '\u{FF00}'..='\u{FF60}'
            | '\u{FFE0}'..='\u{FFE6}'
            | '\u{1F300}'..='\u{1FAFF}'
    )
}

fn ellipsize_width(text: &str, max_cols: usize) -> String {
    if display_width(text) <= max_cols {
        return text.to_owned();
    }
    let keep = max_cols.saturating_sub(1);
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let next = char_display_width(ch);
        if width + next > keep {
            break;
        }
        out.push(ch);
        width += next;
    }
    out.push('…');
    out
}

fn terminal_columns() -> usize {
    if let Ok(value) = env::var("COLUMNS") {
        if let Ok(width) = value.parse::<usize>() {
            if width >= 40 {
                return width;
            }
        }
    }
    #[cfg(unix)]
    {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCGWINSZ writes a winsize; STDOUT_FILENO is a live fd.
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0 {
            let width = usize::from(size.ws_col);
            if width >= 40 {
                return width;
            }
        }
    }
    100
}

fn print_diffs(agents: &[LiveAgent]) {
    let mut seen = HashSet::new();
    for agent in agents {
        if !seen.insert((agent.host.as_str(), agent.cwd.as_str())) {
            continue;
        }
        let cwd = if agent.cwd.is_empty() {
            "-"
        } else {
            agent.cwd.as_str()
        };
        match agent
            .diff
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            Some(diff) => {
                println!();
                println!("== {} {cwd} {} ==", agent.host, agent.diff_summary);
                println!("{diff}");
            }
            None if agent.diff_summary != "-" && !agent.diff_summary.is_empty() => {
                println!();
                println!("== {} {cwd} {} ==", agent.host, agent.diff_summary);
                println!("(no textual diff)");
            }
            _ => {}
        }
    }
}

fn short_cwd(path: &str) -> String {
    if path.is_empty() {
        return "-".to_owned();
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let shortened = match parts.as_slice() {
        [] => path.to_owned(),
        [one] => (*one).to_owned(),
        [.., parent, name] if parent.eq_ignore_ascii_case("projects") => (*name).to_owned(),
        [.., parent, name] => format!("{parent}/{name}"),
    };
    if shortened.chars().count() <= CWD_CHARS {
        return shortened;
    }
    let kept: String = shortened
        .chars()
        .rev()
        .take(CWD_CHARS.saturating_sub(1))
        .collect();
    format!("…{}", kept.chars().rev().collect::<String>())
}

fn use_color() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("AL_LIVE_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(runtime);
        if path.is_dir() {
            return path.join("al-live");
        }
    }
    env::temp_dir().join(format!("al-live-{}", std::process::id()))
}

fn memory_path() -> PathBuf {
    state_dir().join("panes.json")
}

fn load_memory() -> HashMap<String, PaneMemory> {
    let Ok(text) = fs::read_to_string(memory_path()) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<HashMap<String, [u64; 2]>>(&text) else {
        return HashMap::new();
    };
    value
        .into_iter()
        .map(|(pane, [hash, changed_at])| (pane, PaneMemory { hash, changed_at }))
        .collect()
}

fn save_memory(memory: &HashMap<String, PaneMemory>) {
    let path = memory_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let value: HashMap<&str, [u64; 2]> = memory
        .iter()
        .map(|(pane, state)| (pane.as_str(), [state.hash, state.changed_at]))
        .collect();
    if let Ok(text) = serde_json::to_string(&value) {
        let _ = fs::write(path, text);
    }
}

fn shlex_split(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        if let Some(active) = quote {
            if ch == '\\' && active == '"' {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            if ch == active {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

pub fn default_interval() -> u64 {
    DEFAULT_INTERVAL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, command: &str, argv: &[&str]) -> Process {
        Process {
            pid,
            ppid: 1,
            pgid: pid,
            tpgid: pid,
            command: command.to_owned(),
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    #[test]
    fn identifies_direct_and_wrapped_agents() {
        assert_eq!(
            identify_agent(&[process(8, "omlo", &["omlo"])], Some(8)).as_deref(),
            Some("omp")
        );
        assert_eq!(
            identify_agent(
                &[process(
                    9,
                    "node",
                    &["node", "/opt/pi-coding-agent/dist/cli.js"]
                )],
                Some(9)
            )
            .as_deref(),
            Some("pi")
        );
        assert_eq!(
            identify_agent(
                &[process(10, "bash", &["bash", "-c", "exec grok"])],
                Some(10)
            )
            .as_deref(),
            Some("grok")
        );
        assert_eq!(
            identify_agent(
                &[process(11, "node", &["node", "-e", "console.log(1)"])],
                Some(11)
            ),
            None
        );
    }

    #[test]
    fn tmux_names_recover_al_launchers() {
        assert_eq!(agent_from_tmux_name("omlo-sample-app"), Some("omp"));
        assert_eq!(agent_from_tmux_name("pilo-sample-app"), Some("pi"));
        assert_eq!(agent_from_tmux_name("notes"), None);
    }

    #[test]
    fn screen_rules_beat_hash_idle() {
        assert_eq!(
            classify_screen("Working...\nreading src/lib.rs", "", false),
            AgentState::Working
        );
        assert_eq!(
            classify_screen("build failed\nFAILED cargo test", "", false),
            AgentState::Blocked
        );
        assert_eq!(
            classify_screen("Which target should I use?", "", false),
            AgentState::Asking
        );
        assert_eq!(
            classify_screen("1. keep going\n❯ accept", "", false),
            AgentState::Asking
        );
        assert_eq!(classify_screen("›", "", false), AgentState::Idle);
        assert_eq!(classify_screen("›", "", true), AgentState::Working);
        assert_eq!(
            classify_screen("I'll start thinking about the parser.\n", "", false),
            AgentState::Working
        );
        let mut stale = String::new();
        for _ in 0..20 {
            stale.push_str("I'll start thinking about the parser.\n");
            stale.push_str("test result: FAILED\n");
            stale.push_str("[1] example link\n");
        }
        stale.push_str("›\n");
        assert_eq!(classify_screen(&stale, "", false), AgentState::Idle);
        assert_eq!(
            classify_screen("I confirmed the refactor.", "", false),
            AgentState::Idle
        );
        assert_eq!(
            classify_screen("please confirm the path", "", false),
            AgentState::Asking
        );
    }

    #[test]
    fn parse_ps_rows() {
        let processes = parse_processes(
            "    10     1    10    10 omp omp --yolo\n    11    10    10    10 fish fish\n",
        );
        assert_eq!(processes[0].pid, 10);
        assert_eq!(processes[0].command, "omp");
        assert_eq!(processes[0].argv, ["omp", "--yolo"]);
    }

    #[test]
    fn parse_pane_line_keeps_cwd() {
        let pane =
            parse_pane_line("%12\tagents\tomlo-demo\t0\t1\t4242\t/workspace/demo\tomlo").unwrap();
        assert_eq!(pane.pane_id, "%12");
        assert_eq!(pane.cwd, "/workspace/demo");
        assert_eq!(pane.activity, 0);
        assert_eq!(pane.title, "omlo");
    }

    #[test]
    fn parse_snapshot_reads_panes_cwds_and_captures() {
        let snapshot = parse_snapshot(
            "AL_LIVE_1 tok\n\
             PANES\n\
             %3\ts\tw\t0\t0\t9\t/workspace/demo\ttitle\n\
             PS\n\
                 9     1     9     9 omp omp --yolo\n\
             CWDS\n\
             9\t/workspace/agent-cwd\n\
             CAPTURE tok %3\n\
             ENDCAPTURE %3\n\
             Working...\n\
             ENDCAPTURE tok %3\n",
        )
        .unwrap();
        assert_eq!(snapshot.panes[0].cwd, "/workspace/demo");
        assert_eq!(
            snapshot.proc_cwds.get(&9).map(String::as_str),
            Some("/workspace/agent-cwd")
        );
        assert_eq!(
            snapshot.captures.get("%3").map(String::as_str),
            Some("ENDCAPTURE %3\nWorking...")
        );
        assert_eq!(snapshot.processes[0].command, "omp");
    }

    #[test]
    fn agent_cwd_prefers_identified_process() {
        let pane = parse_pane_line("%3\ts\tw\t0\t0\t8\t/workspace/pane\ttitle").unwrap();
        let job = [process(8, "fish", &["fish"]), process(9, "omp", &["omp"])];
        let mut cwds = HashMap::new();
        cwds.insert(8, "/workspace/pane".to_owned());
        cwds.insert(9, "/workspace/agent-cwd".to_owned());
        assert_eq!(agent_cwd(&pane, &job, &cwds), "/workspace/agent-cwd");
    }

    #[test]
    fn keep_action_nudges_idle_and_skips_working_asking_picker() {
        assert_eq!(
            keep_action(AgentState::Idle, "→ Add a follow-up"),
            KeepAction::Nudge
        );
        assert_eq!(
            keep_action(AgentState::Blocked, "FAILED no device"),
            KeepAction::Nudge
        );
        assert_eq!(
            keep_action(AgentState::Working, "ctrl+c to stop"),
            KeepAction::SkipWorking
        );
        assert_eq!(
            keep_action(AgentState::Asking, "Which file should I edit?"),
            KeepAction::SkipAsking
        );
        assert_eq!(
            keep_action(AgentState::Asking, "❯ accept\n❯ reject"),
            KeepAction::SkipPicker
        );
        assert!(composer_has_unknown_paste("→ [Pasted text #236 +7 lines]"));
        assert!(!composer_has_unknown_paste("→ Add a follow-up"));
        assert!(keep_composer_dirty(
            "→ [Pasted text #236 +7 lines]",
            "Continue from GOAL.md"
        ));
        assert!(keep_composer_dirty(
            "→ Add a follow-up\nContinue from GOAL.md",
            "Continue from GOAL.md"
        ));
        assert!(!keep_composer_dirty(
            "→ Add a follow-up",
            "Continue from GOAL.md"
        ));
        assert!(keep_still_unsent(
            "→ Add a follow-up\nContinue from GOAL.md",
            "Continue from GOAL.md"
        ));
        assert!(keep_submit_landed(
            "Working...\nesc to interrupt",
            "Continue from GOAL.md"
        ));
        assert!(keep_submit_landed(
            "Which file should I edit?",
            "Continue from GOAL.md"
        ));
        assert!(!keep_submit_landed(
            "→ Add a follow-up\nContinue from GOAL.md",
            "Continue from GOAL.md"
        ));
    }

    #[test]
    fn attention_agent_prefers_blocked_then_asking() {
        let blocked = live("blocked", AgentState::Blocked, "/workspace/a");
        let asking = live("asking", AgentState::Asking, "/workspace/b");
        let idle = live("idle", AgentState::Idle, "/workspace/c");
        assert_eq!(
            attention_agent(&[idle.clone(), asking.clone(), blocked.clone()])
                .unwrap()
                .agent,
            "blocked"
        );
        assert_eq!(
            attention_agent(&[idle.clone(), asking.clone()])
                .unwrap()
                .agent,
            "asking"
        );
        assert!(attention_agent(&[idle]).is_err());
    }

    #[test]
    fn short_cwd_keeps_last_two_components() {
        assert_eq!(short_cwd(""), "-");
        assert_eq!(short_cwd("/workspace/demo"), "workspace/demo");
        assert_eq!(short_cwd("/workspace/projects/sample-app"), "sample-app");
        assert_eq!(
            short_cwd("/home/user/Projects/agent-loader"),
            "agent-loader"
        );
        let truncated = short_cwd("/a/very-long-parent-directory-name/project");
        assert!(truncated.starts_with('…'));
        assert!(truncated.ends_with("project"));
        assert!(truncated.chars().count() <= CWD_CHARS);
    }

    #[test]
    fn snippet_skips_tui_chrome() {
        assert_eq!(
            snippet(
                "I'll edit src/lib.rs\n\
                 Running  1.2k tokens\n\
                 Goal active (1h 2m)\n\
                 ────────────────────────────────\n\
                 Add a follow-up                          ctrl+c to stop\n\
                 Cursor Grok 4.6 High Fast · 10% · 2 files edited\n\
                   ~/Projects/sample-app · main\n"
            ),
            "Running 1.2k tokens"
        );
        assert_eq!(
            snippet(
                "Session compacted 2 times\n\
                 Next I will update the parser\n\
                 > gpt-5 > demo > v1 *3 > $12.00 10%\n\
                 ╰─ you                    \n"
            ),
            "Next I will update the parser"
        );
        assert_eq!(
            snippet(
                "Which file should I edit?\n\
                 ⠹ 10m > ◕ GPT-5.6-Sol > Goal 0 > 📁 demo > mast...\n"
            ),
            "Which file should I edit?"
        );
        assert_eq!(snippet("↑48M ↓7.4M R858M CH99.8% 65.9%/131k (auto)\n"), "");
        assert_eq!(snippet("(3.11.5) user@host ~/P/demo (master)>\n"), "");
        assert_eq!(snippet("~/Projects/demo (v2)\n"), "");
        assert_eq!(
            snippet("I'll edit src/lib.rs\n1 task\n"),
            "I'll edit src/lib.rs"
        );
        assert_eq!(
            snippet("ready\ngrok --resume 00000000-0000-0000-0000-000000000000\n"),
            "ready"
        );
        assert_eq!(snippet("done\nResume this session with:\n"), "done");
    }

    #[test]
    fn format_idle_uses_compact_units() {
        assert_eq!(format_idle(12), "12s");
        assert_eq!(format_idle(3043), "50m");
        assert_eq!(format_idle(7200), "2h");
        assert_eq!(format_idle(90000), "1d");
    }

    #[test]
    fn idle_since_uses_tmux_activity_not_first_sighting() {
        assert_eq!(idle_since(1_000, 875), 125);
        assert_eq!(idle_since(1_000, 0), 0);
        assert_eq!(idle_since(1_000, 1_500), 0);
    }

    #[test]
    fn parse_pane_line_reads_activity_before_title() {
        let pane =
            parse_pane_line("%12\tagents\tomlo-demo\t0\t1\t4242\t/workspace/demo\t875\tomlo")
                .unwrap();
        assert_eq!(pane.activity, 875);
        assert_eq!(pane.title, "omlo");
        assert_eq!(
            parse_pane_line("%12\tagents\tomlo-demo\t0\t1\t4242\t/workspace/demo\tomlo")
                .unwrap()
                .activity,
            0
        );
    }

    #[test]
    fn clean_worktree_summary_is_blank() {
        let git = GitInfo {
            inside: true,
            branch: "main".to_owned(),
            ..GitInfo::default()
        };
        assert_eq!(git.summary(), "");
        let mut agent = live("omp", AgentState::Idle, "/workspace/demo");
        agent.branch = "main".into();
        agent.diff_summary = git.summary();
        let header = group_header(
            &AgentGroup {
                host: "local",
                cwd: "/workspace/demo",
                agents: vec![&agent],
            },
            false,
            false,
        );
        assert_eq!(header, "workspace/demo · main");
        assert!(!header.contains("clean"));
    }

    #[test]
    fn live_picker_line_round_trips_host_pane_and_attach_target() {
        let mut agent = live("omp", AgentState::Asking, "/workspace/Projects/demo");
        agent.host = "host-a".into();
        agent.session = "omlo-demo".into();
        agent.pane = "%12".into();
        agent.target = "omlo-demo:0.1".into();
        agent.branch = "master".into();
        agent.diff_summary = "clean".into();
        agent.snippet = "Which file?\there".into();
        let line = format_live_picker_line(&agent, false);
        assert!(!line.split('\t').next().unwrap().contains('\t'));
        let key = parse_live_picker_line(&line).unwrap();
        assert_eq!(key.host, "host-a");
        assert_eq!(key.pane, "%12");
        assert_eq!(key.session, "omlo-demo");
        assert_eq!(key.target, "=omlo-demo:0.1");
        assert_eq!(attach_target(&agent), "=omlo-demo:0.1");
        assert_eq!(
            crate::remote::argv_with(
                crate::remote::RemoteTool::Mosh,
                "host-a",
                &["tmux", "attach-session", "-t", "=omlo-demo:0.1"],
                true
            ),
            [
                "mosh",
                "--",
                "host-a",
                "tmux",
                "attach-session",
                "-t",
                "=omlo-demo:0.1"
            ]
        );
        assert_eq!(
            crate::remote::argv_with(
                crate::remote::RemoteTool::Ssh,
                "host-a",
                &["tmux", "attach-session", "-t", "=omlo-demo:0.1"],
                true
            ),
            [
                "ssh",
                "-tt",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ConnectionAttempts=1",
                "--",
                "host-a",
                "tmux",
                "attach-session",
                "-t",
                "=omlo-demo:0.1"
            ]
        );
        agent.host = "local".into();
        assert_eq!(
            attach_argv(&agent, true),
            ["tmux", "switch-client", "-t", "=omlo-demo:0.1"]
        );
        assert_eq!(
            attach_argv(&agent, false),
            ["tmux", "attach-session", "-t", "=omlo-demo:0.1"]
        );
    }

    #[test]
    fn format_table_groups_panes_under_worktree() {
        let mut agent = live("agent", AgentState::Idle, "/workspace/Projects/demo");
        agent.session = "28".into();
        agent.pane = "%8".into();
        agent.branch = "main".into();
        agent.diff_summary = "+12/-3 ?1".into();
        agent.idle_secs = 3043;
        agent.snippet = "Running 1.2k tokens".into();
        let mut other = live("omp", AgentState::Asking, "/workspace/Projects/demo");
        other.session = "omlo-demo".into();
        other.pane = "%9".into();
        other.branch = "main".into();
        other.diff_summary = "+12/-3 ?1".into();
        other.snippet = "Which file?".into();
        let table = format_table(&[agent, other], false);
        assert!(table.contains("demo · main  +12/-3 ?1"), "{table}");
        assert_eq!(table.matches("+12/-3 ?1").count(), 1);
        assert!(table.contains("%8"), "{table}");
        assert!(table.contains("50m"), "{table}");
        assert!(table.contains("Which file?"), "{table}");
        assert!(table.contains("omlo-demo"), "{table}");
        assert!(
            table.lines().any(|line| line.contains("asking")
                && line.contains("omp")
                && line.contains("%9")
                && line.contains("omlo-demo")
                && line.contains("-")
                && line.contains("Which file?")),
            "fixed columns should keep session and idle placeholders: {table}"
        );
        assert!(
            table.lines().any(|line| line.contains("idle")
                && line.contains("%8")
                && line.contains("50m")
                && line.contains("-")),
            "numeric sessions render as -: {table}"
        );
        let colored = format_table(
            &[live("agent", AgentState::Idle, "/workspace/Projects/demo")],
            true,
        );
        assert!(
            colored.contains("\x1b[92midle"),
            "idle should be green: {colored:?}"
        );
    }

    #[test]
    fn format_table_groups_multi_host_blocks() {
        let mut local = live("omp", AgentState::Working, "/workspace/Projects/demo");
        local.host = "local".into();
        local.session = "omlo-demo".into();
        local.pane = "%2".into();
        local.branch = "main".into();
        local.diff_summary = String::new();
        local.snippet = "editing".into();
        let mut remote = live("claude", AgentState::Blocked, "/workspace/Projects/other");
        remote.host = "host-b".into();
        remote.session = "cclo-other".into();
        remote.pane = "%7".into();
        remote.branch = "feat".into();
        remote.diff_summary = "+1/-0".into();
        remote.snippet = "Need approval".into();
        let table = format_table(&[local, remote], false);
        assert!(table.contains("local\n"), "{table}");
        assert!(table.contains("host-b\n"), "{table}");
        assert!(table.contains("demo · main"), "{table}");
        assert!(table.contains("other · feat  +1/-0"), "{table}");
        let local_pos = table.find("local\n").unwrap();
        let remote_pos = table.find("host-b\n").unwrap();
        assert!(local_pos < remote_pos, "{table}");
    }

    #[test]
    fn watch_frame_includes_label_counts_and_failed_hosts() {
        let agent = live("omp", AgentState::Asking, "/workspace/demo");
        let frame = render_watch_frame(
            "watch",
            std::slice::from_ref(&agent),
            3,
            &["host-b".to_owned()],
            false,
        );
        assert!(frame.starts_with("al watch  1 agents"), "{frame}");
        assert!(frame.contains("1 asking"), "{frame}");
        assert!(frame.contains("failed: host-b"), "{frame}");
        assert!(frame.contains("asking"), "{frame}");
        let empty = render_watch_frame("supervise", &[], 5, &[], false);
        assert!(empty.contains("al supervise  0 agents"), "{empty}");
        assert!(empty.contains("no live agents"), "{empty}");
    }

    #[test]
    fn attention_agent_reports_empty_and_idle_hosts_clearly() {
        assert!(attention_agent(&[])
            .unwrap_err()
            .to_string()
            .contains("no live agents"));
        let idle = live("omp", AgentState::Idle, "/workspace/demo");
        let working = live("pi", AgentState::Working, "/workspace/other");
        let err = attention_agent(&[idle, working]).unwrap_err().to_string();
        assert!(err.contains("no blocked or asking"), "{err}");
        assert!(err.contains("omp:idle"), "{err}");
        assert!(err.contains("pi:working"), "{err}");
    }

    #[test]
    fn pane_cells_always_use_fixed_placeholders() {
        let mut agent = live("omp", AgentState::Asking, "/workspace/demo");
        agent.session = "12".into();
        agent.idle_secs = 0;
        agent.snippet = String::new();
        assert_eq!(
            pane_cells(&agent),
            [
                "asking".to_owned(),
                "omp".to_owned(),
                "%1".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
                "-".to_owned(),
            ]
        );
        agent.session = "omlo-demo".into();
        agent.idle_secs = 90;
        agent.snippet = "Which file?".into();
        assert_eq!(
            pane_cells(&agent),
            [
                "asking".to_owned(),
                "omp".to_owned(),
                "%1".to_owned(),
                "omlo-demo".to_owned(),
                "1m".to_owned(),
                "Which file?".to_owned(),
            ]
        );
    }

    #[test]
    fn idle_state_uses_a_visible_green() {
        assert_eq!(AgentState::Idle.color(), "\x1b[92m");
        assert_ne!(AgentState::Idle.color(), AgentState::Working.color());
        assert_ne!(AgentState::Idle.color(), AgentState::Asking.color());
        assert_ne!(AgentState::Idle.color(), AgentState::Blocked.color());
    }

    fn live(agent: &str, state: AgentState, cwd: &str) -> LiveAgent {
        LiveAgent {
            host: "local".to_owned(),
            session: "s".to_owned(),
            window: "w".to_owned(),
            pane: "%1".to_owned(),
            target: "s:0.0".to_owned(),
            cwd: cwd.to_owned(),
            agent: agent.to_owned(),
            branch: String::new(),
            diff_summary: "-".to_owned(),
            diff: None,
            state,
            idle_secs: 0,
            snippet: String::new(),
        }
    }

    #[test]
    fn parse_git_body_reads_status_stat_and_diff() {
        let git = parse_git_body(
            "## main...origin/main\n\
             M src/lib.rs\n\
             ?? scratch.rs\n\
             --STAT--\n\
              1 file changed, 12 insertions(+), 3 deletions(-)\n\
             --UNTRACKED--\n\
             scratch.rs\n\
             --DIFF--\n\
             diff --git a/src/lib.rs b/src/lib.rs\n\
             --STAT--\n\
             --DIFF--\n\
             --UNTRACKED--\n\
             still the patch\n",
        );
        assert!(git.inside);
        assert_eq!(git.branch, "main");
        assert_eq!(git.insertions, 12);
        assert_eq!(git.deletions, 3);
        assert_eq!(git.untracked, 1);
        assert_eq!(git.summary(), "+12/-3 ?1");
        assert!(git.diff.as_deref().is_some_and(|diff| {
            diff.contains("diff --git")
                && diff.contains("still the patch")
                && diff.contains("--STAT--")
                && diff.contains("Untracked:")
        }));
    }

    #[test]
    fn parse_git_body_empty_is_not_a_repo() {
        let git = parse_git_body("");
        assert!(!git.inside);
        assert_eq!(git.summary(), "-");
    }

    #[test]
    fn parse_snapshot_reads_git_blocks() {
        let snapshot = parse_snapshot(
            "AL_LIVE_1 tok\n\
             PANES\n\
             %3\ts\tw\t0\t0\t9\t/workspace/demo\ttitle\n\
             GITS\n\
             GIT tok /workspace/demo\n\
             ## feat\n\
              M README.md\n\
             --STAT--\n\
              1 file changed, 4 insertions(+)\n\
             ENDGIT /workspace/demo\n\
             ENDGIT tok /workspace/demo\n",
        )
        .unwrap();
        let git = snapshot.gits.get("/workspace/demo").unwrap();
        assert_eq!(git.branch, "feat");
        assert_eq!(git.insertions, 4);
        assert_eq!(git.summary(), "+4/-0");
    }

    #[test]
    fn inspect_git_reads_dirty_worktree() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(["-c", "user.name=al", "-c", "user.email=al@example.test"])
                .args(args)
                .current_dir(cwd)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q"]));
        std::fs::write(cwd.join("README.md"), "one\n").unwrap();
        assert!(git(&["add", "README.md"]));
        assert!(git(&["commit", "-qm", "init"]));
        std::fs::write(cwd.join("README.md"), "two\n").unwrap();
        std::fs::write(cwd.join("scratch.rs"), "fn main() {}\n").unwrap();
        let git = inspect_git(cwd.to_str().unwrap(), true);
        assert!(
            git.inside && (git.insertions > 0 || git.files > 0),
            "{git:?}"
        );
        assert_eq!(git.untracked, 1);
        assert!(git
            .diff
            .as_deref()
            .is_some_and(|diff| diff.contains("README.md") && diff.contains("Untracked:")));
    }

    #[test]
    fn dump_header_and_frame_id_require_token() {
        assert_eq!(parse_dump_header("AL_LIVE_1").unwrap(), None);
        assert_eq!(
            parse_dump_header("AL_LIVE_1 tok").unwrap().as_deref(),
            Some("tok")
        );
        assert!(parse_dump_header("AL_LIVE_1 to ken").is_err());
        assert!(parse_dump_header("NOPE").is_err());
        assert_eq!(
            frame_id("ENDCAPTURE tok %3", "ENDCAPTURE ", Some("tok")),
            Some("%3")
        );
        assert_eq!(frame_id("ENDCAPTURE %3", "ENDCAPTURE ", Some("tok")), None);
        assert_eq!(
            frame_id("GIT tok /workspace/demo", "GIT ", Some("tok")),
            Some("/workspace/demo")
        );
    }

    #[test]
    fn resolve_agent_matches_pane_cwd_and_attention() {
        let blocked = live("omp", AgentState::Blocked, "/workspace/a");
        let asking = live("pi", AgentState::Asking, "/workspace/b");
        let idle = live("agent", AgentState::Idle, "/workspace/demo");
        assert_eq!(
            resolve_agent(&[idle.clone(), asking.clone(), blocked.clone()], None)
                .unwrap()
                .agent,
            "omp"
        );
        assert_eq!(
            resolve_agent(std::slice::from_ref(&idle), Some("%1"))
                .unwrap()
                .cwd,
            "/workspace/demo"
        );
        assert_eq!(
            resolve_agent(std::slice::from_ref(&idle), Some("workspace/demo"))
                .unwrap()
                .agent,
            "agent"
        );
        assert!(resolve_agent(std::slice::from_ref(&idle), Some("s")).is_ok());
        assert!(resolve_agent(&[idle.clone(), asking], Some("pi")).is_ok());
        assert!(resolve_agent(&[blocked.clone(), blocked], Some("omp")).is_err());
        assert!(resolve_agent(&[idle], Some("missing")).is_err());
    }

    #[test]
    fn attach_git_falls_back_to_pane_cwd() {
        let mut agent = live("omp", AgentState::Idle, "/workspace/agent-cwd");
        agent.pane = "%9".to_owned();
        let mut snapshot = Snapshot::default();
        snapshot
            .panes
            .push(parse_pane_line("%9\ts\tw\t0\t0\t8\t/workspace/demo\ttitle").unwrap());
        snapshot.gits.insert(
            "/workspace/demo".to_owned(),
            GitInfo {
                inside: true,
                insertions: 4,
                deletions: 1,
                ..GitInfo::default()
            },
        );
        attach_git(std::slice::from_mut(&mut agent), &snapshot, GitMode::Status);
        assert_eq!(agent.diff_summary, "+4/-1");
        assert!(agent.diff.is_none());
        attach_git(std::slice::from_mut(&mut agent), &snapshot, GitMode::Off);
        assert_eq!(agent.diff_summary, "");
        assert!(agent.branch.is_empty());
        assert!(agent.diff.is_none());
    }

    #[test]
    fn pane_activity_hash_ignores_history_above_the_window() {
        let mut tail = String::new();
        for index in 0..STRUCT_LINES {
            tail.push_str(&format!("visible {index}\n"));
        }
        tail.push_str("›\n");
        let left = pane_activity_key(&format!("old thinking\n{tail}"), "title");
        let right = pane_activity_key(&format!("different prefix\n{tail}"), "title");
        assert_eq!(content_hash(&left), content_hash(&right));
        let changed = pane_activity_key(&format!("old thinking\n{tail}Working...\n"), "title");
        assert_ne!(content_hash(&left), content_hash(&changed));
    }

    #[test]
    fn agents_from_snapshot_skips_exited_named_windows() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = MEMORY_LOCK.lock().unwrap();
        unsafe {
            env::set_var("AL_LIVE_STATE_DIR", dir.path());
        }
        let snapshot = parse_snapshot(
            "AL_LIVE_1 tok\n\
             PANES\n\
             %3\ts\tomlo-demo\t0\t0\t9\t/workspace/demo\ttitle\n\
             %4\ts\tnotes\t0\t1\t10\t/tmp\tshell\n\
             PS\n\
                 9     1     9     9 fish fish\n\
                10     1    10    10 fish fish\n\
             CAPTURE tok %3\n\
             previous discussion\n\
             ›\n\
             ENDCAPTURE tok %3\n",
        )
        .unwrap();
        let agents = agents_from_snapshot("local", &snapshot);
        unsafe {
            env::remove_var("AL_LIVE_STATE_DIR");
        }
        assert!(agents.is_empty(), "{agents:?}");
    }

    #[test]
    fn agents_from_snapshot_keeps_live_processes() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = MEMORY_LOCK.lock().unwrap();
        unsafe {
            env::set_var("AL_LIVE_STATE_DIR", dir.path());
        }
        let snapshot = parse_snapshot(
            "AL_LIVE_1 tok\n\
             PANES\n\
             %3\ts\tomlo-demo\t0\t0\t9\t/workspace/demo\ttitle\n\
             %4\ts\tnotes\t0\t1\t10\t/tmp\tshell\n\
             PS\n\
                 9     1     9     9 omp omp --yolo\n\
                10     1    10    10 fish fish\n\
             CAPTURE tok %3\n\
             previous discussion\n\
             ›\n\
             ENDCAPTURE tok %3\n",
        )
        .unwrap();
        let agents = agents_from_snapshot("local", &snapshot);
        unsafe {
            env::remove_var("AL_LIVE_STATE_DIR");
        }
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent, "omp");
        assert_eq!(agents[0].cwd, "/workspace/demo");
        assert_eq!(agents[0].state, AgentState::Idle);
    }

    #[test]
    fn attach_all_plan_groups_sessions_by_host() {
        let mut local_a = live("omp", AgentState::Asking, "/workspace/a");
        local_a.session = "omlo-a".into();
        let mut local_b = live("pi", AgentState::Working, "/workspace/b");
        local_b.session = "pilo-b".into();
        let mut remote = live("claude", AgentState::Idle, "/workspace/c");
        remote.host = "host-b".into();
        remote.session = "cclo-c".into();
        let mut remote_dup = remote.clone();
        remote_dup.pane = "%9".into();
        let plan = attach_all_plan(&[local_b, remote, local_a, remote_dup]);
        assert_eq!(
            plan,
            [
                (
                    "local".to_owned(),
                    vec!["omlo-a".to_owned(), "pilo-b".to_owned()]
                ),
                ("host-b".to_owned(), vec!["cclo-c".to_owned()])
            ]
        );
        assert_eq!(
            session_attach_command("local", "omlo-a"),
            [
                "env",
                "-u",
                "TMUX",
                "tmux",
                "attach-session",
                "-t",
                "=omlo-a"
            ]
        );
        let remote = session_attach_command("host-b", "cclo-c");
        assert!(remote[0] == "mosh" || remote[0] == "ssh", "{remote:?}");
        assert!(remote.contains(&"host-b".to_owned()), "{remote:?}");
        assert!(remote.contains(&"=cclo-c".to_owned()), "{remote:?}");
    }

    #[cfg(unix)]
    #[test]
    fn attach_all_creates_one_window_per_host_session() {
        if Command::new("tmux").arg("-V").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tmux_dir = dir.path().join("tmux");
        fs::create_dir_all(&tmux_dir).unwrap();
        let _lock = super::TMUX_TEST_LOCK.lock().unwrap();
        struct ClearTmuxTmp;
        impl Drop for ClearTmuxTmp {
            fn drop(&mut self) {
                unsafe {
                    env::remove_var("TMUX_TMPDIR");
                }
            }
        }
        let _clear = ClearTmuxTmp;
        unsafe {
            env::set_var("TMUX_TMPDIR", &tmux_dir);
            env::remove_var("TMUX");
        }
        let tmux = |args: &[&str]| {
            Command::new("tmux")
                .env("TMUX_TMPDIR", &tmux_dir)
                .env_remove("TMUX")
                .args(args)
                .output()
                .unwrap()
        };
        let _ = tmux(&["kill-server"]);
        assert!(tmux(&["new-session", "-d", "-s", "omlo-a", "-n", "w"])
            .status
            .success());
        assert!(tmux(&["new-session", "-d", "-s", "pilo-b", "-n", "w"])
            .status
            .success());
        create_host_aggregator("local", &["omlo-a".into(), "pilo-b".into()]).unwrap();
        let listed = tmux(&["list-windows", "-t", "=local", "-F", "#W"]);
        let names = String::from_utf8_lossy(&listed.stdout);
        assert!(
            listed.status.success(),
            "{names} {}",
            String::from_utf8_lossy(&listed.stderr)
        );
        assert!(names.contains("omlo-a"), "{names}");
        assert!(names.contains("pilo-b"), "{names}");
        let _ = tmux(&["kill-server"]);
    }

    static MEMORY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
