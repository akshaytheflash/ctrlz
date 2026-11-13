use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::manifest::Manifest;
use crate::store::{read_blob, BlobError};
use crate::util::{
    is_reparse, join_rel, long_path, set_mode, set_mtime, validate_rel_path, PathError,
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
        if !path.exists() {
            continue;
        }
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
        if path.exists() && is_reparse(&path) {
            report
                .skipped
                .push(format!("{}: symlink or reparse point", write.path));
            continue;
        }

        if write.content {
            let bytes = read_blob(blob_dir, &write.hash)?;
            let temp = temp_path(&path);
            if let Err(error) = fs::write(long_path(&temp), &bytes) {
                return Err(RestoreError::fs("write", &temp, error));
            }
            if let Err(error) = set_mtime(&temp, write.mtime_ns) {
                let _ = fs::remove_file(long_path(&temp));
                return Err(RestoreError::fs("set mtime on", &temp, error));
            }
            if let Err(error) = set_mode(&temp, write.mode) {
                let _ = fs::remove_file(long_path(&temp));
                return Err(RestoreError::fs("set mode on", &temp, error));
            }
            if path.exists() {
                if let Err(error) = fs::remove_file(long_path(&path)) {
                    return Err(RestoreError::fs("replace", &path, error));
                }
            }
            if let Err(error) = fs::rename(long_path(&temp), long_path(&path)) {
                return Err(RestoreError::fs("rename", &path, error));
            }
            report.written += 1;
        } else {
            set_mtime(&path, write.mtime_ns)
                .map_err(|error| RestoreError::fs("set mtime on", &path, error))?;
            set_mode(&path, write.mode)
                .map_err(|error| RestoreError::fs("set mode on", &path, error))?;
        }
    }

    Ok(report)
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
