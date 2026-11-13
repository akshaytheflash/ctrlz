use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::manifest::Manifest;
use crate::store::{read_blob, BlobError};
use crate::util::{
    is_reparse, join_rel, long_path, make_writable, set_mode, set_mtime, validate_rel_path,
    PathError,
};

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("restore aborted: 2 paths collide on case ({0}, {1}). rename one, then rerun.")]
    CaseCollision(String, String),
    #[error(transparent)]
    Path(#[from] PathError),
}

#[derive(Debug, Clone)]
pub struct Write {
    pub path: String,
    pub hash: String,
    pub mode: u32,
    pub mtime_ns: i64,
    pub content: bool,
}

#[derive(Debug)]
pub struct Plan {
    pub deletes: Vec<String>,
    pub writes: Vec<Write>,
    pub unchanged: usize,
}

pub fn plan_restore(
    current: &Manifest,
    target: &Manifest,
    case_insensitive: bool,
) -> Result<Plan, PlanError> {
    for path in target.entries.keys() {
        validate_rel_path(path)?;
    }
    for path in current.entries.keys() {
        validate_rel_path(path)?;
    }

    if case_insensitive {
        let mut seen: HashMap<String, &str> = HashMap::new();
        for path in target.entries.keys() {
            let key = path.to_lowercase();
            if let Some(first) = seen.get(&key) {
                if *first != path {
                    return Err(PlanError::CaseCollision((*first).to_string(), path.clone()));
                }
            } else {
                seen.insert(key, path);
            }
        }
    }

    let mut deletes: Vec<String> = current
        .entries
        .keys()
        .filter(|path| !target.entries.contains_key(*path))
        .filter(|path| !was_skipped(target, path))
        .cloned()
        .collect();
    deletes.sort();

    let mut writes = Vec::new();
    let mut unchanged = 0;
    for (path, entry) in &target.entries {
        match current.entries.get(path) {
            None => writes.push(Write {
                path: path.clone(),
                hash: entry.hash.clone(),
                mode: entry.mode,
                mtime_ns: entry.mtime_ns,
                content: true,
            }),
            Some(old) if old.hash != entry.hash => writes.push(Write {
                path: path.clone(),
                hash: entry.hash.clone(),
                mode: entry.mode,
                mtime_ns: entry.mtime_ns,
                content: true,
            }),
            Some(old) if old.mode != entry.mode || old.mtime_ns != entry.mtime_ns => {
                writes.push(Write {
                    path: path.clone(),
                    hash: entry.hash.clone(),
                    mode: entry.mode,
                    mtime_ns: entry.mtime_ns,
                    content: false,
                });
            }
            Some(_) => unchanged += 1,
        }
    }
    writes.sort_by(|left, right| left.path.cmp(&right.path));

    Ok(Plan {
        deletes,
        writes,
        unchanged,
    })
}

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("cannot {action} {path}: {source}")]
    Fs {
        action: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
}

impl RestoreError {
    fn fs(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Fs {
            action,
            path: path.display().to_string(),
            source,
        }
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub written: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub skipped: Vec<String>,
}

pub fn apply_plan(
    root: &Path,
    blob_dir: &Path,
    plan: &Plan,
) -> Result<RestoreReport, RestoreError> {
    let mut report = RestoreReport {
        unchanged: plan.unchanged,
        ..RestoreReport::default()
    };

    for relative in &plan.deletes {
        let path = join_rel(root, relative)?;
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(RestoreError::fs("inspect", &path, error)),
        };
        if is_reparse(&path) {
            report
                .skipped
                .push(format!("{relative}: symlink or reparse point"));
            continue;
        }
        if meta.is_dir() {
            report
                .skipped
                .push(format!("{relative}: a directory replaced this file"));
            continue;
        }
        if !parents_are_real(root, relative)? {
            report
                .skipped
                .push(format!("{relative}: parent is a symlink"));
            continue;
        }
        make_writable(&path).map_err(|error| RestoreError::fs("make writable", &path, error))?;
        match fs::remove_file(long_path(&path)) {
            Ok(()) => report.deleted += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(RestoreError::fs("delete", &path, error)),
        }
    }

    for write in &plan.writes {
        let path = join_rel(root, &write.path)?;
        if !prepare_parents(root, &write.path)? {
            report.skipped.push(format!(
                "{}: parent is a symlink or not a directory",
                write.path
            ));
            continue;
        }
        let meta = fs::symlink_metadata(&path).ok();
        let exists = meta.is_some();
        if exists && is_reparse(&path) {
            report
                .skipped
                .push(format!("{}: symlink or reparse point", write.path));
            continue;
        }
        if meta.as_ref().is_some_and(|meta| meta.is_dir()) {
            report
                .skipped
                .push(format!("{}: a directory replaced this file", write.path));
            continue;
        }

        if write.content || !exists {
            let bytes = read_blob(blob_dir, &write.hash)?;
            let temp = temp_path(&path);
            if let Err(error) = fs::write(long_path(&temp), &bytes) {
                return Err(RestoreError::fs("write", &temp, error));
            }
            if let Err(error) = set_mtime(&temp, write.mtime_ns) {
                remove_temp(&temp);
                return Err(RestoreError::fs("set mtime on", &temp, error));
            }
            if let Err(error) = set_mode(&temp, write.mode) {
                remove_temp(&temp);
                return Err(RestoreError::fs("set mode on", &temp, error));
            }
            if exists {
                make_writable(&path)
                    .map_err(|error| RestoreError::fs("make writable", &path, error))?;
                if let Err(error) = fs::remove_file(long_path(&path)) {
                    return Err(RestoreError::fs("replace", &path, error));
                }
            }
            if let Err(error) = fs::rename(long_path(&temp), long_path(&path)) {
                return Err(RestoreError::fs("rename", &path, error));
            }
            report.written += 1;
        } else {
            make_writable(&path)
                .map_err(|error| RestoreError::fs("make writable", &path, error))?;
            set_mtime(&path, write.mtime_ns)
                .map_err(|error| RestoreError::fs("set mtime on", &path, error))?;
            set_mode(&path, write.mode)
                .map_err(|error| RestoreError::fs("set mode on", &path, error))?;
        }
    }

    Ok(report)
}

fn remove_temp(path: &Path) {
    // Cleanup after a failed write must not hide the error that caused it.
    if let Err(error) = fs::remove_file(long_path(path)) {
        if error.kind() != io::ErrorKind::NotFound {
            eprintln!(
                "ctrlz: could not remove temporary file {}: {error}",
                path.display()
            );
        }
    }
}

fn was_skipped(target: &Manifest, path: &str) -> bool {
    target.skipped.iter().any(|skipped| {
        path == skipped.path
            || path.starts_with(&format!("{}/", skipped.path.trim_end_matches('/')))
    })
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    path.with_file_name(format!(
        ".{name}.ctrlz-{}-{}",
        std::process::id(),
        crate::util::now_ms()
    ))
}

fn parents_are_real(root: &Path, relative: &str) -> Result<bool, RestoreError> {
    let base = long_path(root);
    let mut current = base.clone();
    let parts: Vec<&str> = relative.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(_) if is_reparse(&current) => return Ok(false),
            Ok(meta) if !meta.is_dir() => return Ok(false),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(RestoreError::fs("inspect", &current, error)),
        }
    }
    Ok(true)
}

fn prepare_parents(root: &Path, relative: &str) -> Result<bool, RestoreError> {
    let mut current = long_path(root);
    let parts: Vec<&str> = relative.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(_) if is_reparse(&current) => return Ok(false),
            Ok(meta) if !meta.is_dir() => return Ok(false),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(RestoreError::fs("create", &current, error)),
                }
            }
            Err(error) => return Err(RestoreError::fs("inspect", &current, error)),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Skipped;

    fn entry(path: &str, hash: &str) -> crate::manifest::Entry {
        crate::manifest::Entry {
            path: path.into(),
            hash: hash.into(),
            size: 4,
            mode: 0o644,
            mtime_ns: 0,
        }
    }

    fn manifest(entries: Vec<crate::manifest::Entry>) -> Manifest {
        let mut manifest = Manifest::empty();
        for entry in entries {
            manifest.entries.insert(entry.path.clone(), entry);
        }
        manifest
    }

    #[test]
    fn orders_deletes_and_writes() {
        let current = manifest(vec![entry("a.txt", "old-a"), entry("b.txt", "b")]);
        let target = manifest(vec![entry("c.txt", "c"), entry("a.txt", "new-a")]);

        let plan = plan_restore(&current, &target, false).unwrap();
        assert_eq!(plan.deletes, vec!["b.txt"]);
        let writes: Vec<&str> = plan.writes.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(writes, vec!["a.txt", "c.txt"]);
        assert!(plan.writes.iter().all(|write| write.content));
    }

    #[test]
    fn counts_unchanged_files_as_a_no_op() {
        let manifest = manifest(vec![entry("a.txt", "a")]);
        let plan = plan_restore(&manifest, &manifest, false).unwrap();
        assert!(plan.deletes.is_empty());
        assert!(plan.writes.is_empty());
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn aborts_on_a_case_collision() {
        let target = manifest(vec![entry("src/App.tsx", "a"), entry("src/app.tsx", "b")]);
        let error = plan_restore(&Manifest::empty(), &target, true).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("src/App.tsx"));
        assert!(message.contains("src/app.tsx"));
        assert!(plan_restore(&Manifest::empty(), &target, false).is_ok());
    }

    #[test]
    fn rejects_paths_that_escape_the_root() {
        for path in ["../evil.txt", "/etc/passwd", "a/../../evil.txt"] {
            let target = manifest(vec![entry(path, "x")]);
            assert!(matches!(
                plan_restore(&Manifest::empty(), &target, false),
                Err(PlanError::Path(_))
            ));
        }
    }

    #[test]
    fn never_deletes_a_file_the_target_skipped() {
        let current = manifest(vec![entry("big.bin", "big")]);
        let mut target = Manifest::empty();
        target.skipped.push(Skipped {
            path: "big.bin".into(),
            size: 9,
            reason: "larger than --max-file 4 B".into(),
        });

        let plan = plan_restore(&current, &target, false).unwrap();
        assert!(plan.deletes.is_empty());
    }
}
