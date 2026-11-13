use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::snapshot::{mark_at, MarkOptions, DEFAULT_MAX_FILE};
use crate::store::Store;
use crate::util::{format_ago, human_size, parse_duration, parse_size};

#[derive(Parser)]
#[command(
    name = "ctrlz",
    version,
    about = "Undo what a coding agent did to your working tree"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prepare the store and print the hook snippets
    Init {
        /// Merge the Claude Code hooks into .claude/settings.json
        #[arg(long)]
        claude: bool,
    },
    /// Snapshot the working tree and record a turn
    Mark {
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        session: Option<String>,
        /// Skip when the last snapshot is newer than this
        #[arg(long, value_parser = parse_duration)]
        if_stale: Option<Duration>,
        #[arg(long, default_value = "8M", value_parser = parse_size)]
        max_file: u64,
    },
    /// List recent turns
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { claude } => init(claude),
        Command::Mark {
            label,
            agent,
            session,
            if_stale,
            max_file,
        } => mark(label, agent, session, if_stale, max_file),
        Command::List { limit } => list(limit),
    }
}

fn init(claude: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let (store, created) = Store::init(&cwd)?;
    if created {
        println!("created store at {}", store.project_dir.display());
    } else {
        println!("store already exists at {}", store.project_dir.display());
    }
    println!("snapshots live outside the repo; nothing was written into it.");

    if claude {
        install_claude(&store.root)?;
    }

    println!();
    println!("Claude Code hooks (.claude/settings.json):");
    println!("{}", claude_snippet());
    println!();
    println!("opencode plugin (.opencode/plugin/ctrlz.ts, auto-loaded):");
    println!("{}", opencode_snippet());
    Ok(())
}

fn claude_snippet() -> String {
    serde_json::to_string_pretty(&json!({
        "hooks": {
            "UserPromptSubmit": [
                { "hooks": [ { "type": "command", "command": "ctrlz hook claude" } ] }
            ],
            "Stop": [
                { "hooks": [ { "type": "command", "command": "ctrlz hook claude --event stop" } ] }
            ]
        }
    }))
    .unwrap_or_default()
}

fn opencode_snippet() -> String {
    r#"import { Plugin } from "@opencode/plugin"
import { spawn } from "node:child_process"

export default Plugin.define({
  id: "ctrlz",
  async setup(ctx) {
    await ctx.tool.hook("execute.before", (event) => {
      const child = spawn("ctrlz", ["mark", "--agent", "opencode",
        "--session", event.sessionID, "--if-stale", "2s"], {
        stdio: "ignore", detached: true, windowsHide: true,
      })
      child.unref()
    })
  },
})"#
    .to_string()
}

fn install_claude(root: &Path) -> Result<()> {
    let dir = root.join(".claude");
    let path = dir.join("settings.json");
    let mut settings: Value = if path.exists() {
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON; not changing it", path.display()))?
    } else {
        json!({})
    };

    let object = settings
        .as_object_mut()
        .context(".claude/settings.json must contain a JSON object")?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks in .claude/settings.json must be a JSON object")?;

    add_hook(hooks, "UserPromptSubmit", "ctrlz hook claude");
    add_hook(hooks, "Stop", "ctrlz hook claude --event stop");

    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(&settings)?;
    text.push('\n');
    fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    println!("merged Claude Code hooks into {}", path.display());
    Ok(())
}

fn add_hook(hooks: &mut serde_json::Map<String, Value>, event: &str, command: &str) {
    let entries = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut();
    let Some(entries) = entries else {
        return;
    };
    let exists = entries.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .map(|hooks| {
                hooks
                    .iter()
                    .any(|hook| hook.get("command").and_then(Value::as_str) == Some(command))
            })
            .unwrap_or(false)
    });
    if !exists {
        entries.push(json!({
            "hooks": [ { "type": "command", "command": command } ]
        }));
    }
}

fn mark(
    label: Option<String>,
    agent: Option<String>,
    session: Option<String>,
    if_stale: Option<Duration>,
    max_file: u64,
) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let label = label.unwrap_or_else(|| "manual".into());
    let agent = agent.unwrap_or_else(|| "manual".into());
    let session = session.unwrap_or_default();

    let mut options = MarkOptions::new(&agent, &label);
    options.session = &session;
    options.if_stale = if_stale;
    options.max_file = if max_file == 0 {
        DEFAULT_MAX_FILE
    } else {
        max_file
    };

    match mark_at(&cwd, &options)? {
        Some(turn) => println!(
            "ctrlz: turn {} ({}), {} files, +{} ~{} -{}, {}",
            turn.id,
            turn.agent,
            turn.file_count,
            turn.added,
            turn.modified,
            turn.deleted,
            human_size(turn.bytes.max(0) as u64)
        ),
        None => println!("ctrlz: mark skipped, the last snapshot is recent"),
    }
    Ok(())
}

fn list(limit: usize) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let store = Store::open(&cwd)?;
    let turns = store.turns_desc(limit)?;
    if turns.is_empty() {
        println!("no turns yet. run `ctrlz mark` or install a hook.");
        return Ok(());
    }

    println!(
        "{:>4}  {:<10}  {:<10}  {:<6} {:<6} {:<6}  {:>9}  {}",
        "turn", "time", "agent", "+add", "~mod", "-del", "on disk", "label"
    );
    for turn in turns {
        println!(
            "{:>4}  {:<10}  {:<10}  +{:<5} ~{:<5} -{:<5}  {:>9}  {}",
            turn.id,
            format_ago(turn.started_at),
            turn.agent,
            turn.added,
            turn.modified,
            turn.deleted,
            human_size(turn.bytes.max(0) as u64),
            turn.label
        );
    }
    Ok(())
}
