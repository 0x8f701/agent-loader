//! Native Antigravity transfers never walk the credential-bearing application root.
use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

/// Reject symlinks in every existing component, including ancestors of a new path.
pub(crate) fn check_path(path: &Path) -> Result<()> {
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            bail!("parent traversal in Antigravity path");
        }
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("refusing Antigravity symlink {}", current.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspecting Antigravity path"),
        }
    }
    Ok(())
}

pub(crate) fn brain_path(carrier: &Path) -> Result<std::path::PathBuf> {
    let id = carrier.file_stem().and_then(|value| value.to_str()).context("missing conversation UUID")?;
    uuid::Uuid::parse_str(id).context("invalid Antigravity conversation UUID")?;
    let parent = carrier.parent().context("missing Antigravity carrier parent")?;
    let root = if parent.file_name() == Some(std::ffi::OsStr::new("conversations")) {
        parent.parent().context("missing Antigravity root")?
    } else {
        parent
    };
    Ok(root.join("brain").join(id))
}

/// VACUUM INTO reads a SQLite transaction snapshot, including committed WAL pages.
#[cfg(feature = "catalog")]
pub(crate) fn snapshot(source: &Path, destination: &Path) -> Result<()> {
    check_path(source)?;
    check_path(destination)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = source.as_os_str().to_owned();
        sidecar.push(suffix);
        check_path(Path::new(&sidecar))?;
    }
    if destination.try_exists()? {
        bail!("destination already exists: {}", destination.display());
    }
    if !fs::symlink_metadata(source)?.is_file() {
        bail!("Antigravity source is not a regular file");
    }
    let parent = destination.parent().context("missing destination parent")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let staged = staging.path().join("snapshot.db");
    let connection = rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute("VACUUM INTO ?1", [staged.to_str().context("snapshot path is not UTF-8")?])?;
    fs::hard_link(&staged, destination).context("installing exclusive SQLite snapshot")?;
    Ok(())
}

#[cfg(not(feature = "catalog"))]
pub(crate) fn snapshot(_source: &Path, _destination: &Path) -> Result<()> {
    bail!("Antigravity transfers require the catalog feature")
}

// Fixed program; all paths and operation arguments are separately quoted argv, never code.
// Python 3 with its standard sqlite3 module is required on both sync endpoints.
#[cfg(any(feature = "cli", test))]
pub(crate) const SYNC_PROGRAM: &str = r#"
import json, os, pathlib, shutil, sqlite3, stat, sys, tempfile, uuid
root = pathlib.Path(sys.argv[2])
mode = sys.argv[1]
def checked(path):
    path = pathlib.Path(path)
    for part in [path, *path.parents]:
        if part.is_symlink():
            raise RuntimeError('symlink in Antigravity path')
    return path
def valid_id(text):
    try:
        return str(uuid.UUID(text)) == text.lower()
    except ValueError:
        return False
def relative(text):
    if not isinstance(text, str) or '\\' in text or ':' in text:
        raise RuntimeError('unsafe cross-platform relative path')
    parts = pathlib.PurePosixPath(text).parts
    if not parts or any(p in ('', '.', '..') for p in parts) or text.startswith('/'):
        raise RuntimeError('unsafe relative path')
    if len(parts) == 2 and parts[0] == 'conversations' and parts[1].endswith('.db') and valid_id(parts[1][:-3]):
        return text
    if len(parts) >= 3 and parts[0] == 'brain' and valid_id(parts[1]):
        return text
    raise RuntimeError('unexpected Antigravity transfer path')
def varint(number):
    encoded = bytearray()
    while number > 127:
        encoded.append((number & 127) | 128)
        number >>= 7
    encoded.append(number)
    return bytes(encoded)
def field(number, value):
    if isinstance(value, int):
        return varint(number << 3) + varint(value)
    return varint((number << 3) | 2) + varint(len(value)) + value
def fields(blob):
    def read(offset):
        number = 0
        for shift in range(0, 70, 7):
            if offset >= len(blob):
                raise RuntimeError('truncated protobuf')
            byte = blob[offset]
            offset += 1
            number |= (byte & 127) << shift
            if byte < 128:
                return number, offset
        raise RuntimeError('invalid protobuf varint')
    offset = 0
    result = []
    while offset < len(blob):
        tag, offset = read(offset)
        wire = tag & 7
        if wire == 0:
            value, offset = read(offset)
        elif wire == 2:
            length, offset = read(offset)
            end = offset + length
            if end > len(blob):
                raise RuntimeError('truncated protobuf field')
            value, offset = blob[offset:end], end
        elif wire in (1, 5):
            end = offset + (8 if wire == 1 else 4)
            if end > len(blob):
                raise RuntimeError('truncated protobuf fixed field')
            value, offset = blob[offset:end], end
        else:
            raise RuntimeError('unsupported protobuf wire type')
        result.append((tag >> 3, value))
    return result
def register(db, saved_summary=None):
    checked(str(db) + '-wal')
    checked(str(db) + '-shm')
    import datetime
    checked(root / 'conversation_summaries.db')
    checked(str(root / 'conversation_summaries.db') + '-wal')
    checked(str(root / 'conversation_summaries.db') + '-shm')
    with sqlite3.connect(db.as_uri() + '?mode=ro', uri=True) as source:
        trajectory, conversation, trajectory_type, origin = source.execute('SELECT trajectory_id,cascade_id,trajectory_type,source FROM trajectory_meta LIMIT 1').fetchone()
        if conversation != db.stem:
            raise RuntimeError('carrier conversation identity mismatch')
        metadata = source.execute("SELECT data FROM trajectory_metadata_blob WHERE id='main'").fetchone()[0]
        count = source.execute('SELECT COUNT(*) FROM steps').fetchone()[0]
        last_user = source.execute('SELECT COALESCE(MAX(idx),-1) FROM steps WHERE step_type=14').fetchone()[0]
        first_user = source.execute('SELECT step_payload FROM steps WHERE step_type=14 ORDER BY idx LIMIT 1').fetchone()
    values = fields(metadata)
    workspaces = [value for number, value in values if number == 1 and isinstance(value, bytes)]
    uris = [value.decode('utf-8') for workspace in workspaces for number, value in fields(workspace) if number == 1]
    if not uris:
        uris = [value.decode('utf-8') for number, value in values if number == 7 and isinstance(value, bytes)]
    created = next((value for number, value in values if number == 2), b'')
    now = datetime.datetime.now(datetime.timezone.utc)
    timestamp = field(1, int(now.timestamp()))
    title = ''
    if first_user is not None:
        payload = next((value for number, value in fields(first_user[0]) if number == 19), b'')
        text = next((value for number, value in fields(payload) if number == 2), b'')
        title = text.decode('utf-8')[:80]
    raw = field(1, title.encode()) + field(2, count) + field(3, timestamp) + field(4, trajectory.encode()) + field(5, 1) + field(7, created) + field(10, timestamp) + field(17, metadata) + field(20, origin) + field(22, trajectory_type)
    raw += b''.join(field(9, workspace) for workspace in workspaces)
    if last_user >= 0:
        raw += field(16, last_user)
    with sqlite3.connect(root / 'conversation_summaries.db') as summaries:
        summaries.execute('CREATE TABLE IF NOT EXISTS conversation_summaries (conversation_id TEXT PRIMARY KEY,title TEXT NOT NULL DEFAULT "",preview TEXT NOT NULL DEFAULT "",step_count INTEGER NOT NULL DEFAULT 0,last_modified_time datetime NOT NULL,workspace_uris TEXT NOT NULL,status TEXT NOT NULL DEFAULT "",source TEXT NOT NULL DEFAULT "",project_id TEXT NOT NULL DEFAULT "",agent_name TEXT NOT NULL DEFAULT "",parent_conversation_id TEXT NOT NULL DEFAULT "",nesting_depth INTEGER NOT NULL DEFAULT 0,battle_id TEXT NOT NULL DEFAULT "",winning_conversation_id TEXT NOT NULL DEFAULT "",not_fully_idle numeric NOT NULL DEFAULT false,killed numeric NOT NULL DEFAULT false,last_user_input_time datetime NOT NULL,last_user_input_step_index INTEGER NOT NULL DEFAULT -1,app_data_dir TEXT NOT NULL DEFAULT "",raw_summary BLOB,group_id TEXT NOT NULL DEFAULT "")')
        existing_summary = summaries.execute('SELECT 1 FROM conversation_summaries WHERE conversation_id=?', (conversation,)).fetchone()
        summaries.execute('INSERT INTO conversation_summaries (conversation_id,title,step_count,last_modified_time,workspace_uris,status,project_id,last_user_input_time,last_user_input_step_index,app_data_dir,raw_summary) VALUES (?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(conversation_id) DO NOTHING', (conversation, title, count, now.isoformat(), json.dumps(uris), 'CASCADE_RUN_STATUS_IDLE', 'default-cli-project', now.isoformat(), last_user, 'antigravity-cli', raw))
        if saved_summary is not None and existing_summary is None:
            import base64
            if saved_summary.get('conversation_id') != conversation or conversation != db.stem:
                raise RuntimeError('summary conversation identity mismatch')
            if set(json.loads(saved_summary['workspace_uris'])) != set(uris):
                raise RuntimeError('summary workspace identity mismatch')
            columns = {row[1] for row in summaries.execute('PRAGMA table_info(conversation_summaries)')}
            values = {key: value for key, value in saved_summary.items() if key in columns}
            if values.get('raw_summary') is not None:
                values['raw_summary'] = base64.b64decode(values['raw_summary'], validate=True)
            names = [key for key in values if key != 'conversation_id']
            summaries.execute('UPDATE conversation_summaries SET ' + ','.join('"' + key + '"=?' for key in names) + ' WHERE conversation_id=?', [values[key] for key in names] + [conversation])
    import urllib.parse
    cache = checked(root / 'cache' / 'last_conversations.json')
    cache.parent.mkdir(parents=True, exist_ok=True)
    lock_path = checked(root / 'cache' / '.al-routing.lock')
    lock = os.open(lock_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        routing = json.loads(cache.read_text()) if cache.exists() else {}
        if not isinstance(routing, dict):
            raise RuntimeError('invalid conversation routing cache')
        for uri in uris:
            parsed = urllib.parse.urlparse(uri)
            if parsed.scheme == 'file' and parsed.netloc in ('', 'localhost'):
                from urllib.request import url2pathname
                routing.setdefault(url2pathname(parsed.path), conversation)
        descriptor, staging = tempfile.mkstemp(prefix='.al-routing-', dir=cache.parent)
        try:
            with os.fdopen(descriptor, 'w') as output:
                json.dump(routing, output)
            os.replace(staging, cache)
        finally:
            if os.path.exists(staging):
                os.unlink(staging)
    finally:
        os.close(lock)
        os.unlink(lock_path)
def inventory():
    checked(root)
    carriers = root / 'conversations'
    checked(carriers)
    if not carriers.exists():
        return None
    files = []
    for db in sorted(carriers.iterdir()):
        if db.suffix != '.db' or not valid_id(db.stem):
            continue
        checked(db)
        if not db.is_file():
            raise RuntimeError('non-file conversation')
        files.append('conversations/' + db.name)
        brain = checked(root / 'brain' / db.stem)
        if not brain.exists():
            continue
        for base, dirs, names in os.walk(brain, followlinks=False):
            for name in dirs + names:
                checked(pathlib.Path(base) / name)
            for name in sorted(names):
                path = pathlib.Path(base) / name
                if not stat.S_ISREG(path.stat().st_mode):
                    raise RuntimeError('non-regular brain file')
                files.append(path.relative_to(root).as_posix())
    return files
checked(root)
if mode == 'inventory':
    files = inventory()
    if files is None:
        sys.exit(3)
    sys.stdout.buffer.write(b'\0'.join(os.fsencode(p) for p in files) + b'\0')
elif mode == 'export':
    requested = [relative(p) for p in sys.argv[3:]]
    with tempfile.TemporaryDirectory(prefix='al-antigravity-') as staging:
        for index, name in enumerate(requested):
            saved_summary = None
            path = checked(root / name)
            if not stat.S_ISREG(path.stat().st_mode):
                raise RuntimeError('non-regular transfer file')
            if name.startswith('conversations/'):
                checked(str(path) + '-wal')
                checked(str(path) + '-shm')
                snapshot = pathlib.Path(staging) / (str(index) + '.db')
                with sqlite3.connect(path.as_uri() + '?mode=ro', uri=True) as source, sqlite3.connect(snapshot) as destination:
                    source.backup(destination)
                with sqlite3.connect(snapshot) as copied:
                    identity = copied.execute('SELECT cascade_id FROM trajectory_meta LIMIT 1').fetchone()[0]
                    if identity != pathlib.Path(name).stem:
                        raise RuntimeError('carrier conversation identity mismatch')
                summary_path = checked(root / 'conversation_summaries.db')
                checked(str(summary_path) + '-wal')
                checked(str(summary_path) + '-shm')
                if summary_path.exists():
                    import base64
                    with sqlite3.connect(summary_path.as_uri() + '?mode=ro', uri=True) as summaries:
                        summaries.row_factory = sqlite3.Row
                        row = summaries.execute('SELECT * FROM conversation_summaries WHERE conversation_id=?', (identity,)).fetchone()
                        if row is not None:
                            saved_summary = dict(row)
                            if saved_summary.get('raw_summary') is not None:
                                saved_summary['raw_summary'] = base64.b64encode(saved_summary['raw_summary']).decode()
                path = snapshot
            with open(path, 'rb') as source:
                size = os.fstat(source.fileno()).st_size
                sys.stdout.buffer.write(json.dumps([name, size, saved_summary]).encode() + b'\n')
                shutil.copyfileobj(source, sys.stdout.buffer)
elif mode == 'install':
    expected = {relative(p) for p in sys.argv[3:]}
    checked(root)
    root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.al-antigravity-', dir=root) as staging:
        received = set()
        saved_summaries = {}
        while True:
            header = sys.stdin.buffer.readline(1048577)
            if len(header) > 1048576:
                raise RuntimeError('transfer header exceeds limit')
            if not header:
                break
            name, size, saved_summary = json.loads(header)
            relative(name)
            if name not in expected or name in received or not isinstance(size, int) or size < 0:
                raise RuntimeError('unexpected transfer record')
            received.add(name)
            if saved_summary is not None:
                if not isinstance(saved_summary, dict):
                    raise RuntimeError('invalid summary object')
                integer_columns = {'step_count', 'nesting_depth', 'not_fully_idle', 'killed', 'last_user_input_step_index'}
                for key, value in saved_summary.items():
                    if not isinstance(key, str) or not isinstance(value, (str, int, float, type(None))):
                        raise RuntimeError('invalid summary scalar')
                    if key in integer_columns and not isinstance(value, int):
                        raise RuntimeError('invalid summary integer')
                    if key not in integer_columns and value is not None and not isinstance(value, str):
                        raise RuntimeError('invalid summary text')
                import base64
                if saved_summary.get('raw_summary') is not None:
                    base64.b64decode(saved_summary['raw_summary'], validate=True)
                saved_summaries[name] = saved_summary
            staged = pathlib.Path(staging) / name
            staged.parent.mkdir(parents=True, exist_ok=True)
            with open(staged, 'xb') as destination:
                remaining = size
                while remaining:
                    chunk = sys.stdin.buffer.read(min(remaining, 1048576))
                    if not chunk:
                        raise RuntimeError('truncated transfer')
                    destination.write(chunk)
                    remaining -= len(chunk)
        if received != expected:
            raise RuntimeError('missing transfer record')
        for name in sorted(received, key=lambda p: p.startswith('conversations/')):
            destination = checked(root / name)
            destination.parent.mkdir(parents=True, exist_ok=True)
            checked(destination)
            try:
                os.link(pathlib.Path(staging) / name, destination)
            except FileExistsError:
                if destination.is_symlink() or not destination.is_file():
                    raise RuntimeError('unsafe existing destination')
                continue
            if name.startswith('conversations/'):
                try:
                    brain = checked(root / 'brain' / pathlib.Path(name).stem)
                    brain.mkdir(parents=True, exist_ok=True)
                    register(destination, saved_summaries.get(name))
                except BaseException:
                    destination.unlink()
                    raise
else:
    raise RuntimeError('unknown transfer operation')
"#;

#[cfg(all(test, feature = "catalog"))]
mod tests {
    use super::*;

    #[test]
    fn python_transfer_preserves_wal_brain_and_existing_destination() {
        use std::process::{Command, Stdio};
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        let id = "00000000-0000-4000-8000-000000000001";
        let relative = format!("conversations/{id}.db");
        let brain = format!("brain/{id}/note.md");
        fs::create_dir_all(source.join("conversations")).unwrap();
        fs::create_dir_all(source.join(format!("brain/{id}"))).unwrap();
        fs::write(source.join(&brain), b"native brain").unwrap();
        fs::write(source.join("auth.json"), b"never transfer").unwrap();
        let connection = rusqlite::Connection::open(source.join(&relative)).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE trajectory_meta(trajectory_id TEXT,cascade_id TEXT,trajectory_type INTEGER,source INTEGER); CREATE TABLE steps(idx INTEGER,step_type INTEGER,step_payload BLOB); CREATE TABLE trajectory_metadata_blob(id TEXT,data BLOB);").unwrap();
        connection.execute("INSERT INTO trajectory_meta VALUES (?1,?1,4,17)", [id]).unwrap();
        let workspace = b"file:///workspace";
        let mut metadata = vec![10, 19, 10, 17];
        metadata.extend_from_slice(workspace);
        metadata.extend_from_slice(&[18, 2, 8, 1]);
        connection.execute("INSERT INTO trajectory_metadata_blob VALUES ('main',?1)", [metadata]).unwrap();
        let user = vec![8_u8, 14, 32, 3, 154, 1, 7, 18, 5, b'h', b'e', b'l', b'l', b'o'];
        connection.execute("INSERT INTO steps VALUES (0,14,?1)", [user.clone()]).unwrap();
        let exported = Command::new("python3").args(["-c", SYNC_PROGRAM, "export"]).arg(&source).args([&relative, &brain]).output().unwrap();
        assert!(exported.status.success(), "{}", String::from_utf8_lossy(&exported.stderr));
        for attempt in 0..2 {
            let mut child = Command::new("python3").args(["-c", SYNC_PROGRAM, "install"]).arg(&destination).args([&relative, &brain]).stdin(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            std::io::Write::write_all(child.stdin.take().as_mut().unwrap(), &exported.stdout).unwrap();
            let installed = child.wait_with_output().unwrap();
            assert!(installed.status.success(), "{}", String::from_utf8_lossy(&installed.stderr));
            if attempt == 0 { fs::write(destination.join(&brain), b"destination owns this").unwrap(); }
        }
        assert_eq!(fs::read(destination.join(&brain)).unwrap(), b"destination owns this");
        assert!(!destination.join("auth.json").exists());
        let copied = rusqlite::Connection::open(destination.join(&relative)).unwrap();
        let payload: Vec<u8> = copied.query_row("SELECT step_payload FROM steps", [], |row| row.get(0)).unwrap();
        assert_eq!(payload, user);
        let summaries = rusqlite::Connection::open(destination.join("conversation_summaries.db")).unwrap();
        let (title, uris): (String, String) = summaries.query_row("SELECT title,workspace_uris FROM conversation_summaries WHERE conversation_id=?1", [id], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(title, "hello");
        assert_eq!(serde_json::from_str::<Vec<String>>(&uris).unwrap(), ["file:///workspace"]);
        let routing: serde_json::Value = serde_json::from_slice(&fs::read(destination.join("cache/last_conversations.json")).unwrap()).unwrap();
        assert_eq!(routing["/workspace"], id);
    }

    #[test]
    fn snapshot_includes_committed_wal_and_never_overwrites() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.db");
        let destination = temporary.path().join("destination.db");
        let connection = rusqlite::Connection::open(&source).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE native (payload BLOB); INSERT INTO native VALUES (x'0001ff');").unwrap();
        snapshot(&source, &destination).unwrap();
        let copied = rusqlite::Connection::open(&destination).unwrap();
        let bytes: Vec<u8> = copied.query_row("SELECT payload FROM native", [], |row| row.get(0)).unwrap();
        assert_eq!(bytes, [0, 1, 255]);
        assert!(snapshot(&source, &destination).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlink_ancestors() {
        let temporary = tempfile::tempdir().unwrap();
        let actual = temporary.path().join("actual");
        fs::create_dir(&actual).unwrap();
        std::os::unix::fs::symlink(&actual, temporary.path().join("link")).unwrap();
        assert!(check_path(&temporary.path().join("link/new.db")).is_err());
    }
}
