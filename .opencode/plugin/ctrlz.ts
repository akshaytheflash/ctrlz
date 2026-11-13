import { Plugin } from "@opencode/plugin"
import { spawn } from "node:child_process"
import { createHandler } from "./ctrlz-handler.mjs"

export default Plugin.define({
  id: "ctrlz",
  async setup(ctx) {
    const cwd = ctx.location.directory
    const bin = process.env.CTRLZ_BIN ?? "ctrlz"

    const handler = createHandler({
      mark(sessionID: string) {
        const child = spawn(
          bin,
          [
            "mark",
            "--agent",
            "opencode",
            "--session",
            sessionID,
            "--label",
            "prompt",
            "--if-stale",
            "2s",
          ],
          { cwd, stdio: "ignore", detached: true, windowsHide: true },
        )
        child.on("error", () => {})
        child.unref()
      },
    })

    await ctx.tool.hook("execute.before", (event) => {
      try {
        handler.before(event)
      } catch {
        // A hook must never turn a snapshot failure into a failed tool call.
      }
    })

    const controller = new AbortController()
    void (async () => {
      try {
        for await (const event of ctx.event.subscribe({
          types: ["session.idle"],
          signal: controller.signal,
        })) {
          const sessionID = event.data?.sessionID
          try {
            handler.idle(sessionID)
          } catch {
            // Resetting the dedupe set is not worth interrupting the session.
          }
        }
      } catch {
        // The subscription ends when the plugin unloads.
      }
    })()

    return () => controller.abort()
  },
})
