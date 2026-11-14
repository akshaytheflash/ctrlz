#[cfg(windows)]
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::prune::dir_size;
use crate::store::{blob_exists, Store};
use crate::util::{format_timestamp, long_path, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    fn push(&mut self, name: &'static str, status: Status, detail: impl Into<String>) {
        self.checks.push(Check {
            name,
            status,
            detail: detail.into(),
        });
    }

    pub fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .count()
    }

    pub fn warnings(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == Status::Warn)
            .count()
    }

    pub fn failed(&self) -> bool {
        self.failures() > 0
    }

    pub fn print(&self) {
        for check in &self.checks {
            let label = match check.status {
                Status::Ok => "ok",
                Status::Warn => "warn",
                Status::Fail => "FAIL",
            };
            println!("{label:<5} {:<10} {}", check.name, check.detail);
        }
        match (self.failures(), self.warnings()) {
            (0, 0) => println!("doctor: ok"),
            (0, warnings) => println!("doctor: {warnings} warning(s)"),
            (failures, warnings) => {
                println!("doctor: {failures} failure(s), {warnings} warning(s)")
            }
        }
    }
}

pub fn run(root: &Path) -> Report {
    let mut report = Report { checks: Vec::new() };
    let store = match Store::open(root) {
        Ok(store) => store,
        Err(error) => {
            report.push("store", Status::Fail, error.to_string());
            return report;
        }
    };

    report.push("store", Status::Ok, store.project_dir.display().to_string());

    match sqlite_integrity(&store) {
        Ok(true) => report.push("index", Status::Ok, "sqlite integrity check passed"),
        Ok(false) => report.push("index", Status::Fail, "sqlite integrity check failed"),
        Err(error) => report.push("index", Status::Fail, error),
    }

    match writable(&store.project_dir) {
        Ok(()) => report.push("store-rw", Status::Ok, "store is writable"),
        Err(error) => report.push("store-rw", Status::Fail, error),
    }
    match writable(&store.root) {
        Ok(()) => report.push("root-rw", Status::Ok, "project root is writable"),
        Err(error) => report.push("root-rw", Status::Fail, error),
    }

    match store.last_turn() {
        Ok(Some(turn)) => {
            report.push(
                "turn",
                Status::Ok,
                format!(
                    "last turn {} by {} at {}",
                    turn.id,
                    turn.agent,
                    format_timestamp(turn.started_at)
                ),
            );
            if turn.started_at > now_ms() + 120_000 {
                report.push(
                    "clock",
                    Status::Fail,
                    "turn timestamps are more than two minutes in the future",
                );
            }
            check_blobs(&store, &mut report);
            check_clock(&store, turn.id, &mut report);
            check_case_collisions(&store, turn.id, &mut report);
        }
        Ok(None) => report.push("turn", Status::Warn, "no turns yet; run `ctrlz mark`"),
        Err(error) => report.push("turn", Status::Fail, error.to_string()),
    }

    check_hooks(&store.root, &mut report);

    match dir_size(&store.project_dir) {
        Ok(bytes) => report.push("size", Status::Ok, crate::util::human_size(bytes)),
        Err(error) => report.push("size", Status::Warn, error.to_string()),
    }

    report
}

fn sqlite_integrity(store: &Store) -> Result<bool, String> {
    let result: String = store
        .connection()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    Ok(result == "ok")
}

fn writable(dir: &Path) -> Result<(), String> {
    let path = dir.join(format!(".ctrlz-doctor-{}-{}", std::process::id(), now_ms()));
    fs::write(long_path(&path), b"ok")
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    fs::remove_file(long_path(&path))
        .map_err(|error| format!("cannot remove {}: {error}", path.display()))
}

fn check_blobs(store: &Store, report: &mut Report) {
    let mut statement = match store
        .connection()
        .prepare("SELECT DISTINCT hash FROM files WHERE state = 'present'")
    {
        Ok(statement) => statement,
        Err(error) => {
            report.push("blobs", Status::Fail, error.to_string());
            return;
        }
    };
    let rows = match statement.query_map([], |row| row.get::<_, String>(0)) {
        Ok(rows) => rows,
        Err(error) => {
            report.push("blobs", Status::Fail, error.to_string());
            return;
        }
    };

    let mut count = 0;
    let mut missing = 0;
    let mut first = String::new();
    for row in rows {
        let Ok(hash) = row else {
            report.push("blobs", Status::Fail, "cannot read the blob index");
            return;
        };
        count += 1;
        if !blob_exists(&store.blob_dir(), &hash) {
            missing += 1;
            if first.is_empty() {
                first = hash;
            }
        }
    }
    if missing == 0 {
        report.push(
            "blobs",
            Status::Ok,
            format!("{count} referenced blobs present"),
        );
    } else {
        report.push(
            "blobs",
            Status::Fail,
            format!("{missing} missing blobs, first: {first}"),
        );
    }
}

fn check_clock(store: &Store, turn_id: i64, report: &mut Report) {
    let Ok(manifest) = store.manifest_of_turn(Some(turn_id)) else {
        return;
    };
    let future_ns = (now_ms() + 86_400_000) * 1_000_000;
    let ahead = manifest
        .entries
        .values()
        .filter(|entry| entry.mtime_ns > future_ns)
        .count();
    if ahead > 0 {
        report.push(
            "mtime",
            Status::Warn,
            format!("{ahead} file mtimes are more than a day in the future"),
        );
    }
}

#[cfg(windows)]
fn check_case_collisions(store: &Store, turn_id: i64, report: &mut Report) {
    let Ok(manifest) = store.manifest_of_turn(Some(turn_id)) else {
        return;
    };
    let mut seen: HashMap<String, String> = HashMap::new();
    for path in manifest.entries.keys() {
        let key = path.to_lowercase();
        if let Some(first) = seen.get(&key) {
            if first != path {
                report.push(
                    "case",
                    Status::Fail,
                    format!("paths collide on case: {first}, {path}"),
                );
                return;
            }
        } else {
            seen.insert(key, path.clone());
        }
    }
    report.push("case", Status::Ok, "no case collisions");
}

#[cfg(not(windows))]
fn check_case_collisions(_store: &Store, _turn_id: i64, _report: &mut Report) {}

fn check_hooks(root: &Path, report: &mut Report) {
    let claude = root.join(".claude").join("settings.json");
    match fs::read_to_string(&claude) {
        Ok(text) => {
            let configured = serde_json::from_str::<Value>(&text)
                .map(|value| contains(&value, "ctrlz hook claude"))
                .unwrap_or(false);
            if configured {
                report.push("claude", Status::Ok, "Claude Code hooks installed");
            } else {
                report.push(
                    "claude",
                    Status::Warn,
                    "no ctrlz hook in .claude/settings.json; run `ctrlz init --claude`",
                );
            }
        }
        Err(_) => report.push(
            "claude",
            Status::Warn,
            "no .claude/settings.json; run `ctrlz init --claude`",
        ),
    }

    let plugin = root.join(".opencode").join("plugin").join("ctrlz.ts");
    let plugin_alt = root.join(".opencode").join("plugins").join("ctrlz.ts");
    if plugin.exists() || plugin_alt.exists() {
        report.push("opencode", Status::Ok, "opencode plugin present");
    } else {
        report.push(
            "opencode",
            Status::Warn,
            "no .opencode/plugin/ctrlz.ts; copy the plugin or use `ctrlz watch`",
        );
    }
}

fn contains(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text.contains(needle),
        Value::Array(values) => values.iter().any(|value| contains(value, needle)),
        Value::Object(values) => values.values().any(|value| contains(value, needle)),
        _ => false,
    }
}
