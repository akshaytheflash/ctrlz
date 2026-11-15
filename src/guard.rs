//! The global guard: baseline capture, change detection, and the watcher loop.
//!
//! The watcher is deliberately thin. All of the behaviour lives in `capture`
//! and `tombstone`, which `guard check` also drives across a full walk. That
//! means the integration tests exercise the real recovery path without sleeping
//! and hoping a filesystem event arrives in time.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use crate::flags::{self, Severity};
use crate::global::{self, key_of, Config, GlobalStore, NewFlag, ShadowRow, ACTIVITY_TTL_MS};
use crate::store::{blob_exists, write_blob};
use crate::util::{file_mode, human_size, is_reparse, mtime_ns, now_ms, rel_string};

/// A lock heartbeat older than this means the daemon is gone.
const LOCK_STALE_MS: i64 = 30_000;
/// How often a running watcher refreshes its heartbeat.
const HEARTBEAT_MS: i64 = 5_000;

pub fn init(
    roots: &[String],
    exclude: &[String],
    protect: &[String],
    max_file: Option<u64>,
) -> Result<()> {
    if roots.is_empty() {
        bail!(
            "at least one --root is required. the guard only watches directories you name; \
             it does not default to your home directory"
        );
    }

    let existing = GlobalStore::open()
        .ok()
        .and_then(|store| store.config().ok());
    let mut config = existing.unwrap_or_else(|| Config::new(Vec::new()));

    for root in roots {
        let path =
            clean(fs::canonicalize(root).with_context(|| format!("cannot use root {root}"))?);
        if !path.is_dir() {
            bail!("root is not a directory: {}", path.display());
        }
        if !config
            .roots
            .iter()
            .any(|known| key_of(Path::new(known)) == key_of(&path))
        {
            config.roots.push(path.to_string_lossy().into_owned());
        }
    }
    config.exclude.extend(exclude.iter().cloned());
    config.protect.extend(protect.iter().cloned());
    if let Some(max_file) = max_file {
        config.max_file = max_file;
    }

    let (store, created) = GlobalStore::create(&config)?;
    store.save_config(&config)?;

    println!(
        "{} guard store at {}",
        if created { "created" } else { "reusing" },
        store.dir.display()
    );
    for root in &config.roots {
        println!("  root  {root}");
    }
    println!();
    println!("next: `ctrlz guard scan` builds the baseline, then `ctrlz guard watch`.");
    println!("the baseline is what makes a later deletion recoverable.");
    Ok(())
}

pub fn scan() -> Result<()> {
    let store = GlobalStore::open()?;
    let config = store.config()?;
    let report = scan_all(&store, &config)?;
    println!(
        "baseline: {} captured, {} unchanged, {} skipped",
        report.captured, report.unchanged, report.skipped
    );
    if report.captured == 0 && report.unchanged == 0 {
        println!("nothing was captured. check that the roots contain files and are readable.");
    }
    Ok(())
}

pub fn check() -> Result<()> {
    let store = GlobalStore::open()?;
    let config = store.config()?;
    let report = check_all(&store, &config)?;
    println!(
        "checked {} root(s): {} new flags, {} deletions, {} capture(s)",
        config.roots.len(),
        report.flags.len(),
        report.deleted,
        report.captured
    );
    for flag in &report.flags {
        println!("  {:<8} {:<10} {}", flag.severity, flag.kind, flag.path);
    }
    Ok(())
}

pub fn check_explicit(paths: &[String]) -> Result<()> {
    let store = GlobalStore::open()?;
    let config = store.config()?;
    let paths: Vec<PathBuf> = paths.iter().map(|path| resolve(Path::new(path))).collect();
    let flags = check_paths(&store, &config, &paths)?;
    for flag in &flags {
        println!("{} {} {}", flag.severity, flag.kind, flag.path);
    }
    println!("{} flag(s)", flags.len());
    Ok(())
}

pub fn activity(agent: Option<&str>, session: Option<&str>, clear: bool) -> Result<()> {
    let store = GlobalStore::open()?;
    if clear {
        store.clear_activity()?;
        println!("cleared the agent marker");
        return Ok(());
    }
    let agent = agent.ok_or_else(|| anyhow!("--agent is required unless --clear is passed"))?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let cwd = cwd.to_string_lossy().into_owned();
    store.record_activity(agent, session.unwrap_or(""), &cwd)?;
    println!(
        "marked {agent} as active for {} minutes",
        ACTIVITY_TTL_MS / 60_000
    );
    Ok(())
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub captured: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

#[derive(Debug)]
pub struct CheckReport {
    pub captured: usize,
    pub deleted: usize,
    pub flags: Vec<FlagView>,
}

#[derive(Debug)]
pub struct FlagView {
    pub severity: String,
    pub kind: String,
    pub path: String,
}

struct Capture {
    hash: String,
    created: bool,
    changed: bool,
}

struct Draft {
    kind: &'static str,
    path: String,
    root: String,
    before_hash: String,
    size: u64,
    mode: u32,
    mtime_ns: i64,
    recoverable: bool,
    severity: Severity,
    note: String,
}

fn scan_all(store: &GlobalStore, config: &Config) -> Result<ScanReport> {
    let existing: HashMap<String, ShadowRow> = store
        .shadow_all()?
        .into_iter()
        .map(|row| (row.key.clone(), row))
        .collect();
    let mut report = ScanReport::default();

    for root in &config.roots {
        let root = PathBuf::from(root);
        walk_root(config, &root, &store.dir, |abs| {
            let key = key_of(abs);
            match capture(store, config, existing.get(&key), abs)? {
                Some(capture) if capture.created => report.captured += 1,
                Some(_) => report.unchanged += 1,
                None => report.skipped += 1,
            }
            Ok(())
        })?;
    }
    Ok(report)
}

fn check_all(store: &GlobalStore, config: &Config) -> Result<CheckReport> {
    let existing: HashMap<String, ShadowRow> = store
        .shadow_all()?
        .into_iter()
        .map(|row| (row.key.clone(), row))
        .collect();
    let active = store.active_activity()?;

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut captures: Vec<(String, Capture)> = Vec::new();

    for root in &config.roots {
        let root = PathBuf::from(root);
        walk_root(config, &root, &store.dir, |abs| {
            if !config.in_scope(abs, &store.dir) {
                return Ok(());
            }
            let key = key_of(abs);
            seen.insert(key.clone());
            // A file we cannot stat between the walk and this call is a race;
            // the next pass catches it. Never turn it into a deletion here.
            if let Some(capture) = capture(store, config, existing.get(&key), abs)? {
                captures.push((abs.to_string_lossy().into_owned(), capture));
            }
            Ok(())
        })?;
    }

    let mut drafts: Vec<Draft> = Vec::new();

    // Files that are in the shadow index but no longer on disk.
    let mut vanished: Vec<ShadowRow> = existing
        .iter()
        .filter(|(key, row)| {
            !seen.contains(*key) && config.in_scope(Path::new(&row.path), &store.dir)
        })
        .map(|(_, row)| row.clone())
        .collect();
    vanished.sort_by(|left, right| left.path.cmp(&right.path));

    let mut created_by_hash: BTreeMap<String, String> = BTreeMap::new();
    for (path, capture) in &captures {
        if capture.created {
            created_by_hash.insert(capture.hash.clone(), path.clone());
        }
    }

    for row in &vanished {
        let recoverable = blob_exists(&store.blob_dir(), &row.hash);
        store.remove_shadow(&row.key)?;
        if let Some(new_path) = created_by_hash.get(&row.hash) {
            drafts.push(Draft {
                kind: "renamed",
                path: row.path.clone(),
                root: row.root.clone(),
                before_hash: row.hash.clone(),
                size: row.size,
                mode: row.mode,
                mtime_ns: row.mtime_ns,
                recoverable,
                severity: Severity::Info,
                note: format!("moved to {new_path}"),
            });
            continue;
        }
        drafts.push(Draft {
            kind: "deleted",
            path: row.path.clone(),
            root: row.root.clone(),
            before_hash: row.hash.clone(),
            size: row.size,
            mode: row.mode,
            mtime_ns: row.mtime_ns,
            recoverable,
            severity: flags::deletion_severity(config, Path::new(&row.path), recoverable),
            note: String::new(),
        });
    }

    let deleted = drafts
        .iter()
        .filter(|draft| draft.kind == "deleted")
        .count();
    if deleted > config.mass_delete {
        let severe = deleted > config.mass_delete.saturating_mul(3);
        for draft in drafts.iter_mut().filter(|draft| draft.kind == "deleted") {
            draft.note = format!("1 of {deleted} files removed in one pass");
            if severe {
                draft.severity = Severity::Critical;
            }
        }
    }

    if let Some(activity) = &active {
        for (path, capture) in &captures {
            if !capture.changed {
                continue;
            }
            let mut note = String::new();
            if !global::is_within(Path::new(&activity.cwd), Path::new(path)) {
                note = "outside the agent's working directory".to_string();
            }
            drafts.push(Draft {
                kind: if capture.created {
                    "created"
                } else {
                    "modified"
                },
                path: path.clone(),
                root: config
                    .relative_to(Path::new(path))
                    .map(|(root, _)| root)
                    .unwrap_or_default(),
                before_hash: capture.hash.clone(),
                size: 0,
                mode: 0,
                mtime_ns: 0,
                recoverable: true,
                severity: flags::modification_severity(config, Path::new(path)),
                note,
            });
        }
    }

    let written = record(store, &drafts, active.as_ref())?;
    let flags = written
        .iter()
        .map(|flag| FlagView {
            severity: flag.severity.clone(),
            kind: flag.kind.clone(),
            path: flag.path.clone(),
        })
        .collect();
    Ok(CheckReport {
        captured: captures.iter().filter(|(_, c)| c.changed).count(),
        deleted,
        flags,
    })
}

/// The watcher's per-event path. Same capture and tombstone logic, no walk.
fn check_paths(
    store: &GlobalStore,
    config: &Config,
    paths: &[PathBuf],
) -> Result<Vec<global::Flag>> {
    let active = store.active_activity()?;
    let mut drafts: Vec<Draft> = Vec::new();

    for abs in paths {
        if !config.in_scope(abs, &store.dir) {
            continue;
        }
        let key = key_of(abs);
        let known = store.shadow(&key)?;
        if let Some(capture) = capture(store, config, known.as_ref(), abs)? {
            if capture.changed {
                if let Some(activity) = &active {
                    drafts.push(Draft {
                        kind: if capture.created {
                            "created"
                        } else {
                            "modified"
                        },
                        path: abs.to_string_lossy().into_owned(),
                        root: config
                            .relative_to(abs)
                            .map(|(root, _)| root)
                            .unwrap_or_default(),
                        before_hash: capture.hash.clone(),
                        size: 0,
                        mode: 0,
                        mtime_ns: 0,
                        recoverable: true,
                        severity: flags::modification_severity(config, abs),
                        note: if global::is_within(Path::new(&activity.cwd), abs) {
                            String::new()
                        } else {
                            "outside the agent's working directory".to_string()
                        },
                    });
                }
            }
            continue;
        }
        if !abs.exists() {
            if let Some(draft) = tombstone(store, config, abs, known.as_ref())? {
                drafts.push(draft);
            }
        }
    }

    record(store, &drafts, active.as_ref())
}

fn record(
    store: &GlobalStore,
    drafts: &[Draft],
    active: Option<&global::Activity>,
) -> Result<Vec<global::Flag>> {
    let mut flags = Vec::new();
    for draft in drafts {
        let (agent, session, confidence) = match active {
            Some(activity) => (
                activity.agent.clone(),
                activity.session.clone(),
                "window".to_string(),
            ),
            None => (String::new(), String::new(), "none".to_string()),
        };
        let flag = NewFlag {
            at: now_ms(),
            kind: draft.kind.to_string(),
            path: draft.path.clone(),
            root: draft.root.clone(),
            severity: draft.severity.as_str().to_string(),
            severity_rank: draft.severity.rank(),
            agent,
            session,
            confidence,
            before_hash: draft.before_hash.clone(),
            size: draft.size,
            mode: draft.mode,
            mtime_ns: draft.mtime_ns,
            recoverable: draft.recoverable,
            note: draft.note.clone(),
        };
        let id = store.add_flag(&flag)?;
        if let Some(stored) = store.flag(id)? {
            flags.push(stored);
        }
    }
    Ok(flags)
}

fn capture(
    store: &GlobalStore,
    config: &Config,
    known: Option<&ShadowRow>,
    abs: &Path,
) -> Result<Option<Capture>> {
    let meta = match fs::symlink_metadata(abs) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    if !meta.is_file() || is_reparse(abs) {
        return Ok(None);
    }
    let size = meta.len();
    if size > config.max_file {
        return Ok(None);
    }
    let mtime = mtime_ns(&meta);

    if let Some(previous) = known {
        if previous.size == size && previous.mtime_ns == mtime {
            return Ok(Some(Capture {
                hash: previous.hash.clone(),
                created: false,
                changed: false,
            }));
        }
    }

    let Ok(bytes) = fs::read(abs) else {
        return Ok(None);
    };
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let changed = known.map(|previous| previous.hash != hash).unwrap_or(true);
    if changed {
        write_blob(&store.blob_dir(), &hash, &bytes)?;
    }

    let Some((root, _)) = config.relative_to(abs) else {
        return Ok(None);
    };
    store.upsert_shadow(&ShadowRow {
        key: key_of(abs),
        path: abs.to_string_lossy().into_owned(),
        root,
        hash: hash.clone(),
        size,
        mode: file_mode(&meta),
        mtime_ns: mtime,
        captured_at: now_ms(),
    })?;

    Ok(Some(Capture {
        hash,
        created: known.is_none(),
        changed,
    }))
}

fn tombstone(
    store: &GlobalStore,
    config: &Config,
    abs: &Path,
    known: Option<&ShadowRow>,
) -> Result<Option<Draft>> {
    let Some((root, _)) = config.relative_to(abs) else {
        return Ok(None);
    };
    let path = abs.to_string_lossy().into_owned();

    let previous = match known {
        Some(row) => row.clone(),
        None => match store.shadow(&key_of(abs))? {
            Some(row) => row,
            None => {
                // The file was created and deleted between two passes. There
                // is nothing to restore, and saying so is more useful than
                // silently ignoring it.
                return Ok(Some(Draft {
                    kind: "deleted",
                    path,
                    root,
                    before_hash: String::new(),
                    size: 0,
                    mode: 0,
                    mtime_ns: 0,
                    recoverable: false,
                    severity: Severity::Critical,
                    note: "never captured; the content is gone".to_string(),
                }));
            }
        },
    };

    let recoverable = blob_exists(&store.blob_dir(), &previous.hash);
    store.remove_shadow(&previous.key)?;
    Ok(Some(Draft {
        kind: "deleted",
        path: previous.path.clone(),
        root: previous.root.clone(),
        before_hash: previous.hash.clone(),
        size: previous.size,
        mode: previous.mode,
        mtime_ns: previous.mtime_ns,
        recoverable,
        severity: flags::deletion_severity(config, Path::new(&previous.path), recoverable),
        note: String::new(),
    }))
}

fn walk_root<F>(config: &Config, root: &Path, store_dir: &Path, mut visit: F) -> Result<()>
where
    F: FnMut(&Path) -> Result<()>,
{
    if !root.is_dir() {
        eprintln!(
            "ctrlz: root is not a directory, skipping: {}",
            root.display()
        );
        return Ok(());
    }
    let mut builder = ignore::WalkBuilder::new(root);
    // `filter_entry` demands a 'static closure, so it owns its copies of the
    // small config values rather than borrowing the caller's stack frame.
    let owned_config = config.clone();
    let owned_root = root.to_path_buf();
    let owned_store = store_dir.to_path_buf();
    builder
        .standard_filters(false)
        .follow_links(false)
        .filter_entry(move |entry| keep_entry(&owned_config, &owned_root, &owned_store, entry));

    for item in builder.build() {
        let entry = match item {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_file() {
            continue;
        }
        visit(entry.path())?;
    }
    Ok(())
}

fn keep_entry(config: &Config, root: &Path, store_dir: &Path, entry: &ignore::DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let path = entry.path();
    if global::is_within(store_dir, path) {
        return false;
    }
    if global::is_never_shadow(&entry.file_name().to_string_lossy()) {
        return false;
    }
    if let Ok(relative) = path.strip_prefix(root) {
        let relative = rel_string(relative);
        if !relative.is_empty() && config.is_excluded_rel(&relative) {
            return false;
        }
    }
    let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
    !(is_dir && is_reparse(path))
}

// ---------------------------------------------------------------------------
// Watcher
// ---------------------------------------------------------------------------

pub fn watch(idle: Duration, exit_after: Option<usize>) -> Result<()> {
    let store = GlobalStore::open()?;
    let config = store.config()?;
    let lock = Lock::acquire(&store.dir)?;

    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .context("cannot start the filesystem watcher")?;
    for root in &config.roots {
        watcher
            .watch(Path::new(root), RecursiveMode::Recursive)
            .with_context(|| format!("cannot watch {root}"))?;
    }

    println!(
        "guarding {} root(s) for {}. log follows.",
        config.roots.len(),
        human_size(config.max_total)
    );
    for root in &config.roots {
        println!("  root  {root}");
    }

    let mut batch = Batch::new();
    let mut batches = 0usize;
    let mut last_full = Instant::now();
    let mut last_beat = now_ms();

    loop {
        // Never block for the full idle window: an idle watcher still has to
        // prove it is alive, or `guard start` will treat the lock as stale and
        // spawn a second watcher.
        let timeout = batch.timeout().min(Duration::from_secs(1));
        match receiver.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                for path in event.paths {
                    batch.push(path, idle, &config, &store.dir);
                }
            }
            Ok(Err(error)) => eprintln!("ctrlz: watch error: {error}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let now = now_ms();
        if now - last_beat >= HEARTBEAT_MS {
            last_beat = now;
            lock.heartbeat()?;
        }

        if batch.is_due() {
            let paths = batch.take();
            if !paths.is_empty() {
                match check_paths(&store, &config, &paths) {
                    Ok(flags) => {
                        for flag in flags {
                            println!("{} {} {}", flag.severity, flag.kind, flag.path);
                        }
                    }
                    Err(error) => eprintln!("ctrlz: {error:#}"),
                }
            }
            batches += 1;
            if exit_after.is_some_and(|limit| batches >= limit) {
                break;
            }
        }

        if last_full.elapsed() >= Duration::from_secs(config.rescan_secs.max(1)) {
            last_full = Instant::now();
            if let Err(error) = check_all(&store, &config) {
                eprintln!("ctrlz: periodic rescan failed: {error:#}");
            }
        }
    }

    Ok(())
}

/// Debounces a burst of filesystem events into one processing pass.
struct Batch {
    pending: BTreeSet<PathBuf>,
    deadline: Option<Instant>,
}

impl Batch {
    fn new() -> Self {
        Self {
            pending: BTreeSet::new(),
            deadline: None,
        }
    }

    fn push(&mut self, path: PathBuf, idle: Duration, config: &Config, store_dir: &Path) {
        if !config.in_scope(&path, store_dir) {
            return;
        }
        self.pending.insert(path);
        self.deadline = Some(Instant::now() + idle);
    }

    fn is_due(&self) -> bool {
        self.deadline
            .map(|deadline| Instant::now() >= deadline)
            .unwrap_or(false)
    }

    fn timeout(&self) -> Duration {
        match self.deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3_600),
        }
    }

    fn take(&mut self) -> Vec<PathBuf> {
        self.deadline = None;
        std::mem::take(&mut self.pending).into_iter().collect()
    }
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct LockFile {
    pid: u32,
    at: i64,
}

struct Lock {
    path: PathBuf,
}

impl Lock {
    fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join("guard.lock");
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    drop(file);
                    let lock = Self { path };
                    lock.heartbeat()?;
                    return Ok(lock);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some((pid, at)) = read_lock(&path) {
                        if now_ms() - at < LOCK_STALE_MS {
                            bail!(
                                "the guard is already running (pid {pid}). \
                                 use `ctrlz guard stop` first"
                            );
                        }
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot create {}", path.display()))
                }
            }
        }
        bail!("cannot acquire the guard lock at {}", path.display())
    }

    fn heartbeat(&self) -> Result<()> {
        let payload = LockFile {
            pid: std::process::id(),
            at: now_ms(),
        };
        fs::write(&self.path, serde_json::to_vec(&payload)?)
            .with_context(|| format!("cannot update {}", self.path.display()))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn read_lock(path: &Path) -> Option<(u32, i64)> {
    let text = fs::read_to_string(path).ok()?;
    let payload: LockFile = serde_json::from_str(&text).ok()?;
    Some((payload.pid, payload.at))
}

pub fn start() -> Result<()> {
    let store = GlobalStore::open()?;
    let lock_path = store.dir.join("guard.lock");
    if let Some((pid, at)) = read_lock(&lock_path) {
        if now_ms() - at < LOCK_STALE_MS {
            println!("guard already running (pid {pid})");
            return Ok(());
        }
        let _ = fs::remove_file(&lock_path);
    }

    let log_path = store.dir.join("guard.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;

    #[cfg(windows)]
    detach_stdio();

    let mut command =
        Command::new(std::env::current_exe().context("cannot find the ctrlz binary")?);
    command
        .arg("guard")
        .arg("watch")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = command.spawn().context("cannot start the guard")?;
    let pid = child.id();
    drop(child);

    let mut reported = false;
    for _ in 0..50 {
        if read_lock(&lock_path).is_some() {
            reported = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !reported {
        bail!("the guard did not report in; see {}", log_path.display());
    }

    println!("guard running (pid {pid})");
    println!("log {}", log_path.display());
    Ok(())
}

pub fn stop() -> Result<()> {
    let store = GlobalStore::open()?;
    let lock_path = store.dir.join("guard.lock");
    let Some((pid, _)) = read_lock(&lock_path) else {
        println!("guard is not running");
        return Ok(());
    };
    kill(pid)?;
    let _ = fs::remove_file(&lock_path);
    println!("stopped guard (pid {pid})");
    Ok(())
}

fn kill(pid: u32) -> Result<()> {
    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("cannot run taskkill")?;
        let _ = status;
    }
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("cannot run kill")?;
        let _ = status;
    }
    Ok(())
}

pub fn status() -> Result<()> {
    let store = GlobalStore::open()?;
    let config = store.config()?;
    let lock_path = store.dir.join("guard.lock");
    let running = read_lock(&lock_path)
        .filter(|(_, at)| now_ms() - at < LOCK_STALE_MS)
        .map(|(pid, _)| pid);

    match running {
        Some(pid) => println!("guard      running (pid {pid})"),
        None => println!("guard      stopped"),
    }
    println!("store      {}", store.dir.display());
    println!("roots      {}", config.roots.join(", "));
    println!("shadowed   {} files", store.shadow_count()?);
    let (critical, warn, total) = store.flag_counts()?;
    println!("flags      {total} total, {critical} critical, {warn} warn");
    println!(
        "blobs      {} of {} budget",
        human_size(usage(&store.dir)),
        human_size(config.max_total)
    );
    if let Some(activity) = store.active_activity()? {
        println!(
            "agent      {} (session {})",
            activity.agent,
            if activity.session.is_empty() {
                "-"
            } else {
                activity.session.as_str()
            }
        );
    }
    Ok(())
}

fn usage(dir: &Path) -> u64 {
    global::usage_of(dir).unwrap_or(0)
}

/// Resolve a path the same way the roots were resolved at `guard init`.
///
/// macOS reports `/var/...` but the canonical form is `/private/var/...`, so a
/// raw comparison against a configured root fails. A deleted file cannot be
/// canonicalised, so resolve its parent and keep the file name.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return clean(canonical);
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(canonical) = fs::canonicalize(parent) {
            return clean(canonical).join(name);
        }
    }
    path.to_path_buf()
}

/// A detached child inherits every inheritable handle, including the stdout
/// pipe a caller may have handed us. If the watcher keeps that handle open, a
/// piped caller waits for an EOF that never arrives. Clear the flag first.
#[cfg(windows)]
fn detach_stdio() {
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: both calls take and return plain handle values. Failure is
        // ignored: it only means the handle was already not inheritable.
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// Windows `canonicalize` returns a `\\?\` verbatim path, which is correct for
/// syscalls but hostile in a config file and incomparable with event paths.
fn clean(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
    }
    path
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
    fn a_batch_collects_a_burst_once() {
        let config = config();
        let store = Path::new("/store");
        let root = Path::new(&config.roots[0]);
        let mut batch = Batch::new();
        batch.push(
            root.join("a.txt"),
            Duration::from_millis(50),
            &config,
            store,
        );
        batch.push(
            root.join("b.txt"),
            Duration::from_millis(50),
            &config,
            store,
        );
        batch.push(
            root.join("a.txt"),
            Duration::from_millis(50),
            &config,
            store,
        );

        let paths = batch.take();
        assert_eq!(paths.len(), 2, "duplicates collapse");
        assert!(paths.iter().any(|path| path.ends_with("a.txt")));
        assert!(batch.take().is_empty(), "taking drains the batch");
    }

    #[test]
    fn out_of_scope_events_never_enter_the_batch() {
        let config = config();
        let mut batch = Batch::new();
        batch.push(
            PathBuf::from("/elsewhere/file.txt"),
            Duration::from_millis(10),
            &config,
            Path::new("/store"),
        );
        batch.push(
            Path::new(&config.roots[0]).join("node_modules/x.js"),
            Duration::from_millis(10),
            &config,
            Path::new("/store"),
        );
        assert!(batch.take().is_empty());
    }

    #[test]
    fn a_batch_is_due_when_its_idle_window_closes() {
        let config = config();
        let mut batch = Batch::new();
        assert!(!batch.is_due(), "an empty batch is never due");
        batch.push(
            Path::new(&config.roots[0]).join("a.txt"),
            Duration::from_millis(0),
            &config,
            Path::new("/store"),
        );
        assert!(batch.is_due());
    }

    #[test]
    fn the_batch_timeout_is_bounded() {
        let batch = Batch::new();
        assert_eq!(batch.timeout(), Duration::from_secs(3_600));
    }
}
