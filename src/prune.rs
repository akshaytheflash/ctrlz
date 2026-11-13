use std::collections::HashSet;
use std::fs;
use std::path::Path;

use rusqlite::params;
use thiserror::Error;

use crate::store::{blob_path, Store, StoreError};
use crate::util::human_size;

#[derive(Debug, Error)]
pub enum PruneError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("store index error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("cannot inspect {path}: {source}")]
    Fs {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
pub struct PruneReport {
    pub turns_removed: usize,
    pub blobs_removed: usize,
    pub bytes_freed: u64,
    pub kept: usize,
    pub store_size: u64,
}

pub fn prune(store: &Store, keep: usize, max_size: Option<u64>) -> Result<PruneReport, PruneError> {
    let keep = keep.max(1);
    let turns = store.turns_desc(usize::MAX)?;
    let mut kept: Vec<i64> = turns.iter().take(keep).map(|turn| turn.id).collect();
    if kept.is_empty() {
        return Ok(PruneReport {
            turns_removed: 0,
            blobs_removed: 0,
            bytes_freed: 0,
            kept: 0,
            store_size: dir_size(&store.project_dir)?,
        });
    }

    if let Some(limit) = max_size {
        loop {
            let referenced = referenced_blobs(store, &kept)?;
            let projected = db_size(&store.project_dir)? + blob_bytes(store, &referenced)?;
            if projected <= limit || kept.len() == 1 {
                break;
            }
            kept.pop();
        }
    }

    let kept_set: HashSet<i64> = kept.iter().copied().collect();
    let drop: Vec<i64> = turns
        .iter()
        .map(|turn| turn.id)
        .filter(|id| !kept_set.contains(id))
        .collect();

    let tx = store.connection().unchecked_transaction()?;
    for id in &drop {
        tx.execute("DELETE FROM turns WHERE id = ?1", params![id])?;
    }
    tx.commit()?;

    let referenced = referenced_blobs(store, &kept)?;
    let (blobs_removed, bytes_freed) = remove_unreferenced(store, &referenced)?;
    store
        .connection()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    Ok(PruneReport {
        turns_removed: drop.len(),
        blobs_removed,
        bytes_freed,
        kept: kept.len(),
        store_size: dir_size(&store.project_dir)?,
    })
}

fn referenced_blobs(store: &Store, turn_ids: &[i64]) -> Result<HashSet<String>, PruneError> {
    let mut hashes = HashSet::new();
    let mut statement = store
        .connection()
        .prepare("SELECT DISTINCT hash FROM files WHERE turn_id = ?1 AND state = 'present'")?;
    for id in turn_ids {
        let rows = statement.query_map(params![id], |row| row.get::<_, String>(0))?;
        for row in rows {
            hashes.insert(row?);
        }
    }
    Ok(hashes)
}

fn remove_unreferenced(
    store: &Store,
    referenced: &HashSet<String>,
) -> Result<(usize, u64), PruneError> {
    let blobs = store.blob_dir();
    let mut removed = 0;
    let mut bytes = 0;
    let entries = match fs::read_dir(&blobs) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => {
            return Err(PruneError::Fs {
                path: blobs.display().to_string(),
                source: error,
            })
        }
    };

    for shard in entries {
        let shard = shard.map_err(|error| PruneError::Fs {
            path: blobs.display().to_string(),
            source: error,
        })?;
        if !shard.path().is_dir() {
            continue;
        }
        let files = fs::read_dir(shard.path()).map_err(|error| PruneError::Fs {
            path: shard.path().display().to_string(),
            source: error,
        })?;
        for file in files {
            let file = file.map_err(|error| PruneError::Fs {
                path: shard.path().display().to_string(),
                source: error,
            })?;
            let name = file.file_name().to_string_lossy().into_owned();
            if referenced.contains(&name) {
                continue;
            }
            let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            fs::remove_file(file.path()).map_err(|error| PruneError::Fs {
                path: file.path().display().to_string(),
                source: error,
            })?;
            removed += 1;
            bytes += size;
        }
        let empty = fs::read_dir(shard.path())
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if empty {
            if let Err(error) = fs::remove_dir(shard.path()) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "ctrlz: could not remove {}: {error}",
                        shard.path().display()
                    );
                }
            }
        }
    }
    Ok((removed, bytes))
}

fn blob_bytes(store: &Store, hashes: &HashSet<String>) -> Result<u64, PruneError> {
    let mut bytes = 0;
    for hash in hashes {
        let path = blob_path(&store.blob_dir(), hash);
        if let Ok(meta) = fs::metadata(path) {
            bytes += meta.len();
        }
    }
    Ok(bytes)
}

fn db_size(project_dir: &Path) -> Result<u64, PruneError> {
    let mut bytes = 0;
    for name in ["index.sqlite", "index.sqlite-wal"] {
        if let Ok(meta) = fs::metadata(project_dir.join(name)) {
            bytes += meta.len();
        }
    }
    Ok(bytes)
}

pub fn dir_size(path: &Path) -> Result<u64, PruneError> {
    let mut bytes = 0;
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(PruneError::Fs {
                path: path.display().to_string(),
                source: error,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| PruneError::Fs {
            path: path.display().to_string(),
            source: error,
        })?;
        let meta = entry.metadata().map_err(|error| PruneError::Fs {
            path: entry.path().display().to_string(),
            source: error,
        })?;
        if meta.is_dir() {
            bytes += dir_size(&entry.path())?;
        } else {
            bytes += meta.len();
        }
    }
    Ok(bytes)
}

pub fn summary(report: &PruneReport) -> String {
    format!(
        "kept {} turns, removed {} turns and {} blobs ({}), store is now {}",
        report.kept,
        report.turns_removed,
        report.blobs_removed,
        human_size(report.bytes_freed),
        human_size(report.store_size)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use crate::store::{blob_exists, write_blob};
    use tempfile::TempDir;

    fn record(store: &mut Store, files: &[(&str, &[u8])]) {
        let mut manifest = Manifest::empty();
        for (path, content) in files {
            let hash = blake3::hash(content).to_hex().to_string();
            write_blob(&store.blob_dir(), &hash, content).unwrap();
            manifest.entries.insert(
                (*path).into(),
                crate::manifest::Entry {
                    path: (*path).into(),
                    hash,
                    size: content.len() as u64,
                    mode: 0o644,
                    mtime_ns: 0,
                },
            );
        }
        store.record_turn(&manifest, "test", "", "turn").unwrap();
    }

    #[test]
    fn keeps_the_newest_turns_and_their_shared_blobs() {
        let home = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let (mut store, _) = Store::init_with(home.path(), repo.path()).unwrap();

        record(
            &mut store,
            &[("shared.txt", b"shared"), ("old.txt", b"old")],
        );
        record(
            &mut store,
            &[("shared.txt", b"shared"), ("new.txt", b"new")],
        );

        let shared = blake3::hash(b"shared").to_hex().to_string();
        let old = blake3::hash(b"old").to_hex().to_string();
        let new = blake3::hash(b"new").to_hex().to_string();

        let report = prune(&store, 1, None).unwrap();
        assert_eq!(report.turns_removed, 1);
        assert_eq!(store.turns_desc(10).unwrap().len(), 1);
        assert!(blob_exists(&store.blob_dir(), &shared));
        assert!(blob_exists(&store.blob_dir(), &new));
        assert!(!blob_exists(&store.blob_dir(), &old));
    }

    #[test]
    fn max_size_can_drop_below_the_keep_count() {
        let home = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let (mut store, _) = Store::init_with(home.path(), repo.path()).unwrap();
        record(&mut store, &[("a.txt", b"one")]);
        record(&mut store, &[("a.txt", b"two")]);
        record(&mut store, &[("a.txt", b"three")]);

        let report = prune(&store, 10, Some(1)).unwrap();
        assert_eq!(report.kept, 1);
        assert_eq!(store.turns_desc(10).unwrap().len(), 1);
    }

    #[test]
    fn keep_zero_still_keeps_one_turn() {
        let home = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let (mut store, _) = Store::init_with(home.path(), repo.path()).unwrap();
        record(&mut store, &[("a.txt", b"a")]);
        record(&mut store, &[("a.txt", b"b")]);

        let report = prune(&store, 0, None).unwrap();
        assert_eq!(report.kept, 1);
        assert_eq!(store.turns_desc(10).unwrap().len(), 1);
    }
}
