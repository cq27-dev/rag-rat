//! The account ANNEX log (`log_id = 3`) — authority-inert bookkeeping artifacts (C6, #609).
//!
//! Today it carries exactly one artifact class: the signed snapshot and its plaintext coverage
//! manifest (§4.7). The log is named for the CLASS rather than the op because log 0's tag set is
//! effectively closed — an added control type must be either effective (which shifts
//! `effective_count` and parks later entries un-healably on binaries that do not know it) or
//! ineffective (which orphans every later entry from that device, #809) — so every future
//! authority-inert artifact belongs here rather than minting a fourth log.
//!
//! **Nothing on this log ever folds.** `fold_account`'s `log_id` gate short-circuits before any tag
//! dispatch, so an annex entry is stored, retained header-only, and cannot mint authority, shift
//! `effective_count`, or enter control-chain branch selection. That inertness is topological, not a
//! property some future match arm must remember to preserve.
//!
//! Scope this slice is deliberately WIRE-ONLY: the payload wire, its canonical form, and the
//! ingest-time structural gate. There is no acceptance lane — an annex entry is never "accepted",
//! it is merely stored and structurally valid — and no verification. Whether a manifest's claim is
//! TRUE (re-folding its covered prefix reproduces `folded_state_hash`) is a read-time question,
//! because answering it requires holding the covered history and an acceptance rule that reads
//! local inventory would make the verdict device-dependent, breaking convergence.

pub(in crate::account) mod ops;
