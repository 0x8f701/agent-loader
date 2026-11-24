//! Launch fresh or continuing coding agents without replacing existing sessions.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::launcher::{self, CommandSpec, LaunchPlan, LauncherKind};
use crate::tmux::NamedWindow;

pub(crate) struct LaunchOptions {
    pub tools: Vec<LauncherKind>,
    pub continue_session: bool,
    pub host: Option<String>,
    pub worktree: bool,
    pub tmux: bool,
    pub no_attach: bool,
}

pub(crate) fn run(options: LaunchOptions) -> Result<i32> {
    validate(&options)?;
    let cwd = env::current_dir().context("reading launch directory")?;
    if let Some(host) = &options.host {
        return run_remote(&options, host, &cwd);
    }
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from).context("locating home directory for launch")?;
    let root = if options.worktree { Some(repo_root(&cwd)?) } else { None };
    let mut windows = Vec::with_capacity(options.tools.len());
    let mut failed = false;
    let mut counts = Vec::new();
    for &tool in &options.tools {
        let name = window_name(tool, &mut counts);
        let prepared = (|| -> Result<NamedWindow> {
            let workdir = match &root {
                Some(root) => create_worktree(root, &cwd, &name)?,
                None => cwd.clone(),
            };
            let (spec, fallback) = commands(tool, options.continue_session, &home, &workdir)?;
            Ok(NamedWindow { name: name.clone(), spec, fallback })
        })();
        match prepared {
            Ok(window) => windows.push(window),
            Err(error) => {
                failed = true;
                eprintln!("al launch: {name} could not be prepared: {error:#}");
            }
        }
    }
    if windows.is_empty() {
        return Ok(1);
    }
    let status = if options.tmux {
        let session = launcher::tmux_session_name("al-launch", root.as_deref(), &cwd);
        crate::tmux::run_batch_windows(&session, &windows, !options.no_attach)?
    } else {
        let window = windows.remove(0);
        let plan = match window.fallback {
            Some(fallback) => LaunchPlan::Fallback { primary: window.spec, fallback },
            None => LaunchPlan::Command(window.spec),
        };
        launcher::execute_plan(&plan)?
    };
    Ok(if failed && status == 0 { 1 } else { status })
}

fn validate(options: &LaunchOptions) -> Result<()> {
    if options.tools.is_empty() {
        bail!("al launch: at least one tool is required");
    }
    if options.continue_session {
        for (index, tool) in options.tools.iter().enumerate() {
            if options.tools[..index].contains(tool) {
                bail!("al launch: --continue cannot repeat the same tool ({})", tool.as_str());
            }
        }
    }
    if options.tools.len() > 1 && !options.tmux {
        bail!("al launch: multiple interactive agents require --tmux");
    }
    if options.no_attach && !options.tmux {
        bail!("al launch: --no-attach requires --tmux");
    }
    if let Some(host) = &options.host {
        if host.is_empty() || host.starts_with('-') || host.chars().any(char::is_whitespace) {
            bail!("al launch: invalid remote host");
        }
    }
    Ok(())
}

fn window_name(tool: LauncherKind, counts: &mut Vec<(LauncherKind, usize)>) -> String {
    let count = match counts.iter_mut().find(|(kind, _)| *kind == tool) {
        Some((_, count)) => { *count += 1; *count }
        None => { counts.push((tool, 1)); 1 }
    };
    let argv = crate::new::launch_argv(Some(tool));
    format!("{}-{count}", argv[0].to_string_lossy())
}

fn commands(tool: LauncherKind, continuing: bool, home: &Path, cwd: &Path)
    -> Result<(CommandSpec, Option<CommandSpec>)>
{
    if continuing {
        return match launcher::build_launcher(tool, &[], home, cwd)? {
            LaunchPlan::Command(mut spec) => {
                spec.cwd = Some(cwd.to_owned());
                Ok((spec, None))
            }
            LaunchPlan::Fallback { mut primary, mut fallback } => {
                primary.cwd = Some(cwd.to_owned());
                fallback.cwd = Some(cwd.to_owned());
                Ok((primary, Some(fallback)))
            }
            _ => bail!("al launch: unexpected nonlocal continuation plan"),
        };
    }
    let mut argv = crate::new::launch_argv(Some(tool)).into_iter();
    let program = argv.next().context("missing fresh launcher program")?;
    let mut spec = CommandSpec::new(program, argv.collect());
    spec.cwd = Some(cwd.to_owned());
    if matches!(tool, LauncherKind::Droid | LauncherKind::Codex) {
        spec.env_remove.push(OsString::from("NO_COLOR"));
    }
    Ok((spec, None))
}

fn repo_root(cwd: &Path) -> Result<PathBuf> {
    let output = Command::new("git").args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd).output().context("locating launch repository")?;
    if !output.status.success() {
        bail!("al launch: this option requires a git repository");
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = output.stdout;
        if bytes.last() == Some(&b'\n') { bytes.pop(); }
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim_end_matches(['\r', '\n'])))
    }
}

fn create_worktree(root: &Path, cwd: &Path, name: &str) -> Result<PathBuf> {
    let parent = root.parent().context("repository has no parent directory")?;
    let repository = root.file_name().context("repository has no directory name")?;
    let mut prefix = OsString::from(repository);
    prefix.push(format!("-al-{name}-"));
    let directory = tempfile::Builder::new().prefix(&prefix).tempdir_in(parent)
        .context("reserving unique worktree directory")?.keep();
    let status = Command::new("git").current_dir(root)
        .args(["worktree", "add", "--detach", "--"]).arg(&directory).arg("HEAD")
        .status().context("creating launch worktree")?;
    if !status.success() {
        bail!("git worktree add failed (exit {}); reserved directory retained", status.code().unwrap_or(1));
    }
    let relative = cwd.strip_prefix(root).context("launch directory is outside repository")?;
    let workdir = directory.join(relative);
    if !workdir.is_dir() {
        bail!("current subdirectory is absent in the new worktree; worktree retained");
    }
    Ok(workdir)
}

fn remote_argv(options: &LaunchOptions) -> Vec<&str> {
    let mut argv = vec!["al", "launch"];
    argv.extend(options.tools.iter().map(|tool| tool.as_str()));
    if options.continue_session { argv.push("--continue"); }
    if options.worktree { argv.push("--worktree"); }
    if options.tmux { argv.push("--tmux"); }
    if options.no_attach { argv.push("--no-attach"); }
    argv
}

fn run_remote(options: &LaunchOptions, host: &str, cwd: &Path) -> Result<i32> {
    let root = repo_root(cwd)?;
    let user = env::var_os("USER").unwrap_or_default();
    let mapped = launcher::remote_path(&root, &user, cfg!(target_os = "macos"))?;
    let workdir = remote_workdir(&root, cwd, &mapped)?;
    let workdir = workdir.to_str().context("remote launch directory is not UTF-8")?;
    let inner = remote_inner(options, workdir);
    let argv = crate::remote::shell_argv(host, &["fish", "-lic", &inner], !options.no_attach);
    let status = Command::new(&argv[0]).args(&argv[1..]).status()
        .context("running remote al launch")?;
    Ok(status.code().unwrap_or(1))
}

fn remote_workdir(root: &Path, cwd: &Path, mapped_root: &Path) -> Result<PathBuf> {
    let relative = cwd.strip_prefix(root).context("launch directory is outside repository")?;
    Ok(mapped_root.join(relative))
}

fn remote_inner(options: &LaunchOptions, workdir: &str) -> String {
    format!("cd -- {}; and exec {}", crate::new::posix_quote(workdir),
        remote_argv(options).iter().map(|arg| crate::new::posix_quote(arg))
            .collect::<Vec<_>>().join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(tools: Vec<LauncherKind>) -> LaunchOptions {
        LaunchOptions { tools, continue_session: false, host: None, worktree: false,
            tmux: true, no_attach: true }
    }

    #[test]
    fn continuation_preserves_fallback_and_sets_both_workdirs() {
        let cwd = Path::new("fixture-project");
        let (primary, fallback) = commands(LauncherKind::Claude, true, Path::new("fixture-home"), cwd).unwrap();
        assert!(primary.args.iter().any(|arg| arg == "-c"));
        let fallback = fallback.unwrap();
        assert!(!fallback.args.iter().any(|arg| arg == "-c"));
        assert_eq!(primary.cwd.as_deref(), Some(cwd));
        assert_eq!(fallback.cwd.as_deref(), Some(cwd));
    }

    #[test]
    fn continuing_duplicate_tools_is_rejected_before_launch() {
        let mut options = options(vec![LauncherKind::Claude, LauncherKind::Claude]);
        assert!(validate(&options).is_ok());
        options.continue_session = true;
        assert!(validate(&options).unwrap_err().to_string().contains("cannot repeat"));
    }

    #[test]
    fn repeated_tools_receive_stable_numbered_windows() {
        let mut counts = Vec::new();
        assert_eq!(window_name(LauncherKind::Claude, &mut counts), "claude-1");
        assert_eq!(window_name(LauncherKind::Pi, &mut counts), "pi-1");
        assert_eq!(window_name(LauncherKind::Claude, &mut counts), "claude-2");
    }

    #[test]
    fn fresh_commands_reuse_new_arguments_without_resume() {
        for tool in [LauncherKind::Claude, LauncherKind::Codex, LauncherKind::Omp,
            LauncherKind::Pi, LauncherKind::Rpi, LauncherKind::Droid, LauncherKind::Grok,
            LauncherKind::Agent, LauncherKind::Antigravity] {
            let (spec, fallback) = commands(tool, false, Path::new("fixture-home"), Path::new("fixture-project")).unwrap();
            let expected = crate::new::launch_argv(Some(tool));
            assert_eq!(spec.program, expected[0]);
            assert_eq!(spec.args, expected[1..]);
            assert!(fallback.is_none());
        }
    }

    #[test]
    fn remote_arguments_preserve_repetition_and_flags_without_recursing_host() {
        let mut options = options(vec![LauncherKind::Pi, LauncherKind::Pi]);
        options.host = Some("fixture-host".into());
        options.worktree = true;
        assert_eq!(remote_argv(&options), ["al", "launch", "pilo", "pilo", "--worktree", "--tmux", "--no-attach"]);
    }

    #[test]
    fn remote_workdir_preserves_repository_relative_suffix() {
        let root = Path::new("fixture-local/project");
        let mapped = Path::new("fixture-remote/project");
        assert_eq!(remote_workdir(root, root, mapped).unwrap(), mapped);
        assert_eq!(remote_workdir(root, &root.join("src/nested"), mapped).unwrap(), mapped.join("src/nested"));
        assert!(remote_workdir(root, Path::new("fixture-local/other"), mapped).is_err());
    }

    #[test]
    fn remote_shell_words_keep_nested_cwd_distinct_for_ssh_and_mosh() {
        use crate::remote::{RemoteTool, shell_argv_with};
        let options = options(vec![LauncherKind::Pi]);
        let inner = remote_inner(&options, "fixture project/it's $(literal)");
        assert!(inner.starts_with("cd -- 'fixture project/it'\\''s $(literal)'; and exec "));
        let words = ["fish", "-lic", inner.as_str()];
        let mosh = shell_argv_with(RemoteTool::Mosh, "fixture-host", &words, false);
        assert_eq!(&mosh[3..], &["fish", "-lic", inner.as_str()]);
        let ssh = shell_argv_with(RemoteTool::Ssh, "fixture-host", &words, false);
        assert_eq!(&ssh[ssh.len() - 3..], &[
            crate::new::posix_quote("fish"), crate::new::posix_quote("-lic"),
            crate::new::posix_quote(&inner),
        ]);
    }

    #[test]
    fn multiple_agents_and_detachment_require_tmux() {
        let mut options = options(vec![LauncherKind::Pi, LauncherKind::Claude]);
        options.tmux = false;
        assert!(validate(&options).is_err());
        options.tools.pop();
        assert!(validate(&options).is_err());
        options.no_attach = false;
        assert!(validate(&options).is_ok());
    }
}
