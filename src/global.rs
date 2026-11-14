//! The machine-wide guard store: configuration, schema, and queries.
//!
//! This is deliberately separate from the per-project store in `store.rs`.
//! A project store is rooted at exactly one repository and records turns; the
//! guard store spans several configured roots and records a shadow index of
//! every in-scope file plus a ledger of flagged changes. Keeping them apart
//! means the project commands cannot be broken by the guard and vice versa.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::glob;
use crate::store::home;
use crate::util::now_ms;

pub const DEFAULT_MAX_FILE: u64 = 8 * 1024 * 1024;
pub const DEFAULT_MAX_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_MASS_DELETE: usize = 20;
pub const DEFAULT_RESCAN_SECS: u64 = 600;
/// How long an agent activity marker stays valid without being refreshed.
pub const ACTIVITY_TTL_MS: i64 = 5 * 60 * 1000;

/// Directory names that are never worth shadowing: reproducible build output
/// and caches. `AppData` is here because it holds browser profiles, token
/// stores, and caches rather than the user's work.
const NEVER_SHADOW: &[&str] = &["AppData", ".cache", ".cargo", ".rustup"];

/// Caches that live outside a `NEVER_SHADOW` directory.
const NEVER_SHADOW_PATTERNS: &[&str] = &[
    "Library/Caches/**",
    "Library/Logs/**",
    ".npm/_cacache/**",
    ".local/share/Trash/**",
];

/// Credentials and configuration that a rogue agent is most likely to damage.
/// These are protected by default and cannot be removed from the list, only
/// extended: silently dropping `.env` protection would be a footgun.
const DEFAULT_PROTECT: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".ssh/**",
    ".aws/**",
    ".gnupg/**",
    ".git/config",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub roots: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub protect: Vec<String>,
    #[serde(default = "default_max_file")]
    pub max_file: u64,
    #[serde(default = "default_max_total")]
    pub max_total: u64,
    #[serde(default = "default_rescan_secs")]
    pub rescan_secs: u64,
    #[serde(default = "default_mass_delete")]
    pub mass_delete: usize,
}

fn default_max_file() -> u64 {
    DEFAULT_MAX_FILE
}
fn default_max_total() -> u64 {
    DEFAULT_MAX_TOTAL
}
fn default_rescan_secs() -> u64 {
    DEFAULT_RESCAN_SECS
}
fn default_mass_delete() -> usize {
    DEFAULT_MASS_DELETE
}

impl Config {
    /// A config with no roots. `guard init` fills them in.
    pub fn new(roots: Vec<String>) -> Self {
        Self {
            roots,
            exclude: Vec::new(),
            protect: Vec::new(),
            max_file: DEFAULT_MAX_FILE,
            max_total: DEFAULT_MAX_TOTAL,
            rescan_secs: DEFAULT_RESCAN_SECS,
            mass_delete: DEFAULT_MASS_DELETE,
        }
    }

    /// Built-in protections plus whatever the user added.
    pub fn protect_patterns(&self) -> Vec<String> {
        let mut patterns: Vec<String> = DEFAULT_PROTECT
            .iter()
            .map(|item| item.to_string())
            .collect();
        patterns.extend(self.protect.iter().cloned());
        patterns
    }

    pub fn is_protected(&self, path: &Path) -> bool {
        let Some(text) = path.to_str() else {
            return false;
        };
        let patterns = self.protect_patterns();
        glob::any_matches(patterns.iter().map(String::as_str), text)
    }

    /// The longest configured root that contains `path`, if any.
    pub fn root_for(&self, path: &Path) -> Option<&str> {
        self.roots
            .iter()
            .filter(|root| is_within(Path::new(root), path))
            .max_by_key(|root| Path::new(root).components().count())
            .map(String::as_str)
    }

    /// `(root, path-relative-to-root with '/' separators)`.
    pub fn relative_to(&self, path: &Path) -> Option<(String, String)> {
        let root = self.root_for(path)?;
        let depth = Path::new(root).components().count();
        let relative: Vec<String> = path
            .components()
            .skip(depth)
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        if relative.is_empty() {
            return None;
        }
        Some((root.to_string(), relative.join("/")))
    }

    pub fn is_excluded_rel(&self, relative: &str) -> bool {
        if relative.split('/').any(is_never_shadow) {
            return true;
        }
        if glob::any_matches(NEVER_SHADOW_PATTERNS.iter().copied(), relative) {
            return true;
        }
        glob::any_matches(self.exclude.iter().map(String::as_str), relative)
    }

    /// Whether a path is worth shadowing. `store_dir` is excluded so the guard
    /// never captures its own blobs.
    pub fn in_scope(&self, path: &Path, store_dir: &Path) -> bool {
        if is_within(store_dir, path) {
            return false;
        }
        let Some((_, relative)) = self.relative_to(path) else {
            return false;
        };
        !self.is_excluded_rel(&relative)
    }
}

pub fn is_never_shadow(name: &str) -> bool {
    crate::ignore::is_excluded_name(name)
        || NEVER_SHADOW
            .iter()
            .any(|item| name.eq_ignore_ascii_case(item))
}

/// Component-wise containment that respects Windows' case-insensitive paths.
pub fn is_within(root: &Path, path: &Path) -> bool {
    let root: Vec<Component> = root.components().collect();
    let path: Vec<Component> = path.components().collect();
    if root.is_empty() || path.len() < root.len() {
        return false;
    }
    root.iter().zip(path.iter()).all(|(left, right)| {
        if cfg!(windows) {
            left.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
        } else {
            left == right
        }
    })
}

/// A filesystem path normalised for use as a database key. Windows paths are
/// lowercased so `C:\Work\a.txt` and `c:\work\A.TXT` are one row.
pub fn key_of(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

#[derive(Debug, Clone)]
pub struct ShadowRow {
    pub key: String,
    /// The real spelling, kept so a restore writes the original case back.
    pub path: String,
    pub root: String,
    pub hash: String,
    pub size: u64,
    pub mode: u32,
    pub mtime_ns: i64,
    pub captured_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewFlag {
    pub at: i64,
    pub kind: String,
    pub path: String,
    pub root: String,
    pub severity: String,
    pub severity_rank: i64,
    pub agent: String,
    pub session: String,
    pub confidence: String,
    pub before_hash: String,
    pub size: u64,
    pub mode: u32,
    pub mtime_ns: i64,
    pub recoverable: bool,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct Flag {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub path: String,
    pub root: String,
    pub severity: String,
    pub agent: String,
    pub session: String,
    pub confidence: String,
    pub before_hash: String,
    pub size: u64,
    pub mode: u32,
    pub mtime_ns: i64,
    pub recoverable: bool,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct Activity {
    pub agent: String,
    pub session: String,
    pub cwd: String,
    pub at: i64,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS shadow (
    key TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    root TEXT NOT NULL,
    hash TEXT NOT NULL,
    size INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    captured_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS flags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    path TEXT NOT NULL,
    root TEXT NOT NULL,
    severity TEXT NOT NULL,
    severity_rank INTEGER NOT NULL,
    agent TEXT NOT NULL DEFAULT '',
    session TEXT NOT NULL DEFAULT '',
    confidence TEXT NOT NULL DEFAULT 'none',
    before_hash TEXT NOT NULL DEFAULT '',
    size INTEGER NOT NULL DEFAULT 0,
    mode INTEGER NOT NULL DEFAULT 0,
    mtime_ns INTEGER NOT NULL DEFAULT 0,
    recoverable INTEGER NOT NULL DEFAULT 1,
    note TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS flags_at ON flags(at);
CREATE INDEX IF NOT EXISTS flags_rank ON flags(severity_rank);
CREATE TABLE IF NOT EXISTS activity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    agent TEXT NOT NULL,
    session TEXT NOT NULL,
    cwd TEXT NOT NULL,
    at INTEGER NOT NULL
);
"#;

pub struct GlobalStore {
    pub dir: PathBuf,
    pub conn: Connection,
}

impl GlobalStore {
    pub fn dir() -> Result<PathBuf> {
        Ok(home().map_err(|error| anyhow!("{error}"))?.join("global"))
    }

    pub fn open() -> Result<Self> {
        let dir = Self::dir()?;
        if !dir.join("global.json").is_file() {
            return Err(anyhow!(
                "no guard store at {}. run `ctrlz guard init --root <path>` first",
                dir.display()
            ));
        }
        let conn = Self::open_db(&dir)?;
        Ok(Self { dir, conn })
    }

    pub fn create(config: &Config) -> Result<(Self, bool)> {
        let dir = Self::dir()?;
        let created = !dir.join("global.json").is_file();
        fs::create_dir_all(dir.join("blobs"))
            .with_context(|| format!("cannot create {}", dir.join("blobs").display()))?;
        let conn = Self::open_db(&dir)?;
        let store = Self { dir, conn };
        if created {
            store.save_config(config)?;
        }
        Ok((store, created))
    }

    fn open_db(dir: &Path) -> Result<Connection> {
        let path = dir.join("index.sqlite");
        let conn =
            Connection::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(conn)
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.dir.join("blobs")
    }

    pub fn config(&self) -> Result<Config> {
        let path = self.dir.join("global.json");
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid guard config", path.display()))
    }

    pub fn save_config(&self, config: &Config) -> Result<()> {
        let path = self.dir.join("global.json");
        let mut text = serde_json::to_string_pretty(config)?;
        text.push('\n');
        fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn shadow(&self, key: &str) -> Result<Option<ShadowRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT key, path, root, hash, size, mode, mtime_ns, captured_at \
                 FROM shadow WHERE key = ?1",
                params![key],
                |row| {
                    Ok(ShadowRow {
                        key: row.get(0)?,
                        path: row.get(1)?,
                        root: row.get(2)?,
                        hash: row.get(3)?,
                        size: row.get::<_, i64>(4)? as u64,
                        mode: row.get::<_, i64>(5)? as u32,
                        mtime_ns: row.get(6)?,
                        captured_at: row.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn shadow_all(&self) -> Result<Vec<ShadowRow>> {
        let mut statement = self.conn.prepare(
            "SELECT key, path, root, hash, size, mode, mtime_ns, captured_at FROM shadow",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ShadowRow {
                key: row.get(0)?,
                path: row.get(1)?,
                root: row.get(2)?,
                hash: row.get(3)?,
                size: row.get::<_, i64>(4)? as u64,
                mode: row.get::<_, i64>(5)? as u32,
                mtime_ns: row.get(6)?,
                captured_at: row.get(7)?,
            })
        })?;
        let mut rows_out = Vec::new();
        for row in rows {
            rows_out.push(row?);
        }
        Ok(rows_out)
    }

    pub fn upsert_shadow(&self, row: &ShadowRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO shadow (key, path, root, hash, size, mode, mtime_ns, captured_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(key) DO UPDATE SET path = ?2, root = ?3, hash = ?4, size = ?5, \
             mode = ?6, mtime_ns = ?7, captured_at = ?8",
            params![
                row.key,
                row.path,
                row.root,
                row.hash,
                row.size as i64,
                row.mode as i64,
                row.mtime_ns,
                row.captured_at
            ],
        )?;
        Ok(())
    }

    pub fn remove_shadow(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM shadow WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn shadow_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM shadow", [], |row| row.get(0))?)
    }

    pub fn add_flag(&self, flag: &NewFlag) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO flags (at, kind, path, root, severity, severity_rank, agent, session, \
             confidence, before_hash, size, mode, mtime_ns, recoverable, note) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                flag.at,
                flag.kind,
                flag.path,
                flag.root,
                flag.severity,
                flag.severity_rank,
                flag.agent,
                flag.session,
                flag.confidence,
                flag.before_hash,
                flag.size as i64,
                flag.mode as i64,
                flag.mtime_ns,
                i64::from(flag.recoverable),
                flag.note
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn flag(&self, id: i64) -> Result<Option<Flag>> {
        let flag = self
            .conn
            .query_row(
                "SELECT id, at, kind, path, root, severity, agent, session, \
                 confidence, before_hash, size, mode, mtime_ns, recoverable, note \
                 FROM flags WHERE id = ?1",
                params![id],
                row_to_flag,
            )
            .optional()?;
        Ok(flag)
    }

    pub fn flags_since(&self, since: i64, min_rank: i64, limit: usize) -> Result<Vec<Flag>> {
        let mut statement = self.conn.prepare(
            "SELECT id, at, kind, path, root, severity, agent, session, \
             confidence, before_hash, size, mode, mtime_ns, recoverable, note \
             FROM flags WHERE at >= ?1 AND severity_rank >= ?2 ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(params![since, min_rank, limit as i64], row_to_flag)?;
        let mut flags = Vec::new();
        for row in rows {
            flags.push(row?);
        }
        Ok(flags)
    }

    /// Deletion-shaped flags in a window, oldest first, so restoring a window
    /// ends with the newest content.
    pub fn deletions_since(&self, since: i64, min_rank: i64) -> Result<Vec<Flag>> {
        let mut statement = self.conn.prepare(
            "SELECT id, at, kind, path, root, severity, agent, session, \
             confidence, before_hash, size, mode, mtime_ns, recoverable, note \
             FROM flags WHERE at >= ?1 AND severity_rank >= ?2 \
             AND kind IN ('deleted', 'modified') AND before_hash != '' ORDER BY id ASC",
        )?;
        let rows = statement.query_map(params![since, min_rank], row_to_flag)?;
        let mut flags = Vec::new();
        for row in rows {
            flags.push(row?);
        }
        Ok(flags)
    }

    pub fn last_deletion(&self) -> Result<Option<Flag>> {
        let flag = self
            .conn
            .query_row(
                "SELECT id, at, kind, path, root, severity, agent, session, \
                 confidence, before_hash, size, mode, mtime_ns, recoverable, note \
                 FROM flags WHERE kind IN ('deleted', 'modified') AND before_hash != '' \
                 ORDER BY id DESC LIMIT 1",
                [],
                row_to_flag,
            )
            .optional()?;
        Ok(flag)
    }

    pub fn flag_counts(&self) -> Result<(i64, i64, i64)> {
        let critical: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM flags WHERE severity_rank = 2",
            [],
            |row| row.get(0),
        )?;
        let warn: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM flags WHERE severity_rank = 1",
            [],
            |row| row.get(0),
        )?;
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM flags", [], |row| row.get(0))?;
        Ok((critical, warn, total))
    }

    pub fn forget(&self, before: i64) -> Result<usize> {
        let removed = self
            .conn
            .execute("DELETE FROM flags WHERE at < ?1", params![before])?;
        Ok(removed)
    }

    pub fn record_activity(&self, agent: &str, session: &str, cwd: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO activity (id, agent, session, cwd, at) VALUES (1, ?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET agent = ?1, session = ?2, cwd = ?3, at = ?4",
            params![agent, session, cwd, now_ms()],
        )?;
        Ok(())
    }

    pub fn clear_activity(&self) -> Result<()> {
        self.conn.execute("DELETE FROM activity", [])?;
        Ok(())
    }

    /// The agent currently working, if its marker has not gone stale.
    pub fn active_activity(&self) -> Result<Option<Activity>> {
        let activity = self
            .conn
            .query_row(
                "SELECT agent, session, cwd, at FROM activity WHERE id = 1",
                [],
                |row| {
                    Ok(Activity {
                        agent: row.get(0)?,
                        session: row.get(1)?,
                        cwd: row.get(2)?,
                        at: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(activity.filter(|activity| now_ms() - activity.at <= ACTIVITY_TTL_MS))
    }
}

fn row_to_flag(row: &rusqlite::Row<'_>) -> rusqlite::Result<Flag> {
    Ok(Flag {
        id: row.get(0)?,
        at: row.get(1)?,
        kind: row.get(2)?,
        path: row.get(3)?,
        root: row.get(4)?,
        severity: row.get(5)?,
        agent: row.get(6)?,
        session: row.get(7)?,
        confidence: row.get(8)?,
        before_hash: row.get(9)?,
        size: row.get::<_, i64>(10)? as u64,
        mode: row.get::<_, i64>(11)? as u32,
        mtime_ns: row.get(12)?,
        recoverable: row.get::<_, i64>(13)? != 0,
        note: row.get(14)?,
    })
}

/// Blob bytes on disk, used by `guard status` without opening the store twice.
pub fn usage_of(dir: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    let blobs = dir.join("blobs");
    let Ok(shards) = fs::read_dir(&blobs) else {
        return Ok(0);
    };
    for shard in shards.flatten() {
        let Ok(files) = fs::read_dir(shard.path()) else {
            continue;
        };
        for file in files.flatten() {
            if let Ok(meta) = file.metadata() {
                if meta.is_file() {
                    total += meta.len();
                }
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::new(vec![if cfg!(windows) {
            r"C:\work".into()
        } else {
            "/work".into()
        }])
    }

    #[test]
    fn a_path_outside_every_root_is_not_in_scope() {
        let config = config();
        let outside = if cfg!(windows) {
            Path::new(r"C:\elsewhere\file.txt")
        } else {
            Path::new("/elsewhere/file.txt")
        };
        assert!(!config.in_scope(outside, Path::new("/store")));
    }

    #[test]
    fn build_output_is_never_shadowed() {
        let config = config();
        let root = Path::new(&config.roots[0]);
        let store = Path::new("/store");
        assert!(config.in_scope(&root.join("src/main.rs"), store));
        assert!(!config.in_scope(&root.join("node_modules/pkg/index.js"), store));
        assert!(!config.in_scope(&root.join("target/debug/ctrlz"), store));
        assert!(!config.in_scope(&root.join(".git/config"), store));
    }

    #[test]
    fn the_store_directory_is_excluded() {
        let config = config();
        let root = Path::new(&config.roots[0]);
        let store = root.join(".store");
        assert!(!config.in_scope(&store.join("blobs/aa/deadbeef"), &store));
    }

    #[test]
    fn credential_files_are_protected_by_default() {
        let config = config();
        assert!(config.is_protected(Path::new("/work/.env")));
        assert!(config.is_protected(Path::new("/work/keys/server.pem")));
        assert!(config.is_protected(Path::new("/home/me/.ssh/id_ed25519")));
        assert!(!config.is_protected(Path::new("/work/src/main.rs")));
    }

    #[test]
    fn user_protect_patterns_are_added_not_substituted() {
        let mut config = config();
        config.protect.push("*.sql".into());
        assert!(config.is_protected(Path::new("/work/db.sql")));
        assert!(config.is_protected(Path::new("/work/.env")));
    }

    #[test]
    fn the_longest_matching_root_wins() {
        let mut config = config();
        let root = config.roots[0].clone();
        config.roots.push(format!("{root}/nested"));
        let path = Path::new(&config.roots[1]).join("a.txt");
        assert_eq!(config.root_for(&path), Some(config.roots[1].as_str()));
        assert_eq!(
            config.relative_to(&path).map(|(_, rel)| rel),
            Some("a.txt".to_string())
        );
    }

    #[test]
    fn keys_normalise_separators_and_case() {
        let key = key_of(Path::new("/work/src/main.rs"));
        assert!(key.contains("src/main.rs"));
        #[cfg(windows)]
        {
            let upper = key_of(Path::new(r"C:\WORK\SRC\MAIN.RS"));
            assert_eq!(upper, key_of(Path::new(r"c:\work\src\main.rs")));
            assert!(upper.contains("src/main.rs"));
        }
    }
}
