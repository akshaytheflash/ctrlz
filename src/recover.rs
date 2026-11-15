//! Restoring files that the guard captured.
//!
//! Restores reuse `plan_restore` and `apply_plan`, so every safety property the
//! project restore already guarantees holds here too: paths are validated,
//! symlinks are never written through, and content is written to a temporary
//! file before being renamed into place.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Component, Path};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::flags::Severity;
use crate::global::{self, Flag, GlobalStore, ShadowRow};
use crate::manifest::{Entry, Manifest};
use crate::restore::{apply_plan, plan_restore};
use crate::util::{file_mode, human_size, is_reparse, mtime_ns, now_ms};

pub fn undo(id: Option<i64>, dry_run: bool) -> Result<()> {
    let store = GlobalStore::open()?;
    let config = store.config()?;
    let flag = match id {
        Some(id) => store
            .flag(id)?
            .ok_or_else(|| anyhow!("flag {id} not found"))?,
        None => store
            .last_deletion()?
            .ok_or_else(|| anyhow!("no captured deletion to restore"))?,
    };
    let restored = restore_one(&store, &config, &flag, dry_run)?;
    if !restored {
        println!("{} already matches the captured content", flag.path);
    }
    Ok(())
}

pub fn restore_window(since: Duration, min: Severity, dry_run: bool) -> Result<()> {
    let store = GlobalStore::open()?;
    let config = store.config()?;
    let cutoff = now_ms() - since.as_millis().min(i64::MAX as u128) as i64;

    // Newest flag per path wins: restoring ascending order would end on the
    // newest content anyway, but this makes the intent explicit.
    let mut latest: BTreeMap<String, Flag> = BTreeMap::new();
    for flag in store.deletions_since(cutoff, min.rank())? {
        latest.insert(flag.path.clone(), flag);
    }
    if latest.is_empty() {
        println!("nothing to restore in the last {}", human_duration(since));
        return Ok(());
    }

    let mut restored = 0usize;
    let mut skipped = 0usize;
    for flag in latest.values() {
        match restore_one(&store, &config, flag, dry_run) {
            Ok(true) => restored += 1,
            Ok(false) => {}
            Err(error) => {
                eprintln!("skipped {}: {error}", flag.path);
                skipped += 1;
            }
        }
    }
    println!(
        "{} {restored} file(s), skipped {skipped}",
        if dry_run { "would restore" } else { "restored" }
    );
    Ok(())
}

fn restore_one(
    store: &GlobalStore,
    config: &global::Config,
    flag: &Flag,
    dry_run: bool,
) -> Result<bool> {
    if flag.before_hash.is_empty() {
        bail!(
            "flag {} recorded no content. it was never captured, so it cannot be restored",
            flag.id
        );
    }
    if !flag.recoverable {
        bail!(
            "flag {} is not recoverable: its blob is missing from the store",
            flag.id
        );
    }
    if config.root_for(Path::new(&flag.path)).is_none() {
        bail!("{} is outside every configured root", flag.path);
    }

    let root = Path::new(&flag.root);
    if !root.is_dir() {
        bail!("root no longer exists: {}", flag.root);
    }
    let relative = relative_to(root, Path::new(&flag.path))?;
    let current = current_manifest(Path::new(&flag.path), &relative)?;

    let mut target = Manifest::empty();
    target.entries.insert(
        relative.clone(),
        Entry {
            path: relative.clone(),
            hash: flag.before_hash.clone(),
            size: flag.size,
            mode: flag.mode,
            mtime_ns: flag.mtime_ns,
        },
    );

    let plan = plan_restore(&current, &target, cfg!(windows))?;
    if plan.writes.is_empty() && plan.deletes.is_empty() {
        return Ok(false);
    }
    if dry_run {
        println!(
            "would restore {} ({} bytes from {})",
            flag.path,
            flag.size,
            &flag.before_hash[..flag.before_hash.len().min(12)]
        );
        return Ok(true);
    }

    let report = apply_plan(root, &store.blob_dir(), &plan)?;
    if report.written == 0 {
        for skipped in &report.skipped {
            eprintln!("skipped {skipped}");
        }
        return Ok(false);
    }

    // Keep the shadow index truthful: the file is back to the captured state,
    // so a later `guard check` must not treat the restore as a new change.
    store.upsert_shadow(&ShadowRow {
        key: global::key_of(Path::new(&flag.path)),
        path: flag.path.clone(),
        root: flag.root.clone(),
        hash: flag.before_hash.clone(),
        size: flag.size,
        mode: flag.mode,
        mtime_ns: flag.mtime_ns,
        captured_at: now_ms(),
    })?;

    println!("restored {}", flag.path);
    Ok(true)
}

pub fn forget(keep: Duration) -> Result<()> {
    let store = GlobalStore::open()?;
    let cutoff = now_ms() - keep.as_millis().min(i64::MAX as u128) as i64;
    let removed = store.forget(cutoff)?;
    let reclaimed = reclaim(&store)?;
    println!(
        "forgot {removed} flag(s) older than {} and reclaimed {}",
        human_duration(keep),
        human_size(reclaimed)
    );
    Ok(())
}

/// Delete blobs that neither the shadow index nor a surviving flag needs.
fn reclaim(store: &GlobalStore) -> Result<u64> {
    let mut referenced: HashSet<String> = store
        .shadow_all()?
        .into_iter()
        .map(|row| row.hash)
        .collect();
    for flag in store.flags_since(0, 0, usize::MAX)? {
        if !flag.before_hash.is_empty() {
            referenced.insert(flag.before_hash);
        }
    }

    let mut reclaimed = 0u64;
    let blobs = store.blob_dir();
    let Ok(shards) = fs::read_dir(&blobs) else {
        return Ok(0);
    };
    for shard in shards.flatten() {
        let Ok(files) = fs::read_dir(shard.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if referenced.contains(&name) {
                continue;
            }
            let Ok(meta) = file.metadata() else {
                continue;
            };
            if meta.is_file() && fs::remove_file(file.path()).is_ok() {
                reclaimed += meta.len();
            }
        }
    }
    Ok(reclaimed)
}

fn current_manifest(path: &Path, relative: &str) -> Result<Manifest> {
    let mut manifest = Manifest::empty();
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(manifest);
    };
    if !meta.is_file() || is_reparse(path) {
        return Ok(manifest);
    }
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    manifest.entries.insert(
        relative.to_string(),
        Entry {
            path: relative.to_string(),
            hash: blake3::hash(&bytes).to_hex().to_string(),
            size: meta.len(),
            mode: file_mode(&meta),
            mtime_ns: mtime_ns(&meta),
        },
    );
    Ok(manifest)
}

fn relative_to(root: &Path, path: &Path) -> Result<String> {
    if !global::is_within(root, path) {
        bail!("{} is not inside {}", path.display(), root.display());
    }
    let depth = root.components().count();
    let relative: Vec<String> = path
        .components()
        .skip(depth)
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if relative.is_empty() {
        bail!("{} is the root itself", path.display());
    }
    Ok(relative.join("/"))
}

fn human_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_computed_from_components() {
        let root = if cfg!(windows) {
            Path::new(r"C:\work")
        } else {
            Path::new("/work")
        };
        let path = root.join("src").join("main.rs");
        assert_eq!(relative_to(root, &path).unwrap(), "src/main.rs");
    }

    #[test]
    fn a_path_outside_the_root_is_rejected() {
        let root = if cfg!(windows) {
            Path::new(r"C:\work")
        } else {
            Path::new("/work")
        };
        let outside = if cfg!(windows) {
            Path::new(r"C:\elsewhere\file.txt")
        } else {
            Path::new("/elsewhere/file.txt")
        };
        assert!(relative_to(root, outside).is_err());
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(Duration::from_secs(30)), "30s");
        assert_eq!(human_duration(Duration::from_secs(120)), "2m");
        assert_eq!(human_duration(Duration::from_secs(7_200)), "2h");
        assert_eq!(human_duration(Duration::from_secs(172_800)), "2d");
    }
}
