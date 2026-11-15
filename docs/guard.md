# The global guard

`ctrlz guard` is the machine-wide counterpart to the per-project store. The
project store answers "put this repository back the way it was before the last
turn". The guard answers a narrower, more urgent question: "an agent just
deleted a file outside the repo, or between turns — do you still have it?"

## Why a baseline, not just a watcher

A filesystem watcher reports that a path changed or vanished. It does not
report the bytes. If the guard only listened for delete events, the content
would already be gone by the time it reacted.

So the guard runs in two halves:

1. `guard scan` walks the configured roots, hashes every in-scope file, and
   writes the content into the blob store. This is the baseline.
2. `guard watch` keeps that baseline current. Every create or write event
   re-captures the file, so the bytes are in the store before anything can
   delete them.

A delete then becomes a tombstone: the shadow row is removed, the last known
hash is checked against the blob store, and a flag is written.

If a file was never captured, the guard says so. It records `recoverable:
false` and `critical` severity rather than pretending the deletion is undoable.
That case only happens when a file was created and deleted between two passes,
or when the guard was not running.

## Commands

```sh
ctrlz guard init --root ~/code [--root ~/notes] [--exclude '*.log'] [--protect '*.sql']
ctrlz guard scan                 # baseline
ctrlz guard check                # one pass over the roots
ctrlz guard check --path <file>  # one pass over explicit paths
ctrlz guard watch [--idle 2s]    # foreground
ctrlz guard start | stop | status
ctrlz guard activity --agent <name> [--session <id>] | --clear
ctrlz flags [--since 24h] [--min-severity warn] [--json]
ctrlz flags <id>
ctrlz recover [<id>] [--since 1h] [--dry-run]
ctrlz forget [--keep 30d]
```

`guard check --path` is the same code path the watcher runs per event. It exists
so the watcher's behaviour can be driven and tested without waiting on the OS
to deliver an event.

## What is watched

Only the roots you name. There is no default root, and `guard init` refuses to
run without `--root`, because defaulting to a home directory would quietly copy
SSH keys, cloud credentials, and browser profiles into one store.

Never shadowed, even inside a named root:

- `.git`, `node_modules`, `target`, `dist`, `build`, `.next`, `.venv`, `venv`,
  `__pycache__`, `.ctrlz`
- `AppData`, `.cache`, `.cargo`, `.rustup`
- `Library/Caches/**`, `Library/Logs/**`, `.npm/_cacache/**`,
  `.local/share/Trash/**`
- the guard's own store

`.gitignore` is deliberately **not** applied. The project store honours it; the
guard does not, because `.env` is usually ignored and is exactly the file you
want back.

Symlinks and Windows reparse points are skipped, never followed, matching the
project store's rule.

## Flag rules

| severity | when |
| --- | --- |
| `critical` | a deletion the guard could not capture (`recoverable: false`) |
| `critical` | deleting or changing a protected file |
| `critical` | more than three times `mass_delete` files removed in one pass |
| `warn` | any other deletion |
| `info` | a rename, or an edit made while an agent marker was active |

Protected patterns by default: `.env`, `.env.*`, `*.pem`, `*.key`, `*.p12`,
`*.pfx`, `id_rsa`, `id_ed25519`, `id_ecdsa`, `.npmrc`, `.pypirc`, `.netrc`,
`.ssh/**`, `.aws/**`, `.gnupg/**`, `.git/config`. `--protect` adds to this list;
it cannot remove from it.

Deletions are always flagged. Ordinary edits are only flagged while an agent
marker is active, which keeps the ledger about agents rather than about every
save your editor makes.

## Attribution, honestly

Windows and macOS filesystem events do not carry the writing process, and Linux
only does so through `fanotify` with elevated privileges. The guard therefore
does not claim exact attribution.

Instead, the Claude Code and opencode hooks record an activity marker
(`agent`, `session`, `cwd`) when a turn starts and clear it when the turn ends.
A change that lands inside that window is labelled with that agent and
`confidence: window`. A change outside any window is labelled `unattributed`.

`confidence: window` means "this agent was working at the time", not "this agent
did it". Running two agents at once will blur the label. For agents without
hooks, wrap the turn:

```sh
ctrlz guard activity --agent codex --session "$RUN_ID"
codex ...
ctrlz guard activity --clear
```

## Restoring

`ctrlz recover` reuses the project store's restore engine, so the same safety
properties hold:

- paths are validated and must stay inside a configured root
- content is written to a temporary file, then renamed into place
- symlinks and reparse points are never written through
- directories are never deleted
- mode bits and mtimes are restored

`recover --since 1h` groups by path and restores the newest captured content per
file, oldest flag first. `--dry-run` prints the plan without writing.

After a successful restore the shadow row is updated, so a later `guard check`
does not report the restore itself as a change.

## Storage and retention

The guard store lives beside the project stores:

- Windows: `%LOCALAPPDATA%\ctrlz\global`
- macOS: `~/Library/Application Support/ctrlz/global`
- Linux: `$XDG_DATA_HOME/ctrlz/global` (or `~/.local/share/ctrlz/global`)

Blobs are content-addressed and deduplicated, so a file that does not change
costs one blob no matter how often it is captured. `--max-file` (default 8 MB)
caps what is captured at all; larger files are skipped, not truncated.

`ctrlz forget --keep 30d` deletes old flags and then removes blobs that neither
the shadow index nor a surviving flag references. Nothing is evicted
automatically: silently dropping the one blob you needed would defeat the point
of a guard.

## Lifecycle

There is no login persistence. No launchd plist, no systemd user unit, no
Windows scheduled task. `guard start` spawns a detached `guard watch` and writes
a heartbeat lock at `global/guard.lock`; `guard stop` kills it and removes the
lock. A lock whose heartbeat is older than 30 seconds is treated as stale and
reclaimed, so a crash does not wedge the next start.

If you want it to survive a reboot, add it to your own startup mechanism. It is
not installed for you.

## Limits

- **It cannot protect itself.** The guard is a normal process running as you.
  An agent that deletes `$CTRLZ_HOME` deletes the baseline too. Keep the store
  outside the directories you guard, and do not hand an agent a shell with the
  store in scope.
- **It is not a backup.** It covers files under named roots, up to `--max-file`,
  while it is running. It does not cover permissions you never captured, and it
  does not help with a disk failure.
- **A stopped guard misses events.** The periodic rescan (`rescan_secs`,
  default 600) recovers from missed events while running, not from downtime.
  Run `guard scan` again after a long pause.
- **Windows buffer overflow.** Under heavy churn `ReadDirectoryChangesW` can
  drop events. The periodic rescan is the backstop.
