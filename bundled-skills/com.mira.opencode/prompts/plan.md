The user has asked for code. Before writing any:

1. **Restate the problem** in your own words to confirm understanding. One sentence.
2. **Identify ambiguities.** Ask 1–2 clarifying questions if there are critical unknowns. Otherwise, state your assumptions explicitly.
3. **Sketch the approach** — data structures, key functions, error cases, dependencies, what's out of scope.
4. **Acceptance criteria** — how the user (or a test) will know the result is correct.

Only after the plan is acceptable, write the code in a follow-up message. Don't combine plan + code in one reply unless the task is genuinely trivial (one function, no design choices).

---

If the user asked you to actually BUILD something (not just plan), call `spawn_background_task(skill="com.mira.opencode", brief="<full instruction>")` instead — that runs the `opencode` CLI as a real coding subagent. This `plan` tool only returns this prompt template; it does not write or execute code itself.
