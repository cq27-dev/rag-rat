//! Clone-refinement algorithms (#215 Plan 4a/4b): coherence split → LCS align → anti-unify →
//! signature → score → cache.
//!
//! Each step lives in a focused sibling file; this module curates the `pub(crate)` surface.

#[allow(dead_code)] // wired by Plan-4a Task 4
pub(crate) mod split;

/// Input to the LCS-based variation-point analysis for one clone-class member (#215 Plan 4a Task
/// 2).
///
/// Produced by `IndexDatabase::load_refine_members`, which re-reads each member's scoped source,
/// re-parses it, descends to the symbol node, and re-normalizes to the ordered baseline token
/// sequence (`seq`) — the refine input Plan 1 does NOT persist (Plan 1 stores only the struct_hash
/// and the order-independent token multiset). The members are returned in CANONICAL order, sorted
/// by `struct_hash` then `symbol_id` as a tiebreak — the ordinal basis the anti-unify
/// `per_member_values[]` aligns to.
#[allow(dead_code)] // fields wired by Plan-4a Task 3 (LCS) / Task 4 (driver)
pub(crate) struct RefineMember {
    pub(crate) symbol_id: i64,
    pub(crate) lang: crate::language::Language,
    pub(crate) path: String,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    /// Persisted baseline struct_hash — the canonical sort key + cache key.
    pub(crate) struct_hash: String,
    /// Ordered baseline token sequence (LCS input).
    pub(crate) seq: Vec<String>,
}
