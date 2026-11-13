use std::collections::BTreeMap;

use crate::manifest::{Entry, Manifest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Added,
    Modified,
    Deleted,
    Renamed { from: String },
}

#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    pub kind: Kind,
    pub old: Option<Entry>,
    pub new: Option<Entry>,
}

pub fn diff_manifests(old: &Manifest, new: &Manifest) -> Vec<Change> {
    let mut paths = BTreeMap::new();
    for path in old.entries.keys() {
        paths.insert(path.clone(), ());
    }
    for path in new.entries.keys() {
        paths.insert(path.clone(), ());
    }

    let mut changes = Vec::new();
    for path in paths.keys() {
        match (old.entries.get(path), new.entries.get(path)) {
            (None, Some(entry)) => changes.push(Change {
                path: path.clone(),
                kind: Kind::Added,
                old: None,
                new: Some(entry.clone()),
            }),
            (Some(entry), None) => changes.push(Change {
                path: path.clone(),
                kind: Kind::Deleted,
                old: Some(entry.clone()),
                new: None,
            }),
            (Some(before), Some(after)) => {
                if before.hash != after.hash || before.mode != after.mode {
                    changes.push(Change {
                        path: path.clone(),
                        kind: Kind::Modified,
                        old: Some(before.clone()),
                        new: Some(after.clone()),
                    });
                }
            }
            (None, None) => {}
        }
    }
    changes
}

pub fn detect_renames(changes: Vec<Change>) -> Vec<Change> {
    let mut deleted: BTreeMap<String, Vec<Change>> = BTreeMap::new();
    let mut renamed = Vec::new();

    for change in changes {
        match change.kind {
            Kind::Deleted => {
                if let Some(entry) = change.old.as_ref() {
                    deleted.entry(entry.hash.clone()).or_default().push(change);
                }
            }
            Kind::Added => {
                let hash = change.new.as_ref().map(|entry| entry.hash.clone());
                let pair = hash
                    .as_ref()
                    .and_then(|hash| deleted.get_mut(hash))
                    .and_then(|entries| entries.pop());
                if let Some(old) = pair {
                    renamed.push(Change {
                        path: change.path,
                        kind: Kind::Renamed { from: old.path },
                        old: old.old,
                        new: change.new,
                    });
                } else {
                    renamed.push(change);
                }
            }
            _ => renamed.push(change),
        }
    }

    for mut entries in deleted.into_values() {
        renamed.append(&mut entries);
    }
    renamed.sort_by(|left, right| left.path.cmp(&right.path));
    renamed
}

pub fn unified(old: Option<&[u8]>, new: Option<&[u8]>, path: &str) -> String {
    let old_bytes = old.unwrap_or_default();
    let new_bytes = new.unwrap_or_default();
    if !is_text(old_bytes) || !is_text(new_bytes) {
        return format!("binary {path}\n");
    }

    let old_text = String::from_utf8_lossy(old_bytes);
    let new_text = String::from_utf8_lossy(new_bytes);
    let diff = similar::TextDiff::from_lines(old_text.as_ref(), new_text.as_ref());
    let formatted = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    if formatted.is_empty() {
        format!("mode {path}\n")
    } else {
        formatted
    }
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}
