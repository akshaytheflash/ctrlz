# Hook notes

These are the event names and payload shapes verified on this machine on
2026-11-13. The Rust side only reads the fields listed here; extra fields are
ignored.

## Versions

- Claude Code `2.1.310` (`claude --version`)
- opencode `2.2.6` (`opencode --version`)
- `@opencode/plugin` `2.2.6`
- `@opencode/schema` `2.2.6`

## Claude Code

Claude Code runs command hooks with the event JSON on stdin. The common fields
are `session_id`, `transcript_path`, `cwd`, `permission_mode`, and
`hook_event_name`. The two events ctrlz uses are:

`UserPromptSubmit` fires before Claude processes a prompt:

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/me/.claude/projects/.../abc123.jsonl",
  "cwd": "/Users/me/project",
  "permission_mode": "default",
  "hook_event_name": "UserPromptSubmit",
  "prompt": "fix the failing test"
}
```

`Stop` fires when the main agent finishes responding:

```json
{
  "session_id": "abc123",
  "transcript_path": "/Users/me/.claude/projects/.../abc123.jsonl",
  "cwd": "/Users/me/project",
  "permission_mode": "default",
  "hook_event_name": "Stop",
  "stop_hook_active": false,
  "last_assistant_message": "I updated the parser.",
  "background_tasks": [],
  "session_crons": []
}
```

The project settings are `.claude/settings.json`:

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

`ctrlz init --claude` merges these entries without replacing other hooks. Both
handlers exit 0 even when the store cannot be opened; failures go to
`$CTRLZ_HOME/log/hooks.log` because a non-zero `UserPromptSubmit` hook can block
the prompt.

## opencode

opencode 2.2.6 uses the V2 plugin API. A plugin default-exports
`Plugin.define({ id, setup(ctx) })`; the V1 function-with-hooks shape from older
releases is not executed by V2. V2 discovers local plugins under both
`.opencode/plugin/` and `.opencode/plugins/`, so this repo keeps the file at
`.opencode/plugin/ctrlz.ts` as requested.

The tool hook is registered on the tool domain:

```ts
await ctx.tool.hook("execute.before", (event) => {
  // event.tool, event.sessionID, event.agent, event.messageID, event.id, event.input
})
```

`event.id` is the tool call ID (`Tool.CallID`). The plugin dedupes on
`(sessionID, id)`, so a retried notification for the same call does not run
`ctrlz mark` twice. It also marks only the first tool call of a user turn:
later tool calls are already covered by that snapshot, and marking again would
move the undo target into the middle of the agent's work. The verified type in
`@opencode/plugin` 2.2.6 is:

```ts
interface ToolHooks {
  "execute.before": {
    tool: string
    readonly sessionID: string
    readonly agent: string
    readonly messageID: string
    readonly id: string
    input: unknown
  }
}
```

The plugin resets its per-session dedupe set on `session.idle`. The event is
subscribed through the public event stream:

```ts
for await (const event of ctx.event.subscribe({
  types: ["session.idle"],
  signal: controller.signal,
})) {
  const sessionID = event.data?.sessionID
}
```

The verified shape in `@opencode/schema` 2.2.6 is `type: "session.idle"` with
`data.sessionID`. There is no `callID` field in V2; the V1 `input.callID` from
the brief maps to `event.id` here.

The plugin is non-throwing and quiet. It spawns `ctrlz mark --agent opencode
--session <id> --label prompt --if-stale 2s` detached, with `stdio: "ignore"`
and `windowsHide: true`, and unrefs the child. The spawn itself does not wait
for the snapshot, so a tool call is not delayed by the mark. `--if-stale` is the
second guard against repeat marks when several sessions start close together.

`ctrlz hook opencode` also exists for clients that prefer a stdin payload. It
accepts `sessionID` (or `session`) and `event` (or `type`), and treats an event
name containing `idle` as the end of a turn. The shipped plugin calls `mark`
directly instead.
