export function createHandler({ mark }) {
  const seen = new Map()
  const marked = new Set()

  return {
    before(event) {
      const session = event?.sessionID
      const call = event?.id ?? event?.callID
      if (!session || !call) return false

      let calls = seen.get(session)
      if (!calls) {
        calls = new Set()
        seen.set(session, calls)
      }
      if (calls.has(call)) return false
      calls.add(call)

      // One snapshot per user turn. Later tool calls are already covered by
      // the first snapshot; marking again would make undo land mid-turn.
      if (marked.has(session)) return false
      marked.add(session)

      try {
        mark(session, call)
      } catch {
        calls.delete(call)
        marked.delete(session)
        return false
      }
      return true
    },

    idle(session) {
      if (session === undefined) {
        seen.clear()
        marked.clear()
      } else {
        seen.delete(session)
        marked.delete(session)
      }
    },
  }
}
