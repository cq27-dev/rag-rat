# Version check (`[version_check]`)

Part of the [config reference](../config.md).

`[version_check]` controls the best-effort check for a newer published `rag-rat` on crates.io,
surfaced to agents/operators in the SessionStart digest and the `index_status` MCP tool's `version`
field (current vs latest + the `cargo install rag-rat --force` update command):

```toml
[version_check]
enabled = true   # opted in by default; false makes zero network calls
```

The check is **cached** (refreshed at most once a day, out of band by the long-lived `rag-rat mcp`
server) and **fail-open** — offline, a non-200, or a parse miss simply yields no version info, never
an error, and **never blocks** session start (reads only the cache; `rag-rat version-check` refreshes
it synchronously on demand). The cache lives at `<index-dir>/version-check.json`. Set
`enabled = false` to disable the feature entirely (no crates.io requests).
