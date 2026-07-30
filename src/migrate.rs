//! Read-only classification of legacy `sessions-convert` Codex and OMP exports.
//!
//! Detectors open candidates read-only without following links or reparse
//! points, require a complete regular JSONL file, and only report whether
//! native re-emission is needed. They never resolve runtime state or write.

use std::fs::{File, OpenOptions};
#[cfg(all(not(unix), not(windows)))]
use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Return whether a Codex export needs native-format re-emission.
pub fn needs_legacy_codex_conversion(path: &Path) -> Result<bool> {
    let Some(records) = read_regular_jsonl(path)? else {
        return Ok(false);
    };
    let Some(meta) = records.iter().find(|record| is_legacy_codex_meta(record)) else {
        return Ok(false);
    };
    let payload = meta
        .get("payload")
        .and_then(Value::as_object)
        .expect("legacy Codex predicate requires an object payload");
    let missing_provider = payload
        .get("model_provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_none_or(str::is_empty);
    let missing_model_context = !records.iter().any(|record| {
        record.get("type").and_then(Value::as_str) == Some("turn_context")
            && record
                .get("payload")
                .and_then(Value::as_object)
                .and_then(|payload| payload.get("model"))
                .and_then(Value::as_str)
                .is_some_and(|model| !model.trim().is_empty())
    });
    Ok(missing_provider || missing_model_context)
}

/// Return whether an OMP export needs native-format re-emission.
pub fn needs_legacy_omp_conversion(path: &Path) -> Result<bool> {
    let Some(records) = read_regular_jsonl(path)? else {
        return Ok(false);
    };
    Ok(records
        .iter()
        .any(|record| is_legacy_omp_model_change(record) || is_legacy_omp_header(record)))
}

fn read_regular_jsonl(path: &Path) -> Result<Option<Vec<Value>>> {
    let mut file = match open_readonly_nofollow(path)? {
        Some(file) => file,
        None => return Ok(None),
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting open session {}", path.display()))?;
    if !metadata.is_file() {
        return Ok(None);
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading session {}", path.display()))?;
    let Ok(contents) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let mut records = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(trimmed) else {
            return Ok(None);
        };
        if !record.is_object() {
            return Ok(None);
        }
        records.push(record);
    }
    Ok(Some(records))
}

#[cfg(unix)]
fn open_readonly_nofollow(path: &Path) -> Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ELOOP) =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(error).with_context(|| format!("opening session {}", path.display()))
        }
    }
}

#[cfg(windows)]
fn open_readonly_nofollow(path: &Path) -> Result<Option<File>> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x20_0000;

    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("opening session {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting open session {}", path.display()))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Ok(None);
    }
    Ok(Some(file))
}

#[cfg(all(not(unix), not(windows)))]
fn open_readonly_nofollow(path: &Path) -> Result<Option<File>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting session {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    match OpenOptions::new().read(true).open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("opening session {}", path.display()))
        }
    }
}

fn is_legacy_codex_meta(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("session_meta")
        && record
            .get("payload")
            .and_then(Value::as_object)
            .and_then(|payload| payload.get("cli_version"))
            .and_then(Value::as_str)
            == Some("sessions-convert")
}

fn is_legacy_omp_model_change(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("model_change")
        && record.get("provider").and_then(Value::as_str) == Some("sessions-convert")
        && record
            .get("modelId")
            .and_then(Value::as_str)
            .is_some_and(|model| model.starts_with("converted-from-"))
        && record.get("model").is_none()
}

fn is_legacy_omp_header(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("session")
        && record.get("titleSource").and_then(Value::as_str) == Some("converted")
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    use serde_json::{Value, json};
    use tempfile::tempdir;

    use super::*;

    fn write_records(path: &Path, records: &[Value]) {
        let contents = records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap() + "\n")
            .collect::<String>();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn codex_detector_classifies_only_incomplete_legacy_exports() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        write_records(
            &path,
            &[json!({
                "timestamp": "2026-01-02T03:04:05Z",
                "type": "session_meta",
                "payload": {
                    "id": "legacy",
                    "cwd": "/tmp/project",
                    "cli_version": "sessions-convert"
                }
            })],
        );
        assert!(needs_legacy_codex_conversion(&path).unwrap());

        write_records(
            &path,
            &[
                json!({
                    "type": "session_meta",
                    "payload": {
                        "id": "repaired",
                        "cli_version": "sessions-convert",
                        "model_provider": "openai"
                    }
                }),
                json!({"type":"turn_context","payload":{"model":"gpt-5"}}),
            ],
        );
        assert!(!needs_legacy_codex_conversion(&path).unwrap());

        write_records(
            &path,
            &[json!({
                "type": "session_meta",
                "payload": {"id":"native","cli_version":"codex-cli"}
            })],
        );
        assert!(!needs_legacy_codex_conversion(&path).unwrap());
    }

    #[test]
    fn omp_detector_classifies_model_and_header_markers() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        write_records(
            &path,
            &[json!({
                "type":"model_change",
                "provider":"sessions-convert",
                "modelId":"converted-from-pi"
            })],
        );
        assert!(needs_legacy_omp_conversion(&path).unwrap());

        write_records(
            &path,
            &[json!({"type":"session","titleSource":"converted"})],
        );
        assert!(needs_legacy_omp_conversion(&path).unwrap());

        write_records(
            &path,
            &[
                json!({"type":"session","title":"native"}),
                json!({"type":"model_change","model":"openai/gpt-5"}),
            ],
        );
        assert!(!needs_legacy_omp_conversion(&path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn detectors_are_read_only_for_legacy_files() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("legacy.jsonl");
        write_records(
            &path,
            &[
                json!({"type":"session","titleSource":"converted"}),
                json!({"type":"future_record","opaque":[1,2,3]}),
            ],
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o440)).unwrap();
        let before = fs::read(&path).unwrap();

        assert!(needs_legacy_omp_conversion(&path).unwrap());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o440);
    }

    #[test]
    fn detectors_reject_malformed_and_non_object_jsonl() {
        let directory = tempdir().unwrap();
        for (name, contents) in [
            ("malformed.jsonl", b"{not-json}\n".as_slice()),
            ("non-object.jsonl", b"[1,2,3]\n".as_slice()),
        ] {
            let path = directory.path().join(name);
            fs::write(&path, contents).unwrap();
            assert!(!needs_legacy_codex_conversion(&path).unwrap());
            assert!(!needs_legacy_omp_conversion(&path).unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn detectors_reject_symlink_and_non_regular_inputs() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("target.jsonl");
        let link = directory.path().join("link.jsonl");
        let subdirectory = directory.path().join("not-a-file");
        write_records(
            &target,
            &[json!({"type":"session","titleSource":"converted"})],
        );
        symlink(&target, &link).unwrap();
        fs::create_dir(&subdirectory).unwrap();
        let target_before = fs::read(&target).unwrap();

        for path in [&link, &subdirectory] {
            assert!(!needs_legacy_codex_conversion(path).unwrap());
            assert!(!needs_legacy_omp_conversion(path).unwrap());
        }
        assert_eq!(fs::read(&target).unwrap(), target_before);
    }
}
