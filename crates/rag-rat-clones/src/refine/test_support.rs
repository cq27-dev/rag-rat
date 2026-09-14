//! Fixtures shared by the refine module's unit tests.

use std::path::Path;
use std::sync::Arc;

use rag_rat_base::language::Language;
use rag_rat_core::index::parser;

use super::RefineMember;
use crate::normalize::normalize_baseline_spanned;
use crate::tokens;

/// Build a `RefineMember` from a Rust snippet, mirroring `load_refine_members`: parse, descend
/// to the first `function` symbol, span-normalize, compute the faithfulness struct_hash.
pub(crate) fn member(symbol_id: i64, src: &str) -> RefineMember {
    let text: Arc<str> = Arc::from(src);
    let parsed = parser::parse_file(Path::new("t.rs"), Language::Rust, &text).expect("parse");
    let func = parsed.symbols.iter().find(|s| s.kind == "function").expect("a function symbol");
    let node =
        parsed.root().descendant_for_byte_range(func.start_byte, func.end_byte).expect("node");
    let (seq, node_spans) = normalize_baseline_spanned(node, &text, Language::Rust);
    let struct_hash = tokens::struct_hash(&seq);
    RefineMember {
        callee_monikers: Default::default(),
        symbol_id,
        lang: Language::Rust,
        struct_hash,
        seq,
        node_spans,
        text,
    }
}

/// Build a `RefineMember` from a TypeScript snippet — the TS analogue of [`member`]. Picks the
/// target symbol by MAX normalized-token count (the function body), exactly as the production
/// loader / the `normalize` tests' `target_node_for` do, so it works for TS `function`,
/// `const`/arrow declarators, etc. Used by the template-literal / TS-string tests (#254 #274).
pub(crate) fn member_ts(symbol_id: i64, src: &str) -> RefineMember {
    let text: Arc<str> = Arc::from(src);
    let parsed = parser::parse_file(Path::new("t.ts"), Language::TypeScript, &text).expect("parse");
    let node = parsed
        .symbols
        .iter()
        .filter_map(|s| {
            let n = parsed.root().descendant_for_byte_range(s.start_byte, s.end_byte)?;
            Some((normalize_baseline_spanned(n, &text, Language::TypeScript).0.len(), n))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, n)| n)
        .expect("a body symbol");
    let (seq, node_spans) = normalize_baseline_spanned(node, &text, Language::TypeScript);
    let struct_hash = tokens::struct_hash(&seq);
    RefineMember {
        callee_monikers: Default::default(),
        symbol_id,
        lang: Language::TypeScript,
        struct_hash,
        seq,
        node_spans,
        text,
    }
}

/// Sort members into the canonical order the loader guarantees. Production keys on the
/// REINDEX-STABLE `(struct_hash, path, start_byte)` (see `canonical_member_order_key` /
/// `refine_member_order_is_reindex_stable`). `RefineMember` (a test fixture here) carries no
/// `path`/`start_byte`, so this helper sorts `struct_hash` then `symbol_id` — the test members
/// assign `symbol_id` to coincide with `(path, start_byte)`, so the two keys produce the SAME
/// order on these fixtures; the production guard is the reindex-stable unit test, not this
/// sort.
pub(crate) fn canonical(mut members: Vec<RefineMember>) -> Vec<RefineMember> {
    members.sort_by(|a, b| {
        a.struct_hash.cmp(&b.struct_hash).then_with(|| a.symbol_id.cmp(&b.symbol_id))
    });
    members
}
