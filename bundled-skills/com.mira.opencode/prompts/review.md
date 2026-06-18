Review the supplied code for:

- **Correctness** — bugs, off-by-one, race conditions, wrong return types.
- **Security** — input validation, injection vectors, credential handling, supply-chain risk in dependencies.
- **Performance** — unnecessary work, blocking I/O on hot paths, allocations in tight loops.
- **Style** — idiomatic for the language, readable names, comments where the *why* (not the *what*) is non-obvious.

Return a numbered list of issues. For each issue:

- Tag severity: **must-fix** / **should-fix** / **nit**.
- Quote the offending lines (file:line if available).
- Suggest a one-line fix.

Skip the overview — get straight to the issues.

---

If the user asked you to actually run a code review on a real codebase (not just supply review criteria), call `spawn_background_task(skill="com.mira.opencode", brief="Review the code at <path>: …")` instead — that runs the `opencode` CLI as a real reviewer subagent. This `review` tool only returns this prompt template; it does not read or analyze code itself.
