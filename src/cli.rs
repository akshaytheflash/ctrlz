use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::diff::{detect_renames, diff_manifests, unified, Kind};
use crate::manifest::manifest_of;
use crate::prune::{prune as prune_turns, summary as prune_summary};
use crate::restore::{apply_plan, plan_restore};
use crate::snapshot::{mark_at, MarkOptions, DEFAULT_MAX_FILE};
use crate::store::{read_blob, Store};
use crate::util::{format_ago, format_timestamp, human_size, parse_duration, parse_size};

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
    /// Show the file-level changes of a turn
    Show { n: i64 },
    /// Print a unified diff for a turn
    Diff { n: i64 },
    /// Restore a turn (default: the snapshot before the last agent turn)
    Undo { n: Option<i64> },
    /// Restore a specific turn
    Back { n: i64 },
    /// Receive a coding-agent hook payload on stdin
    Hook {
        #[command(subcommand)]
        kind: HookKind,
    },
    /// Delete old turns and unreferenced blobs
    Prune {
        #[arg(long, default_value_t = 50)]
        keep: usize,
        #[arg(long)]
        max_size: Option<String>,
    },
    /// Snapshot after bursts of file changes
    Watch {
        #[arg(long, default_value = "3s", value_parser = parse_duration)]
        idle: Duration,
        #[arg(long, default_value = "8M", value_parser = parse_size)]
        max_file: u64,
    },
    /// Check the store, the hooks, write permissions, and clock skew
    Doctor,
}

#[derive(Subcommand)]
enum HookKind {
    /// Claude Code hook payload
    Claude {
        #[arg(long)]
        event: Option<String>,
    },
    /// opencode hook payload
    Opencode,
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
        Command::Show { n } => show(n),
        Command::Diff { n } => diff(n),
        Command::Undo { n } => undo(n),
        Command::Back { n } => undo(Some(n)),
        Command::Hook { kind } => {
            match kind {
                HookKind::Claude { event } => crate::hooks::run_claude(event.as_deref()),
                HookKind::Opencode => crate::hooks::run_opencode(),
            };
            Ok(())
        }
        Command::Prune { keep, max_size } => {
            let max_size = match max_size {
                Some(text) => Some(parse_size(&text).map_err(|error| anyhow!(error))?),
                None => None,
            };
            prune(keep, max_size)
        }
        Command::Watch { idle, max_file } => crate::watch::watch(idle, max_file),
        Command::Doctor => doctor(),
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
        "--session", event.sessionID, "--label", "prompt",
        "--if-stale", "2s"], {
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

    add_hook(hooks, "UserPromptSubmit", "ctrlz hook claude")?;
    add_hook(hooks, "Stop", "ctrlz hook claude --event stop")?;

    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(&settings)?;
    text.push('\n');
    fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    println!("merged Claude Code hooks into {}", path.display());
    Ok(())
}

fn add_hook(hooks: &mut serde_json::Map<String, Value>, event: &str, command: &str) -> Result<()> {
    let entries = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut();
    let Some(entries) = entries else {
        return Err(anyhow!(
            "hooks.{event} in .claude/settings.json must be an array"
        ));
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
    Ok(())
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
        "{:>4}  {:<10}  {:<10}  {:<6} {:<6} {:<6}  {:>9}  label",
        "turn", "time", "agent", "+add", "~mod", "-del", "on disk"
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

fn show(n: i64) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let store = Store::open(&cwd)?;
    let turn = store
        .turn(n)?
        .ok_or_else(|| anyhow!("turn {n} not found"))?;
    let previous = store.previous_turn_id(n)?;
    let before = store.manifest_of_turn(previous)?;
    let after = store.manifest_of_turn(Some(n))?;
    let changes = detect_renames(diff_manifests(&before, &after));

    println!(
        "turn {}  {}  {}  {}",
        turn.id,
        turn.agent,
        format_timestamp(turn.started_at),
        format_ago(turn.started_at)
    );
    if !turn.session.is_empty() {
        println!("session {}", turn.session);
    }
    let label = if turn.label.is_empty() {
        "-"
    } else {
        turn.label.as_str()
    };
    println!(
        "label {}  files {}  +{} ~{} -{}  {} on disk",
        label,
        turn.file_count,
        turn.added,
        turn.modified,
        turn.deleted,
        human_size(turn.bytes.max(0) as u64)
    );

    for change in &changes {
        match &change.kind {
            Kind::Added => println!(
                "+ {}  {}",
                change.path,
                human_size(change.new.as_ref().map(|e| e.size).unwrap_or(0))
            ),
            Kind::Modified => println!("~ {}", change.path),
            Kind::Deleted => println!("- {}", change.path),
            Kind::Renamed { from } => println!("R {} -> {}", from, change.path),
        }
    }
    for skipped in store.skipped_summary(n)? {
        println!("! skipped {skipped}");
    }
    if changes.is_empty() {
        println!("no file changes");
    }
    Ok(())
}

fn diff(n: i64) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let store = Store::open(&cwd)?;
    if store.turn(n)?.is_none() {
        return Err(anyhow!("turn {n} not found"));
    }
    let previous = store.previous_turn_id(n)?;
    let before = store.manifest_of_turn(previous)?;
    let after = store.manifest_of_turn(Some(n))?;
    let changes = detect_renames(diff_manifests(&before, &after));
    if changes.is_empty() {
        println!("no changes in turn {n}");
        return Ok(());
    }

    for change in changes {
        if let Kind::Renamed { from } = &change.kind {
            println!("rename {from} -> {}", change.path);
            continue;
        }
        let old = match change.old.as_ref() {
            Some(entry) => Some(load_blob(&store, &entry.hash)?),
            None => None,
        };
        let new = match change.new.as_ref() {
            Some(entry) => Some(load_blob(&store, &entry.hash)?),
            None => None,
        };
        print!("{}", unified(old.as_deref(), new.as_deref(), &change.path));
    }
    Ok(())
}

fn load_blob(store: &Store, hash: &str) -> Result<Vec<u8>> {
    read_blob(&store.blob_dir(), hash)
        .with_context(|| format!("cannot read blob {hash}; the store may be incomplete"))
}

fn undo(n: Option<i64>) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let mut store = Store::open(&cwd)?;
    let last = store
        .last_turn()?
        .ok_or_else(|| anyhow!("no turns yet. run `ctrlz mark` first"))?;

    let target_id = match n {
        Some(id) => id,
        None if last.label == "stop" => store.previous_turn_id(last.id)?.unwrap_or(last.id),
        None => last.id,
    };
    if target_id == 0 {
        return Err(anyhow!(
            "there is no snapshot before turn {}; run `ctrlz back 1` to restore the first snapshot",
            last.id
        ));
    }
    let target = store
        .turn(target_id)?
        .ok_or_else(|| anyhow!("turn {target_id} not found"))?;
    let target_manifest = store.manifest_of_turn(Some(target_id))?;
    let cache = store.cached_entries(Some(last.id))?;
    let current = manifest_of(&store.root, &store.blob_dir(), DEFAULT_MAX_FILE, &cache)?;
    let plan = plan_restore(&current, &target_manifest, cfg!(windows))?;

    if plan.deletes.is_empty() && plan.writes.is_empty() {
        println!(
            "turn {} already matches the working tree; nothing to do",
            target.id
        );
        return Ok(());
    }

    let snapshot = store.record_turn(&current, "ctrlz", "", "pre-restore")?;
    println!("ctrlz: pre-restore snapshot is turn {}", snapshot.id);

    let report = apply_plan(&store.root, &store.blob_dir(), &plan)?;
    println!(
        "restored turn {}: {} written, {} deleted, {} unchanged",
        target.id, report.written, report.deleted, report.unchanged
    );
    for skipped in report.skipped {
        println!("skipped {skipped}");
    }
    Ok(())
}

fn prune(keep: usize, max_size: Option<u64>) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let store = Store::open(&cwd)?;
    let report = prune_turns(&store, keep, max_size)?;
    println!("{}", prune_summary(&report));
    Ok(())
}

fn doctor() -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let report = crate::doctor::run(&cwd);
    report.print();
    if report.failed() {
        return Err(anyhow!("doctor found {} problem(s)", report.failures()));
    }
    Ok(())
}
