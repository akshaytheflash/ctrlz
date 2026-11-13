use std::path::Path;
use std::time::Duration;

use thiserror::Error;

use crate::manifest::{manifest_of, Cached, Manifest, ManifestError};
use crate::store::{Store, StoreError, Turn};
use crate::util::now_ms;

pub const DEFAULT_MAX_FILE: u64 = 8 * 1024 * 1024;

pub struct MarkOptions<'a> {
    pub agent: &'a str,
    pub session: &'a str,
    pub label: &'a str,
    pub if_stale: Option<Duration>,
    pub max_file: u64,
}

impl<'a> MarkOptions<'a> {
    pub fn new(agent: &'a str, label: &'a str) -> Self {
        Self {
            agent,
            session: "",
            label,
            if_stale: None,
            max_file: DEFAULT_MAX_FILE,
        }
    }
}

#[derive(Debug, Error)]
pub enum MarkError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

pub fn mark_at(root: &Path, options: &MarkOptions<'_>) -> Result<Option<Turn>, MarkError> {
    let mut store = Store::open(root)?;
    let last = store.last_turn()?;

    if let (Some(window), Some(turn)) = (options.if_stale, last.as_ref()) {
        let age = now_ms() - turn.started_at;
        if age >= 0 && (age as u128) < window.as_millis() {
            return Ok(None);
        }
    }

    let cache = store.cached_entries(last.as_ref().map(|turn| turn.id))?;
    let manifest = manifest_of(&store.root, &store.blob_dir(), options.max_file, &cache)?;

    // A no-op prompt or manual mark should not copy a 50k-row manifest. Reuse
    // the latest turn, but never relabel a pre-restore snapshot: undo-an-undo
    // depends on it. Stop always records a real turn end.
    if let Some(last) = last.as_ref() {
        if last.label != "pre-restore"
            && options.label != "stop"
            && manifest.skipped.is_empty()
            && same_as_previous(&manifest, &cache)
        {
            let turn = store.touch_turn(last.id, options.agent, options.session, options.label)?;
            return Ok(Some(turn));
        }
    }

    let turn = store.record_turn_using(
        &manifest,
        last.as_ref().map(|turn| turn.id),
        &cache,
        options.agent,
        options.session,
        options.label,
    )?;
    Ok(Some(turn))
}

fn same_as_previous(
    manifest: &Manifest,
    previous: &std::collections::HashMap<String, Cached>,
) -> bool {
    manifest.entries.len() == previous.len()
        && manifest.entries.values().all(|entry| {
            previous.get(&entry.path).is_some_and(|old| {
                old.hash == entry.hash
                    && old.size == entry.size
                    && old.mode == entry.mode
                    && old.mtime_ns == entry.mtime_ns
            })
        })
}
