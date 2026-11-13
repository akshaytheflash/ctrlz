import test from "node:test"
import assert from "node:assert/strict"

import { createHandler } from "../../.opencode/plugin/ctrlz-handler.mjs"

test("marks once per tool call id and once per user turn", () => {
  const marks = []
  const handler = createHandler({
    mark(session, call) {
      marks.push(`${session}:${call}`)
    },
  })

  const payload = { tool: "write", sessionID: "ses_1", id: "call_1" }
  handler.before(payload)
  handler.before(payload)
  assert.deepEqual(marks, ["ses_1:call_1"])

  // A second tool call in the same turn must not move the undo target.
  assert.equal(handler.before({ ...payload, id: "call_2" }), false)
  assert.deepEqual(marks, ["ses_1:call_1"])
})

test("session.idle resets the turn for that session", () => {
  const marks = []
  const handler = createHandler({
    mark(session, call) {
      marks.push(`${session}:${call}`)
    },
  })

  handler.before({ sessionID: "ses_1", id: "call_1" })
  handler.idle("ses_1")
  handler.before({ sessionID: "ses_1", id: "call_2" })
  assert.deepEqual(marks, ["ses_1:call_1", "ses_1:call_2"])

  handler.before({ sessionID: "ses_2", id: "call_1" })
  assert.deepEqual(marks, [
    "ses_1:call_1",
    "ses_1:call_2",
    "ses_2:call_1",
  ])
})

test("a failed mark can be retried", () => {
  let attempts = 0
  const handler = createHandler({
    mark() {
      attempts += 1
      if (attempts === 1) throw new Error("spawn failed")
    },
  })

  const payload = { sessionID: "ses_1", id: "call_1" }
  assert.equal(handler.before(payload), false)
  assert.equal(handler.before(payload), true)
  assert.equal(attempts, 2)
})
