//! Clone detection for the rag-rat workspace: scope-independent structural fingerprints
//! (computed from the engine's shared parse), token-bag postings, and the refine/antiunify
//! pipeline that upgrades candidate pairs into ranked clone classes. The engine supplies
//! parsed ASTs and symbol spans; this crate never parses or reads engine state upward.

//! Clone-detection fingerprint substrate (#215 Phase 1): a scope-independent structural
//! fingerprint per function symbol, computed during indexing.

// `NormalizerKind::Scip` + `from_db_str` are the Plan-3 SCIP token-space surface (a separate
// `normalizer_kind='scip'` postings space, used only at refine/ranking when every member has it).
// They are dead until Plan 3 lands; the rest of the module is live (R4's candidate read uses it).
#![allow(dead_code)]

pub mod bag_blob;
pub mod normalize;
pub mod refine;
pub mod tokens;

use tree_sitter::Node;

/// Bumped when normalization changes; invalidates fingerprints (and the later refine cache) without
/// a schema migration.
///
/// `2` (#232): the normalization stream changed in two ways that affect every fingerprint — (1)
/// comments and other tree-sitter EXTRAs are skipped (`walk_spanned`), and (2) multi-language
/// literal leaves bucket (TS `string_fragment` → `LIT_STRING_FRAGMENT`; `true`/`false` value-erase
/// to `LIT_BOOL`). The already-shipped read filter (`sf.normalizer_version = NORM_VERSION` on every
/// fingerprint read) auto-EXCLUDES the v1 rows, which are recomputed at v2 on the next reindex; the
/// 4b refinement cache invalidates via `NORM_VERSION` in the content-addressed `refinement_key` +
/// the freshness predicate (`refine::cache`). No schema migration is needed.
///
/// `3` (#253): two intra-language literal-bucketing recall gaps close, both partition-contained
/// (the language partition already blocks cross-language pairing, so these are MISSED-clone recall,
/// never false positives) — (1) Kotlin `true`/`false` (lexed as bare `identifier` leaves) now
/// value-erase to `LIT_BOOL` instead of alpha-renaming to `ID<n>`, and `null` stays verbatim
/// instead of alpha-renaming; (2) the C/C++ `char_literal` VALUE leaf (`character`) now buckets to
/// `LIT_CHARACTER` instead of leaking the char value verbatim. Same auto-exclude-then-recompute
/// path as the v2 bump (no schema migration); a bump forces re-fingerprinting on the next reindex.
///
/// `4` (#635): Swift's string-BODY leaves (`line_str_text`, `multi_line_str_text`, `raw_str_part`,
/// `raw_str_end_part`, `str_escaped_char`) now bucket instead of leaking their VALUE verbatim.
/// tree-sitter-swift wraps them in an internal `line_string_literal` node, and only LEAVES bucket,
/// so the `ends_with("literal")` arm never fired for the value — two Swift bodies differing only in
/// a string constant failed to normalize equal. Partition-contained like the v3 bump (missed-clone
/// recall, never false positives). Also widens the fingerprintable set: Swift closure properties
/// (`let f = { … }`) and `constructor` (`init`) bodies now fingerprint like every other function
/// body. Same auto-exclude-then-recompute path; the bump forces re-fingerprinting on the next
/// reindex.
pub const NORM_VERSION: i64 = 4;
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
///
/// `3` (#254 / #274 items 16): three string-hole widening fixes change the rendered template +
/// variation-point payload for affected classes — a TS/JS `template_string` (`` `hi` ``) now
/// widens to the whole backtick literal, an empty-`""`-vs-nonempty diff now widens its quote run to
/// the whole `string_literal` (no stray quote), and an interpolated `` `…${x}…` `` is left
/// un-widened (the `${x}` stays fixed). A class refined + cached at `alignment_version = 2` holds
/// the OLD (broken) template, so the bump invalidates those rows; they recompute with the corrected
/// payload on the next refine. Display-only — scoring/coverage SEMANTICS are unchanged; this is the
/// cache-freshness discipline, not a behavior change to the over-claim contract.
pub(crate) const ALIGNMENT_VERSION: i64 = 3;
/// Smallest normalized-token count a symbol must reach to be fingerprinted (skip trivial getters).
pub const MIN_TOKENS: usize = 20;

/// Which token space a fingerprint was computed in. Baseline is always present and is the only
/// input to candidate recall; Scip is an optional precision signal (Plan 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum NormalizerKind {
    Baseline,
    Scip,
}

impl NormalizerKind {
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        value.parse().ok()
    }
}

/// One symbol's baseline fingerprint, ready to persist.
#[derive(Debug, Clone)]
pub struct SymbolFingerprint {
    pub struct_hash: String,
    pub token_len: i64,
    /// `(token_hash, freq)` multiset sorted by `token_hash`. Serialized into the
    /// `symbol_fingerprints.token_bag` BLOB column (#231; `bag_blob::encode_token_bag`).
    pub token_bag: Vec<(i64, i64)>,
}

/// Baseline fingerprint for a symbol's AST node, or `None` if it normalizes below `MIN_TOKENS`.
/// `lang` selects the per-language normalization overrides (Kotlin boolean/null leaves, C/C++ char
/// value — #253); it must match the grammar that produced `node`.
pub(crate) fn fingerprint_symbol(
    node: Node<'_>,
    text: &str,
    lang: rag_rat_base::language::Language,
) -> Option<SymbolFingerprint> {
    let tokens = normalize::normalize_baseline(node, text, lang);
    if tokens.len() < MIN_TOKENS {
        return None;
    }
    Some(SymbolFingerprint {
        struct_hash: tokens::struct_hash(&tokens),
        token_len: tokens.len() as i64,
        token_bag: tokens::token_bag(&tokens),
    })
}

/// `true` when a symbol's AST node is a FUNCTION-VALUED declarator: a `variable_declarator`
/// (`const f = () => {…}`, `let f = function(){…}`) or `public_field_definition` (a class-field
/// arrow handler) whose `value` child is an `arrow_function` / `function_expression` (#232 #5), or
/// Swift's `property_declaration` whose `value` is a `lambda_literal`
/// (`let handler: (Int) -> Void = { … }`).
///
/// These carry symbol `kind = "const"` / `"property"` in `parser.rs` (NOT changed — `kind` drives
/// chunking, graph edges, and search facets; see the plan's invariant) but ARE real function bodies
/// worth fingerprinting: two `const x = () => {…}` clones match each other (same declarator shape).
/// The node-level check is what keeps a plain-value `const x = 5;` / `let plain = 5` excluded — its
/// `value` child is a `number` / `integer_literal`, not a function. Probe-confirmed against the
/// wired grammars: matches TS const-arrow, const-func-expr, let-async-arrow, class-field-arrow and
/// Swift closure properties; rejects destructure / number / object.
fn symbol_is_function_valued(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "variable_declarator" | "public_field_definition" | "property_declaration"
    ) && node.child_by_field_name("value").is_some_and(|v| {
        matches!(v.kind(), "arrow_function" | "function_expression" | "lambda_literal")
    })
}

/// Baseline fingerprints for a file's fingerprintable symbols, walking the SHARED parse tree (no
/// re-parse, no DB). Returns `(local_symbol_index, fingerprint)` pairs keyed by index into
/// `symbols`, so the caller maps each to the right DB id when it writes. A symbol is fingerprinted
/// when it is a `kind == "function"` symbol OR a function-valued declarator
/// ([`symbol_is_function_valued`], #232 #5); symbols that can't be located in the tree and bodies
/// that normalize below `MIN_TOKENS` are skipped. The full-rebuild prepare phase calls this from
/// the parse it already did for symbols/edges; the incremental path re-parses and calls it from
/// `store_symbol_fingerprints`.
/// Boundary view of one indexed symbol the engine wants fingerprinted (span + kind only).
pub struct FingerprintCandidate<'a> {
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: &'a str,
}

pub fn fingerprint_symbols(
    root: Node<'_>,
    text: &str,
    lang: rag_rat_base::language::Language,
    symbols: &[FingerprintCandidate<'_>],
) -> Vec<(usize, SymbolFingerprint)> {
    let mut out = Vec::new();
    for (i, symbol) in symbols.iter().enumerate() {
        let Some(node) = root.descendant_for_byte_range(symbol.start_byte, symbol.end_byte) else {
            continue;
        };
        // Fingerprint a `function` symbol, a `constructor` (Swift `init` — a real function body,
        // and the direct analog of a Rust `fn new()`, which fingerprints as a `function`),
        // OR a function-valued declarator (the node check rejects plain-value consts —
        // `const x = 5;` — so symbol `kind` stays unchanged, #232 #5 / R2).
        if !matches!(symbol.kind, "function" | "constructor") && !symbol_is_function_valued(node) {
            continue;
        }
        if let Some(fp) = fingerprint_symbol(node, text, lang) {
            out.push((i, fp));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rag_rat_base::language::Language;
    use rag_rat_core::index::{parser, symbols};

    use super::*;

    fn candidates_of(
        symbols: &[rag_rat_core::index::symbols::Symbol],
    ) -> Vec<FingerprintCandidate<'_>> {
        symbols
            .iter()
            .map(|s| FingerprintCandidate {
                start_byte: s.start_byte,
                end_byte: s.end_byte,
                kind: &s.kind,
            })
            .collect()
    }

    #[test]
    fn normalizer_kind_db_str_round_trips() {
        for (kind, token) in
            [(NormalizerKind::Baseline, "baseline"), (NormalizerKind::Scip, "scip")]
        {
            assert_eq!(kind.as_db_str(), token);
            assert_eq!(NormalizerKind::from_db_str(kind.as_db_str()), Some(kind));
        }
        assert_eq!(NormalizerKind::from_db_str("bogus"), None);
    }

    /// Generalized fingerprint harness (#232): parse `src` with `language` (under `path`), select
    /// the target symbol — the one whose subtree normalizes to the MOST tokens, language-agnostic
    /// so it works for Rust `function`, TS `const`/function-valued declarators, and Python
    /// `function` alike — and return its fingerprint (`None` below `MIN_TOKENS`). Test fixtures
    /// MUST clear `MIN_TOKENS` (20) or `fp_lang` returns `None` for the size gate, not the
    /// property under test.
    fn fp_lang(src: &str, path: &str, language: Language) -> Option<SymbolFingerprint> {
        let parsed = parser::parse_file(Path::new(path), language, src).expect("parse");
        let node = parsed
            .symbols
            .iter()
            .filter_map(|s| {
                let node = parsed.root().descendant_for_byte_range(s.start_byte, s.end_byte)?;
                let len = normalize::normalize_baseline(node, src, language).len();
                Some((len, node))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, node)| node)
            .expect("at least one symbol with a normalizable subtree");
        fingerprint_symbol(node, src, language)
    }

    /// Rust-only wrapper — the existing Rust fingerprint tests call this unchanged.
    fn fp(src: &str) -> Option<SymbolFingerprint> {
        fp_lang(src, "t.rs", Language::Rust)
    }

    #[test]
    fn norm_version_is_4() {
        // #635: Swift string-body leaves now bucket (their VALUE used to leak into the stream), and
        // Swift closure properties + `init` bodies joined the fingerprintable set — a stream change
        // on top of the #253 Kotlin/C++ bucketing that took it to 3. The read filter + content-
        // addressed refinement key both key off this constant, so a stream change without the bump
        // silently serves stale fingerprints/refinements.
        assert_eq!(NORM_VERSION, 4);
    }

    /// Swift closure-valued properties and `init` bodies are fingerprintable function bodies, and a
    /// plain-value property is not — the `value`-field check is what separates them.
    #[test]
    fn swift_closure_properties_and_inits_fingerprint_but_plain_values_do_not() {
        let closure = "struct S {\n  let handler: (Int) -> Void = { value in\n    let a = \
                       compute(value)\n    let b = transform(a)\n    let c = combine(a, b)\n    \
                       report(c)\n  }\n}\n";
        assert!(
            fp_lang(closure, "S.swift", Language::Swift).is_some(),
            "a Swift closure-valued property is a real function body"
        );

        let init = "struct S {\n  init(seed: Int) {\n    let a = compute(seed)\n    let b = \
                    transform(a)\n    let c = combine(a, b)\n    report(c)\n  }\n}\n";
        assert!(
            fp_lang(init, "S.swift", Language::Swift).is_some(),
            "a Swift init is a real function body (the analog of a Rust `fn new()`)"
        );

        // A plain-value property has an `integer_literal` value, not a closure: not
        // fingerprintable.
        let parsed = parser::parse_file(
            Path::new("S.swift"),
            Language::Swift,
            "struct S {\n  let plain = 5\n}\n",
        )
        .expect("parse");
        let symbols = symbols::from_parsed(&parsed.symbols);
        let fingerprints = fingerprint_symbols(
            parsed.root(),
            "struct S {\n  let plain = 5\n}\n",
            Language::Swift,
            &candidates_of(&symbols),
        );
        assert!(
            fingerprints.is_empty(),
            "a plain-value Swift property must not be fingerprinted: {fingerprints:?}"
        );
    }

    #[test]
    fn fp_lang_fingerprints_ts_and_python() {
        // Smoke test for the generalized fingerprint harness — both fixtures clear MIN_TOKENS (20).
        let ts = "function f() { const a = get(1); const b = get(2); const c = get(3); return a + \
                  b + c; }";
        assert!(fp_lang(ts, "t.ts", Language::TypeScript).is_some(), "TS fn must fingerprint");
        let py = "def f():\n    a = get(1)\n    b = get(2)\n    c = get(3)\n    return a + b + c\n";
        assert!(fp_lang(py, "t.py", Language::Python).is_some(), "Python fn must fingerprint");
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

    /// #253: two Kotlin fns differing ONLY in a boolean leaf fingerprint identically. Pre-#253 the
    /// `true`/`false` leaves alpha-renamed to `ID<n>` (and into the shared identifier numbering),
    /// so the struct_hash diverged and the clone was MISSED. The Kotlin-gated `LIT_BOOL`
    /// override makes them a clone. (Both bodies clear MIN_TOKENS.)
    #[test]
    fn kotlin_boolean_only_diff_is_a_clone() {
        let t = "fun f(): Boolean { val a = compute(); val b = check(a); val c = wrap(b); val x = \
                 true; return x }";
        let f = "fun f(): Boolean { val a = compute(); val b = check(a); val c = wrap(b); val x = \
                 false; return x }";
        let ft = fp_lang(t, "t.kt", Language::Kotlin).expect("true body fingerprinted");
        let ff = fp_lang(f, "t.kt", Language::Kotlin).expect("false body fingerprinted");
        assert_eq!(ft.struct_hash, ff.struct_hash, "Kotlin boolean-only diff must be a clone");
        assert_eq!(ft.token_bag, ff.token_bag);
    }

    /// #253: two C++ fns differing ONLY in a char value (`'p'` vs `'q'`) fingerprint identically.
    /// Pre-#253 the inner `character` leaf leaked the value verbatim, diverging the struct_hash and
    /// MISSING the clone; the C/C++-gated `LIT_CHARACTER` bucket value-erases it. (Both bodies
    /// clear MIN_TOKENS.) C and C++ share the `character` leaf; the C/C++ gate covers both.
    #[test]
    fn cpp_char_value_only_diff_is_a_clone() {
        let p = "int f() { int a = compute(); int b = check(a); int c = wrap(b); char x = 'p'; \
                 return c; }";
        let q = "int f() { int a = compute(); int b = check(a); int c = wrap(b); char x = 'q'; \
                 return c; }";
        let fp_p = fp_lang(p, "t.cpp", Language::Cpp).expect("'p' body fingerprinted");
        let fp_q = fp_lang(q, "t.cpp", Language::Cpp).expect("'q' body fingerprinted");
        assert_eq!(fp_p.struct_hash, fp_q.struct_hash, "C++ char-value-only diff must be a clone");
        assert_eq!(fp_p.token_bag, fp_q.token_bag);
    }

    // ── Task 5 (#232): function-valued declarators are fingerprinted (kind unchanged)
    // ─────────

    #[test]
    fn ts_function_valued_declarator_is_fingerprinted() {
        // A `const`-bound arrow function (>20 tokens, clears MIN_TOKENS) IS fingerprinted even
        // though its symbol `kind` is "const" (#232 #5).
        let arrow = "const load = (id) => { const row = get(id); const ok = check(row); return ok \
                     ? row : null; }";
        assert!(
            fp_lang(arrow, "t.ts", Language::TypeScript).is_some(),
            "const-bound arrow function must be fingerprinted"
        );

        // A `let`-bound async arrow (>20 tokens) IS fingerprinted too.
        let async_arrow = "let run = async (q) => { const a = await fetch(q); const b = await \
                           parse(a); return b.value; }";
        assert!(
            fp_lang(async_arrow, "t.ts", Language::TypeScript).is_some(),
            "let-bound async arrow must be fingerprinted"
        );
    }

    #[test]
    fn ts_class_field_arrow_handler_is_fingerprinted() {
        // A class-field arrow handler (`public_field_definition` with arrow value, >20 tokens) IS
        // fingerprinted (#232 #5).
        let field = "class C { handler = (ev) => { const x = read(ev); const y = norm(x); \
                     send(y); return y; } }";
        assert!(
            fp_lang(field, "t.ts", Language::TypeScript).is_some(),
            "class-field arrow handler must be fingerprinted"
        );
    }

    #[test]
    fn ts_large_non_function_value_const_is_not_fingerprinted() {
        // R3: the negative MUST be a >20-token NON-function value to isolate
        // `symbol_is_function_valued` (a <20-token `const x = 5` would pass for the WRONG
        // reason — the MIN_TOKENS size gate). A large object-literal const clears
        // MIN_TOKENS but its `value` child is an `object`, not a function, so it is NOT
        // fingerprinted.
        let big_object =
            "const cfg = { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10, k: 11 };";
        let parsed =
            parser::parse_file(Path::new("t.ts"), Language::TypeScript, big_object).expect("parse");
        let decl = parsed.symbols.iter().find(|s| s.kind == "const").expect("a const symbol");
        let node =
            parsed.root().descendant_for_byte_range(decl.start_byte, decl.end_byte).expect("node");
        // Clears MIN_TOKENS (so this isn't the size gate) but is NOT function-valued.
        assert!(
            normalize::normalize_baseline(node, big_object, Language::TypeScript).len()
                >= MIN_TOKENS,
            "fixture must clear MIN_TOKENS so the negative isolates the value guard"
        );
        assert!(
            !symbol_is_function_valued(node),
            "object-literal const must not be function-valued"
        );
        let fps = fingerprint_symbols(
            parsed.root(),
            big_object,
            Language::TypeScript,
            &candidates_of(&symbols::from_parsed(&parsed.symbols)),
        );
        assert!(
            fps.is_empty(),
            "a large non-function-value const must NOT be fingerprinted: {fps:?}"
        );
    }

    #[test]
    fn ts_const_kind_is_unchanged_by_fingerprint_guard() {
        // #232 #5 must NOT change symbol `kind`: a function-valued declarator is still classified
        // `const` (the guard is fingerprint-local; parser.rs is untouched).
        let arrow = "const load = (id) => { const row = get(id); return row; }";
        let parsed =
            parser::parse_file(Path::new("t.ts"), Language::TypeScript, arrow).expect("parse");
        let sym = parsed.symbols.iter().find(|s| s.name == "load").expect("load symbol");
        assert_eq!(sym.kind, "const", "function-valued declarator keeps kind=const");
    }
}
