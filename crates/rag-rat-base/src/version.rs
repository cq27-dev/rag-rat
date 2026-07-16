//! Process-wide binary-version identity for provenance stamps.

/// The running binary's version string, for migration provenance (#585) and diagnostics. The CLI
/// sets it once at startup from its git-stamped `RAG_RAT_VERSION` (a dev build reads e.g.
/// `0.16.0+g<hash>`, a tagged release `0.16.0`); left unset (library callers, tests) it falls back
/// to this crate's compile-time `CARGO_PKG_VERSION`.
static BINARY_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the running binary's version once, at process startup. Idempotent: only the first call
/// wins, so a later mistaken call can't rewrite provenance mid-run.
pub fn set_binary_version(version: impl Into<String>) {
    let _ = BINARY_VERSION.set(version.into());
}

/// The version string [`set_binary_version`] recorded, or this crate's `CARGO_PKG_VERSION` default.
pub fn binary_version() -> &'static str {
    BINARY_VERSION.get().map(String::as_str).unwrap_or(env!("CARGO_PKG_VERSION"))
}
