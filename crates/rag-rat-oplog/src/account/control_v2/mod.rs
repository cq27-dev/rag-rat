//! Control-specific v2 prerequisites, deliberately disconnected from production ingestion.
//!
//! A resolved view is an exact replay INPUT, not an authority verdict. Activation still needs
//! frozen legacy branch exclusions, register admission and readiness, then a policy-aware fold
//! deriving accepted-authority counts and cut-local credit from these views. Never feed the
//! combined candidate set into the v1 fold or treat a resolved frontier as effective credit.

pub(super) mod ops;
pub(super) mod views;

#[cfg(test)]
mod tests;
