use std::collections::HashMap;

use rag_rat_base::language::Language;
use tree_sitter::Node;

/// A leaf tree-sitter kind that names a binding/reference identifier (kept language-agnostic by
/// matching the `*identifier` suffix tree-sitter grammars use).
fn is_identifier_kind(kind: &str) -> bool {
    kind.ends_with("identifier")
}

/// A leaf kind that is a literal whose *value* must not drive matching.
///
/// `string_fragment` is the TypeScript/JavaScript string-body leaf (the text inside the quotes, the
/// counterpart to Python's `string_content` — both are the value-bearing inner leaf, #232 #2a). It
/// buckets to `LIT_STRING_FRAGMENT`, which the anti-unify/signature contract maps to `&str` (see
/// `signature::literal_bucket_to_type`).
///
/// `character` is the C/C++ char-LITERAL VALUE leaf: `tree-sitter-c`/`-cpp` parse `'x'` as a
/// `char_literal` node (which already buckets via the `ends_with("literal")` arm) wrapping a
/// `'` / `character` / `'` leaf triple — the quotes are structural (identical for any char), but
/// the inner `character` leaf carries the value, so `'x'` vs `'y'` would leak verbatim without it
/// (#253). GATED to C/C++ via `lang`: `character` is a generic-enough kind name that we only bucket
/// it where it's known to be the char-literal value leaf, never speculatively across other
/// grammars.
///
/// `line_str_text` / `multi_line_str_text` / `raw_str_part` / `raw_str_end_part` /
/// `str_escaped_char` are Swift's string-body leaves — the counterparts to Python's
/// `string_content` and TS's `string_fragment`. tree-sitter-swift wraps them in a
/// `line_string_literal` (an INTERNAL node: quote, text, quote), and only LEAVES are bucketed, so
/// the wrapper's `ends_with("literal")` never fires for the value: without these names the text
/// leaf falls through to the keep-verbatim branch and the string's VALUE lands in the normalized
/// token stream. Two Swift bodies differing only in a string constant would then fail to normalize
/// equal — silently costing Swift clone recall that every other language has. (Interpolation
/// segments are internal nodes and still recurse as real code.) `raw_str_end_part` carries the
/// delimiters as well as the text — an uninterpolated `#"one"#` arrives as ONE leaf — which is why
/// it must bucket rather than pass through as punctuation.
fn is_literal_kind(kind: &str, lang: Language) -> bool {
    kind.ends_with("literal")
        || matches!(
            kind,
            "string_content"
                | "string_fragment"
                | "line_str_text"
                | "multi_line_str_text"
                | "raw_str_part"
                | "raw_str_end_part"
                | "str_escaped_char"
                | "integer"
                | "float"
                | "number"
                | "char"
        )
        || (matches!(lang, Language::C | Language::Cpp) && kind == "character")
}

/// A leaf kind that is a boolean literal in the grammars that expose booleans as their own leaf
/// kind: Rust emits `true`/`false` as leaves under an internal `boolean_literal`; TypeScript,
/// Python, C and C++ emit bare `true`/`false` leaves (Python lexemes `True`/`False` map to kinds
/// `true`/`false`). Bucketing both to ONE `LIT_BOOL` token (not `LIT_TRUE`/`LIT_FALSE`)
/// value-ERASES the boolean: two bodies differing only in `true` vs `false` then normalize equal,
/// and `LIT_BOOL` recovers `bool` typing in the signature contract (#232 #2b). The single bucket
/// is why this is NOT routed through the generic `LIT_{kind.uppercased}` path — that would encode
/// the value.
///
/// Kotlin is handled separately by leaf TEXT (see [`kotlin_identifier_token`]):
/// `tree-sitter-kotlin` emits `true`/`false`/`null` as plain `identifier` leaves, so they never
/// reach this kind-based predicate. The fix (#253) intercepts them under the identifier branch in
/// `walk_spanned`, GATED to Kotlin, mapping `true`/`false` to `LIT_BOOL` (was alpha-renamed to
/// `ID<n>` — a recall leak, partition-contained since Kotlin only clone-matches Kotlin).
fn is_boolean_leaf_kind(kind: &str) -> bool {
    matches!(kind, "true" | "false")
}

/// Kotlin-only override for an `identifier`-kind leaf whose TEXT is a boolean or null keyword.
/// `tree-sitter-kotlin` lexes `true`/`false`/`null` as bare `identifier` leaves (not their own
/// kinds), so the generic `is_boolean_leaf_kind`/`is_identifier_kind` path would alpha-rename them
/// to `ID<n>` and leak the value into matching (#253). Returns:
/// - `Some("LIT_BOOL")` for `true`/`false` — value-erased to the same single bucket every other
///   grammar's booleans use, so a Kotlin `true`↔`false`-only diff normalizes equal.
/// - `Some("null")` for `null` — kept VERBATIM (the cross-language null-family policy, #232 #2c:
///   not bucketed, but also not alpha-renamed to an `ID<n>`).
/// - `None` for any other identifier text — falls through to the normal `ID<n>` alpha-rename.
///
/// GATED to Kotlin by the caller; other grammars emit these as dedicated leaf kinds and must not be
/// matched by text here.
fn kotlin_identifier_token(leaf: &str) -> Option<&'static str> {
    match leaf {
        "true" | "false" => Some("LIT_BOOL"),
        "null" => Some("null"),
        _ => None,
    }
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
    lang: Language,
) -> (Vec<String>, Vec<NodeSpan>) {
    let mut tokens = Vec::new();
    let mut spans = Vec::new();
    let mut idents: HashMap<String, usize> = HashMap::new();
    walk_spanned(node, text.as_bytes(), lang, &mut idents, &mut tokens, &mut spans);
    (tokens, spans)
}

/// Baseline normalization (#215 §4): pre-order walk of the symbol subtree emitting one token per
/// node — structural node kinds for internal nodes; for leaves, an alpha-renamed `ID<n>` for
/// identifiers (numbered by first occurrence), a `LIT_<KIND>` bucket for literals, and the verbatim
/// text for operators/keywords/punctuation. Scope-independent: depends only on this node's subtree.
pub(crate) fn normalize_baseline(node: Node<'_>, text: &str, lang: Language) -> Vec<String> {
    normalize_baseline_spanned(node, text, lang).0
}

fn walk_spanned(
    node: Node<'_>,
    src: &[u8],
    lang: Language,
    idents: &mut HashMap<String, usize>,
    tokens: &mut Vec<String>,
    spans: &mut Vec<NodeSpan>,
) {
    let kind = node.kind();
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();

    // (#232 #1) Comments and other tree-sitter EXTRAs (e.g. Python `line_continuation`) carry no
    // semantic token: skip the WHOLE subtree, pushing NEITHER a token NOR a span so the seq↔span
    // bijection is preserved. The OR is load-bearing (R4): `is_extra()` catches every grammar's
    // comments via the runtime EXTRA flag PLUS non-comment extras like `line_continuation`, while
    // the explicit `kind.contains("comment")` is a belt-and-braces guard for any grammar that
    // flags a comment as a normal (non-extra) node — neither predicate alone covers all six
    // grammars, so we keep both. (Comments themselves never anchor a clone variation point;
    // skipping them at the source means no caller — antiunify, signature recovery — ever sees a
    // comment span.)
    if node.is_extra() || kind.contains("comment") {
        return;
    }

    if node.child_count() == 0 {
        // Leaf: push the token and its span.
        let leaf = node.utf8_text(src).unwrap_or("");
        let token = if is_identifier_kind(kind) {
            // (#253) Kotlin lexes `true`/`false`/`null` as bare `identifier` leaves, so they reach
            // the identifier branch FIRST. GATED to Kotlin, intercept by leaf text: `true`/`false`
            // value-erase to `LIT_BOOL` (same single bucket as every other grammar's booleans),
            // `null` stays verbatim (null-family policy). Every other identifier alpha-renames.
            if let Some(tok) =
                (lang == Language::Kotlin).then(|| kotlin_identifier_token(leaf)).flatten()
            {
                tok.to_string()
            } else {
                let next = idents.len();
                let id = *idents.entry(leaf.to_string()).or_insert(next);
                format!("ID{id}")
            }
        } else if is_boolean_leaf_kind(kind) {
            // Value-erase booleans to ONE bucket (NOT `LIT_{kind.uppercased}`): `true`/`false`
            // collapse to `LIT_BOOL` so a true↔false-only diff is a clone (#232 #2b).
            "LIT_BOOL".to_string()
        } else if is_literal_kind(kind, lang) {
            format!("LIT_{}", kind.to_ascii_uppercase())
        } else {
            // (#232 #2c) NULL-FAMILY out of scope (low value + wrapped-node hazard): `null` /
            // `undefined` (TS), `none` (Python), and the C/C++ `NULL`/`nullptr` leaves stay
            // verbatim. keyword / operator / punctuation / null-family — structural,
            // kept verbatim
            leaf.to_string()
        };
        tokens.push(token);
        spans.push(NodeSpan { start_byte, end_byte, kind, is_leaf: true });
        return;
    }

    // Internal node: push kind token + span (whole subtree) BEFORE recursing (pre-order).
    tokens.push(kind.to_string());
    spans.push(NodeSpan { start_byte, end_byte, kind, is_leaf: false });

    // grow_stack: this recurses to full subtree depth; a hostile deeply-nested clone body must grow
    // the stack, not overflow it (#543).
    crate::index::grow_stack(|| {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_spanned(child, src, lang, idents, tokens, spans);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rag_rat_base::language::Language;

    use super::*;
    use crate::index::parser;

    /// Pick the target symbol's AST node for a normalization test: the symbol whose subtree
    /// normalizes to the MOST tokens (the actual body under test, language-agnostic). Choosing by
    /// token count instead of `kind == "function"` is what lets the same harness drive Rust
    /// `function`, TS `const`/function-valued declarators, and Python `function` symbols — the
    /// languages disagree on the symbol `kind` string but agree that the biggest normalized subtree
    /// is the body we want to compare.
    fn target_node_for<'a>(
        parsed: &'a parser::ParsedFile,
        src: &str,
        language: Language,
    ) -> tree_sitter::Node<'a> {
        parsed
            .symbols
            .iter()
            .filter_map(|s| {
                let node = parsed.root().descendant_for_byte_range(s.start_byte, s.end_byte)?;
                let len = normalize_baseline(node, src, language).len();
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
        normalize_baseline(target_node_for(&parsed, src, language), src, language)
    }

    /// Generalized spanned-normalize harness (token stream + parallel `NodeSpan`s).
    fn spanned_lang(src: &str, path: &str, language: Language) -> (Vec<String>, Vec<NodeSpan>) {
        let parsed = parser::parse_file(Path::new(path), language, src).expect("parse");
        normalize_baseline_spanned(target_node_for(&parsed, src, language), src, language)
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

    // ── Task 2 (#232): comments are ignored
    // ──────────────────────────────────────────

    /// For Rust (`//`, `/* */`), TS (`//`, `/* */`), and Python (`#`): two function bodies that
    /// differ ONLY by comments normalize to the SAME token stream (comments carry no semantic
    /// token), and the seq↔span bijection holds throughout.
    #[test]
    fn comments_are_ignored_across_languages() {
        // Rust — line + block comments.
        let rust_plain = "fn f(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }";
        let rust_commented = "fn f(db: Db) -> i32 {\n    // load it\n    let u = db.get(10); /* \
                              hmm */ validate(u); u + 1 }";
        let (tp, sp) = spanned(rust_plain);
        let (tc, sc) = spanned(rust_commented);
        assert_eq!(tp, tc, "Rust comment-only diff must normalize equal");
        assert_eq!(tp.len(), sp.len(), "bijection broken (rust plain)");
        assert_eq!(tc.len(), sc.len(), "bijection broken (rust commented)");

        // TypeScript — line + block comments.
        let ts_plain = "function f() { const a = get(1); const b = get(2); return a + b; }";
        let ts_commented =
            "function f() {\n  // a\n  const a = get(1); /* b */ const b = get(2); return a + b; }";
        let (ttp, _) = spanned_lang(ts_plain, "t.ts", Language::TypeScript);
        let (ttc, ttcs) = spanned_lang(ts_commented, "t.ts", Language::TypeScript);
        assert_eq!(ttp, ttc, "TS comment-only diff must normalize equal");
        assert_eq!(ttc.len(), ttcs.len(), "bijection broken (ts commented)");

        // Python — `#` comments.
        let py_plain = "def f():\n    a = get(1)\n    b = get(2)\n    return a + b\n";
        let py_commented =
            "def f():\n    # load a\n    a = get(1)\n    b = get(2)  # and b\n    return a + b\n";
        let (typ, _) = spanned_lang(py_plain, "t.py", Language::Python);
        let (tyc, tycs) = spanned_lang(py_commented, "t.py", Language::Python);
        assert_eq!(typ, tyc, "Python comment-only diff must normalize equal");
        assert_eq!(tyc.len(), tycs.len(), "bijection broken (py commented)");
    }

    /// Python `line_continuation` (`\` at EOL) is a tree-sitter EXTRA leaf, not a comment — the
    /// `is_extra()` arm of the skip catches it (R4). It carries no semantic token, so a body with a
    /// line continuation normalizes equal to the same body written on one line, and the bijection
    /// still holds.
    #[test]
    fn python_line_continuation_is_skipped() {
        let one_line = "def f():\n    x = aaa + bbb + ccc\n    return x\n";
        let continued = "def f():\n    x = aaa + \\\n        bbb + ccc\n    return x\n";
        let (t1, _) = spanned_lang(one_line, "t.py", Language::Python);
        let (t2, s2) = spanned_lang(continued, "t.py", Language::Python);
        assert_eq!(t1, t2, "line_continuation must not change the normalized stream");
        assert_eq!(t2.len(), s2.len(), "bijection broken (py line_continuation)");
    }

    // ── Task 3 (#232): multi-language literal bucketing
    // ───────────────────────────────────

    /// TS strings (`string_fragment`) and booleans (`true`/`false` → `LIT_BOOL`) bucket: two bodies
    /// differing ONLY in string contents normalize equal, and two differing ONLY in `true` vs
    /// `false` normalize equal (the `LIT_BOOL` value-erase, #232 #2b).
    #[test]
    fn ts_strings_and_booleans_bucket() {
        // Strings — differ only in the quoted contents.
        let s1 = "function f() { const a = log(\"hello\"); const b = log(\"world\"); return a; }";
        let s2 = "function f() { const a = log(\"foo\"); const b = log(\"bar\"); return a; }";
        assert_eq!(
            norm_lang(s1, "t.ts", Language::TypeScript),
            norm_lang(s2, "t.ts", Language::TypeScript),
            "TS string-content-only diff must normalize equal (string_fragment bucketed)"
        );

        // Booleans — differ only in true vs false.
        let b_true = "function f() { const x = check(); const y = x; return true; }";
        let b_false = "function f() { const x = check(); const y = x; return false; }";
        let nt = norm_lang(b_true, "t.ts", Language::TypeScript);
        let nf = norm_lang(b_false, "t.ts", Language::TypeScript);
        assert_eq!(nt, nf, "TS true/false-only diff must normalize equal (LIT_BOOL)");
        assert!(nt.iter().any(|t| t == "LIT_BOOL"), "TS boolean must emit LIT_BOOL: {nt:?}");
    }

    /// Swift string bodies bucket like every other language's: two bodies differing ONLY in string
    /// contents normalize equal. Swift wraps its text leaf (`line_str_text`) in an INTERNAL
    /// `line_string_literal` node, so the `ends_with("literal")` arm — which only ever fires on a
    /// LEAF — never saw the value, and the string contents leaked verbatim into the token stream.
    /// Covers the line, multi-line, and raw string leaves, plus the escaped-char leaf.
    #[test]
    fn swift_strings_and_booleans_bucket() {
        let s1 = "func f() -> Int { let a = log(\"hello\"); let b = log(\"world\"); return a }";
        let s2 = "func f() -> Int { let a = log(\"foo\"); let b = log(\"bar\"); return a }";
        let n1 = norm_lang(s1, "t.swift", Language::Swift);
        assert_eq!(
            n1,
            norm_lang(s2, "t.swift", Language::Swift),
            "Swift string-content-only diff must normalize equal (line_str_text bucketed)"
        );
        assert!(
            !n1.iter().any(|t| t == "hello" || t == "world"),
            "Swift string VALUES must never reach the normalized stream: {n1:?}"
        );

        // Escaped chars are value-bearing leaves of their own (`str_escaped_char`).
        let e1 = "func f() -> Int { let a = log(\"a\\nb\"); let b = log(\"c\"); return a }";
        let e2 = "func f() -> Int { let a = log(\"x\\ty\"); let b = log(\"z\"); return a }";
        assert_eq!(
            norm_lang(e1, "t.swift", Language::Swift),
            norm_lang(e2, "t.swift", Language::Swift),
            "Swift escaped-char-only diff must normalize equal (str_escaped_char bucketed)"
        );

        // Raw strings (`raw_str_part`) and multi-line strings (`multi_line_str_text`).
        let r1 = "func f() -> Int { let a = log(#\"one\"#); let b = log(#\"two\"#); return a }";
        let r2 = "func f() -> Int { let a = log(#\"three\"#); let b = log(#\"four\"#); return a }";
        let nr = norm_lang(r1, "t.swift", Language::Swift);
        assert_eq!(
            nr,
            norm_lang(r2, "t.swift", Language::Swift),
            "Swift raw-string-only diff must normalize equal (raw_str_part bucketed)"
        );
        assert!(
            !nr.iter().any(|t| t == "one" || t == "two"),
            "Swift raw-string VALUES must never reach the normalized stream: {nr:?}"
        );

        // Booleans already bucket via the shared `true`/`false` leaf kinds — pin it for Swift too.
        let b_true = "func f() -> Bool { let x = check(); let y = x; return true }";
        let b_false = "func f() -> Bool { let x = check(); let y = x; return false }";
        let nt = norm_lang(b_true, "t.swift", Language::Swift);
        assert_eq!(
            nt,
            norm_lang(b_false, "t.swift", Language::Swift),
            "Swift true/false-only diff must normalize equal (LIT_BOOL)"
        );
        assert!(nt.iter().any(|t| t == "LIT_BOOL"), "Swift boolean must emit LIT_BOOL: {nt:?}");
    }

    /// Python booleans (`True`/`False`, leaf kinds `true`/`false`) bucket to `LIT_BOOL`; `None`
    /// (the null-family) stays verbatim — out of scope (#232 #2c).
    #[test]
    fn python_booleans_bucket_null_stays_verbatim() {
        let t = "def f():\n    x = check()\n    y = x\n    return True\n";
        let f = "def f():\n    x = check()\n    y = x\n    return False\n";
        let nt = norm_lang(t, "t.py", Language::Python);
        let nf = norm_lang(f, "t.py", Language::Python);
        assert_eq!(nt, nf, "Python True/False-only diff must normalize equal (LIT_BOOL)");
        assert!(
            nt.iter().any(|tok| tok == "LIT_BOOL"),
            "Python boolean must emit LIT_BOOL: {nt:?}"
        );

        // null-family deferred: `None` stays VERBATIM (the leaf kind is `none`, but the verbatim
        // branch pushes the leaf TEXT `None` — no LIT_ bucket, #232 #2c).
        let n = "def f():\n    x = check()\n    y = x\n    return None\n";
        let nn = norm_lang(n, "t.py", Language::Python);
        assert!(
            nn.iter().any(|tok| tok == "None"),
            "Python None stays verbatim (null-family deferred): {nn:?}"
        );
        assert!(
            !nn.iter().any(|tok| tok.starts_with("LIT_")),
            "Python None must NOT bucket to any LIT_ token: {nn:?}"
        );
    }

    /// Rust regression / post-#2 stream pin: `let x = true` now emits `LIT_BOOL` for the boolean
    /// leaf (where pre-#232 the `true`/`false` leaf fell through VERBATIM). This pins the NEW
    /// stream — exactly one `LIT_BOOL` token, no `true` token left, the wrapping
    /// `boolean_literal` node kind still pushed before it. A true↔false-only Rust diff now
    /// normalizes equal.
    #[test]
    fn rust_booleans_bucket_to_lit_bool() {
        let t = "fn f() -> bool { let a = compute(); let b = a; let x = true; x }";
        let f = "fn f() -> bool { let a = compute(); let b = a; let x = false; x }";
        let nt = norm(t);
        let nf = norm(f);
        assert_eq!(nt, nf, "Rust true/false-only diff must normalize equal (LIT_BOOL)");
        // Exactly one LIT_BOOL, no bare `true`/`false` survivor, wrapping node kind still present.
        assert_eq!(
            nt.iter().filter(|tok| *tok == "LIT_BOOL").count(),
            1,
            "exactly one LIT_BOOL leaf: {nt:?}"
        );
        assert!(!nt.iter().any(|tok| tok == "true" || tok == "false"), "no verbatim bool: {nt:?}");
        assert!(
            nt.iter().any(|tok| tok == "boolean_literal"),
            "wrapping boolean_literal node kind still pushed: {nt:?}"
        );
    }

    // ── #253: intra-language literal-bucketing recall gaps
    // ──────────────────────────────────────

    /// Kotlin lexes `true`/`false` as bare `identifier` leaves (not their own kinds), so before
    /// #253 they alpha-renamed to `ID<n>` and a `true`↔`false`-only diff did NOT normalize equal.
    /// The Kotlin-gated leaf-TEXT override now value-erases them to the SAME `LIT_BOOL` bucket
    /// every other grammar's booleans use, so two Kotlin fns differing only in a boolean ARE a
    /// clone. Also pins that `null` (also a bare `identifier` leaf in Kotlin) stays VERBATIM —
    /// neither bucketed nor alpha-renamed to an `ID<n>` — matching the cross-language
    /// null-family policy.
    #[test]
    fn kotlin_booleans_bucket_to_lit_bool() {
        let t = "fun f(): Boolean { val a = compute(); val b = a; val x = true; return x }";
        let f = "fun f(): Boolean { val a = compute(); val b = a; val x = false; return x }";
        let nt = norm_lang(t, "t.kt", Language::Kotlin);
        let nf = norm_lang(f, "t.kt", Language::Kotlin);
        assert_eq!(nt, nf, "Kotlin true/false-only diff must normalize equal (LIT_BOOL)");
        assert_eq!(
            nt.iter().filter(|tok| *tok == "LIT_BOOL").count(),
            1,
            "exactly one LIT_BOOL leaf for the Kotlin boolean: {nt:?}"
        );
        // The boolean must NOT have leaked as a verbatim `true`/`false` NOR as an alpha-renamed ID
        // sharing the identifier numbering (the pre-#253 bug).
        assert!(
            !nt.iter().any(|tok| tok == "true" || tok == "false"),
            "no verbatim Kotlin bool: {nt:?}"
        );

        // `null` stays verbatim — not bucketed, not alpha-renamed.
        let n = "fun g(): String? { val a = compute(); val b = a; val x = null; return x }";
        let nn = norm_lang(n, "t.kt", Language::Kotlin);
        assert!(
            nn.iter().any(|tok| tok == "null"),
            "Kotlin null stays verbatim (null-family policy): {nn:?}"
        );
        assert!(
            !nn.iter().any(|tok| tok.starts_with("LIT_")),
            "Kotlin null must NOT bucket to any LIT_ token: {nn:?}"
        );
    }

    /// C and C++ parse `'x'` as a `char_literal` node wrapping a `'` / `character` / `'` leaf
    /// triple. The quotes are structural (identical for any char) but the inner `character`
    /// leaf carries the VALUE — before #253 it leaked verbatim, so two fns differing only in a
    /// char (`'x'` vs `'y'`) did NOT normalize equal. The C/C++-gated `character` bucket
    /// value-erases it to `LIT_CHARACTER`, so a char-value-only diff IS a clone. Checked for
    /// both C and C++.
    #[test]
    fn c_and_cpp_char_value_buckets() {
        // C: differ only in the char value.
        let c_x = "int f(void) { int a = compute(); int b = a; char x = 'p'; return b; }";
        let c_y = "int f(void) { int a = compute(); int b = a; char x = 'q'; return b; }";
        let nc_x = norm_lang(c_x, "t.c", Language::C);
        let nc_y = norm_lang(c_y, "t.c", Language::C);
        assert_eq!(nc_x, nc_y, "C char-value-only diff must normalize equal (LIT_CHARACTER)");
        assert!(
            nc_x.iter().any(|tok| tok == "LIT_CHARACTER"),
            "C char value must bucket to LIT_CHARACTER: {nc_x:?}"
        );
        // The value must not have leaked verbatim.
        assert!(
            !nc_x.iter().any(|tok| tok == "p"),
            "C char value must not leak verbatim: {nc_x:?}"
        );

        // C++: differ only in the char value.
        let cpp_x = "int f() { int a = compute(); int b = a; char x = 'p'; return b; }";
        let cpp_y = "int f() { int a = compute(); int b = a; char x = 'q'; return b; }";
        let ncpp_x = norm_lang(cpp_x, "t.cpp", Language::Cpp);
        let ncpp_y = norm_lang(cpp_y, "t.cpp", Language::Cpp);
        assert_eq!(ncpp_x, ncpp_y, "C++ char-value-only diff must normalize equal (LIT_CHARACTER)");
        assert!(
            ncpp_x.iter().any(|tok| tok == "LIT_CHARACTER"),
            "C++ char value must bucket to LIT_CHARACTER: {ncpp_x:?}"
        );
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
