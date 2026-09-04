//! Relocate native session files between directories without converting them.
//!
//! `FROM` can be a catalog folder (match by file path) or a recorded workspace
//! (match by `cwd`). `TO` can be another catalog folder, a dump directory, or
//! the new workspace used to rewrite `cwd` and re-home files.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::domain::{Session, SourceTool};
use crate::formats::pi;
use crate::fs::atomic_write_jsonl;
use crate::sessions::{
    Catalog, destination_overlaps_source, remove_path_nofollow, remove_source_session,
    source_removal_path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOptions {
    pub from: PathBuf,
    pub to: PathBuf,
    pub tools: Vec<SourceTool>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedSession {
    pub source: PathBuf,
    pub destination: PathBuf,
}

struct PlannedMove {
    session: Session,
    destination: PathBuf,
    rewrite_cwd: bool,
}

pub fn move_sessions(catalog: &Catalog, options: &MoveOptions) -> Result<Vec<MovedSession>> {
    let from = catalog.absolute_path(&options.from);
    let to = catalog.absolute_path(&options.to);
    if path_key(&from).as_os_str().is_empty() || path_key(&to).as_os_str().is_empty() {
        bail!("move requires a source directory and a destination directory");
    }
    if from == to || same_resolved(&from, &to) {
        bail!("source and destination directories are the same");
    }
    if from.exists() && !from.is_dir() {
        bail!("source is not a directory: {}", from.display());
    }
    if to.exists() && !to.is_dir() {
        bail!("destination is not a directory: {}", to.display());
    }
    if is_too_broad(&from, catalog) {
        bail!(
            "refusing to move the entire catalog or home directory: {}",
            from.display()
        );
    }

    let tools = movable_tools(&options.tools);
    let mut planned = Vec::new();
    for tool in tools {
        for path in catalog.discover(tool) {
            let session = match catalog.parse(tool, &path) {
                Ok(session) => session,
                Err(_) => continue,
            };
            if !selects_session(&session, &from) {
                continue;
            }
            planned.push(plan_move(catalog, session, &from, &to)?);
        }
    }
    planned.sort_by(|left, right| left.session.path.cmp(&right.session.path));
    planned.retain(|plan| {
        plan.rewrite_cwd || !same_resolved(&plan.session.path, &plan.destination)
    });
    if planned.is_empty() {
        bail!("no sessions found to move from {}", from.display());
    }
    validate_destinations(&planned)?;
    if options.dry_run {
        return Ok(to_moved(&planned));
    }
    for plan in &planned {
        execute_move(plan, &from, &to)?;
    }
    Ok(to_moved(&planned))
}

fn movable_tools(filters: &[SourceTool]) -> Vec<SourceTool> {
    let selected: Vec<SourceTool> = if filters.is_empty() {
        SourceTool::ALL.into_iter().collect()
    } else {
        filters.to_vec()
    };
    selected
        .into_iter()
        .filter(|tool| *tool != SourceTool::Agent)
        .collect()
}

fn is_too_broad(from: &Path, catalog: &Catalog) -> bool {
    if from == Path::new("/") {
        return true;
    }
    if same_resolved(from, catalog.user_home()) || same_resolved(from, catalog.sessions_home()) {
        return true;
    }
    catalog
        .roots()
        .iter()
        .any(|root| same_resolved(from, &root.path))
}

fn selects_session(session: &Session, from: &Path) -> bool {
    path_is_under(&session.path, from) || cwd_is_under(&session.cwd, from)
}

fn plan_move(
    catalog: &Catalog,
    session: Session,
    from: &Path,
    to: &Path,
) -> Result<PlannedMove> {
    let cwd_match = cwd_is_under(&session.cwd, from);
    if catalog_dir_for_tool(to, catalog, session.tool) {
        let destination = dump_destination(&session, to)?;
        return Ok(PlannedMove {
            session,
            destination,
            rewrite_cwd: cwd_match,
        });
    }
    let new_cwd = if cwd_match {
        rewrite_cwd_path(&session.cwd, from, to)?
    } else {
        to.to_path_buf()
    };
    let rewrite_cwd = session.cwd != new_cwd;
    let destination = native_session_path(catalog, &session, &new_cwd)?;
    Ok(PlannedMove {
        session,
        destination,
        rewrite_cwd,
    })
}

fn catalog_dir_for_tool(to: &Path, catalog: &Catalog, tool: SourceTool) -> bool {
    let root = catalog.root_for_tool(tool).path;
    same_resolved(to, &root) || path_is_under(to, &root)
}

fn dump_destination(session: &Session, to: &Path) -> Result<PathBuf> {
    if session.tool == SourceTool::Grok {
        let session_id = grok_session_dir_name(session)?;
        return Ok(to.join(session_id).join("summary.json"));
    }
    let name = session
        .path
        .file_name()
        .with_context(|| format!("session path has no file name: {}", session.path.display()))?;
    Ok(to.join(name))
}

fn grok_session_dir_name(session: &Session) -> Result<String> {
    if !session.session_id.is_empty()
        && !session.session_id.contains(['/', '\\', '\0'])
        && session.session_id != "."
        && session.session_id != ".."
    {
        return Ok(session.session_id.clone());
    }
    session
        .path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .context("grok session id is not a safe directory name")
}

fn native_session_path(
    catalog: &Catalog,
    session: &Session,
    cwd: &Path,
) -> Result<PathBuf> {
    let cwd_text = cwd
        .to_str()
        .with_context(|| format!("session cwd is not valid UTF-8: {}", cwd.display()))?;
    let root = catalog.root_for_tool(session.tool).path;
    let name = session
        .path
        .file_name()
        .with_context(|| format!("session path has no file name: {}", session.path.display()))?;
    let path = match session.tool {
        SourceTool::Pi | SourceTool::Rpi => root.join(pi::encode_cwd(cwd)?).join(name),
        SourceTool::Omp => root
            .join(crate::formats::omp::encode_omp_cwd_with(
                cwd,
                catalog.user_home(),
                &env::temp_dir(),
            ))
            .join(name),
        SourceTool::Droid | SourceTool::Claude => {
            root.join(crate::emit::encode_single_dash_cwd(cwd_text)).join(name)
        }
        SourceTool::Codex => session.path.clone(),
        SourceTool::Grok => root
            .join(crate::emit::encode_grok_cwd(cwd_text))
            .join(grok_session_dir_name(session)?)
            .join("summary.json"),
        SourceTool::Agent => bail!("Agent sessions cannot be moved"),
    };
    Ok(path)
}

fn validate_destinations(planned: &[PlannedMove]) -> Result<()> {
    let mut seen = HashSet::new();
    for plan in planned {
        let removal = source_removal_path(&plan.session);
        if destination_overlaps_source(&removal, &plan.destination)
            && !same_resolved(&plan.session.path, &plan.destination)
        {
            bail!(
                "move would overwrite or delete the destination: {}",
                plan.destination.display()
            );
        }
        if !same_resolved(&plan.session.path, &plan.destination) && plan.destination.exists() {
            bail!(
                "destination already exists: {}",
                plan.destination.display()
            );
        }
        if plan.session.tool == SourceTool::Grok {
            if let Some(directory) = plan.destination.parent() {
                if !same_resolved(&removal, directory)
                    && directory.exists()
                    && !same_resolved(&plan.session.path, &plan.destination)
                {
                    bail!(
                        "destination grok session directory already exists: {}",
                        directory.display()
                    );
                }
            }
        }
        for (_, dest) in companion_pairs(&plan.session, &plan.destination) {
            if dest.exists() && !same_resolved(&plan.session.path, &dest) {
                bail!("destination already exists: {}", dest.display());
            }
        }
        let key = path_key(&plan.destination);
        if !seen.insert(key) {
            bail!(
                "multiple sessions would write {}",
                plan.destination.display()
            );
        }
    }
    Ok(())
}

fn execute_move(plan: &PlannedMove, from: &Path, to: &Path) -> Result<()> {
    let source = &plan.session.path;
    if plan.rewrite_cwd {
        write_rewritten(&plan.session, &plan.destination, from, to)?;
    } else if !same_resolved(source, &plan.destination) {
        copy_session(&plan.session, &plan.destination)?;
    }
    if !same_resolved(source, &plan.destination) {
        relocate_companions(&plan.session, &plan.destination)?;
        remove_source_session(&plan.session)?;
    }
    Ok(())
}

fn companion_pairs(session: &Session, destination: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Some(src_parent) = session.path.parent() else {
        return Vec::new();
    };
    let Some(dst_parent) = destination.parent() else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    match session.tool {
        SourceTool::Pi | SourceTool::Rpi | SourceTool::Omp => {
            if let Some(name) = session.path.file_name() {
                let loops = format!("{}.loops.json", name.to_string_lossy());
                pairs.push((src_parent.join(&loops), dst_parent.join(&loops)));
            }
            if let Some(stem) = session.path.file_stem() {
                pairs.push((src_parent.join(stem), dst_parent.join(stem)));
            }
            if session.tool.uses_pi_jsonl()
                && !session.session_id.is_empty()
                && !session.session_id.contains(['/', '\\', '\0'])
            {
                pairs.push((
                    src_parent.join("children").join(&session.session_id),
                    dst_parent.join("children").join(&session.session_id),
                ));
            }
        }
        SourceTool::Droid => {
            if let Some(stem) = session.path.file_stem() {
                let stem = stem.to_string_lossy();
                for suffix in [".settings.json", ".settings.json.bak"] {
                    let name = format!("{stem}{suffix}");
                    pairs.push((src_parent.join(&name), dst_parent.join(&name)));
                }
            }
        }
        SourceTool::Grok | SourceTool::Codex | SourceTool::Claude | SourceTool::Agent => {}
    }
    pairs
        .into_iter()
        .filter(|(source, dest)| source.exists() && !same_resolved(source, dest))
        .collect()
}

fn relocate_companions(session: &Session, destination: &Path) -> Result<()> {
    for (source, dest) in companion_pairs(session, destination) {
        let metadata = fs::symlink_metadata(&source)
            .with_context(|| format!("inspecting {}", source.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("refusing to copy symlink {}", source.display());
        }
        if metadata.is_file() {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            copy_regular_file(&source, &dest)?;
            remove_path_nofollow(&source)?;
        } else if metadata.is_dir() {
            copy_tree(&source, &dest)?;
            remove_path_nofollow(&source)?;
        } else {
            bail!("refusing to copy special file {}", source.display());
        }
    }
    Ok(())
}

fn write_rewritten(session: &Session, destination: &Path, from: &Path, to: &Path) -> Result<()> {
    if session.tool == SourceTool::Grok {
        write_rewritten_grok(session, destination, from, to)
    } else {
        write_rewritten_jsonl(session, destination, from, to)
    }
}

fn write_rewritten_jsonl(
    session: &Session,
    destination: &Path,
    from: &Path,
    to: &Path,
) -> Result<()> {
    let file = fs::File::open(&session.path)
        .with_context(|| format!("reading session {}", session.path.display()))?;
    let mut records = Vec::new();
    for line in BufReader::new(file).lines() {
        let line =
            line.with_context(|| format!("reading session {}", session.path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let mut record: Value = serde_json::from_str(&line).with_context(|| {
            format!(
                "parsing session record in {}",
                session.path.display()
            )
        })?;
        rewrite_record_cwd(&mut record, from, to);
        records.push(record);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    atomic_write_jsonl(destination, &records)
}

fn write_rewritten_grok(
    session: &Session,
    destination: &Path,
    from: &Path,
    to: &Path,
) -> Result<()> {
    copy_grok_session(session, destination)?;
    rewrite_json_file_cwd(destination, from, to)?;
    if let Some(parent) = destination.parent() {
        rewrite_json_file_cwd(&parent.join("prompt_context.json"), from, to)?;
    }
    Ok(())
}

fn copy_session(session: &Session, destination: &Path) -> Result<()> {
    if session.tool == SourceTool::Grok {
        return copy_grok_session(session, destination);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    copy_regular_file(&session.path, destination)
}

fn copy_grok_session(session: &Session, destination: &Path) -> Result<()> {
    let directory = session.path.parent().with_context(|| {
        format!("grok session has no directory: {}", session.path.display())
    })?;
    let dest_dir = destination.parent().with_context(|| {
        format!(
            "grok destination has no directory: {}",
            destination.display()
        )
    })?;
    copy_tree(directory, dest_dir)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspecting {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("refusing to copy symlink {}", source.display());
    }
    if !metadata.is_dir() {
        bail!("refusing to copy non-directory {}", source.display());
    }
    fs::create_dir_all(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    for entry in fs::read_dir(source)
        .with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading entry under {}", source.display()))?;
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(".lock") {
            continue;
        }
        let next_source = entry.path();
        let next_dest = destination.join(&name);
        let file_type = entry
            .file_type()
            .with_context(|| format!("inspecting {}", next_source.display()))?;
        if file_type.is_symlink() {
            bail!("refusing to copy symlink {}", next_source.display());
        }
        if file_type.is_dir() {
            copy_tree(&next_source, &next_dest)?;
        } else if file_type.is_file() {
            copy_regular_file(&next_source, &next_dest)?;
        } else {
            bail!("refusing to copy special file {}", next_source.display());
        }
    }
    Ok(())
}

fn rewrite_json_file_cwd(path: &Path, from: &Path, to: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut record: Value = serde_json::from_str(text.trim())
        .with_context(|| format!("parsing {}", path.display()))?;
    rewrite_record_cwd(&mut record, from, to);
    atomic_write_jsonl(path, std::slice::from_ref(&record))
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspecting {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("refusing to copy symlink {}", source.display());
    }
    if !metadata.is_file() {
        bail!("refusing to copy non-file {}", source.display());
    }
    if destination.exists() {
        bail!("destination already exists: {}", destination.display());
    }
    fs::copy(source, destination).with_context(|| {
        format!(
            "copying {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn rewrite_record_cwd(record: &mut Value, from: &Path, to: &Path) {
    let Some(object) = record.as_object_mut() else {
        return;
    };
    rewrite_cwd_value(object.get_mut("cwd"), from, to);
    rewrite_cwd_value(object.get_mut("working_directory"), from, to);
    if let Some(payload) = object.get_mut("payload").and_then(Value::as_object_mut) {
        rewrite_cwd_value(payload.get_mut("cwd"), from, to);
    }
    if let Some(info) = object.get_mut("info").and_then(Value::as_object_mut) {
        rewrite_cwd_value(info.get_mut("cwd"), from, to);
    }
}

fn rewrite_cwd_value(value: Option<&mut Value>, from: &Path, to: &Path) {
    let Some(Value::String(text)) = value else {
        return;
    };
    let current = Path::new(text.as_str());
    if !cwd_is_under(current, from) {
        return;
    }
    if let Ok(rewritten) = rewrite_cwd_path(current, from, to) {
        if let Some(rewritten) = rewritten.to_str() {
            *text = rewritten.to_owned();
        }
    }
}

fn rewrite_cwd_path(cwd: &Path, from: &Path, to: &Path) -> Result<PathBuf> {
    if cwd == from {
        return Ok(to.to_path_buf());
    }
    match cwd.strip_prefix(from) {
        Ok(rest) => Ok(to.join(rest)),
        Err(_) => Ok(cwd.to_path_buf()),
    }
}

fn cwd_is_under(cwd: &Path, from: &Path) -> bool {
    if cwd.as_os_str().is_empty() {
        return false;
    }
    cwd == from || cwd.starts_with(from)
}

fn path_is_under(path: &Path, from: &Path) -> bool {
    if !from.exists() {
        return false;
    }
    let path = resolved_or_abs(path);
    let from = resolved_or_abs(from);
    path == from || path.starts_with(&from)
}

fn same_resolved(left: &Path, right: &Path) -> bool {
    resolved_or_abs(left) == resolved_or_abs(right)
}

fn resolved_or_abs(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    if let Some(parent) = path.parent() {
        if let (Ok(parent), Some(name)) = (parent.canonicalize(), path.file_name()) {
            return parent.join(name);
        }
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn path_key(path: &Path) -> PathBuf {
    resolved_or_abs(path)
}

fn to_moved(planned: &[PlannedMove]) -> Vec<MovedSession> {
    planned
        .iter()
        .map(|plan| MovedSession {
            source: plan.session.path.clone(),
            destination: plan.destination.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    use tempfile::TempDir;

    use crate::sessions::Catalog;

    fn write_pi(home: &Path, project: &str, file: &str, cwd: &str, id: &str, text: &str) -> PathBuf {
        write_pi_jsonl(home, ".pi/agent/sessions", project, file, cwd, id, text)
    }

    fn write_rpi(home: &Path, project: &str, file: &str, cwd: &str, id: &str, text: &str) -> PathBuf {
        write_pi_jsonl(home, ".rpi/sessions", project, file, cwd, id, text)
    }

    fn write_pi_jsonl(
        home: &Path,
        root: &str,
        project: &str,
        file: &str,
        cwd: &str,
        id: &str,
        text: &str,
    ) -> PathBuf {
        let path = home
            .join(root)
            .join(project)
            .join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut handle = fs::File::create(&path).unwrap();
        writeln!(
            handle,
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}"}}"#
        )
        .unwrap();
        writeln!(
            handle,
            r#"{{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
        .unwrap();
        path
    }

    fn write_grok(home: &Path, encoded: &str, id: &str, cwd: &str) -> PathBuf {
        let directory = home.join(".grok/sessions").join(encoded).join(id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("summary.json"),
            format!(
                r#"{{"info":{{"id":"{id}","cwd":"{cwd}"}},"generated_title":"t","created_at":"2026-01-01T00:00:00Z"}}"#
            ),
        )
        .unwrap();
        fs::write(
            directory.join("chat_history.jsonl"),
            r#"{"type":"user","content":[{"type":"text","text":"hello"}]}"#,
        )
        .unwrap();
        fs::write(
            directory.join("prompt_context.json"),
            format!(r#"{{"working_directory":"{cwd}","prompt_mode":"default"}}"#),
        )
        .unwrap();
        fs::write(directory.join("system_prompt.txt"), "prompt").unwrap();
        fs::write(directory.join("events.jsonl"), "{}\n").unwrap();
        directory.join("summary.json")
    }

    #[test]
    fn catalog_directory_move_keeps_bytes_and_cwd() {
        let home = TempDir::new().unwrap();
        let source = write_pi(
            home.path(),
            "--old--",
            "2026-01-01T00-00-00_sid.jsonl",
            "/workspace/old",
            "sid",
            "keep me",
        );
        let from = source.parent().unwrap();
        let to = home.path().join(".pi/agent/sessions/--new--");
        let catalog = Catalog::new(home.path());
        let moved = move_sessions(
            &catalog,
            &MoveOptions {
                from: from.to_path_buf(),
                to: to.clone(),
                tools: vec![SourceTool::Pi],
                dry_run: false,
            },
        )
        .unwrap();
        assert_eq!(moved.len(), 1);
        let dest = to.join("2026-01-01T00-00-00_sid.jsonl");
        assert_eq!(moved[0].destination, dest);
        assert!(!source.exists());
        let text = fs::read_to_string(&dest).unwrap();
        assert!(text.contains("/workspace/old"));
        assert!(text.contains("keep me"));
    }

    #[test]
    fn workspace_rehome_rewrites_cwd_and_native_path() {
        let home = TempDir::new().unwrap();
        let source = write_pi(
            home.path(),
            "--workspace-old--",
            "2026-01-01T00-00-00_sid.jsonl",
            "/workspace/old",
            "sid",
            "rehome",
        );
        let catalog = Catalog::new(home.path());
        let moved = move_sessions(
            &catalog,
            &MoveOptions {
                from: PathBuf::from("/workspace/old"),
                to: PathBuf::from("/workspace/new"),
                tools: vec![SourceTool::Pi],
                dry_run: false,
            },
        )
        .unwrap();
        let dest = home.path().join(
            ".pi/agent/sessions/--workspace-new--/2026-01-01T00-00-00_sid.jsonl",
        );
        assert_eq!(moved[0].destination, dest);
        assert!(!source.exists());
        let text = fs::read_to_string(&dest).unwrap();
        assert!(text.contains("/workspace/new"));
        assert!(!text.contains("/workspace/old"));
    }

    #[test]
    fn dry_run_does_not_touch_files() {
        let home = TempDir::new().unwrap();
        let source = write_pi(
            home.path(),
            "--old--",
            "keep.jsonl",
            "/workspace/old",
            "sid",
            "dry",
        );
        let catalog = Catalog::new(home.path());
        let moved = move_sessions(
            &catalog,
            &MoveOptions {
                from: PathBuf::from("/workspace/old"),
                to: PathBuf::from("/workspace/new"),
                tools: Vec::new(),
                dry_run: true,
            },
        )
        .unwrap();
        assert_eq!(moved.len(), 1);
        assert!(source.exists());
        assert!(!moved[0].destination.exists());
    }

    #[test]
    fn grok_workspace_rehome_moves_session_directory() {
        let home = TempDir::new().unwrap();
        let source = write_grok(home.path(), "%2Fworkspace%2Fold", "gid", "/workspace/old");
        let source_dir = source.parent().unwrap().to_path_buf();
        let catalog = Catalog::new(home.path());
        let moved = move_sessions(
            &catalog,
            &MoveOptions {
                from: PathBuf::from("/workspace/old"),
                to: PathBuf::from("/workspace/new"),
                tools: vec![SourceTool::Grok],
                dry_run: false,
            },
        )
        .unwrap();
        let dest = home
            .path()
            .join(".grok/sessions/%2Fworkspace%2Fnew/gid/summary.json");
        assert_eq!(moved[0].destination, dest);
        assert!(!source_dir.exists());
        assert!(dest.is_file());
        assert!(dest.with_file_name("chat_history.jsonl").is_file());
        assert!(dest.with_file_name("system_prompt.txt").is_file());
        assert!(dest.with_file_name("events.jsonl").is_file());
        let summary = fs::read_to_string(&dest).unwrap();
        assert!(summary.contains("/workspace/new"));
        let context = fs::read_to_string(dest.with_file_name("prompt_context.json")).unwrap();
        assert!(context.contains("/workspace/new"), "{context}");
        assert!(!context.contains("/workspace/old"), "{context}");
    }

    #[test]
    fn refuses_existing_destination() {
        let home = TempDir::new().unwrap();
        write_pi(
            home.path(),
            "--old--",
            "session.jsonl",
            "/workspace/old",
            "sid",
            "one",
        );
        let dest_dir = home.path().join(".pi/agent/sessions/--new--");
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(dest_dir.join("session.jsonl"), "taken\n").unwrap();
        let catalog = Catalog::new(home.path());
        let error = move_sessions(
            &catalog,
            &MoveOptions {
                from: home.path().join(".pi/agent/sessions/--old--"),
                to: dest_dir,
                tools: vec![SourceTool::Pi],
                dry_run: false,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn rewrite_cwd_path_replaces_prefix() {
        assert_eq!(
            rewrite_cwd_path(
                Path::new("/old/proj/src"),
                Path::new("/old/proj"),
                Path::new("/new/proj")
            )
            .unwrap(),
            PathBuf::from("/new/proj/src")
        );
    }

    const FEAT_A: &str = "/workspace/feat-a/projectX";
    const FEAT_B: &str = "/workspace/feat-b/projectX";

    fn move_feat(catalog: &Catalog, tools: Vec<SourceTool>) -> Vec<MovedSession> {
        move_sessions(
            catalog,
            &MoveOptions {
                from: PathBuf::from(FEAT_A),
                to: PathBuf::from(FEAT_B),
                tools,
                dry_run: false,
            },
        )
        .unwrap()
    }

    fn write_omp(home: &Path, project: &str, file: &str, cwd: &str, id: &str) -> PathBuf {
        let path = home.join(".omp/agent/sessions").join(project).join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                format!(
                    r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}"}}"#
                ),
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"omp"}}"#
            ),
        )
        .unwrap();
        path
    }

    fn write_claude(home: &Path, project: &str, file: &str, cwd: &str, id: &str) -> PathBuf {
        let path = home.join(".claude/projects").join(project).join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                format!(
                    r#"{{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-07-21T06:13:11.040Z","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"user","content":"hi"}}}}"#
                ),
                format!(
                    r#"{{"type":"assistant","uuid":"a1","parentUuid":"u1","timestamp":"2026-07-21T06:13:12.040Z","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"assistant","content":[{{"type":"text","text":"ok"}}]}}}}"#
                ),
            ),
        )
        .unwrap();
        path
    }

    fn write_droid(home: &Path, project: &str, id: &str, cwd: &str) -> PathBuf {
        let path = home
            .join(".factory/sessions")
            .join(project)
            .join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                format!(
                    r#"{{"type":"session_start","id":"{id}","title":"droid","cwd":"{cwd}","version":2}}"#
                ),
                r#"{"type":"message","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi"}}"#
            ),
        )
        .unwrap();
        path
    }

    fn write_codex(home: &Path, cwd: &str, id: &str) -> PathBuf {
        let path = home.join(".codex/sessions/2026/01/01").join(format!(
            "rollout-2026-01-01T00-00-00-{id}.jsonl"
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                format!(
                    r#"{{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{{"id":"{id}","session_id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}","originator":"codex-tui","source":"cli","cli_version":"test"}}}}"#
                ),
                r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn feat_a_project_x_rehomes_pi_rpi_omp_claude_droid_grok_and_codex() {
        let home = TempDir::new().unwrap();
        let catalog = Catalog::new(home.path());
        let pi = write_pi(
            home.path(),
            "--workspace-feat-a-projectX--",
            "2026-01-01T00-00-00_pi.jsonl",
            FEAT_A,
            "pi-id",
            "pi body",
        );
        let rpi = write_rpi(
            home.path(),
            "--workspace-feat-a-projectX--",
            "2026-01-01T00-00-00_rpi.jsonl",
            FEAT_A,
            "rpi-id",
            "rpi body",
        );
        let omp = write_omp(
            home.path(),
            "--workspace-feat-a-projectX--",
            "2026-01-01T00-00-00_omp.jsonl",
            FEAT_A,
            "omp-id",
        );
        let claude = write_claude(
            home.path(),
            "-workspace-feat-a-projectX",
            "claude-id.jsonl",
            FEAT_A,
            "claude-id",
        );
        let droid = write_droid(
            home.path(),
            "-workspace-feat-a-projectX",
            "droid-id",
            FEAT_A,
        );
        let grok = write_grok(
            home.path(),
            "%2Fworkspace%2Ffeat-a%2FprojectX",
            "grok-id",
            FEAT_A,
        );
        let codex = write_codex(
            home.path(),
            FEAT_A,
            "11111111-1111-4111-8111-111111111111",
        );

        let moved = move_feat(&catalog, Vec::new());
        assert_eq!(moved.len(), 7, "{moved:?}");

        let pi_dest = home.path().join(
            ".pi/agent/sessions/--workspace-feat-b-projectX--/2026-01-01T00-00-00_pi.jsonl",
        );
        let rpi_dest = home.path().join(
            ".rpi/sessions/--workspace-feat-b-projectX--/2026-01-01T00-00-00_rpi.jsonl",
        );
        let omp_home = crate::formats::omp::encode_omp_cwd_with(
            Path::new(FEAT_B),
            catalog.user_home(),
            &env::temp_dir(),
        );
        let omp_dest = home
            .path()
            .join(".omp/agent/sessions")
            .join(omp_home)
            .join("2026-01-01T00-00-00_omp.jsonl");
        let claude_dest = home.path().join(
            ".claude/projects/-workspace-feat-b-projectX/claude-id.jsonl",
        );
        let droid_dest = home
            .path()
            .join(".factory/sessions/-workspace-feat-b-projectX/droid-id.jsonl");
        let grok_dest = home.path().join(
            ".grok/sessions/%2Fworkspace%2Ffeat-b%2FprojectX/grok-id/summary.json",
        );

        assert!(!pi.exists());
        assert!(pi_dest.is_file());
        assert!(fs::read_to_string(&pi_dest).unwrap().contains(FEAT_B));
        assert!(!fs::read_to_string(&pi_dest).unwrap().contains(FEAT_A));

        assert!(!rpi.exists());
        assert!(rpi_dest.is_file());
        assert!(fs::read_to_string(&rpi_dest).unwrap().contains(FEAT_B));
        assert!(!fs::read_to_string(&rpi_dest).unwrap().contains(FEAT_A));

        assert!(!omp.exists());
        assert!(omp_dest.is_file(), "omp dest {}", omp_dest.display());
        assert!(fs::read_to_string(&omp_dest).unwrap().contains(FEAT_B));

        let claude_text = fs::read_to_string(&claude_dest).unwrap();
        assert!(!claude.exists());
        assert_eq!(claude_text.matches(FEAT_B).count(), 2, "{claude_text}");
        assert!(!claude_text.contains(FEAT_A));

        assert!(!droid.exists());
        assert!(fs::read_to_string(&droid_dest).unwrap().contains(FEAT_B));

        assert!(!grok.parent().unwrap().exists());
        assert!(grok_dest.is_file());
        assert!(fs::read_to_string(&grok_dest).unwrap().contains(FEAT_B));

        assert!(codex.is_file(), "codex stays on the date path");
        let codex_text = fs::read_to_string(&codex).unwrap();
        assert!(codex_text.contains(FEAT_B));
        assert!(!codex_text.contains(FEAT_A));
    }

    #[test]
    fn feat_a_nested_cwd_is_rewritten_and_sibling_name_is_ignored() {
        let home = TempDir::new().unwrap();
        let nested = format!("{FEAT_A}/crates/foo");
        let sibling = "/workspace/feat-a/projectX-extra";
        let parent = "/workspace/feat-a";
        let keep_nested = write_pi(
            home.path(),
            "--workspace-feat-a-projectX-crates-foo--",
            "nested.jsonl",
            &nested,
            "nested",
            "nested",
        );
        let keep_sibling = write_pi(
            home.path(),
            "--workspace-feat-a-projectX-extra--",
            "sibling.jsonl",
            sibling,
            "sibling",
            "sibling",
        );
        let keep_parent = write_pi(
            home.path(),
            "--workspace-feat-a--",
            "parent.jsonl",
            parent,
            "parent",
            "parent",
        );
        let catalog = Catalog::new(home.path());
        let moved = move_feat(&catalog, vec![SourceTool::Pi]);
        assert_eq!(moved.len(), 1);
        let dest = home.path().join(
            ".pi/agent/sessions/--workspace-feat-b-projectX-crates-foo--/nested.jsonl",
        );
        assert_eq!(moved[0].destination, dest);
        assert!(!keep_nested.exists());
        assert!(fs::read_to_string(&dest).unwrap().contains(&format!(
            "{FEAT_B}/crates/foo"
        )));
        assert!(keep_sibling.exists());
        assert!(keep_parent.exists());
        assert!(fs::read_to_string(&keep_sibling).unwrap().contains(sibling));
        assert!(fs::read_to_string(&keep_parent).unwrap().contains(parent));
    }

    #[test]
    fn feat_a_matches_after_workspace_directory_is_gone() {
        let home = TempDir::new().unwrap();
        let source = write_pi(
            home.path(),
            "--workspace-feat-a-projectX--",
            "gone.jsonl",
            FEAT_A,
            "gone",
            "gone",
        );
        assert!(!Path::new(FEAT_A).exists());
        let catalog = Catalog::new(home.path());
        let moved = move_feat(&catalog, vec![SourceTool::Pi]);
        assert_eq!(moved.len(), 1);
        assert!(!source.exists());
        assert!(moved[0].destination.is_file());
    }

    #[test]
    fn omp_home_relative_feat_dirs_use_short_names() {
        let home = TempDir::new().unwrap();
        let from = home.path().join("Projects/feat-a/projectX");
        let to = home.path().join("Projects/feat-b/projectX");
        fs::create_dir_all(&from).unwrap();
        fs::create_dir_all(&to).unwrap();
        let encoded = crate::formats::omp::encode_omp_cwd_with(
            &from,
            home.path(),
            &env::temp_dir(),
        );
        assert_eq!(encoded, "-Projects-feat-a-projectX");
        let source = write_omp(
            home.path(),
            &encoded,
            "omp.jsonl",
            from.to_str().unwrap(),
            "omp-home",
        );
        let catalog = Catalog::new(home.path());
        let moved = move_sessions(
            &catalog,
            &MoveOptions {
                from: from.clone(),
                to: to.clone(),
                tools: vec![SourceTool::Omp],
                dry_run: false,
            },
        )
        .unwrap();
        let dest = home
            .path()
            .join(".omp/agent/sessions/-Projects-feat-b-projectX/omp.jsonl");
        assert_eq!(moved[0].destination, dest);
        assert!(!source.exists());
        assert!(fs::read_to_string(&dest).unwrap().contains(to.to_str().unwrap()));
    }

    #[test]
    fn tool_filter_leaves_other_tools_in_place() {
        let home = TempDir::new().unwrap();
        let pi = write_pi(
            home.path(),
            "--workspace-feat-a-projectX--",
            "pi.jsonl",
            FEAT_A,
            "pi",
            "pi",
        );
        let grok = write_grok(
            home.path(),
            "%2Fworkspace%2Ffeat-a%2FprojectX",
            "gid",
            FEAT_A,
        );
        let catalog = Catalog::new(home.path());
        let moved = move_feat(&catalog, vec![SourceTool::Pi]);
        assert_eq!(moved.len(), 1);
        assert!(!pi.exists());
        assert!(grok.is_file());
    }

    #[test]
    fn pi_omp_and_droid_sidecars_follow_the_session() {
        let home = TempDir::new().unwrap();
        let source = write_pi(
            home.path(),
            "--workspace-feat-a-projectX--",
            "2026-01-01T00-00-00_sid.jsonl",
            FEAT_A,
            "sid",
            "body",
        );
        let loops = source.with_file_name("2026-01-01T00-00-00_sid.jsonl.loops.json");
        fs::write(&loops, "{}\n").unwrap();
        let child = source
            .parent()
            .unwrap()
            .join("children/sid/orchestration-state.json");
        fs::create_dir_all(child.parent().unwrap()).unwrap();
        fs::write(&child, "{}\n").unwrap();
        let droid = write_droid(
            home.path(),
            "-workspace-feat-a-projectX",
            "droid-id",
            FEAT_A,
        );
        let settings = droid.with_file_name("droid-id.settings.json");
        fs::write(&settings, "{}\n").unwrap();
        let omp = write_omp(
            home.path(),
            "--workspace-feat-a-projectX--",
            "2026-01-01T00-00-00_omp.jsonl",
            FEAT_A,
            "omp-id",
        );
        let omp_dir = omp.with_file_name("2026-01-01T00-00-00_omp");
        fs::create_dir_all(&omp_dir).unwrap();
        fs::write(omp_dir.join("log.txt"), "omp log").unwrap();

        let catalog = Catalog::new(home.path());
        move_feat(
            &catalog,
            vec![SourceTool::Pi, SourceTool::Droid, SourceTool::Omp],
        );

        let pi_dest = home.path().join(
            ".pi/agent/sessions/--workspace-feat-b-projectX--/2026-01-01T00-00-00_sid.jsonl",
        );
        assert!(pi_dest.is_file());
        assert!(pi_dest
            .with_file_name("2026-01-01T00-00-00_sid.jsonl.loops.json")
            .is_file());
        assert!(pi_dest
            .parent()
            .unwrap()
            .join("children/sid/orchestration-state.json")
            .is_file());
        assert!(!loops.exists());
        assert!(!child.exists());

        let droid_dest = home
            .path()
            .join(".factory/sessions/-workspace-feat-b-projectX/droid-id.jsonl");
        assert!(droid_dest.is_file());
        assert!(droid_dest
            .with_file_name("droid-id.settings.json")
            .is_file());
        assert!(!settings.exists());

        let omp_home = crate::formats::omp::encode_omp_cwd_with(
            Path::new(FEAT_B),
            catalog.user_home(),
            &env::temp_dir(),
        );
        let omp_dest = home
            .path()
            .join(".omp/agent/sessions")
            .join(omp_home)
            .join("2026-01-01T00-00-00_omp.jsonl");
        assert!(omp_dest.is_file());
        assert!(omp_dest
            .with_file_name("2026-01-01T00-00-00_omp")
            .join("log.txt")
            .is_file());
        assert!(!omp_dir.exists());
    }

    #[test]
    fn agent_sessions_with_matching_cwd_are_not_moved() {
        let home = TempDir::new().unwrap();
        write_pi(
            home.path(),
            "--workspace-feat-a-projectX--",
            "pi.jsonl",
            FEAT_A,
            "pi",
            "pi",
        );
        let agent_dir = home
            .path()
            .join(".cursor/chats/0123456789abcdef0123456789abcdef/agent-id");
        fs::create_dir_all(&agent_dir).unwrap();
        let store = agent_dir.join("store.db");
        let connection = rusqlite::Connection::open(&store).unwrap();
        connection
            .execute("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT)", [])
            .unwrap();
        connection
            .execute("CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB)", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO meta VALUES('0', ?1)",
                [r#"{"agentId":"agent-id","name":"New Agent"}"#],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO blobs VALUES('message', ?1)",
                [br#"{"role":"user","content":"hello"}"#.as_slice()],
            )
            .unwrap();
        drop(connection);
        fs::write(
            agent_dir.join("meta.json"),
            format!(r#"{{"cwd":"{FEAT_A}","title":"Agent","updatedAtMs":1}}"#),
        )
        .unwrap();

        let catalog = Catalog::new(home.path());
        let moved = move_feat(&catalog, Vec::new());
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].source.file_name().unwrap(), "pi.jsonl");
        assert!(store.is_file());
        assert!(agent_dir.join("meta.json").is_file());
    }

    #[test]
    fn wt_style_projectx_feat_a_directory_is_a_different_cwd() {
        let home = TempDir::new().unwrap();
        let wt = "/workspace/projectX-feat-a";
        write_pi(
            home.path(),
            "--workspace-projectX-feat-a--",
            "wt.jsonl",
            wt,
            "wt",
            "wt",
        );
        let catalog = Catalog::new(home.path());
        let error = move_sessions(
            &catalog,
            &MoveOptions {
                from: PathBuf::from(FEAT_A),
                to: PathBuf::from(FEAT_B),
                tools: vec![SourceTool::Pi],
                dry_run: false,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("no sessions found"));
    }
}
