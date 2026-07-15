# Memory surfacing (`[memory] surface`)

Part of the [config reference](../config.md).

`[memory] surface` controls how memory attachments and memory-query results render. The default is
`summary`: memory attachments (and `memory_search` / `memory_for_call_path` / `memory_for_edges`
hits) show the dream-compacted summary plus a plain-text verdict marker (`[verdict: diverged]` /
`[verdict: current @<short-commit>]`) in place of the full body, for each memory that has one, and
fall back to the title alone until one is generated. Set it to `full` to restore whole bodies
everywhere. Either way, `memory show` / `memory_show` always returns the full body — that is the
deliberate "expand on request" path.

```toml
[memory]
surface = "summary"   # default; or "full" — return whole bodies everywhere
```

`summary` applies across every drive-by renderer: `impact_surface`'s compact `repo_memories`, the
`symbol_lookup` / `find_callers` / `trace_callees` attached memories, `read_chunk`'s memories,
`memory_for_symbol` / `memory_for_path` (which keep the full binding/call-path structure but defer
the body), and the grep-augmentation hook context. A memory with no summary for its current body
falls back to title-only. `memory show` / `memory_show` **always** return the full body, regardless
of this setting — it is the expand path.
