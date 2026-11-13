use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use thiserror::Error;

use crate::ignore::is_excluded_name;
use crate::store::{write_blob, BlobError};
use crate::util::{file_mode, human_size, mtime_ns, rel_string};

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
            !(is_dir && reparse_entry(entry))
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
        let meta = match entry.metadata() {
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

        if kind.is_symlink() || reparse_entry(&entry) {
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
        let reused = cache
            .get(&relative)
            .filter(|old| old.size == size && old.mtime_ns == modified);
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

fn reparse_entry(entry: &::ignore::DirEntry) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        entry
            .metadata()
            .map(|meta| meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        entry
            .file_type()
            .map(|kind| kind.is_symlink())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn keys(manifest: &Manifest) -> Vec<&str> {
        manifest.entries.keys().map(String::as_str).collect()
    }

    #[test]
    fn applies_gitignore_ctrlzignore_and_hardcoded_excludes() {
        let root = TempDir::new().unwrap();
        let blobs = TempDir::new().unwrap();
        write(root.path(), "keep.txt", "keep");
        write(root.path(), "ignored.txt", "ignored");
        write(root.path(), "custom.log", "custom");
        write(root.path(), "important.log", "important");
        write(root.path(), "node_modules/pkg/index.js", "module");
        write(root.path(), "target/debug/app", "binary");
        write(root.path(), ".gitignore", "ignored.txt\n*.log\n");
        write(
            root.path(),
            ".ctrlzignore",
            "!important.log\n!node_modules/pkg/index.js\n",
        );

        let manifest = manifest_of(
            root.path(),
            blobs.path(),
            1024 * 1024,
            &std::collections::HashMap::new(),
        )
        .unwrap();
        let files = keys(&manifest);

        assert!(files.contains(&"keep.txt"));
        assert!(files.contains(&"important.log"));
        assert!(files.contains(&".gitignore"));
        assert!(files.contains(&".ctrlzignore"));
        assert!(!files.contains(&"ignored.txt"));
        assert!(!files.contains(&"custom.log"));
        assert!(!files.contains(&"node_modules/pkg/index.js"));
        assert!(!files.contains(&"target/debug/app"));
    }

    #[test]
    fn records_files_over_max_file_as_skipped() {
        let root = TempDir::new().unwrap();
        let blobs = TempDir::new().unwrap();
        write(root.path(), "small.txt", "ok");
        write(root.path(), "big.bin", "0123456789");

        let manifest = manifest_of(
            root.path(),
            blobs.path(),
            4,
            &std::collections::HashMap::new(),
        )
        .unwrap();

        assert!(manifest.entries.contains_key("small.txt"));
        assert!(!manifest.entries.contains_key("big.bin"));
        let skipped = manifest
            .skipped
            .iter()
            .find(|entry| entry.path == "big.bin")
            .unwrap();
        assert!(skipped.reason.contains("--max-file"));
    }
}
