use std::path::Path;
use std::time::Duration;

use thiserror::Error;

use crate::manifest::{manifest_of, ManifestError};
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
    let turn = store.record_turn(&manifest, options.agent, options.session, options.label)?;
    Ok(Some(turn))
}
