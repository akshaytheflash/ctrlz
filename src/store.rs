use std::collections::HashMap;
use std::fs;
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::manifest::{Cached, Entry, Manifest, Skipped};
use crate::util::{human_size, now_ms};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS turns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent TEXT NOT NULL,
    session TEXT NOT NULL DEFAULT '',
    started_at INTEGER NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    file_count INTEGER NOT NULL,
    added INTEGER NOT NULL,
    modified INTEGER NOT NULL,
    deleted INTEGER NOT NULL,
    bytes INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS files (
    turn_id INTEGER NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('present', 'deleted')),
    hash TEXT NOT NULL,
    size INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    PRIMARY KEY (turn_id, path)
) WITHOUT ROWID;
-- Manifest copies are the hot path and no query needs a hash index.
DROP INDEX IF EXISTS files_hash;
CREATE TABLE IF NOT EXISTS skipped (
    turn_id INTEGER NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (turn_id, path)
);
"#;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot read or write {path}: {source}")]
    Fs {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("store index error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("invalid project metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot find a home for the store. set CTRLZ_HOME and retry")]
    NoHome,
    #[error("no ctrlz store for {0}. run `ctrlz init` first")]
    Missing(String),
    #[error(
        "project root moved: the store records {recorded}, but the current folder is {current}. \
         fix: run `ctrlz init` in the current folder to start a new store"
    )]
    Moved { recorded: String, current: String },
}

impl StoreError {
    fn fs(path: &Path, source: io::Error) -> Self {
        Self::Fs {
            path: path.display().to_string(),
            source,
        }
    }
}

#[derive(Debug, Error)]
pub enum BlobError {
    #[error("cannot write blob {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("cannot read blob {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("blob {hash} is corrupt: {message}")]
    Corrupt { hash: String, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectFile {
    root: String,
    created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub id: i64,
    pub agent: String,
    pub session: String,
    pub started_at: i64,
    pub label: String,
    pub file_count: i64,
    pub added: i64,
    pub modified: i64,
    pub deleted: i64,
    pub bytes: i64,
}

pub struct Store {
    pub root: PathBuf,
    pub project_dir: PathBuf,
    conn: Connection,
}

pub fn home() -> Result<PathBuf, StoreError> {
    if let Some(value) = std::env::var_os("CTRLZ_HOME") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    default_home()
}

#[cfg(windows)]
fn default_home() -> Result<PathBuf, StoreError> {
    if let Some(value) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(value).join("ctrlz"));
    }
    if let Some(value) = std::env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(value)
            .join("AppData")
            .join("Local")
            .join("ctrlz"));
    }
    Err(StoreError::NoHome)
}

#[cfg(target_os = "macos")]
fn default_home() -> Result<PathBuf, StoreError> {
    if let Some(value) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(value)
            .join("Library")
            .join("Application Support")
            .join("ctrlz"));
    }
    Err(StoreError::NoHome)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_home() -> Result<PathBuf, StoreError> {
    if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value).join("ctrlz"));
        }
    }
    if let Some(value) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(value)
            .join(".local")
            .join("share")
            .join("ctrlz"));
    }
    Err(StoreError::NoHome)
}

pub fn project_id(root: &Path) -> String {
    let hash = blake3::hash(root.to_string_lossy().as_bytes());
    hash.to_hex()[..16].to_string()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let home = home()?;
        Self::open_with(&home, path)
    }

    pub fn open_with(home: &Path, path: &Path) -> Result<Self, StoreError> {
        let current = fs::canonicalize(path).map_err(|e| StoreError::fs(path, e))?;
        let projects = home.join("projects");

        let mut best: Option<(usize, PathBuf)> = None;
        if let Ok(entries) = fs::read_dir(&projects) {
            for entry in entries.flatten() {
                let dir = entry.path();
                let Ok(text) = fs::read_to_string(dir.join("project.json")) else {
                    continue;
                };
                let Ok(project) = serde_json::from_str::<ProjectFile>(&text) else {
                    continue;
                };
                let recorded = PathBuf::from(&project.root);
                if current.starts_with(&recorded) {
                    let depth = recorded.components().count();
                    if best.as_ref().map(|(d, _)| depth > *d).unwrap_or(true) {
                        best = Some((depth, dir));
                    }
                }
            }
        }

        if let Some((_, dir)) = best {
            return Self::open_dir(&dir, &current);
        }

        let dir = projects.join(project_id(&current));
        if dir.join("project.json").exists() {
            return Self::open_dir(&dir, &current);
        }

        Err(StoreError::Missing(path.display().to_string()))
    }

    pub fn init(path: &Path) -> Result<(Self, bool), StoreError> {
        let home = home()?;
        Self::init_with(&home, path)
    }

    pub fn init_with(home: &Path, path: &Path) -> Result<(Self, bool), StoreError> {
        let root = fs::canonicalize(path).map_err(|e| StoreError::fs(path, e))?;
        let dir = home.join("projects").join(project_id(&root));
        let metadata = dir.join("project.json");

        let created = if metadata.exists() {
            let store = Self::open_dir(&dir, &root)?;
            if !same_path(&store.root, &root) {
                return Err(StoreError::Moved {
                    recorded: store.root.display().to_string(),
                    current: root.display().to_string(),
                });
            }
            return Ok((store, false));
        } else {
            fs::create_dir_all(&dir).map_err(|e| StoreError::fs(&dir, e))?;
            let project = ProjectFile {
                root: root.to_string_lossy().into_owned(),
                created_at: now_ms(),
            };
            let text = serde_json::to_vec_pretty(&project)?;
            fs::write(&metadata, text).map_err(|e| StoreError::fs(&metadata, e))?;
            true
        };

        fs::create_dir_all(dir.join("blobs")).map_err(|e| StoreError::fs(&dir, e))?;
        let conn = Self::open_db(&dir)?;
        let mut store = Self {
            root,
            project_dir: dir,
            conn,
        };
        store.prepare()?;
        Ok((store, created))
    }

    fn open_dir(dir: &Path, current: &Path) -> Result<Self, StoreError> {
        let metadata = dir.join("project.json");
        let text = fs::read_to_string(&metadata).map_err(|e| StoreError::fs(&metadata, e))?;
        let project: ProjectFile = serde_json::from_str(&text)?;
        let recorded = PathBuf::from(&project.root);

        let canonical = fs::canonicalize(&recorded).map_err(|_| StoreError::Moved {
            recorded: recorded.display().to_string(),
            current: current.display().to_string(),
        })?;
        let root = if same_path(&canonical, &recorded) {
            recorded
        } else if current.starts_with(&canonical) {
            canonical
        } else {
            return Err(StoreError::Moved {
                recorded: recorded.display().to_string(),
                current: current.display().to_string(),
            });
        };

        let mut store = Self {
            root,
            project_dir: dir.to_path_buf(),
            conn: Self::open_db(dir)?,
        };
        store.prepare()?;
        Ok(store)
    }

    fn open_db(dir: &Path) -> Result<Connection, StoreError> {
        let path = dir.join("index.sqlite");
        let conn = Connection::open(&path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        Ok(conn)
    }

    fn prepare(&mut self) -> Result<(), StoreError> {
        self.conn.execute_batch(SCHEMA)?;
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.project_dir.join("blobs")
    }

    pub fn last_turn_id(&self) -> Result<Option<i64>, StoreError> {
        let id = self
            .conn
            .query_row("SELECT MAX(id) FROM turns", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?;
        Ok(id)
    }

    pub fn last_turn(&self) -> Result<Option<Turn>, StoreError> {
        match self.last_turn_id()? {
            Some(id) => self.turn(id),
            None => Ok(None),
        }
    }

    pub fn previous_turn_id(&self, id: i64) -> Result<Option<i64>, StoreError> {
        let previous = self.conn.query_row(
            "SELECT MAX(id) FROM turns WHERE id < ?1",
            params![id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok(previous)
    }

    pub fn turn(&self, id: i64) -> Result<Option<Turn>, StoreError> {
        let turn = self
            .conn
            .query_row(
                "SELECT id, agent, session, started_at, label, file_count, added, modified, \
                 deleted, bytes FROM turns WHERE id = ?1",
                params![id],
                |row| {
                    Ok(Turn {
                        id: row.get(0)?,
                        agent: row.get(1)?,
                        session: row.get(2)?,
                        started_at: row.get(3)?,
                        label: row.get(4)?,
                        file_count: row.get(5)?,
                        added: row.get(6)?,
                        modified: row.get(7)?,
                        deleted: row.get(8)?,
                        bytes: row.get(9)?,
                    })
                },
            )
            .optional()?;
        Ok(turn)
    }

    pub fn turns_desc(&self, limit: usize) -> Result<Vec<Turn>, StoreError> {
        let mut statement = self.conn.prepare(
            "SELECT id, agent, session, started_at, label, file_count, added, modified, \
             deleted, bytes FROM turns ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok(Turn {
                id: row.get(0)?,
                agent: row.get(1)?,
                session: row.get(2)?,
                started_at: row.get(3)?,
                label: row.get(4)?,
                file_count: row.get(5)?,
                added: row.get(6)?,
                modified: row.get(7)?,
                deleted: row.get(8)?,
                bytes: row.get(9)?,
            })
        })?;
        let mut turns = Vec::new();
        for turn in rows {
            turns.push(turn?);
        }
        Ok(turns)
    }

    pub fn cached_entries(
        &self,
        turn_id: Option<i64>,
    ) -> Result<HashMap<String, Cached>, StoreError> {
        let Some(turn_id) = turn_id else {
            return Ok(HashMap::new());
        };
        let mut statement = self.conn.prepare(
            "SELECT path, size, mode, mtime_ns, hash FROM files \
             WHERE turn_id = ?1 AND state = 'present'",
        )?;
        let rows = statement.query_map(params![turn_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Cached {
                    size: row.get::<_, i64>(1)? as u64,
                    mode: row.get::<_, i64>(2)? as u32,
                    mtime_ns: row.get(3)?,
                    hash: row.get(4)?,
                },
            ))
        })?;
        let mut entries = HashMap::new();
        for row in rows {
            let (path, cached) = row?;
            entries.insert(path, cached);
        }
        Ok(entries)
    }

    pub fn manifest_of_turn(&self, turn_id: Option<i64>) -> Result<Manifest, StoreError> {
        let Some(turn_id) = turn_id else {
            return Ok(Manifest::empty());
        };
        let mut manifest = Manifest::empty();
        let mut statement = self.conn.prepare(
            "SELECT path, hash, size, mode, mtime_ns FROM files \
             WHERE turn_id = ?1 AND state = 'present'",
        )?;
        let rows = statement.query_map(params![turn_id], |row| {
            Ok(Entry {
                path: row.get(0)?,
                hash: row.get(1)?,
                size: row.get::<_, i64>(2)? as u64,
                mode: row.get::<_, i64>(3)? as u32,
                mtime_ns: row.get(4)?,
            })
        })?;
        for row in rows {
            let entry = row?;
            manifest.entries.insert(entry.path.clone(), entry);
        }

        let mut statement = self
            .conn
            .prepare("SELECT path, size, reason FROM skipped WHERE turn_id = ?1 ORDER BY path")?;
        let rows = statement.query_map(params![turn_id], |row| {
            Ok(Skipped {
                path: row.get(0)?,
                size: row.get::<_, i64>(1)? as u64,
                reason: row.get(2)?,
            })
        })?;
        for row in rows {
            manifest.skipped.push(row?);
        }
        Ok(manifest)
    }

    pub fn touch_turn(
        &mut self,
        id: i64,
        agent: &str,
        session: &str,
        label: &str,
    ) -> Result<Turn, StoreError> {
        self.conn.execute(
            "UPDATE turns SET agent = ?1, session = ?2, started_at = ?3, label = ?4 \
             WHERE id = ?5",
            params![agent, session, now_ms(), label, id],
        )?;
        self.turn(id)?.ok_or_else(|| StoreError::Fs {
            path: self.project_dir.display().to_string(),
            source: io::Error::other(format!("turn {id} disappeared while updating")),
        })
    }

    pub fn record_turn(
        &mut self,
        manifest: &Manifest,
        agent: &str,
        session: &str,
        label: &str,
    ) -> Result<Turn, StoreError> {
        let previous_id = self.last_turn_id()?;
        let previous = self.cached_entries(previous_id)?;
        self.record_turn_using(manifest, previous_id, &previous, agent, session, label)
    }

    pub fn record_turn_using(
        &mut self,
        manifest: &Manifest,
        previous_id: Option<i64>,
        previous: &HashMap<String, Cached>,
        agent: &str,
        session: &str,
        label: &str,
    ) -> Result<Turn, StoreError> {
        let (added, modified, deleted) = change_counts(manifest, previous);
        let started_at = now_ms();
        let bytes = manifest.new_bytes.min(i64::MAX as u64) as i64;
        let record = TurnRecord {
            agent,
            session,
            started_at,
            label,
            added,
            modified,
            deleted,
            bytes,
        };
        let tx = self.conn.transaction()?;
        let turn_id = insert_turn(&tx, manifest, &record)?;
        copy_files(&tx, turn_id, previous_id, manifest, previous)?;
        insert_skipped(&tx, turn_id, &manifest.skipped)?;
        tx.commit()?;

        Ok(Turn {
            id: turn_id,
            agent: agent.to_string(),
            session: session.to_string(),
            started_at,
            label: label.to_string(),
            file_count: manifest.entries.len() as i64,
            added,
            modified,
            deleted,
            bytes,
        })
    }

    pub fn skipped_summary(&self, turn_id: i64) -> Result<Vec<String>, StoreError> {
        let mut statement = self
            .conn
            .prepare("SELECT path, size, reason FROM skipped WHERE turn_id = ?1 ORDER BY path")?;
        let rows = statement.query_map(params![turn_id], |row| {
            let path: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            let reason: String = row.get(2)?;
            Ok(format!("{path} ({}; {reason})", human_size(size as u64)))
        })?;
        let mut summary = Vec::new();
        for row in rows {
            summary.push(row?);
        }
        Ok(summary)
    }
}

fn change_counts(manifest: &Manifest, previous: &HashMap<String, Cached>) -> (i64, i64, i64) {
    let mut added = 0;
    let mut modified = 0;
    for (path, entry) in &manifest.entries {
        match previous.get(path) {
            None => added += 1,
            Some(old) if old.hash != entry.hash || old.mode != entry.mode => modified += 1,
            Some(_) => {}
        }
    }
    let deleted = previous
        .keys()
        .filter(|path| !manifest.entries.contains_key(*path))
        .count() as i64;
    (added, modified, deleted)
}

struct TurnRecord<'a> {
    agent: &'a str,
    session: &'a str,
    started_at: i64,
    label: &'a str,
    added: i64,
    modified: i64,
    deleted: i64,
    bytes: i64,
}

fn insert_turn(
    tx: &Transaction<'_>,
    manifest: &Manifest,
    record: &TurnRecord<'_>,
) -> Result<i64, rusqlite::Error> {
    tx.execute(
        "INSERT INTO turns (agent, session, started_at, label, file_count, added, modified, \
         deleted, bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            record.agent,
            record.session,
            record.started_at,
            record.label,
            manifest.entries.len() as i64,
            record.added,
            record.modified,
            record.deleted,
            record.bytes
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

fn copy_files(
    tx: &Transaction<'_>,
    turn_id: i64,
    previous_id: Option<i64>,
    manifest: &Manifest,
    previous: &HashMap<String, Cached>,
) -> Result<(), rusqlite::Error> {
    if let Some(previous_id) = previous_id {
        tx.execute(
            "INSERT INTO files (turn_id, path, state, hash, size, mode, mtime_ns) \
             SELECT ?1, path, state, hash, size, mode, mtime_ns FROM files WHERE turn_id = ?2",
            params![turn_id, previous_id],
        )?;
    }

    for (path, entry) in &manifest.entries {
        let unchanged = previous.get(path).is_some_and(|old| {
            old.hash == entry.hash && old.size == entry.size && old.mtime_ns == entry.mtime_ns
        });
        if unchanged {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO files \
             (turn_id, path, state, hash, size, mode, mtime_ns) \
             VALUES (?1, ?2, 'present', ?3, ?4, ?5, ?6)",
            params![
                turn_id,
                path,
                entry.hash,
                entry.size as i64,
                entry.mode as i64,
                entry.mtime_ns
            ],
        )?;
    }

    for (path, old) in previous {
        if manifest.entries.contains_key(path) {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO files \
             (turn_id, path, state, hash, size, mode, mtime_ns) \
             VALUES (?1, ?2, 'deleted', ?3, 0, 0, 0)",
            params![turn_id, path, old.hash],
        )?;
    }
    Ok(())
}

fn insert_skipped(
    tx: &Transaction<'_>,
    turn_id: i64,
    skipped: &[Skipped],
) -> Result<(), rusqlite::Error> {
    for skipped in skipped {
        tx.execute(
            "INSERT OR REPLACE INTO skipped (turn_id, path, size, reason) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                turn_id,
                skipped.path,
                skipped.size.min(i64::MAX as u64) as i64,
                skipped.reason
            ],
        )?;
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

pub fn blob_path(blob_dir: &Path, hash: &str) -> PathBuf {
    let shard = hash.get(..2).unwrap_or("00");
    blob_dir.join(shard).join(hash)
}

pub fn blob_exists(blob_dir: &Path, hash: &str) -> bool {
    blob_path(blob_dir, hash).is_file()
}

pub fn write_blob(blob_dir: &Path, hash: &str, content: &[u8]) -> Result<(bool, u64), BlobError> {
    let path = blob_path(blob_dir, hash);
    if let Ok(meta) = fs::metadata(&path) {
        return Ok((false, meta.len()));
    }
    let Some(shard) = path.parent() else {
        return Err(BlobError::Write {
            path: path.display().to_string(),
            source: io::Error::other("blob path has no parent directory"),
        });
    };
    fs::create_dir_all(shard).map_err(|e| BlobError::Write {
        path: shard.display().to_string(),
        source: e,
    })?;

    let compressed =
        zstd::stream::encode_all(Cursor::new(content), 3).map_err(|e| BlobError::Write {
            path: path.display().to_string(),
            source: e,
        })?;
    let length = compressed.len() as u64;

    let mut last_error = None;
    for attempt in 0..3 {
        let temp = shard.join(format!(
            ".{}.{}.{}.{}.tmp",
            hash,
            std::process::id(),
            now_ms(),
            attempt
        ));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
                continue;
            }
            Err(error) => {
                return Err(BlobError::Write {
                    path: temp.display().to_string(),
                    source: error,
                })
            }
        };
        if let Err(error) = file.write_all(&compressed).and_then(|()| file.sync_all()) {
            discard_temp(&temp);
            return Err(BlobError::Write {
                path: temp.display().to_string(),
                source: error,
            });
        }
        drop(file);

        if let Err(error) = fs::rename(&temp, &path) {
            discard_temp(&temp);
            if path.is_file() {
                return Ok((false, length));
            }
            return Err(BlobError::Write {
                path: path.display().to_string(),
                source: error,
            });
        }
        return Ok((true, length));
    }

    Err(BlobError::Write {
        path: path.display().to_string(),
        source: last_error.unwrap_or_else(|| io::Error::other("could not create a temporary blob")),
    })
}

fn discard_temp(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != io::ErrorKind::NotFound {
            eprintln!("ctrlz: could not remove {}: {error}", path.display());
        }
    }
}

pub fn read_blob(blob_dir: &Path, hash: &str) -> Result<Vec<u8>, BlobError> {
    let path = blob_path(blob_dir, hash);
    let compressed = fs::read(&path).map_err(|e| BlobError::Read {
        path: path.display().to_string(),
        source: e,
    })?;
    zstd::stream::decode_all(Cursor::new(compressed)).map_err(|e| BlobError::Corrupt {
        hash: hash.to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn blob_count(dir: &Path) -> usize {
        let mut count = 0;
        for shard in fs::read_dir(dir).unwrap() {
            for file in fs::read_dir(shard.unwrap().path()).unwrap() {
                if file.unwrap().path().is_file() {
                    count += 1;
                }
            }
        }
        count
    }

    #[test]
    fn the_same_content_is_stored_once() {
        let dir = TempDir::new().unwrap();
        let content = b"same bytes";
        let hash = blake3::hash(content).to_hex().to_string();

        let (first, first_size) = write_blob(dir.path(), &hash, content).unwrap();
        let (second, second_size) = write_blob(dir.path(), &hash, content).unwrap();

        assert!(first);
        assert!(!second);
        assert_eq!(first_size, second_size);
        assert_eq!(blob_count(dir.path()), 1);
        assert_eq!(read_blob(dir.path(), &hash).unwrap(), content);
    }

    #[test]
    fn different_content_gets_a_second_blob() {
        let dir = TempDir::new().unwrap();
        let first = blake3::hash(b"one").to_hex().to_string();
        let second = blake3::hash(b"two").to_hex().to_string();

        write_blob(dir.path(), &first, b"one").unwrap();
        write_blob(dir.path(), &second, b"two").unwrap();

        assert_eq!(blob_count(dir.path()), 2);
    }
}
