use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};

use crate::ignore::is_excluded_rel;
use crate::snapshot::{mark_at, MarkOptions};
use crate::store::Store;
use crate::util::{display_path, human_size, rel_string};

pub fn watch(idle: Duration, max_file: u64) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let store = Store::open(&cwd)?;
    let root = store.root.clone();
    drop(store);

    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        if sender.send(event).is_err() {
            // The receiver is gone; the watcher is about to be dropped.
        }
    })
    .context("cannot start the filesystem watcher")?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .with_context(|| format!("cannot watch {}", root.display()))?;

    println!(
        "watching {} (idle {} ms)",
        display_path(&root),
        idle.as_millis()
    );
    let mut pending = false;
    let mut last_event = Instant::now();

    loop {
        let timeout = if pending {
            idle.checked_sub(last_event.elapsed())
                .unwrap_or(Duration::from_millis(0))
        } else {
            Duration::from_secs(3_600)
        };

        match receiver.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                if event.paths.is_empty() || event.paths.iter().any(|path| relevant(&root, path)) {
                    pending = true;
                    last_event = Instant::now();
                }
            }
            Ok(Err(error)) => eprintln!("ctrlz: watch error: {error}"),
            Err(RecvTimeoutError::Timeout) => {
                if !pending {
                    continue;
                }
                pending = false;
                let options = MarkOptions {
                    agent: "watch",
                    session: "",
                    label: "watch",
                    if_stale: None,
                    max_file: max_file.max(1),
                };
                match mark_at(&root, &options) {
                    Ok(Some(turn)) => println!(
                        "turn {}: {} files, {}",
                        turn.id,
                        turn.file_count,
                        human_size(turn.bytes.max(0) as u64)
                    ),
                    Ok(None) => {}
                    Err(error) => eprintln!("ctrlz: snapshot failed: {error:#}"),
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn relevant(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let relative = rel_string(relative);
    !relative.is_empty() && !is_excluded_rel(&relative)
}
