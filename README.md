# ctrlz

A local shadow store for your working tree. `ctrlz` snapshots a repository
before each agent turn, and `ctrlz undo` puts the tree back exactly as it was:
tracked files, untracked files, ignored-but-real files, everything git does not
cover.

Git protects commits. ctrlz protects the thirty seconds between them.

## Build

```sh
cargo build --release
# put target/release/ctrlz on PATH, or:
cargo install --path .
```

There is no network and no account. The per-project store is a directory of zstd
blobs and a SQLite index outside the repository, and it needs no daemon. The
optional global guard runs one only when you start it.

## Store location

`$CTRLZ_HOME` if set. Otherwise:

- Windows: `%LOCALAPPDATA%\ctrlz`
- macOS: `~/Library/Application Support/ctrlz`
- Linux: `$XDG_DATA_HOME/ctrlz` (or `~/.local/share/ctrlz`)

Each project gets `projects/<blake3(root)[..16]>/` containing `project.json`,
`index.sqlite` in WAL mode, and `blobs/<aa>/<hash>`.

## Quick start

```sh
cd my-project
ctrlz init
ctrlz mark --label before-agent
# let the agent work
ctrlz undo
```

`ctrlz init --claude` also merges the Claude Code hooks into
`.claude/settings.json`. The opencode plugin is
[`.opencode/plugin/ctrlz.ts`](.opencode/plugin/ctrlz.ts); copy that file and
`ctrlz-handler.mjs` beside it into a project, or run `ctrlz watch` instead of
installing hooks.

## Commands

```
ctrlz init [--claude]          create the store, print the hook snippets
ctrlz mark [--label t]         snapshot the tree and record a turn
ctrlz list [--limit 20]        recent turns, change counts, and disk use
ctrlz show <n>                 file-level changes in a turn
ctrlz diff <n>                 unified diff for text files, "binary" otherwise
ctrlz undo [n]                 restore a turn (default: before the last agent turn)
ctrlz back <n>                 restore a specific turn
ctrlz prune --keep 50          delete old turns and unreferenced blobs
ctrlz watch [--idle 3s]        snapshot after change bursts, no hooks needed
ctrlz doctor                   check the store, hooks, write access, clock skew
ctrlz hook claude              Claude Code hook entry point (stdin JSON)
ctrlz hook opencode            opencode hook entry point (stdin JSON)
ctrlz guard init --root <p>    register directories to guard (starts nothing)
ctrlz guard scan               capture the baseline that makes recovery possible
ctrlz guard check              compare against the baseline and raise flags
ctrlz guard watch|start|stop   run the watcher, foreground or detached
ctrlz guard status             roots, shadowed files, flags, disk use
ctrlz flags [--json]           list or inspect flagged changes
ctrlz recover [--since 1h]     restore captured files
ctrlz forget --keep 30d        drop old flags and reclaim unreferenced blobs
```

`mark` is built for a prompt hook: it stats the tree, hashes only files whose
size or mtime changed, and writes a blob only when the content hash is new.
Identical consecutive snapshots collapse into the latest turn, which keeps a
no-change mark cheap. Files over `--max-file` (default 8 MB) are recorded as
skipped and shown by `ctrlz show`; they are never deleted by a restore.

`undo` takes a `pre-restore` snapshot first, so the undo itself is undoable.
Running `undo` again restores the pre-restore snapshot. `back <n>` always
restores exactly turn `n` and is idempotent.

## Global guard

`ctrlz guard` watches directories you name and keeps a shadow index of every
file in them, so a deletion is recoverable even when it happens outside a
repository or between agent turns.

```sh
ctrlz guard init --root ~/code --root ~/notes
ctrlz guard scan          # capture the baseline
ctrlz guard watch         # foreground; or `ctrlz guard start` to detach
ctrlz guard status
```

The baseline is the whole point. A filesystem watcher learns that a path
vanished, not what was in it, so `scan` puts the bytes in the store first and
the watcher keeps them current on every write. A file that was never captured is
reported as `critical` with `recoverable: false` instead of being silently
ignored.

```sh
ctrlz flags                 # what changed, newest first
ctrlz flags 12              # one flag in detail
ctrlz recover               # restore the most recent deletion
ctrlz recover 12            # restore one flag
ctrlz recover --since 1h    # restore every deletion in the window
```

Deletions are always recorded. Ordinary edits are recorded only while an agent
marker is active, which the Claude Code and opencode hooks write automatically.
For any other agent, call `ctrlz guard activity --agent <name> --session <id>`
at the start of a turn and `ctrlz guard activity --clear` at the end. Deleting
or changing a protected file (`.env`, `*.pem`, `id_*`, `.ssh/**`) is `critical`.

The guard never starts on login. There is no launchd plist, no systemd unit, and
no scheduled task: `ctrlz guard start` runs a detached process and
`ctrlz guard stop` ends it. See [docs/guard.md](docs/guard.md) for the rule
table and the threat model.

## Hooks

Claude Code, in `.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "ctrlz hook claude" } ] }
    ],
    "Stop": [
      { "hooks": [ { "type": "command", "command": "ctrlz hook claude --event stop" } ] }
    ]
  }
}
```

opencode 2.2.6 uses the V2 plugin API. The shipped plugin registers
`ctx.tool.hook("execute.before", ...)`, marks before the first tool call of a
turn (deduped by call ID), and resets on `session.idle`. The exact event names
and payload shapes verified against the installed versions are in
[`docs/hooks.md`](docs/hooks.md).

Both hook paths exit 0 even when the store is unavailable. Errors are appended
to `$CTRLZ_HOME/log/hooks.log`; a failed hook must not block the user's prompt.

## Scope and safety

Included by default: everything under the canonical project root, honouring
`.gitignore` and `.ctrlzignore`, minus `.git`, `node_modules`, `target`, `dist`,
`build`, `.next`, `.venv`, `venv`, `__pycache__`, and `.ctrlz`.

- Symlinks and reparse points are skipped, never followed.
- Restore writes through a temporary file and refuses to write through a link.
- Restore never deletes directories, only files the target turn did not have.
- Paths are stored as `/`-separated relative strings and validated before use.
- On Windows, case collisions abort before any write, with both paths listed.
- Mode bits and mtimes are restored so incremental builds stay incremental.
- Long paths use the `\\?\` prefix internally on Windows.

## Demo

```sh
mkdir ctrlz-demo && cd ctrlz-demo
cat > check.js <<'EOF'
const { add } = require("./math")
if (add(2, 3) !== 5) process.exit(1)
console.log("ok")
EOF
cat > math.js <<'EOF'
exports.add = (a, b) => a + b
EOF
node check.js
ctrlz init --claude
# In Claude Code, ask: "make add return the product instead"
node check.js        # fails
ctrlz undo
node check.js        # ok again
```

## Tests

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
node --test tests/opencode/*.test.mjs
```

The integration tests run the real binary against real temporary repositories.
The ignored perf test covers 10,000 files:

```sh
cargo test --release --test perf -- --ignored --nocapture
```

CI runs the same commands on Windows, macOS, and Linux.

## License

MIT. See [LICENSE](LICENSE).
