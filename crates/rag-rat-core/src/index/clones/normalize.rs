use std::collections::HashMap;

use tree_sitter::Node;

/// A leaf tree-sitter kind that names a binding/reference identifier (kept language-agnostic by
/// matching the `*identifier` suffix tree-sitter grammars use).
fn is_identifier_kind(kind: &str) -> bool {
    kind.ends_with("identifier")
}

/// A leaf kind that is a literal whose *value* must not drive matching.
fn is_literal_kind(kind: &str) -> bool {
    kind.ends_with("literal")
        || matches!(kind, "string_content" | "integer" | "float" | "number" | "char")
}

/// `true` for a tree-sitter node kind that names a Rust type position — a bare type name
/// (`type_identifier` / `scoped_type_identifier` / `qualified_type`), a primitive, OR an OUTER
/// composite type node that WRAPS another type (`reference_type` `&Foo`, `array_type` `[T; N]`,
/// `tuple_type` `(A, B)`, `pointer_type` `*const T`, `generic_type` `Box<Foo>`, `dynamic_type`
/// `dyn Trait`, `bounded_type`, `function_type` `fn(…) -> …`, `abstract_type` `impl Trait`,
/// `never_type` `!`).
///
/// SINGLE source of truth for the Rust type-node set, shared by
/// [`super::refine::antiunify`]'s `is_type_position` (classifying a variation that snaps to a type
/// node as a `type_param`) AND [`super::refine::signature`]'s `is_type_kind` (recovering the type
/// slice for a `: T` / `-> T` annotation). Keeping ONE predicate is what stops the two from
/// diverging — an outer composite type (`&Foo`, `[T; N]`, `(A, B)`) used to be a type to the
/// signature recoverer but NOT to the anti-unify classifier, which then mis-routed it to
/// `closure_param` (Fix 4, #215 Plan 4b Codex round-5). Any new Rust type kind goes here once.
pub(crate) fn is_rust_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        "type_identifier"
            | "generic_type"
            | "scoped_type_identifier"
            | "qualified_type"
            | "primitive_type"
            | "reference_type"
            | "array_type"
            | "tuple_type"
            | "abstract_type"
            | "never_type"
            | "dynamic_type"
            | "bounded_type"
            | "pointer_type"
            | "function_type"
    )
}

/// One AST node, parallel to the token at the same index in the normalized sequence
/// (`tokens[i]` ⇔ `spans[i]`). Pre-order; an internal node's span covers its WHOLE subtree,
/// a leaf's span is the leaf. Byte offsets are ABSOLUTE file offsets (node.start_byte/end_byte).
#[derive(Clone, Debug)]
pub(crate) struct NodeSpan {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    /// `node.kind()` returns a `&'static str` from the grammar — stored directly, no allocation.
    pub(crate) kind: &'static str,
    /// `true` iff `node.child_count() == 0`.
    pub(crate) is_leaf: bool,
}

/// Spanned baseline normalization: identical token stream to `normalize_baseline`, PLUS one
/// `NodeSpan` per token (same index). Single source of truth for the walk.
///
/// The returned vecs are always the same length: `tokens[i]` ↔ `spans[i]`.
/// Internal-node tokens carry the span of the whole subtree (pre-order push before recursing);
/// leaf tokens carry the span of the leaf node itself.
pub(crate) fn normalize_baseline_spanned(
    node: Node<'_>,
    text: &str,
) -> (Vec<String>, Vec<NodeSpan>) {
    let mut tokens = Vec::new();
    let mut spans = Vec::new();
    let mut idents: HashMap<String, usize> = HashMap::new();
    walk_spanned(node, text.as_bytes(), &mut idents, &mut tokens, &mut spans);
    (tokens, spans)
}

/// Baseline normalization (#215 §4): pre-order walk of the symbol subtree emitting one token per
/// node — structural node kinds for internal nodes; for leaves, an alpha-renamed `ID<n>` for
/// identifiers (numbered by first occurrence), a `LIT_<KIND>` bucket for literals, and the verbatim
/// text for operators/keywords/punctuation. Scope-independent: depends only on this node's subtree.
pub(crate) fn normalize_baseline(node: Node<'_>, text: &str) -> Vec<String> {
    normalize_baseline_spanned(node, text).0
}

fn walk_spanned(
    node: Node<'_>,
    src: &[u8],
    idents: &mut HashMap<String, usize>,
    tokens: &mut Vec<String>,
    spans: &mut Vec<NodeSpan>,
) {
    let kind = node.kind();
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();

    if node.child_count() == 0 {
        // Leaf: push the token and its span.
        let leaf = node.utf8_text(src).unwrap_or("");
        let token = if is_identifier_kind(kind) {
            let next = idents.len();
            let id = *idents.entry(leaf.to_string()).or_insert(next);
            format!("ID{id}")
        } else if is_literal_kind(kind) {
            format!("LIT_{}", kind.to_ascii_uppercase())
        } else {
            // keyword / operator / punctuation — structural, kept verbatim
            leaf.to_string()
        };
        tokens.push(token);
        spans.push(NodeSpan { start_byte, end_byte, kind, is_leaf: true });
        return;
    }

    // Internal node: push kind token + span (whole subtree) BEFORE recursing (pre-order).
    tokens.push(kind.to_string());
    spans.push(NodeSpan { start_byte, end_byte, kind, is_leaf: false });

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_spanned(child, src, idents, tokens, spans);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::index::parser;
    use crate::language::Language;

    /// Pick the target symbol's AST node for a normalization test: the symbol whose subtree
    /// normalizes to the MOST tokens (the actual body under test, language-agnostic). Choosing by
    /// token count instead of `kind == "function"` is what lets the same harness drive Rust
    /// `function`, TS `const`/function-valued declarators, and Python `function` symbols — the
    /// languages disagree on the symbol `kind` string but agree that the biggest normalized subtree
    /// is the body we want to compare.
    fn target_node_for<'a>(parsed: &'a parser::ParsedFile, src: &str) -> tree_sitter::Node<'a> {
        parsed
            .symbols
            .iter()
            .filter_map(|s| {
                let node = parsed.root().descendant_for_byte_range(s.start_byte, s.end_byte)?;
                let len = normalize_baseline(node, src).len();
                Some((len, node))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, node)| node)
            .expect("at least one symbol with a normalizable subtree")
    }

    /// Generalized normalize harness: parse `src` with `language` (under `path`, which selects the
    /// grammar / generated heuristics), select the target symbol, return its normalized stream.
    fn norm_lang(src: &str, path: &str, language: Language) -> Vec<String> {
        let parsed = parser::parse_file(Path::new(path), language, src).expect("parse");
        normalize_baseline(target_node_for(&parsed, src), src)
    }

    /// Generalized spanned-normalize harness (token stream + parallel `NodeSpan`s).
    fn spanned_lang(src: &str, path: &str, language: Language) -> (Vec<String>, Vec<NodeSpan>) {
        let parsed = parser::parse_file(Path::new(path), language, src).expect("parse");
        normalize_baseline_spanned(target_node_for(&parsed, src), src)
    }

    /// Rust-only wrappers — the existing Rust tests call these unchanged.
    fn norm(src: &str) -> Vec<String> {
        norm_lang(src, "t.rs", Language::Rust)
    }

    fn spanned(src: &str) -> (Vec<String>, Vec<NodeSpan>) {
        spanned_lang(src, "t.rs", Language::Rust)
    }

    // ── Task 1 (#232): multi-language harness smoke test
    // ───────────────────────────────────────

    /// The generalized harness parses TypeScript and Python (not just Rust) and returns a non-empty
    /// normalized stream carrying the function-body tokens. Pure harness smoke test — proves the
    /// grammar selection + target-symbol picker work cross-language before T2/T3/T5 lean on them.
    #[test]
    fn harness_parses_ts_and_python() {
        let ts = norm_lang(
            "function f() { const a = get(1); const b = get(2); return a + b; }",
            "t.ts",
            Language::TypeScript,
        );
        assert!(!ts.is_empty(), "TS normalize stream must be non-empty");
        assert!(ts.iter().any(|t| t == "return_statement"), "TS stream missing body: {ts:?}");

        let py = norm_lang(
            "def f():\n    a = get(1)\n    b = get(2)\n    return a + b\n",
            "t.py",
            Language::Python,
        );
        assert!(!py.is_empty(), "Python normalize stream must be non-empty");
        assert!(py.iter().any(|t| t == "return_statement"), "Python stream missing body: {py:?}");
    }

    // ── Original tests (must stay green)
    // ──────────────────────────────────────────────────────────

    #[test]
    fn renamed_identical_bodies_normalize_equal() {
        let a = "fn load_user(db: Db) -> i32 { let u = db.get(10); u + 1 }";
        let b = "fn load_order(store: Db) -> i32 { let o = store.get(10); o + 1 }";
        assert_eq!(norm(a), norm(b));
    }

    #[test]
    fn structurally_different_bodies_differ() {
        let a = "fn f(db: Db) -> i32 { let u = db.get(10); u + 1 }";
        let b = "fn g(db: Db) -> i32 { while true { } 0 }";
        assert_ne!(norm(a), norm(b));
    }

    #[test]
    fn literals_are_bucketed_not_compared_by_value() {
        let a = "fn f() -> i32 { let x = 10; x }";
        let b = "fn g() -> i32 { let y = 99999; y }";
        assert_eq!(norm(a), norm(b));
    }

    // ── Task 5a: spanned normalize
    // ────────────────────────────────────────────────────────────────

    /// For a diverse set of fixtures: the spanned `.0` equals `normalize_baseline` output.
    /// Also cross-checks against the expected token vecs from the original tests, so the stream is
    /// byte-identical (struct_hash faithfulness preserved).
    #[test]
    fn spanned_tokens_equal_baseline_across_corpus() {
        let fixtures = [
            // from existing tests
            "fn load_user(db: Db) -> i32 { let u = db.get(10); u + 1 }",
            "fn load_order(store: Db) -> i32 { let o = store.get(10); o + 1 }",
            "fn f(db: Db) -> i32 { let u = db.get(10); u + 1 }",
            "fn g(db: Db) -> i32 { while true { } 0 }",
            "fn f() -> i32 { let x = 10; x }",
            "fn g() -> i32 { let y = 99999; y }",
            // additional diverse fixtures
            "fn add(a: i32, b: i32) -> i32 { a + b }",
            "fn renamed(x: i32, y: i32) -> i32 { x + y }",
            "fn nested(v: Vec<i32>) -> i32 { v.iter().map(|x| x + 1).sum() }",
            r#"fn greet(name: &str) -> String { format!("hello {}", name) }"#,
            "fn check(x: i32) -> bool { if x > 0 { true } else { false } }",
            "fn pick(n: i32) -> i32 { match n { 0 => 1, _ => n } }",
        ];
        for src in &fixtures {
            let baseline = norm(src);
            let (spanned_tokens, _spans) = spanned(src);
            assert_eq!(spanned_tokens, baseline, "token stream mismatch for: {src}");
        }
        // Cross-check that `renamed_identical_bodies_normalize_equal` still holds through spanned:
        let (ta, _) = spanned("fn load_user(db: Db) -> i32 { let u = db.get(10); u + 1 }");
        let (tb, _) = spanned("fn load_order(store: Db) -> i32 { let o = store.get(10); o + 1 }");
        assert_eq!(ta, tb, "renamed clones must still produce equal spanned token streams");

        // Literals bucketed:
        let (tf, _) = spanned("fn f() -> i32 { let x = 10; x }");
        let (tg, _) = spanned("fn g() -> i32 { let y = 99999; y }");
        assert_eq!(tf, tg, "literal-valued clones must still produce equal spanned token streams");
    }

    /// For every fixture: `tokens.len() == spans.len()` (bijection invariant).
    #[test]
    fn spans_len_equals_tokens_len() {
        let fixtures = [
            "fn load_user(db: Db) -> i32 { let u = db.get(10); u + 1 }",
            "fn f() -> i32 { let x = 10; x }",
            "fn add(a: i32, b: i32) -> i32 { a + b }",
            "fn nested(v: Vec<i32>) -> i32 { v.iter().map(|x| x + 1).sum() }",
            "fn check(x: i32) -> bool { if x > 0 { true } else { false } }",
            "fn pick(n: i32) -> i32 { match n { 0 => 1, _ => n } }",
        ];
        for src in &fixtures {
            let (tokens, spans) = spanned(src);
            assert_eq!(tokens.len(), spans.len(), "bijection broken for: {src}");
        }
    }

    /// For a concrete fixture: leaf spans recover the real identifier text, and internal-node spans
    /// cover the whole subtree source text.
    #[test]
    fn span_slices_recover_real_source() {
        let src = "fn load_user(db: Db) -> i32 { let u = db.get(10); u + 1 }";
        let (tokens, spans) = spanned(src);
        assert_eq!(tokens.len(), spans.len());

        // Find a leaf token that is an identifier (ID<n>) and confirm its span returns real source.
        let id_idx =
            tokens.iter().position(|t| t.starts_with("ID")).expect("at least one ID token");
        let sp = &spans[id_idx];
        assert!(sp.is_leaf, "identifier span must be a leaf");
        let slice = src.get(sp.start_byte..sp.end_byte).expect("valid byte range");
        // Must be a real identifier from the source, not empty.
        assert!(!slice.is_empty(), "identifier span slice must not be empty");
        // Known identifiers in this source:
        let known_idents = ["load_user", "db", "Db", "i32", "u", "get", "load_order", "store"];
        assert!(
            known_idents.contains(&slice),
            "leaf identifier span should be a real source identifier, got: {slice:?}"
        );

        // Find an internal-node token (a non-leaf span) and confirm it covers a real subtree.
        let internal_idx =
            spans.iter().position(|s| !s.is_leaf).expect("at least one internal node");
        let sp_internal = &spans[internal_idx];
        assert!(!sp_internal.is_leaf);
        let subtree_src = src
            .get(sp_internal.start_byte..sp_internal.end_byte)
            .expect("valid byte range for internal node");
        assert!(!subtree_src.is_empty(), "internal node span must not be empty");
        // The first internal token for a function_item covers the whole function.
        // It must contain the function name somewhere.
        assert!(
            subtree_src.contains("load_user"),
            "outermost internal node should span the function body, got: {subtree_src:?}"
        );

        // Find the "db.get(10)" call_expression if present and check its span covers that text.
        // token for call_expression will be the kind string, find it by kind in spans.
        if let Some(call_idx) = spans.iter().position(|s| !s.is_leaf && s.kind == "call_expression")
        {
            let call_sp = &spans[call_idx];
            let call_src = src
                .get(call_sp.start_byte..call_sp.end_byte)
                .expect("valid byte range for call_expression");
            assert!(
                call_src.contains("get"),
                "call_expression span should cover 'get', got: {call_src:?}"
            );
        }
    }
}
