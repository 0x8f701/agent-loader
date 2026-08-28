#[cfg(unix)]
mod platform {
use std::env;
use std::ffi::{CStr, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tempfile::{Builder, TempDir};

const STARTUP_READY_DELAY: Duration = Duration::from_millis(150);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const EARLY_EXIT_GRACE: Duration = Duration::from_secs(10);
const PAYLOAD_MAGIC: &[u8; 8] = b"ALTMUX\0\x01";
const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PAYLOAD_ARGS: u64 = 1_048_576;

const USAGE: &str = "Usage:\n  al tmux-run [--no-attach] [--fresh] [-s session] [-n window] [-c cwd] [-L socket-name | -S socket-path] [--] [command ...]\n\nBehavior:\n  - One command argument is evaluated by the login shell for script compatibility.\n  - Two or more command arguments preserve and execute exact argv.\n  - In the target session, run the command directly unless --fresh is used.\n  - Otherwise create and validate a detached session, then attach or switch.\n  - --no-attach creates and validates without attaching or switching.\n";

/// How the command supplied to `tmux-run` is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMode {
    /// Evaluate the one compatibility command string with the selected login shell.
    ShellString,
    /// Execute the command as native argv without a shell boundary.
    Argv,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExactCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl ExactCommand {
    fn from_spec(spec: &crate::launcher::CommandSpec) -> Self {
        let mut argv = Vec::with_capacity(
            1 + spec.args.len() + spec.env_remove.len() * 2 + spec.env_set.len(),
        );
        if !spec.env_remove.is_empty() || !spec.env_set.is_empty() {
            argv.push(os("env"));
            for name in &spec.env_remove {
                argv.extend([os("-u"), name.clone()]);
            }
            for (name, value) in &spec.env_set {
                let mut assignment = name.clone();
                assignment.push("=");
                assignment.push(value);
                argv.push(assignment);
            }
        }
        argv.push(spec.program.clone());
        argv.extend_from_slice(&spec.args);
        let program = argv.remove(0);
        Self { program, args: argv }
    }

    fn into_argv(self) -> Vec<OsString> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program);
        argv.extend(self.args);
        argv
    }
}

/// A parsed `tmux-run` invocation. `Argv` mode retains native `OsString`
/// values exactly; `ShellString` mode deliberately preserves the legacy
/// single-command-argument shell boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub no_attach: bool,
    pub fresh: bool,
    pub session: OsString,
    pub window: Option<OsString>,
    pub cwd: PathBuf,
    pub socket_name: Option<OsString>,
    pub socket_path: Option<OsString>,
    pub command: Vec<OsString>,
    pub command_mode: CommandMode,
    fallback: Option<ExactCommand>,
    pub help: bool,
}

impl Request {
    pub fn parse(argv: &[OsString]) -> Result<Self> {
        let mut no_attach = false;
        let mut fresh = false;
        let mut session = None;
        let mut window = None;
        let mut cwd = env::current_dir().context("reading current directory for tmux-run")?;
        let mut socket_name = None;
        let mut socket_path = None;
        let mut force_argv = false;
        let mut help = false;
        let mut index = 0;

        while index < argv.len() {
            let argument = argv[index].as_os_str();
            if argument == OsStr::new("--no-attach") {
                no_attach = true;
                index += 1;
            } else if argument == OsStr::new("--fresh") {
                fresh = true;
                index += 1;
            } else if argument == OsStr::new("--argv") {
                force_argv = true;
                index += 1;
            } else if argument == OsStr::new("-h") || argument == OsStr::new("--help") {
                help = true;
                index += 1;
                break;
            } else if argument == OsStr::new("--") {
                index += 1;
                break;
            } else if argument == OsStr::new("-s") || argument == OsStr::new("--session") {
                session = Some(option_value(argv, &mut index, "--session")?);
            } else if argument == OsStr::new("-n") || argument == OsStr::new("--window") {
                window = Some(option_value(argv, &mut index, "--window")?);
            } else if argument == OsStr::new("-c") || argument == OsStr::new("--cwd") {
                cwd = PathBuf::from(option_value(argv, &mut index, "--cwd")?);
            } else if argument == OsStr::new("-L") || argument == OsStr::new("--socket-name") {
                socket_name = Some(option_value(argv, &mut index, "--socket-name")?);
            } else if argument == OsStr::new("-S") || argument == OsStr::new("--socket-path") {
                socket_path = Some(option_value(argv, &mut index, "--socket-path")?);
            } else {
                break;
            }
        }

        if socket_name.is_some() && socket_path.is_some() {
            bail!("tmux-run: -L/--socket-name and -S/--socket-path are mutually exclusive");
        }

        let command = if help {
            Vec::new()
        } else {
            argv[index..].to_vec()
        };
        let command_mode = if force_argv || command.len() != 1 {
            CommandMode::Argv
        } else {
            CommandMode::ShellString
        };
        let session = session
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default_session(&command));

        Ok(Self {
            no_attach,
            fresh,
            session,
            window: window.filter(|value| !value.is_empty()),
            cwd,
            socket_name,
            socket_path,
            command,
            command_mode,
            fallback: None,
            help,
        })
    }
}

fn option_value(argv: &[OsString], index: &mut usize, option: &'static str) -> Result<OsString> {
    let value_index = *index + 1;
    let value = argv
        .get(value_index)
        .ok_or_else(|| anyhow!("tmux-run: {option} requires a value"))?
        .clone();
    *index += 2;
    Ok(value)
}

fn default_session(command: &[OsString]) -> OsString {
    let Some(program) = command.first() else {
        return OsString::from("shell");
    };
    Path::new(program)
        .file_name()
        .filter(|name| !name.is_empty())
        .map(OsStr::to_owned)
        .unwrap_or_else(|| OsString::from("run"))
}

/// Parse and run raw `tmux-run` arguments, returning the exact command or tmux
/// exit status. Multi-argument and `--argv` commands retain exact argv.
pub fn run_argv(argv: &[OsString]) -> Result<i32> {
    let request = Request::parse(argv)?;
    run(&request)
}

/// Run a launcher command in tmux with an unconditional exact-argv boundary.
pub fn run_exact(session: &str, spec: &crate::launcher::CommandSpec) -> Result<i32> {
    run_exact_fallback(session, spec, None)
}

/// Run a launcher primary command and, only when it completes unsuccessfully,
/// execute the optional fallback as exact argv in the same pane.
pub fn run_exact_fallback(
    session: &str,
    primary: &crate::launcher::CommandSpec,
    fallback: Option<&crate::launcher::CommandSpec>,
) -> Result<i32> {
    run(&exact_request(session, primary, fallback)?)
}

/// Build the exact-argv request for a launcher command: `env`-prefixed argv
/// derived from the spec's environment edits, the spec's cwd (or the current
/// directory), and an optional exact-argv fallback.
fn exact_request(
    session: &str,
    primary: &crate::launcher::CommandSpec,
    fallback: Option<&crate::launcher::CommandSpec>,
) -> Result<Request> {
    let primary_command = ExactCommand::from_spec(primary);
    Ok(Request {
        no_attach: false,
        fresh: false,
        session: os(session),
        window: None,
        cwd: primary
            .cwd
            .clone()
            .unwrap_or(env::current_dir().context("reading current directory for tmux-run")?),
        socket_name: None,
        socket_path: None,
        command: primary_command.into_argv(),
        fallback: fallback.map(ExactCommand::from_spec),
        command_mode: CommandMode::Argv,
        help: false,
    })
}

/// Run a parsed request.
pub fn run(request: &Request) -> Result<i32> {
    if request.help {
        print!("{USAGE}");
        return Ok(0);
    }

    let environment = RunEnvironment {
        inside_tmux: inside_requested_tmux(request),
        executable: env::current_exe().context("locating al for tmux child")?,
        shell: login_shell(),
        startup_timeout: startup_timeout()?,
        process_id: std::process::id(),
    };
    run_with(request, &environment, &mut SystemExecutor)
}
fn inside_requested_tmux(request: &Request) -> bool {
    let Some(tmux) = env::var_os("TMUX").filter(|value| !value.is_empty()) else {
        return false;
    };
    let socket = tmux
        .as_bytes()
        .split(|byte| *byte == b',')
        .next()
        .unwrap_or_default();
    socket_matches_request(request, socket, env::var_os("TMUX_TMPDIR").as_deref())
}

fn socket_matches_request(request: &Request, socket: &[u8], tmux_tmpdir: Option<&OsStr>) -> bool {
    if let Some(path) = &request.socket_path {
        return socket == path.as_bytes();
    }
    if let Some(name) = &request.socket_name {
        let mut path = tmux_tmpdir
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        path.push(format!("tmux-{}", unsafe { libc::geteuid() }));
        path.push(name);
        return socket == path.as_os_str().as_bytes();
    }
    true
}

fn startup_timeout() -> Result<Duration> {
    let Some(value) = env::var_os("TMUX_RUN_STARTUP_TIMEOUT") else {
        return Ok(DEFAULT_STARTUP_TIMEOUT);
    };
    let value = value
        .to_str()
        .ok_or_else(|| anyhow!("TMUX_RUN_STARTUP_TIMEOUT is not valid UTF-8"))?;
    let seconds: f64 = value
        .parse()
        .with_context(|| format!("invalid TMUX_RUN_STARTUP_TIMEOUT: {value:?}"))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("TMUX_RUN_STARTUP_TIMEOUT must be a positive number");
    }
    Ok(Duration::from_secs_f64(seconds))
}

#[derive(Debug)]
struct RunEnvironment {
    inside_tmux: bool,
    executable: PathBuf,
    shell: OsString,
    startup_timeout: Duration,
    process_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecuteMode {
    Capture,
    Inherit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecuteOutput {
    status: i32,
    stdout: Vec<u8>,
}

trait Executor {
    fn execute(
        &mut self,
        program: &OsStr,
        args: &[OsString],
        mode: ExecuteMode,
    ) -> io::Result<ExecuteOutput>;

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Debug)]
struct SystemExecutor;

impl Executor for SystemExecutor {
    fn execute(
        &mut self,
        program: &OsStr,
        args: &[OsString],
        mode: ExecuteMode,
    ) -> io::Result<ExecuteOutput> {
        match mode {
            ExecuteMode::Capture => {
                let output = Command::new(program).args(args).output()?;
                Ok(ExecuteOutput {
                    status: exit_status_code(output.status),
                    stdout: output.stdout,
                })
            }
            ExecuteMode::Inherit => {
                let status = Command::new(program).args(args).status()?;
                Ok(ExecuteOutput {
                    status: exit_status_code(status),
                    stdout: Vec::new(),
                })
            }
        }
    }
}

fn run_with<E: Executor>(
    request: &Request,
    environment: &RunEnvironment,
    executor: &mut E,
) -> Result<i32> {
    let current_session = if environment.inside_tmux {
        let output = tmux_capture(request, executor, [os("display-message"), os("-p"), os("#S")])?;
        if output.status != 0 {
            return Ok(output.status);
        }
        Some(os_from_output_line(&output.stdout))
    } else {
        None
    };

    if current_session.as_ref() == Some(&request.session) && !request.fresh {
        return execute_direct(request, &environment.shell, executor);
    }

    if current_session.as_ref() == Some(&request.session)
        && request.fresh
        && request.no_attach
    {
        eprintln!("tmux-run: --fresh cannot replace the current session with --no-attach");
        return Ok(2);
    }

    let swap_current = current_session.as_ref() == Some(&request.session) && request.fresh;
    let (create_session, old_session) = if swap_current {
        let create = temporary_session_name(&request.session, "new", environment.process_id);
        let old = temporary_session_name(&request.session, "old", environment.process_id);
        if has_session(request, executor, &create)? || has_session(request, executor, &old)? {
            eprintln!(
                "tmux-run: temporary replacement session already exists for {}",
                request.session.to_string_lossy()
            );
            return Ok(1);
        }
        (create, Some(old))
    } else {
        if has_session(request, executor, &request.session)? {
            let status = tmux_status(
                request,
                executor,
                [os("kill-session"), os("-t"), request.session.clone()],
            )?;
            if status != 0 {
                return Ok(status);
            }
        }
        (request.session.clone(), None)
    };

    let child_launch = if request.command.is_empty() {
        None
    } else {
        Some(ChildLaunch::create(
            &environment.shell,
            request.command_mode,
            &request.command,
            request.fallback.as_ref(),
        )?)
    };

    let mut new_session = vec![
        os("new-session"),
        os("-d"),
        os("-s"),
        create_session.clone(),
    ];
    if let Some(window) = &request.window {
        new_session.extend([os("-n"), window.clone()]);
    }
    new_session.extend([
        os("-c"),
        request.cwd.as_os_str().to_owned(),
        os("-e"),
        os("TERM=tmux-256color"),
        os("-e"),
        os("COLORTERM=truecolor"),
    ]);
    if let Some(launch) = &child_launch {
        new_session.extend([
            environment.executable.as_os_str().to_owned(),
            os("__tmux-child"),
            os("--payload"),
            launch.payload.as_os_str().to_owned(),
            os("--ready"),
            launch.ready.as_os_str().to_owned(),
        ]);
    } else {
        new_session.push(environment.shell.clone());
    }

    let status = tmux_status(request, executor, new_session)?;
    if status != 0 {
        return Ok(status);
    }

    if let Some(launch) = &child_launch {
        let target = pane_target(&create_session);
        match wait_for_startup(
            request,
            executor,
            &create_session,
            &target,
            &launch.ready,
            environment.startup_timeout,
        )? {
            StartupResult::Ready => {}
            StartupResult::Failed(status) => return Ok(status),
        }
    }

    if swap_current {
        let old_session = old_session.as_ref().expect("swap has old session");
        let status = tmux_status(
            request,
            executor,
            [
                os("rename-session"),
                os("-t"),
                request.session.clone(),
                old_session.clone(),
            ],
        )?;
        if status != 0 {
            kill_session(request, executor, &create_session);
            eprintln!(
                "tmux-run: failed to stage current session replacement: {}",
                request.session.to_string_lossy()
            );
            return Ok(1);
        }

        let status = tmux_status(
            request,
            executor,
            [
                os("rename-session"),
                os("-t"),
                create_session.clone(),
                request.session.clone(),
            ],
        )?;
        if status != 0 {
            let _ = tmux_status(
                request,
                executor,
                [
                    os("rename-session"),
                    os("-t"),
                    old_session.clone(),
                    request.session.clone(),
                ],
            );
            kill_session(request, executor, &create_session);
            eprintln!(
                "tmux-run: failed to activate current session replacement: {}",
                request.session.to_string_lossy()
            );
            return Ok(1);
        }
    }

    if request.no_attach {
        return Ok(0);
    }

    if environment.inside_tmux {
        let status = tmux_status(
            request,
            executor,
            [
                os("switch-client"),
                os("-t"),
                request.session.clone(),
            ],
        )?;
        if status != 0 {
            if swap_current {
                let old_session = old_session.as_ref().expect("swap has old session");
                let _ = tmux_status(
                    request,
                    executor,
                    [
                        os("rename-session"),
                        os("-t"),
                        request.session.clone(),
                        create_session.clone(),
                    ],
                );
                let _ = tmux_status(
                    request,
                    executor,
                    [
                        os("rename-session"),
                        os("-t"),
                        old_session.clone(),
                        request.session.clone(),
                    ],
                );
                kill_session(request, executor, &create_session);
                eprintln!(
                    "tmux-run: failed to switch to replacement session: {}",
                    request.session.to_string_lossy()
                );
                return Ok(1);
            }
            return Ok(status);
        }
        if let Some(old_session) = old_session.as_ref() {
            return tmux_status(
                request,
                executor,
                [os("kill-session"), os("-t"), old_session.clone()],
            );
        }
        Ok(0)
    } else {
        tmux_status(
            request,
            executor,
            [
                os("attach-session"),
                os("-t"),
                request.session.clone(),
            ],
        )
    }
}

fn execute_direct<E: Executor>(
    request: &Request,
    shell: &OsStr,
    executor: &mut E,
) -> Result<i32> {
    let (program, args) = command_program(shell, request.command_mode, &request.command)?;
    let output = executor
        .execute(&program, &args, ExecuteMode::Inherit)
        .with_context(|| format!("executing {:?}", program))?;
    if output.status == 0 {
        return Ok(0);
    }
    let Some(fallback) = &request.fallback else {
        return Ok(output.status);
    };
    Ok(executor
        .execute(&fallback.program, &fallback.args, ExecuteMode::Inherit)
        .with_context(|| format!("executing fallback {:?}", fallback.program))?
        .status)
}

fn command_program(
    shell: &OsStr,
    mode: CommandMode,
    command: &[OsString],
) -> Result<(OsString, Vec<OsString>)> {
    match mode {
        CommandMode::ShellString => {
            let command = command
                .first()
                .ok_or_else(|| anyhow!("tmux shell-string command is empty"))?;
            Ok((shell.to_owned(), vec![os("-lc"), command.clone()]))
        }
        CommandMode::Argv => {
            let Some((program, args)) = command.split_first() else {
                return Ok((shell.to_owned(), Vec::new()));
            };
            Ok((program.clone(), args.to_vec()))
        }
    }
}

fn wait_for_startup<E: Executor>(
    request: &Request,
    executor: &mut E,
    create_session: &OsStr,
    target: &OsStr,
    ready: &Path,
    timeout: Duration,
) -> Result<StartupResult> {
    let interval_nanos = STARTUP_POLL_INTERVAL.as_nanos();
    let samples = timeout.as_nanos().div_ceil(interval_nanos).max(1);

    for _ in 0..samples {
        if let Some(state) = read_ready_state(ready)? {
            if state == b"ready" {
                return Ok(StartupResult::Ready);
            }
            if let Some(status) = parse_early_status(&state) {
                kill_session(request, executor, create_session);
                eprintln!(
                    "tmux-run: command exited during startup (status {status}): {}",
                    display_command(&request.command)
                );
                return Ok(StartupResult::Failed(status));
            }
        }

        let pane = tmux_capture(
            request,
            executor,
            [
                os("display-message"),
                os("-p"),
                os("-t"),
                target.to_owned(),
                os("#{pane_dead}"),
            ],
        )?;
        let pane_state = trim_line_end(&pane.stdout);
        if pane.status != 0 || pane_state.is_empty() || pane_state == b"1" {
            kill_session(request, executor, create_session);
            eprintln!(
                "tmux-run: pane exited during startup: {}",
                display_command(&request.command)
            );
            return Ok(StartupResult::Failed(1));
        }
        executor.sleep(STARTUP_POLL_INTERVAL);
    }

    kill_session(request, executor, create_session);
    eprintln!(
        "tmux-run: command did not become ready: {}",
        display_command(&request.command)
    );
    Ok(StartupResult::Failed(1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupResult {
    Ready,
    Failed(i32),
}

fn read_ready_state(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(state) => Ok(Some(trim_line_end(&state).to_vec())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading readiness marker {path:?}")),
    }
}

fn parse_early_status(state: &[u8]) -> Option<i32> {
    let status = state.strip_prefix(b"exit ")?;
    let status = std::str::from_utf8(status).ok()?.trim().parse().ok()?;
    Some(if status == 0 { 1 } else { status })
}

fn display_command(command: &[OsString]) -> String {
    command
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_session<E: Executor>(request: &Request, executor: &mut E, session: &OsStr) -> Result<bool> {
    let output = tmux_capture(
        request,
        executor,
        [os("has-session"), os("-t"), session.to_owned()],
    )?;
    Ok(output.status == 0)
}

fn kill_session<E: Executor>(request: &Request, executor: &mut E, session: &OsStr) {
    let _ = tmux_status(
        request,
        executor,
        [os("kill-session"), os("-t"), session.to_owned()],
    );
}

fn tmux_capture<E, I>(
    request: &Request,
    executor: &mut E,
    command: I,
) -> Result<ExecuteOutput>
where
    E: Executor,
    I: IntoIterator<Item = OsString>,
{
    tmux_execute(request, executor, command, ExecuteMode::Capture)
}

fn tmux_status<E, I>(request: &Request, executor: &mut E, command: I) -> Result<i32>
where
    E: Executor,
    I: IntoIterator<Item = OsString>,
{
    Ok(tmux_execute(request, executor, command, ExecuteMode::Inherit)?.status)
}

fn tmux_execute<E, I>(
    request: &Request,
    executor: &mut E,
    command: I,
    mode: ExecuteMode,
) -> Result<ExecuteOutput>
where
    E: Executor,
    I: IntoIterator<Item = OsString>,
{
    let mut args = socket_args(request);
    args.extend(command);
    executor
        .execute(OsStr::new("tmux"), &args, mode)
        .context("executing tmux")
}

fn socket_args(request: &Request) -> Vec<OsString> {
    if let Some(name) = &request.socket_name {
        vec![os("-L"), name.clone()]
    } else if let Some(path) = &request.socket_path {
        vec![os("-S"), path.clone()]
    } else {
        Vec::new()
    }
}

fn temporary_session_name(session: &OsStr, stage: &str, process_id: u32) -> OsString {
    let mut temporary = session.to_owned();
    temporary.push(format!(".tmux-run-{stage}.{process_id}"));
    temporary
}

fn pane_target(session: &OsStr) -> OsString {
    let mut target = session.to_owned();
    target.push(":0.0");
    target
}

fn os_from_output_line(output: &[u8]) -> OsString {
    OsString::from_vec(trim_line_end(output).to_vec())
}

fn trim_line_end(mut value: &[u8]) -> &[u8] {
    while matches!(value.last(), Some(b'\n' | b'\r')) {
        value = &value[..value.len() - 1];
    }
    value
}

fn os(value: impl AsRef<OsStr>) -> OsString {
    value.as_ref().to_owned()
}

struct ChildLaunch {
    _directory: TempDir,
    payload: PathBuf,
    ready: PathBuf,
}

impl ChildLaunch {
    fn create(
        shell: &OsStr,
        mode: CommandMode,
        argv: &[OsString],
        fallback: Option<&ExactCommand>,
    ) -> Result<Self> {
        let directory = Builder::new()
            .prefix("al-tmux-")
            .tempdir()
            .context("creating private tmux child directory")?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .context("securing tmux child directory")?;
        let payload = directory.path().join("argv");
        let ready = directory.path().join("ready");
        write_payload(&payload, shell, mode, argv, fallback)?;
        Ok(Self {
            _directory: directory,
            payload,
            ready,
        })
    }
}

fn write_payload(
    path: &Path,
    shell: &OsStr,
    mode: CommandMode,
    argv: &[OsString],
    fallback: Option<&ExactCommand>,
) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating private tmux argv payload {path:?}"))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing tmux argv payload {path:?}"))?;
    let mut writer = BufWriter::new(&file);
    writer
        .write_all(PAYLOAD_MAGIC)
        .context("writing tmux payload magic")?;
    write_bytes(&mut writer, shell.as_bytes())?;
    writer
        .write_all(&[match mode {
            CommandMode::ShellString => 0,
            CommandMode::Argv => 1,
        }])
        .context("writing tmux payload command mode")?;
    write_u64(&mut writer, argv.len() as u64)?;
    for argument in argv {
        write_bytes(&mut writer, argument.as_bytes())?;
    }
    match fallback {
        Some(fallback) => {
            writer
                .write_all(&[1])
                .context("writing tmux payload fallback flag")?;
            write_bytes(&mut writer, fallback.program.as_bytes())?;
            write_u64(&mut writer, fallback.args.len() as u64)?;
            for argument in &fallback.args {
                write_bytes(&mut writer, argument.as_bytes())?;
            }
        }
        None => writer
            .write_all(&[0])
            .context("writing tmux payload fallback flag")?,
    }
    writer.flush().context("flushing tmux argv payload")?;
    file.sync_all().context("syncing tmux argv payload")?;
    Ok(())
}

fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    write_u64(writer, bytes.len() as u64)?;
    writer.write_all(bytes).context("writing tmux argv bytes")
}

fn write_u64(writer: &mut impl Write, value: u64) -> Result<()> {
    writer
        .write_all(&value.to_le_bytes())
        .context("writing tmux payload length")
}

fn read_payload(path: &Path) -> Result<ChildPayload> {
    let cleanup = FileCleanup(path.to_owned());
    let file = File::open(path).with_context(|| format!("opening tmux argv payload {path:?}"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting tmux argv payload {path:?}"))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > MAX_PAYLOAD_BYTES
    {
        bail!("tmux argv payload is not a private, owned regular file");
    }

    let mut reader = BufReader::new(file);
    let mut magic = [0_u8; PAYLOAD_MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .context("reading tmux payload magic")?;
    if &magic != PAYLOAD_MAGIC {
        bail!("invalid tmux argv payload");
    }
    let shell = OsString::from_vec(read_bytes(&mut reader)?);
    let mut mode = [0_u8; 1];
    reader
        .read_exact(&mut mode)
        .context("reading tmux payload command mode")?;
    let mode = match mode[0] {
        0 => CommandMode::ShellString,
        1 => CommandMode::Argv,
        _ => bail!("invalid tmux argv payload command mode"),
    };
    let argument_count = read_u64(&mut reader)?;
    if argument_count > MAX_PAYLOAD_ARGS {
        bail!("tmux argv payload has too many arguments");
    }
    let mut argv = Vec::with_capacity(argument_count as usize);
    for _ in 0..argument_count {
        argv.push(OsString::from_vec(read_bytes(&mut reader)?));
    }
    let mut fallback_flag = [0_u8; 1];
    reader
        .read_exact(&mut fallback_flag)
        .context("reading tmux payload fallback flag")?;
    let fallback = match fallback_flag[0] {
        0 => None,
        1 => {
            let program = OsString::from_vec(read_bytes(&mut reader)?);
            let argument_count = read_u64(&mut reader)?;
            if argument_count > MAX_PAYLOAD_ARGS {
                bail!("tmux fallback payload has too many arguments");
            }
            let mut args = Vec::with_capacity(argument_count as usize);
            for _ in 0..argument_count {
                args.push(OsString::from_vec(read_bytes(&mut reader)?));
            }
            Some(ExactCommand { program, args })
        }
        _ => bail!("invalid tmux payload fallback flag"),
    };
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing).context("checking tmux payload end")? != 0 {
        bail!("tmux argv payload has trailing data");
    }
    drop(cleanup);
    Ok(ChildPayload {
        shell,
        mode,
        argv,
        fallback,
    })
}

fn read_bytes(reader: &mut impl Read) -> Result<Vec<u8>> {
    let length = read_u64(reader)?;
    if length > MAX_PAYLOAD_BYTES {
        bail!("tmux argv payload field is too large");
    }
    let mut bytes = vec![0_u8; length as usize];
    reader
        .read_exact(&mut bytes)
        .context("reading tmux argv bytes")?;
    Ok(bytes)
}

fn read_u64(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0_u8; 8];
    reader
        .read_exact(&mut bytes)
        .context("reading tmux payload length")?;
    Ok(u64::from_le_bytes(bytes))
}

#[derive(Debug, PartialEq, Eq)]
struct ChildPayload {
    shell: OsString,
    mode: CommandMode,
    argv: Vec<OsString>,
    fallback: Option<ExactCommand>,
}

struct FileCleanup(PathBuf);

impl Drop for FileCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Hidden child entry point. It consumes the private raw-argv payload, signals
/// readiness only after the command survives 150 ms, and enters the selected
/// login shell after a normally started command exits.
pub fn run_child(payload: &Path, ready: &Path) -> Result<i32> {
    run_child_with(payload, ready, &mut SystemChildRuntime)
}

trait ChildHandle {
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
    fn wait(&mut self) -> io::Result<i32>;
}

trait ChildRuntime {
    fn spawn(&mut self, program: &OsStr, args: &[OsString]) -> io::Result<Box<dyn ChildHandle>>;
    fn login_shell(&mut self, shell: &OsStr, args: &[OsString]) -> io::Result<i32>;
    fn sleep(&mut self, duration: Duration);
}

struct SystemChildRuntime;

impl ChildRuntime for SystemChildRuntime {
    fn spawn(&mut self, program: &OsStr, args: &[OsString]) -> io::Result<Box<dyn ChildHandle>> {
        Ok(Box::new(Command::new(program).args(args).spawn()?))
    }

    fn login_shell(&mut self, shell: &OsStr, args: &[OsString]) -> io::Result<i32> {
        use std::os::unix::process::CommandExt;
        Err(Command::new(shell).args(args).exec())
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

impl ChildHandle for Child {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Child::try_wait(self).map(|status| status.map(exit_status_code))
    }

    fn wait(&mut self) -> io::Result<i32> {
        Child::wait(self).map(exit_status_code)
    }
}

fn run_child_with<R: ChildRuntime>(payload: &Path, ready: &Path, runtime: &mut R) -> Result<i32> {
    let mut payload = read_payload(payload)?;
    let (program, args) = command_program(&payload.shell, payload.mode, &payload.argv)?;
    let ready_cleanup = FileCleanup(ready.to_owned());
    let mut active_program = program;
    let mut child = match runtime.spawn(&active_program, &args) {
        Ok(child) => child,
        Err(error) => {
            return report_spawn_error(
                ready,
                "starting tmux child command",
                &active_program,
                &error,
                false,
                runtime,
            );
        }
    };

    if let Some(status) = startup_exit(&mut *child, runtime)? {
        if status != 0 {
            if let Some(fallback) = payload.fallback.take() {
                active_program = fallback.program;
                child = match runtime.spawn(&active_program, &fallback.args) {
                    Ok(child) => child,
                    Err(error) => {
                        return report_spawn_error(
                            ready,
                            "starting tmux fallback command",
                            &active_program,
                            &error,
                            false,
                            runtime,
                        );
                    }
                };
                if let Some(status) = startup_exit(&mut *child, runtime)? {
                    return report_early_exit(ready, status, runtime);
                }
            } else {
                return report_early_exit(ready, status, runtime);
            }
        } else {
            return report_early_exit(ready, status, runtime);
        }
    }

    write_ready_marker(ready, b"ready")?;
    let status = child.wait().context("waiting for tmux child command")?;
    if status != 0 {
        if let Some(fallback) = payload.fallback.take() {
            let mut child = match runtime.spawn(&fallback.program, &fallback.args) {
                Ok(child) => child,
                Err(error) => {
                    return report_spawn_error(
                        ready,
                        "starting tmux fallback command",
                        &fallback.program,
                        &error,
                        true,
                        runtime,
                    );
                }
            };
            let _ = child.wait().context("waiting for tmux fallback command")?;
        }
    }
    drop(ready_cleanup);

    let shell_args = if shell_basename(&payload.shell) == OsStr::new("fish") {
        vec![os("-li")]
    } else {
        vec![os("-l")]
    };
    runtime
        .login_shell(&payload.shell, &shell_args)
        .with_context(|| format!("entering login shell {:?}", payload.shell))
}

fn report_spawn_error<R: ChildRuntime>(
    ready: &Path,
    context: &str,
    program: &OsStr,
    error: &io::Error,
    marker_exists: bool,
    runtime: &mut R,
) -> Result<i32> {
    let status = match error.kind() {
        io::ErrorKind::NotFound => 127,
        io::ErrorKind::PermissionDenied => 126,
        _ => 1,
    };
    eprintln!("al: {context} {program:?}: {error}");
    if marker_exists {
        replace_ready_marker(ready, format!("exit {status}\n").as_bytes())?;
        runtime.sleep(EARLY_EXIT_GRACE);
        Ok(status)
    } else {
        report_early_exit(ready, status, runtime)
    }
}

fn startup_exit<R: ChildRuntime>(
    child: &mut dyn ChildHandle,
    runtime: &mut R,
) -> Result<Option<i32>> {
    let tick = Duration::from_millis(10);
    let samples = STARTUP_READY_DELAY.as_millis().div_ceil(tick.as_millis());
    for _ in 0..samples {
        if let Some(status) = child.try_wait().context("checking tmux child command")? {
            return Ok(Some(status));
        }
        runtime.sleep(tick);
    }
    child.try_wait().context("checking tmux child command")
}

fn report_early_exit<R: ChildRuntime>(ready: &Path, status: i32, runtime: &mut R) -> Result<i32> {
    let status = if status == 0 { 1 } else { status };
    write_ready_marker(ready, format!("exit {status}\n").as_bytes())?;
    runtime.sleep(EARLY_EXIT_GRACE);
    Ok(status)
}

fn write_ready_marker(path: &Path, state: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating tmux readiness marker {path:?}"))?;
    file.write_all(state)
        .with_context(|| format!("writing tmux readiness marker {path:?}"))?;
    file.flush()
        .with_context(|| format!("flushing tmux readiness marker {path:?}"))?;
    Ok(())
}
fn replace_ready_marker(path: &Path, state: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening tmux readiness marker {path:?} for update"))?;
    file.write_all(state)
        .with_context(|| format!("updating tmux readiness marker {path:?}"))?;
    file.flush()
        .with_context(|| format!("flushing tmux readiness marker {path:?}"))?;
    Ok(())
}


fn shell_basename(shell: &OsStr) -> &OsStr {
    Path::new(shell).file_name().unwrap_or(shell)
}

fn login_shell() -> OsString {
    choose_shell(env::var_os("SHELL"), passwd_shell(), is_executable)
}

fn choose_shell<F>(
    environment_shell: Option<OsString>,
    passwd_shell: Option<OsString>,
    mut executable: F,
) -> OsString
where
    F: FnMut(&OsStr) -> bool,
{
    environment_shell
        .filter(|shell| executable(shell))
        .or_else(|| passwd_shell.filter(|shell| executable(shell)))
        .unwrap_or_else(|| OsString::from("/bin/bash"))
}

fn is_executable(path: &OsStr) -> bool {
    fs::metadata(Path::new(path)).is_ok_and(|metadata| {
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    })
}

fn passwd_shell() -> Option<OsString> {
    let initial_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if initial_size > 0 {
        initial_size as usize
    } else {
        16 * 1024
    };

    loop {
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; size];
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < MAX_PAYLOAD_BYTES as usize {
            size = size.saturating_mul(2);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let record = unsafe { record.assume_init() };
        if record.pw_shell.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(record.pw_shell) }.to_bytes();
        return (!bytes.is_empty()).then(|| OsString::from_vec(bytes.to_vec()));
    }
}

fn exit_status_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    use std::os::unix::process::ExitStatusExt;
    128 + status.signal().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::os::unix::ffi::OsStringExt;

    use tempfile::tempdir;

    use super::*;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(os).collect()
    }

    fn request(argv: &[&str]) -> Request {
        Request::parse(&strings(argv)).expect("request")
    }

    fn spec(program: &str, args: &[&str]) -> crate::launcher::CommandSpec {
        crate::launcher::CommandSpec {
            program: os(program),
            args: strings(args),
            cwd: None,
            env_remove: Vec::new(),
            env_set: Vec::new(),
        }
    }

    #[test]
    fn parser_supports_all_options_and_stops_at_command() {
        let parsed = request(&[
            "--no-attach",
            "--fresh",
            "-s",
            "agents",
            "--window",
            "main pane",
            "-c",
            "/tmp/work tree",
            "-L",
            "private socket",
            "--",
            "printf",
            "%s; $(nope)",
            "two words",
        ]);
        assert!(parsed.no_attach);
        assert!(parsed.fresh);
        assert_eq!(parsed.session, OsStr::new("agents"));
        assert_eq!(parsed.window.as_deref(), Some(OsStr::new("main pane")));
        assert_eq!(parsed.cwd, Path::new("/tmp/work tree"));
        assert_eq!(
            parsed.socket_name.as_deref(),
            Some(OsStr::new("private socket"))
        );
        assert_eq!(
            parsed.command,
            strings(&["printf", "%s; $(nope)", "two words"])
        );
        assert_eq!(parsed.command_mode, CommandMode::Argv);

        let stopped = request(&["echo", "--fresh", "-s", "not-an-option-now"]);
        assert!(!stopped.fresh);
        assert_eq!(stopped.session, OsStr::new("echo"));
        assert_eq!(
            stopped.command,
            strings(&["echo", "--fresh", "-s", "not-an-option-now"])
        );
        assert!(Request::parse(&strings(&["-s"])).is_err());
        assert!(Request::parse(&strings(&["-L", "one", "-S", "two"])).is_err());
    }


    #[test]
    fn parser_derives_shell_and_program_sessions() {
        assert_eq!(request(&[]).session, OsStr::new("shell"));
        assert_eq!(request(&["/usr/bin/python3", "-q"]).session, OsStr::new("python3"));
        assert_eq!(request(&["-s", "", "cargo"]).session, OsStr::new("cargo"));
    }

    #[test]
    fn parser_distinguishes_legacy_shell_string_from_forced_argv() {
        assert_eq!(
            request(&["echo value | cat"]).command_mode,
            CommandMode::ShellString
        );
        assert_eq!(
            request(&["--argv", "--", "single-program"]).command_mode,
            CommandMode::Argv
        );
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Record {
        program: OsString,
        args: Vec<OsString>,
        mode: ExecuteMode,
    }

    #[derive(Debug)]
    struct Recorder {
        current: Option<OsString>,
        sessions: HashSet<OsString>,
        records: Vec<Record>,
        startup_state: Option<Vec<u8>>,
        direct_status: i32,
        switch_status: i32,
        pane_alive: bool,
        rename_fail_source: Option<OsString>,
        rename_fail_status: i32,
        sleeps: usize,
        child_paths: Option<(PathBuf, PathBuf)>,
    }

    impl Default for Recorder {
        fn default() -> Self {
            Self {
                current: None,
                sessions: HashSet::new(),
                records: Vec::new(),
                startup_state: Some(b"ready".to_vec()),
                direct_status: 0,
                switch_status: 0,
                pane_alive: true,
                child_paths: None,
                rename_fail_source: None,
                rename_fail_status: 1,
                sleeps: 0,
            }
        }
    }

    impl Recorder {
        fn tmux_command<'a>(&self, args: &'a [OsString]) -> &'a [OsString] {
            if matches!(args.first().and_then(|arg| arg.to_str()), Some("-L" | "-S")) {
                &args[2..]
            } else {
                args
            }
        }

        fn tmux_records(&self) -> impl Iterator<Item = &[OsString]> {
            self.records
                .iter()
                .filter(|record| record.program == OsStr::new("tmux"))
                .map(|record| record.args.as_slice())
        }
    }

    impl Executor for Recorder {
        fn execute(
            &mut self,
            program: &OsStr,
            args: &[OsString],
            mode: ExecuteMode,
        ) -> io::Result<ExecuteOutput> {
            self.records.push(Record {
                program: program.to_owned(),
                args: args.to_vec(),
                mode,
            });
            if program != OsStr::new("tmux") {
                return Ok(ExecuteOutput {
                    status: self.direct_status,
                    stdout: Vec::new(),
                });
            }

            let command = self.tmux_command(args).to_vec();
            let name = command.first().and_then(|value| value.to_str()).unwrap_or("");
            match name {
                "display-message" if command.iter().any(|arg| arg == OsStr::new("#S")) => {
                    let mut stdout = self.current.clone().unwrap_or_default().into_vec();
                    stdout.push(b'\n');
                    Ok(ExecuteOutput { status: 0, stdout })
                }
                "display-message" => Ok(ExecuteOutput {
                    status: if self.pane_alive { 0 } else { 1 },
                    stdout: if self.pane_alive {
                        b"0\n".to_vec()
                    } else {
                        Vec::new()
                    },
                }),
                "has-session" => {
                    let target = option_after(&command, "-t").unwrap_or_default();
                    Ok(ExecuteOutput {
                        status: if self.sessions.contains(&target) { 0 } else { 1 },
                        stdout: Vec::new(),
                    })
                }
                "new-session" => {
                    let target = option_after(&command, "-s").unwrap_or_default();
                    self.sessions.insert(target);
                    if let (Some(payload), Some(ready)) = (
                        option_after(&command, "--payload"),
                        option_after(&command, "--ready"),
                    ) {
                        let payload = PathBuf::from(payload);
                        let ready = PathBuf::from(ready);
                        self.child_paths = Some((payload, ready.clone()));
                        if let Some(state) = &self.startup_state {
                            fs::write(&ready, state)?;
                        }
                    }
                    Ok(ExecuteOutput {
                        status: 0,
                        stdout: Vec::new(),
                    })
                }
                "kill-session" => {
                    if let Some(target) = option_after(&command, "-t") {
                        self.sessions.remove(&target);
                    }
                    Ok(ExecuteOutput {
                        status: 0,
                        stdout: Vec::new(),
                    })
                }
                "rename-session" => {
                    let old = option_after(&command, "-t").unwrap_or_default();
                    let new = command.last().cloned().unwrap_or_default();
                    if let Some(fail_source) = &self.rename_fail_source {
                        if fail_source == &old {
                            return Ok(ExecuteOutput {
                                status: self.rename_fail_status,
                                stdout: Vec::new(),
                            });
                        }
                    }
                    self.sessions.remove(&old);
                    self.sessions.insert(new.clone());
                    if self.current.as_ref() == Some(&old) {
                        self.current = Some(new);
                    }
                    Ok(ExecuteOutput {
                        status: 0,
                        stdout: Vec::new(),
                    })
                }
                "switch-client" => Ok(ExecuteOutput {
                    status: self.switch_status,
                    stdout: Vec::new(),
                }),
                "attach-session" => Ok(ExecuteOutput {
                    status: 0,
                    stdout: Vec::new(),
                }),
                other => panic!("unexpected tmux command {other:?}: {command:?}"),
            }
        }

        fn sleep(&mut self, _duration: Duration) {
            self.sleeps += 1;
        }
    }

    fn option_after(command: &[OsString], option: &str) -> Option<OsString> {
        command
            .iter()
            .position(|argument| argument == OsStr::new(option))
            .and_then(|index| command.get(index + 1))
            .cloned()
    }

    fn environment(inside_tmux: bool) -> RunEnvironment {
        RunEnvironment {
            inside_tmux,
            executable: PathBuf::from("/proc/self/exe"),
            shell: OsString::from("/bin/bash"),
            startup_timeout: Duration::from_millis(100),
            process_id: 4242,
        }
    }

    #[test]
    fn matching_session_executes_exact_argv_and_returns_its_status() {
        let request = request(&[
            "-s",
            "agents",
            "--",
            "/tool with spaces",
            "literal ; $(not shell)",
            "two words",
        ]);
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents")]),
            direct_status: 37,
            ..Default::default()
        };
        let status = run_with(&request, &environment(true), &mut recorder).unwrap();
        assert_eq!(status, 37);
        let direct = recorder.records.last().unwrap();
        assert_eq!(direct.program, OsStr::new("/tool with spaces"));
        assert_eq!(
            direct.args,
            strings(&["literal ; $(not shell)", "two words"])
        );
        assert_eq!(direct.mode, ExecuteMode::Inherit);
        assert_eq!(recorder.tmux_records().count(), 1);
    }
    #[test]
    fn matching_session_runs_exact_fallback_only_after_primary_failure() {
        let mut request = request(&["--argv", "-s", "agents", "--", "primary"]);
        request.fallback = Some(ExactCommand {
            program: os("fallback ; literal"),
            args: strings(&["two words", "$(not-shell)"]),
        });
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents")]),
            direct_status: 17,
            ..Default::default()
        };
        assert_eq!(
            run_with(&request, &environment(true), &mut recorder).unwrap(),
            17
        );
        let direct: Vec<&Record> = recorder
            .records
            .iter()
            .filter(|record| record.program != OsStr::new("tmux"))
            .collect();
        assert_eq!(direct.len(), 2);
        assert_eq!(direct[0].program, OsStr::new("primary"));
        assert_eq!(direct[1].program, OsStr::new("fallback ; literal"));
        assert_eq!(direct[1].args, strings(&["two words", "$(not-shell)"]));
    }

    #[test]
    fn single_string_command_uses_login_shell_parsing() {
        let request = request(&["-s", "agents", "echo one | sed s/one/two/"]);
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents")]),
            direct_status: 19,
            ..Default::default()
        };
        assert_eq!(
            run_with(&request, &environment(true), &mut recorder).unwrap(),
            19
        );
        let direct = recorder.records.last().unwrap();
        assert_eq!(direct.program, OsStr::new("/bin/bash"));
        assert_eq!(
            direct.args,
            strings(&["-lc", "echo one | sed s/one/two/"])
        );
    }

    #[test]
    fn forced_single_argv_never_uses_shell_parsing() {
        let request = request(&["--argv", "-s", "agents", "--", "literal ; command"]);
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents")]),
            direct_status: 7,
            ..Default::default()
        };
        assert_eq!(
            run_with(&request, &environment(true), &mut recorder).unwrap(),
            7
        );
        let direct = recorder.records.last().unwrap();
        assert_eq!(direct.program, OsStr::new("literal ; command"));
        assert!(direct.args.is_empty());
    }

    #[test]
    fn from_spec_prepends_env_edits_before_program() {
        let mut command = spec("agent", &["arg one", "arg two"]);
        command.env_remove = strings(&["RUST_LOG", "DEBUG"]);
        command.env_set = vec![
            (os("TERM"), os("xterm-256color")),
            (os("SPACED"), os("a b=c")),
        ];
        let exact = ExactCommand::from_spec(&command);
        assert_eq!(exact.program, os("env"));
        assert_eq!(
            exact.args,
            strings(&[
                "-u",
                "RUST_LOG",
                "-u",
                "DEBUG",
                "TERM=xterm-256color",
                "SPACED=a b=c",
                "agent",
                "arg one",
                "arg two",
            ])
        );
        assert_eq!(
            exact.clone().into_argv(),
            strings(&[
                "env",
                "-u",
                "RUST_LOG",
                "-u",
                "DEBUG",
                "TERM=xterm-256color",
                "SPACED=a b=c",
                "agent",
                "arg one",
                "arg two",
            ])
        );
    }

    #[test]
    fn from_spec_without_env_edits_uses_program_directly() {
        let command = spec("agent", &["arg one", "arg two"]);
        let exact = ExactCommand::from_spec(&command);
        assert_eq!(exact.program, os("agent"));
        assert_eq!(exact.args, strings(&["arg one", "arg two"]));
        assert_eq!(
            exact.clone().into_argv(),
            strings(&["agent", "arg one", "arg two"])
        );
    }

    #[test]
    fn exact_request_propagates_spec_cwd() {
        let mut command = spec("agent", &["arg"]);
        command.cwd = Some(PathBuf::from("/work tree"));
        let request = exact_request("agents", &command, None).unwrap();
        assert_eq!(request.cwd, Path::new("/work tree"));
        assert_eq!(request.session, OsStr::new("agents"));
        assert_eq!(request.command, strings(&["agent", "arg"]));
        assert_eq!(request.command_mode, CommandMode::Argv);
        assert!(request.fallback.is_none());
        assert!(!request.no_attach);
        assert!(!request.fresh);
        assert!(!request.help);
        assert!(request.window.is_none());
        assert!(request.socket_name.is_none());
        assert!(request.socket_path.is_none());
    }

    #[test]
    fn exact_request_without_spec_cwd_uses_current_directory() {
        let command = spec("agent", &["arg"]);
        let request = exact_request("agents", &command, None).unwrap();
        assert_eq!(
            request.cwd,
            env::current_dir().expect("current directory")
        );
    }

    #[test]
    fn exact_request_bridges_primary_and_fallback_exact_argv() {
        let mut primary = spec("agent", &["arg one"]);
        primary.env_remove = strings(&["RUST_LOG"]);
        primary.env_set = vec![(os("TERM"), os("xterm-256color"))];
        let mut fallback = spec("fallback ; literal", &["two words", "$(not-shell)"]);
        fallback.env_remove = strings(&["DEBUG"]);
        let request = exact_request("agents", &primary, Some(&fallback)).unwrap();
        assert_eq!(
            request.command,
            strings(&[
                "env",
                "-u",
                "RUST_LOG",
                "TERM=xterm-256color",
                "agent",
                "arg one",
            ])
        );
        assert_eq!(
            request.fallback,
            Some(ExactCommand {
                program: os("env"),
                args: strings(&["-u", "DEBUG", "fallback ; literal", "two words", "$(not-shell)"]),
            })
        );
        assert_eq!(request.command_mode, CommandMode::Argv);
    }


    #[test]
    fn fresh_current_session_renames_only_after_readiness_and_never_kills_current() {
        let request = request(&["--fresh", "-s", "agents", "--", "agent", "arg"]);
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents")]),
            ..Default::default()
        };
        assert_eq!(
            run_with(&request, &environment(true), &mut recorder).unwrap(),
            0
        );

        let commands: Vec<Vec<OsString>> = recorder
            .tmux_records()
            .map(|args| recorder.tmux_command(args).to_vec())
            .collect();
        let names: Vec<&str> = commands
            .iter()
            .map(|command| command[0].to_str().unwrap())
            .collect();
        let new_index = names.iter().position(|name| *name == "new-session").unwrap();
        let first_rename = names.iter().position(|name| *name == "rename-session").unwrap();
        let switch = names.iter().position(|name| *name == "switch-client").unwrap();
        let final_kill = names.iter().rposition(|name| *name == "kill-session").unwrap();
        assert!(new_index < first_rename && first_rename < switch && switch < final_kill);
        assert!(!commands[..switch].iter().any(|command| {
            command[0] == OsStr::new("kill-session")
                && option_after(command, "-t").as_deref() == Some(OsStr::new("agents"))
        }));
        assert_eq!(
            option_after(&commands[first_rename], "-t").as_deref(),
            Some(OsStr::new("agents"))
        );
    }

    #[test]
    fn early_failure_returns_status_kills_session_and_cleans_private_files() {
        let request = request(&[
            "--no-attach",
            "-s",
            "bad",
            "--",
            "false",
            "spaced arg",
        ]);
        let mut recorder = Recorder {
            startup_state: Some(b"exit 23\n".to_vec()),
            ..Default::default()
        };
        assert_eq!(
            run_with(&request, &environment(false), &mut recorder).unwrap(),
            23
        );
        let commands: Vec<Vec<OsString>> = recorder
            .tmux_records()
            .map(|args| recorder.tmux_command(args).to_vec())
            .collect();
        assert!(commands.iter().any(|command| {
            command.first() == Some(&os("kill-session"))
                && option_after(command, "-t").as_deref() == Some(OsStr::new("bad"))
        }));
        let (payload, ready) = recorder.child_paths.unwrap();
        assert!(!payload.exists());
        assert!(!ready.exists());
    }

    #[test]
    fn socket_selection_is_forwarded_to_every_tmux_invocation() {
        for socket_option in [
            strings(&["-L", "hermetic name"]),
            strings(&["-S", "/tmp/hermetic socket"]),
        ] {
            let mut argv = socket_option.clone();
            argv.extend(strings(&[
                "--no-attach",
                "-s",
                "socket-test",
                "--",
                "sleep",
                "10",
            ]));
            let request = Request::parse(&argv).unwrap();
            let mut recorder = Recorder::default();
            assert_eq!(
                run_with(&request, &environment(false), &mut recorder).unwrap(),
                0
            );
            for args in recorder.tmux_records() {
                assert_eq!(&args[..2], socket_option.as_slice());
            }
        }
    }
    #[test]
    fn private_socket_matches_only_its_requested_tmux_environment() {
        let request = request(&["-S", "/tmp/requested.sock", "--no-attach"]);
        assert!(!socket_matches_request(
            &request,
            b"/tmp/unrelated.sock",
            None
        ));
        assert!(socket_matches_request(
            &request,
            b"/tmp/requested.sock",
            None
        ));
    }


    #[test]
    fn fresh_no_attach_rejects_replacing_current_session() {
        let request = request(&["--fresh", "--no-attach", "-s", "agents", "agent"]);
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents")]),
            ..Default::default()
        };
        assert_eq!(
            run_with(&request, &environment(true), &mut recorder).unwrap(),
            2
        );
        assert_eq!(recorder.tmux_records().count(), 1);
    }

    #[test]
    fn raw_payload_round_trips_spaces_metacharacters_and_non_utf8_privately() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("payload");
        let raw = OsString::from_vec(vec![b'a', 0xff, b'z']);
        let argv = vec![
            os("tool name"),
            os("; $(touch /nope) ' \""),
            raw.clone(),
        ];
        write_payload(
            &path,
            OsStr::new("/bin/fish"),
            CommandMode::Argv,
            &argv,
            None,
        )
        .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            read_payload(&path).unwrap(),
            ChildPayload {
                shell: os("/bin/fish"),
                mode: CommandMode::Argv,
                argv,
                fallback: None,
            }
        );
        assert!(!path.exists());
    }

    struct FakeChild {
        polls: usize,
        early_status: Option<i32>,
        waited_status: i32,
    }

    impl ChildHandle for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            self.polls += 1;
            Ok(self.early_status)
        }

        fn wait(&mut self) -> io::Result<i32> {
            Ok(self.waited_status)
        }
    }

    #[derive(Default)]
    struct FakeChildRuntime {
        spawned: Option<(OsString, Vec<OsString>)>,
        login: Option<(OsString, Vec<OsString>)>,
        login_status: i32,
        sleeps: Vec<Duration>,
    }

    impl ChildRuntime for FakeChildRuntime {
        fn spawn(&mut self, program: &OsStr, args: &[OsString]) -> io::Result<Box<dyn ChildHandle>> {
            self.spawned = Some((program.to_owned(), args.to_vec()));
            Ok(Box::new(FakeChild {
                polls: 0,
                early_status: None,
                waited_status: 9,
            }))
        }

        fn login_shell(&mut self, shell: &OsStr, args: &[OsString]) -> io::Result<i32> {
            self.login = Some((shell.to_owned(), args.to_vec()));
            Ok(self.login_status)
        }

        fn sleep(&mut self, duration: Duration) {
            self.sleeps.push(duration);
        }
    }
    struct SpawnFailureRuntime {
        ready: PathBuf,
        fake_primary_failure: bool,
        spawn_count: usize,
        marker_during_grace: Option<Vec<u8>>,
        sleeps: Vec<Duration>,
    }

    impl ChildRuntime for SpawnFailureRuntime {
        fn spawn(
            &mut self,
            program: &OsStr,
            args: &[OsString],
        ) -> io::Result<Box<dyn ChildHandle>> {
            self.spawn_count += 1;
            if self.fake_primary_failure && self.spawn_count == 1 {
                return Ok(Box::new(FakeChild {
                    polls: 0,
                    early_status: Some(9),
                    waited_status: 9,
                }));
            }
            Command::new(program)
                .args(args)
                .spawn()
                .map(|child| Box::new(child) as Box<dyn ChildHandle>)
        }

        fn login_shell(&mut self, _shell: &OsStr, _args: &[OsString]) -> io::Result<i32> {
            panic!("spawn failure must not enter a login shell")
        }

        fn sleep(&mut self, duration: Duration) {
            self.sleeps.push(duration);
            if duration == EARLY_EXIT_GRACE {
                self.marker_during_grace = Some(fs::read(&self.ready).unwrap());
            }
        }
    }

    fn assert_spawn_failure(
        program: &Path,
        fallback: bool,
        expected_status: i32,
        expected_marker: &[u8],
    ) {
        let directory = tempdir().unwrap();
        let payload = directory.path().join("payload");
        let ready = directory.path().join("ready");
        let fallback_command = fallback.then(|| ExactCommand {
            program: program.as_os_str().to_owned(),
            args: Vec::new(),
        });
        let command = if fallback {
            vec![os("primary")]
        } else {
            vec![program.as_os_str().to_owned()]
        };
        write_payload(
            &payload,
            OsStr::new("/bin/bash"),
            CommandMode::Argv,
            &command,
            fallback_command.as_ref(),
        )
        .unwrap();
        let mut runtime = SpawnFailureRuntime {
            ready: ready.clone(),
            fake_primary_failure: fallback,
            spawn_count: 0,
            marker_during_grace: None,
            sleeps: Vec::new(),
        };

        assert_eq!(
            run_child_with(&payload, &ready, &mut runtime).unwrap(),
            expected_status
        );
        assert_eq!(runtime.marker_during_grace.as_deref(), Some(expected_marker));
        assert_eq!(runtime.spawn_count, if fallback { 2 } else { 1 });
        assert_eq!(runtime.sleeps, vec![EARLY_EXIT_GRACE]);
        assert!(!payload.exists());
        assert!(!ready.exists());
    }

    #[test]
    fn absent_primary_executable_reports_127_through_readiness_marker() {
        let directory = tempdir().unwrap();
        assert_spawn_failure(
            &directory.path().join("absent"),
            false,
            127,
            b"exit 127\n",
        );
    }

    #[test]
    fn non_executable_primary_reports_126_through_readiness_marker() {
        let directory = tempdir().unwrap();
        let program = directory.path().join("non-executable");
        fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();
        assert_spawn_failure(&program, false, 126, b"exit 126\n");
    }

    #[test]
    fn absent_fallback_executable_reports_127_through_readiness_marker() {
        let directory = tempdir().unwrap();
        assert_spawn_failure(
            &directory.path().join("absent"),
            true,
            127,
            b"exit 127\n",
        );
    }

    #[test]
    fn non_executable_fallback_reports_126_through_readiness_marker() {
        let directory = tempdir().unwrap();
        let program = directory.path().join("non-executable");
        fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();
        assert_spawn_failure(&program, true, 126, b"exit 126\n");
    }


    #[test]
    fn normal_child_exit_removes_marker_and_enters_selected_login_shell() {
        let directory = tempdir().unwrap();
        let payload = directory.path().join("payload");
        let ready = directory.path().join("ready");
        write_payload(
            &payload,
            OsStr::new("/custom/fish"),
            CommandMode::Argv,
            &strings(&["agent", "literal ; value", "two words"]),
            None,
        )
        .unwrap();
        let mut runtime = FakeChildRuntime {
            login_status: 42,
            ..Default::default()
        };
        assert_eq!(
            run_child_with(&payload, &ready, &mut runtime).unwrap(),
            42
        );
        assert_eq!(
            runtime.spawned,
            Some((
                os("agent"),
                strings(&["literal ; value", "two words"])
            ))
        );
        assert_eq!(
            runtime.login,
            Some((os("/custom/fish"), strings(&["-li"])))
        );
        assert!(!payload.exists());
        assert!(!ready.exists());
        assert_eq!(runtime.sleeps.len(), 15);
    }

    #[derive(Default)]
    struct FallbackRuntime {
        spawned: Vec<(OsString, Vec<OsString>)>,
        login: Option<(OsString, Vec<OsString>)>,
    }

    impl ChildRuntime for FallbackRuntime {
        fn spawn(&mut self, program: &OsStr, args: &[OsString]) -> io::Result<Box<dyn ChildHandle>> {
            self.spawned.push((program.to_owned(), args.to_vec()));
            let primary = self.spawned.len() == 1;
            Ok(Box::new(FakeChild {
                polls: 0,
                early_status: primary.then_some(9),
                waited_status: 0,
            }))
        }

        fn login_shell(&mut self, shell: &OsStr, args: &[OsString]) -> io::Result<i32> {
            self.login = Some((shell.to_owned(), args.to_vec()));
            Ok(0)
        }

        fn sleep(&mut self, _duration: Duration) {}
    }

    #[test]
    fn primary_failure_runs_fallback_as_exact_argv_before_readiness() {
        let directory = tempdir().unwrap();
        let payload = directory.path().join("payload");
        let ready = directory.path().join("ready");
        let fallback = ExactCommand {
            program: os("fallback ; literal"),
            args: strings(&["two words", "$(not-shell)"]),
        };
        write_payload(
            &payload,
            OsStr::new("/bin/bash"),
            CommandMode::Argv,
            &strings(&["primary", "arg"]),
            Some(&fallback),
        )
        .unwrap();
        let mut runtime = FallbackRuntime::default();
        assert_eq!(
            run_child_with(&payload, &ready, &mut runtime).unwrap(),
            0
        );
        assert_eq!(
            runtime.spawned,
            vec![
                (os("primary"), strings(&["arg"])),
                (
                    os("fallback ; literal"),
                    strings(&["two words", "$(not-shell)"])
                ),
            ]
        );
        assert_eq!(runtime.login, Some((os("/bin/bash"), strings(&["-l"]))));
    }

    #[test]
    fn invalid_environment_shell_falls_back_to_passwd_then_bash() {
        let shell = choose_shell(
            Some(os("/invalid/env-shell")),
            Some(os("/passwd/shell")),
            |candidate| candidate == OsStr::new("/passwd/shell"),
        );
        assert_eq!(shell, OsStr::new("/passwd/shell"));
        let shell = choose_shell(Some(os("bad")), Some(os("also-bad")), |_| false);
        assert_eq!(shell, OsStr::new("/bin/bash"));
    }

    fn build_payload_bytes(
        shell: &str,
        mode: u8,
        argv: &[&str],
        fallback: Option<(&str, &[&str])>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.write_all(PAYLOAD_MAGIC).unwrap();
        write_bytes(&mut buf, shell.as_bytes()).unwrap();
        buf.write_all(&[mode]).unwrap();
        write_u64(&mut buf, argv.len() as u64).unwrap();
        for argument in argv {
            write_bytes(&mut buf, argument.as_bytes()).unwrap();
        }
        match fallback {
            Some((program, args)) => {
                buf.write_all(&[1]).unwrap();
                write_bytes(&mut buf, program.as_bytes()).unwrap();
                write_u64(&mut buf, args.len() as u64).unwrap();
                for argument in args {
                    write_bytes(&mut buf, argument.as_bytes()).unwrap();
                }
            }
            None => buf.write_all(&[0]).unwrap(),
        }
        buf
    }

    fn write_raw_payload(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = directory.join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        path
    }

    fn write_permissive_payload(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = write_raw_payload(directory, name, bytes);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        path
    }

    fn tmux_subcommands(recorder: &Recorder) -> Vec<OsString> {
        recorder
            .tmux_records()
            .map(|args| recorder.tmux_command(args).to_vec())
            .filter_map(|command| command.first().cloned())
            .collect()
    }

    fn killed_target(recorder: &Recorder, target: &OsStr) -> bool {
        recorder.tmux_records().any(|args| {
            let command = recorder.tmux_command(args);
            command.first() == Some(&os("kill-session"))
                && option_after(command, "-t").as_deref() == Some(target)
        })
    }

    #[test]
    fn read_payload_rejects_malformed_inputs() {
        let directory = tempdir().unwrap();

        let mut wrong_magic = build_payload_bytes("/bin/sh", 1, &["a"], None);
        wrong_magic[..PAYLOAD_MAGIC.len()].copy_from_slice(b"WRONGMAG");
        let invalid_mode = build_payload_bytes("/bin/sh", 2, &["a"], None);
        let mut invalid_fallback_flag = build_payload_bytes("/bin/sh", 1, &["a"], None);
        *invalid_fallback_flag.last_mut().unwrap() = 2;
        let mut trailing = build_payload_bytes("/bin/sh", 1, &["a"], None);
        trailing.push(0xff);

        let oversized_field = {
            let mut buf = Vec::new();
            buf.write_all(PAYLOAD_MAGIC).unwrap();
            write_u64(&mut buf, MAX_PAYLOAD_BYTES + 1).unwrap();
            buf
        };
        let excessive_args = {
            let mut buf = Vec::new();
            buf.write_all(PAYLOAD_MAGIC).unwrap();
            write_bytes(&mut buf, b"/bin/sh").unwrap();
            buf.write_all(&[1]).unwrap();
            write_u64(&mut buf, MAX_PAYLOAD_ARGS + 1).unwrap();
            buf
        };
        let excessive_fallback_args = {
            let mut buf = Vec::new();
            buf.write_all(PAYLOAD_MAGIC).unwrap();
            write_bytes(&mut buf, b"/bin/sh").unwrap();
            buf.write_all(&[1]).unwrap();
            write_u64(&mut buf, 1).unwrap();
            write_bytes(&mut buf, b"primary").unwrap();
            buf.write_all(&[1]).unwrap();
            write_bytes(&mut buf, b"fallback").unwrap();
            write_u64(&mut buf, MAX_PAYLOAD_ARGS + 1).unwrap();
            buf
        };

        let cases: &[(&str, &[u8], &str)] = &[
            ("wrong magic", &wrong_magic, "invalid tmux argv payload"),
            (
                "invalid command mode",
                &invalid_mode,
                "invalid tmux argv payload command mode",
            ),
            (
                "excessive argument count",
                &excessive_args,
                "tmux argv payload has too many arguments",
            ),
            (
                "excessive fallback argument count",
                &excessive_fallback_args,
                "tmux fallback payload has too many arguments",
            ),
            (
                "oversized field",
                &oversized_field,
                "tmux argv payload field is too large",
            ),
            (
                "invalid fallback flag",
                &invalid_fallback_flag,
                "invalid tmux payload fallback flag",
            ),
            (
                "trailing bytes",
                &trailing,
                "tmux argv payload has trailing data",
            ),
        ];

        for (label, bytes, expected) in cases {
            let path = write_raw_payload(directory.path(), "payload", bytes);
            let result = read_payload(&path);
            assert!(result.is_err(), "{label}: malformed payload must be rejected");
            let message = format!("{:#}", result.as_ref().unwrap_err());
            assert!(
                message.contains(*expected),
                "{label}: expected error containing {expected:?}, got {message:?}"
            );
            assert!(
                !path.exists(),
                "{label}: rejected payload must be cleaned up"
            );
        }
    }

    #[test]
    fn read_payload_rejects_permissive_file_permissions() {
        let directory = tempdir().unwrap();
        let bytes = build_payload_bytes("/bin/sh", 1, &["a"], None);
        let path = write_permissive_payload(directory.path(), "payload", &bytes);
        let error = read_payload(&path).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("not a private, owned regular file"),
            "permissive payload must be rejected, got {message:?}"
        );
    }

    #[test]
    fn malformed_payload_never_spawns_child_runtime() {
        let directory = tempdir().unwrap();

        let mut wrong_magic = build_payload_bytes("/bin/sh", 1, &["a"], None);
        wrong_magic[..PAYLOAD_MAGIC.len()].copy_from_slice(b"WRONGMAG");
        let invalid_mode = build_payload_bytes("/bin/sh", 2, &["a"], None);
        let mut invalid_fallback_flag = build_payload_bytes("/bin/sh", 1, &["a"], None);
        *invalid_fallback_flag.last_mut().unwrap() = 2;
        let mut trailing = build_payload_bytes("/bin/sh", 1, &["a"], None);
        trailing.push(0xff);
        let oversized_field = {
            let mut buf = Vec::new();
            buf.write_all(PAYLOAD_MAGIC).unwrap();
            write_u64(&mut buf, MAX_PAYLOAD_BYTES + 1).unwrap();
            buf
        };
        let excessive_args = {
            let mut buf = Vec::new();
            buf.write_all(PAYLOAD_MAGIC).unwrap();
            write_bytes(&mut buf, b"/bin/sh").unwrap();
            buf.write_all(&[1]).unwrap();
            write_u64(&mut buf, MAX_PAYLOAD_ARGS + 1).unwrap();
            buf
        };

        let cases: &[(&str, &[u8])] = &[
            ("wrong magic", &wrong_magic),
            ("invalid command mode", &invalid_mode),
            ("excessive argument count", &excessive_args),
            ("oversized field", &oversized_field),
            ("invalid fallback flag", &invalid_fallback_flag),
            ("trailing bytes", &trailing),
        ];

        for (index, (label, bytes)) in cases.iter().enumerate() {
            let path = write_raw_payload(directory.path(), &format!("payload-{index}"), bytes);
            let ready = directory.path().join(format!("ready-{index}"));
            let mut runtime = FakeChildRuntime::default();
            let result = run_child_with(&path, &ready, &mut runtime);
            assert!(result.is_err(), "{label}: malformed payload must fail closed");
            assert!(
                runtime.spawned.is_none(),
                "{label}: child runtime must never spawn for a malformed payload"
            );
            assert!(
                runtime.login.is_none(),
                "{label}: login shell must never start for a malformed payload"
            );
        }
    }

    #[test]
    fn permissive_payload_file_never_spawns_child_runtime() {
        let directory = tempdir().unwrap();
        let bytes = build_payload_bytes("/bin/sh", 1, &["a"], None);
        let path = write_permissive_payload(directory.path(), "payload", &bytes);
        let ready = directory.path().join("ready");
        let mut runtime = FakeChildRuntime::default();
        let result = run_child_with(&path, &ready, &mut runtime);
        assert!(result.is_err(), "permissive payload must fail closed");
        assert!(
            runtime.spawned.is_none(),
            "child runtime must never spawn for a permissive payload"
        );
        assert!(
            runtime.login.is_none(),
            "login shell must never start for a permissive payload"
        );
    }

    #[test]
    fn swap_current_temp_name_collision_preserves_original_and_returns_nonzero() {
        let request = request(&["--fresh", "-s", "agents", "--", "agent", "arg"]);
        let new_temp = temporary_session_name(OsStr::new("agents"), "new", 4242);
        let old_temp = temporary_session_name(OsStr::new("agents"), "old", 4242);
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents"), new_temp.clone()]),
            ..Default::default()
        };
        let status = run_with(&request, &environment(true), &mut recorder).unwrap();
        assert_eq!(
            status, 1,
            "temp-name collision must fail closed with nonzero status"
        );
        assert!(
            recorder.sessions.contains(&os("agents")),
            "original session must remain after collision"
        );
        assert!(
            recorder.sessions.contains(&new_temp),
            "pre-existing colliding temp must be left untouched"
        );
        assert!(
            !killed_target(&recorder, OsStr::new("agents")),
            "original session must not be killed on collision"
        );
        assert!(
            !recorder.sessions.contains(&old_temp),
            "no old temp must be created on collision"
        );
        let subcommands = tmux_subcommands(&recorder);
        assert!(
            !subcommands.iter().any(|name| name == &os("new-session")),
            "no new session may be created on collision"
        );
        assert!(
            !subcommands.iter().any(|name| name == &os("rename-session")),
            "no rename may be issued on collision"
        );
    }

    #[test]
    fn swap_current_activation_rename_failure_restores_original_and_cleans_temp() {
        let request = request(&["--fresh", "-s", "agents", "--", "agent", "arg"]);
        let new_temp = temporary_session_name(OsStr::new("agents"), "new", 4242);
        let old_temp = temporary_session_name(OsStr::new("agents"), "old", 4242);
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents")]),
            rename_fail_source: Some(new_temp.clone()),
            rename_fail_status: 3,
            ..Default::default()
        };
        let status = run_with(&request, &environment(true), &mut recorder).unwrap();
        assert_eq!(
            status, 1,
            "activation rename failure must fail closed with nonzero status"
        );
        assert!(
            recorder.sessions.contains(&os("agents")),
            "original session name must be restored after activation failure"
        );
        assert!(
            !recorder.sessions.contains(&new_temp),
            "temp replacement must be cleaned up after activation failure"
        );
        assert!(
            !recorder.sessions.contains(&old_temp),
            "staged old session must be rolled back after activation failure"
        );
        assert!(
            killed_target(&recorder, &new_temp),
            "temp replacement must be explicitly killed after activation failure"
        );
    }

    #[test]
    fn swap_current_switch_client_failure_restores_original_and_cleans_temp() {
        let request = request(&["--fresh", "-s", "agents", "--", "agent", "arg"]);
        let new_temp = temporary_session_name(OsStr::new("agents"), "new", 4242);
        let old_temp = temporary_session_name(OsStr::new("agents"), "old", 4242);
        let mut recorder = Recorder {
            current: Some(os("agents")),
            sessions: HashSet::from([os("agents")]),
            switch_status: 5,
            ..Default::default()
        };
        let status = run_with(&request, &environment(true), &mut recorder).unwrap();
        assert_eq!(
            status, 1,
            "switch-client failure must fail closed with nonzero status"
        );
        assert!(
            recorder.sessions.contains(&os("agents")),
            "original session name must be restored after switch-client failure"
        );
        assert!(
            !recorder.sessions.contains(&new_temp),
            "temp replacement must be cleaned up after switch-client failure"
        );
        assert!(
            !recorder.sessions.contains(&old_temp),
            "staged old session must be rolled back after switch-client failure"
        );
        assert!(
            killed_target(&recorder, &new_temp),
            "temp replacement must be explicitly killed after switch-client failure"
        );
    }

    #[test]
    fn pane_dead_during_startup_kills_session_without_waiting() {
        let request = request(&["--no-attach", "-s", "ghost", "--", "agent"]);
        let mut recorder = Recorder {
            startup_state: None,
            pane_alive: false,
            ..Default::default()
        };
        let status = run_with(&request, &environment(false), &mut recorder).unwrap();
        assert_eq!(
            status, 1,
            "pane death during startup must fail closed with nonzero status"
        );
        assert!(
            !recorder.sessions.contains(&os("ghost")),
            "dead-pane session must be killed"
        );
        assert_eq!(
            recorder.sleeps, 0,
            "pane death must be detected before any startup poll sleep"
        );
        assert!(
            killed_target(&recorder, OsStr::new("ghost")),
            "dead-pane session must be explicitly killed"
        );
    }

    #[test]
    fn startup_timeout_kills_unready_session() {
        let request = request(&["--no-attach", "-s", "stalled", "--", "agent"]);
        let mut recorder = Recorder {
            startup_state: None,
            pane_alive: true,
            ..Default::default()
        };
        let status = run_with(&request, &environment(false), &mut recorder).unwrap();
        assert_eq!(
            status, 1,
            "startup timeout must fail closed with nonzero status"
        );
        assert!(
            !recorder.sessions.contains(&os("stalled")),
            "unready session must be killed on timeout"
        );
        assert!(
            recorder.sleeps >= 1,
            "startup timeout must poll at least once before giving up"
        );
        assert!(
            killed_target(&recorder, OsStr::new("stalled")),
            "unready session must be explicitly killed on timeout"
        );
    }
}
}

#[cfg(not(unix))]
mod platform {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use anyhow::{Result, bail};

    const UNSUPPORTED: &str =
        "tmux-run is unsupported on this platform; tmux integration requires Unix";

    /// How the command supplied to `tmux-run` is executed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CommandMode {
        /// Evaluate the one compatibility command string with the selected login shell.
        ShellString,
        /// Execute the command as native argv without a shell boundary.
        Argv,
    }

    #[allow(dead_code)]
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ExactCommand;

    /// A parsed `tmux-run` invocation. `Argv` mode retains native `OsString`
    /// values exactly; `ShellString` mode deliberately preserves the legacy
    /// single-command-argument shell boundary.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Request {
        pub no_attach: bool,
        pub fresh: bool,
        pub session: OsString,
        pub window: Option<OsString>,
        pub cwd: PathBuf,
        pub socket_name: Option<OsString>,
        pub socket_path: Option<OsString>,
        pub command: Vec<OsString>,
        pub command_mode: CommandMode,
        fallback: Option<ExactCommand>,
        pub help: bool,
    }

    impl Request {
        pub fn parse(_argv: &[OsString]) -> Result<Self> {
            unsupported()
        }
    }

    /// Reject `tmux-run` on platforms where tmux integration is unavailable.
    pub fn run_argv(_argv: &[OsString]) -> Result<i32> {
        unsupported()
    }

    /// Reject a launcher tmux request on platforms where tmux integration is unavailable.
    pub fn run_exact(_session: &str, _spec: &crate::launcher::CommandSpec) -> Result<i32> {
        unsupported()
    }

    /// Reject a launcher tmux request on platforms where tmux integration is unavailable.
    pub fn run_exact_fallback(
        _session: &str,
        _primary: &crate::launcher::CommandSpec,
        _fallback: Option<&crate::launcher::CommandSpec>,
    ) -> Result<i32> {
        unsupported()
    }

    /// Reject a parsed tmux request on platforms where tmux integration is unavailable.
    pub fn run(_request: &Request) -> Result<i32> {
        unsupported()
    }

    /// Reject the hidden tmux child entry point on unsupported platforms.
    pub fn run_child(_payload: &Path, _ready: &Path) -> Result<i32> {
        unsupported()
    }

    fn unsupported<T>() -> Result<T> {
        bail!(UNSUPPORTED)
    }
}

pub use platform::{CommandMode, Request, run, run_argv, run_child, run_exact, run_exact_fallback};
