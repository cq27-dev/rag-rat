//! Clone-detection fingerprint substrate (#215 Phase 1): a scope-independent structural
//! fingerprint per function symbol, computed during indexing.

// `NormalizerKind::Scip` + `from_db_str` are the Plan-3 SCIP token-space surface (a separate
// `normalizer_kind='scip'` postings space, used only at refine/ranking when every member has it).
// They are dead until Plan 3 lands; the rest of the module is live (R4's candidate read uses it).
#![allow(dead_code)]

pub(crate) mod normalize;
pub(crate) mod refine;
pub(crate) mod tokens;

use tree_sitter::Node;

/// Bumped when normalization changes; invalidates fingerprints (and the later refine cache) without
/// a schema migration.
pub(crate) const NORM_VERSION: i64 = 1;
/// Bumped when the LCS alignment / refinement algorithm changes; participates in the content-
/// addressed `refinement_key` and in the `clone_refinements` cache freshness predicate, so a bump
/// invalidates every cached refinement without a schema migration (the same discipline as
/// [`NORM_VERSION`]).
///
/// `2` (#215 Plan 4b): the placeholder 4a refinement (crude LCS skeleton template, `"[]"` /
/// `"{}"` payloads, `lcs_ratio` coverage proxy, v1 scores) is replaced by the full
/// anti-unification (real template + variation points + proposed signature + REAL coverage, v2
/// scores). Every 4a-cached row at `alignment_version = 1` therefore MISSES the lookup
/// (`WHERE … alignment_version = ?3`) and is recomputed with the 4b payload on the next refine.
pub(crate) const ALIGNMENT_VERSION: i64 = 2;
/// Smallest normalized-token count a symbol must reach to be fingerprinted (skip trivial getters).
pub(crate) const MIN_TOKENS: usize = 20;

/// Which token space a fingerprint was computed in. Baseline is always present and is the only
/// input to candidate recall; Scip is an optional precision signal (Plan 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NormalizerKind {
    Baseline,
    Scip,
}

impl NormalizerKind {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            NormalizerKind::Baseline => "baseline",
            NormalizerKind::Scip => "scip",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "baseline" => Some(NormalizerKind::Baseline),
            "scip" => Some(NormalizerKind::Scip),
            _ => None,
        }
    }
}

/// One symbol's baseline fingerprint, ready to persist.
#[derive(Debug, Clone)]
pub(crate) struct SymbolFingerprint {
    pub struct_hash: String,
    pub token_len: i64,
    /// `(token_hash, freq)` multiset sorted by `token_hash`. Stored in `symbol_token_postings`.
    pub token_bag: Vec<(i64, i64)>,
}

/// Baseline fingerprint for a symbol's AST node, or `None` if it normalizes below `MIN_TOKENS`.
pub(crate) fn fingerprint_symbol(node: Node<'_>, text: &str) -> Option<SymbolFingerprint> {
    let tokens = normalize::normalize_baseline(node, text);
    if tokens.len() < MIN_TOKENS {
        return None;
    }
    Some(SymbolFingerprint {
        struct_hash: tokens::struct_hash(&tokens),
        token_len: tokens.len() as i64,
        token_bag: tokens::token_bag(&tokens),
    })
}

/// Baseline fingerprints for a file's function symbols, walking the SHARED parse tree (no re-parse,
/// no DB). Returns `(local_symbol_index, fingerprint)` pairs keyed by index into `symbols`, so the
/// caller maps each to the right DB id when it writes. Non-function symbols, symbols that can't be
/// located in the tree, and bodies that normalize below `MIN_TOKENS` are skipped. The full-rebuild
/// prepare phase calls this from the parse it already did for symbols/edges; the incremental path
/// re-parses and calls it from `store_symbol_fingerprints`.
pub(crate) fn fingerprint_symbols(
    root: Node<'_>,
    text: &str,
    symbols: &[crate::index::symbols::Symbol],
) -> Vec<(usize, SymbolFingerprint)> {
    let mut out = Vec::new();
    for (i, symbol) in symbols.iter().enumerate() {
        if symbol.kind != "function" {
            continue;
        }
        let Some(node) = root.descendant_for_byte_range(symbol.start_byte, symbol.end_byte) else {
            continue;
        };
        if let Some(fp) = fingerprint_symbol(node, text) {
            out.push((i, fp));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::index::parser;
    use crate::language::Language;

    fn fp(src: &str) -> Option<SymbolFingerprint> {
        let parsed = parser::parse_file(Path::new("t.rs"), Language::Rust, src).expect("parse");
        let func = parsed.symbols.iter().find(|s| s.kind == "function").expect("function");
        let node =
            parsed.root().descendant_for_byte_range(func.start_byte, func.end_byte).expect("node");
        fingerprint_symbol(node, src)
    }

    #[test]
    fn renamed_clones_get_the_same_struct_hash_and_token_bag() {
        let a = "fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }";
        let b = "fn load_order(store: Db) -> i32 { let o = store.get(20); validate(o); o + 1 }";
        let fa = fp(a).expect("a fingerprinted");
        let fb = fp(b).expect("b fingerprinted");
        assert_eq!(fa.struct_hash, fb.struct_hash);
        assert_eq!(fa.token_bag, fb.token_bag);
    }

    #[test]
    fn trivial_bodies_below_min_tokens_are_not_fingerprinted() {
        assert!(fp("fn x() -> i32 { 0 }").is_none());
    }
}
