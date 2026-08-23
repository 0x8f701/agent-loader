#[cfg(unix)]
use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::{File, Metadata};
#[cfg(not(unix))]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::fs::Permissions;
use std::io::{self, BufWriter, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

#[cfg(unix)]
const DIRECTORY_OPEN_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
#[cfg(unix)]
const FILE_OPEN_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
#[cfg(any(target_os = "android", target_os = "linux"))]
const ENTRY_INSPECTION_FLAGS: libc::c_int = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
#[cfg(all(unix, not(any(target_os = "android", target_os = "linux"))))]
const ENTRY_INSPECTION_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
#[cfg(unix)]
const TEMP_FILE_FLAGS: libc::c_int =
    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
#[derive(Debug)]
pub struct OwnedFd {
    path: PathBuf,
    // Delete sharing is denied on every handle, pinning the complete directory chain.
    guards: Vec<File>,
}

#[cfg(all(not(unix), not(windows)))]
#[derive(Debug)]
pub struct OwnedFd {
    path: PathBuf,
    file: File,
}

/// Returns whether an existing regular file or directory is safely contained by `root`.
///
/// Both paths are interpreted as absolute paths (relative paths are based at the current
/// directory). Parent components, symlink components, and special files fail closed.
pub fn path_under_root(path: &Path, root: &Path) -> bool {
    let Ok(path) = absolute_without_parent_components(path) else {
        return false;
    };
    let Ok(root) = absolute_without_parent_components(root) else {
        return false;
    };
    if path.strip_prefix(&root).is_err() {
        return false;
    }

    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if metadata_is_link_or_reparse(&metadata) {
        return false;
    }
    if metadata.is_dir() {
        return open_directory_under_root(&path, &root).is_ok();
    }
    if !metadata.is_file() {
        return false;
    }

    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name() else {
        return false;
    };
    let Ok(directory) = open_directory_under_root(parent, &root) else {
        return false;
    };
    open_regular_file_at_os(&directory, name).is_ok()
}

/// Validates a top-level Pi/OMP tree session at `root/<project>/<session>.jsonl`.
pub fn is_tree_top_level_session(path: &Path, root: &Path) -> bool {
    let Some((path, root, relative)) = absolute_relative_path(path, root) else {
        return false;
    };
    let mut components = relative.components();
    let (Some(Component::Normal(_)), Some(Component::Normal(file_name)), None) =
        (components.next(), components.next(), components.next())
    else {
        return false;
    };
    if Path::new(file_name).extension() != Some(OsStr::new("jsonl")) {
        return false;
    }

    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(name) = path.file_name() else {
        return false;
    };
    let Ok(directory) = open_directory_under_root(parent, &root) else {
        return false;
    };
    open_regular_file_at_os(&directory, name).is_ok()
}

/// Validates a Grok summary at `root/<encoded-cwd>/<session-id>/summary.json`.
pub fn is_grok_summary(path: &Path, root: &Path) -> bool {
    let Some((path, root, relative)) = absolute_relative_path(path, root) else {
        return false;
    };
    let mut components = relative.components();
    let (
        Some(Component::Normal(_)),
        Some(Component::Normal(_)),
        Some(Component::Normal(file_name)),
        None,
    ) = (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    )
    else {
        return false;
    };
    if file_name != OsStr::new("summary.json") {
        return false;
    }

    path_under_root(&path, &root)
}

/// Validates an Agent store at `root/<32-hex-workspace>/<session-id>/store.db`.
pub fn is_agent_store(path: &Path, root: &Path) -> bool {
    let Some((path, root, relative)) = absolute_relative_path(path, root) else {
        return false;
    };
    let mut components = relative.components();
    let (
        Some(Component::Normal(workspace)),
        Some(Component::Normal(_)),
        Some(Component::Normal(file_name)),
        None,
    ) = (
        components.next(),
        components.next(),
        components.next(),
        components.next(),
    )
    else {
        return false;
    };
    let workspace = workspace.as_encoded_bytes();
    if workspace.len() != 32 || !workspace.iter().all(u8::is_ascii_hexdigit) {
        return false;
    }
    file_name == OsStr::new("store.db") && path_under_root(&path, &root)
}

/// Atomically replaces `path` with one compact JSON value per line.
///
/// The temporary file and destination are addressed relative to a descriptor for the parent
/// directory. The file is flushed and synced before rename, and the directory is synced after
/// rename. An existing regular file's permission bits are retained; symlinks and special files
/// are rejected rather than followed or replaced.
#[cfg(unix)]
pub fn atomic_write_jsonl<T: Serialize>(path: &Path, records: &[T]) -> Result<()> {
    let path = absolute_without_parent_components(path)
        .with_context(|| format!("validating output path {}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("output path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("output path has no file name: {}", path.display()))?;
    validate_single_name(name)
        .with_context(|| format!("validating output file name {}", path.display()))?;

    let parent_directory = open_absolute_directory(parent)
        .with_context(|| format!("opening output directory {}", parent.display()))?;
    let mode = match inspect_entry_at(&parent_directory, name) {
        Ok(metadata) => {
            if !metadata.is_file() {
                bail!("refusing to replace non-regular file {}", path.display());
            }
            metadata.permissions().mode() & 0o7777
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0o600,
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting output {}", path.display()));
        }
    };

    let (temporary_name, temporary_fd) = create_temporary_file_at(&parent_directory)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    let mut temporary_file = File::from(temporary_fd);

    let result = (|| -> Result<()> {
        {
            let mut writer = BufWriter::new(&mut temporary_file);
            for record in records {
                serde_json::to_writer(&mut writer, record).context("serializing JSONL record")?;
                writer.write_all(b"\n").context("writing JSONL newline")?;
            }
            writer.flush().context("flushing temporary JSONL file")?;
        }
        temporary_file
            .set_permissions(Permissions::from_mode(mode))
            .context("setting temporary JSONL file mode")?;
        temporary_file
            .sync_all()
            .context("syncing temporary JSONL file")?;
        rename_at(&parent_directory, &temporary_name, name)
            .with_context(|| format!("replacing output {}", path.display()))?;
        File::from(
            parent_directory
                .try_clone()
                .context("duplicating output directory descriptor")?,
        )
        .sync_all()
        .context("syncing output directory")?;
        Ok(())
    })();

    if result.is_err() {
        let _ = unlink_at(&parent_directory, &temporary_name);
    }
    result
}

#[cfg(not(unix))]
pub fn atomic_write_jsonl<T: Serialize>(path: &Path, records: &[T]) -> Result<()> {
    let path = absolute_without_parent_components(path)
        .with_context(|| format!("validating output path {}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("output path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("output path has no file name: {}", path.display()))?;
    validate_single_name(name)
        .with_context(|| format!("validating output file name {}", path.display()))?;
    let parent_directory = open_absolute_directory(parent)
        .with_context(|| format!("opening output directory {}", parent.display()))?;
    let permissions = match inspect_entry_at(&parent_directory, name) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
                bail!("refusing to replace non-regular file {}", path.display());
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("inspecting output {}", path.display())),
    };
    let (temporary_path, mut temporary_file) = create_temporary_file_at(&parent_directory)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    let result = (|| -> Result<()> {
        {
            let mut writer = BufWriter::new(&mut temporary_file);
            for record in records {
                serde_json::to_writer(&mut writer, record).context("serializing JSONL record")?;
                writer.write_all(b"\n").context("writing JSONL newline")?;
            }
            writer.flush().context("flushing temporary JSONL file")?;
        }
        if let Some(permissions) = permissions {
            temporary_file.set_permissions(permissions).context("setting temporary JSONL file permissions")?;
        }
        temporary_file.sync_all().context("syncing temporary JSONL file")?;
        validate_open_directory(&parent_directory)
            .with_context(|| format!("revalidating output directory {}", parent.display()))?;
        match inspect_entry_at(&parent_directory, name) {
            Ok(metadata) if metadata.is_file() && !metadata_is_link_or_reparse(&metadata) => {}
            Ok(_) => bail!("refusing to replace non-regular file {}", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("reinspecting output {}", path.display())),
        }
        std::fs::rename(&temporary_path, &path)
            .with_context(|| format!("replacing output {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

/// Opens `directory` beneath `root`, walking every component with `O_NOFOLLOW`.
///
/// The returned descriptor has close-on-exec set. Lexical containment is checked before any
/// traversal, and any parent component or symlink in either path is rejected.
#[cfg(unix)]
pub fn open_directory_under_root(directory: &Path, root: &Path) -> Result<OwnedFd> {
    let directory = absolute_without_parent_components(directory)
        .with_context(|| format!("validating directory {}", directory.display()))?;
    let root = absolute_without_parent_components(root)
        .with_context(|| format!("validating root {}", root.display()))?;
    directory.strip_prefix(&root).with_context(|| {
        format!(
            "directory {} is not beneath root {}",
            directory.display(),
            root.display()
        )
    })?;

    open_absolute_directory(&directory)
}

#[cfg(not(unix))]
pub fn open_directory_under_root(directory: &Path, root: &Path) -> Result<OwnedFd> {
    let directory = absolute_without_parent_components(directory)
        .with_context(|| format!("validating directory {}", directory.display()))?;
    let root = absolute_without_parent_components(root)
        .with_context(|| format!("validating root {}", root.display()))?;
    directory.strip_prefix(&root).with_context(|| format!(
        "directory {} is not beneath root {}", directory.display(), root.display()
    ))?;
    open_absolute_directory(&directory)
}

/// Opens one regular file relative to an already trusted directory descriptor.
///
/// Names containing separators, `.`/`..`, symlinks, directories, and special files return
/// `None`. The returned file descriptor has close-on-exec set.
pub fn open_regular_file_at(directory: &OwnedFd, name: &str) -> Option<(File, Metadata)> {
    open_regular_file_at_os(directory, OsStr::new(name)).ok()
}

fn absolute_relative_path(path: &Path, root: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let path = absolute_without_parent_components(path).ok()?;
    let root = absolute_without_parent_components(root).ok()?;
    let relative = path.strip_prefix(&root).ok()?.to_path_buf();
    Some((path, root, relative))
}

#[cfg(unix)]
fn absolute_without_parent_components(path: &Path) -> io::Result<PathBuf> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("parent path component is not allowed: {}", path.display()),
        ));
    }

    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("parent path component is not allowed: {}", path.display()),
                ));
            }
            Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported path prefix: {}", path.display()),
                ));
            }
        }
    }
    Ok(normalized)
}

#[cfg(not(unix))]
fn absolute_without_parent_components(path: &Path) -> io::Result<PathBuf> {
    if path.components().any(|component| matches!(component, Component::ParentDir)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!(
            "parent path component is not allowed: {}", path.display()
        )));
    }
    let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                validate_path_prefix(prefix.as_os_str(), &path)?;
                normalized.push(prefix.as_os_str());
            }
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(name) => {
                validate_platform_name(name)?;
                normalized.push(name);
            }
            Component::ParentDir => return Err(io::Error::new(io::ErrorKind::InvalidInput, format!(
                "parent path component is not allowed: {}", path.display()
            ))),
        }
    }
    if !normalized.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("path is not absolute: {}", path.display())));
    }
    Ok(normalized)
}

#[cfg(unix)]
fn open_absolute_directory(directory: &Path) -> Result<OwnedFd> {
    debug_assert!(directory.is_absolute());
    let mut descriptor = openat_owned(
        libc::AT_FDCWD,
        OsStr::new("/"),
        DIRECTORY_OPEN_FLAGS,
    )
    .context("opening filesystem root")?;
    for component in directory.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                descriptor = openat_owned(descriptor.as_raw_fd(), name, DIRECTORY_OPEN_FLAGS)
                    .with_context(|| format!("opening directory component {:?}", name))?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                bail!("unsafe directory path {}", directory.display());
            }
        }
    }
    Ok(descriptor)
}

#[cfg(windows)]
fn open_absolute_directory(directory: &Path) -> Result<OwnedFd> {
    debug_assert!(directory.is_absolute());
    let mut current = PathBuf::new();
    let mut guards = Vec::new();
    for component in directory.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => {
                current.push(component.as_os_str());
                guards.push(open_directory_no_follow(&current).context("opening filesystem root")?);
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                validate_platform_name(name)?;
                current.push(name);
                guards.push(open_directory_no_follow(&current)
                    .with_context(|| format!("opening directory component {:?}", name))?);
            }
            Component::ParentDir => bail!("unsafe directory path {}", directory.display()),
        }
    }
    if guards.is_empty() {
        guards.push(open_directory_no_follow(directory).context("opening filesystem root")?);
    }
    Ok(OwnedFd { path: directory.to_path_buf(), guards })
}

#[cfg(all(not(unix), not(windows)))]
fn open_absolute_directory(directory: &Path) -> Result<OwnedFd> {
    debug_assert!(directory.is_absolute());
    let mut current = PathBuf::new();
    let mut descriptor = None;
    for component in directory.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(name) => {
                validate_platform_name(name)?;
                current.push(name);
                descriptor = Some(open_directory_no_follow(&current)
                    .with_context(|| format!("opening directory component {:?}", name))?);
            }
            Component::ParentDir => bail!("unsafe directory path {}", directory.display()),
        }
    }
    let file = match descriptor {
        Some(file) => file,
        None => open_directory_no_follow(directory).context("opening filesystem root")?,
    };
    Ok(OwnedFd { path: directory.to_path_buf(), file })
}

#[cfg(unix)]
fn open_regular_file_at_os(directory: &OwnedFd, name: &OsStr) -> io::Result<(File, Metadata)> {
    validate_single_name(name)?;
    let descriptor = openat_owned(directory.as_raw_fd(), name, FILE_OPEN_FLAGS)?;
    let file = File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened entry is not a regular file",
        ));
    }
    Ok((file, metadata))
}

#[cfg(not(unix))]
fn open_regular_file_at_os(directory: &OwnedFd, name: &OsStr) -> io::Result<(File, Metadata)> {
    validate_single_name(name)?;
    validate_open_directory(directory)?;
    let file = open_regular_no_follow(&directory.path.join(name))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "opened entry is not a regular file"));
    }
    Ok((file, metadata))
}

#[cfg(unix)]
fn inspect_entry_at(directory: &OwnedFd, name: &OsStr) -> io::Result<Metadata> {
    validate_single_name(name)?;
    let descriptor = openat_owned(directory.as_raw_fd(), name, ENTRY_INSPECTION_FLAGS)?;
    File::from(descriptor).metadata()
}

#[cfg(not(unix))]
fn inspect_entry_at(directory: &OwnedFd, name: &OsStr) -> io::Result<Metadata> {
    validate_single_name(name)?;
    validate_open_directory(directory)?;
    let metadata = std::fs::symlink_metadata(directory.path.join(name))?;
    if metadata_is_link_or_reparse(&metadata) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "entry is a symbolic link or reparse point"));
    }
    Ok(metadata)
}

fn validate_single_name(name: &OsStr) -> io::Result<()> {
    let mut components = Path::new(name).components();
    let Some(Component::Normal(component)) = components.next() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry name must be exactly one normal path component"));
    };
    if components.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry name must be exactly one normal path component"));
    }
    validate_platform_name(component)
}

#[cfg(unix)]
fn openat_owned(directory: RawFd, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let name = c_string(name)?;
    // SAFETY: `name` is NUL-terminated and valid for the duration of the syscall. On success,
    // openat returns a new descriptor owned by this function, which is immediately transferred
    // exactly once into `OwnedFd`.
    let raw_fd = unsafe { libc::openat(directory, name.as_ptr(), flags) };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw_fd` was freshly returned by a successful openat call and is uniquely owned.
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

#[cfg(unix)]
fn create_temporary_file_at(directory: &OwnedFd) -> io::Result<(CString, OwnedFd)> {
    for _ in 0..128 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = CString::new(format!(
            ".al-jsonl-{}-{sequence:016x}.tmp",
            std::process::id()
        ))
        .expect("generated temporary file name has no NUL bytes");
        // SAFETY: `name` is a valid NUL-terminated path component. O_CREAT is present, so the
        // mode argument is supplied. A successful call returns a new uniquely owned descriptor.
        let raw_fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                TEMP_FILE_FLAGS,
                0o600 as libc::c_uint,
            )
        };
        if raw_fd >= 0 {
            // SAFETY: `raw_fd` was freshly returned by openat and has not been transferred.
            return Ok((name, unsafe { OwnedFd::from_raw_fd(raw_fd) }));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file name",
    ))
}

#[cfg(unix)]
fn rename_at(directory: &OwnedFd, source: &CString, destination: &OsStr) -> io::Result<()> {
    let destination = c_string(destination)?;
    // SAFETY: both names are valid NUL-terminated path components, and both directory arguments
    // are the same live descriptor. renameat does not retain either pointer or descriptor.
    let result = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlink_at(directory: &OwnedFd, name: &CString) -> io::Result<()> {
    // SAFETY: `name` is a valid NUL-terminated path component and `directory` remains live for
    // the syscall. No pointer or descriptor is retained by unlinkat.
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn c_string(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path component contains a NUL byte",
        )
    })
}

#[cfg(unix)]
fn validate_platform_name(_name: &OsStr) -> io::Result<()> { Ok(()) }

#[cfg(not(unix))]
fn validate_path_prefix(prefix: &OsStr, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        match Path::new(prefix).components().next() {
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::UNC(_, _)) => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unsupported path prefix: {}", path.display()))),
        }
    }
    #[cfg(not(windows))]
    { let _ = (prefix, path); Ok(()) }
}

#[cfg(windows)]
fn validate_platform_name(name: &OsStr) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let name: Vec<u16> = name.encode_wide().collect();
    let invalid_character = name.iter().any(|character| *character == 0 || *character < 32
        || matches!(*character, 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124));
    let ambiguous_ending = matches!(name.last(), Some(32 | 46));
    let stem_end = name.iter().position(|character| *character == 46).unwrap_or(name.len());
    let stem = &name[..stem_end];
    let reserved = windows_name_eq(stem, b"CON") || windows_name_eq(stem, b"PRN")
        || windows_name_eq(stem, b"AUX") || windows_name_eq(stem, b"NUL")
        || windows_name_eq(stem, b"CLOCK$") || windows_numbered_device(stem, b"COM")
        || windows_numbered_device(stem, b"LPT");
    if invalid_character || ambiguous_ending || reserved {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry name is unsafe on Windows"));
    }
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn validate_platform_name(_name: &OsStr) -> io::Result<()> { Ok(()) }

#[cfg(windows)]
fn windows_name_eq(name: &[u16], expected: &[u8]) -> bool {
    name.len() == expected.len() && name.iter().zip(expected).all(|(actual, expected)| {
        let uppercase = u16::from(*expected);
        *actual == uppercase || *actual == uppercase + u16::from(expected.is_ascii_alphabetic()) * 32
    })
}

#[cfg(windows)]
fn windows_numbered_device(name: &[u16], prefix: &[u8; 3]) -> bool {
    name.len() == 4 && windows_name_eq(&name[..3], prefix) && matches!(name[3], 49..=57)
}

#[cfg(windows)]
fn validate_open_directory(directory: &OwnedFd) -> io::Result<()> {
    if directory.guards.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "directory has no held handles"));
    }
    for guard in &directory.guards {
        let metadata = guard.metadata()?;
        if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "directory handle is not a regular directory"));
        }
    }
    open_directory_no_follow(&directory.path).map(drop)
}

#[cfg(all(not(unix), not(windows)))]
fn validate_open_directory(directory: &OwnedFd) -> io::Result<()> {
    let metadata = directory.file.metadata()?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "directory handle is not a regular directory"));
    }
    open_absolute_directory(&directory.path).map(|_| ()).map_err(io::Error::other)
}

#[cfg(not(unix))]
fn create_temporary_file_at(directory: &OwnedFd) -> io::Result<(PathBuf, File)> {
    validate_open_directory(directory)?;
    for _ in 0..128 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(".al-jsonl-{}-{sequence:016x}.tmp", std::process::id());
        validate_single_name(OsStr::new(&name))?;
        let path = directory.path.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not allocate a unique temporary file name"))
}

#[cfg(windows)]
fn open_directory_no_follow(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata_is_link_or_reparse(&metadata) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "path component is not a regular directory"));
    }
    Ok(file)
}

#[cfg(all(not(unix), not(windows)))]
fn open_directory_no_follow(path: &Path) -> io::Result<File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "path component is not a regular directory"));
    }
    File::open(path)
}

#[cfg(windows)]
fn open_regular_no_follow(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)
}

#[cfg(all(not(unix), not(windows)))]
fn open_regular_no_follow(path: &Path) -> io::Result<File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "entry is not a regular file"));
    }
    File::open(path)
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(all(not(unix), not(windows)))]
fn metadata_is_link_or_reparse(metadata: &Metadata) -> bool { metadata.file_type().is_symlink() }

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn containment_rejects_outside_prefixes_parent_components_and_symlinks() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("sessions");
        let sibling = temporary.path().join("sessions-other");
        fs::create_dir_all(root.join("project")).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let inside = root.join("project/session.jsonl");
        let outside = sibling.join("session.jsonl");
        fs::write(&inside, "{}\n").unwrap();
        fs::write(&outside, "{}\n").unwrap();
        symlink(&inside, root.join("project/linked.jsonl")).unwrap();

        assert!(path_under_root(&inside, &root));
        assert!(!path_under_root(&outside, &root));
        assert!(!path_under_root(
            &root.join("project/../project/session.jsonl"),
            &root
        ));
        assert!(!path_under_root(&root.join("project/linked.jsonl"), &root));
    }

    #[test]
    fn tree_validation_accepts_only_regular_top_level_jsonl_sessions() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("tree");
        fs::create_dir_all(root.join("project/nested")).unwrap();
        let session = root.join("project/session.jsonl");
        let nested = root.join("project/nested/session.jsonl");
        fs::write(&session, "{}\n").unwrap();
        fs::write(&nested, "{}\n").unwrap();
        fs::create_dir(root.join("project/directory.jsonl")).unwrap();
        symlink(&session, root.join("project/symlink.jsonl")).unwrap();

        assert!(is_tree_top_level_session(&session, &root));
        assert!(!is_tree_top_level_session(&nested, &root));
        assert!(!is_tree_top_level_session(
            &root.join("project/directory.jsonl"),
            &root
        ));
        assert!(!is_tree_top_level_session(
            &root.join("project/symlink.jsonl"),
            &root
        ));
    }

    #[test]
    fn grok_validation_requires_the_exact_summary_depth() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("grok");
        let session_directory = root.join("encoded-cwd/session-id");
        fs::create_dir_all(session_directory.join("subagent")).unwrap();
        let summary = session_directory.join("summary.json");
        let nested_summary = session_directory.join("subagent/summary.json");
        let wrong_name = session_directory.join("other.json");
        fs::write(&summary, "{}").unwrap();
        fs::write(&nested_summary, "{}").unwrap();
        fs::write(&wrong_name, "{}").unwrap();

        assert!(is_grok_summary(&summary, &root));
        assert!(!is_grok_summary(&nested_summary, &root));
        assert!(!is_grok_summary(&wrong_name, &root));
    }

    #[test]
    fn agent_validation_requires_exact_depth_hash_and_store_name() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("chats");
        let directory = root.join("0123456789abcdef0123456789abcdef/session-id");
        fs::create_dir_all(directory.join("nested")).unwrap();
        let store = directory.join("store.db");
        let nested = directory.join("nested/store.db");
        let wrong = directory.join("meta.json");
        let bad_hash = root.join("not-a-workspace-hash/session-id/store.db");
        fs::create_dir_all(bad_hash.parent().unwrap()).unwrap();
        for path in [&store, &nested, &wrong, &bad_hash] { fs::write(path, "x").unwrap(); }
        assert!(is_agent_store(&store, &root));
        assert!(!is_agent_store(&nested, &root));
        assert!(!is_agent_store(&wrong, &root));
        assert!(!is_agent_store(&bad_hash, &root));
    }

    #[test]
    fn directory_open_rejects_symlink_components_and_dot_dot() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("root");
        let real = root.join("real/session");
        fs::create_dir_all(&real).unwrap();
        symlink(root.join("real"), root.join("linked")).unwrap();

        assert!(open_directory_under_root(&real, &root).is_ok());
        assert!(open_directory_under_root(&root.join("linked/session"), &root).is_err());
        assert!(open_directory_under_root(&root.join("real/../real/session"), &root).is_err());
    }

    #[test]
    fn regular_open_rejects_final_symlinks_directories_and_unsafe_names() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("root");
        fs::create_dir_all(root.join("session/directory")).unwrap();
        fs::write(root.join("session/summary.json"), "{}").unwrap();
        symlink("summary.json", root.join("session/linked.json")).unwrap();
        let directory = open_directory_under_root(&root.join("session"), &root).unwrap();

        assert!(open_regular_file_at(&directory, "summary.json").is_some());
        assert!(open_regular_file_at(&directory, "linked.json").is_none());
        assert!(open_regular_file_at(&directory, "directory").is_none());
        assert!(open_regular_file_at(&directory, "../summary.json").is_none());
        assert!(open_regular_file_at(&directory, ".").is_none());
    }

    #[test]
    fn atomic_jsonl_write_preserves_mode_and_replaces_complete_contents() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("session.jsonl");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        atomic_write_jsonl(&path, &[json!({"one": 1}), json!({"two": "✓"})]).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"one\":1}\n{\"two\":\"✓\"}\n"
        );
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o640);
    }

    #[test]
    fn atomic_jsonl_write_rejects_final_symlinks_and_directories() {
        let temporary = tempdir().unwrap();
        let real = temporary.path().join("real.jsonl");
        let linked = temporary.path().join("linked.jsonl");
        let directory = temporary.path().join("directory.jsonl");
        fs::write(&real, "unchanged\n").unwrap();
        symlink(&real, &linked).unwrap();
        fs::create_dir(&directory).unwrap();

        assert!(atomic_write_jsonl(&linked, &[json!({"changed": true})]).is_err());
        assert!(atomic_write_jsonl(&directory, &[json!({"changed": true})]).is_err());
        assert_eq!(fs::read_to_string(&real).unwrap(), "unchanged\n");
    }
}
