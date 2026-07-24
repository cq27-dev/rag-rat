use super::*;

#[cfg(test)]
thread_local! {
    /// Counts embed-path FromText re-classifications ([`policy_for_job`]) since the last reset — the
    /// tree-sitter re-parse the #530 stamped-column fast path exists to avoid. A test resets it,
    /// runs a reconcile, and asserts 0 (fast path: read the stamped column) vs > 0 (fallback: the
    /// FromText recompute). This is what lets the fast-path tests DISAGREE with the fallback instead
    /// of passing via identical output.
    pub(crate) static POLICY_FROMTEXT_CALLS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_policy_fromtext_calls() {
    POLICY_FROMTEXT_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn policy_fromtext_calls() -> usize {
    POLICY_FROMTEXT_CALLS.with(std::cell::Cell::get)
}

/// Behavior version of the embedding-policy classifier. It certifies that a persisted
/// `chunks.embedding_policy` value reflects the CURRENT classifier — the reconcile skip-summary
/// reads the column via `GROUP BY` (the always-fast path, #530) only when a full rebuild has
/// stamped this version into `repo_meta`. It is the hash of `embedding_policy_for_chunk`'s
/// decisions over a fixed corpus, pinned by `policy_version_tests`: any behavior change — a
/// threshold, a gate, a plumbing node kind, or a tree-sitter grammar bump that reclassifies a node
/// with zero rag-rat source diff — changes the hash and reddens that test with the value to set
/// here. That is what makes the version impossible to forget: a hand-bumped const fails UNSAFE (a
/// stale-but-matching stamp would let the fast path serve mixed-code counts). A version mismatch
/// instead correctly forces the slow recompute. See the freshness-model Risk memory bound to this
/// file.
pub(crate) const EMBEDDING_POLICY_VERSION: &str = "cc6a08883bce0a23";

/// `repo_meta` keys carrying the embedding-policy freshness stamp a full rebuild writes
/// (`mark_embedding_policy_current`). PER-REPO, not the DB-global `index_meta`: one database can
/// host several repos and a rebuild of one must not certify another's un-rebuilt column.
/// `_VERSION_KEY` stores [`EMBEDDING_POLICY_VERSION`]; `_CAP_KEY` stores the char cap the column
/// was stamped at (always [`DEFAULT_MAX_EMBEDDING_CHARS`] — prep stamps at the default), so the
/// fast path can refuse a request at a different cap that would re-bucket `SkipTooLarge`.
pub(crate) const EMBEDDING_POLICY_VERSION_KEY: &str = "embedding_policy_version";
pub(crate) const EMBEDDING_POLICY_CAP_KEY: &str = "embedding_policy_cap";

/// The staleness clauses decidable WITHOUT the chunk text: missing (no embedding row → status !=
/// "Current"), re-hashed, model-/dim-/text-version-shifted. [`needs_embedding`]'s first gate, and
/// what lets the count path (`estimated_reconcile_jobs`) defer fetching a candidate's text until a
/// metadata-fresh chunk actually reaches the input-hash clause — the only staleness signal that
/// reads the text.
pub(crate) fn is_stale_without_text(chunk: &CurrentChunk, model_version: &str, dim: usize) -> bool {
    chunk.embedding_status.as_deref() != Some("Current")
        || chunk.source_text_hash.as_deref() != Some(chunk.text_hash.as_str())
        || chunk.model_version.as_deref() != Some(model_version)
        || chunk.embedding_dim != Some(i64::try_from(dim).unwrap_or(i64::MAX))
        || chunk.embedding_text_version.as_deref() != Some(EMBEDDING_TEXT_VERSION)
}

pub(crate) fn needs_embedding(
    chunk: &CurrentChunk,
    model_id: &str,
    model_version: &str,
    dim: usize,
    max_embedding_chars: usize,
) -> bool {
    // Evaluate the CHEAP staleness signals first. The only clause that needs the (O(text))
    // embedding input + its hash is the `input_hash` comparison, so defer building them until
    // every cheaper signal has said "fresh" — a chunk that is missing, re-hashed,
    // model-/dim-/text-version-shifted is decided by `is_stale_without_text` without touching the
    // text. This is a pure reordering of an OR chain (`build_embedding_input` is
    // side-effect-free), so the boolean is byte-identical; it matters because
    // `estimated_reconcile_jobs` runs this per candidate on the idle watcher/maintenance gate,
    // where a repo full of policy-skipped chunks (all missing, so decided by the first clause)
    // must not pay a build+hash each pass.
    if is_stale_without_text(chunk, model_version, dim) {
        return true;
    }
    let input = build_embedding_input(chunk, max_embedding_chars);
    let expected_input_hash = embedding_input_hash(model_id, model_version, &input.text);
    chunk.input_hash.as_deref() != Some(expected_input_hash.as_str())
}

/// How the `SkipLowSignal` gate classifies a chunk. Evaluated ONLY if the cheaper gates
/// (`SkipTooLarge`/`SkipGenerated`/`SkipTestFixture`/language/`SkipTooSmall`) don't short-circuit
/// first — a sub-80-char chunk must pay neither a re-parse nor a tree walk.
pub(crate) enum LowSignalCheck<'a> {
    /// Re-parse the chunk text (`is_low_signal_chunk`) — for callers with no tree at hand:
    /// generated / oversized / markdown / parse-failure files, the heal path, the reconcile paths.
    FromText,
    /// Classify the chunk's byte span against the file's shared parse (`is_low_signal_span`,
    /// #516) — one tree-sitter parse per file instead of one per chunk.
    FromSpan { language: Language, root: tree_sitter::Node<'a>, start_byte: usize, end_byte: usize },
}

impl LowSignalCheck<'_> {
    fn is_low_signal(
        &self,
        language: &str,
        chunk_kind: &str,
        symbol_path: Option<&str>,
        trimmed: &str,
    ) -> bool {
        match self {
            Self::FromText => is_low_signal_chunk(language, chunk_kind, symbol_path, trimmed),
            Self::FromSpan { language, root, start_byte, end_byte } =>
                is_low_signal_span(*language, *root, *start_byte, *end_byte),
        }
    }
}

/// The policy from the cheap, PARSE-FREE gates that precede the low-signal check (`trimmed` is the
/// pre-trimmed chunk text), or `None` when the chunk REACHES the low-signal gate — the only gate
/// that needs a tree-sitter parse. A caller that shares one parse across a file's chunks uses this
/// to skip the parse entirely when no chunk reaches the low-signal gate.
pub(crate) fn cheap_skip_policy(
    path: &Path,
    language: &str,
    file_kind: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
    trimmed: &str,
    max_embedding_chars: usize,
) -> Option<EmbeddingPolicyDecision> {
    // Use the SAME normalization the stored `files.path` uses (`paths::path_string`), so this
    // index-time stamp classifies generated/fixture paths byte-identically to the reconcile
    // recompute — which reads `files.path`. The raw `relative_path` is OS-native (`\` on Windows),
    // so without this the stamp disagrees with the recompute there; going through `path_string`
    // keeps it in lockstep with storage on every platform.
    let path_text = rag_rat_base::paths::path_string(path);
    if trimmed.chars().count() > max_embedding_chars.saturating_mul(4)
        && (file_kind == "generated" || chunk_kind == "generated" || symbol_path.is_none())
    {
        return Some(policy("SkipTooLarge", 9, false));
    }
    if file_kind == "generated" || chunk_kind == "generated" || looks_generated_path(&path_text) {
        return Some(policy("SkipGenerated", 9, false));
    }
    if is_test_fixture_path(&path_text) {
        return Some(policy("SkipTestFixture", 9, false));
    }
    let Ok(language_kind) = language.parse::<Language>() else {
        return Some(policy("SkipLanguageUnsupported", 9, false));
    };
    if !language_kind.supports_embeddings() {
        return Some(policy("SkipLanguageUnsupported", 9, false));
    }
    if trimmed.chars().count() < MIN_EMBEDDING_CHARS {
        return Some(policy("SkipTooSmall", 9, false));
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn embedding_policy_for_chunk(
    path: &Path,
    language: &str,
    file_kind: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
    text: &str,
    max_embedding_chars: usize,
    low_signal: LowSignalCheck<'_>,
) -> EmbeddingPolicyDecision {
    let trimmed = text.trim();
    if let Some(skip) = cheap_skip_policy(
        path,
        language,
        file_kind,
        chunk_kind,
        symbol_path,
        trimmed,
        max_embedding_chars,
    ) {
        return skip;
    }
    if low_signal.is_low_signal(language, chunk_kind, symbol_path, trimmed) {
        return policy("SkipLowSignal", 9, false);
    }
    // Normalize via `paths::path_string` as `cheap_skip_policy` does, so `embedding_priority`'s
    // path heuristics see the same form the stored `files.path` uses on every platform.
    let path_text = rag_rat_base::paths::path_string(path);
    policy("Embed", embedding_priority(&path_text, language, chunk_kind, symbol_path), true)
}

pub(crate) fn policy(name: &str, priority: i64, eligible: bool) -> EmbeddingPolicyDecision {
    EmbeddingPolicyDecision { policy: name.to_string(), priority, eligible }
}

pub(crate) fn policy_for_job(
    chunk: &CurrentChunk,
    max_embedding_chars: usize,
) -> EmbeddingPolicyDecision {
    #[cfg(test)]
    POLICY_FROMTEXT_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    embedding_policy_for_chunk(
        Path::new(&chunk.path),
        &chunk.language,
        &chunk.file_kind,
        &chunk.chunk_kind,
        chunk.symbol_path.as_deref(),
        &chunk.text,
        max_embedding_chars,
        LowSignalCheck::FromText,
    )
}

/// One embed-path candidate's policy. Under `stamped_policy` (#530: the stamps certify the column
/// at this run's cap) the decision comes straight from the stamped
/// `chunks.embedding_policy`/`embedding_priority` — the index-time FromSpan truth, no per-chunk
/// tree-sitter re-parse (#725; `Embed` is the sole eligible policy). Otherwise re-derive FromText
/// via [`policy_for_job`], the pre-certification behavior.
pub(crate) fn job_policy(
    chunk: &CurrentChunk,
    max_embedding_chars: usize,
    stamped_policy: bool,
) -> EmbeddingPolicyDecision {
    if stamped_policy {
        return policy(
            &chunk.embedding_policy,
            chunk.embedding_priority,
            chunk.embedding_policy == "Embed",
        );
    }
    policy_for_job(chunk, max_embedding_chars)
}

/// Embedding budget order: source symbols (0) before docs (1) before tests (2).
///
/// Test detection is the CANONICAL [`rag_rat_base::path_class::is_test_path`], not a local list.
/// The local copy this replaced knew only lowercase `tests/`/`test/` segments and Rust/TS filename
/// suffixes, so SwiftPM layouts (`Tests/AppTests/AppTests.swift` — capitalized directory, `*Tests`
/// stem) read as production source and competed with it for the priority-0 budget, while the rest
/// of the system (`staleness`, `repo_brief`, the graph's test-callsite filter, `symbols.is_test`)
/// called the very same file a test. One predicate, one answer, every language.
pub(crate) fn embedding_priority(
    path: &str,
    language: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
) -> i64 {
    let is_test = rag_rat_base::path_class::is_test_path(path);
    if symbol_path.is_some() && matches!(chunk_kind, "code") && !is_test && language != "markdown" {
        return 0;
    }
    if language == "markdown" {
        return 1;
    }
    if is_test {
        return 2;
    }
    1
}

pub(crate) fn priority_label(priority: i64) -> &'static str {
    match priority {
        0 => "source_symbols",
        1 => "source_or_docs",
        2 => "tests",
        3 => "low_signal",
        9 => "skipped",
        _ => "other",
    }
}

/// Build output and dependency trees, plus the lockfiles that sit at their root. `/.build/` and
/// `Package.resolved` are SwiftPM's `target/`+`Cargo.lock`; `Pods/` is CocoaPods' vendored
/// dependency tree.
pub(crate) fn looks_generated_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/src/generated/")
        || path.contains("/target/")
        || path.contains("/.build/")
        || path.starts_with(".build/")
        || path.contains("/Pods/")
        || path.starts_with("Pods/")
        || path.ends_with("Cargo.lock")
        || path.ends_with("package-lock.json")
        || path.ends_with("pnpm-lock.yaml")
        || path.ends_with("Package.resolved")
}

pub(crate) fn is_test_fixture_path(path: &str) -> bool {
    path.contains("/fixtures/")
        || path.contains("/__fixtures__/")
        || path.contains("/testdata/")
        || path.contains("/snapshots/")
        || path.ends_with(".snap")
}

/// Whether a chunk is "pure plumbing" with no semantic signal worth embedding — every top-level
/// statement is an import/include, a comment, or a bare docstring. AST-driven (#170): the chunk is
/// re-parsed and classified by tree-sitter **node kind**, not line prefixes — robust to multi-line
/// imports (`from x import (\n a,\n b,\n)`), docstrings, `#`-vs-`#define`, and spacing the old
/// string heuristic mishandled. A symbol chunk is a definition node, so it's never low-signal.
///
/// `chunk_kind` / `symbol_path` are unused now (the node kinds carry the signal) but kept on the
/// signature for the call site. This is the FALLBACK for callers with no tree at hand
/// ([`LowSignalCheck::FromText`]); the prepare phase classifies against the file's shared parse
/// instead ([`is_low_signal_span`], #516) so it never pays a per-chunk re-parse.
pub(crate) fn is_low_signal_chunk(
    language: &str,
    _chunk_kind: &str,
    _symbol_path: Option<&str>,
    text: &str,
) -> bool {
    let Ok(lang) = language.parse::<Language>() else {
        return false;
    };
    if lang == Language::Markdown {
        return false;
    }
    let ext = lang.simple_extensions().first().copied().unwrap_or("txt");
    let path = std::path::PathBuf::from(format!("chunk.{ext}"));
    let Some(parsed) = crate::index::parser::parse_file(&path, lang, text) else {
        // No grammar / hard parse failure — keep it; don't skip a chunk we can't classify.
        return false;
    };
    let root = parsed.root();
    // Low-signal iff EVERY top-level statement is plumbing (vacuously true for an empty/`}`-only
    // chunk that parses to no statements). Any definition/expression/other node is signal.
    let mut cursor = root.walk();
    root.named_children(&mut cursor).all(|child| is_plumbing_node(lang, child))
}

/// Span-based twin of [`is_low_signal_chunk`] (#516): classify a chunk's byte span against the
/// FILE's already-parsed tree, so the prepare phase pays one tree-sitter parse per file instead of
/// one per chunk (each of those re-parses cost a worker-thread spawn + a full text copy in
/// `parse_within_budget`). Low-signal iff every named node the span covers is plumbing, descending
/// into container nodes that extend beyond the span (a `mod` / class / function body the chunk
/// slices into) — so a `use`-only slice of a mod body classifies the way its standalone parse used
/// to. Vacuously true for a span covering no named nodes (blank / `}`-only chunks), which the
/// `SkipTooSmall` gate catches before low-signal is consulted anyway.
pub(crate) fn is_low_signal_span(
    language: Language,
    root: tree_sitter::Node<'_>,
    start_byte: usize,
    end_byte: usize,
) -> bool {
    span_is_plumbing(language, root, start_byte, end_byte)
}

fn span_is_plumbing(
    language: Language,
    node: tree_sitter::Node<'_>,
    start_byte: usize,
    end_byte: usize,
) -> bool {
    // grow_stack: this recurses into container children to full subtree depth; a hostile
    // deeply-nested chunk must grow the stack, not overflow it (#543).
    rag_rat_base::stack::grow_stack(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).all(|child| {
            if child.end_byte() <= start_byte || child.start_byte() >= end_byte {
                return true;
            }
            if is_plumbing_node(language, child) {
                return true;
            }
            if start_byte <= child.start_byte() && child.end_byte() <= end_byte {
                // A whole non-plumbing statement inside the chunk is signal.
                return false;
            }
            // The child extends beyond the span — a container the chunk slices into. Classify the
            // slice by the child's own children. A sliced leaf (e.g. one line inside a long string
            // literal) has none and classifies as plumbing, consistent with docstring interiors.
            span_is_plumbing(language, child, start_byte, end_byte)
        })
    })
}

/// A top-level node carrying no embed-worthy signal according to its structural language package.
/// Definition-introducing forms remain signal and keep their embedding.
fn is_plumbing_node(language: Language, node: tree_sitter::Node<'_>) -> bool {
    crate::index::languages::is_plumbing_node(language, node)
}

#[cfg(test)]
mod low_signal_tests {
    use super::is_low_signal_chunk;

    #[test]
    fn c_macro_definition_is_not_low_signal() {
        // Regression (Codex on #167): `#` is a C directive, not a comment, and `#define` is a macro
        // SYMBOL — its chunk must keep its embedding, not be filtered as plumbing.
        assert!(!is_low_signal_chunk("c", "code", Some("DEVICE_API"), "#define DEVICE_API(x) (x)"));
    }

    #[test]
    fn c_include_only_chunk_is_low_signal() {
        assert!(is_low_signal_chunk(
            "c",
            "code",
            Some("s"),
            "#include <stdio.h>\n#include \"a.h\""
        ));
    }

    #[test]
    fn python_comment_and_docstring_only_is_low_signal() {
        assert!(is_low_signal_chunk("python", "code", Some("s"), "# a note\n\"\"\"doc\"\"\""));
    }

    #[test]
    fn python_multiline_docstring_only_is_low_signal() {
        // The AST sees one `string` statement; the old line-prefix heuristic let the interior
        // prose lines through and embedded it (#170 motivation).
        let chunk =
            "\"\"\"\nA multi-line module docstring.\nSpanning several prose lines.\n\"\"\"\n";
        assert!(is_low_signal_chunk("python", "code", None, chunk));
    }

    #[test]
    fn python_multiline_import_is_low_signal() {
        // Parenthesized multi-import is one `import_from_statement` node — robust where line
        // prefixes weren't.
        let chunk = "from pkg import (\n    A,\n    B,\n    C,\n)\n";
        assert!(is_low_signal_chunk("python", "code", None, chunk));
    }

    #[test]
    fn python_real_code_is_not_low_signal() {
        assert!(!is_low_signal_chunk("python", "code", None, "x = compute()\nreturn x\n"));
    }

    #[test]
    fn python_import_only_is_low_signal_but_a_def_is_not() {
        assert!(is_low_signal_chunk("python", "code", Some("s"), "import os\nfrom a import b"));
        assert!(!is_low_signal_chunk(
            "python",
            "code",
            Some("Api"),
            "def host(self):\n    return 1"
        ));
    }

    #[test]
    fn rust_use_only_is_low_signal() {
        assert!(is_low_signal_chunk("rust", "code", Some("s"), "use a::b;\npub use c::d;"));
    }
}

/// The span-based twin of `is_low_signal_chunk` (#516): classifies a chunk's byte span against the
/// FILE's shared parse instead of re-parsing the chunk text, so the prepare phase pays one parse
/// per file, not one per chunk. These pin parity with the text-based semantics on the same shapes
/// the `low_signal_tests` above cover, plus the container-descent case the text parse could only
/// approximate (plumbing nested inside a `mod`/function body the chunk slices into).
#[cfg(test)]
mod low_signal_span_tests {
    use std::path::Path;

    use rag_rat_base::language::Language;

    use super::is_low_signal_span;

    fn parsed(path: &str, language: Language, src: &str) -> crate::index::parser::ParsedFile {
        crate::index::parser::parse_file(Path::new(path), language, src).expect("fixture parses")
    }

    #[test]
    fn rust_use_only_span_at_file_root_is_low_signal_and_a_fn_span_is_not() {
        let src = "use a::b;\npub use c::d;\n\nfn real() {\n    let x = 1;\n}\n";
        let file = parsed("s.rs", Language::Rust, src);
        let uses_end = src.find("\nfn").expect("fn present") + 1;
        assert!(is_low_signal_span(Language::Rust, file.root(), 0, uses_end), "use-only span");
        assert!(
            !is_low_signal_span(Language::Rust, file.root(), uses_end, src.len()),
            "a definition span is signal"
        );
    }

    #[test]
    fn rust_use_lines_inside_a_mod_body_are_low_signal() {
        // The chunk slices INTO the mod body (the mod node extends beyond the span), so the
        // classifier must descend through the container instead of treating it as signal.
        let src = "mod tests {\n    use super::*;\n    use std::fmt;\n\n    fn helper() {}\n}\n";
        let file = parsed("s.rs", Language::Rust, src);
        let start = src.find("    use super").expect("use present");
        let end = src.find("\n\n").expect("blank line") + 1;
        assert!(is_low_signal_span(Language::Rust, file.root(), start, end));
    }

    #[test]
    fn whitespace_only_span_is_low_signal() {
        let src = "use a::b;\n\n\nfn real() {}\n";
        let file = parsed("s.rs", Language::Rust, src);
        let start = src.find("\n\n").expect("gap") + 1;
        assert!(is_low_signal_span(Language::Rust, file.root(), start, start + 1));
    }

    #[test]
    fn python_docstring_and_import_spans_are_low_signal_but_a_def_is_not() {
        let src = "\"\"\"Module doc.\nMore prose.\n\"\"\"\nimport os\nfrom a import b\n\ndef \
                   real():\n    return 1\n";
        let file = parsed("s.py", Language::Python, src);
        let def_start = src.find("\ndef").expect("def present") + 1;
        assert!(is_low_signal_span(Language::Python, file.root(), 0, def_start));
        assert!(!is_low_signal_span(Language::Python, file.root(), def_start, src.len()));
    }

    #[test]
    fn c_include_span_is_low_signal_but_a_define_is_not() {
        // Regression twin of `c_macro_definition_is_not_low_signal`: `#define` is a macro SYMBOL.
        let src = "#include <stdio.h>\n#include \"a.h\"\n#define DEVICE_API(x) (x)\nint f(void) \
                   {\n    return 0;\n}\n";
        let file = parsed("s.c", Language::C, src);
        let define_start = src.find("#define").expect("define present");
        let define_end = src.find("\nint").expect("fn present") + 1;
        assert!(is_low_signal_span(Language::C, file.root(), 0, define_start));
        assert!(!is_low_signal_span(Language::C, file.root(), define_start, define_end));
    }

    #[test]
    fn mixed_span_with_any_definition_is_signal() {
        let src = "use a::b;\nfn real() {}\n";
        let file = parsed("s.rs", Language::Rust, src);
        assert!(!is_low_signal_span(Language::Rust, file.root(), 0, src.len()));
    }
}

/// Behavior-hash tripwire for [`EMBEDDING_POLICY_VERSION`] (#530). The corpus routes through every
/// embedding-policy gate — the parse-free cheap gates and the low-signal gate, the latter both
/// `FromText` and `FromSpan` across every grammar — so any classifier change (a threshold, a gate,
/// a plumbing node kind) or a tree-sitter grammar bump that reclassifies a node changes the hash
/// and fails this test with the value to set. The version certifies the persisted
/// `chunks.embedding_policy` column, so it must move whenever the classifier's output could.
#[cfg(test)]
mod policy_version_tests {
    use std::collections::HashSet;
    use std::fmt::Write as _;
    use std::path::Path;

    use rag_rat_base::hash::hex_sha256;
    use rag_rat_base::language::Language;

    use super::{
        DEFAULT_MAX_EMBEDDING_CHARS, EMBEDDING_POLICY_VERSION, LowSignalCheck, MIN_EMBEDDING_CHARS,
        embedding_policy_for_chunk,
    };

    fn record(sig: &mut String, label: &str, d: &super::EmbeddingPolicyDecision) {
        let _ = writeln!(sig, "{label}|{}|{}|{}", d.policy, d.priority, d.eligible);
    }

    fn behavior_signature() -> String {
        let mut sig = String::new();
        let big = "x".repeat(20_000);
        // Boundary-adjacent lengths pinned as LITERALS (not `MIN_EMBEDDING_CHARS`, which would move
        // with the constant and never flip): one exactly AT the current MIN and one just BELOW it,
        // so a `<` vs `<=` change or an off-by-one shift of the SkipTooSmall gate flips the
        // hash.
        let at_min = "a".repeat(80);
        let below_min = "a".repeat(79);

        // Parse-free cheap gates (FromText — no tree needed for these to decide).
        // (label, path, language, file_kind, chunk_kind, symbol_path, text, cap)
        type TextCase<'a> =
            (&'a str, &'a str, &'a str, &'a str, &'a str, Option<&'a str>, &'a str, usize);
        let text_cases: &[TextCase] = &[
            ("too_large_generated", "a.rs", "rust", "generated", "code", None, &big, 4000),
            ("too_large_no_symbol", "a.rs", "rust", "source", "code", None, &big, 4000),
            ("at_min_len", "a.rs", "rust", "source", "code", Some("s"), &at_min, 4000),
            ("below_min_len", "a.rs", "rust", "source", "code", Some("s"), &below_min, 4000),
            // FromText low-signal branch (`is_low_signal_chunk`) — the classifier used for
            // oversized / markdown / parse-failure files, which the span cases below
            // do NOT exercise. An all-imports block (>= MIN) is low-signal; a real
            // definition is not.
            (
                "fromtext_plumbing",
                "a.rs",
                "rust",
                "source",
                "code",
                Some("s"),
                "use std::collections::HashMap;\nuse std::fmt::Debug;\nuse std::io::Read;\nuse \
                 std::sync::Arc;\n",
                4000,
            ),
            (
                "fromtext_signal",
                "a.rs",
                "rust",
                "source",
                "code",
                Some("s"),
                "pub fn real_function(x: i64) -> i64 {\n    let value = x * 2 + 1;\n    \
                 value.wrapping_mul(3)\n}\n",
                4000,
            ),
            (
                "generated_file_kind",
                "a.rs",
                "rust",
                "generated",
                "code",
                Some("s"),
                "fn a() {}",
                4000,
            ),
            (
                "generated_chunk_kind",
                "a.rs",
                "rust",
                "source",
                "generated",
                Some("s"),
                "fn a() {}",
                4000,
            ),
            (
                "generated_path",
                "pkg/target/a.rs",
                "rust",
                "source",
                "code",
                Some("s"),
                "fn a() { let value = compute(); process(value); }",
                4000,
            ),
            (
                "test_fixture_path",
                "pkg/fixtures/a.rs",
                "rust",
                "source",
                "code",
                Some("s"),
                "fn a() { let value = compute(); process(value); }",
                4000,
            ),
            // Backslash paths (Windows `relative_path`): these classify as SkipGenerated /
            // SkipTestFixture ONLY because the policy `/`-normalizes via `paths::path_string` — so
            // they pin that normalization AND bump the version, which forces existing Windows
            // indexes (stamped before the normalization) to re-stamp their
            // `chunks.embedding_policy`.
            (
                "backslash_generated_path",
                "pkg\\target\\a.rs",
                "rust",
                "source",
                "code",
                Some("s"),
                "fn a() { let value = compute(); process(value); }",
                4000,
            ),
            (
                "backslash_test_fixture_path",
                "pkg\\fixtures\\a.rs",
                "rust",
                "source",
                "code",
                Some("s"),
                "fn a() { let value = compute(); process(value); }",
                4000,
            ),
            (
                "lang_unsupported",
                "a.txt",
                "plaintext",
                "source",
                "code",
                Some("s"),
                "some prose here that is long enough to clear the small gate for sure",
                4000,
            ),
            ("too_small", "a.rs", "rust", "source", "code", Some("s"), "fn a() {}", 4000),
            (
                "embed_plain",
                "a.rs",
                "rust",
                "source",
                "code",
                Some("s"),
                "fn real() { let value = compute_something(); process_the(value); done(); }",
                4000,
            ),
        ];
        for (label, path, lang, fk, ck, sp, text, cap) in text_cases {
            let d = embedding_policy_for_chunk(
                Path::new(path),
                lang,
                fk,
                ck,
                *sp,
                text,
                *cap,
                LowSignalCheck::FromText,
            );
            record(&mut sig, label, &d);
        }

        // EVERY path-predicate branch of `looks_generated_path` + `is_test_fixture_path`, so an
        // edit to any one of them flips the hash (the two path cases above only sample
        // `/target/` and `/fixtures/`). A non-generated, non-fixture path is the code
        // default and is already covered by `embed_plain` / the span cases.
        //
        // ALSO the `embedding_priority` test-path branch, which nothing pinned before: every case
        // here carries a symbol and real (>= MIN) source text, so a non-skipped path records
        // priority 0 (source) vs 2 (test) and the CANONICAL
        // `rag_rat_base::path_class::is_test_path` is under the hash. It has to be — a
        // local test-path copy silently drifted from the canonical one and mis-ranked every
        // SwiftPM `Tests/` file as production source, with no test to catch it.
        // Cases run in their OWN language so the `FromText` low-signal re-parse sees code it can
        // actually parse.
        const RUST_SRC: &str = "fn real_function(input: i64) -> i64 {\n    let value = input * 2 \
                                + 1;\n    another_call(value)\n}\n";
        const SWIFT_SRC: &str = "func realFunction(_ input: Int) -> Int {\n    let value = input \
                                 + 1\n    return value + compute(value)\n}\n";
        const MD_SRC: &str = "# Guide\n\nThis document explains how the indexer resolves \
                              candidate edges to real symbols.\n";
        let path_cases: &[(&str, &str, &str)] = &[
            ("pkg/generated/a.rs", "rust", RUST_SRC),
            ("pkg/src/generated/a.rs", "rust", RUST_SRC),
            ("pkg/target/a.rs", "rust", RUST_SRC),
            ("Cargo.lock", "rust", RUST_SRC),
            ("some/dir/package-lock.json", "rust", RUST_SRC),
            ("some/dir/pnpm-lock.yaml", "rust", RUST_SRC),
            ("pkg/fixtures/a.rs", "rust", RUST_SRC),
            ("pkg/__fixtures__/a.rs", "rust", RUST_SRC),
            ("pkg/testdata/a.rs", "rust", RUST_SRC),
            ("pkg/snapshots/a.rs", "rust", RUST_SRC),
            ("pkg/thing.snap", "rust", RUST_SRC),
            // SwiftPM / CocoaPods build + dependency trees (`looks_generated_path`).
            ("app/.build/checkouts/dep/Sources/dep/a.swift", "swift", SWIFT_SRC),
            (".build/checkouts/dep/Sources/dep/a.swift", "swift", SWIFT_SRC),
            ("Package.resolved", "swift", SWIFT_SRC),
            ("Pods/Alamofire/Source/a.swift", "swift", SWIFT_SRC),
            // Test paths (priority 2) vs production source (priority 0), per language convention.
            ("pkg/tests/a.rs", "rust", RUST_SRC),
            ("pkg/src/a_test.rs", "rust", RUST_SRC),
            ("Tests/AppTests/AppTests.swift", "swift", SWIFT_SRC),
            ("Sources/App/ClientTests.swift", "swift", SWIFT_SRC),
            ("Sources/App/Client.swift", "swift", SWIFT_SRC),
            ("pkg/src/thing.rs", "rust", RUST_SRC),
            // Docs (priority 1) — the remaining `embedding_priority` branch.
            ("docs/guide.md", "markdown", MD_SRC),
        ];
        for (path, lang, text) in path_cases {
            // A path gate fires before the low-signal check, so the cap is immaterial here.
            let d = embedding_policy_for_chunk(
                Path::new(path),
                lang,
                "source",
                "code",
                Some("s"),
                text,
                4000,
                LowSignalCheck::FromText,
            );
            record(&mut sig, &format!("path_{path}"), &d);
        }

        // Low-signal via FromSpan across every grammar: a pure-plumbing file (imports/comments
        // only, >=80 chars so it reaches the low-signal gate) classifies low-signal; a file
        // with a real definition classifies as signal. Exercises `is_plumbing_node` per
        // language + the grammar. (label, path, language, plumbing_src, def_src)
        // Every fixture is >=80 chars so it clears the SkipTooSmall gate and actually reaches the
        // span classifier (the point of the FromSpan cases). Whatever each classifies as is the
        // pinned behavior; a grammar bump that reclassifies a node flips the hash.
        let span_cases: &[(&str, &str, Language, &str, &str)] = &[
            (
                "rust",
                "s.rs",
                Language::Rust,
                "use std::collections::HashMap;\nuse std::fmt::Debug;\n// a descriptive comment \
                 line\nuse std::io::Read;\nuse std::sync::Arc;\n",
                "pub fn real_function(input: i32) -> i32 {\n    let value = input + 1;\n    \
                 println!(\"{}\", value);\n    another_call(value)\n}\n",
            ),
            (
                "typescript",
                "s.ts",
                Language::TypeScript,
                "import defaultThing from 'a';\n// a descriptive comment line here now\nimport { \
                 namedThing } from 'b';\nimport * as ns from 'c';\n",
                "export function realFunction(input: number): number {\n    const value = input + \
                 1;\n    return value + compute(value);\n}\n",
            ),
            (
                "kotlin",
                "s.kt",
                Language::Kotlin,
                "package com.example.app\nimport kotlin.collections.List\n// a descriptive \
                 comment line here now\nimport kotlin.io.println\n",
                "fun realFunction(input: Int): Int {\n    val value = input + 1\n    return value \
                 + compute(value)\n}\n",
            ),
            (
                "c",
                "s.c",
                Language::C,
                "#include <stdio.h>\n#include \"local_header.h\"\n// a descriptive comment line \
                 here now goes on\n#include <string.h>\n",
                "int real_function(int input) {\n    int value = input + 1;\n    return value + \
                 compute(value);\n}\n",
            ),
            (
                "cpp",
                "s.cpp",
                Language::Cpp,
                "#include <vector>\n#include <string>\n// a descriptive comment line here now \
                 goes on and on\n#include <memory>\n",
                "int real_function(int input) {\n    int value = input + 1;\n    return value + \
                 compute(value);\n}\n",
            ),
            (
                "python",
                "s.py",
                Language::Python,
                "import os\nimport sys\nfrom collections import defaultdict\n# a descriptive \
                 comment line here now goes on\n",
                "def real_function(input):\n    value = input + 1\n    result = value + \
                 compute(value)\n    return result\n",
            ),
            (
                "swift",
                "s.swift",
                Language::Swift,
                "import Foundation\nimport Dispatch\n// a descriptive comment line here now goes \
                 on long enough\nimport Observation\n",
                "func realFunction(_ input: Int) -> Int {\n    let value = input + 1\n    return \
                 value + compute(value)\n}\n",
            ),
        ];
        for (label, path, language, plumbing, def) in span_cases {
            let pf = crate::index::parser::parse_file(Path::new(path), *language, plumbing)
                .expect("plumbing parses");
            let d = embedding_policy_for_chunk(
                Path::new(path),
                &language.to_string(),
                "source",
                "code",
                Some("s"),
                plumbing,
                4000,
                LowSignalCheck::FromSpan {
                    language: *language,
                    root: pf.root(),
                    start_byte: 0,
                    end_byte: plumbing.len(),
                },
            );
            record(&mut sig, &format!("span_plumbing_{label}"), &d);

            let df = crate::index::parser::parse_file(Path::new(path), *language, def)
                .expect("def parses");
            let d = embedding_policy_for_chunk(
                Path::new(path),
                &language.to_string(),
                "source",
                "code",
                Some("s"),
                def,
                4000,
                LowSignalCheck::FromSpan {
                    language: *language,
                    root: df.root(),
                    start_byte: 0,
                    end_byte: def.len(),
                },
            );
            record(&mut sig, &format!("span_def_{label}"), &d);
        }

        // Fold the governing thresholds in directly, so a change to any of them flips the hash even
        // if no corpus case happens to straddle the new boundary (the `MIN 80→79` blind spot).
        let _ = writeln!(
            sig,
            "consts|{}|{}|{}",
            MIN_EMBEDDING_CHARS,
            DEFAULT_MAX_EMBEDDING_CHARS,
            crate::index::chunker::MAX_STRUCTURAL_PARSE_BYTES,
        );

        sig
    }

    #[test]
    fn policy_version_pins_classifier_behavior() {
        let hash = &hex_sha256(behavior_signature().as_bytes())[..16];
        assert_eq!(
            hash, EMBEDDING_POLICY_VERSION,
            "embedding-policy behavior changed (a classifier gate/threshold or a tree-sitter \
             grammar bump); set EMBEDDING_POLICY_VERSION to \"{hash}\""
        );
    }

    #[test]
    fn corpus_exercises_every_policy() {
        // The tripwire only pins the HASH; this guards that the corpus keeps EXERCISING every
        // policy outcome, so a future corpus edit that collapses cases (e.g. every case
        // falling into `SkipTooSmall`) can't silently weaken the version guard. `record`
        // writes `label|policy|..`.
        let sig = behavior_signature();
        for expected in [
            "SkipTooLarge",
            "SkipGenerated",
            "SkipTestFixture",
            "SkipLanguageUnsupported",
            "SkipTooSmall",
            "SkipLowSignal",
            "Embed",
        ] {
            assert!(
                sig.contains(&format!("|{expected}|")),
                "the version corpus must exercise {expected} — otherwise the hash guard covers \
                 less than it appears to"
            );
        }
        // ...and every embedding PRIORITY, so the test-path predicate the priority depends on is
        // under the hash too. It wasn't: no corpus case was a test path, so `embedding_priority`'s
        // test branch went unpinned — and a local test-path copy drifted from the canonical
        // predicate, mis-ranking every SwiftPM `Tests/` file as priority-0 production source, with
        // nothing to redden. `record` writes `label|policy|priority|eligible`.
        let priorities: HashSet<&str> =
            sig.lines().filter_map(|line| line.split('|').nth(2)).collect();
        for expected in ["0", "1", "2"] {
            assert!(
                priorities.contains(expected),
                "the version corpus must exercise embedding priority {expected} ({}) — otherwise \
                 a change to the test-path predicate that decides it cannot flip the hash",
                super::priority_label(expected.parse().unwrap()),
            );
        }
    }
}
