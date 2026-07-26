//! Live LSP oracle (#74): a resident language-server client that produces the SAME `edge_oracle`
//! resolution shape the batch SCIP pass produces, but for dirty/overlay files at watcher cadence
//! rather than a whole-checkout SCIP build.
//!
//! Slice 1 (this module) is the CLIENT SUBSTRATE — process lifecycle + JSON-RPC framing +
//! per-document `textDocument/definition` resolution + the position-encoding conversion the LSP
//! spec demands — exercised end to end against a fake in-process LSP server (a thread over
//! `std::io::pipe`), so it is fully unit-testable without a real `rust-analyzer`. Slice 2 (#534)
//! wired it to the watcher: `crate::live` owns the DB mapping (definition → symbol id via
//! `join::map_definition_to_symbol`, batch-moniker synthesis, `EdgeOracleRow` assembly + write)
//! and the maintenance-pass tail drives it. `resolve_callees` (the variant WITH the moniker
//! fan-out) has no production caller yet — the live write path never persists LSP monikers, so it
//! resolves definitions only (`resolve_definitions`).
//!
//! Layout mirrors the batch reader's split (`scip.rs` / `join.rs`):
//! - `position.rs` — LSP `(line, character)` ↔ absolute byte conversion, per the negotiated
//!   position encoding. The batch analog is `scip::LineColumnToByte`; LSP defaults to UTF-16 and
//!   negotiates via `initialize`, and unlike SCIP we need BOTH directions (byte → position to ASK
//!   for a definition, position → byte to READ one back).
#![allow(dead_code)] // `resolve_callees`'s moniker fan-out is unused until a consumer needs it.

pub(crate) mod client;
pub(crate) mod position;
mod protocol;
pub(crate) mod resolve;
