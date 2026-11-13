use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::Deserialize;

use crate::snapshot::{mark_at, MarkOptions, DEFAULT_MAX_FILE};
use crate::store::home;
use crate::util::{format_timestamp, now_ms};

#[derive(Debug, Deserialize)]
struct ClaudePayload {
    session_id: Option<String>,
    cwd: Option<String>,
    hook_event_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpencodePayload {
    #[serde(alias = "session", alias = "sessionID")]
    session_id: Option<String>,
    cwd: Option<String>,
    #[serde(alias = "type")]
    event: Option<String>,
}

pub fn run_claude(event_override: Option<&str>) {
    run_hook("claude", |input| {
        let payload: ClaudePayload = serde_json::from_str(input)?;
        let stop = event_override
            .map(|event| event.eq_ignore_ascii_case("stop"))
            .unwrap_or_else(|| {
                payload
                    .hook_event_name
                    .as_deref()
                    .map(|event| event.eq_ignore_ascii_case("stop"))
                    .unwrap_or(false)
            });
        let label = if stop { "stop" } else { "prompt" };
        let session = payload.session_id.unwrap_or_default();
        let root = root_from(payload.cwd.as_deref());
        Ok((root, label.to_string(), session))
    });
}

pub fn run_opencode() {
    run_hook("opencode", |input| {
        let payload: OpencodePayload = serde_json::from_str(input)?;
        let idle = payload
            .event
            .as_deref()
            .map(|event| event.to_ascii_lowercase().contains("idle"))
            .unwrap_or(false);
        let label = if idle { "stop" } else { "prompt" };
        let session = payload.session_id.unwrap_or_default();
        let root = root_from(payload.cwd.as_deref());
        Ok((root, label.to_string(), session))
    });
}

fn root_from(cwd: Option<&str>) -> PathBuf {
    cwd.map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn run_hook<F>(agent: &str, parse: F)
where
    F: FnOnce(&str) -> Result<(PathBuf, String, String), serde_json::Error>,
{
    let mut input = String::new();
    if let Err(error) = io::stdin().read_to_string(&mut input) {
        log_error(&format!("{agent}: cannot read stdin: {error}"));
        return;
    }

    let (root, label, session) = match parse(&input) {
        Ok(parsed) => parsed,
        Err(error) => {
            log_error(&format!("{agent}: invalid hook JSON: {error}"));
            return;
        }
    };

    let mut options = MarkOptions::new(agent, &label);
    options.session = &session;
    options.max_file = DEFAULT_MAX_FILE;
    if let Err(error) = mark_at(&root, &options) {
        log_error(&format!(
            "{agent}: snapshot failed for {}: {error:#}",
            root.display()
        ));
    }
}

fn log_error(message: &str) {
    // A hook must never fail the user's prompt because the log itself failed.
    let Ok(home) = home() else {
        return;
    };
    let dir = home.join("log");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("hooks.log");
    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => file,
        Err(_) => return,
    };
    // Best-effort logging: the hook's exit status matters more than the log.
    drop(writeln!(
        file,
        "{}  {}",
        format_timestamp(now_ms()),
        message.replace('\n', " ")
    ));
}
