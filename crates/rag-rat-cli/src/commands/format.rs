//! Process-wide output-format state, split out of the `commands` god-module. The global `--json`
//! flag is pinned once in `main` and read by `print_output` (~30 call sites) without threading an
//! `OutputFormat` through every command signature.
use std::sync::OnceLock;

use rag_rat_core::OutputFormat;

/// Process-wide output format, set once from the global `--json` flag in `main` before any command
/// runs. A `OnceLock` keeps `print_output` (~30 call sites) from threading an `OutputFormat`
/// through every command signature; it defaults to TOON if `main` never sets it (e.g. a unit test
/// calling a command helper directly).
static OUTPUT_FORMAT: OnceLock<OutputFormat> = OnceLock::new();

/// Set the global output format. Called once from `main` from the parsed `--json` flag; a second
/// call is a no-op (`OnceLock::set` returns `Err`), so tests can't accidentally clobber it.
pub(crate) fn set_output_format(format: OutputFormat) {
    let _ = OUTPUT_FORMAT.set(format);
}

/// The format `print_output` renders in — TOON unless `main` set JSON from `--json`.
pub(crate) fn output_format() -> OutputFormat {
    OUTPUT_FORMAT.get().copied().unwrap_or_default()
}
