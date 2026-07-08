use super::*;

pub(crate) fn needs_embedding(
    chunk: &CurrentChunk,
    model_id: &str,
    model_version: &str,
    dim: usize,
    max_embedding_chars: usize,
) -> bool {
    let input = build_embedding_input(chunk, max_embedding_chars);
    let expected_input_hash = embedding_input_hash(model_id, model_version, &input.text);
    chunk.embedding_status.as_deref() != Some("Current")
        || chunk.source_text_hash.as_deref() != Some(chunk.text_hash.as_str())
        || chunk.model_version.as_deref() != Some(model_version)
        || chunk.embedding_dim != Some(i64::try_from(dim).unwrap_or(i64::MAX))
        || chunk.input_hash.as_deref() != Some(expected_input_hash.as_str())
        || chunk.embedding_text_version.as_deref() != Some(EMBEDDING_TEXT_VERSION)
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
    let path_text = path.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.chars().count() > max_embedding_chars.saturating_mul(4)
        && (file_kind == "generated" || chunk_kind == "generated" || symbol_path.is_none())
    {
        return policy("SkipTooLarge", 9, false);
    }
    if file_kind == "generated" || chunk_kind == "generated" || looks_generated_path(&path_text) {
        return policy("SkipGenerated", 9, false);
    }
    if is_test_fixture_path(&path_text) {
        return policy("SkipTestFixture", 9, false);
    }
    let Ok(language_kind) = language.parse::<Language>() else {
        return policy("SkipLanguageUnsupported", 9, false);
    };
    if !language_kind.supports_embeddings() {
        return policy("SkipLanguageUnsupported", 9, false);
    }
    if trimmed.chars().count() < MIN_EMBEDDING_CHARS {
        return policy("SkipTooSmall", 9, false);
    }
    if low_signal.is_low_signal(language, chunk_kind, symbol_path, trimmed) {
        return policy("SkipLowSignal", 9, false);
    }
    policy("Embed", embedding_priority(&path_text, language, chunk_kind, symbol_path), true)
}

pub(crate) fn policy(name: &str, priority: i64, eligible: bool) -> EmbeddingPolicyDecision {
    EmbeddingPolicyDecision { policy: name.to_string(), priority, eligible }
}

pub(crate) fn policy_for_job(
    chunk: &CurrentChunk,
    max_embedding_chars: usize,
) -> EmbeddingPolicyDecision {
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

pub(crate) fn embedding_priority(
    path: &str,
    language: &str,
    chunk_kind: &str,
    symbol_path: Option<&str>,
) -> i64 {
    if symbol_path.is_some()
        && matches!(chunk_kind, "code")
        && !is_test_path(path)
        && language != "markdown"
    {
        return 0;
    }
    if language == "markdown" {
        return 1;
    }
    if is_test_path(path) {
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

pub(crate) fn looks_generated_path(path: &str) -> bool {
    path.contains("/generated/")
        || path.contains("/src/generated/")
        || path.contains("/target/")
        || path.ends_with("Cargo.lock")
        || path.ends_with("package-lock.json")
        || path.ends_with("pnpm-lock.yaml")
}

pub(crate) fn is_test_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("__tests__")
        || path.ends_with("_test.rs")
        || path.ends_with(".test.ts")
        || path.ends_with(".spec.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".spec.tsx")
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
}

/// A top-level node carrying no embed-worthy signal: a comment, or a per-language import/include /
/// package / docstring / `pass`. NOT a definition-introducing form (a C `#define` macro, a Rust
/// `mod`, a Python `def`/`class`) — those are symbols and must keep their embedding.
fn is_plumbing_node(language: Language, node: tree_sitter::Node<'_>) -> bool {
    let kind = node.kind();
    if kind.contains("comment") {
        return true;
    }
    match language {
        Language::Rust => kind == "use_declaration",
        Language::TypeScript => kind == "import_statement",
        Language::Kotlin => matches!(kind, "import_header" | "package_header"),
        Language::C | Language::Cpp => kind == "preproc_include",
        Language::Python =>
            matches!(
                kind,
                "import_statement"
                    | "import_from_statement"
                    | "future_import_statement"
                    | "pass_statement"
            ) || is_python_docstring_statement(node),
        Language::Markdown => true,
    }
}

/// A Python bare docstring: an `expression_statement` whose sole child is a string literal.
fn is_python_docstring_statement(node: tree_sitter::Node<'_>) -> bool {
    node.kind() == "expression_statement"
        && node.named_child_count() == 1
        && node.named_child(0).is_some_and(|child| child.kind() == "string")
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

    use super::is_low_signal_span;
    use crate::index::parser;
    use crate::language::Language;

    fn parsed(path: &str, language: Language, src: &str) -> parser::ParsedFile {
        parser::parse_file(Path::new(path), language, src).expect("fixture parses")
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
