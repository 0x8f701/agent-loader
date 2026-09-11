//! Create or update a git project on this machine or a remote host, then
//! optionally open it in tmux (and optionally launch a coding agent).

use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::launcher::{CommandSpec, LauncherKind};

const DEFAULT_PARENT_DIR: &str = "Projects";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Orchestrator,
    Executor,
    Reviewer,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orchestrator => "orchestrator",
            Self::Executor => "executor",
            Self::Reviewer => "reviewer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    pub host: Option<String>,
    pub path: PathBuf,
    pub goal: String,
    pub executor: Option<LauncherKind>,
    pub orchestrator: Option<LauncherKind>,
    pub reviewer: Option<LauncherKind>,
    pub worktree: Option<String>,
    pub gits: Vec<String>,
    pub tmux: bool,
    pub print_command: bool,
}

pub fn parse_launcher_tool(value: &str) -> Result<LauncherKind, String> {
    match value {
        "omlo" | "omp" => Ok(LauncherKind::Omp),
        "pilo" | "pi" => Ok(LauncherKind::Pi),
        "rpilo" | "rpi" => Ok(LauncherKind::Rpi),
        "grolo" | "grok" => Ok(LauncherKind::Grok),
        "hyperlo" | "hyper" => Ok(LauncherKind::Hyper),
        "dolo" | "droid" => Ok(LauncherKind::Droid),
        "colo" | "codex" => Ok(LauncherKind::Codex),
        "cclo" | "claude" => Ok(LauncherKind::Claude),
        "agentlo" | "agent" => Ok(LauncherKind::Agent),
        _ => Err(format!(
            "unsupported tool {value:?}; expected omlo, pilo, rpilo, grolo, hyperlo, dolo, colo, cclo, or agentlo"
        )),
    }
}

pub fn parse_git_url(value: &str) -> Result<String, String> {
    validate_git_url(value).map_err(|error| error.to_string())?;
    repo_name_from_git(value).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

pub fn validate_project_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("project name must not be empty");
    }
    if name.len() > 64 {
        bail!("project name must be at most 64 characters");
    }
    if name.starts_with('.') || name.starts_with('-') {
        bail!("project name must not start with '.' or '-'");
    }
    if name == "." || name == ".." {
        bail!("project name must not be '.' or '..'");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("project name must not contain a path separator");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    {
        bail!("project name may only contain ASCII letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

pub fn parse_new_targets(args: &[String]) -> Result<(Option<String>, PathBuf, String)> {
    match args {
        [path, goal] => {
            if goal.trim().is_empty() {
                bail!("goal must not be empty");
            }
            Ok((None, PathBuf::from(path), goal.clone()))
        }
        [host, path, goal] if looks_like_path(host) => {
            bail!("first argument looks like a path; use: al new [HOST] NAME|PATH GOAL");
        }
        [host, path, goal] => {
            if goal.trim().is_empty() {
                bail!("goal must not be empty");
            }
            if host == "local" {
                return Ok((None, PathBuf::from(path), goal.clone()));
            }
            validate_host(host)?;
            Ok((Some(host.clone()), PathBuf::from(path), goal.clone()))
        }
        _ => bail!("use: al new [HOST] NAME|PATH GOAL"),
    }
}

pub fn run(spec: NewProject) -> Result<()> {
    if spec.goal.trim().is_empty() {
        bail!("goal must not be empty");
    }
    if let Some(name) = spec.worktree.as_deref() {
        crate::launcher::validate_worktree_name(OsStr::new(name))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    validate_gits(&spec)?;
    if let Some(host) = spec.host.as_deref() {
        validate_host(host)?;
    }
    if !looks_like_path_buf(&spec.path) {
        validate_project_name(spec.path.to_str().context("project name is not UTF-8")?)?;
    }

    let role_count = spec.executor.is_some() as u8
        + spec.orchestrator.is_some() as u8
        + spec.reviewer.is_some() as u8;
    if role_count > 1 && !spec.tmux {
        bail!("multiple roles require tmux; omit --no-tmux");
    }

    if spec.print_command {
        print_plan(&spec)?;
        return Ok(());
    }

    #[cfg(not(unix))]
    if spec.tmux
        || spec.executor.is_some()
        || spec.orchestrator.is_some()
        || spec.reviewer.is_some()
    {
        bail!("al new --tmux / --executor is unsupported on this platform");
    }

    let destination = apply(&spec)?;
    println!("{}", destination.display());

    if role_launches(&spec, &destination).is_empty() && !spec.tmux {
        return Ok(());
    }
    open_destination(&spec, &destination)
}

fn apply(spec: &NewProject) -> Result<PathBuf> {
    if let Some(host) = spec.host.as_deref() {
        return apply_remote(spec, host);
    }
    apply_local(spec)
}

fn apply_local(spec: &NewProject) -> Result<PathBuf> {
    if !spec.gits.is_empty() {
        return apply_local_clones(spec);
    }
    let repo = resolve_local_repo(&spec.path)?;
    if !repo.exists() {
        create_local_repo(&repo, &spec.goal)?;
    } else if !repo.is_dir() {
        bail!(
            "project path exists and is not a directory: {}",
            repo.display()
        );
    }
    let destination = match spec.worktree.as_deref() {
        Some(name) => {
            let dest = local_worktree_destination(&repo, name)?;
            ensure_local_worktree(&repo, &dest, name)?;
            dest
        }
        None => repo,
    };
    write_goal_file(&destination, &spec.goal)?;
    write_role_files(spec, &destination)?;
    commit_goal(&destination)?;
    Ok(destination)
}

fn apply_remote(spec: &NewProject, host: &str) -> Result<PathBuf> {
    let destination = project_destination(spec, true)?;
    let script = remote_script(spec)?;
    let status =
        crate::remote::status(host, &[&format!("fish -c {}", posix_quote(&script))], false)
            .context("could not reach remote host")?;
    if !status.success() {
        bail!(
            "failed to apply project on host {host:?} (exit {})",
            status.code().unwrap_or(1)
        );
    }
    Ok(destination)
}

fn apply_local_clones(spec: &NewProject) -> Result<PathBuf> {
    let destination = git_destination(spec, false)?;
    if spec.gits.len() == 1 {
        ensure_local_clone(&spec.gits[0], &destination)?;
    } else {
        fs::create_dir_all(&destination)
            .with_context(|| format!("creating {}", destination.display()))?;
        for url in &spec.gits {
            let name = repo_name_from_git(url)?;
            ensure_local_clone(url, &destination.join(name))?;
        }
    }
    write_goal_file(&destination, &spec.goal)?;
    write_role_files(spec, &destination)?;
    commit_goal(&destination)?;
    Ok(destination)
}

fn validate_gits(spec: &NewProject) -> Result<()> {
    if spec.gits.is_empty() {
        return Ok(());
    }
    if spec.gits.len() > 1 && spec.worktree.is_none() {
        bail!("multiple --git requires --worktree; clones go under ~/Projects/worktree");
    }
    let mut seen = HashSet::new();
    for url in &spec.gits {
        validate_git_url(url)?;
        let name = repo_name_from_git(url)?;
        if !seen.insert(name.clone()) {
            bail!("duplicate --git repo name {name}");
        }
    }
    Ok(())
}

fn validate_git_url(url: &str) -> Result<()> {
    if url.is_empty() {
        bail!("git URL must not be empty");
    }
    if url.len() > 2048 {
        bail!("git URL is too long");
    }
    if url.starts_with('-') {
        bail!("git URL must not start with '-'");
    }
    if url
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        bail!("git URL must not contain whitespace or control characters");
    }
    Ok(())
}

fn repo_name_from_git(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches(['/', '\\']);
    let without_git = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let name = without_git
        .rsplit(['/', ':', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("");
    if name.is_empty() {
        bail!("could not read a repository name from git URL {url:?}");
    }
    validate_project_name(name)?;
    Ok(name.to_owned())
}

fn project_destination(spec: &NewProject, remote: bool) -> Result<PathBuf> {
    if !spec.gits.is_empty() {
        return git_destination(spec, remote);
    }
    let repo = if remote {
        resolve_remote_repo(&spec.path)?
    } else {
        resolve_local_repo(&spec.path)?
    };
    match spec.worktree.as_deref() {
        Some(name) if remote => remote_worktree_destination(&spec.path, name),
        Some(name) => local_worktree_destination(&repo, name),
        None => Ok(repo),
    }
}

fn git_destination(spec: &NewProject, remote: bool) -> Result<PathBuf> {
    if spec.gits.len() == 1 && spec.worktree.is_none() {
        return if remote {
            resolve_remote_repo(&spec.path)
        } else {
            resolve_local_repo(&spec.path)
        };
    }
    let folder = git_clone_folder_name(spec.worktree.as_deref().unwrap_or("worktree"));
    crate::launcher::validate_worktree_name(OsStr::new(folder))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    if remote {
        Ok(PathBuf::from("$HOME").join(DEFAULT_PARENT_DIR).join(folder))
    } else {
        Ok(local_parent()?.join(folder))
    }
}

fn git_clone_folder_name(name: &str) -> &str {
    if name == "wt" { "worktree" } else { name }
}

fn ensure_local_clone(url: &str, dest: &Path) -> Result<()> {
    if is_git_checkout(dest) {
        return Ok(());
    }
    if dest.exists() {
        if !(dest.is_dir() && dir_is_empty(dest)?) {
            bail!(
                "destination exists and is not a git checkout: {}",
                dest.display()
            );
        }
    } else if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    clone_into(url, dest)
}

fn dir_is_empty(path: &Path) -> Result<bool> {
    Ok(fs::read_dir(path)
        .with_context(|| format!("reading {}", path.display()))?
        .next()
        .is_none())
}

fn clone_into(url: &str, dest: &Path) -> Result<()> {
    let dest_str = dest.to_str().context("clone path is not UTF-8")?;
    let output = Command::new("git")
        .args(["clone", "--", url, dest_str])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .context("could not run git")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git clone failed: {}", stderr.trim().replace('\n', " "))
}

fn create_local_repo(destination: &Path, goal: &str) -> Result<()> {
    let name = repo_file_name(destination)?;
    fs::create_dir_all(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    write_project_files(destination, &name, goal)?;
    init_git(destination)
}

fn write_project_files(destination: &Path, name: &str, goal: &str) -> Result<()> {
    fs::write(destination.join("README.md"), readme_contents(name, goal))
        .with_context(|| format!("writing {}", destination.join("README.md").display()))?;
    write_goal_file(destination, goal)
}

fn readme_contents(name: &str, goal: &str) -> String {
    format!("# {name}\n\n{goal}\n")
}

fn goal_contents(goal: &str) -> String {
    format!("# Goal\n\n{goal}\n")
}

fn write_goal_file(destination: &Path, goal: &str) -> Result<()> {
    fs::write(destination.join("GOAL.md"), goal_contents(goal))
        .with_context(|| format!("writing {}", destination.join("GOAL.md").display()))
}

fn init_git(destination: &Path) -> Result<()> {
    run_git(destination, &["init"])?;
    run_git(destination, &["add", "README.md", "GOAL.md"])?;
    commit_with_fallback(destination, "Initial commit")
}

fn commit_goal(destination: &Path) -> Result<()> {
    if !is_git_checkout(destination) {
        return Ok(());
    }
    run_git(destination, &["add", "GOAL.md"])?;
    let staged = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(destination)
        .status()
        .context("could not run git")?;
    if staged.success() {
        return Ok(());
    }
    commit_with_fallback(destination, "Set goal")
}

fn commit_with_fallback(destination: &Path, message: &str) -> Result<()> {
    if run_git(destination, &["commit", "-m", message]).is_err() {
        run_git(
            destination,
            &[
                "-c",
                "user.name=al",
                "-c",
                "user.email=al@localhost",
                "commit",
                "-m",
                message,
            ],
        )?;
    }
    Ok(())
}

fn is_git_checkout(path: &Path) -> bool {
    path.join(".git").exists()
}

fn run_git(destination: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(destination)
        .stdin(Stdio::null())
        .output()
        .context("could not run git")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "git {} failed: {}",
        args.join(" "),
        stderr.trim().replace('\n', " ")
    );
}

fn open_destination(spec: &NewProject, destination: &Path) -> Result<()> {
    let launches = role_launches(spec, destination);
    if launches.len() > 1 && !spec.tmux {
        bail!("multiple roles require tmux; omit --no-tmux");
    }
    if let Some(host) = spec.host.as_deref() {
        return open_remote(spec, host, destination, &launches);
    }
    open_local(destination, spec.tmux, &launches)
}

fn open_local(destination: &Path, use_tmux: bool, launches: &[RoleLaunch]) -> Result<()> {
    if use_tmux {
        let session = session_name(destination);
        if tmux_session_exists(&session) {
            bail!(
                "tmux session {session} already exists; pick another project name or close that session first"
            );
        }
        let windows = tmux_windows(destination, launches)?;
        let code = crate::tmux::run_named_windows(&session, &windows, false)?;
        if code != 0 {
            bail!("tmux-run exited {code}");
        }
        apply_native_goals(&session, launches)?;
        if !std::io::stdout().is_terminal() {
            return Ok(());
        }
        let code = crate::tmux::attach_session(&session)?;
        if code == 0 {
            return Ok(());
        }
        bail!("tmux attach exited {code}");
    }
    if launches.iter().any(|role| goal_recipe(role.tool).is_some()) {
        eprintln!("al: --no-tmux cannot send /goal; wrote GOAL.md only");
    }
    let launch = match launches.first() {
        Some(role) => role.argv.clone(),
        None => launch_argv(None),
    };
    let mut command = Command::new(&launch[0]);
    command.args(&launch[1..]).current_dir(destination);
    let status = command
        .status()
        .with_context(|| format!("launching {}", launch[0].to_string_lossy()))?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "{} exited {}",
            launch[0].to_string_lossy(),
            status.code().unwrap_or(1)
        )
    }
}

fn open_remote(
    spec: &NewProject,
    host: &str,
    destination: &Path,
    _launches: &[RoleLaunch],
) -> Result<()> {
    let dest = fish_path(destination)?;
    let remote_al = push_self(host)?;
    let inner = remote_open_inner(spec, &dest, &remote_al);
    let remote = format!("fish -lic {}", posix_quote(&inner));
    let status =
        crate::remote::status(host, &[&remote], true).context("could not reach remote host")?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "failed to open project on host {host:?} (exit {})",
            status.code().unwrap_or(1)
        )
    }
}

fn remote_open_inner(spec: &NewProject, dest: &str, remote_al: &str) -> String {
    let mut inner = format!(
        "cd {dest}; and {} new . {}",
        posix_quote(remote_al),
        posix_quote(&spec.goal)
    );
    if let Some(tool) = spec.orchestrator {
        inner.push_str(" --orchestrator ");
        inner.push_str(tool.as_str());
    }
    if let Some(tool) = spec.executor {
        inner.push_str(" --executor ");
        inner.push_str(tool.as_str());
    }
    if let Some(tool) = spec.reviewer {
        inner.push_str(" --reviewer ");
        inner.push_str(tool.as_str());
    }
    if spec.tmux {
        inner.push_str(" --tmux");
    } else {
        inner.push_str(" --no-tmux");
    }
    inner
}

fn push_self(host: &str) -> Result<String> {
    let remote = format!("/tmp/al-{}", env!("CARGO_PKG_VERSION"));
    if remote_self_matches(host, &remote) {
        return Ok(remote);
    }
    let src = env::current_exe().context("locating al to copy to the remote host")?;
    let dest = format!("{host}:{remote}");
    let status = Command::new("scp")
        .args([
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ConnectionAttempts=1",
            "--",
        ])
        .arg(&src)
        .arg(&dest)
        .status()
        .context("could not copy al to the remote host")?;
    if !status.success() {
        bail!(
            "failed to copy al to host {host:?} (exit {})",
            status.code().unwrap_or(1)
        );
    }
    let chmod = crate::remote::status(
        host,
        &[&format!("chmod 755 {}", posix_quote(&remote))],
        false,
    )
    .context("could not chmod remote al")?;
    if !chmod.success() {
        bail!(
            "failed to chmod al on host {host:?} (exit {})",
            chmod.code().unwrap_or(1)
        );
    }
    Ok(remote)
}

fn remote_self_matches(host: &str, remote: &str) -> bool {
    let output = crate::remote::output(
        host,
        &[&format!("{} --version", posix_quote(remote))],
        false,
    );
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION"))
        }
        _ => false,
    }
}

fn tmux_windows(
    destination: &Path,
    launches: &[RoleLaunch],
) -> Result<Vec<crate::tmux::NamedWindow>> {
    if launches.is_empty() {
        let mut spec = CommandSpec::new("fish", vec![OsString::from("-li")]);
        spec.cwd = Some(destination.to_path_buf());
        return Ok(vec![crate::tmux::NamedWindow {
            name: session_name(destination),
            spec,
        }]);
    }
    Ok(launches
        .iter()
        .map(|role| {
            let mut spec = CommandSpec::new(role.argv[0].clone(), role.argv[1..].to_vec());
            spec.cwd = Some(destination.to_path_buf());
            crate::tmux::NamedWindow {
                name: role.window.clone(),
                spec,
            }
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoleLaunch {
    role: Role,
    tool: LauncherKind,
    window: String,
    argv: Vec<OsString>,
    goal: String,
}

fn role_launches(spec: &NewProject, destination: &Path) -> Vec<RoleLaunch> {
    let project = project_label(destination, &spec.path);
    let mut assigned = Vec::new();
    if let Some(tool) = spec.orchestrator {
        assigned.push((Role::Orchestrator, tool));
    }
    if let Some(tool) = spec.executor {
        assigned.push((Role::Executor, tool));
    }
    if let Some(tool) = spec.reviewer {
        assigned.push((Role::Reviewer, tool));
    }
    let mut used = Vec::new();
    assigned
        .into_iter()
        .map(|(role, tool)| {
            let window = unique_window_name(tool, &project, role, &mut used);
            RoleLaunch {
                role,
                tool,
                window,
                argv: launch_argv(Some(tool)),
                goal: spec.goal.clone(),
            }
        })
        .collect()
}

fn unique_window_name(
    tool: LauncherKind,
    project: &str,
    role: Role,
    used: &mut Vec<String>,
) -> String {
    let standard = format!("{}-{project}", tool.as_str());
    let name = if used.iter().any(|existing| existing == &standard) {
        format!("{}-{project}-{}", tool.as_str(), role.as_str())
    } else {
        standard
    };
    used.push(name.clone());
    name
}

fn launch_argv(tool: Option<LauncherKind>) -> Vec<OsString> {
    match tool {
        None => vec![OsString::from("fish"), OsString::from("-li")],
        Some(LauncherKind::Pi) => vec![OsString::from("pi")],
        Some(LauncherKind::Rpi) => vec![OsString::from("rpi")],
        Some(LauncherKind::Omp) => {
            vec![OsString::from("omp"), OsString::from("--auto-approve")]
        }
        Some(LauncherKind::Grok) => {
            vec![OsString::from("grok"), OsString::from("--always-approve")]
        }
        Some(LauncherKind::Hyper) => {
            vec![OsString::from("hyper"), OsString::from("--always-approve")]
        }
        Some(LauncherKind::Droid) => {
            vec![
                OsString::from("droid"),
                OsString::from("--auto"),
                OsString::from("high"),
            ]
        }
        Some(LauncherKind::Codex) => vec![
            OsString::from("codex"),
            OsString::from("--ask-for-approval"),
            OsString::from("never"),
            OsString::from("--sandbox"),
            OsString::from("danger-full-access"),
        ],
        Some(LauncherKind::Claude) => vec![
            OsString::from("claude"),
            OsString::from("--dangerously-skip-permissions"),
        ],
        Some(LauncherKind::Agent) => vec![
            OsString::from("agent"),
            OsString::from("--force"),
            OsString::from("--trust"),
            OsString::from("--approve-mcps"),
        ],
    }
}

fn project_label(destination: &Path, fallback: &Path) -> String {
    sanitize_tmux_name(
        destination
            .file_name()
            .or_else(|| fallback.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "project".to_owned()),
    )
}

fn session_name(destination: &Path) -> String {
    sanitize_tmux_name(
        destination
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "project".to_owned()),
    )
}

fn sanitize_tmux_name(value: String) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            sanitized.push(character);
        } else {
            sanitized.push('_');
        }
    }
    let sanitized = sanitized.trim_start_matches(['-', '_']);
    if sanitized.is_empty() {
        "project".to_owned()
    } else {
        sanitized.to_owned()
    }
}

fn write_role_files(spec: &NewProject, destination: &Path) -> Result<()> {
    let launches = role_launches(spec, destination);
    if launches.is_empty() {
        return Ok(());
    }
    let dir = destination.join(".al");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let session = session_name(destination);
    for role in &launches {
        fs::write(
            dir.join(format!("{}.md", role.role.as_str())),
            role_brief(role, spec, &session, &launches),
        )
        .with_context(|| {
            format!(
                "writing {}",
                dir.join(format!("{}.md", role.role.as_str())).display()
            )
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoalRecipe {
    /// `/goal <objective>` then submit (grok, hyper, claude).
    Inline,
    /// `/goal` submit, then type the objective and submit (pi, omp).
    PromptThen,
}

fn goal_recipe(tool: LauncherKind) -> Option<GoalRecipe> {
    match tool {
        LauncherKind::Grok | LauncherKind::Hyper | LauncherKind::Claude => Some(GoalRecipe::Inline),
        LauncherKind::Pi | LauncherKind::Rpi | LauncherKind::Omp => Some(GoalRecipe::PromptThen),
        LauncherKind::Droid | LauncherKind::Codex | LauncherKind::Agent => None,
    }
}

fn apply_native_goals(session: &str, launches: &[RoleLaunch]) -> Result<()> {
    for role in launches {
        match goal_recipe(role.tool) {
            None => continue,
            Some(recipe) => send_native_goal(session, &role.window, recipe, &role.goal)?,
        }
    }
    Ok(())
}

fn send_native_goal(session: &str, window: &str, recipe: GoalRecipe, goal: &str) -> Result<()> {
    wait_for_tui(session, window);
    match recipe {
        GoalRecipe::Inline => {
            tmux_send_literal(session, window, &format!("/goal {goal}"))?;
            tmux_send_submit(session, window)
        }
        GoalRecipe::PromptThen => {
            tmux_send_literal(session, window, "/goal")?;
            tmux_send_submit(session, window)?;
            thread::sleep(Duration::from_millis(800));
            tmux_send_literal(session, window, goal)?;
            tmux_send_submit(session, window)
        }
    }
}

fn wait_for_tui(session: &str, window: &str) {
    let timeout = goal_wait();
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Ok(text) = tmux_capture(session, window) {
            if tui_looks_ready(&text) {
                return;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn goal_wait() -> Duration {
    env::var("AL_GOAL_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|millis: &u64| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(20))
}

fn tui_looks_ready(text: &str) -> bool {
    [
        "always-approve",
        "auto-approve",
        "bypass permissions",
        "YOLO mode",
        "No goal",
        "Goal:",
        "Shift+Tab",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn tmux_target(session: &str, window: &str) -> String {
    format!("={session}:{window}")
}

fn tmux_session_exists(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", &format!("={session}")])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn tmux_send_literal(session: &str, window: &str, text: &str) -> Result<()> {
    let target = tmux_target(session, window);
    let status = Command::new("tmux")
        .args(["send-keys", "-t", &target, "-l", "--", text])
        .status()
        .context("could not run tmux send-keys")?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "tmux send-keys to {target} exited {}",
            status.code().unwrap_or(1)
        )
    }
}

fn tmux_send_submit(session: &str, window: &str) -> Result<()> {
    let target = tmux_target(session, window);
    let status = Command::new("tmux")
        .args(["send-keys", "-t", &target, "C-m"])
        .status()
        .context("could not run tmux send-keys")?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "tmux submit to {target} exited {}",
            status.code().unwrap_or(1)
        )
    }
}

fn tmux_capture(session: &str, window: &str) -> Result<String> {
    let target = tmux_target(session, window);
    let output = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", &target])
        .output()
        .context("could not run tmux capture-pane")?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        bail!(
            "tmux capture-pane {target} exited {}",
            output.status.code().unwrap_or(1)
        )
    }
}

fn role_brief(role: &RoleLaunch, spec: &NewProject, session: &str, fleet: &[RoleLaunch]) -> String {
    let windows = fleet
        .iter()
        .map(|item| {
            format!(
                "- {}: {} ({})",
                item.role.as_str(),
                item.window,
                item.tool.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let envelope = "[<role> <session> <pane>] <TYPE> <task-id>: <body> | contact: <session> <pane>";
    let identity = "ME=$(tmux display-message -p '#{session_name} #{pane_id}')";
    let send = format!(
        "tmux send-keys -t {session}:<window> — use C-m, never Enter; send text + double Enter in one call"
    );
    let duties = match role.role {
        Role::Orchestrator => {
            "You are the only supervisor for this project (star topology). Plan, dispatch, own files, route review, and hold the validation lease. Do not implement. Only you send TASK. Require ACK / PROGRESS / DONE / BLOCKED / RELAY. Peer-to-peer is read-only AUDIT/INFO."
        }
        Role::Executor => {
            "You implement the goal. Edit only owned files. Do not expand scope. Do not run project-wide builds without the validation lease. Never idle-wait. Only the orchestrator assigns work. ACK with | modifying: <files> before edits. Reply DONE, BLOCKED, or RELAY. Never order other agents."
        }
        Role::Reviewer => {
            "You review security and correctness (bugs, spec, change propagation). Read-only unless assigned a fix. Send AUDIT, not work orders. High-risk persistence/protocol/security changes need P0=0 and P1=0. P2 does not block unless asked."
        }
    };
    format!(
        "# {}\n\n{duties}\n\n## Goal\n\n{}\n\n## Tmux\n\n- session: `{session}`\n- your window: `{}`\n- identity: `{identity}`\n{}\n\n## Communication\n\nEnvelope: `{envelope}`\n\nrole: supervisor | worker | peer\nTYPE: TASK (orchestrator only) · ACK · PROGRESS · DONE · BLOCKED · RELAY · AUDIT · INFO\n\nModification messages also declare `| modifying: <files>` before any edit.\n\n{send}\n\nState-check the target pane before every send (alive, intended agent, no picker, composer clean).\n",
        role.role.as_str(),
        spec.goal,
        role.window,
        windows,
    )
}

fn local_parent() -> Result<PathBuf> {
    if let Some(home) = env::var_os("AL_PROJECTS_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(home.join(DEFAULT_PARENT_DIR))
}

fn expand_path(path: &Path) -> Result<PathBuf> {
    if let Ok(stripped) = path.strip_prefix("~") {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        return Ok(if stripped.as_os_str().is_empty() {
            home
        } else {
            home.join(stripped)
        });
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(env::current_dir()?.join(path))
}

fn resolve_local_repo(path: &Path) -> Result<PathBuf> {
    if looks_like_path_buf(path) {
        return expand_path(path);
    }
    Ok(local_parent()?.join(path))
}

fn resolve_remote_repo(path: &Path) -> Result<PathBuf> {
    if looks_like_path_buf(path) {
        return remote_abs_path(path);
    }
    Ok(PathBuf::from("$HOME").join(DEFAULT_PARENT_DIR).join(path))
}

fn ensure_local_worktree(repo: &Path, dest: &Path, name: &str) -> Result<()> {
    if !is_git_checkout(repo) {
        bail!("project is not a git repository: {}", repo.display());
    }
    if dest.exists() {
        if dest.is_dir() {
            return Ok(());
        }
        bail!("worktree destination exists: {}", dest.display());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let dest = dest.to_str().context("worktree path is not UTF-8")?;
    if run_git(repo, &["worktree", "add", "-b", name, "--", dest]).is_ok() {
        return Ok(());
    }
    run_git(repo, &["worktree", "add", "--", dest, name])
}

fn local_worktree_destination(repo: &Path, name: &str) -> Result<PathBuf> {
    let repo_name = repo_file_name(repo)?;
    Ok(local_parent()?.join(format!("{repo_name}-{name}")))
}

fn remote_worktree_destination(repo: &Path, name: &str) -> Result<PathBuf> {
    let repo_name = repo_file_name(repo)?;
    Ok(PathBuf::from("$HOME")
        .join(DEFAULT_PARENT_DIR)
        .join(format!("{repo_name}-{name}")))
}

fn repo_file_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .context("project path has no directory name")?
        .to_string_lossy();
    if name.is_empty() || name == "~" {
        bail!("project path has no directory name");
    }
    Ok(name.into_owned())
}

fn remote_script(spec: &NewProject) -> Result<String> {
    if !spec.gits.is_empty() {
        return remote_clone_script(spec);
    }
    let repo = fish_path(&resolve_remote_repo(&spec.path)?)?;
    let name = repo_file_name(&spec.path)?;
    let readme = posix_quote(readme_contents(&name, &spec.goal).trim_end());
    let goal_file = posix_quote(goal_contents(&spec.goal).trim_end());
    let dest = match spec.worktree.as_deref() {
        Some(worktree) => fish_path(&remote_worktree_destination(&spec.path, worktree)?)?,
        None => "$repo".to_owned(),
    };
    let worktree = match spec.worktree.as_deref() {
        Some(worktree) => format!(
            "mkdir -p (dirname {dest}); and begin; test -d {dest}; or git worktree add -b {name} -- {dest}; or git worktree add -- {dest} {name}; end; and",
            dest = dest,
            name = posix_quote(worktree),
        ),
        None => String::new(),
    };
    let roles = remote_role_files(spec)?;
    Ok(format!(
        "set repo {repo}; if not test -d $repo; mkdir -p $repo; and cd $repo; and git init; and begin; printf '%s\\n' {readme} > README.md; printf '%s\\n' {goal_file} > GOAL.md; end; and git add README.md GOAL.md; and begin; git commit -m 'Initial commit'; or git -c user.name=al -c user.email=al@localhost commit -m 'Initial commit'; end; else; cd $repo; end; and {worktree} begin; printf '%s\\n' {goal_file} > {dest}/GOAL.md; end; {roles} and cd {dest}; and git add GOAL.md; and begin; git diff --cached --quiet; or begin; git commit -m 'Set goal'; or git -c user.name=al -c user.email=al@localhost commit -m 'Set goal'; end; end",
        repo = repo,
        readme = readme,
        goal_file = goal_file,
        dest = dest,
        worktree = worktree,
        roles = roles,
    ))
}

fn remote_clone_script(spec: &NewProject) -> Result<String> {
    let dest_path = git_destination(spec, true)?;
    let dest = fish_path(&dest_path)?;
    let goal_file = posix_quote(goal_contents(&spec.goal).trim_end());
    let mut script = format!("set dest {dest}; ");
    if spec.gits.len() == 1 {
        script.push_str(&format!(
            "mkdir -p (dirname $dest); and begin; test -d $dest/.git; or git clone -- {url} $dest; end; and ",
            url = posix_quote(&spec.gits[0]),
        ));
    } else {
        script.push_str("mkdir -p $dest; and ");
        for url in &spec.gits {
            let child = dest_path.join(repo_name_from_git(url)?);
            let child = fish_path(&child)?;
            script.push_str(&format!(
                "begin; test -d {child}/.git; or git clone -- {url} {child}; end; and ",
                url = posix_quote(url),
                child = child,
            ));
        }
    }
    let roles = remote_role_files(spec)?;
    script.push_str(&format!(
        "begin; printf '%s\\n' {goal_file} > $dest/GOAL.md; end; {roles} if test -d $dest/.git; cd $dest; and git add GOAL.md; and begin; git diff --cached --quiet; or begin; git commit -m 'Set goal'; or git -c user.name=al -c user.email=al@localhost commit -m 'Set goal'; end; end; end",
        goal_file = goal_file,
        roles = roles,
    ));
    Ok(script)
}

fn remote_role_files(spec: &NewProject) -> Result<String> {
    let destination = project_destination(spec, true)?;
    let launches = role_launches(spec, &destination);
    if launches.is_empty() {
        return Ok(String::new());
    }
    let dest = fish_path(&destination)?;
    let session = session_name(&destination);
    let mut script = format!("and mkdir -p {dest}/.al; ");
    for role in &launches {
        script.push_str(&format!(
            "and begin; printf '%s\\n' {body} > {dest}/.al/{name}.md; end; ",
            body = posix_quote(role_brief(role, spec, &session, &launches).trim_end()),
            name = role.role.as_str(),
        ));
    }
    Ok(script)
}

fn print_plan(spec: &NewProject) -> Result<()> {
    let remote = spec.host.is_some();
    let destination = project_destination(spec, remote)?;
    if spec.gits.is_empty() {
        let repo = if remote {
            resolve_remote_repo(&spec.path)?
        } else {
            resolve_local_repo(&spec.path)?
        };
        println!("project\t{}", repo.display());
    }
    println!("destination\t{}", destination.display());
    if spec.gits.is_empty() {
        if let Some(name) = spec.worktree.as_deref() {
            println!(
                "worktree\tgit worktree add -b {name} -- {}",
                destination.display()
            );
        }
    } else if spec.gits.len() == 1 {
        println!("clone\t{}\t{}", spec.gits[0], destination.display());
    } else {
        for url in &spec.gits {
            let child = destination.join(repo_name_from_git(url)?);
            println!("clone\t{url}\t{}", child.display());
        }
    }
    if let Some(host) = spec.host.as_deref() {
        println!(
            "bootstrap\t{}",
            crate::remote::argv_preview(
                host,
                &[&format!("fish -c {}", posix_quote(&remote_script(spec)?))],
                false
            )
            .join(" ")
        );
    } else if spec.gits.is_empty() {
        println!("bootstrap\tcreate-or-update GOAL.md && commit");
    } else {
        println!("bootstrap\tclone-or-update GOAL.md && commit");
    }
    let launches = role_launches(spec, &destination);
    if !launches.is_empty() || spec.tmux {
        let session = session_name(&destination);
        println!("session\t{session}");
        if launches.is_empty() {
            println!("window\t{session}\tfish -li");
        } else {
            for role in &launches {
                let launch = role
                    .argv
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("window\t{}\t{launch}", role.window);
                match goal_recipe(role.tool) {
                    Some(GoalRecipe::Inline) => {
                        println!("goal\t{}\t/goal {}", role.window, role.goal);
                    }
                    Some(GoalRecipe::PromptThen) => {
                        println!("goal\t{}\t/goal then {}", role.window, role.goal);
                    }
                    None => {
                        println!(
                            "goal\t{}\tGOAL.md only ({} has no /goal)",
                            role.window,
                            role.tool.as_str()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn looks_like_path(value: &str) -> bool {
    value == "."
        || value == ".."
        || value.starts_with('.')
        || value.starts_with('~')
        || value.contains('/')
        || value.contains('\\')
}

fn looks_like_path_buf(path: &Path) -> bool {
    path.to_str().is_some_and(looks_like_path) || path.is_absolute()
}

fn remote_abs_path(path: &Path) -> Result<PathBuf> {
    match path {
        path if path.as_os_str() == OsStr::new("~") => Ok(PathBuf::from("$HOME")),
        path if path.starts_with("~/") => {
            let rest = path
                .strip_prefix("~")
                .or_else(|_| path.strip_prefix("~/"))
                .unwrap_or(path);
            Ok(PathBuf::from("$HOME").join(rest))
        }
        path if path.is_absolute() => Ok(path.to_path_buf()),
        _ => bail!("remote project path must be absolute or start with ~/"),
    }
}

fn fish_path(path: &Path) -> Result<String> {
    if path.as_os_str() == OsStr::new("$HOME") {
        return Ok("\"$HOME\"".to_owned());
    }
    if let Ok(rest) = path.strip_prefix("$HOME") {
        let rest = rest
            .to_str()
            .context("project path is not UTF-8")?
            .trim_start_matches('/');
        if rest.is_empty() {
            return Ok("\"$HOME\"".to_owned());
        }
        if rest.contains('"') || rest.contains('$') || rest.contains('\\') {
            bail!("remote path must not contain \", $, or \\ after $HOME");
        }
        return Ok(format!("\"$HOME/{rest}\""));
    }
    Ok(posix_quote(
        path.to_str().context("project path is not UTF-8")?,
    ))
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("host must not be empty");
    }
    if host == "local" {
        bail!("host 'local' is reserved; omit the host to use this machine");
    }
    if host.starts_with('-') {
        bail!("host must not start with '-'");
    }
    if host
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        bail!("host must not contain whitespace or control characters");
    }
    Ok(())
}

fn posix_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn spec(path: PathBuf, goal: &str) -> NewProject {
        NewProject {
            host: None,
            path,
            goal: goal.to_owned(),
            executor: None,
            orchestrator: None,
            reviewer: None,
            worktree: None,
            gits: Vec::new(),
            tmux: false,
            print_command: false,
        }
    }

    #[test]
    fn project_name_rejects_unsafe_values() {
        assert!(validate_project_name("sample-app").is_ok());
        assert!(validate_project_name("").is_err());
        assert!(validate_project_name("-hidden").is_err());
        assert!(validate_project_name("../escape").is_err());
        assert!(validate_project_name("has/slash").is_err());
        assert!(validate_project_name("has space").is_err());
    }

    #[test]
    fn launcher_spellings_map_to_kinds() {
        assert_eq!(parse_launcher_tool("pilo").unwrap(), LauncherKind::Pi);
        assert_eq!(parse_launcher_tool("pi").unwrap(), LauncherKind::Pi);
        assert_eq!(parse_launcher_tool("grolo").unwrap(), LauncherKind::Grok);
        assert!(parse_launcher_tool("sks").is_err());
    }

    #[test]
    fn targets_treat_two_args_as_local() {
        let (host, path, goal) =
            parse_new_targets(&["~/Projects/pi-zig".into(), "ship it".into()]).unwrap();
        assert_eq!(host, None);
        assert_eq!(path, PathBuf::from("~/Projects/pi-zig"));
        assert_eq!(goal, "ship it");
    }

    #[test]
    fn targets_take_host_then_path() {
        let (host, path, goal) = parse_new_targets(&[
            "x3".into(),
            "~/Projects/pi-zig".into(),
            "完成zig版本的pi-coding-agent".into(),
        ])
        .unwrap();
        assert_eq!(host.as_deref(), Some("x3"));
        assert_eq!(path, PathBuf::from("~/Projects/pi-zig"));
        assert_eq!(goal, "完成zig版本的pi-coding-agent");
        assert!(
            parse_new_targets(&["~/Projects/a".into(), "~/Projects/b".into(), "goal".into()])
                .is_err()
        );
        let (host, _, _) =
            parse_new_targets(&["local".into(), "~/Projects/pi-zig".into(), "goal".into()])
                .unwrap();
        assert_eq!(host, None);
    }

    #[test]
    fn local_create_writes_goal_and_initial_commit() {
        let home = TempDir::new().unwrap();
        let destination =
            apply_local(&spec(home.path().join("sample-app"), "ship the parser")).unwrap();
        assert_eq!(destination, home.path().join("sample-app"));
        let readme = fs::read_to_string(destination.join("README.md")).unwrap();
        let goal = fs::read_to_string(destination.join("GOAL.md")).unwrap();
        assert!(readme.contains("# sample-app"));
        assert!(goal.contains("ship the parser"));
        let log = Command::new("git")
            .args(["log", "-1", "--pretty=%s"])
            .current_dir(&destination)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "Initial commit"
        );
    }

    #[test]
    fn local_existing_repo_updates_goal() {
        let home = TempDir::new().unwrap();
        let repo = home.path().join("sample-app");
        apply_local(&spec(repo.clone(), "first")).unwrap();
        apply_local(&spec(repo.clone(), "ship the parser")).unwrap();
        let goal = fs::read_to_string(repo.join("GOAL.md")).unwrap();
        assert!(goal.contains("ship the parser"));
        let log = Command::new("git")
            .args(["log", "-1", "--pretty=%s"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "Set goal");
    }

    #[test]
    fn git_url_name_and_validation() {
        assert_eq!(
            repo_name_from_git("https://example.test/org/demo.git").unwrap(),
            "demo"
        );
        assert_eq!(
            repo_name_from_git("git@example.test:org/demo.git").unwrap(),
            "demo"
        );
        assert_eq!(repo_name_from_git("/tmp/foo/bar").unwrap(), "bar");
        assert!(parse_git_url("-evil").is_err());
        assert!(parse_git_url("https://example.test/org/has space.git").is_err());
        assert!(parse_git_url("").is_err());
    }

    #[test]
    fn multiple_gits_require_worktree() {
        let mut project = spec(PathBuf::from("bundle"), "ship");
        project.gits = vec![
            "https://example.test/org/alpha.git".into(),
            "https://example.test/org/beta.git".into(),
        ];
        let error = run(project).unwrap_err().to_string();
        assert!(
            error.contains("multiple --git requires --worktree"),
            "{error}"
        );
    }

    #[test]
    fn duplicate_git_repo_names_are_rejected() {
        let mut project = spec(PathBuf::from("bundle"), "ship");
        project.worktree = Some("wt".into());
        project.gits = vec![
            "https://example.test/org/demo.git".into(),
            "https://example.test/other/demo.git".into(),
        ];
        let error = run(project).unwrap_err().to_string();
        assert!(error.contains("duplicate --git repo name demo"), "{error}");
    }

    #[test]
    fn single_git_clones_into_named_project() {
        let home = TempDir::new().unwrap();
        let upstream = seed_git_repo(home.path().join("demo"));
        let dest = home.path().join("sample-app");
        let mut project = spec(dest.clone(), "ship the parser");
        project.gits = vec![upstream.display().to_string()];
        let destination = apply_local(&project).unwrap();
        assert_eq!(destination, dest);
        assert!(dest.join(".git").exists());
        assert!(dest.join("README.md").exists());
        let goal = fs::read_to_string(dest.join("GOAL.md")).unwrap();
        assert!(goal.contains("ship the parser"));
        let log = Command::new("git")
            .args(["log", "-1", "--pretty=%s"])
            .current_dir(&dest)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "Set goal");
    }

    #[test]
    fn existing_git_clone_skips_clone_and_updates_goal() {
        let home = TempDir::new().unwrap();
        let upstream = seed_git_repo(home.path().join("demo"));
        let dest = home.path().join("sample-app");
        let mut project = spec(dest.clone(), "first");
        project.gits = vec![upstream.display().to_string()];
        apply_local(&project).unwrap();
        project.goal = "ship the parser".into();
        apply_local(&project).unwrap();
        let goal = fs::read_to_string(dest.join("GOAL.md")).unwrap();
        assert!(goal.contains("ship the parser"));
        let log = Command::new("git")
            .args(["log", "--pretty=%s"])
            .current_dir(&dest)
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(subjects.contains("Set goal"), "{subjects}");
        assert_eq!(subjects.matches("Set goal").count(), 2);
    }

    #[test]
    fn git_destination_maps_bare_worktree_to_projects_worktree() {
        let mut project = spec(PathBuf::from("bundle"), "ship");
        project.worktree = Some("wt".into());
        project.gits = vec![
            "https://example.test/org/alpha.git".into(),
            "https://example.test/org/beta.git".into(),
        ];
        assert_eq!(
            git_destination(&project, true).unwrap(),
            PathBuf::from("$HOME/Projects/worktree")
        );
        project.worktree = Some("feat".into());
        assert_eq!(
            git_destination(&project, true).unwrap(),
            PathBuf::from("$HOME/Projects/feat")
        );
        project.gits = vec!["https://example.test/org/demo.git".into()];
        project.worktree = Some("wt".into());
        assert_eq!(
            git_destination(&project, true).unwrap(),
            PathBuf::from("$HOME/Projects/worktree")
        );
    }

    #[test]
    fn remote_clone_script_skips_init_and_worktree_add() {
        let spec = NewProject {
            host: Some("host-a".to_owned()),
            path: PathBuf::from("bundle"),
            goal: "don't leak 'quotes'".to_owned(),
            executor: None,
            orchestrator: None,
            reviewer: None,
            worktree: Some("wt".to_owned()),
            gits: vec![
                "https://example.test/org/alpha.git".into(),
                "https://example.test/org/beta.git".into(),
            ],
            tmux: false,
            print_command: true,
        };
        let script = remote_script(&spec).unwrap();
        assert!(script.contains("git clone -- 'https://example.test/org/alpha.git'"));
        assert!(script.contains("\"$HOME/Projects/worktree/alpha\""));
        assert!(script.contains("\"$HOME/Projects/worktree/beta\""));
        assert!(!script.contains("git init"));
        assert!(!script.contains("git worktree add"));
        assert!(script.contains(&posix_quote("# Goal\n\ndon't leak 'quotes'")));
    }

    fn seed_git_repo(dir: PathBuf) -> PathBuf {
        fs::create_dir_all(&dir).unwrap();
        let status = Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        fs::write(dir.join("README.md"), "# upstream\n").unwrap();
        let status = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());
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
            .unwrap();
        assert!(status.success());
        dir
    }

    #[test]
    fn print_command_does_not_create_files() {
        let home = TempDir::new().unwrap();
        let path = home.path().join("sample-app");
        let mut project = spec(path.clone(), "ship the parser");
        project.executor = Some(LauncherKind::Pi);
        project.tmux = true;
        project.print_command = true;
        run(project).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn remote_script_creates_if_missing_and_supports_worktree() {
        let spec = NewProject {
            host: Some("x3".to_owned()),
            path: PathBuf::from("~/Projects/pi-zig"),
            goal: "don't leak 'quotes'".to_owned(),
            executor: Some(LauncherKind::Omp),
            orchestrator: Some(LauncherKind::Pi),
            reviewer: Some(LauncherKind::Grok),
            worktree: Some("wt".to_owned()),
            gits: Vec::new(),
            tmux: true,
            print_command: true,
        };
        let script = remote_script(&spec).unwrap();
        assert!(script.contains("mkdir -p $repo"));
        assert!(script.contains("git init"));
        assert!(script.contains("Initial commit"));
        assert!(script.contains(&posix_quote("# Goal\n\ndon't leak 'quotes'")));
        assert!(script.contains("\"$HOME/Projects/pi-zig\""));
        assert!(script.contains("\"$HOME/Projects/pi-zig-wt\""));
        assert!(script.contains("git worktree add"));
        assert!(script.contains(".al/executor.md"));
        assert!(script.contains(".al/orchestrator.md"));
        assert!(script.contains(".al/reviewer.md"));
        assert_eq!(
            remote_worktree_destination(&spec.path, "wt").unwrap(),
            PathBuf::from("$HOME/Projects/pi-zig-wt")
        );
    }

    #[test]
    fn fish_path_keeps_home_expandable() {
        assert_eq!(
            fish_path(Path::new("$HOME/Projects/pi-zig")).unwrap(),
            "\"$HOME/Projects/pi-zig\""
        );
        assert_eq!(
            fish_path(Path::new("/srv/workspace")).unwrap(),
            "'/srv/workspace'"
        );
    }

    #[test]
    fn roles_use_standard_window_names_in_one_session() {
        let spec = NewProject {
            host: None,
            path: PathBuf::from("~/Projects/pi-zig"),
            goal: "goal".to_owned(),
            executor: Some(LauncherKind::Omp),
            orchestrator: Some(LauncherKind::Pi),
            reviewer: Some(LauncherKind::Grok),
            worktree: Some("wt".to_owned()),
            gits: Vec::new(),
            tmux: true,
            print_command: true,
        };
        let dest = Path::new("$HOME/Projects/pi-zig-wt");
        let launches = role_launches(&spec, dest);
        assert_eq!(session_name(dest), "pi-zig-wt");
        assert_eq!(
            launches
                .iter()
                .map(|role| (role.role, role.window.as_str()))
                .collect::<Vec<_>>(),
            [
                (Role::Orchestrator, "pilo-pi-zig-wt"),
                (Role::Executor, "omlo-pi-zig-wt"),
                (Role::Reviewer, "grolo-pi-zig-wt"),
            ]
        );
        assert_eq!(
            launches[1].argv,
            vec![OsString::from("omp"), OsString::from("--auto-approve")]
        );
        assert_eq!(goal_recipe(LauncherKind::Grok), Some(GoalRecipe::Inline));
        assert_eq!(goal_recipe(LauncherKind::Hyper), Some(GoalRecipe::Inline));
        assert_eq!(goal_recipe(LauncherKind::Claude), Some(GoalRecipe::Inline));
        assert_eq!(goal_recipe(LauncherKind::Omp), Some(GoalRecipe::PromptThen));
        assert_eq!(goal_recipe(LauncherKind::Pi), Some(GoalRecipe::PromptThen));
        assert_eq!(goal_recipe(LauncherKind::Rpi), Some(GoalRecipe::PromptThen));
        assert_eq!(goal_recipe(LauncherKind::Agent), None);
        assert_eq!(goal_recipe(LauncherKind::Codex), None);
        assert_eq!(goal_recipe(LauncherKind::Droid), None);
        assert_eq!(tmux_target("demo", "grolo-demo"), "=demo:grolo-demo");
        assert!(!tui_looks_ready("loading\nplease wait\nstarting agent\n"));
        assert!(!tui_looks_ready("set a goal later"));
        assert!(tui_looks_ready("always-approve\n❯\n"));
        assert!(tui_looks_ready("[⋅ Goal: Planning]"));
        assert!(tui_looks_ready("No goal set"));
    }

    #[test]
    fn remote_open_forwards_no_tmux_and_uses_copied_al() {
        let spec = NewProject {
            host: Some("host-a".to_owned()),
            path: PathBuf::from("~/Projects/sample-app"),
            goal: "ship the parser".to_owned(),
            executor: Some(LauncherKind::Grok),
            orchestrator: None,
            reviewer: None,
            worktree: None,
            gits: Vec::new(),
            tmux: false,
            print_command: false,
        };
        let inner = remote_open_inner(&spec, "\"$HOME/Projects/sample-app\"", "/tmp/al-0.6.0");
        assert!(inner.contains("/tmp/al-0.6.0"));
        assert!(inner.contains("--executor grolo"));
        assert!(inner.contains("--no-tmux"));
        assert!(!inner.contains("--tmux"));
        let mut tmux = spec;
        tmux.tmux = true;
        let inner = remote_open_inner(&tmux, "\"$HOME/Projects/sample-app\"", "/tmp/al-0.6.0");
        assert!(inner.contains("--tmux"));
        assert!(!inner.contains("--no-tmux"));
    }

    #[cfg(unix)]
    #[test]
    fn existing_tmux_session_is_refused() {
        if !tmux_available() {
            eprintln!("skipping existing_tmux_session_is_refused: tmux not on PATH");
            return;
        }
        let home = TempDir::new().unwrap();
        let dest = apply_local(&spec(
            home.path()
                .join(format!("altest-exists-{}", std::process::id())),
            "ship",
        ))
        .unwrap();
        let session = session_name(&dest);
        let _guard = TmuxSessionGuard {
            session: session.clone(),
        };
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", &session, "sleep", "30"])
            .status()
            .unwrap();
        assert!(status.success());
        let mut project = spec(dest, "ship");
        project.executor = Some(LauncherKind::Grok);
        project.tmux = true;
        let error = run(project).unwrap_err().to_string();
        assert!(
            error.contains("already exists"),
            "expected refuse, got {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_goal_is_sent_with_tmux_send_keys() {
        if !tmux_available() {
            eprintln!("skipping native_goal_is_sent_with_tmux_send_keys: tmux not on PATH");
            return;
        }
        let id = format!(
            "altest-goal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let window = "grolo-sample";
        let _guard = TmuxSessionGuard {
            session: id.clone(),
        };
        let status = Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &id,
                "-n",
                window,
                "sh",
                "-c",
                "printf 'ready\\nNo goal\\n>\\n'; exec cat",
            ])
            .status()
            .unwrap();
        assert!(status.success(), "tmux new-session failed");
        assert!(tmux_session_exists(&id));
        assert!(
            !tmux_session_exists("altest-goal"),
            "prefix must not match {id}"
        );
        send_native_goal(&id, window, GoalRecipe::Inline, "ship the parser").unwrap();
        let pane = tmux_capture(&id, window).unwrap();
        assert!(
            pane.contains("/goal ship the parser"),
            "expected /goal in pane, got {pane:?}"
        );
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
    struct TmuxSessionGuard {
        session: String,
    }

    #[cfg(unix)]
    impl Drop for TmuxSessionGuard {
        fn drop(&mut self) {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &self.session])
                .status();
        }
    }
}
