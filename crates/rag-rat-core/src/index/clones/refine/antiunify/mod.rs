//! Anti-unification (least general generalization) of a clone class (#215 Plan 4b Task 5d).
//!
//! Turns a set of structurally-similar members into an extractable-helper **template** plus a list
//! of **variation points** (metavars): the columns where the members agree become fixed source, the
//! columns where they disagree become holes with ordinal-aligned per-member values and an
//! extraction-role classification.
//!
//! # Mechanism — medoid-anchored star LCS
//!
//! Pure algorithm, no DB. One anchor member (the class medoid, or the canonical-first member when
//! no medoid is threaded) carries the spine. Each other member is aligned to the anchor with
//! [`super::align::lcs_align`] (N−1 aligns, not N²). A spine column is FIXED iff *every* member
//! contributes a matched token there; otherwise it is a VARIATION column. Contiguous variation
//! columns coalesce into runs, each run snaps to the tightest enclosing anchor subtree, and the
//! members' real source slices (recovered through the seq↔AST [`NodeSpan`] bijection) become the
//! per-member values.
//!
//! # Baseline-only — the Plan-3 SCIP seam
//!
//! Classification is syntactic. In particular, two call/method heads whose callee subtree DIFFERS
//! across members are **always** `closure_param` Medium/Low — never a high-confidence
//! `value_param`. Baseline normalization cannot prove two differing callees resolve to the same
//! symbol, so the differing-callee guard ([`classify_run`]) deliberately refuses to promote them.
//! SCIP resolution (Plan 3) is what would lift that; until then the conservative band is the honest
//! one.
//!
//! # Determinism
//!
//! Members arrive in canonical `(struct_hash, path, start_byte)` order — the `per_member_values`
//! ordinal basis. `metavar_id`s are assigned `m0, m1, …` ascending by the run's first spine column.
//! No HashMap iteration drives any output ordering (inserts use a `BTreeMap` keyed by anchor
//! column).
//! The same struct_hash multiset therefore always yields the same template.
//!
//! # Simplifications (documented, cosmetic-only)
//!
//! - **Subtree snap** ([`snap_run`]): a run snaps to an anchor subtree only when the anchor node at
//!   the run's first column is itself the subtree root AND its whole pre-order token count fits the
//!   run exactly. Otherwise the run keeps its raw column span and the snapped kind is the anchor's
//!   node kind at `lo`. A looser snap (outermost-contained / smallest-enclosing) is a template
//!   *legibility* refinement, never a correctness one — `per_member_values` are recovered from real
//!   source byte offsets regardless of how the run snaps.
//! - **Recurrence collapse** ([`collapse_recurring`]): two runs collapse into one metavar with
//!   multiple `occurrences[]` only when their `per_member_values` tuple AND snapped-subtree kind
//!   are byte-identical across all members. Conservative — a near-miss stays two metavars.

mod alignment;
mod budget;
mod build;
mod classify;
mod render;
mod spans;
mod statement;
mod types;
mod values;
mod widen;

#[cfg(test)]
use std::collections::BTreeMap;

#[cfg(test)]
use alignment::align_to_anchor_with_budget;
pub(crate) use alignment::{align_to_anchor, resolve_anchor_idx};
#[cfg(test)]
use budget::{ALIGN_AGGREGATE_CELLS_BUDGET, CellBudget};
pub(crate) use build::{anti_unify, anti_unify_global};
#[cfg(test)]
use build::{anti_unify_with_budget, collapse_recurring};
#[cfg(test)]
use classify::run_in_callee_position;
#[cfg(test)]
use render::render_template;
#[cfg(test)]
use types::RunMetavar;
#[allow(unused_imports)]
pub(crate) use types::{ClassAlignment, OccSpan};
pub(crate) use types::{MetavarKind, Template, VariationPoint};

#[cfg(test)]
use super::RefineMember;
#[cfg(test)]
use super::align;
#[cfg(test)]
use super::score::Confidence;
#[cfg(test)]
use crate::index::clones::normalize::NodeSpan;

#[cfg(test)]
mod tests;
