//! Pick mosh only when both sides can run it: local `mosh` and remote
//! `mosh-server`. Otherwise ssh. `AL_REMOTE=mosh|ssh` skips the probe.

use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteTool {
    Mosh,
    Ssh,
}

impl RemoteTool {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mosh => "mosh",
            Self::Ssh => "ssh",
        }
    }
}

static REMOTE_MOSH: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn preferred_for(host: &str) -> RemoteTool {
    if let Some(tool) = forced_tool() {
        return tool;
    }
    if !tool_on_path("mosh") {
        return RemoteTool::Ssh;
    }
    choose(None, true, remote_has_mosh_server(host))
}

fn choose(force: Option<RemoteTool>, local_mosh: bool, remote_mosh: bool) -> RemoteTool {
    match force {
        Some(tool) => tool,
        None if local_mosh && remote_mosh => RemoteTool::Mosh,
        None => RemoteTool::Ssh,
    }
}

fn forced_tool() -> Option<RemoteTool> {
    match env::var("AL_REMOTE") {
        Ok(value) if value.eq_ignore_ascii_case("ssh") => Some(RemoteTool::Ssh),
        Ok(value) if value.eq_ignore_ascii_case("mosh") => Some(RemoteTool::Mosh),
        _ => None,
    }
}

fn remote_has_mosh_server(host: &str) -> bool {
    if cfg!(test) {
        // Lib tests must not open ssh. Integration tests run the real `al` binary.
        return true;
    }
    if let Some(hit) = cache_get(host) {
        return hit;
    }
    let found = probe_remote_mosh_server(host);
    cache_set(host, found);
    found
}

fn cache_get(host: &str) -> Option<bool> {
    REMOTE_MOSH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(host)
        .copied()
}

fn cache_set(host: &str, found: bool) {
    REMOTE_MOSH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(host.to_owned(), found);
}

fn probe_remote_mosh_server(host: &str) -> bool {
    let output = Command::new("ssh").args(ssh_probe_args(host)).output();
    matches!(output, Ok(output) if output.status.success())
}

fn ssh_probe_args(host: &str) -> [&str; 11] {
    [
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ConnectionAttempts=1",
        "-o",
        "BatchMode=yes",
        "--",
        host,
        "command",
        "-v",
        "mosh-server",
    ]
}

pub fn argv(host: &str, remote: &[&str], tty: bool) -> Vec<String> {
    argv_with(preferred_for(host), host, remote, tty)
}

/// Local pick only: force flag, cached probe, or local `mosh`. Does not ssh.
pub fn argv_preview(host: &str, remote: &[&str], tty: bool) -> Vec<String> {
    argv_with(preview_tool(host), host, remote, tty)
}

fn preview_tool(host: &str) -> RemoteTool {
    if let Some(tool) = forced_tool() {
        return tool;
    }
    if !tool_on_path("mosh") {
        return RemoteTool::Ssh;
    }
    match cache_get(host) {
        Some(remote) => choose(None, true, remote),
        None => RemoteTool::Mosh,
    }
}

pub fn argv_with(tool: RemoteTool, host: &str, remote: &[&str], tty: bool) -> Vec<String> {
    match tool {
        RemoteTool::Mosh => {
            let mut out = vec!["mosh".into(), "--".into(), host.into()];
            out.extend(remote.iter().map(|part| (*part).to_owned()));
            out
        }
        RemoteTool::Ssh => {
            let mut out = vec!["ssh".into()];
            if tty {
                out.push("-tt".into());
            }
            out.extend([
                "-o".into(),
                "ConnectTimeout=10".into(),
                "-o".into(),
                "ConnectionAttempts=1".into(),
                "--".into(),
                host.into(),
            ]);
            out.extend(remote.iter().map(|part| (*part).to_owned()));
            out
        }
    }
}

pub fn argv_os(host: &OsStr, remote: impl AsRef<OsStr>, tty: bool) -> (OsString, Vec<OsString>) {
    let host = host.to_string_lossy();
    let remote = remote.as_ref().to_string_lossy();
    let argv = argv(&host, &[remote.as_ref()], tty);
    let mut parts = argv.into_iter().map(OsString::from);
    let program = parts.next().unwrap_or_else(|| OsString::from("ssh"));
    (program, parts.collect())
}

pub fn command(host: &str, remote: &[&str], tty: bool) -> Command {
    command_from_argv(&argv(host, remote, tty))
}

pub fn output(host: &str, remote: &[&str], tty: bool) -> io::Result<Output> {
    let argv = argv(host, remote, tty);
    match command_from_argv(&argv).output() {
        Err(error) if should_fallback(&error, &argv) => {
            command_from_argv(&argv_with(RemoteTool::Ssh, host, remote, tty)).output()
        }
        other => other,
    }
}

pub fn status(host: &str, remote: &[&str], tty: bool) -> io::Result<std::process::ExitStatus> {
    let argv = argv(host, remote, tty);
    match command_from_argv(&argv).status() {
        Err(error) if should_fallback(&error, &argv) => {
            command_from_argv(&argv_with(RemoteTool::Ssh, host, remote, tty)).status()
        }
        other => other,
    }
}

pub fn spawn_with(
    host: &str,
    remote: &[&str],
    tty: bool,
    configure: impl Fn(&mut Command),
) -> io::Result<std::process::Child> {
    let argv = argv(host, remote, tty);
    let mut primary = command_from_argv(&argv);
    configure(&mut primary);
    match primary.spawn() {
        Err(error) if should_fallback(&error, &argv) => {
            let mut fallback = command_from_argv(&argv_with(RemoteTool::Ssh, host, remote, tty));
            configure(&mut fallback);
            fallback.spawn()
        }
        other => other,
    }
}

fn command_from_argv(argv: &[String]) -> Command {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}

fn should_fallback(error: &io::Error, argv: &[String]) -> bool {
    should_fallback_with(forced_tool(), error, argv)
}

fn should_fallback_with(force: Option<RemoteTool>, error: &io::Error, argv: &[String]) -> bool {
    force.is_none()
        && error.kind() == io::ErrorKind::NotFound
        && argv.first().is_some_and(|program| program == "mosh")
}

fn tool_on_path(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| executable(&dir.join(name)))
}

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteTool, argv_with, choose, should_fallback_with, ssh_probe_args};
    use std::io;

    #[test]
    fn mosh_and_ssh_argv_shapes() {
        assert_eq!(
            argv_with(RemoteTool::Mosh, "host-a", &["tmux", "list-panes"], false),
            ["mosh", "--", "host-a", "tmux", "list-panes"]
        );
        assert_eq!(
            argv_with(RemoteTool::Ssh, "host-a", &["tmux", "list-panes"], true),
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
                "list-panes"
            ]
        );
    }

    #[test]
    fn mosh_needs_local_client_and_remote_server() {
        assert_eq!(choose(None, true, true), RemoteTool::Mosh);
        assert_eq!(choose(None, true, false), RemoteTool::Ssh);
        assert_eq!(choose(None, false, true), RemoteTool::Ssh);
        assert_eq!(choose(None, false, false), RemoteTool::Ssh);
        assert_eq!(
            choose(Some(RemoteTool::Mosh), false, false),
            RemoteTool::Mosh
        );
        assert_eq!(choose(Some(RemoteTool::Ssh), true, true), RemoteTool::Ssh);
    }

    #[test]
    fn missing_mosh_falls_back_only_when_not_forced() {
        let missing = io::Error::from(io::ErrorKind::NotFound);
        let mosh = ["mosh".to_owned()];
        let ssh = ["ssh".to_owned()];
        assert!(should_fallback_with(None, &missing, &mosh));
        assert!(!should_fallback_with(
            Some(RemoteTool::Mosh),
            &missing,
            &mosh
        ));
        assert!(!should_fallback_with(None, &missing, &ssh));
        assert!(!should_fallback_with(
            None,
            &io::Error::from(io::ErrorKind::PermissionDenied),
            &mosh
        ));
    }

    #[test]
    fn remote_probe_uses_ssh_and_mosh_server() {
        assert_eq!(
            ssh_probe_args("host-a"),
            [
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ConnectionAttempts=1",
                "-o",
                "BatchMode=yes",
                "--",
                "host-a",
                "command",
                "-v",
                "mosh-server",
            ]
        );
    }
}
