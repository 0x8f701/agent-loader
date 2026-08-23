//! Explicit point-to-point session sync.
//!
//! Local work uses filesystem APIs and exact `OsString` argv. SSH is the only shell boundary:
//! fixed Bash programs are assembled from independently single-quoted path tokens, then passed as
//! one SSH remote-command argument. Tests inject [`CommandExecutor`] and never touch the network.

use std::collections::{BTreeSet, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

use serde_json::Value;
use tempfile::TempDir;
use thiserror::Error;
use walkdir::WalkDir;

use crate::domain::SourceTool;
use crate::sessions::Catalog;

const TRANSFER_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Endpoint {
    Local,
    Remote(String),
}

impl Endpoint {
    pub fn from_host(value: impl Into<String>) -> Self {
        let value = value.into();
        if matches!(value.as_str(), "" | "." | "local" | "localhost" | "this" | "self") {
            Self::Local
        } else {
            Self::Remote(value)
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Local => "localhost",
            Self::Remote(host) => host,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOptions {
    /// Empty means all six source tools. Duplicates are ignored.
    pub tools: Vec<SourceTool>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    Success,
    PartialFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSyncStatus {
    SourceAbsent,
    WouldCopy,
    NothingToDo,
    Copied,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSyncReport {
    pub tool: SourceTool,
    pub root: PathBuf,
    pub status: ToolSyncStatus,
    pub missing_files: usize,
    pub transferred_files: usize,
    pub codex_history_added: Option<usize>,
    pub messages: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub source: Endpoint,
    pub destination: Endpoint,
    pub status: SyncStatus,
    pub tools: Vec<ToolSyncReport>,
    pub messages: Vec<String>,
}

impl SyncReport {
    pub fn is_success(&self) -> bool {
        self.status == SyncStatus::Success
    }

    pub fn success_count(&self) -> usize {
        self.tools.len().saturating_sub(self.failure_count())
    }

    pub fn failure_count(&self) -> usize {
        self.tools
            .iter()
            .filter(|report| report.status == ToolSyncStatus::Failed)
            .count()
    }

    pub fn exit_code(&self) -> i32 {
        if self.is_success() { 0 } else { 1 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
}

impl CommandSpec {
    fn new(program: impl Into<OsString>, args: Vec<OsString>) -> Self {
        Self {
            program: program.into(),
            args,
            cwd: None,
            stdin: None,
        }
    }

    fn with_stdin(mut self, stdin: Vec<u8>) -> Self {
        self.stdin = Some(stdin);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn success() -> Self {
        Self {
            status: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineOutput {
    pub producer_status: i32,
    pub consumer_status: i32,
    pub producer_stderr: Vec<u8>,
    pub consumer_stdout: Vec<u8>,
    pub consumer_stderr: Vec<u8>,
}

impl PipelineOutput {
    pub fn success() -> Self {
        Self {
            producer_status: 0,
            consumer_status: 0,
            producer_stderr: Vec::new(),
            consumer_stdout: Vec::new(),
            consumer_stderr: Vec::new(),
        }
    }
}

/// Injectable seam for every external command used by sync.
pub trait CommandExecutor {
    fn program_exists(&mut self, program: &OsStr) -> bool;
    fn execute(&mut self, command: &CommandSpec) -> io::Result<CommandOutput>;
    fn pipeline(
        &mut self,
        producer: &CommandSpec,
        consumer: &CommandSpec,
    ) -> io::Result<PipelineOutput>;
}

#[derive(Debug, Default)]
pub struct SystemCommandExecutor;

impl CommandExecutor for SystemCommandExecutor {
    fn program_exists(&mut self, program: &OsStr) -> bool {
        resolve_program(program).is_some()
    }

    fn execute(&mut self, command: &CommandSpec) -> io::Result<CommandOutput> {
        let mut process = process_command(command);
        process.stdout(Stdio::piped()).stderr(Stdio::piped());
        process.stdin(if command.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = process.spawn()?;
        let writer = command.stdin.clone().map(|input| {
            let mut stdin = child.stdin.take().expect("piped stdin");
            thread::spawn(move || stdin.write_all(&input))
        });
        let output = child.wait_with_output()?;
        if let Some(writer) = writer {
            match writer.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(io::Error::other("stdin writer thread panicked")),
            }
        }
        Ok(CommandOutput {
            status: exit_code(output.status),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn pipeline(
        &mut self,
        producer: &CommandSpec,
        consumer: &CommandSpec,
    ) -> io::Result<PipelineOutput> {
        if producer.stdin.is_some() || consumer.stdin.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pipeline commands cannot define byte stdin",
            ));
        }

        let mut producer_process = process_command(producer);
        producer_process
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut producer_child = producer_process.spawn()?;
        let producer_stdout = producer_child.stdout.take().expect("piped producer stdout");

        let mut consumer_process = process_command(consumer);
        consumer_process
            .stdin(Stdio::from(producer_stdout))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let consumer_child = match consumer_process.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = producer_child.kill();
                let _ = producer_child.wait();
                return Err(error);
            }
        };

        let producer_wait = thread::spawn(move || producer_child.wait_with_output());
        let consumer_output = consumer_child.wait_with_output()?;
        let producer_output = producer_wait
            .join()
            .map_err(|_| io::Error::other("producer wait thread panicked"))??;
        Ok(PipelineOutput {
            producer_status: exit_code(producer_output.status),
            consumer_status: exit_code(consumer_output.status),
            producer_stderr: producer_output.stderr,
            consumer_stdout: consumer_output.stdout,
            consumer_stderr: consumer_output.stderr,
        })
    }
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("source and destination cannot both be local")]
    BothEndpointsLocal,
    #[error("remote endpoint host must not be empty")]
    EmptyRemoteHost,
    #[error("remote endpoint host contains a NUL byte")]
    InvalidRemoteHost,
    #[error("rsync not found in PATH")]
    MissingRsync,
    #[error("invalid synchronized relative path {path:?}: {reason}")]
    InvalidRelativePath { path: PathBuf, reason: &'static str },
    #[error("{operation} {path}: {source}")]
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to start {operation}: {source}")]
    Spawn {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{operation} failed (exit {status}){detail}")]
    CommandFailed {
        operation: &'static str,
        status: i32,
        detail: String,
    },
    #[error("{operation} pipeline failed (producer exit {producer_status}, consumer exit {consumer_status}){detail}")]
    PipelineFailed {
        operation: &'static str,
        producer_status: i32,
        consumer_status: i32,
        detail: String,
    },
    #[error("remote inventory for {tool} contained an invalid path {path:?}")]
    InvalidInventory { tool: SourceTool, path: PathBuf },
    #[error("staged remote archive did not contain exactly the requested regular files")]
    UnsafeArchive,
    #[error("refusing unsafe Codex history path {0}")]
    UnsafeHistory(PathBuf),
    #[error("refusing unsafe session root {0}")]
    UnsafeRoot(PathBuf),
    #[error("remote path cannot be represented safely: {0:?}")]
    UnsupportedRemotePath(OsString),
}

pub type Result<T, E = SyncError> = std::result::Result<T, E>;

/// Direction-neutral point-to-point sync.
///
/// Endpoint/preflight errors are returned. Per-tool failures are accumulated so later tools still
/// run, without hiding partial failure.
pub fn sync<E: CommandExecutor>(
    src: &Endpoint,
    dst: &Endpoint,
    options: &SyncOptions,
    home: &Path,
    executor: &mut E,
) -> Result<SyncReport> {
    validate_request(src, dst)?;
    let selected = selected_tools(&options.tools);
    let rsync_available = !src.is_local()
        && !dst.is_local()
        || executor.program_exists(OsStr::new("rsync"));
    let catalog = Catalog::new(home);
    let mut tools = Vec::new();
    let mut messages = Vec::new();

    for tool in selected {
        let root = catalog.root_for_tool(tool).path;
        let report = if !rsync_available && tool != SourceTool::Codex {
            Err(SyncError::MissingRsync)
        } else {
            sync_tool(src, dst, options, tool, root.clone(), home, executor)
        };
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                let message = format!(
                    "failed to sync {tool} sessions from {} to {}: {error}",
                    src.label(),
                    dst.label()
                );
                ToolSyncReport {
                    tool,
                    root,
                    status: ToolSyncStatus::Failed,
                    missing_files: 0,
                    transferred_files: 0,
                    codex_history_added: None,
                    messages: vec![message.clone()],
                    error: Some(error.to_string()),
                }
            }
        };
        messages.extend(report.messages.iter().cloned());
        tools.push(report);
    }

    let status = if tools
        .iter()
        .any(|report| report.status == ToolSyncStatus::Failed)
    {
        SyncStatus::PartialFailure
    } else {
        SyncStatus::Success
    };
    Ok(SyncReport {
        source: src.clone(),
        destination: dst.clone(),
        status,
        tools,
        messages,
    })
}

pub fn sync_default(
    src: &Endpoint,
    dst: &Endpoint,
    options: &SyncOptions,
    home: &Path,
) -> Result<SyncReport> {
    sync(src, dst, options, home, &mut SystemCommandExecutor)
}

fn sync_tool<E: CommandExecutor>(
    src: &Endpoint,
    dst: &Endpoint,
    options: &SyncOptions,
    tool: SourceTool,
    root: PathBuf,
    home: &Path,
    executor: &mut E,
) -> Result<ToolSyncReport> {
    let source_root_exists = directory_exists(src, &root, executor)?;
    let mut messages = Vec::new();
    if !source_root_exists {
        let message = match src {
            Endpoint::Local => format!(
                "skip {tool}: local session root does not exist: {}",
                root.display()
            ),
            Endpoint::Remote(host) => format!(
                "skip {tool}: source session root does not exist on {host}: {}",
                root.display()
            ),
        };
        if tool != SourceTool::Codex {
            return Ok(ToolSyncReport {
                tool,
                root,
                status: ToolSyncStatus::SourceAbsent,
                missing_files: 0,
                transferred_files: 0,
                codex_history_added: None,
                messages: vec![message],
                error: None,
            });
        }
        messages.push(message);
    }

    let (missing, transferred_files, session_status) = if source_root_exists {
        let destination_exists = directory_exists(dst, &root, executor)?;
        if !destination_exists {
            if options.dry_run {
                messages.push(match dst {
                    Endpoint::Local => format!("would create local directory {}", root.display()),
                    Endpoint::Remote(host) => {
                        format!("would create remote directory {host}:{}", root.display())
                    }
                });
            } else {
                create_directory(dst, &root, executor)?;
            }
        }

        let source_files = inventory(src, tool, &root, executor)?;
        let destination_files = if destination_exists {
            inventory(dst, tool, &root, executor)?
        } else {
            BTreeSet::new()
        };
        let missing: Vec<SyncRelativePath> = source_files
            .difference(&destination_files)
            .cloned()
            .collect();

        let (status, transferred_files) = if options.dry_run {
            messages.push(format!(
                "would sync missing {tool} sessions from {} to {}: {} ({} files)",
                src.label(),
                dst.label(),
                root.display(),
                missing.len()
            ));
            (ToolSyncStatus::WouldCopy, 0)
        } else if missing.is_empty() {
            messages.push(format!(
                "synced missing {tool} sessions from {} to {}: {} (nothing to do)",
                src.label(),
                dst.label(),
                root.display()
            ));
            (ToolSyncStatus::NothingToDo, 0)
        } else {
            transfer_missing(src, dst, &root, &missing, executor)?;
            messages.push(format!(
                "synced missing {tool} sessions from {} to {}: {} ({} files)",
                src.label(),
                dst.label(),
                root.display(),
                missing.len()
            ));
            (ToolSyncStatus::Copied, missing.len())
        };
        (missing, transferred_files, Some(status))
    } else {
        (Vec::new(), 0, None)
    };

    let codex_history = if tool == SourceTool::Codex {
        let history_path = home.join(".codex/history.jsonl");
        let history = merge_codex_history(src, dst, &history_path, options.dry_run, executor)?;
        if !source_root_exists && !history.source_exists {
            return Ok(ToolSyncReport {
                tool,
                root,
                status: ToolSyncStatus::SourceAbsent,
                missing_files: 0,
                transferred_files: 0,
                codex_history_added: None,
                messages,
                error: None,
            });
        }
        messages.push(format!(
            "{} codex history into {}: {} (+{} entries)",
            if options.dry_run { "would merge" } else { "merged" },
            dst.label(),
            history_path.display(),
            history.added
        ));
        Some(history)
    } else {
        None
    };

    let status = if options.dry_run {
        ToolSyncStatus::WouldCopy
    } else if session_status == Some(ToolSyncStatus::Copied)
        || codex_history
            .as_ref()
            .is_some_and(|history| history.added != 0)
    {
        ToolSyncStatus::Copied
    } else {
        ToolSyncStatus::NothingToDo
    };
    Ok(ToolSyncReport {
        tool,
        root,
        status,
        missing_files: missing.len(),
        transferred_files,
        codex_history_added: codex_history.map(|history| history.added),
        messages,
        error: None,
    })
}

fn validate_request(src: &Endpoint, dst: &Endpoint) -> Result<()> {
    if src.is_local() && dst.is_local() {
        return Err(SyncError::BothEndpointsLocal);
    }
    validate_endpoint(src)?;
    validate_endpoint(dst)?;
    Ok(())
}

fn validate_endpoint(endpoint: &Endpoint) -> Result<()> {
    let Endpoint::Remote(host) = endpoint else {
        return Ok(());
    };
    if host.is_empty() {
        return Err(SyncError::EmptyRemoteHost);
    }
    if host.as_bytes().contains(&0) {
        return Err(SyncError::InvalidRemoteHost);
    }
    Ok(())
}

fn selected_tools(requested: &[SourceTool]) -> Vec<SourceTool> {
    let requested: HashSet<SourceTool> = requested.iter().copied().collect();
    SourceTool::ALL
        .into_iter()
        .filter(|tool| *tool != SourceTool::Agent)
        .filter(|tool| requested.is_empty() || requested.contains(tool))
        .collect()
}

fn directory_exists<E: CommandExecutor>(
    endpoint: &Endpoint,
    path: &Path,
    executor: &mut E,
) -> Result<bool> {
    match endpoint {
        Endpoint::Local => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Err(SyncError::UnsafeRoot(path.to_path_buf()))
            }
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(SyncError::Filesystem {
                operation: "probing directory",
                path: path.to_path_buf(),
                source,
            }),
        },
        Endpoint::Remote(host) => {
            let output = execute(
                executor,
                &remote_bash(host, remote_probe_script(path)?, None),
                "remote directory probe",
            )?;
            match output.status {
                0 => Ok(true),
                3 => Ok(false),
                status => Err(command_failed("remote directory probe", status, &output.stderr)),
            }
        }
    }
}

fn create_directory<E: CommandExecutor>(
    endpoint: &Endpoint,
    path: &Path,
    executor: &mut E,
) -> Result<()> {
    match endpoint {
        Endpoint::Local => fs::create_dir_all(path).map_err(|source| SyncError::Filesystem {
            operation: "creating directory",
            path: path.to_path_buf(),
            source,
        }),
        Endpoint::Remote(host) => {
            let mut script = OsString::from("mkdir -p -- ");
            script.push(quote_posix(path.as_os_str())?);
            let output = execute(executor, &remote_bash(host, script, None), "remote mkdir")?;
            if output.status == 0 {
                Ok(())
            } else {
                Err(command_failed("remote mkdir", output.status, &output.stderr))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SyncRelativePath(PathBuf);

impl SyncRelativePath {
    pub fn parse(path: PathBuf) -> Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(SyncError::InvalidRelativePath {
                path,
                reason: "path is empty",
            });
        }
        if path.is_absolute() {
            return Err(SyncError::InvalidRelativePath {
                path,
                reason: "absolute paths are not allowed",
            });
        }
        if os_bytes(path.as_os_str())?.contains(&0) {
            return Err(SyncError::InvalidRelativePath {
                path,
                reason: "NUL bytes are not allowed",
            });
        }
        for component in path.components() {
            match component {
                Component::Normal(value) if value != OsStr::new(".rsync-partial") => {}
                Component::Normal(_) => {
                    return Err(SyncError::InvalidRelativePath {
                        path,
                        reason: ".rsync-partial is excluded",
                    });
                }
                _ => {
                    return Err(SyncError::InvalidRelativePath {
                        path,
                        reason: "only normal relative components are allowed",
                    });
                }
            }
        }
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

fn inventory<E: CommandExecutor>(
    endpoint: &Endpoint,
    tool: SourceTool,
    root: &Path,
    executor: &mut E,
) -> Result<BTreeSet<SyncRelativePath>> {
    match endpoint {
        Endpoint::Local => local_inventory(tool, root),
        Endpoint::Remote(host) => remote_inventory(host, tool, root, executor),
    }
}

fn local_inventory(tool: SourceTool, root: &Path) -> Result<BTreeSet<SyncRelativePath>> {
    let mut files = BTreeSet::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new(".rsync-partial"));
    for entry in walker {
        let entry = entry.map_err(|error| SyncError::Filesystem {
            operation: "walking session root",
            path: error.path().unwrap_or(root).to_path_buf(),
            source: error
                .into_io_error()
                .unwrap_or_else(|| io::Error::other("walk failed")),
        })?;
        if entry.depth() == 0 || !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| SyncError::InvalidInventory {
                tool,
                path: entry.path().to_path_buf(),
            })?
            .to_path_buf();
        if matches_tool_path(tool, &relative) {
            files.insert(SyncRelativePath::parse(relative)?);
        }
    }
    Ok(files)
}

fn remote_inventory<E: CommandExecutor>(
    host: &str,
    tool: SourceTool,
    root: &Path,
    executor: &mut E,
) -> Result<BTreeSet<SyncRelativePath>> {
    let output = execute(
        executor,
        &remote_bash(host, remote_inventory_script(tool, root)?, None),
        "remote inventory",
    )?;
    if output.status == 3 {
        return Ok(BTreeSet::new());
    }
    if output.status != 0 {
        return Err(command_failed("remote inventory", output.status, &output.stderr));
    }

    let mut files = BTreeSet::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let path = PathBuf::from(os_string_from_bytes(raw.to_vec())?);
        let relative = SyncRelativePath::parse(path.clone())?;
        if !matches_tool_path(tool, relative.as_path()) {
            return Err(SyncError::InvalidInventory { tool, path });
        }
        files.insert(relative);
    }
    Ok(files)
}

fn matches_tool_path(tool: SourceTool, path: &Path) -> bool {
    let depth = path.components().count();
    let Some(name) = path.file_name() else {
        return false;
    };
    match tool {
        SourceTool::Pi | SourceTool::Omp => {
            depth == 2 && path.extension() == Some(OsStr::new("jsonl"))
        }
        SourceTool::Droid | SourceTool::Claude => {
            path.extension() == Some(OsStr::new("jsonl"))
        }
        SourceTool::Codex => {
            path.extension() == Some(OsStr::new("jsonl")) && os_starts_with(name, b"rollout-")
        }
        SourceTool::Grok => depth == 3 && !os_ends_with(name, b".lock"),
        SourceTool::Agent => false,
    }
}

fn remote_probe_script(path: &Path) -> Result<OsString> {
    let mut script = OsString::from("test -d ");
    script.push(quote_posix(path.as_os_str())?);
    script.push(" || exit 3");
    Ok(script)
}

fn remote_inventory_script(tool: SourceTool, root: &Path) -> Result<OsString> {
    let mut script = OsString::from("cd -- ");
    script.push(quote_posix(root.as_os_str())?);
    script.push(" || exit 3; find . ");
    script.push(match tool {
        SourceTool::Pi | SourceTool::Omp => {
            "-mindepth 2 -maxdepth 2 -type f -name '*.jsonl'"
        }
        SourceTool::Droid | SourceTool::Claude => "-type f -name '*.jsonl'",
        SourceTool::Codex => "-type f -name 'rollout-*.jsonl'",
        SourceTool::Grok => "-mindepth 3 -maxdepth 3 -type f ! -name '*.lock'",
        SourceTool::Agent => "-false",
    });
    script.push(" -not -path '*/.rsync-partial/*' -printf '%P\\0'");
    Ok(script)
}

fn transfer_missing<E: CommandExecutor>(
    src: &Endpoint,
    dst: &Endpoint,
    root: &Path,
    missing: &[SyncRelativePath],
    executor: &mut E,
) -> Result<()> {
    if src.is_local() || dst.is_local() {
        transfer_rsync(src, dst, root, missing, executor)
    } else {
        transfer_remote_relay(src, dst, root, missing, executor)
    }
}

fn transfer_rsync<E: CommandExecutor>(
    src: &Endpoint,
    dst: &Endpoint,
    root: &Path,
    missing: &[SyncRelativePath],
    executor: &mut E,
) -> Result<()> {
    if !executor.program_exists(OsStr::new("rsync")) {
        return Err(SyncError::MissingRsync);
    }
    let mut args: Vec<OsString> = [
        "-a",
        "--partial",
        "--partial-dir=.rsync-partial",
        "--exclude=.rsync-partial/",
        "--ignore-existing",
        "--relative",
        "--from0",
        "--files-from=-",
        "--protect-args",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if !src.is_local() || !dst.is_local() {
        // This value is rsync's documented remote-program boundary, not local shell execution.
        args.push(OsString::from("--rsync-path=command rsync"));
    }
    args.push(OsString::from("--"));
    args.push(rsync_endpoint(src, root));
    args.push(rsync_endpoint(dst, root));

    let command = CommandSpec::new("rsync", args).with_stdin(encode_nul_paths(missing)?);
    let output = execute(executor, &command, "rsync")?;
    if output.status == 0 {
        Ok(())
    } else {
        Err(command_failed("rsync", output.status, &output.stderr))
    }
}

fn transfer_remote_relay<E: CommandExecutor>(
    src: &Endpoint,
    dst: &Endpoint,
    root: &Path,
    missing: &[SyncRelativePath],
    executor: &mut E,
) -> Result<()> {
    let Endpoint::Remote(source_host) = src else {
        unreachable!("remote relay source")
    };
    let Endpoint::Remote(destination_host) = dst else {
        unreachable!("remote relay destination")
    };
    let staging = TempDir::new().map_err(|source| SyncError::Filesystem {
        operation: "creating sync staging directory",
        path: env::temp_dir(),
        source,
    })?;

    for batch in missing.chunks(TRANSFER_BATCH_SIZE) {
        let pull = remote_tar_create(source_host, root, batch)?;
        let extract = CommandSpec::new(
            "tar",
            vec![
                OsString::from("-C"),
                staging.path().as_os_str().to_owned(),
                OsString::from("--no-same-owner"),
                OsString::from("--no-same-permissions"),
                OsString::from("-xf"),
                OsString::from("-"),
            ],
        );
        ensure_pipeline_success(
            "remote tar pull",
            &pipeline(executor, &pull, &extract, "remote tar pull")?,
        )?;
    }
    validate_staging(staging.path(), missing)?;

    for batch in missing.chunks(TRANSFER_BATCH_SIZE) {
        let mut tar_args = vec![
            OsString::from("-C"),
            staging.path().as_os_str().to_owned(),
            OsString::from("-cf"),
            OsString::from("-"),
            OsString::from("--"),
        ];
        tar_args.extend(batch.iter().map(|path| path.as_path().as_os_str().to_owned()));
        let create = CommandSpec::new("tar", tar_args);
        let extract = remote_tar_extract(destination_host, root, batch)?;
        ensure_pipeline_success(
            "remote tar push",
            &pipeline(executor, &create, &extract, "remote tar push")?,
        )?;
    }
    Ok(())
}

fn remote_tar_create(host: &str, root: &Path, files: &[SyncRelativePath]) -> Result<CommandSpec> {
    let mut script = OsString::from("cd -- ");
    script.push(quote_posix(root.as_os_str())?);
    script.push(" && tar --no-recursion -cf - --");
    for file in files {
        script.push(" ");
        script.push(quote_posix(file.as_path().as_os_str())?);
    }
    Ok(remote_bash(host, script, None))
}

fn remote_tar_extract(
    host: &str,
    root: &Path,
    files: &[SyncRelativePath],
) -> Result<CommandSpec> {
    Ok(remote_bash(host, remote_tar_install_script(root, files)?, None))
}

fn remote_tar_install_script(root: &Path, files: &[SyncRelativePath]) -> Result<OsString> {
    let root_quoted = quote_posix(root.as_os_str())?;
    let mut script = OsString::from("set -e; if [ -L ");
    script.push(&root_quoted);
    script.push(" ] || { [ -e ");
    script.push(&root_quoted);
    script.push(" ] && [ ! -d ");
    script.push(&root_quoted);
    script.push(" ]; }; then exit 4; fi; mkdir -p -- ");
    script.push(&root_quoted);
    script.push("; [ ! -L ");
    script.push(&root_quoted);
    script.push(" ] || exit 4; partial=");
    script.push(&root_quoted);
    script.push("/.rsync-partial; if [ -L \"$partial\" ] || { [ -e \"$partial\" ] && [ ! -d \"$partial\" ]; }; then exit 4; fi; mkdir -p -- \"$partial\"; [ ! -L \"$partial\" ] || exit 4; stage=$(mktemp -d -p \"$partial\" agent-loader.XXXXXX); trap 'rm -rf -- \"$stage\"' EXIT; tar -C \"$stage\" --no-same-owner --no-same-permissions -xf -;");

    for file in files {
        let stage_file = stage_path_expression(file.as_path())?;
        script.push(" [ -f ");
        script.push(&stage_file);
        script.push(" ] && [ ! -L ");
        script.push(&stage_file);
        script.push(" ] || exit 4;");

        let mut parent = root.to_path_buf();
        let mut components = file.as_path().components().peekable();
        while let Some(Component::Normal(component)) = components.next() {
            if components.peek().is_none() {
                break;
            }
            parent.push(component);
            let quoted = quote_posix(parent.as_os_str())?;
            script.push(" if [ -L ");
            script.push(&quoted);
            script.push(" ] || { [ -e ");
            script.push(&quoted);
            script.push(" ] && [ ! -d ");
            script.push(&quoted);
            script.push(" ]; }; then exit 4; fi; mkdir -p -- ");
            script.push(&quoted);
            script.push("; [ ! -L ");
            script.push(&quoted);
            script.push(" ] || exit 4;");
        }

        let destination = quote_posix(&root.join(file.as_path()).into_os_string())?;
        script.push(" if [ -L ");
        script.push(&destination);
        script.push(" ]; then :; elif [ -e ");
        script.push(&destination);
        script.push(" ]; then :; else mv -n -- ");
        script.push(&stage_file);
        script.push(" ");
        script.push(&destination);
        script.push("; fi;");
    }
    Ok(script)
}

fn stage_path_expression(relative: &Path) -> Result<OsString> {
    let quoted = quote_posix(relative.as_os_str())?;
    let bytes = os_bytes(&quoted)?;
    let mut expression = Vec::with_capacity(bytes.len() + 10);
    expression.extend_from_slice(b"\"$stage\"/");
    expression.extend_from_slice(bytes);
    os_string_from_bytes(expression)
}

fn validate_staging(root: &Path, requested: &[SyncRelativePath]) -> Result<()> {
    let expected: BTreeSet<PathBuf> = requested.iter().map(|path| path.0.clone()).collect();
    let mut actual = BTreeSet::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| SyncError::Filesystem {
            operation: "validating staged archive",
            path: error.path().unwrap_or(root).to_path_buf(),
            source: error
                .into_io_error()
                .unwrap_or_else(|| io::Error::other("walk failed")),
        })?;
        if entry.depth() == 0 || entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            return Err(SyncError::UnsafeArchive);
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| SyncError::UnsafeArchive)?
            .to_path_buf();
        SyncRelativePath::parse(relative.clone())?;
        actual.insert(relative);
    }
    if actual == expected {
        Ok(())
    } else {
        Err(SyncError::UnsafeArchive)
    }
}

fn rsync_endpoint(endpoint: &Endpoint, root: &Path) -> OsString {
    let mut value = match endpoint {
        Endpoint::Local => root.as_os_str().to_owned(),
        Endpoint::Remote(host) => {
            let mut value = OsString::from(host);
            value.push(":");
            value.push(root.as_os_str());
            value
        }
    };
    value.push("/");
    value
}

fn encode_nul_paths(paths: &[SyncRelativePath]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for path in paths {
        let raw = os_bytes(path.as_path().as_os_str())?;
        if raw.contains(&0) {
            return Err(SyncError::InvalidRelativePath {
                path: path.0.clone(),
                reason: "NUL bytes are not allowed",
            });
        }
        bytes.extend_from_slice(raw);
        bytes.push(0);
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexHistoryMerge {
    source_exists: bool,
    added: usize,
}

fn merge_codex_history<E: CommandExecutor>(
    src: &Endpoint,
    dst: &Endpoint,
    path: &Path,
    dry_run: bool,
    executor: &mut E,
) -> Result<CodexHistoryMerge> {
    let Some(source_bytes) = read_history(src, path, executor)? else {
        return Ok(CodexHistoryMerge {
            source_exists: false,
            added: 0,
        });
    };
    let destination_bytes = read_history(dst, path, executor)?.unwrap_or_default();
    let (added, merged) = merge_codex_history_bytes(&source_bytes, &destination_bytes);
    if added != 0 && !dry_run {
        // Append rather than replace: a concurrent Codex writer may add rows after our read, and
        // destination bytes are authoritative. O_APPEND preserves those rows instead of losing
        // them to a stale whole-file rename.
        append_history(dst, path, &merged[destination_bytes.len()..], executor)?;
    }
    Ok(CodexHistoryMerge {
        source_exists: true,
        added,
    })
}

fn merge_codex_history_bytes(source: &[u8], destination: &[u8]) -> (usize, Vec<u8>) {
    let mut keys: HashSet<HistoryKey> = history_lines(destination)
        .filter_map(history_key)
        .collect();
    let mut missing = Vec::new();
    for line in history_lines(source) {
        let Some(key) = history_key(line) else {
            continue;
        };
        if keys.insert(key) {
            missing.push(line);
        }
    }
    if missing.is_empty() {
        return (0, destination.to_vec());
    }

    let mut merged = Vec::with_capacity(
        destination.len() + missing.iter().map(|line| line.len() + 1).sum::<usize>() + 1,
    );
    merged.extend_from_slice(destination);
    if !destination.is_empty() && !destination.ends_with(b"\n") && !destination.ends_with(b"\r") {
        merged.push(b'\n');
    }
    for line in &missing {
        merged.extend_from_slice(line);
        merged.push(b'\n');
    }
    (missing.len(), merged)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HistoryKey {
    session_id: String,
    timestamp: String,
    text: String,
}

fn history_key(line: &[u8]) -> Option<HistoryKey> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let object = value.as_object()?;
    let session_id = object.get("session_id")?.as_str()?;
    if session_id.is_empty() {
        return None;
    }
    let timestamp = object.get("ts")?.as_number()?;
    if !timestamp.is_i64() && !timestamp.is_u64() {
        return None;
    }
    let text = object.get("text")?.as_str()?;
    Some(HistoryKey {
        session_id: session_id.to_owned(),
        timestamp: timestamp.to_string(),
        text: text.to_owned(),
    })
}

fn history_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|byte| *byte == b'\n').filter_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            None
        } else {
            Some(line)
        }
    })
}

fn read_history<E: CommandExecutor>(
    endpoint: &Endpoint,
    path: &Path,
    executor: &mut E,
) -> Result<Option<Vec<u8>>> {
    match endpoint {
        Endpoint::Local => match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(SyncError::UnsafeHistory(path.to_path_buf()))
            }
            Ok(_) => fs::read(path)
                .map(Some)
                .map_err(|source| SyncError::Filesystem {
                    operation: "reading Codex history",
                    path: path.to_path_buf(),
                    source,
                }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SyncError::Filesystem {
                operation: "probing Codex history",
                path: path.to_path_buf(),
                source,
            }),
        },
        Endpoint::Remote(host) => {
            let output = execute(
                executor,
                &remote_bash(host, remote_history_read_script(path)?, None),
                "remote Codex history read",
            )?;
            match output.status {
                0 => Ok(Some(output.stdout)),
                3 => Ok(None),
                status => Err(command_failed(
                    "remote Codex history read",
                    status,
                    &output.stderr,
                )),
            }
        }
    }
}

fn append_history<E: CommandExecutor>(
    endpoint: &Endpoint,
    path: &Path,
    content: &[u8],
    executor: &mut E,
) -> Result<()> {
    match endpoint {
        Endpoint::Local => append_history_local(path, content),
        Endpoint::Remote(host) => {
            let command = remote_bash(host, remote_history_append_script(path)?, Some(content.to_vec()));
            let output = execute(executor, &command, "remote Codex history append")?;
            if output.status == 0 {
                Ok(())
            } else {
                Err(command_failed(
                    "remote Codex history append",
                    output.status,
                    &output.stderr,
                ))
            }
        }
    }
}

fn append_history_local(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| SyncError::Filesystem {
        operation: "locating Codex history parent",
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| SyncError::Filesystem {
        operation: "creating Codex history parent",
        path: parent.to_path_buf(),
        source,
    })?;

    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SyncError::UnsafeHistory(path.to_path_buf()));
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| SyncError::Filesystem {
        operation: "opening Codex history for append",
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| SyncError::Filesystem {
        operation: "inspecting Codex history",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(SyncError::UnsafeHistory(path.to_path_buf()));
    }
    file.write_all(content)
        .and_then(|()| file.sync_all())
        .map_err(|source| SyncError::Filesystem {
            operation: "appending Codex history",
            path: path.to_path_buf(),
            source,
        })
}

fn remote_history_read_script(path: &Path) -> Result<OsString> {
    let quoted = quote_posix(path.as_os_str())?;
    let mut script = OsString::from("if [ ! -e ");
    script.push(&quoted);
    script.push(" ]; then exit 3; fi; if [ -L ");
    script.push(&quoted);
    script.push(" ] || [ ! -f ");
    script.push(&quoted);
    script.push(" ]; then exit 4; fi; cat -- ");
    script.push(quoted);
    Ok(script)
}

fn remote_history_append_script(path: &Path) -> Result<OsString> {
    let parent = path
        .parent()
        .ok_or_else(|| SyncError::UnsupportedRemotePath(path.as_os_str().to_owned()))?;
    let parent = quote_posix(parent.as_os_str())?;
    let path = quote_posix(path.as_os_str())?;
    let mut script = OsString::from("set -e; mkdir -p -- ");
    script.push(&parent);
    script.push("; if [ -L ");
    script.push(&path);
    script.push(" ] || { [ -e ");
    script.push(&path);
    script.push(" ] && [ ! -f ");
    script.push(&path);
    script.push(" ]; }; then exit 4; fi; umask 077; cat >> ");
    script.push(&path);
    Ok(script)
}

fn remote_bash(host: &str, script: OsString, stdin: Option<Vec<u8>>) -> CommandSpec {
    let mut remote_command = OsString::from("bash -lc ");
    remote_command.push(quote_posix(&script).expect("constructed script is quoteable"));
    let mut command = CommandSpec::new(
        "ssh",
        vec![
            OsString::from("--"),
            OsString::from(host),
            remote_command,
        ],
    );
    command.stdin = stdin;
    command
}

fn execute<E: CommandExecutor>(
    executor: &mut E,
    command: &CommandSpec,
    operation: &'static str,
) -> Result<CommandOutput> {
    executor
        .execute(command)
        .map_err(|source| SyncError::Spawn { operation, source })
}

fn pipeline<E: CommandExecutor>(
    executor: &mut E,
    producer: &CommandSpec,
    consumer: &CommandSpec,
    operation: &'static str,
) -> Result<PipelineOutput> {
    executor
        .pipeline(producer, consumer)
        .map_err(|source| SyncError::Spawn { operation, source })
}

fn ensure_pipeline_success(operation: &'static str, output: &PipelineOutput) -> Result<()> {
    if output.producer_status == 0 && output.consumer_status == 0 {
        return Ok(());
    }
    let mut diagnostics = output.producer_stderr.clone();
    diagnostics.extend_from_slice(&output.consumer_stderr);
    Err(SyncError::PipelineFailed {
        operation,
        producer_status: output.producer_status,
        consumer_status: output.consumer_status,
        detail: stderr_detail(&diagnostics),
    })
}

fn command_failed(operation: &'static str, status: i32, stderr: &[u8]) -> SyncError {
    SyncError::CommandFailed {
        operation,
        status,
        detail: stderr_detail(stderr),
    }
}

fn stderr_detail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(8);
    if start == lines.len() {
        String::new()
    } else {
        format!("\n{}", lines[start..].join("\n"))
    }
}

fn quote_posix(value: &OsStr) -> Result<OsString> {
    let bytes = os_bytes(value)?;
    if bytes.contains(&0) {
        return Err(SyncError::UnsupportedRemotePath(value.to_owned()));
    }
    let mut quoted = Vec::with_capacity(bytes.len() + 2);
    quoted.push(b'\'');
    for byte in bytes {
        if *byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(*byte);
        }
    }
    quoted.push(b'\'');
    os_string_from_bytes(quoted)
}

fn os_bytes(value: &OsStr) -> Result<&[u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(value.as_bytes())
    }
    #[cfg(not(unix))]
    {
        value
            .to_str()
            .map(str::as_bytes)
            .ok_or_else(|| SyncError::UnsupportedRemotePath(value.to_owned()))
    }
}

fn os_string_from_bytes(bytes: Vec<u8>) -> Result<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(OsString::from)
            .map_err(|error| SyncError::UnsupportedRemotePath(OsString::from(error.to_string())))
    }
}

fn os_starts_with(value: &OsStr, prefix: &[u8]) -> bool {
    os_bytes(value)
        .map(|bytes| bytes.starts_with(prefix))
        .unwrap_or(false)
}

fn os_ends_with(value: &OsStr, suffix: &[u8]) -> bool {
    os_bytes(value)
        .map(|bytes| bytes.ends_with(suffix))
        .unwrap_or(false)
}

fn process_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map_or(1, |signal| 128 + signal)
    }
    #[cfg(not(unix))]
    {
        1
    }
}

fn resolve_program(program: &OsStr) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return executable_file(path).then(|| path.to_path_buf());
    }
    let search = env::var_os("PATH")?;
    for directory in env::split_paths(&search) {
        let candidate = directory.join(program);
        if executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let extensions = env::var_os("PATHEXT")
                .unwrap_or_else(|| OsString::from(".EXE;.CMD;.BAT;.COM"));
            for extension in extensions.to_string_lossy().split(';') {
                let candidate = directory.join(format!(
                    "{}{}",
                    program.to_string_lossy(),
                    extension
                ));
                if executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::*;

    #[derive(Default)]
    struct FakeExecutor {
        program_exists: bool,
        outputs: VecDeque<CommandOutput>,
        pipeline_outputs: VecDeque<PipelineOutput>,
        commands: Vec<CommandSpec>,
        pipelines: Vec<(CommandSpec, CommandSpec)>,
        materialize_on_pull: Vec<PathBuf>,
    }

    impl FakeExecutor {
        fn with_rsync() -> Self {
            Self {
                program_exists: true,
                ..Self::default()
            }
        }

        fn output(&mut self, status: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) {
            self.outputs.push_back(CommandOutput {
                status,
                stdout: stdout.into(),
                stderr: stderr.into(),
            });
        }
    }

    impl CommandExecutor for FakeExecutor {
        fn program_exists(&mut self, _program: &OsStr) -> bool {
            self.program_exists
        }

        fn execute(&mut self, command: &CommandSpec) -> io::Result<CommandOutput> {
            self.commands.push(command.clone());
            self.outputs
                .pop_front()
                .ok_or_else(|| io::Error::other("missing fake command output"))
        }

        fn pipeline(
            &mut self,
            producer: &CommandSpec,
            consumer: &CommandSpec,
        ) -> io::Result<PipelineOutput> {
            self.pipelines.push((producer.clone(), consumer.clone()));
            if consumer.program == OsStr::new("tar") {
                let destination = consumer
                    .args
                    .windows(2)
                    .find(|pair| pair[0] == OsStr::new("-C"))
                    .map(|pair| PathBuf::from(&pair[1]))
                    .expect("tar destination");
                for relative in &self.materialize_on_pull {
                    let path = destination.join(relative);
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(path, b"session").unwrap();
                }
            }
            self.pipeline_outputs
                .pop_front()
                .ok_or_else(|| io::Error::other("missing fake pipeline output"))
        }
    }

    fn root(home: &Path, tool: SourceTool) -> PathBuf {
        Catalog::new(home).root_for_tool(tool).path
    }

    fn command_text(command: &CommandSpec) -> String {
        command
            .args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn codex_merge_keeps_destination_bytes_and_appends_only_missing_valid_records() {
        let destination = concat!(
            "not-json\n",
            "{\"session_id\":\"a\",\"ts\":1,\"text\":\"same\",\"future\":true}\n",
            "{\"other\":\"kept\"}"
        )
        .as_bytes();
        let source = concat!(
            "bad-source\n",
            "{\"session_id\":\"a\",\"ts\":1,\"text\":\"same\",\"source_only\":1}\n",
            " {\"session_id\":\"b\",\"ts\":2,\"text\":\"new\",\"unknown\":{\"x\":1}} \r\n",
            "{\"session_id\":\"b\",\"ts\":2,\"text\":\"new\"}\n",
            "{\"session_id\":\"c\",\"ts\":false,\"text\":\"invalid\"}\n"
        )
        .as_bytes();

        let (added, merged) = merge_codex_history_bytes(source, destination);

        assert_eq!(added, 1);
        assert!(merged.starts_with(destination));
        assert_eq!(
            &merged[destination.len()..],
            b"\n {\"session_id\":\"b\",\"ts\":2,\"text\":\"new\",\"unknown\":{\"x\":1}} \n"
        );
    }

    #[test]
    fn codex_sync_merges_remote_history_into_local_destination_authoritatively() {
        let temporary = tempdir().unwrap();
        let session_root = root(temporary.path(), SourceTool::Codex);
        fs::create_dir_all(&session_root).unwrap();
        let history = temporary.path().join(".codex/history.jsonl");
        let destination = concat!(
            "malformed but retained\n",
            "{\"session_id\":\"same\",\"ts\":1,\"text\":\"kept\",\"destination\":true}"
        );
        fs::write(&history, destination).unwrap();

        let mut executor = FakeExecutor::with_rsync();
        executor.output(0, Vec::new(), Vec::new());
        executor.output(0, Vec::new(), Vec::new());
        executor.output(
            0,
            concat!(
                "{\"session_id\":\"same\",\"ts\":1,\"text\":\"kept\",\"source\":true}\n",
                "not valid\n",
                "{\"session_id\":\"new\",\"ts\":2,\"text\":\"added\",\"future\":{\"x\":1}}\n"
            ),
            Vec::new(),
        );

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Codex],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert!(report.is_success());
        assert_eq!(report.tools[0].status, ToolSyncStatus::Copied);
        assert_eq!(report.tools[0].codex_history_added, Some(1));
        let merged = fs::read(&history).unwrap();
        assert!(merged.starts_with(destination.as_bytes()));
        assert!(merged.ends_with(
            b"\n{\"session_id\":\"new\",\"ts\":2,\"text\":\"added\",\"future\":{\"x\":1}}\n"
        ));
        assert!(!String::from_utf8_lossy(&merged).contains("not valid"));
    }

    #[test]
    fn codex_sync_merges_history_when_session_root_is_absent() {
        let temporary = tempdir().unwrap();
        let history = temporary.path().join(".codex/history.jsonl");
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        let destination = concat!(
            "malformed but retained\n",
            "{\"session_id\":\"same\",\"ts\":1,\"text\":\"kept\",\"destination\":true}"
        );
        fs::write(&history, destination).unwrap();

        let mut executor = FakeExecutor::with_rsync();
        executor.output(3, Vec::new(), Vec::new());
        executor.output(
            0,
            concat!(
                "{\"session_id\":\"same\",\"ts\":1,\"text\":\"kept\",\"source\":true}\n",
                "invalid source row\n",
                "{\"session_id\":\"new\",\"ts\":2,\"text\":\"added\",\"future\":true}\n"
            ),
            Vec::new(),
        );

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Codex],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        let tool = &report.tools[0];
        assert!(report.is_success());
        assert_eq!(tool.status, ToolSyncStatus::Copied);
        assert_eq!(tool.missing_files, 0);
        assert_eq!(tool.transferred_files, 0);
        assert_eq!(tool.codex_history_added, Some(1));
        assert_eq!(
            fs::read(&history).unwrap(),
            format!(
                "{destination}\n{{\"session_id\":\"new\",\"ts\":2,\"text\":\"added\",\"future\":true}}\n"
            )
            .as_bytes()
        );
        assert_eq!(executor.commands.len(), 2);
        assert!(executor
            .commands
            .iter()
            .all(|command| command.program != OsStr::new("rsync")));
    }

    #[test]
    fn codex_history_only_dry_run_reports_rows_without_writing() {
        let temporary = tempdir().unwrap();
        let history = temporary.path().join(".codex/history.jsonl");
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        let destination = b"{\"session_id\":\"old\",\"ts\":1,\"text\":\"old\"}\n";
        fs::write(&history, destination).unwrap();

        let mut executor = FakeExecutor::with_rsync();
        executor.output(3, Vec::new(), Vec::new());
        executor.output(
            0,
            b"{\"session_id\":\"new\",\"ts\":2,\"text\":\"new\"}\n".to_vec(),
            Vec::new(),
        );

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Codex],
                dry_run: true,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        let tool = &report.tools[0];
        assert_eq!(tool.status, ToolSyncStatus::WouldCopy);
        assert_eq!(tool.missing_files, 0);
        assert_eq!(tool.transferred_files, 0);
        assert_eq!(tool.codex_history_added, Some(1));
        assert_eq!(fs::read(&history).unwrap(), destination);
        assert!(!root(temporary.path(), SourceTool::Codex).exists());
        assert_eq!(executor.commands.len(), 2);
    }

    #[test]
    fn codex_history_only_sync_does_not_require_rsync() {
        let temporary = tempdir().unwrap();
        let history = temporary.path().join(".codex/history.jsonl");
        fs::create_dir_all(history.parent().unwrap()).unwrap();

        let mut executor = FakeExecutor::default();
        executor.output(3, Vec::new(), Vec::new());
        executor.output(
            0,
            b"{\"session_id\":\"new\",\"ts\":2,\"text\":\"new\"}\n".to_vec(),
            Vec::new(),
        );

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Codex],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert_eq!(report.tools[0].status, ToolSyncStatus::Copied);
        assert_eq!(report.tools[0].codex_history_added, Some(1));
        assert_eq!(
            fs::read(&history).unwrap(),
            b"{\"session_id\":\"new\",\"ts\":2,\"text\":\"new\"}\n"
        );
        assert_eq!(executor.commands.len(), 2);
    }

    #[test]
    fn codex_history_only_dedupe_keeps_destination_authoritative() {
        let temporary = tempdir().unwrap();
        let history = temporary.path().join(".codex/history.jsonl");
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        let destination = concat!(
            "malformed but retained\n",
            "{\"session_id\":\"same\",\"ts\":7,\"text\":\"kept\",\"destination_only\":true}"
        );
        fs::write(&history, destination).unwrap();

        let mut executor = FakeExecutor::with_rsync();
        executor.output(3, Vec::new(), Vec::new());
        executor.output(
            0,
            concat!(
                "{\"session_id\":\"same\",\"ts\":7,\"text\":\"kept\",\"source_only\":true}\n",
                "not valid\n"
            ),
            Vec::new(),
        );

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Codex],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        let tool = &report.tools[0];
        assert_eq!(tool.status, ToolSyncStatus::NothingToDo);
        assert_eq!(tool.codex_history_added, Some(0));
        assert_eq!(fs::read(&history).unwrap(), destination.as_bytes());
        assert_eq!(executor.commands.len(), 2);
    }

    #[test]
    fn codex_is_source_absent_only_when_root_and_history_are_absent() {
        let temporary = tempdir().unwrap();
        let mut executor = FakeExecutor::with_rsync();
        executor.output(3, Vec::new(), Vec::new());
        executor.output(3, Vec::new(), Vec::new());

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Codex],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert_eq!(report.tools[0].status, ToolSyncStatus::SourceAbsent);
        assert_eq!(report.tools[0].codex_history_added, None);
        assert_eq!(executor.commands.len(), 2);
    }

    #[test]
    fn non_codex_tool_keeps_early_source_absent_behavior() {
        let temporary = tempdir().unwrap();
        let mut executor = FakeExecutor::with_rsync();
        executor.output(3, Vec::new(), Vec::new());

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Droid],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert_eq!(report.tools[0].status, ToolSyncStatus::SourceAbsent);
        assert_eq!(report.tools[0].codex_history_added, None);
        assert_eq!(executor.commands.len(), 1);
    }

    #[test]
    fn non_codex_tool_still_requires_rsync() {
        let temporary = tempdir().unwrap();
        let mut executor = FakeExecutor::default();

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Droid],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert_eq!(report.tools[0].status, ToolSyncStatus::Failed);
        assert!(matches!(
            report.tools[0].error.as_deref(),
            Some(error) if error.contains("rsync")
        ));
        assert!(executor.commands.is_empty());
    }

    #[test]
    fn remote_to_remote_sync_does_not_probe_local_rsync() {
        let temporary = tempdir().unwrap();
        let mut executor = FakeExecutor::default();
        executor.output(3, Vec::new(), Vec::new());

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Remote("destination".into()),
            &SyncOptions {
                tools: vec![SourceTool::Droid],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert_eq!(report.tools[0].status, ToolSyncStatus::SourceAbsent);
        assert_eq!(executor.commands.len(), 1);
    }

    #[test]
    fn append_history_preserves_rows_added_after_merge_read() {
        let temporary = tempdir().unwrap();
        let history = temporary.path().join("history.jsonl");
        let destination = b"{\"session_id\":\"old\",\"ts\":1,\"text\":\"old\"}\n";
        let source = b"{\"session_id\":\"new\",\"ts\":2,\"text\":\"new\"}\n";
        fs::write(&history, destination).unwrap();
        let (_, merged) = merge_codex_history_bytes(source, destination);

        fs::OpenOptions::new()
            .append(true)
            .open(&history)
            .unwrap()
            .write_all(b"{\"session_id\":\"live\",\"ts\":3,\"text\":\"live\"}\n")
            .unwrap();
        append_history_local(&history, &merged[destination.len()..]).unwrap();

        let bytes = fs::read(&history).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("\"live\""));
        assert!(String::from_utf8_lossy(&bytes).contains("\"new\""));
    }

    #[test]
    fn local_to_remote_rsync_uses_nul_paths_and_preserves_metacharacters() {
        let temporary = tempdir().unwrap();
        let session_root = root(temporary.path(), SourceTool::Droid);
        let relative = PathBuf::from("project/odd ;$'[*]?.jsonl");
        fs::create_dir_all(session_root.join("project")).unwrap();
        fs::write(session_root.join(&relative), b"{}\n").unwrap();

        let mut executor = FakeExecutor::with_rsync();
        executor.output(3, Vec::new(), Vec::new());
        executor.output(0, Vec::new(), Vec::new());
        executor.output(0, Vec::new(), Vec::new());

        let report = sync(
            &Endpoint::Local,
            &Endpoint::Remote("remote".into()),
            &SyncOptions {
                tools: vec![SourceTool::Droid],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert!(report.is_success());
        assert_eq!(report.tools[0].transferred_files, 1);
        let rsync = executor.commands.last().unwrap();
        assert_eq!(rsync.program, OsStr::new("rsync"));
        assert!(rsync.args.contains(&OsString::from("--from0")));
        assert!(rsync.args.contains(&OsString::from("--ignore-existing")));
        assert!(rsync.args.contains(&OsString::from("--rsync-path=command rsync")));
        let mut expected = os_bytes(relative.as_os_str()).unwrap().to_vec();
        expected.push(0);
        assert_eq!(rsync.stdin.as_deref(), Some(expected.as_slice()));
    }

    #[test]
    fn grok_inventory_transfers_bundle_files_but_not_locks_or_partials() {
        let temporary = tempdir().unwrap();
        let session_root = root(temporary.path(), SourceTool::Grok);
        let session = session_root.join("encoded/session-id");
        fs::create_dir_all(session.join(".rsync-partial")).unwrap();
        for name in ["summary.json", "chat_history.jsonl", "updates.jsonl", ".cwd"] {
            fs::write(session.join(name), b"x").unwrap();
        }
        fs::write(session.join("active.lock"), b"x").unwrap();
        fs::write(session.join(".rsync-partial/ignored"), b"x").unwrap();

        let files = local_inventory(SourceTool::Grok, &session_root).unwrap();
        let names: BTreeSet<PathBuf> = files.into_iter().map(|path| path.0).collect();

        assert_eq!(
            names,
            [
                PathBuf::from("encoded/session-id/.cwd"),
                PathBuf::from("encoded/session-id/chat_history.jsonl"),
                PathBuf::from("encoded/session-id/summary.json"),
                PathBuf::from("encoded/session-id/updates.jsonl"),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn remote_installer_preserves_files_and_rejects_symlink_parents() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("destination");
        let source = temporary.path().join("source");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(source.join("project")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(source.join("project/session.jsonl"), b"source").unwrap();

        let archive = CommandSpec::new(
            "tar",
            vec![
                OsString::from("-C"),
                source.as_os_str().to_owned(),
                OsString::from("-cf"),
                OsString::from("-"),
                OsString::from("--"),
                OsString::from("project/session.jsonl"),
            ],
        );
        let relative = SyncRelativePath::parse(PathBuf::from("project/session.jsonl")).unwrap();
        let script = remote_tar_install_script(&root, std::slice::from_ref(&relative)).unwrap();
        let install = CommandSpec::new("bash", vec![OsString::from("-c"), script.clone()]);

        let output = SystemCommandExecutor.pipeline(&archive, &install).unwrap();
        assert_eq!(output.producer_status, 0);
        assert_eq!(output.consumer_status, 0);
        assert_eq!(fs::read(root.join("project/session.jsonl")).unwrap(), b"source");

        fs::remove_dir_all(root.join("project")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("project")).unwrap();
        let install = CommandSpec::new("bash", vec![OsString::from("-c"), script]);
        let output = SystemCommandExecutor.pipeline(&archive, &install).unwrap();
        assert_eq!(output.producer_status, 0);
        assert_ne!(output.consumer_status, 0);
        assert!(!outside.join("session.jsonl").exists());
    }

    #[test]
    fn remote_relay_quotes_exact_names_and_validates_staging() {
        let temporary = tempdir().unwrap();
        let relative = PathBuf::from("repo/weird ' ; $() [*].jsonl");
        let mut inventory = os_bytes(relative.as_os_str()).unwrap().to_vec();
        inventory.push(0);


        let mut executor = FakeExecutor::default();
        executor.output(0, Vec::new(), Vec::new());
        executor.output(0, Vec::new(), Vec::new());
        executor.output(0, inventory, Vec::new());
        executor.output(0, Vec::new(), Vec::new());
        executor.pipeline_outputs.push_back(PipelineOutput::success());
        executor.pipeline_outputs.push_back(PipelineOutput::success());
        executor.materialize_on_pull.push(relative.clone());

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Remote("destination".into()),
            &SyncOptions {
                tools: vec![SourceTool::Droid],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert!(report.is_success());
        assert_eq!(executor.pipelines.len(), 2);
        let pull = &executor.pipelines[0].0;
        assert_eq!(pull.program, OsStr::new("ssh"));
        assert_eq!(pull.args[0], OsStr::new("--"));
        assert_eq!(pull.args[1], OsStr::new("source"));
        assert!(command_text(pull).contains("'\\''"));
        let push = &executor.pipelines[1];
        assert_eq!(push.0.program, OsStr::new("tar"));
        assert!(push.0.args.contains(&relative.into_os_string()));
        assert_eq!(push.1.args[1], OsStr::new("destination"));
    }

    #[test]
    fn sync_aggregates_tool_failures_instead_of_stopping() {
        let temporary = tempdir().unwrap();
        let mut executor = FakeExecutor::with_rsync();
        executor.output(255, Vec::new(), b"pi unavailable\n".to_vec());
        executor.output(255, Vec::new(), b"omp unavailable\n".to_vec());

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Local,
            &SyncOptions {
                tools: vec![SourceTool::Pi, SourceTool::Omp],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert_eq!(report.failure_count(), 2);
        assert_eq!(executor.commands.len(), 2);
        assert!(report.tools[0].error.as_deref().unwrap().contains("pi unavailable"));
        assert!(report.tools[1].error.as_deref().unwrap().contains("omp unavailable"));
    }

    #[test]
    fn dry_run_does_not_create_or_transfer_destination() {
        let temporary = tempdir().unwrap();
        let session_root = root(temporary.path(), SourceTool::Pi);
        fs::create_dir_all(session_root.join("project")).unwrap();
        fs::write(session_root.join("project/session.jsonl"), b"{}\n").unwrap();

        let mut executor = FakeExecutor::with_rsync();
        executor.output(3, Vec::new(), Vec::new());
        let report = sync(
            &Endpoint::Local,
            &Endpoint::Remote("remote".into()),
            &SyncOptions {
                tools: vec![SourceTool::Pi],
                dry_run: true,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert_eq!(report.tools[0].status, ToolSyncStatus::WouldCopy);
        assert_eq!(report.tools[0].missing_files, 1);
        assert_eq!(executor.commands.len(), 1);
    }

    #[test]
    fn both_local_is_rejected_before_any_command() {
        let temporary = tempdir().unwrap();
        let mut executor = FakeExecutor::with_rsync();
        let error = sync(
            &Endpoint::Local,
            &Endpoint::Local,
            &SyncOptions::default(),
            temporary.path(),
            &mut executor,
        )
        .unwrap_err();
        assert!(matches!(error, SyncError::BothEndpointsLocal));
        assert!(executor.commands.is_empty());
    }

    #[test]
    fn remote_inventory_rejects_parent_escape_before_transfer() {
        let temporary = tempdir().unwrap();
        let mut executor = FakeExecutor::default();
        executor.output(0, Vec::new(), Vec::new());
        executor.output(0, Vec::new(), Vec::new());
        executor.output(0, b"../escape.jsonl\0".to_vec(), Vec::new());

        let report = sync(
            &Endpoint::Remote("source".into()),
            &Endpoint::Remote("destination".into()),
            &SyncOptions {
                tools: vec![SourceTool::Droid],
                dry_run: false,
            },
            temporary.path(),
            &mut executor,
        )
        .unwrap();

        assert_eq!(report.failure_count(), 1);
        assert!(executor.pipelines.is_empty());
    }
    #[test]
    fn sync_relative_path_parse_rejects_unsafe_and_accepts_normal_paths() {
        let reject: [(&str, PathBuf, &str); 10] = [
            ("empty path", PathBuf::new(), "path is empty"),
            ("absolute path", PathBuf::from("/etc/passwd"), "absolute paths are not allowed"),
            ("nul byte", PathBuf::from("a\0b"), "NUL bytes are not allowed"),
            ("rsync-partial at root", PathBuf::from(".rsync-partial"), ".rsync-partial is excluded"),
            ("rsync-partial nested", PathBuf::from("project/.rsync-partial"), ".rsync-partial is excluded"),
            ("rsync-partial mid path", PathBuf::from("a/.rsync-partial/b"), ".rsync-partial is excluded"),
            ("parent dir", PathBuf::from(".."), "only normal relative components are allowed"),
            ("parent escape", PathBuf::from("a/../b"), "only normal relative components are allowed"),
            ("cur dir", PathBuf::from("."), "only normal relative components are allowed"),
            ("cur dir prefix", PathBuf::from("./a"), "only normal relative components are allowed"),
        ];
        for (name, path, reason) in reject {
            let error = SyncRelativePath::parse(path).expect_err(name);
            match error {
                SyncError::InvalidRelativePath { reason: actual, .. } => {
                    assert_eq!(actual, reason, "{name}");
                }
                other => panic!("{name}: expected InvalidRelativePath, got {other:?}"),
            }
        }

        let accept = [
            "session.jsonl",
            "project/session.jsonl",
            "a/b/c.jsonl",
            "deep/nested/dir/file.jsonl",
        ];
        for path in accept {
            let parsed = SyncRelativePath::parse(PathBuf::from(path)).expect(path);
            assert_eq!(parsed.as_path(), Path::new(path), "{path}");
        }
    }

    #[test]
    fn validate_staging_rejects_extra_file_outside_request() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("stage");
        fs::create_dir_all(root.join("project")).unwrap();
        fs::write(root.join("project/session.jsonl"), b"x").unwrap();
        fs::write(root.join("project/extra.jsonl"), b"x").unwrap();
        let requested = vec![SyncRelativePath::parse(PathBuf::from("project/session.jsonl")).unwrap()];

        let error = validate_staging(&root, &requested).unwrap_err();

        assert!(matches!(error, SyncError::UnsafeArchive));
        assert!(!root.join("project/extra.jsonl").metadata().unwrap().is_dir());
    }

    #[test]
    fn validate_staging_rejects_non_regular_entry() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("stage");
        fs::create_dir_all(root.join("project")).unwrap();
        fs::write(root.join("project/session.jsonl"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            root.join("project/session.jsonl"),
            root.join("project/link.jsonl"),
        )
        .unwrap();
        let requested = vec![
            SyncRelativePath::parse(PathBuf::from("project/session.jsonl")).unwrap(),
            SyncRelativePath::parse(PathBuf::from("project/link.jsonl")).unwrap(),
        ];

        let error = validate_staging(&root, &requested).unwrap_err();

        assert!(matches!(error, SyncError::UnsafeArchive));
    }

    #[test]
    fn validate_staging_rejects_missing_requested_file() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("stage");
        fs::create_dir_all(root.join("project")).unwrap();
        fs::write(root.join("project/session.jsonl"), b"x").unwrap();
        let requested = vec![
            SyncRelativePath::parse(PathBuf::from("project/session.jsonl")).unwrap(),
            SyncRelativePath::parse(PathBuf::from("project/missing.jsonl")).unwrap(),
        ];

        let error = validate_staging(&root, &requested).unwrap_err();

        assert!(matches!(error, SyncError::UnsafeArchive));
    }

    #[test]
    fn validate_staging_accepts_exact_requested_regular_files() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("stage");
        fs::create_dir_all(root.join("project")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join("project/session.jsonl"), b"x").unwrap();
        fs::write(root.join("other/log.jsonl"), b"x").unwrap();
        let requested = vec![
            SyncRelativePath::parse(PathBuf::from("project/session.jsonl")).unwrap(),
            SyncRelativePath::parse(PathBuf::from("other/log.jsonl")).unwrap(),
        ];

        validate_staging(&root, &requested).unwrap();
    }

    #[test]
    fn matches_tool_path_depth_and_prefix_table() {
        let cases: &[(SourceTool, &str, bool)] = &[
            // Pi / Omp require depth 2 and a .jsonl extension.
            (SourceTool::Pi, "project/session.jsonl", true),
            (SourceTool::Omp, "project/session.jsonl", true),
            (SourceTool::Pi, "session.jsonl", false),
            (SourceTool::Omp, "a/b/c.jsonl", false),
            (SourceTool::Pi, "project/session.json", false),
            (SourceTool::Omp, "project/session.json", false),
            // Droid / Claude accept any depth with a .jsonl extension.
            (SourceTool::Droid, "session.jsonl", true),
            (SourceTool::Claude, "a/b/c.jsonl", true),
            (SourceTool::Droid, "session.json", false),
            (SourceTool::Claude, "a/b/c", false),
            // Codex requires a rollout- prefix and a .jsonl extension.
            (SourceTool::Codex, "rollout-1.jsonl", true),
            (SourceTool::Codex, "deep/rollout-x.jsonl", true),
            (SourceTool::Codex, "session.jsonl", false),
            (SourceTool::Codex, "rollout-1.json", false),
            // Grok requires depth 3 and a name not ending in .lock.
            (SourceTool::Grok, "encoded/session-id/summary.json", true),
            (SourceTool::Grok, "encoded/session-id/active.lock", false),
            (SourceTool::Grok, "session-id/summary.json", false),
            (SourceTool::Grok, "a/b/c/d", false),
            (SourceTool::Agent, "0123456789abcdef0123456789abcdef/id/store.db", false),
        ];
        for (tool, path, expected) in cases {
            let actual = matches_tool_path(*tool, Path::new(path));
            assert_eq!(actual, *expected, "{tool:?} {path}");
        }
    }
}
