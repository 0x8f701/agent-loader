use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::SystemTime;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::Value;
use thiserror::Error;
use walkdir::WalkDir;

use crate::domain::TargetTool;

const REMOTE_PATH_MAPS_ENV: &str = "AL_REMOTE_PATH_MAPS";
const URL_PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'!')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env_remove: Vec<OsString>,
    pub env_set: Vec<(OsString, OsString)>,
}

impl CommandSpec {
    pub fn new(program: impl Into<OsString>, args: Vec<OsString>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            env_remove: Vec::new(),
            env_set: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherKind {
    Omp,
    Pi,
    Grok,
    Hyper,
    Droid,
    Codex,
    Claude,
}

impl LauncherKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Omp => "omlo",
            Self::Pi => "pilo",
            Self::Grok => "grolo",
            Self::Hyper => "hyperlo",
            Self::Droid => "dolo",
            Self::Codex => "colo",
            Self::Claude => "cclo",
        }
    }

    const fn argument_error_code(self) -> i32 {
        match self {
            Self::Grok | Self::Hyper | Self::Claude => 2,
            Self::Omp | Self::Pi | Self::Droid | Self::Codex => 1,
        }
    }

    const fn common_option_error_code(self) -> i32 {
        2
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchPlan {
    Command(CommandSpec),
    Fallback {
        primary: CommandSpec,
        fallback: CommandSpec,
    },
    Tmux {
        session: String,
        command: CommandSpec,
        fallback: Option<CommandSpec>,
    },
    Remote {
        host: OsString,
        repo_root: PathBuf,
        workdir: PathBuf,
        worktree: Option<OsString>,
        launcher: LauncherKind,
        argv: Vec<OsString>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeNameError {
    Empty,
    LeadingHyphen,
    Dot,
    DotDot,
    ContainsSlash,
}

impl std::fmt::Display for WorktreeNameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "must not be empty",
            Self::LeadingHyphen => "must not start with '-'",
            Self::Dot => "must not be '.'",
            Self::DotDot => "must not be '..'",
            Self::ContainsSlash => "must not contain '/'",
        })
    }
}

#[derive(Debug, Error)]
pub enum LauncherError {
    #[error("{launcher}: {option} requires a value")]
    MissingValue {
        launcher: &'static str,
        option: &'static str,
        code: i32,
    },
    #[error("{launcher}: {option} requires a non-option value")]
    OptionValue {
        launcher: &'static str,
        option: &'static str,
        code: i32,
    },
    #[error("{launcher}: --wt requires --host")]
    WorktreeRequiresHost {
        launcher: &'static str,
        code: i32,
    },
    #[error("invalid worktree name {name:?}: {reason}")]
    InvalidWorktreeName {
        name: OsString,
        reason: WorktreeNameError,
        code: i32,
    },
    #[error("{launcher}: not inside a git repository: {cwd}")]
    NotGitRepository {
        launcher: &'static str,
        cwd: PathBuf,
        code: i32,
    },
    #[error("{launcher}: no resumable session found under {root}")]
    NoSession {
        launcher: &'static str,
        root: PathBuf,
        code: i32,
    },
    #[error("{launcher}: could not read a session ID from {path}")]
    SessionId {
        launcher: &'static str,
        path: PathBuf,
        code: i32,
    },
    #[error("target executable not found: {executable:?}")]
    MissingExecutable { executable: OsString },
    #[error("{launcher}: path is not valid UTF-8: {path:?}")]
    NonUtf8Path {
        launcher: &'static str,
        path: PathBuf,
        code: i32,
    },
    #[error("AL_REMOTE_PATH_MAPS is not valid UTF-8")]
    NonUtf8RemotePathMaps { code: i32 },
    #[error("invalid AL_REMOTE_PATH_MAPS JSON: {source}")]
    InvalidRemotePathMapsJson {
        #[source]
        source: serde_json::Error,
        code: i32,
    },
    #[error("invalid AL_REMOTE_PATH_MAPS entry {index}: {side} path must be absolute: {path:?}")]
    InvalidRemotePathMapPath {
        index: usize,
        side: &'static str,
        path: PathBuf,
        code: i32,
    },
    #[error("hyperlo --fork --worktree only supports the interactive TUI")]
    HyperWorktreeHeadless,
    #[error("failed to execute {program:?}: {source}")]
    Execute {
        program: OsString,
        #[source]
        source: io::Error,
    },
}

impl LauncherError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::MissingValue { code, .. }
            | Self::OptionValue { code, .. }
            | Self::WorktreeRequiresHost { code, .. }
            | Self::InvalidWorktreeName { code, .. }
            | Self::NotGitRepository { code, .. }
            | Self::NoSession { code, .. }
            | Self::SessionId { code, .. }
            | Self::NonUtf8Path { code, .. }
            | Self::NonUtf8RemotePathMaps { code }
            | Self::InvalidRemotePathMapsJson { code, .. }
            | Self::InvalidRemotePathMapPath { code, .. } => *code,
            Self::MissingExecutable { .. } => 127,
            Self::HyperWorktreeHeadless => 2,
            Self::Execute { source, .. } => match source.kind() {
                io::ErrorKind::NotFound => 127,
                io::ErrorKind::PermissionDenied => 126,
                _ => 1,
            },
        }
    }
}

pub type Result<T, E = LauncherError> = std::result::Result<T, E>;

/// Build the raw, native argv for resuming an exact session.
///
/// `session_path` is authoritative for Pi and OMP. The remaining tools resume
/// by `session_id`. `home` is used to prefer Hyper's managed executable at
/// `~/.hyper/bin/hyper` without consulting a shell.
pub fn native_resume(
    target: TargetTool,
    session_path: &Path,
    session_id: &str,
    home: &Path,
) -> CommandSpec {
    match target {
        TargetTool::Pi => command("pi", [os("--session"), session_path.as_os_str().to_owned()]),
        TargetTool::Omp => command("omp", [os("--resume"), session_path.as_os_str().to_owned()]),
        TargetTool::Droid => command("droid", [os("--resume"), os(session_id)]),
        TargetTool::Codex => command("codex", [os("resume"), os(session_id)]),
        TargetTool::Claude => command("claude", [os("--resume"), os(session_id)]),
        TargetTool::Grok => command("grok", [os("--resume"), os(session_id)]),
        TargetTool::Hyper => command(
            preferred_hyper_executable(home),
            [os("--resume"), os(session_id)],
        ),
    }
}

/// Build the raw, native argv for forking an exact session.
pub fn native_fork(
    target: TargetTool,
    session_path: &Path,
    session_id: &str,
    home: &Path,
) -> CommandSpec {
    match target {
        TargetTool::Pi => command("pi", [os("--fork"), session_path.as_os_str().to_owned()]),
        TargetTool::Omp => command("omp", [os("--fork"), session_path.as_os_str().to_owned()]),
        TargetTool::Droid => command("droid", [os("--fork"), os(session_id)]),
        TargetTool::Codex => command("codex", [os("fork"), os(session_id)]),
        TargetTool::Claude => command(
            "claude",
            [os("--fork-session"), os("--resume"), os(session_id)],
        ),
        TargetTool::Grok => command(
            "grok",
            [os("--fork-session"), os("--resume"), os(session_id)],
        ),
        TargetTool::Hyper => command(
            preferred_hyper_executable(home),
            [os("--fork-session"), os("--resume"), os(session_id)],
        ),
    }
}

/// Resolve the executable used by a native target without involving a shell.
pub fn resolve_tool_executable(target: TargetTool, home: &Path) -> Result<OsString> {
    let program = match target {
        TargetTool::Pi => OsString::from("pi"),
        TargetTool::Omp => OsString::from("omp"),
        TargetTool::Droid => OsString::from("droid"),
        TargetTool::Codex => OsString::from("codex"),
        TargetTool::Claude => OsString::from("claude"),
        TargetTool::Grok => OsString::from("grok"),
        TargetTool::Hyper => preferred_hyper_executable(home),
    };
    resolve_executable(&program)
        .map(PathBuf::into_os_string)
        .ok_or(LauncherError::MissingExecutable {
            executable: program,
        })
}

/// Resolve an executable using the process PATH. Explicit paths are checked
/// directly. The returned path is passed to `Command` as an argv value, never
/// interpolated into a shell string.
pub fn resolve_executable(program: &OsStr) -> Option<PathBuf> {
    let program_path = Path::new(program);
    if program_path.is_absolute()
        || program_path.parent().is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return is_executable_file(program_path).then(|| program_path.to_owned());
    }
    let path = env::var_os("PATH")?;
    resolve_executable_in(program, &path)
}

pub fn resolve_executable_in(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    for directory in env::split_paths(path) {
        let candidate = directory.join(program);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        if candidate.extension().is_none() {
            let extensions = env::var_os("PATHEXT")
                .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
            for extension in extensions.to_string_lossy().split(';') {
                let extension = extension.trim_start_matches('.');
                let candidate = candidate.with_extension(extension);
                if is_executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Return the canonical recorded cwd when it still names a directory.
/// Otherwise the caller should inherit its current directory.
pub fn local_recorded_cwd(recorded_cwd: &Path) -> Option<PathBuf> {
    if recorded_cwd.as_os_str().is_empty() || !recorded_cwd.is_dir() {
        return None;
    }
    fs::canonicalize(recorded_cwd).ok()
}

pub fn validate_worktree_name(name: &OsStr) -> Result<()> {
    let reason = if name.is_empty() {
        Some(WorktreeNameError::Empty)
    } else if name.as_encoded_bytes().first() == Some(&b'-') {
        Some(WorktreeNameError::LeadingHyphen)
    } else if name == OsStr::new(".") {
        Some(WorktreeNameError::Dot)
    } else if name == OsStr::new("..") {
        Some(WorktreeNameError::DotDot)
    } else if name.as_encoded_bytes().contains(&b'/') {
        Some(WorktreeNameError::ContainsSlash)
    } else {
        None
    };

    match reason {
        Some(reason) => Err(LauncherError::InvalidWorktreeName {
            name: name.to_owned(),
            reason,
            code: 2,
        }),
        None => Ok(()),
    }
}

fn parse_remote_path_maps(value: Option<OsString>) -> Result<Vec<(PathBuf, PathBuf)>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let value = value
        .into_string()
        .map_err(|_| LauncherError::NonUtf8RemotePathMaps { code: 2 })?;
    let mappings: Vec<[String; 2]> =
        serde_json::from_str(&value).map_err(|source| LauncherError::InvalidRemotePathMapsJson {
            source,
            code: 2,
        })?;

    mappings
        .into_iter()
        .enumerate()
        .map(|(index, [source, destination])| {
            let source = PathBuf::from(source);
            if !source.is_absolute() {
                return Err(LauncherError::InvalidRemotePathMapPath {
                    index: index + 1,
                    side: "source",
                    path: source,
                    code: 2,
                });
            }
            let destination = PathBuf::from(destination);
            if !destination.is_absolute() {
                return Err(LauncherError::InvalidRemotePathMapPath {
                    index: index + 1,
                    side: "destination",
                    path: destination,
                    code: 2,
                });
            }
            Ok((source, destination))
        })
        .collect()
}

fn darwin_remote_path_with_mappings(
    path: &Path,
    user: &OsStr,
    mappings: &[(PathBuf, PathBuf)],
) -> PathBuf {
    let root = Path::new("/");
    let user_source = root.join("Users").join(user);
    if let Ok(suffix) = path.strip_prefix(&user_source) {
        return root.join("home").join(user).join(suffix);
    }
    // The generic home mapping has precedence; configured mappings retain their JSON order.
    for (source, destination) in mappings {
        if let Ok(suffix) = path.strip_prefix(source) {
            return destination.join(suffix);
        }
    }
    path.to_owned()
}

/// Apply the generic macOS-to-Linux user-home mapping. Prefixes are matched on
/// path-component boundaries, so a short username never rewrites a longer name
/// that only shares that username as a prefix.
pub fn darwin_remote_path(path: &Path, user: &OsStr) -> PathBuf {
    darwin_remote_path_with_mappings(path, user, &[])
}

pub fn remote_path(path: &Path, user: &OsStr, is_darwin: bool) -> Result<PathBuf> {
    let mappings = parse_remote_path_maps(env::var_os(REMOTE_PATH_MAPS_ENV))?;
    if is_darwin {
        Ok(darwin_remote_path_with_mappings(path, user, &mappings))
    } else {
        Ok(path.to_owned())
    }
}

pub fn tmux_session_name(tool: &str, repo_root: Option<&Path>, cwd: &Path) -> String {
    let project_path = repo_root.unwrap_or(cwd);
    let project = project_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let mut sanitized = String::with_capacity(project.len());
    for character in project.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            sanitized.push(character);
        } else {
            sanitized.push('_');
        }
    }
    let sanitized = sanitized.trim_start_matches(['-', '_']);
    let project = if sanitized.is_empty() { "root" } else { sanitized };
    format!("{tool}-{project}")
}

pub fn build_launcher(
    kind: LauncherKind,
    argv: &[OsString],
    home: &Path,
    cwd: &Path,
) -> Result<LaunchPlan> {
    let common = parse_common(kind, argv)?;
    if let Some(name) = common.worktree.as_deref() {
        validate_worktree_name(name)?;
    }
    if common.worktree.is_some() && common.host.is_none() {
        return Err(LauncherError::WorktreeRequiresHost {
            launcher: kind.as_str(),
            code: 2,
        });
    }

    if let Some(host) = common.host {
        return build_remote(
            kind,
            host,
            common.worktree,
            common.tmux,
            common.args,
            common.tail,
            cwd,
        );
    }

    let repo_root = find_repo_root(cwd);
    // Local builds receive the protected tail appended to the tool args without
    // the `--` delimiter, preserving the established local passthrough behavior.
    let mut local_args = common.args;
    local_args.extend(common.tail);
    let mut plan = build_local(kind, &local_args, home, cwd, repo_root.as_deref())?;
    if common.tmux {
        let (mut command, mut fallback) = match plan {
            LaunchPlan::Command(command) => (command, None),
            LaunchPlan::Fallback { primary, fallback } => (primary, Some(fallback)),
            LaunchPlan::Tmux { .. } | LaunchPlan::Remote { .. } => unreachable!(),
        };
        if matches!(kind, LauncherKind::Droid | LauncherKind::Codex) {
            command.env_remove.push(os("NO_COLOR"));
        }
        if let Some(fallback) = &mut fallback {
            if matches!(kind, LauncherKind::Droid | LauncherKind::Codex) {
                fallback.env_remove.push(os("NO_COLOR"));
            }
        }
        let session = tmux_session_name(kind.as_str(), repo_root.as_deref(), cwd);
        plan = LaunchPlan::Tmux {
            session,
            command,
            fallback,
        };
    }
    Ok(plan)
}

#[derive(Debug)]
struct CommonArgs {
    tmux: bool,
    host: Option<OsString>,
    worktree: Option<OsString>,
    args: Vec<OsString>,
    /// Arguments captured after a literal `--` boundary. Preserved byte-for-byte
    /// (OsString) so non-UTF8 tails round-trip exactly. Local builds receive them
    /// appended to `args` without the delimiter; remote builds reinsert `--`
    /// immediately before this tail so literal controls are not reinterpreted.
    tail: Vec<OsString>,
}

fn parse_common(kind: LauncherKind, argv: &[OsString]) -> Result<CommonArgs> {
    let mut parsed = CommonArgs {
        tmux: false,
        host: None,
        worktree: None,
        args: Vec::new(),
        tail: Vec::new(),
    };
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        if arg == "--" {
            for tail in &argv[index + 1..] {
                parsed.tail.push(tail.clone());
            }
            break;
        } else if arg == "--tmux" {
            parsed.tmux = true;
        } else if arg == "--host" || arg == "--wt" {
            let option = if arg == "--host" { "--host" } else { "--wt" };
            index += 1;
            let Some(value) = argv.get(index) else {
                return Err(LauncherError::MissingValue {
                    launcher: kind.as_str(),
                    option,
                    code: kind.common_option_error_code(),
                });
            };
            if value.is_empty() || starts_with_hyphen(value) {
                return Err(LauncherError::OptionValue {
                    launcher: kind.as_str(),
                    option,
                    code: kind.common_option_error_code(),
                });
            }
            if option == "--host" {
                parsed.host = Some(value.clone());
            } else {
                parsed.worktree = Some(value.clone());
            }
        } else if let Some(value) = utf8_option_value(arg, "--host=") {
            if value.is_empty() {
                return Err(LauncherError::MissingValue {
                    launcher: kind.as_str(),
                    option: "--host",
                    code: kind.common_option_error_code(),
                });
            }
            parsed.host = Some(OsString::from(value));
        } else if let Some(value) = utf8_option_value(arg, "--wt=") {
            if value.is_empty() || value.starts_with('-') {
                return Err(LauncherError::OptionValue {
                    launcher: kind.as_str(),
                    option: "--wt",
                    code: kind.common_option_error_code(),
                });
            }
            parsed.worktree = Some(OsString::from(value));
        } else if let Some(value) = utf8_option_value(arg, "--session=") {
            if value.is_empty() || value.starts_with('-') {
                return Err(LauncherError::OptionValue {
                    launcher: kind.as_str(),
                    option: "--session",
                    code: kind.argument_error_code(),
                });
            }
            parsed.args.push(os("--session"));
            parsed.args.push(os(value));
        } else {
            parsed.args.push(arg.clone());
        }
        index += 1;
    }
    Ok(parsed)
}

fn build_remote(
    kind: LauncherKind,
    host: OsString,
    worktree: Option<OsString>,
    use_tmux: bool,
    mut args: Vec<OsString>,
    tail: Vec<OsString>,
    cwd: &Path,
) -> Result<LaunchPlan> {
    let repo_root = find_repo_root(cwd).ok_or_else(|| LauncherError::NotGitRepository {
        launcher: kind.as_str(),
        cwd: cwd.to_owned(),
        code: 1,
    })?;
    let user = env::var_os("USER").unwrap_or_default();
    let remote_repo = remote_path(&repo_root, &user, cfg!(target_os = "macos"))?;
    let workdir = match worktree.as_deref() {
        Some(name) => worktree_path(&remote_repo, name, kind)?,
        None => remote_repo.clone(),
    };

    if use_tmux {
        args.insert(0, os("--tmux"));
    }
    let mut remote_argv = Vec::with_capacity(args.len() + tail.len() + 3);
    remote_argv.extend([os("al"), os(kind.as_str())]);
    remote_argv.extend(args);
    // Reinsert the literal `--` boundary immediately before the protected tail
    // so literal controls (e.g. `--tmux`, `--host`) in the tail are passed to
    // the remote launcher as tool args rather than reinterpreted as controls.
    if !tail.is_empty() {
        remote_argv.push(os("--"));
        remote_argv.extend(tail);
    }
    Ok(LaunchPlan::Remote {
        host,
        repo_root: remote_repo,
        workdir,
        worktree,
        launcher: kind,
        argv: remote_argv,
    })
}

fn build_local(
    kind: LauncherKind,
    args: &[OsString],
    home: &Path,
    cwd: &Path,
    repo_root: Option<&Path>,
) -> Result<LaunchPlan> {
    match kind {
        LauncherKind::Omp => build_omp(args, home),
        LauncherKind::Pi => build_pi(args, home),
        LauncherKind::Grok => build_grok(args, home, repo_root),
        LauncherKind::Hyper => build_hyper(args, home, repo_root),
        LauncherKind::Droid => build_droid(args, home, repo_root),
        LauncherKind::Codex => build_codex(args),
        LauncherKind::Claude => build_claude(args),
    }
    .map(|mut plan| {
        if let LaunchPlan::Command(command) = &mut plan {
            command.cwd = None;
        }
        let _ = cwd;
        plan
    })
}

fn build_omp(args: &[OsString], home: &Path) -> Result<LaunchPlan> {
    let command = if args.is_empty() {
        command("omp", [os("--continue")])
    } else if args[0] == "--session" {
        let id = required_selector(LauncherKind::Omp, args, "--session")?;
        command_with_tail("omp", [os("--resume"), id.clone()], &args[2..])
    } else if args[0] == "--fork" {
        let (session, tail) = optional_selector(args, 1);
        let session = match session {
            Some(session) => session.clone(),
            None => {
                let root = home.join(".omp/agent/sessions");
                latest_jsonl(&root, true)
                    .map(PathBuf::into_os_string)
                    .ok_or(LauncherError::NoSession {
                        launcher: LauncherKind::Omp.as_str(),
                        root,
                        code: 1,
                    })?
            }
        };
        command_with_tail("omp", [os("--fork"), session], tail)
    } else if starts_with_hyphen(&args[0]) {
        command("omp", args.iter().cloned())
    } else {
        command_with_tail("omp", [os("--resume"), args[0].clone()], &args[1..])
    };
    Ok(LaunchPlan::Command(command))
}

fn build_pi(args: &[OsString], home: &Path) -> Result<LaunchPlan> {
    let pi = resolve_launcher_executable("pi")?;
    let command = if args.is_empty() {
        command(pi, [os("--continue")])
    } else if args[0] == "--session" {
        let id = required_selector(LauncherKind::Pi, args, "--session")?;
        command_with_tail(pi, [os("--session"), id.clone()], &args[2..])
    } else if args[0] == "--fork" {
        let (session, tail) = optional_selector(args, 1);
        let session = match session {
            Some(session) => session.clone(),
            None => {
                let root = home.join(".pi/agent/sessions");
                let path = latest_jsonl(&root, true).ok_or_else(|| LauncherError::NoSession {
                    launcher: LauncherKind::Pi.as_str(),
                    root: root.clone(),
                    code: 1,
                })?;
                read_pi_session_id(&path).ok_or(LauncherError::SessionId {
                    launcher: LauncherKind::Pi.as_str(),
                    path,
                    code: 1,
                })?
            }
        };
        command_with_tail(pi, [os("--fork"), session], tail)
    } else if starts_with_hyphen(&args[0]) {
        command(pi, args.iter().cloned())
    } else {
        command_with_tail(pi, [os("--session"), args[0].clone()], &args[1..])
    };
    Ok(LaunchPlan::Command(command))
}

fn build_grok(args: &[OsString], home: &Path, repo_root: Option<&Path>) -> Result<LaunchPlan> {
    let mut command = if args.is_empty() {
        let has_latest = match repo_root {
            Some(repo) => latest_grok_session(home, repo, LauncherKind::Grok)?.is_some(),
            None => false,
        };
        if has_latest {
            command("grok", [os("--continue")])
        } else {
            command("grok", [])
        }
    } else if args[0] == "--session" {
        let id = required_selector(LauncherKind::Grok, args, "--session")?;
        command_with_tail("grok", [os("--resume"), id.clone()], &args[2..])
    } else if args[0] == "--fork" {
        let (session, tail) = optional_selector(args, 1);
        let session = match session {
            Some(session) => session.clone(),
            None => latest_required_grok(home, repo_root, LauncherKind::Grok)?,
        };
        command_with_tail(
            "grok",
            [os("--fork-session"), os("--resume"), session],
            tail,
        )
    } else if starts_with_hyphen(&args[0]) {
        command("grok", args.iter().cloned())
    } else if is_uuid(&args[0]) {
        command_with_tail("grok", [os("--resume"), args[0].clone()], &args[1..])
    } else {
        command("grok", args.iter().cloned())
    };
    apply_grok_environment(&mut command);
    Ok(LaunchPlan::Command(command))
}

fn build_hyper(args: &[OsString], home: &Path, repo_root: Option<&Path>) -> Result<LaunchPlan> {
    let hyper = resolve_hyper_launcher_executable(home)?;
    let mut command = if args.is_empty() {
        let has_latest = match repo_root {
            Some(repo) => latest_grok_session(home, repo, LauncherKind::Hyper)?.is_some(),
            None => false,
        };
        if has_latest {
            command(hyper, [os("--continue")])
        } else {
            command(hyper, [])
        }
    } else if args[0] == "--session" {
        let id = required_selector(LauncherKind::Hyper, args, "--session")?;
        command_with_tail(hyper, [os("--resume"), id.clone()], &args[2..])
    } else if args[0] == "--fork" {
        let explicit_session = args.get(1).filter(|value| is_uuid(value));
        let (session, fork_args) = if let Some(session) = explicit_session {
            (session.clone(), &args[2..])
        } else {
            (
                latest_required_grok(home, repo_root, LauncherKind::Hyper)?,
                &args[1..],
            )
        };
        let (fork_args, worktree, headless) = normalize_hyper_fork_args(fork_args);
        if worktree && headless {
            return Err(LauncherError::HyperWorktreeHeadless);
        }
        if worktree {
            command_with_tail(hyper, [os("--resume"), session], &fork_args)
        } else {
            command_with_tail(
                hyper,
                [os("--fork-session"), os("--resume"), session],
                &fork_args,
            )
        }
    } else if starts_with_hyphen(&args[0]) {
        command(hyper, args.iter().cloned())
    } else if is_uuid(&args[0]) {
        command_with_tail(hyper, [os("--resume"), args[0].clone()], &args[1..])
    } else {
        command(hyper, args.iter().cloned())
    };
    command.env_remove.push(os("NO_COLOR"));
    command.env_set.push((os("COLORTERM"), os("truecolor")));
    Ok(LaunchPlan::Command(command))
}

fn build_droid(args: &[OsString], home: &Path, repo_root: Option<&Path>) -> Result<LaunchPlan> {
    let base = [
        os("--settings"),
        home.join(".factory/settings.json").into_os_string(),
        os("--auto"),
        os("high"),
    ];
    let command = if args.is_empty() {
        command_with_tail("droid", base, &[os("--resume")])
    } else if args[0] == "--resume" {
        let id = required_selector(LauncherKind::Droid, args, "--resume")?;
        let mut prefix = base.to_vec();
        prefix.extend([os("--resume"), id.clone()]);
        command_with_tail("droid", prefix, &args[2..])
    } else if args[0] == "--fork" {
        let (session, tail) = optional_selector(args, 1);
        let session = match session {
            Some(session) => session.clone(),
            None => latest_required_droid(home, repo_root)?,
        };
        let mut prefix = base.to_vec();
        prefix.extend([os("--fork"), session]);
        command_with_tail("droid", prefix, tail)
    } else {
        let mut prefix = base.to_vec();
        prefix.push(os("--resume"));
        command_with_tail("droid", prefix, args)
    };
    Ok(LaunchPlan::Command(command))
}

fn build_codex(args: &[OsString]) -> Result<LaunchPlan> {
    let base = [
        os("-c"),
        os("check_for_update_on_startup=false"),
        os("--ask-for-approval"),
        os("never"),
        os("--sandbox"),
        os("danger-full-access"),
    ];
    let command = if args.is_empty() {
        let mut prefix = base.to_vec();
        prefix.extend([os("resume"), os("--last")]);
        command("codex", prefix)
    } else if args[0] == "--session" {
        let id = required_selector(LauncherKind::Codex, args, "--session")?;
        let mut prefix = base.to_vec();
        prefix.extend([os("resume"), id.clone()]);
        command_with_tail("codex", prefix, &args[2..])
    } else if args[0] == "--fork" {
        let (session, tail) = optional_selector(args, 1);
        let mut prefix = base.to_vec();
        prefix.push(os("fork"));
        prefix.push(session.cloned().unwrap_or_else(|| os("--last")));
        command_with_tail("codex", prefix, tail)
    } else if args[0] == "fork" || args[0] == "resume" {
        command_with_tail("codex", base, args)
    } else {
        let mut prefix = base.to_vec();
        prefix.push(os("resume"));
        command_with_tail("codex", prefix, args)
    };
    Ok(LaunchPlan::Command(command))
}

fn build_claude(args: &[OsString]) -> Result<LaunchPlan> {
    let base = [os("--dangerously-skip-permissions")];
    if args.is_empty() {
        let primary = command_with_tail("claude", base.clone(), &[os("-c")]);
        let fallback = command("claude", base);
        return Ok(LaunchPlan::Fallback { primary, fallback });
    }

    let command = if args[0] == "--session" || args[0] == "--resume" {
        let option = if args[0] == "--session" {
            "--session"
        } else {
            "--resume"
        };
        let id = required_selector(LauncherKind::Claude, args, option)?;
        command_with_tail(
            "claude",
            [base[0].clone(), os("--resume"), id.clone()],
            &args[2..],
        )
    } else if args[0] == "--fork" || args[0] == "fork" {
        let (session, tail) = optional_selector(args, 1);
        let continuation = match session {
            Some(session) => vec![os("--resume"), session.clone()],
            None => vec![os("--continue")],
        };
        let mut prefix = base.to_vec();
        prefix.push(os("--fork-session"));
        prefix.extend(continuation);
        command_with_tail("claude", prefix, tail)
    } else {
        let primary = command_with_tail("claude", base.clone(), &[os("-c")]);
        let mut fallback_prefix = base.to_vec();
        fallback_prefix.extend_from_slice(args);
        let fallback = command("claude", fallback_prefix);
        return Ok(LaunchPlan::Fallback { primary, fallback });
    };
    Ok(LaunchPlan::Command(command))
}

fn required_selector<'a>(
    kind: LauncherKind,
    args: &'a [OsString],
    option: &'static str,
) -> Result<&'a OsString> {
    let Some(value) = args.get(1) else {
        return Err(LauncherError::MissingValue {
            launcher: kind.as_str(),
            option,
            code: kind.argument_error_code(),
        });
    };
    if value.is_empty() || starts_with_hyphen(value) {
        return Err(LauncherError::OptionValue {
            launcher: kind.as_str(),
            option,
            code: kind.argument_error_code(),
        });
    }
    Ok(value)
}

fn optional_selector(args: &[OsString], index: usize) -> (Option<&OsString>, &[OsString]) {
    match args.get(index) {
        Some(value) if !value.is_empty() && !starts_with_hyphen(value) => {
            (Some(value), &args[index + 1..])
        }
        _ => (None, &args[index..]),
    }
}

fn apply_grok_environment(command: &mut CommandSpec) {
    command.env_remove.push(os("NO_COLOR"));
    command.env_set.extend([
        (os("TERM"), os("tmux-256color")),
        (os("COLORTERM"), os("truecolor")),
    ]);
}

fn normalize_hyper_fork_args(args: &[OsString]) -> (Vec<OsString>, bool, bool) {
    let mut normalized = Vec::with_capacity(args.len());
    let mut worktree = false;
    let mut headless = false;
    for arg in args {
        if arg == "-w" || arg == "--worktree" {
            worktree = true;
            normalized.push(os("--worktree="));
        } else if utf8_option_value(arg, "--worktree=").is_some() {
            worktree = true;
            normalized.push(arg.clone());
        } else {
            if arg == "-p"
                || arg == "--single"
                || arg == "--prompt-file"
                || arg == "--prompt-json"
            {
                headless = true;
            }
            normalized.push(arg.clone());
        }
    }
    (normalized, worktree, headless)
}

fn latest_required_grok(
    home: &Path,
    repo_root: Option<&Path>,
    kind: LauncherKind,
) -> Result<OsString> {
    let repo_root = repo_root.ok_or_else(|| LauncherError::NotGitRepository {
        launcher: kind.as_str(),
        cwd: PathBuf::from("."),
        code: 1,
    })?;
    let session_root = grok_session_root(home, repo_root, kind)?;
    latest_grok_session(home, repo_root, kind)?.ok_or(LauncherError::NoSession {
        launcher: kind.as_str(),
        root: session_root,
        code: 1,
    })
}

fn latest_grok_session(
    home: &Path,
    repo_root: &Path,
    kind: LauncherKind,
) -> Result<Option<OsString>> {
    let root = grok_session_root(home, repo_root, kind)?;
    let mut latest: Option<(SystemTime, OsString)> = None;
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !is_uuid(entry.file_name().as_os_str()) {
            continue;
        }
        let summary = path.join("summary.json");
        let Ok(modified) = summary.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
            latest = Some((modified, entry.file_name()));
        }
    }
    Ok(latest.map(|(_, id)| id))
}

fn grok_session_root(home: &Path, repo_root: &Path, kind: LauncherKind) -> Result<PathBuf> {
    let text = repo_root.to_str().ok_or_else(|| LauncherError::NonUtf8Path {
        launcher: kind.as_str(),
        path: repo_root.to_owned(),
        code: 1,
    })?;
    let base = if kind == LauncherKind::Hyper {
        env::var_os("GROK_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".grok"))
    } else {
        home.join(".grok")
    };
    Ok(base
        .join("sessions")
        .join(utf8_percent_encode(text, URL_PATH_ENCODE_SET).to_string()))
}

fn latest_required_droid(home: &Path, repo_root: Option<&Path>) -> Result<OsString> {
    let repo_root = repo_root.ok_or_else(|| LauncherError::NotGitRepository {
        launcher: LauncherKind::Droid.as_str(),
        cwd: PathBuf::from("."),
        code: 1,
    })?;
    let text = repo_root.to_str().ok_or_else(|| LauncherError::NonUtf8Path {
        launcher: LauncherKind::Droid.as_str(),
        path: repo_root.to_owned(),
        code: 1,
    })?;
    let root = home
        .join(".factory/sessions")
        .join(text.replace('/', "-"));
    let mut latest: Option<(SystemTime, OsString)> = None;
    let Ok(entries) = fs::read_dir(&root) else {
        return Err(LauncherError::NoSession {
            launcher: LauncherKind::Droid.as_str(),
            root,
            code: 1,
        });
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("jsonl")) {
            continue;
        }
        let Some(stem) = path.file_stem() else {
            continue;
        };
        if !is_uuid(stem) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
            latest = Some((modified, stem.to_owned()));
        }
    }
    latest
        .map(|(_, id)| id)
        .ok_or(LauncherError::NoSession {
            launcher: LauncherKind::Droid.as_str(),
            root,
            code: 1,
        })
}

fn latest_jsonl(root: &Path, recursive: bool) -> Option<PathBuf> {
    let mut latest: Option<(SystemTime, PathBuf)> = None;
    let walker = if recursive {
        WalkDir::new(root)
    } else {
        WalkDir::new(root).max_depth(1)
    };
    for entry in walker.into_iter().filter_map(std::result::Result::ok) {
        if !entry.file_type().is_file() || entry.path().extension() != Some(OsStr::new("jsonl")) {
            continue;
        }
        let Some(modified) = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
        else {
            continue;
        };
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
            latest = Some((modified, entry.into_path()));
        }
    }
    latest.map(|(_, path)| path)
}

fn read_pi_session_id(path: &Path) -> Option<OsString> {
    let file = File::open(path).ok()?;
    for line in BufReader::new(file).lines().map_while(std::result::Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        if object.get("type").is_some_and(|kind| kind != "session") {
            continue;
        }
        if let Some(id) = object.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
            return Some(os(id));
        }
    }
    None
}

fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    let start = fs::canonicalize(cwd).ok()?;
    for ancestor in start.ancestors() {
        if fs::symlink_metadata(ancestor.join(".git")).is_ok() {
            return Some(ancestor.to_owned());
        }
    }
    None
}

fn worktree_path(repo: &Path, name: &OsStr, kind: LauncherKind) -> Result<PathBuf> {
    let parent = repo.parent().ok_or_else(|| LauncherError::NotGitRepository {
        launcher: kind.as_str(),
        cwd: repo.to_owned(),
        code: 1,
    })?;
    let repo_name = repo.file_name().ok_or_else(|| LauncherError::NotGitRepository {
        launcher: kind.as_str(),
        cwd: repo.to_owned(),
        code: 1,
    })?;
    let mut worktree_name = repo_name.to_owned();
    worktree_name.push("-");
    worktree_name.push(name);
    Ok(parent.join(worktree_name))
}

fn preferred_hyper_executable(home: &Path) -> OsString {
    preferred_hyper_executable_in(home, env::var_os("HYPER_HOME").as_deref())
}

fn preferred_hyper_executable_in(home: &Path, hyper_home: Option<&OsStr>) -> OsString {
    let hyper_home = hyper_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".hyper"));
    let managed = hyper_home.join("bin/hyper");
    if managed.is_file() {
        managed.into_os_string()
    } else {
        os("hyper")
    }
}

fn resolve_hyper_launcher_executable(home: &Path) -> Result<OsString> {
    let preferred = preferred_hyper_executable(home);
    resolve_executable(&preferred)
        .map(PathBuf::into_os_string)
        .ok_or(LauncherError::MissingExecutable {
            executable: preferred,
        })
}

fn resolve_launcher_executable(program: &str) -> Result<OsString> {
    resolve_executable(OsStr::new(program))
        .map(PathBuf::into_os_string)
        .ok_or_else(|| LauncherError::MissingExecutable {
            executable: os(program),
        })
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_uuid(value: &OsStr) -> bool {
    let Some(value) = value.to_str() else {
        return false;
    };
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

fn starts_with_hyphen(value: &OsStr) -> bool {
    value.as_encoded_bytes().first() == Some(&b'-')
}

fn utf8_option_value<'a>(value: &'a OsStr, prefix: &str) -> Option<&'a str> {
    value.to_str()?.strip_prefix(prefix)
}

/// Execute one structured local command without involving a shell.
pub fn execute(spec: &CommandSpec) -> io::Result<ExitStatus> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &spec.env_set {
        command.env(name, value);
    }
    command.status()
}

/// Render a command for display only. Execution always uses `CommandSpec`
/// directly, so these quotes are never parsed on the local launch path.
pub fn render_command(spec: &CommandSpec) -> Result<String> {
    let display_path = spec.cwd.as_deref().unwrap_or_else(|| Path::new("."));
    let mut values = Vec::new();
    if !spec.env_remove.is_empty() || !spec.env_set.is_empty() {
        values.push(os("env"));
        for name in &spec.env_remove {
            values.extend([os("-u"), name.clone()]);
        }
        for (name, value) in &spec.env_set {
            let mut assignment = name.clone();
            assignment.push("=");
            assignment.push(value);
            values.push(assignment);
        }
    }
    values.push(spec.program.clone());
    values.extend_from_slice(&spec.args);
    values
        .iter()
        .map(|value| posix_quote(value, "command", display_path))
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join(" "))
}

/// Execute a constructed plan and return the exact child exit code. A child
/// exiting unsuccessfully is normal process status, not an execution error.
pub fn execute_plan(plan: &LaunchPlan) -> Result<i32> {
    match plan {
        LaunchPlan::Command(spec) => execute_code(spec),
        LaunchPlan::Fallback { primary, fallback } => {
            let status = execute_code_with_stderr(primary, Stdio::null())?;
            if status == 0 {
                Ok(0)
            } else {
                execute_code(fallback)
            }
        }
        LaunchPlan::Tmux {
            session,
            command,
            fallback,
        } => execute_tmux(session, command, fallback.as_ref()),
        LaunchPlan::Remote {
            host,
            repo_root,
            workdir,
            worktree,
            launcher,
            argv,
        } => execute_remote(host, repo_root, workdir, worktree.as_deref(), *launcher, argv),
    }
}

fn execute_code(spec: &CommandSpec) -> Result<i32> {
    let status = execute(spec).map_err(|source| LauncherError::Execute {
        program: spec.program.clone(),
        source,
    })?;
    Ok(exit_status_code(status))
}

fn execute_code_with_stderr(spec: &CommandSpec, stderr: Stdio) -> Result<i32> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args).stderr(stderr);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &spec.env_set {
        command.env(name, value);
    }
    let status = command.status().map_err(|source| LauncherError::Execute {
        program: spec.program.clone(),
        source,
    })?;
    Ok(exit_status_code(status))
}

fn execute_tmux(
    session: &str,
    spec: &CommandSpec,
    fallback: Option<&CommandSpec>,
) -> Result<i32> {
    crate::tmux::run_exact_fallback(session, spec, fallback).map_err(|error| {
        LauncherError::Execute {
            program: os("al tmux-run"),
            source: io::Error::other(error),
        }
    })
}

fn execute_remote(
    host: &OsStr,
    repo_root: &Path,
    workdir: &Path,
    worktree: Option<&OsStr>,
    launcher: LauncherKind,
    argv: &[OsString],
) -> Result<i32> {
    if let Some(worktree) = worktree {
        execute_remote_worktree(host, repo_root, workdir, worktree, launcher)?;
    }

    let remote = remote_command_string(workdir, argv, true)?;
    let ssh = CommandSpec::new(
        "ssh",
        vec![
            os("-tt"),
            os("-o"),
            os("ConnectTimeout=10"),
            os("-o"),
            os("ConnectionAttempts=1"),
            os("--"),
            host.to_owned(),
            os(remote),
        ],
    );
    execute_code(&ssh)
}

fn execute_remote_worktree(
    host: &OsStr,
    repo_root: &Path,
    workdir: &Path,
    worktree: &OsStr,
    launcher: LauncherKind,
) -> Result<()> {
    validate_worktree_name(worktree)?;
    let repo = posix_quote(repo_root.as_os_str(), launcher.as_str(), repo_root)?;
    let path = posix_quote(workdir.as_os_str(), launcher.as_str(), workdir)?;
    let workdir_text = workdir.to_str().ok_or_else(|| LauncherError::NonUtf8Path {
        launcher: launcher.as_str(),
        path: workdir.to_owned(),
        code: 1,
    })?;
    let listing_value = OsString::from(format!("worktree {workdir_text}"));
    let listing = posix_quote(&listing_value, launcher.as_str(), workdir)?;
    let command = format!(
        "cd {repo}; and begin; contains -- {listing} (git worktree list --porcelain 2>/dev/null); or git worktree add --detach -- {path}; end; and contains -- {listing} (git worktree list --porcelain 2>/dev/null)"
    );
    let remote = format!(
        "fish -c {}",
        posix_quote(OsStr::new(&command), launcher.as_str(), repo_root)?
    );
    let ssh = CommandSpec::new(
        "ssh",
        vec![
            os("-o"),
            os("ConnectTimeout=10"),
            os("-o"),
            os("ConnectionAttempts=1"),
            os("--"),
            host.to_owned(),
            os(remote),
        ],
    );
    let status = execute_code(&ssh)?;
    if status == 0 {
        Ok(())
    } else {
        Err(LauncherError::Execute {
            program: os("ssh"),
            source: io::Error::other(format!(
                "{}: failed to create or verify worktree on remote host (exit {status})",
                launcher.as_str()
            )),
        })
    }
}


fn remote_command_string(workdir: &Path, argv: &[OsString], interactive_fish: bool) -> Result<String> {
    let mut agent = String::from("cd ");
    agent.push_str(&posix_quote(workdir.as_os_str(), "remote", workdir)?);
    agent.push_str("; and");
    for arg in argv {
        agent.push(' ');
        agent.push_str(&posix_quote(arg, "remote", workdir)?);
    }
    if interactive_fish {
        agent.push_str("; set -l __al_status $status; if test $__al_status -eq 0; exec fish -li; else exit $__al_status; end");
    }
    Ok(format!("fish -lic {}", posix_quote(OsStr::new(&agent), "remote", workdir)?))
}

fn posix_quote(value: &OsStr, launcher: &'static str, path: &Path) -> Result<String> {
    let value = value.to_str().ok_or_else(|| LauncherError::NonUtf8Path {
        launcher,
        path: path.to_owned(),
        code: 1,
    })?;
    if value.is_empty() {
        return Ok("''".to_owned());
    }
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
    Ok(quoted)
}

fn exit_status_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(1)
    }
    #[cfg(not(unix))]
    {
        1
    }
}


fn command(
    program: impl Into<OsString>,
    args: impl IntoIterator<Item = OsString>,
) -> CommandSpec {
    CommandSpec::new(program, args.into_iter().collect())
}

fn command_with_tail(
    program: impl Into<OsString>,
    prefix: impl IntoIterator<Item = OsString>,
    tail: &[OsString],
) -> CommandSpec {
    let mut args: Vec<OsString> = prefix.into_iter().collect();
    args.extend_from_slice(tail);
    CommandSpec::new(program, args)
}

fn os(value: impl AsRef<OsStr>) -> OsString {
    value.as_ref().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(os).collect()
    }

    fn assert_command(plan: LaunchPlan) -> CommandSpec {
        match plan {
            LaunchPlan::Command(command) => command,
            other => panic!("expected command, got {other:?}"),
        }
    }

    fn test_home() -> PathBuf {
        Path::new("/").join("workspace").join("user")
    }

    #[test]
    fn native_resume_uses_paths_only_for_pi_and_omp() {
        let home = test_home();
        let home = home.as_path();
        let path = Path::new("/tmp/a session.jsonl");
        assert_eq!(
            native_resume(TargetTool::Pi, path, "sid", home),
            command("pi", strings(&["--session", "/tmp/a session.jsonl"]))
        );
        assert_eq!(
            native_resume(TargetTool::Omp, path, "sid", home),
            command("omp", strings(&["--resume", "/tmp/a session.jsonl"]))
        );
        assert_eq!(
            native_resume(TargetTool::Codex, path, "sid", home),
            command("codex", strings(&["resume", "sid"]))
        );
        assert_eq!(
            native_resume(TargetTool::Claude, path, "sid", home),
            command("claude", strings(&["--resume", "sid"]))
        );
    }

    #[test]
    fn native_fork_has_exact_multi_flag_order() {
        let home = test_home();
        let home = home.as_path();
        let path = Path::new("/tmp/session.jsonl");
        for target in [TargetTool::Claude, TargetTool::Grok] {
            let executable = target.as_str();
            assert_eq!(
                native_fork(target, path, "sid", home),
                command(
                    executable,
                    strings(&["--fork-session", "--resume", "sid"]),
                )
            );
        }
        assert_eq!(
            native_fork(TargetTool::Pi, path, "ignored", home),
            command("pi", strings(&["--fork", "/tmp/session.jsonl"]))
        );
    }

    #[test]
    fn native_hyper_prefers_managed_binary() {
        let temp = TempDir::new().unwrap();
        let binary = temp.path().join(".hyper/bin/hyper");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, "").unwrap();
        assert_eq!(
            preferred_hyper_executable_in(temp.path(), None),
            binary.into_os_string()
        );
    }

    #[test]
    fn local_omp_arguments_are_not_shell_quoted_or_reparsed() {
        let argument = OsString::from("id with spaces; $(not-a-command) 'quoted'");
        let plan = build_launcher(
            LauncherKind::Omp,
            &[os("--session"), argument.clone(), os("prompt with spaces")],
            &test_home(),
            Path::new("/tmp"),
        )
        .unwrap();
        let command = assert_command(plan);
        assert_eq!(command.program, "omp");
        assert_eq!(
            command.args,
            vec![os("--resume"), argument, os("prompt with spaces")]
        );
    }

    #[test]
    fn grok_environment_prefix_is_structured() {
        let command = assert_command(
            build_launcher(
                LauncherKind::Grok,
                &[os("hello")],
                &test_home(),
                Path::new("/tmp"),
            )
            .unwrap(),
        );
        assert_eq!(command.program, "grok");
        assert_eq!(command.args, strings(&["hello"]));
        assert_eq!(command.env_remove, strings(&["NO_COLOR"]));
        assert_eq!(
            command.env_set,
            vec![
                (os("TERM"), os("tmux-256color")),
                (os("COLORTERM"), os("truecolor")),
            ]
        );
    }

    #[test]
    fn codex_has_exact_default_flags() {
        let command = assert_command(
            build_launcher(
                LauncherKind::Codex,
                &[],
                &test_home(),
                Path::new("/tmp"),
            )
            .unwrap(),
        );
        assert_eq!(
            command.args,
            strings(&[
                "-c",
                "check_for_update_on_startup=false",
                "--ask-for-approval",
                "never",
                "--sandbox",
                "danger-full-access",
                "resume",
                "--last",
            ])
        );
    }

    #[test]
    fn droid_expands_settings_under_supplied_home() {
        let home = test_home();
        let settings = home.join(".factory").join("settings.json");
        let command = assert_command(
            build_launcher(
                LauncherKind::Droid,
                &[],
                &home,
                Path::new("/tmp"),
            )
            .unwrap(),
        );
        assert_eq!(
            command.args,
            vec![
                os("--settings"),
                settings.into_os_string(),
                os("--auto"),
                os("high"),
                os("--resume"),
            ]
        );
    }

    #[test]
    fn cclo_default_is_continue_then_permission_default_fallback() {
        let plan = build_launcher(
            LauncherKind::Claude,
            &[],
            &test_home(),
            Path::new("/tmp"),
        )
        .unwrap();
        let LaunchPlan::Fallback { primary, fallback } = plan else {
            panic!("expected fallback plan");
        };
        assert_eq!(
            primary.args,
            strings(&["--dangerously-skip-permissions", "-c"])
        );
        assert_eq!(
            fallback.args,
            strings(&["--dangerously-skip-permissions"])
        );
    }

    #[test]
    fn cclo_resume_and_fork_are_native_and_dangerous() {
        let resume = assert_command(
            build_launcher(
                LauncherKind::Claude,
                &strings(&["--session", "sid with spaces"]),
                &test_home(),
                Path::new("/tmp"),
            )
            .unwrap(),
        );
        assert_eq!(
            resume.args,
            strings(&[
                "--dangerously-skip-permissions",
                "--resume",
                "sid with spaces",
            ])
        );
        let fork = assert_command(
            build_launcher(
                LauncherKind::Claude,
                &strings(&["--fork", "sid"]),
                &test_home(),
                Path::new("/tmp"),
            )
            .unwrap(),
        );
        assert_eq!(
            fork.args,
            strings(&[
                "--dangerously-skip-permissions",
                "--fork-session",
                "--resume",
                "sid",
            ])
        );
    }

    #[test]
    fn cclo_generic_args_are_continue_fallback_with_args() {
        let plan = build_launcher(
            LauncherKind::Claude,
            &strings(&["--verbose", "fix the bug"]),
            &test_home(),
            Path::new("/tmp"),
        )
        .unwrap();
        let LaunchPlan::Fallback { primary, fallback } = plan else {
            panic!("expected fallback plan");
        };
        assert_eq!(
            primary.args,
            strings(&["--dangerously-skip-permissions", "-c"])
        );
        assert_eq!(
            fallback.args,
            strings(&["--dangerously-skip-permissions", "--verbose", "fix the bug"])
        );
    }

    #[test]
    fn cclo_tmux_preserves_both_primary_and_fallback() {
        let plan = build_launcher(
            LauncherKind::Claude,
            &strings(&["--tmux", "fix the bug"]),
            &test_home(),
            Path::new("/tmp"),
        )
        .unwrap();
        let LaunchPlan::Tmux {
            command,
            fallback,
            ..
        } = plan
        else {
            panic!("expected tmux plan");
        };
        assert_eq!(
            command.args,
            strings(&["--dangerously-skip-permissions", "-c"])
        );
        let fallback = fallback.expect("fallback preserved under tmux");
        assert_eq!(
            fallback.args,
            strings(&["--dangerously-skip-permissions", "fix the bug"])
        );
    }

    #[test]
    fn dash_dash_stops_option_parsing_for_every_launcher() {
        for kind in [
            LauncherKind::Omp,
            LauncherKind::Pi,
            LauncherKind::Grok,
            LauncherKind::Droid,
            LauncherKind::Codex,
            LauncherKind::Claude,
        ] {
            let plan = build_launcher(
                kind,
                &strings(&["--", "--host", "evil", "--wt", "x", "--tmux"]),
                &test_home(),
                Path::new("/tmp"),
            )
            .unwrap();
            let command = match plan {
                LaunchPlan::Command(command) => command,
                LaunchPlan::Fallback { fallback, .. } => fallback,
                _ => panic!("expected local command plan"),
            };
            assert!(
                command.args.iter().any(|arg| arg == "--host"),
                "{kind:?}: --host preserved after --"
            );
            assert!(
                command.args.iter().any(|arg| arg == "--tmux"),
                "{kind:?}: --tmux preserved after --"
            );
        }
    }

    #[test]
    fn equals_session_form_normalizes_for_every_launcher() {
        for kind in [
            LauncherKind::Omp,
            LauncherKind::Pi,
            LauncherKind::Grok,
            LauncherKind::Hyper,
            LauncherKind::Droid,
            LauncherKind::Codex,
            LauncherKind::Claude,
        ] {
            let plan = build_launcher(
                kind,
                &strings(&["--session=my-id"]),
                &test_home(),
                Path::new("/tmp"),
            )
            .unwrap();
            let command = match plan {
                LaunchPlan::Command(command) => command,
                LaunchPlan::Fallback { primary, .. } => primary,
                _ => panic!("expected local command plan"),
            };
            assert!(
                command.args.iter().any(|arg| arg == "my-id"),
                "{kind:?}: --session=value passes value through"
            );
            assert!(
                !command.args.iter().any(|arg| arg == "--session=my-id"),
                "{kind:?}: --session=value not left as literal equals form"
            );
        }
    }

    #[test]
    fn dash_host_with_empty_equals_form_errors_uniformly() {
        for kind in [
            LauncherKind::Omp,
            LauncherKind::Pi,
            LauncherKind::Grok,
            LauncherKind::Droid,
            LauncherKind::Codex,
            LauncherKind::Claude,
        ] {
            let error = build_launcher(
                kind,
                &[os("--host=")],
                &test_home(),
                Path::new("/tmp"),
            )
            .unwrap_err();
            assert_eq!(error.exit_code(), 2, "{kind:?}: empty --host= exits 2");
        }
    }

    #[test]
    fn cclo_fork_without_id_forks_continue() {
        let command = assert_command(
            build_launcher(
                LauncherKind::Claude,
                &[os("--fork")],
                &test_home(),
                Path::new("/tmp"),
            )
            .unwrap(),
        );
        assert_eq!(
            command.args,
            strings(&[
                "--dangerously-skip-permissions",
                "--fork-session",
                "--continue",
            ])
        );
    }

    #[test]
    fn tmux_session_name_matches_fish_sanitization() {
        assert_eq!(
            tmux_session_name("omlo", Some(Path::new("/work/--my.project:one")), Path::new("/")),
            "omlo-my_project_one"
        );
        assert_eq!(tmux_session_name("pilo", None, Path::new("/")), "pilo-root");
    }

    #[test]
    fn rendered_command_preserves_environment_and_single_arguments() {
        let mut spec = command("tool path", strings(&["one arg", "a'b"]));
        spec.env_remove.push(os("NO_COLOR"));
        spec.env_set.push((os("TERM"), os("tmux-256color")));
        assert_eq!(
            render_command(&spec).unwrap(),
            "'env' '-u' 'NO_COLOR' 'TERM=tmux-256color' 'tool path' 'one arg' 'a'\\''b'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn execute_plan_propagates_nonzero_child_status() {
        let plan = LaunchPlan::Command(command("/bin/sh", strings(&["-c", "exit 23"])));
        assert_eq!(execute_plan(&plan).unwrap(), 23);
    }

    #[cfg(unix)]
    #[test]
    fn fallback_primary_success_skips_fallback_and_returns_zero() {
        let temp = TempDir::new().unwrap();
        let sentinel = temp.path().join("ran");
        let fallback_script = format!("touch {} 2>/dev/null; exit 7", sentinel.display());
        let primary = command("/bin/sh", strings(&["-c", "exit 0"]));
        let fallback = command("/bin/sh", strings(&["-c", &fallback_script]));
        let plan = LaunchPlan::Fallback { primary, fallback };
        assert_eq!(execute_plan(&plan).unwrap(), 0);
        assert!(
            !sentinel.exists(),
            "fallback executed despite primary reporting success"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fallback_primary_nonzero_runs_fallback_and_returns_fallback_status() {
        let temp = TempDir::new().unwrap();
        let sentinel = temp.path().join("ran");
        let fallback_script = format!("touch {} 2>/dev/null; exit 42", sentinel.display());
        let primary = command("/bin/sh", strings(&["-c", "exit 3"]));
        let fallback = command("/bin/sh", strings(&["-c", &fallback_script]));
        let plan = LaunchPlan::Fallback { primary, fallback };
        assert_eq!(
            execute_plan(&plan).unwrap(),
            42,
            "fallback status must replace the nonzero primary status"
        );
        assert!(
            sentinel.exists(),
            "fallback did not run after primary reported a nonzero status"
        );
    }

    #[test]
    fn remote_interactive_fish_quotes_exact_argv_and_branches_on_status() {
        let command = remote_command_string(
            Path::new("/work/a repo"),
            &strings(&["al", "omlo", "--session", "$(rm -rf ~); echo pwned"]),
            true,
        )
        .unwrap();
        assert_eq!(
            command,
            r#"fish -lic 'cd '\''/work/a repo'\''; and '\''al'\'' '\''omlo'\'' '\''--session'\'' '\''$(rm -rf ~); echo pwned'\''; set -l __al_status $status; if test $__al_status -eq 0; exec fish -li; else exit $__al_status; end'"#
        );
        // The malicious shell metacharacters must survive literally inside
        // single quotes rather than being interpolated by the remote fish.
        assert!(command.contains("'$(rm -rf ~); echo pwned'"));
        // Captured status drives the branch: success hands off to an
        // interactive login fish, failure re-exits with the captured status.
        assert!(command.contains("set -l __al_status $status"));
        assert!(command.contains("exec fish -li"));
        assert!(command.contains("else exit $__al_status; end"));
    }

    #[test]
    fn posix_quote_empty_returns_closed_single_quotes() {
        let path = Path::new("/work/repo");
        assert_eq!(posix_quote(OsStr::new(""), "remote", path).unwrap(), "''");
    }

    #[cfg(unix)]
    #[test]
    fn posix_quote_non_utf8_fails_closed() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new("/work/repo");
        let invalid = OsStr::from_bytes(b"/work/\xff\xfe");
        let error = posix_quote(invalid, "remote", path).unwrap_err();
        match error {
            LauncherError::NonUtf8Path {
                launcher,
                code,
                path: reported,
            } => {
                assert_eq!(launcher, "remote");
                assert_eq!(code, 1);
                assert_eq!(reported, PathBuf::from("/work/repo"));
            }
            other => panic!("expected NonUtf8Path, got {other:?}"),
        }
    }

    #[test]
    fn remote_command_targets_al_and_quotes_exact_argv() {
        let command = remote_command_string(
            Path::new("/work/a repo"),
            &strings(&["al", "omlo", "--session", "id; echo nope"]),
            false,
        )
        .unwrap();
        assert!(command.contains("al"));
        assert!(command.contains("omlo"));
        assert!(command.contains("id; echo nope"));
        assert!(!command.contains("exec omlo"));
    }

    #[test]
    fn darwin_home_mapping_observes_component_boundaries() {
        let user = OsStr::new("alice");
        let root = Path::new("/");
        assert_eq!(
            darwin_remote_path(
                &root.join("Users").join(user).join("Projects").join("x"),
                user,
            ),
            root.join("home").join(user).join("Projects").join("x"),
        );
        assert_eq!(
            darwin_remote_path(
                &root.join("Users").join("alice2").join("Projects").join("x"),
                user,
            ),
            root.join("Users").join("alice2").join("Projects").join("x"),
        );
    }

    #[test]
    fn configured_darwin_mapping_is_component_aware() {
        let mappings = parse_remote_path_maps(Some(os(
            r#"[["/Volumes/workspace","/srv/workspace"]]"#,
        )))
        .unwrap();
        let user = OsStr::new("alice");
        assert_eq!(
            darwin_remote_path_with_mappings(
                Path::new("/Volumes/workspace/project"),
                user,
                &mappings,
            ),
            PathBuf::from("/srv/workspace/project"),
        );
        assert_eq!(
            darwin_remote_path_with_mappings(
                Path::new("/Volumes/workspace-backup/project"),
                user,
                &mappings,
            ),
            PathBuf::from("/Volumes/workspace-backup/project"),
        );
    }

    #[test]
    fn remote_path_maps_reject_malformed_json() {
        let error = parse_remote_path_maps(Some(os("not-json"))).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("invalid AL_REMOTE_PATH_MAPS JSON"));
    }

    #[test]
    fn remote_path_maps_reject_relative_paths() {
        for value in [
            r#"[["Volumes/workspace","/srv/workspace"]]"#,
            r#"[["/Volumes/workspace","srv/workspace"]]"#,
        ] {
            let error = parse_remote_path_maps(Some(os(value))).unwrap_err();
            assert_eq!(error.exit_code(), 2);
            assert!(error.to_string().contains("path must be absolute"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn remote_path_maps_reject_non_utf8_configuration() {
        use std::os::unix::ffi::OsStringExt;

        let error = parse_remote_path_maps(Some(OsString::from_vec(vec![0xff]))).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert_eq!(error.to_string(), "AL_REMOTE_PATH_MAPS is not valid UTF-8");
    }

    #[test]
    fn worktree_validation_rejects_unsafe_names() {
        for name in ["", "-x", ".", "..", "a/b"] {
            let error = validate_worktree_name(OsStr::new(name)).unwrap_err();
            assert_eq!(error.exit_code(), 2);
        }
        validate_worktree_name(OsStr::new("feature.one" )).unwrap();
    }

    #[test]
    fn recorded_cwd_is_canonical_only_when_it_exists() {
        let temp = TempDir::new().unwrap();
        let directory = temp.path().join("project");
        fs::create_dir(&directory).unwrap();
        assert_eq!(local_recorded_cwd(&directory), Some(directory.canonicalize().unwrap()));
        assert_eq!(local_recorded_cwd(&temp.path().join("missing")), None);
        assert_eq!(local_recorded_cwd(Path::new("")), None);
    }

    #[test]
    fn remote_plan_keeps_exact_argv_without_a_shell_command() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let plan = build_launcher(
            LauncherKind::Omp,
            &strings(&[
                "--host",
                "host-b",
                "--wt",
                "feature",
                "--tmux",
                "--session",
                "id with spaces",
            ]),
            &test_home(),
            temp.path(),
        )
        .unwrap();
        let LaunchPlan::Remote {
            host,
            repo_root,
            workdir,
            worktree,
            launcher,
            argv,
        } = plan
        else {
            panic!("expected remote plan");
        };
        assert_eq!(host, "host-b");
        assert_eq!(
            workdir,
            temp.path()
                .parent()
                .unwrap()
                .join(format!("{}-feature", temp.path().file_name().unwrap().to_string_lossy()))
        );
        assert_eq!(
            argv,
            strings(&["al", "omlo", "--tmux", "--session", "id with spaces"])
        );
        assert_eq!(repo_root, temp.path().canonicalize().unwrap());
        assert_eq!(worktree, Some(os("feature")));
        assert_eq!(launcher, LauncherKind::Omp);
    }

    #[test]
    fn typed_argument_errors_retain_shell_exit_codes() {
        let error = build_launcher(
            LauncherKind::Grok,
            &[os("--session")],
            &test_home(),
            Path::new("/tmp"),
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 2);

        let error = build_launcher(
            LauncherKind::Omp,
            &strings(&["--wt", "feature"]),
            &test_home(),
            Path::new("/tmp"),
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn remote_inserts_literal_delimiter_before_protected_tail() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();

        // `--tmux` after the boundary is a protected tool arg, not a control:
        // the remote argv keeps `--` before it and no leading `--tmux` control
        // is added. The plan stays Remote (not Tmux).
        let plan = build_launcher(
            LauncherKind::Omp,
            &strings(&["--host", "host-a", "--", "--tmux"]),
            &test_home(),
            temp.path(),
        )
        .unwrap();
        let LaunchPlan::Remote { host, argv, .. } = plan else {
            panic!("expected remote plan, got {plan:?}");
        };
        assert_eq!(host, "host-a");
        assert_eq!(argv, strings(&["al", "omlo", "--", "--tmux"]));
        // The literal control was not reinterpreted as a launcher control.
        assert_ne!(argv.get(2), Some(&os("--tmux")));
    }

    #[test]
    fn remote_protected_tail_does_not_override_host() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();

        // `--host host-shadow` after the boundary is protected and must not replace host-a.
        let plan = build_launcher(
            LauncherKind::Omp,
            &strings(&["--host", "host-a", "--", "--host", "host-shadow"]),
            &test_home(),
            temp.path(),
        )
        .unwrap();
        let LaunchPlan::Remote { host, argv, .. } = plan else {
            panic!("expected remote plan, got {plan:?}");
        };
        assert_eq!(host, "host-a");
        assert_eq!(argv, strings(&["al", "omlo", "--", "--host", "host-shadow"]));
    }

    #[test]
    fn remote_without_delimiter_has_no_reinserted_boundary() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();

        // Normal remote args (no literal `--`) must not gain a boundary, and
        // ordinary tool args flow straight through to the remote argv.
        let plan = build_launcher(
            LauncherKind::Omp,
            &strings(&["--host", "host-a", "--session", "sid", "prompt"]),
            &test_home(),
            temp.path(),
        )
        .unwrap();
        let LaunchPlan::Remote { argv, .. } = plan else {
            panic!("expected remote plan, got {plan:?}");
        };
        assert_eq!(
            argv,
            strings(&["al", "omlo", "--session", "sid", "prompt"])
        );
        assert!(!argv.iter().any(|arg| arg == "--"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_protected_tail_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();

        // A non-UTF8 protected tail must round-trip byte-for-byte through the
        // remote argv, with the literal `--` boundary retained before it.
        let non_utf8 = OsStr::from_bytes(b"\xff\xfe bad");
        let mut argv = strings(&["--host", "host-a", "--"]);
        argv.push(non_utf8.to_owned());

        let plan = build_launcher(
            LauncherKind::Omp,
            &argv,
            &test_home(),
            temp.path(),
        )
        .unwrap();
        let LaunchPlan::Remote {
            argv: remote_argv, ..
        } = plan else {
            panic!("expected remote plan, got {plan:?}");
        };
        assert_eq!(remote_argv.len(), 4);
        assert_eq!(remote_argv[0], os("al"));
        assert_eq!(remote_argv[1], os("omlo"));
        assert_eq!(remote_argv[2], os("--"));
        assert_eq!(
            OsStr::new(&remote_argv[3]).as_bytes(),
            non_utf8.as_bytes()
        );
    }
}
