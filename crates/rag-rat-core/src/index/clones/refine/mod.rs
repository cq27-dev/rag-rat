//! Clone-refinement algorithms (#215 Plan 4a/4b): coherence split → LCS align → anti-unify →
//! signature → score → cache.
//!
//! Each step lives in a focused sibling file; this module curates the `pub(crate)` surface.

pub(crate) mod split;

pub(crate) mod align;

pub(crate) mod antiunify;

pub(crate) mod signature;

pub(crate) mod cache;

pub(crate) mod score;

/// Input to the LCS-based variation-point analysis for one clone-class member (#215 Plan 4a Task
/// 2, extended in Plan 4b Task 5b).
///
/// Produced by `IndexDatabase::load_refine_members`, which re-reads each member's scoped source,
/// re-parses it, descends to the symbol node, and re-normalizes to the ordered baseline token
/// sequence (`seq`) — the refine input Plan 1 does NOT persist (Plan 1 stores only the struct_hash
/// and the order-independent token multiset). The members are returned in CANONICAL order, sorted
/// by `struct_hash` then `symbol_id` as a tiebreak — the ordinal basis the anti-unify
/// `per_member_values[]` aligns to.
///
/// `node_spans` and `text` support the Plan-4b anti-unification step: `node_spans[i]` carries the
/// ABSOLUTE file byte offsets for `seq[i]`, and `text` is the whole-file source. Members sharing
/// one file share the same `Arc<str>` buffer (one allocation per file). Per §1.6: recover a token's
/// source slice via `text.get(node_spans[i].start_byte..node_spans[i].end_byte)`.
pub(crate) struct RefineMember {
    pub(crate) symbol_id: i64,
    pub(crate) lang: crate::language::Language,
    /// Persisted baseline struct_hash — the canonical sort key + cache key.
    pub(crate) struct_hash: String,
    /// Ordered baseline token sequence (LCS input). Parallel to `node_spans`.
    pub(crate) seq: Vec<String>,
    /// AST span for each token in `seq` (Plan-4b). `node_spans[i]` ↔ `seq[i]`.
    /// Byte offsets are ABSOLUTE file offsets (full-file `text` is the backing buffer).
    /// Produced by `normalize_baseline_spanned`; same length as `seq`.
    pub(crate) node_spans: Vec<crate::index::clones::normalize::NodeSpan>,
    /// Whole-file source for this member's file (Plan-4b). Members sharing one file share the
    /// same `Arc` — one allocation per distinct path. Use
    /// `text.get(span.start_byte..span.end_byte)` to recover real source for any `NodeSpan` in
    /// `node_spans`.
    pub(crate) text: std::sync::Arc<str>,
}
