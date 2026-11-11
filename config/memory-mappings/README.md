# Memory-provider MCP mappings

Starter mapping files that translate the MIRA Memory Provider SPI verbs
(`search` / `add` / `forget`) onto a specific MCP memory server's tool names and
arguments. Point a provider at one with `memory.providers[].mapping`:

```jsonc
{ "id": "mem0", "enabled": true, "transport": "mcp",
  "command": "…launch the mem0 MCP server…",
  "mapping": "config/memory-mappings/mem0.json" }
```

See `docs/reference/memory-provider-spec.md` §8 for the format. Placeholders
`$query`, `$user_id`, `$k`, `$content`, `$id`, `$namespace` are substituted from
each call.

## What ships here

- **`mem0.json`** — Mem0 OSS (`search_memories` / `add_memory` / `delete_memory`).
  Flat args; matches the SPI example directly. Read + write + forget.
- **`reference.json`** — the MCP reference knowledge-graph server
  (`search_nodes`). Read-only: the reference server's write API is
  graph-structured (`create_entities` / `add_observations` take nested entity
  objects, not a flat fact), which the current flat placeholder substitution
  can't express — recall works, writes go through the built-in store.
- **`graphiti.json`** — Graphiti/Zep (`search_memory_facts` / `add_memory`).
  Read + write.

## Caveats (verify before trusting)

These are **starting points**, not guaranteed-correct against every server
version — confirm each against your server's actual tool schema:

- Placeholder substitution is **top-level only**: a value like `"$query"` is
  replaced, but a placeholder nested inside an array/object (e.g.
  `"group_ids": ["$user_id"]`) is **not**. If a server needs an array-wrapped or
  nested argument, adjust the mapping (or the server's input shape) accordingly.
- Result extraction reads **top-level** per-element fields (`content_at` etc.).
- An unmapped server is rejected — MIRA never guesses tool names.
