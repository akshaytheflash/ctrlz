use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use thiserror::Error;

use crate::ignore::is_excluded_name;
use crate::store::{blob_exists, write_blob, BlobError};
use crate::util::{file_mode, human_size, is_reparse, mtime_ns, rel_string};

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub mode: u32,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone)]
pub struct Skipped {
    pub path: String,
    pub size: u64,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Cached {
    pub size: u64,
    pub mode: u32,
    pub mtime_ns: i64,
    pub hash: String,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub entries: BTreeMap<String, Entry>,
    pub skipped: Vec<Skipped>,
    pub new_bytes: u64,
}

impl Manifest {
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
            skipped: Vec::new(),
            new_bytes: 0,
        }
    }
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("cannot walk the project: {0}")]
    Walk(String),
    #[error(transparent)]
    Blob(#[from] BlobError),
}

pub fn manifest_of(
    root: &Path,
    blob_dir: &Path,
    max_file: u64,
    cache: &HashMap<String, Cached>,
) -> Result<Manifest, ManifestError> {
    let mut builder = ::ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .add_custom_ignore_filename(".ctrlzignore")
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if is_excluded_name(&entry.file_name().to_string_lossy()) {
                return false;
            }
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            !(is_dir && is_reparse(entry.path()))
        });

    let mut manifest = Manifest::empty();
    for item in builder.build() {
        let entry = item.map_err(|error| ManifestError::Walk(error.to_string()))?;
        if entry.depth() == 0 {
            continue;
        }
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            continue;
        }

        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = rel_string(relative);
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) => {
                manifest.skipped.push(Skipped {
                    path: relative,
                    size: 0,
                    reason: format!("cannot stat: {error}"),
                });
                continue;
            }
        };

        if kind.is_symlink() || is_reparse(path) {
            manifest.skipped.push(Skipped {
                path: relative,
                size: meta.len(),
                reason: "symlink or reparse point".into(),
            });
            continue;
        }
        if !kind.is_file() {
            manifest.skipped.push(Skipped {
                path: relative,
                size: meta.len(),
                reason: "not a regular file".into(),
            });
            continue;
        }

        let size = meta.len();
        if size > max_file {
            manifest.skipped.push(Skipped {
                path: relative,
                size,
                reason: format!("larger than --max-file {}", human_size(max_file)),
            });
            continue;
        }

        let modified = mtime_ns(&meta);
        let reused = cache.get(&relative).filter(|old| {
            old.size == size && old.mtime_ns == modified && blob_exists(blob_dir, &old.hash)
        });
        if let Some(old) = reused {
            manifest.entries.insert(
                relative.clone(),
                Entry {
                    path: relative,
                    hash: old.hash.clone(),
                    size,
                    mode: file_mode(&meta),
                    mtime_ns: modified,
                },
            );
            continue;
        }

        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                manifest.skipped.push(Skipped {
                    path: relative,
                    size,
                    reason: format!("cannot read: {error}"),
                });
                continue;
            }
        };
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let (written, compressed) = write_blob(blob_dir, &hash, &bytes)?;
        if written {
            manifest.new_bytes += compressed;
        }
        manifest.entries.insert(
            relative.clone(),
            Entry {
                path: relative,
                hash,
                size,
                mode: file_mode(&meta),
                mtime_ns: modified,
            },
        );
    }

    manifest
        .skipped
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(manifest)
}
