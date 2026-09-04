use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::domain::{SourceTool, TargetTool};

#[derive(Debug, Parser)]
#[command(name = "al", version, about, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}


impl Cli {
    pub fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let arguments = arguments
            .into_iter()
            .map(Into::into)
            .collect::<Vec<OsString>>();
        let mut cli = <Self as Parser>::try_parse_from(arguments.clone())?;
        restore_raw_tail(&mut cli, &arguments);
        Ok(cli)
    }
}

fn restore_raw_tail(cli: &mut Cli, arguments: &[OsString]) {
    let Some(command_name) = arguments.get(1).and_then(|argument| argument.to_str()) else {
        return;
    };
    if !matches!(
        command_name,
        "omlo" | "pilo" | "rpilo" | "grolo" | "hyperlo" | "dolo" | "colo" | "cclo" | "agentlo"
            | "tmux-run"
    ) {
        return;
    }
    let tail = match cli.command.as_mut() {
        Some(Command::Omlo(tail))
        | Some(Command::Pilo(tail))
        | Some(Command::Rpilo(tail))
        | Some(Command::Grolo(tail))
        | Some(Command::Hyperlo(tail))
        | Some(Command::Dolo(tail))
        | Some(Command::Colo(tail))
        | Some(Command::Cclo(tail))
        | Some(Command::Agentlo(tail))
        | Some(Command::TmuxRun(tail)) => tail,
        _ => return,
    };
    tail.argv = arguments[2..].to_vec();
}
#[derive(Debug, Subcommand)]
pub enum Command {
    Sessions(SessionsCli),
    Omlo(RawTail),
    Pilo(RawTail),
    Rpilo(RawTail),
    Grolo(RawTail),
    Hyperlo(RawTail),
    Dolo(RawTail),
    Colo(RawTail),
    Cclo(RawTail),
    Agentlo(RawTail),
    #[command(name = "tmux-run")]
    TmuxRun(RawTail),
    #[command(name = "__tmux-child", hide = true)]
    TmuxChild(TmuxChildArgs),
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct SessionsCli {
    #[command(subcommand)]
    pub command: Option<SessionsCommand>,
    #[command(flatten)]
    pub default_list: SessionListArgs,
}

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    List(SessionListArgs),
    Search(SessionSearchArgs),
    /// Search message bodies, then pick a session with fzf and open it.
    Query(SessionQueryArgs),
    #[command(visible_alias = "migrate")]
    Convert(SessionConvertArgs),
    /// Move sessions from one directory to another.
    Move(SessionMoveArgs),
    Fork(SessionForkArgs),
    Open(SessionOpenArgs),
    Sync(SessionSyncArgs),
}

#[derive(Debug, Args, Default, PartialEq, Eq)]
pub struct SessionListArgs {
    #[arg(id = "count")]
    pub count: Option<usize>,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub dedupe: bool,
    #[arg(
        long = "host",
        value_name = "HOST",
        value_parser = nonempty_host,
        conflicts_with_all = ["fzf", "paths", "picker"]
    )]
    pub hosts: Vec<String>,
    /// Interactively pick a session with fzf, choose a target, and open it.
    #[arg(long, group = "output")]
    pub fzf: bool,
    #[arg(long, group = "output")]
    pub paths: bool,
    #[arg(long, group = "output")]
    pub picker: bool,
}

#[derive(Debug, Args, PartialEq, Eq)]
pub struct SessionSearchArgs {
    #[arg(long)]
    pub dedupe: bool,
    #[arg(long)]
    pub picker: bool,
    #[arg(value_parser = nonempty_query)]
    pub query: String,
}

#[derive(Debug, Args, PartialEq, Eq)]
pub struct SessionConvertArgs {
    #[arg(value_parser = non_agent_source)]
    pub source_tool: SourceTool,
    #[arg(value_parser = non_agent_target)]
    pub target_tool: TargetTool,
    pub input: OsString,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args, PartialEq, Eq)]
pub struct SessionMoveArgs {
    pub from: PathBuf,
    pub to: PathBuf,
    #[arg(long = "tool", value_parser = non_agent_source)]
    pub tools: Vec<SourceTool>,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args, PartialEq, Eq)]
pub struct SessionForkArgs {
    #[arg(long)]
    pub print_command: bool,
    pub session_ref: OsString,
    #[arg(value_parser = non_agent_target)]
    pub target_tool: TargetTool,
}

#[derive(Debug, Args, PartialEq, Eq)]
pub struct SessionOpenArgs {
    #[arg(long)]
    pub print_command: bool,
    pub session_ref: OsString,
    #[arg(value_parser = parse_target_tool)]
    pub target_tool: TargetTool,
}

#[derive(Debug, Args, PartialEq, Eq)]
pub struct SessionSyncArgs {
    #[arg(required = true, num_args = 1..=2)]
    pub hosts: Vec<String>,
    #[arg(long = "tool", value_parser = non_agent_source)]
    pub tools: Vec<SourceTool>,
    #[arg(long)]
    pub dry_run: bool,
}


#[derive(Debug, Args, PartialEq, Eq)]
pub struct SessionQueryArgs {
    #[arg(required = true, num_args = 1.., value_parser = nonempty_query)]
    pub query: Vec<String>,
}

#[derive(Debug, Args, PartialEq, Eq)]
#[command(trailing_var_arg = true, disable_help_flag = true)]
pub struct RawTail {
    #[arg(value_name = "ARG", allow_hyphen_values = true, num_args = 0..)]
    pub argv: Vec<OsString>,
}

#[derive(Debug, Args, PartialEq, Eq)]
pub struct TmuxChildArgs {
    #[arg(long)]
    pub payload: PathBuf,
    #[arg(long)]
    pub ready: PathBuf,
}

fn parse_target_tool(value: &str) -> Result<TargetTool, String> {
    value.parse::<TargetTool>().map_err(|error| error.to_string())
}

fn non_agent_source(value: &str) -> Result<SourceTool, String> {
    let tool = value.parse::<SourceTool>().map_err(|error| error.to_string())?;
    if tool == SourceTool::Agent {
        Err("Agent sessions do not support conversion, move, or sync".to_owned())
    } else {
        Ok(tool)
    }
}

fn non_agent_target(value: &str) -> Result<TargetTool, String> {
    let tool = parse_target_tool(value)?;
    if tool == TargetTool::Agent {
        Err("Agent is not a conversion or fork target".to_owned())
    } else {
        Ok(tool)
    }
}


fn nonempty_query(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("query must not be empty".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

fn nonempty_host(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err("host must not be empty".to_owned())
    } else if trimmed != value {
        Err("host must not contain surrounding whitespace".to_owned())
    } else if value.starts_with('-') {
        Err("host must not start with '-'".to_owned())
    } else if value.chars().any(char::is_whitespace) {
        Err("host must not contain whitespace".to_owned())
    } else if value.chars().any(char::is_control) {
        Err("host must not contain control characters".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

pub fn run() -> anyhow::Result<()> {
    let cli = match Cli::try_parse_from(std::env::args_os()) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    match cli.command {
        None => {
            let help = Cli::command().render_help().to_string();
            write_stdout(&help)
        }
        Some(command) => dispatch(command),
    }
}

fn dispatch(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Sessions(sessions) => dispatch_sessions(sessions),
        Command::Omlo(args) => dispatch_launcher(crate::launcher::LauncherKind::Omp, args.argv),
        Command::Pilo(args) => dispatch_launcher(crate::launcher::LauncherKind::Pi, args.argv),
        Command::Rpilo(args) => dispatch_launcher(crate::launcher::LauncherKind::Rpi, args.argv),
        Command::Grolo(args) => dispatch_launcher(crate::launcher::LauncherKind::Grok, args.argv),
        Command::Hyperlo(args) => dispatch_launcher(crate::launcher::LauncherKind::Hyper, args.argv),
        Command::Dolo(args) => dispatch_launcher(crate::launcher::LauncherKind::Droid, args.argv),
        Command::Colo(args) => dispatch_launcher(crate::launcher::LauncherKind::Codex, args.argv),
        Command::Cclo(args) => dispatch_launcher(crate::launcher::LauncherKind::Claude, args.argv),
        Command::Agentlo(args) => dispatch_launcher(crate::launcher::LauncherKind::Agent, args.argv),
        Command::TmuxRun(args) => dispatch_tmux_run(args.argv),
        Command::TmuxChild(args) => dispatch_tmux_child(args),
    }
}

fn dispatch_sessions(sessions: SessionsCli) -> anyhow::Result<()> {
    match sessions.command {
        None => dispatch_list(sessions.default_list),
        Some(SessionsCommand::List(args)) => dispatch_list(args),
        Some(SessionsCommand::Search(args)) => dispatch_search(args),
        Some(SessionsCommand::Query(args)) => dispatch_picker(Some(args.query.join(" "))),
        Some(SessionsCommand::Convert(args)) => dispatch_convert(args),
        Some(SessionsCommand::Move(args)) => dispatch_move(args),
        Some(SessionsCommand::Fork(args)) => {
            dispatch_session_launch(args.session_ref, args.target_tool, args.print_command, true)
        }
        Some(SessionsCommand::Open(args)) => {
            dispatch_session_launch(args.session_ref, args.target_tool, args.print_command, false)
        }
        Some(SessionsCommand::Sync(args)) => dispatch_sync(args),
    }
}

fn dispatch_list(args: SessionListArgs) -> anyhow::Result<()> {
    if args.hosts.is_empty() {
        return dispatch_local_list(&args);
    }

    let remote_command = remote_session_list_command(&args);
    let mut failed = false;
    for host in &args.hosts {
        write_stdout_line(&format!("== {host} =="))?;
        if host == "local" {
            if dispatch_local_list(&args).is_err() {
                eprintln!("al: sessions list failed for host {host:?}");
                failed = true;
            }
        } else if !dispatch_remote_list(host, &remote_command)? {
            failed = true;
        }
    }

    if failed { exit_with(1) } else { Ok(()) }
}

fn dispatch_local_list(args: &SessionListArgs) -> anyhow::Result<()> {
    if args.fzf {
        return dispatch_picker(None);
    }
    let rows = crate::sessions::list_rows(&crate::sessions::ListOptions {
        count: args.count,
        show_all: args.all,
        dedupe: args.dedupe,
        tools: Vec::new(),
    })?;
    if args.paths {
        print_byte_lines(&crate::picker::render_paths_tsv(&rows))?;
    } else if args.picker {
        let use_color = crate::picker::use_color_for_picker();
        let lines = join_byte_lines(
            rows.iter()
                .filter_map(|row| crate::picker::format_picker_line(row, use_color)),
        );
        print_byte_lines(&lines)?;
    } else {
        let use_color = crate::picker::use_color_for_list();
        for row in rows {
            write_stdout_line(&crate::picker::format_row(&row, use_color))?;
        }
    }
    Ok(())
}

fn dispatch_remote_list(host: &str, remote_command: &str) -> anyhow::Result<bool> {
    let output = match std::process::Command::new("ssh")
        .args(["-o", "ConnectTimeout=10", "-o", "ConnectionAttempts=1", "--"])
        .arg(host)
        .arg(remote_command)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            eprintln!(
                "al: sessions list failed for host {host:?}: could not run ssh: {error}"
            );
            return Ok(false);
        }
    };
    if !output.status.success() {
        eprintln!(
            "al: sessions list failed for host {host:?}: ssh exited with {}",
            output.status
        );
        return Ok(false);
    }
    if !output.stdout.is_empty() {
        write_stdout_bytes(&output.stdout, !output.stdout.ends_with(b"\n"))?;
    }
    Ok(true)
}

fn remote_session_list_command(args: &SessionListArgs) -> String {
    let mut command = String::from("exec 'al' 'sessions' 'list'");
    if let Some(count) = args.count {
        command.push(' ');
        command.push_str(&shell_quote(&count.to_string()));
    }
    if args.all {
        command.push_str(" '--all'");
    }
    if args.dedupe {
        command.push_str(" '--dedupe'");
    }
    command
}

fn shell_quote(value: &str) -> String {
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

fn dispatch_search(args: SessionSearchArgs) -> anyhow::Result<()> {
    let rows = crate::sessions::search_rows(
        &args.query,
        &crate::sessions::SearchOptions {
            dedupe: args.dedupe,
            tools: Vec::new(),
        },
    )?;
    if args.picker {
        let use_color = crate::picker::use_color_for_picker();
        let lines = join_byte_lines(
            rows.iter()
                .filter_map(|row| crate::picker::format_picker_line(row, use_color)),
        );
        print_byte_lines(&lines)?;
    } else {
        let use_color = crate::picker::use_color_for_list();
        for row in rows {
            write_stdout_line(&crate::picker::format_row(&row, use_color))?;
        }
    }
    Ok(())
}

fn dispatch_sync(args: SessionSyncArgs) -> anyhow::Result<()> {
    let (source, destination) = match args.hosts.as_slice() {
        [destination] => (
            crate::sync::Endpoint::Local,
            crate::sync::Endpoint::from_host(destination.clone()),
        ),
        [source, destination] => (
            crate::sync::Endpoint::from_host(source.clone()),
            crate::sync::Endpoint::from_host(destination.clone()),
        ),
        _ => unreachable!("Clap enforces one or two sync endpoints"),
    };
    let report = crate::sync::sync_default(
        &source,
        &destination,
        &crate::sync::SyncOptions {
            tools: args.tools,
            dry_run: args.dry_run,
        },
        &sessions_home()?,
    )?;
    for message in &report.messages {
        write_stdout_line(message)?;
    }
    exit_with(report.exit_code())
}


fn dispatch_launcher(
    kind: crate::launcher::LauncherKind,
    argv: Vec<OsString>,
) -> anyhow::Result<()> {
    let home = home_dir()?;
    let cwd = std::env::current_dir()?;
    let plan = match crate::launcher::build_launcher(kind, &argv, &home, &cwd) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("al: {error}");
            return exit_with(error.exit_code());
        }
    };
    match crate::launcher::execute_plan(&plan) {
        Ok(code) => exit_with(code),
        Err(error) => {
            eprintln!("al: {error}");
            exit_with(error.exit_code())
        }
    }
}

fn join_byte_lines(lines: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut joined = Vec::new();
    for line in lines {
        if !joined.is_empty() {
            joined.push(b'\n');
        }
        joined.extend_from_slice(&line);
    }
    joined
}

fn print_byte_lines(lines: &[u8]) -> anyhow::Result<()> {
    if lines.is_empty() {
        Ok(())
    } else {
        write_stdout_bytes(lines, true)
    }
}


fn write_stdout(value: &str) -> anyhow::Result<()> {
    write_stdout_bytes(value.as_bytes(), false)
}

fn write_stdout_bytes(value: &[u8], newline: bool) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    if let Err(error) = stdout.write_all(value) {
        return if error.kind() == io::ErrorKind::BrokenPipe {
            Ok(())
        } else {
            Err(error.into())
        };
    }
    if !newline {
        return Ok(());
    }
    match stdout.write_all(b"\n") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_stdout_line(value: &str) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    if let Err(error) = stdout.write_all(value.as_bytes()) {
        return if error.kind() == io::ErrorKind::BrokenPipe {
            Ok(())
        } else {
            Err(error.into())
        };
    }
    match stdout.write_all(b"\n") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn home_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME");
    #[cfg(windows)]
    let user_profile = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let user_profile = None;
    home_dir_from(home, user_profile)
}

fn home_dir_from(
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> anyhow::Result<PathBuf> {
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    #[cfg(windows)]
    if let Some(user_profile) = user_profile.filter(|home| !home.is_empty()) {
        return Ok(PathBuf::from(user_profile));
    }
    #[cfg(windows)]
    anyhow::bail!("HOME and USERPROFILE are not set");
    #[cfg(not(windows))]
    {
        let _ = user_profile;
        anyhow::bail!("HOME is not set");
    }
}

fn sessions_home() -> anyhow::Result<PathBuf> {
    Ok(std::env::var_os("SESSIONS_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or(home_dir()?))
}


fn exit_with(code: i32) -> anyhow::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code.clamp(1, 255));
    }
}

fn dispatch_convert(args: SessionConvertArgs) -> anyhow::Result<()> {
    let home = sessions_home()?;
    let session = crate::sessions::resolve_input_session(args.source_tool, &args.input)?;
    let emitted = match args.output {
        Some(output) => crate::emit::emit_to_default(&session, args.target_tool, &home, &output)?,
        None => crate::emit::emit_default(&session, args.target_tool, &home)?,
    };
    write_stdout_line(&emitted.path.display().to_string())
}

fn dispatch_move(args: SessionMoveArgs) -> anyhow::Result<()> {
    let catalog = crate::sessions::Catalog::from_env()?;
    let moved = crate::relocate::move_sessions(
        &catalog,
        &crate::relocate::MoveOptions {
            from: args.from,
            to: args.to,
            tools: args.tools,
            dry_run: args.dry_run,
        },
    )?;
    for item in moved {
        write_stdout_line(&item.destination.display().to_string())?;
    }
    Ok(())
}

fn dispatch_session_launch(
    session_ref: OsString,
    target: TargetTool,
    print_command: bool,
    fork: bool,
) -> anyhow::Result<()> {
    let home = home_dir()?;
    let session = crate::sessions::resolve_any_session(&session_ref)?;
    if session.tool == SourceTool::Agent && (fork || target != TargetTool::Agent) {
        eprintln!("al: Agent sessions can only be reopened with target agent");
        return exit_with(2);
    }
    if target == TargetTool::Agent && session.tool != SourceTool::Agent {
        eprintln!("al: target agent only supports native Agent sessions");
        return exit_with(2);
    }
    let same_format = target.source() == Some(session.tool)
        || (session.tool == SourceTool::Grok && target == TargetTool::Hyper)
        || (session.tool.uses_pi_jsonl() && target.uses_pi_storage());

    let (path, session_id, created_output) = prepare_session_launch(
        &session,
        target,
        same_format,
        print_command,
        fork,
        &home,
    )?;

    let mut plan = if fork && !same_format {
        if target == TargetTool::Claude {
            crate::launcher::LaunchPlan::Command(crate::launcher::native_resume(
                target,
                &path,
                &session_id,
                &home,
            ))
        } else {
            let kind = launcher_kind_for_target(target);
            let args = match target {
                TargetTool::Droid => vec![OsString::from(&session_id)],
                TargetTool::Pi | TargetTool::Rpi | TargetTool::Omp => vec![
                    OsString::from("--session"),
                    path.as_os_str().to_owned(),
                ],
                TargetTool::Codex | TargetTool::Grok | TargetTool::Hyper => vec![
                    OsString::from("--session"),
                    OsString::from(&session_id),
                ],
                TargetTool::Claude => unreachable!(),
                TargetTool::Agent => unreachable!("Agent cross-format launch rejected"),
            };
            crate::launcher::build_launcher(
                kind,
                &args,
                &home,
                &std::env::current_dir()?,
            )?
        }
    } else {
        let command = if fork {
            crate::launcher::native_fork(target, &path, &session_id, &home)
        } else {
            crate::launcher::native_resume(target, &path, &session_id, &home)
        };
        crate::launcher::LaunchPlan::Command(command)
    };

    if let crate::launcher::LaunchPlan::Command(command) = &mut plan {
        command.cwd = launch_cwd(&session.cwd);
    }
    if print_command {
        let command = plan_command(&plan)?;
        if fork {
            if let Some(path) = created_output {
                write_stdout_line(&path.display().to_string())?;
            }
        }
        write_stdout_line(&crate::launcher::render_command(command)?)?;
        return Ok(());
    }
    match crate::launcher::execute_plan(&plan) {
        Ok(code) => exit_with(code),
        Err(error) => {
            eprintln!("al: {error}");
            exit_with(1)
        }
    }
}

fn prepare_session_launch(
    session: &crate::domain::Session,
    target: TargetTool,
    same_format: bool,
    print_command: bool,
    fork: bool,
    home: &std::path::Path,
) -> anyhow::Result<(PathBuf, String, Option<PathBuf>)> {
    let rematerialize_legacy = same_format
        && !print_command
        && match session.tool {
            SourceTool::Omp => crate::migrate::needs_legacy_omp_conversion(&session.path)?,
            SourceTool::Codex if !fork => {
                crate::migrate::needs_legacy_codex_conversion(&session.path)?
            }
            _ => false,
        };

    if same_format && !rematerialize_legacy {
        return Ok((session.path.clone(), session.session_id.clone(), None));
    }
    if !fork {
        crate::launcher::resolve_tool_executable(target, home)
            .map_err(|error| anyhow::anyhow!(error))?;
    }
    let emitted = crate::emit::emit_default(session, target, &sessions_home()?)?;
    let created_output = emitted.path.clone();
    Ok((emitted.path, emitted.session_id, Some(created_output)))
}
fn dispatch_picker(query: Option<String>) -> anyhow::Result<()> {
    let is_search = query.is_some();
    let rows = match query {
        Some(query) => crate::sessions::search_rows(
            &query,
            &crate::sessions::SearchOptions {
                dedupe: true,
                tools: Vec::new(),
            },
        )?,
        None => crate::sessions::list_rows(&crate::sessions::ListOptions {
            count: None,
            show_all: true,
            dedupe: true,
            tools: Vec::new(),
        })?,
    };
    let prompt = if is_search { "search> " } else { "sessions> " };
    let (source, path) = match crate::picker::select_session(rows, None, true, false, prompt)? {
        crate::picker::SessionOutcome::Selected { source, path } => (source, path),
        crate::picker::SessionOutcome::Cancelled => return exit_with(1),
        crate::picker::SessionOutcome::Error(code) => return exit_with(code),
    };
    let targets = crate::picker::target_tools_for_source(source);
    let target = match crate::picker::pick_target_tool(&targets)? {
        crate::picker::TargetOutcome::Selected(target) => target,
        crate::picker::TargetOutcome::Cancelled => return exit_with(1),
        crate::picker::TargetOutcome::Error(code) => return exit_with(code),
    };
    dispatch_session_launch(path.into_os_string(), target, false, false)
}

fn launcher_kind_for_target(target: TargetTool) -> crate::launcher::LauncherKind {
    match target {
        TargetTool::Pi => crate::launcher::LauncherKind::Pi,
        TargetTool::Rpi => crate::launcher::LauncherKind::Rpi,
        TargetTool::Omp => crate::launcher::LauncherKind::Omp,
        TargetTool::Droid => crate::launcher::LauncherKind::Droid,
        TargetTool::Codex => crate::launcher::LauncherKind::Codex,
        TargetTool::Claude => crate::launcher::LauncherKind::Claude,
        TargetTool::Grok => crate::launcher::LauncherKind::Grok,
        TargetTool::Hyper => crate::launcher::LauncherKind::Hyper,
        TargetTool::Agent => crate::launcher::LauncherKind::Agent,
    }
}

fn launch_cwd(recorded: &std::path::Path) -> Option<PathBuf> {
    let cwd = crate::launcher::local_recorded_cwd(recorded);
    if cwd.is_none() && !recorded.as_os_str().is_empty() {
        eprintln!(
            "warning: recorded session cwd does not exist; inheriting current directory: {}",
            recorded.display()
        );
    }
    cwd
}

fn plan_command(plan: &crate::launcher::LaunchPlan) -> anyhow::Result<&crate::launcher::CommandSpec> {
    match plan {
        crate::launcher::LaunchPlan::Command(command) => Ok(command),
        crate::launcher::LaunchPlan::Fallback { primary, .. } => Ok(primary),
        crate::launcher::LaunchPlan::Tmux { command, .. } => Ok(command),
        crate::launcher::LaunchPlan::Remote { .. } => {
            anyhow::bail!("cannot print a remote launcher command for a local session")
        }
    }
}

fn dispatch_tmux_run(argv: Vec<OsString>) -> anyhow::Result<()> {
    match crate::tmux::run_argv(&argv) {
        Ok(code) => exit_with(code),
        Err(error) if tmux_usage_error(&error) => {
            eprintln!("al: {error:#}");
            exit_with(2)
        }
        Err(error) => Err(error),
    }
}

fn dispatch_tmux_child(args: TmuxChildArgs) -> anyhow::Result<()> {
    exit_with(crate::tmux::run_child(&args.payload, &args.ready)?)
}

fn tmux_usage_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("requires a value")
        || message.contains("mutually exclusive")
        || message.contains("--fresh cannot be combined with --no-attach")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use clap::CommandFactory;

    use super::{Cli, Command, SessionsCommand, home_dir_from};
    use crate::domain::{SourceTool, TargetTool};

    #[test]
    fn home_selection_prefers_home_and_rejects_missing_values() {
        assert_eq!(
            home_dir_from(Some(OsString::from("/workspace/primary")), None).unwrap(),
            std::path::PathBuf::from("/workspace/primary")
        );
        assert!(home_dir_from(None, None).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn home_selection_falls_back_to_userprofile_on_windows() {
        assert_eq!(
            home_dir_from(None, Some(OsString::from(r"<workspace>\user"))).unwrap(),
            std::path::PathBuf::from(r"<workspace>\user")
        );
    }

    #[test]
    fn bare_root_and_sessions_parse_as_defaults() {
        let root = Cli::try_parse_from(["al"]).unwrap();
        assert!(root.command.is_none());

        let sessions = Cli::try_parse_from(["al", "sessions"]).unwrap();
        let Some(Command::Sessions(sessions)) = sessions.command else {
            panic!("expected sessions command");
        };
        assert!(sessions.command.is_none());
        assert_eq!(sessions.default_list, Default::default());
    }

    #[test]
    fn sessions_list_supports_default_and_explicit_spellings() {
        let default = Cli::try_parse_from(["al", "sessions", "12", "--dedupe", "--paths"])
            .unwrap();
        let Some(Command::Sessions(default)) = default.command else {
            panic!("expected sessions command");
        };
        assert_eq!(default.default_list.count, Some(12));
        assert!(default.default_list.dedupe);
        assert!(default.default_list.paths);
        assert!(default.default_list.hosts.is_empty());

        let explicit = Cli::try_parse_from([
            "al", "sessions", "list", "7", "--all", "--picker",
        ])
        .unwrap();
        let Some(Command::Sessions(explicit)) = explicit.command else {
            panic!("expected sessions command");
        };
        let Some(SessionsCommand::List(list)) = explicit.command else {
            panic!("expected list command");
        };
        assert_eq!(list.count, Some(7));
        assert!(list.all);
        assert!(list.picker);
    }

    #[test]
    fn sessions_list_accepts_repeatable_validated_hosts() {
        let default = Cli::try_parse_from([
            "al",
            "sessions",
            "12",
            "--all",
            "--dedupe",
            "--host",
            "host-a",
            "--host=local",
        ])
        .unwrap();
        let Some(Command::Sessions(default)) = default.command else {
            panic!("expected sessions command");
        };
        assert_eq!(default.default_list.count, Some(12));
        assert_eq!(default.default_list.hosts, ["host-a", "local"]);

        let explicit = Cli::try_parse_from([
            "al",
            "sessions",
            "list",
            "7",
            "--host",
            "host-b;literal",
        ])
        .unwrap();
        let Some(Command::Sessions(explicit)) = explicit.command else {
            panic!("expected sessions command");
        };
        let Some(SessionsCommand::List(list)) = explicit.command else {
            panic!("expected list command");
        };
        assert_eq!(list.count, Some(7));
        assert_eq!(list.hosts, ["host-b;literal"]);

        for argv in [
            vec!["al", "sessions", "--host="],
            vec!["al", "sessions", "--host=-host-a"],
            vec!["al", "sessions", "--host", "   "],
            vec!["al", "sessions", "list", "--host", " -host-a"],
            vec!["al", "sessions", "--host", "host-a "],
            vec!["al", "sessions", "--host", "host a"],
            vec!["al", "sessions", "--host", "host-a\nspoof"],
            vec!["al", "sessions", "--host", "host-a\u{1b}"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err());
        }
    }

    #[test]
    fn sessions_default_args_conflict_with_subcommands() {
        assert!(Cli::try_parse_from(["al", "sessions", "--all", "list"]).is_err());
    }

    #[test]
    fn list_output_modes_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["al", "sessions", "list", "--fzf", "--paths"]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "--picker", "--paths"]).is_err());
    }

    #[test]
    fn remote_hosts_conflict_with_interactive_and_path_modes() {
        for mode in ["--paths", "--picker", "--fzf"] {
            assert!(Cli::try_parse_from(["al", "sessions", "--host", "host-a", mode]).is_err());
            assert!(
                Cli::try_parse_from(["al", "sessions", "list", "--host", "host-a", mode])
                    .is_err()
            );
        }
    }

    #[test]
    fn search_is_local_and_requires_a_nonempty_query() {
        let parsed = Cli::try_parse_from([
            "al", "sessions", "search", "--dedupe", "--picker", "Needle",
        ])
        .unwrap();
        let Some(Command::Sessions(sessions)) = parsed.command else {
            panic!("expected sessions command");
        };
        let Some(SessionsCommand::Search(search)) = sessions.command else {
            panic!("expected search command");
        };
        assert!(search.dedupe);
        assert!(search.picker);
        assert_eq!(search.query, "Needle");

        assert!(Cli::try_parse_from(["al", "sessions", "search", "   "]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "search", "--host", "host-a", "q"]).is_err());
    }

    #[test]
    fn convert_and_migrate_compatibility_spellings_match() {
        for spelling in ["convert", "migrate"] {
            let parsed = Cli::try_parse_from([
                "al", "sessions", spelling, "omp", "hyper", "session.jsonl", "output.jsonl",
            ])
            .unwrap();
            let Some(Command::Sessions(sessions)) = parsed.command else {
                panic!("expected sessions command");
            };
            let Some(SessionsCommand::Convert(convert)) = sessions.command else {
                panic!("expected convert command");
            };
            assert_eq!(convert.source_tool, SourceTool::Omp);
            assert_eq!(convert.target_tool, TargetTool::Hyper);
            assert_eq!(convert.input, OsString::from("session.jsonl"));
            assert_eq!(convert.output.unwrap(), std::path::PathBuf::from("output.jsonl"));
        }
    }

    #[test]
    fn move_takes_from_and_to_directories() {
        let parsed = Cli::try_parse_from([
            "al",
            "sessions",
            "move",
            "/old/project",
            "/new/project",
            "--tool",
            "pi",
            "--dry-run",
        ])
        .unwrap();
        let Some(Command::Sessions(sessions)) = parsed.command else {
            panic!("expected sessions command");
        };
        let Some(SessionsCommand::Move(moved)) = sessions.command else {
            panic!("expected move command");
        };
        assert_eq!(moved.from, std::path::PathBuf::from("/old/project"));
        assert_eq!(moved.to, std::path::PathBuf::from("/new/project"));
        assert_eq!(moved.tools, [SourceTool::Pi]);
        assert!(moved.dry_run);
        assert!(Cli::try_parse_from(["al", "sessions", "move", "/only-one"]).is_err());
        let rpi = Cli::try_parse_from([
            "al",
            "sessions",
            "move",
            "/old",
            "/new",
            "--tool",
            "rpi",
        ])
        .unwrap();
        let Some(Command::Sessions(sessions)) = rpi.command else {
            panic!("expected sessions command");
        };
        let Some(SessionsCommand::Move(moved)) = sessions.command else {
            panic!("expected move command");
        };
        assert_eq!(moved.tools, [SourceTool::Rpi]);
    }

    #[test]
    fn agent_is_open_only_on_the_cli_surface() {
        assert!(Cli::try_parse_from(["al", "sessions", "convert", "agent", "omp", "id"]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "convert", "omp", "agent", "id"]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "move", "/old", "/new", "--tool", "agent"]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "fork", "id", "agent"]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "sync", "host", "--tool", "agent"]).is_err());

        let parsed = Cli::try_parse_from(["al", "sessions", "open", "id", "agent"]).unwrap();
        let Some(Command::Sessions(sessions)) = parsed.command else { panic!("expected sessions") };
        let Some(SessionsCommand::Open(open)) = sessions.command else { panic!("expected open") };
        assert_eq!(open.target_tool, TargetTool::Agent);
    }

    #[test]
    fn open_and_fork_preserve_session_ref_as_os_string() {
        for command in ["open", "fork"] {
            let parsed = Cli::try_parse_from([
                "al", "sessions", command, "--print-command", "session-ref", "claude",
            ])
            .unwrap();
            let Some(Command::Sessions(sessions)) = parsed.command else {
                panic!("expected sessions command");
            };
            match sessions.command.unwrap() {
                SessionsCommand::Open(args) => {
                    assert!(args.print_command);
                    assert_eq!(args.session_ref, OsString::from("session-ref"));
                    assert_eq!(args.target_tool, TargetTool::Claude);
                }
                SessionsCommand::Fork(args) => {
                    assert!(args.print_command);
                    assert_eq!(args.session_ref, OsString::from("session-ref"));
                    assert_eq!(args.target_tool, TargetTool::Claude);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn sync_accepts_exactly_one_or_two_hosts() {
        for hosts in [&["host-a"][..], &["host-a", "local"][..]] {
            let mut argv = vec!["al", "sessions", "sync"];
            argv.extend(hosts);
            argv.extend(["--tool", "omp", "--tool", "claude", "--dry-run"]);
            let parsed = Cli::try_parse_from(argv).unwrap();
            let Some(Command::Sessions(sessions)) = parsed.command else {
                panic!("expected sessions command");
            };
            let Some(SessionsCommand::Sync(sync)) = sessions.command else {
                panic!("expected sync command");
            };
            assert_eq!(sync.hosts, hosts);
            assert_eq!(sync.tools, [SourceTool::Omp, SourceTool::Claude]);
            assert!(sync.dry_run);
        }

        assert!(Cli::try_parse_from(["al", "sessions", "sync"]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "sync", "host-a", "host-b", "host-c"]).is_err());
    }

    #[test]
    fn removed_sessions_all_spellings_are_rejected_and_hidden_from_help() {
        assert!(Cli::try_parse_from(["al", "sessions-all"]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "all"]).is_err());

        let mut root = Cli::command();
        let root_help = root.render_long_help().to_string();
        assert!(!root_help.contains("sessions-all"));
        let sessions_help = root
            .find_subcommand_mut("sessions")
            .expect("sessions command")
            .render_long_help()
            .to_string();
        assert!(!sessions_help.contains("  all"));
    }

    #[test]
    fn compatibility_command_spellings_parse() {
        let cases = [
            "omlo", "pilo", "rpilo", "grolo", "hyperlo", "dolo", "colo", "cclo", "agentlo",
            "tmux-run",
        ];
        for spelling in cases {
            let parsed = Cli::try_parse_from(["al", spelling, "--unknown", "value"]).unwrap();
            let argv = match parsed.command.unwrap() {
                Command::Omlo(tail)
                | Command::Pilo(tail)
                | Command::Rpilo(tail)
                | Command::Grolo(tail)
                | Command::Hyperlo(tail)
                | Command::Dolo(tail)
                | Command::Colo(tail)
                | Command::Cclo(tail)
                | Command::Agentlo(tail)
                | Command::TmuxRun(tail) => tail.argv,
                other => panic!("unexpected command: {other:?}"),
            };
            assert_eq!(argv, [OsString::from("--unknown"), OsString::from("value")]);
        }
    }

    #[test]
    fn raw_tail_restoration_does_not_match_program_name() {
        let parsed = Cli::try_parse_from(["/tmp/omlo", "sessions", "list", "--paths"])
            .unwrap();
        let Some(Command::Sessions(sessions)) = parsed.command else {
            panic!("expected sessions command");
        };
        assert!(matches!(sessions.command, Some(SessionsCommand::List(_))));
    }

    #[test]
    fn launcher_tail_preserves_stop_parsing_delimiter() {
        let parsed = Cli::try_parse_from(["al", "omlo", "--", "--host", "tool-host"])
            .unwrap();
        let Some(Command::Omlo(tail)) = parsed.command else {
            panic!("expected omlo command");
        };
        assert_eq!(
            tail.argv,
            [
                OsString::from("--"),
                OsString::from("--host"),
                OsString::from("tool-host"),
            ]
        );
    }

    #[test]
    fn launcher_help_is_forwarded_but_top_level_help_is_available() {
        let parsed = Cli::try_parse_from(["al", "omlo", "--help"]).unwrap();
        let Some(Command::Omlo(tail)) = parsed.command else {
            panic!("expected omlo command");
        };
        assert_eq!(tail.argv, [OsString::from("--help")]);

        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("omlo")
            .expect("omlo command")
            .render_help()
            .to_string();
        assert!(help.contains("Usage: omlo"));
    }

    #[test]
    fn query_joins_words_and_rejects_empty() {
        let parsed = Cli::try_parse_from(["al", "sessions", "query", "one", "two"]).unwrap();
        let Some(Command::Sessions(sessions)) = parsed.command else {
            panic!("expected sessions command");
        };
        let Some(SessionsCommand::Query(args)) = sessions.command else {
            panic!("expected query command");
        };
        assert_eq!(args.query, ["one", "two"]);
        assert!(Cli::try_parse_from(["al", "sessions", "query"]).is_err());
        assert!(Cli::try_parse_from(["al", "sessions", "query", "   "]).is_err());
    }

    #[test]
    fn removed_sks_spellings_are_rejected_and_hidden_from_help() {
        assert!(Cli::try_parse_from(["al", "sks"]).is_err());
        assert!(Cli::try_parse_from(["al", "skss", "needle"]).is_err());

        let mut root = Cli::command();
        assert!(root.find_subcommand("sks").is_none());
        assert!(root.find_subcommand("skss").is_none());
        let help = root.render_long_help().to_string();
        assert!(!help.contains("  sks"));
        assert!(!help.contains("  skss"));
    }

    #[test]
    fn hidden_tmux_child_parses_but_is_absent_from_root_help() {
        let parsed = Cli::try_parse_from([
            "al", "__tmux-child", "--payload", "/tmp/payload", "--ready", "/tmp/ready",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Some(Command::TmuxChild(_))));

        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("__tmux-child"));
    }

    #[cfg(unix)]
    #[test]
    fn raw_launcher_tail_preserves_non_utf8_argv() {
        use std::os::unix::ffi::OsStringExt;

        let raw = OsString::from_vec(vec![b'a', 0xff, b'z']);
        let parsed = Cli::try_parse_from([
            OsString::from("al"),
            OsString::from("pilo"),
            raw.clone(),
        ])
        .unwrap();
        let Some(Command::Pilo(tail)) = parsed.command else {
            panic!("expected pilo command");
        };
        assert_eq!(tail.argv, [raw]);
    }
}
