//! Give freed heap back to the OS after a large transient working set.
//!
//! The sibling of [`crate::stack`]: `stack` grows what a deep walk needs, this returns what a
//! heavy pass no longer holds.

/// Release freed heap pages back to the OS after a large transient working set has been dropped.
///
/// glibc malloc rarely returns freed memory to the OS on its own: freed blocks park in its
/// arenas, so a long-lived process retains its *peak* heap as private-dirty RSS. SQLite is the
/// dominant source of that peak here — the bundled libsqlite3 allocates through the C allocator
/// (Rust's `#[global_allocator]` never sees it), so a maintenance pass's page cache and
/// `temp_store = MEMORY` sorts, a full rebuild's bulk-build cache, a migration's one-time
/// buffers, and a VACUUM's rewrite all land in glibc arenas. SQLite frees all of it at
/// connection close, but the arenas keep the pages: a store-owning server was measured holding
/// 745 MiB RSS with zero open connections (#906). `malloc_trim(0)` walks the arenas and hands
/// the freed pages back.
///
/// Call it only at the terminal of a heavy, bounded episode — a maintenance pass that ran real
/// work, a full rebuild, a schema migration, a VACUUM — never on a hot path: it takes the malloc
/// locks and `madvise`s freed ranges, so it costs milliseconds after a big pass, and it can only
/// help when a large working set was actually just freed.
///
/// glibc-only: `malloc_trim` is a glibc extension (musl, macOS, and Windows have no equivalent),
/// so everywhere else this compiles to a no-op. jemalloc builds (the CLI binary) are unaffected
/// either way — jemalloc's background purge already returns the *Rust* heap, and this call
/// reaches only the glibc arenas that C allocations (SQLite) live in.
pub fn release_freed_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim is a thread-safe glibc call with no preconditions; the argument is
    // the amount of free space to leave untrimmed at the top of the heap.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    /// Smoke: callable on every target — glibc trims, everywhere else it is a no-op. The RSS
    /// effect itself is not unit-assertable (`malloc_trim`'s return value only says whether any
    /// memory was released, which depends on allocator state); the measured evidence lives in
    /// #906.
    #[test]
    fn release_freed_heap_is_a_safe_no_op_or_trim() {
        super::release_freed_heap();
        // A second call must also be fine: pass terminals can fire back-to-back (a startup
        // catch-up pass followed by a scheduled pass).
        super::release_freed_heap();
    }
}
