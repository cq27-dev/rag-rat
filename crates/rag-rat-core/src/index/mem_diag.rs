//! Opt-in memory diagnostics for the indexer: a SQLite soft-heap-limit cap and a peak-RSS / SQLite
//! allocation probe, both gated on env vars and no-ops otherwise. Used to localize and attribute
//! full-rebuild memory spikes (Rust heap vs SQLite allocator) without affecting normal runs.

/// Diagnostic: when `RAG_RAT_SQLITE_SOFT_HEAP_LIMIT_MB` is set, cap SQLite's soft heap limit
/// (process-wide) for this run. Distinguishes a *discretionary* SQLite buffer spike (clamps with
/// little wall cost — the buffer just flushes earlier) from a *load-bearing* allocation (wall
/// blows up under the cap because the work genuinely needs the memory). No-op unless set.
pub(crate) fn maybe_set_sqlite_soft_heap_limit() {
    let Ok(raw) = std::env::var("RAG_RAT_SQLITE_SOFT_HEAP_LIMIT_MB") else {
        return;
    };
    let Ok(mb) = raw.trim().parse::<i64>() else {
        return;
    };
    if mb <= 0 {
        return;
    }
    let bytes = mb.saturating_mul(1024 * 1024);
    // SAFETY: sqlite3_soft_heap_limit64 is a thread-safe configuration accessor with no
    // preconditions; it returns the prior limit.
    let prev = unsafe { rusqlite::ffi::sqlite3_soft_heap_limit64(bytes) };
    eprintln!("MEMTRACE soft_heap_limit set to {mb} MB (was {} MB)", prev / 1024 / 1024);
}

/// Diagnostic peak-RSS probe: when `RAG_RAT_MEM_TRACE` is set, print process resident set
/// (`/proc/self/status` VmRSS) and SQLite's outstanding allocation (`sqlite3_memory_used`,
/// process-wide) at a labelled point. The two together localize a full-rebuild memory spike to a
/// phase AND attribute it to Rust heap vs SQLite. Off by default — a single env check, zero cost in
/// normal runs; it only reads counters and prints to stderr, so it never affects index output.
pub(crate) fn mem_trace(label: &str) {
    // Off unless the env var is set to a truthy value — empty / "0" / "false" count as off so a
    // workflow that always passes the var (possibly empty) doesn't trace by accident.
    match std::env::var("RAG_RAT_MEM_TRACE").as_deref() {
        Ok("" | "0" | "false") | Err(_) => return,
        Ok(_) => {},
    }
    let vmrss_kb = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmRSS:"))
                .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
        })
        .and_then(|kb| kb.parse::<i64>().ok())
        .unwrap_or(0);
    // SAFETY: both are thread-safe libsqlite3 accessors with no preconditions. `highwater(1)`
    // returns the peak SQLite allocation since the previous probe and RESETS it, so each line's
    // `sqlite_peak` is the high-water of the phase that just ran — this catches an in-statement
    // spike (e.g. an FTS5 'rebuild') that has already freed by the time we read `used` at the
    // boundary. A high `sqlite_peak` with flat `rss` ⇒ SQLite-allocator; a high `rss` jump with
    // flat `sqlite_peak` ⇒ the spike is outside SQLite's allocator (glibc arena / temp / mmap).
    let sqlite_bytes = unsafe { rusqlite::ffi::sqlite3_memory_used() };
    let sqlite_peak = unsafe { rusqlite::ffi::sqlite3_memory_highwater(1) };
    // Elapsed since the first probe — phase deltas reveal which trailing phase is the long one that
    // the 1 s sampler's spike window falls inside.
    static FIRST: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let elapsed = FIRST.get_or_init(std::time::Instant::now).elapsed().as_secs_f64();
    let rss_gb = vmrss_kb as f64 / 1024.0 / 1024.0;
    let sqlite_gb = sqlite_bytes as f64 / 1_073_741_824.0;
    let sqlite_peak_gb = sqlite_peak as f64 / 1_073_741_824.0;
    eprintln!(
        "MEMTRACE {label}: t+{elapsed:.0}s rss={rss_gb:.2}GB sqlite={sqlite_gb:.2}GB \
         sqlite_peak={sqlite_peak_gb:.2}GB"
    );
    // Also capture in the debug log (when `[log]` is on). The `RAG_RAT_MEM_TRACE` env gate above
    // still guards whether this runs at all, so a broad `debug` filter never enables memtrace spam;
    // the dedicated `rag_rat_core::index::mem_diag` target keeps it filterable on its own.
    tracing::debug!(target: "rag_rat_core::index::mem_diag", label, elapsed_s = elapsed, rss_gb, sqlite_gb, sqlite_peak_gb, "memtrace");
}
