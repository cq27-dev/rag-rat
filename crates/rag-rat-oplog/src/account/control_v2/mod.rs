//! Control-specific v2 prerequisites, deliberately disconnected from production ingestion.
//!
//! A resolved view is an exact replay INPUT, not an authority verdict. [`executor`] turns one into
//! a verdict: it authenticates every nominated entry, walks it back to a branch the checkpoint
//! accepted, applies the frozen legacy policy, and derives the operation's registers plus the
//! bounded credit its nomination earns. Nothing here dispatches from the v1 fold, and executing an
//! operation flips no readiness or pin state.

pub(super) mod executor;
pub(super) mod ops;
pub(super) mod views;

#[cfg(test)]
mod tests;
