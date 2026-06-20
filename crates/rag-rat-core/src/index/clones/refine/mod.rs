//! Clone-refinement algorithms (#215 Plan 4a/4b): coherence split → LCS align → anti-unify →
//! signature → score → cache.
//!
//! Each step lives in a focused sibling file; this module curates the `pub(crate)` surface.

#[allow(dead_code)] // wired by Plan-4a Task 4
pub(crate) mod split;
