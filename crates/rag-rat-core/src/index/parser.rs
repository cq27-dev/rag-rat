use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tree_sitter::{Node, ParseOptions, ParseState, Parser, Tree};

use crate::language::Language;

/// Wall-clock budget for a single file's tree-sitter parse. A normal file parses in well under this
/// (tens of ms even for thousands of lines); a pathological input that drives tree-sitter into
/// super-linear parsing — a grammar-ambiguity blowup, e.g. some Kotlin files (#210) — would
/// otherwise pin a core at 100% CPU FOREVER, hanging `index` and the background watcher with no
/// recovery. Past the budget the parse is treated as a parser failure (the file is recorded +
/// skipped), keeping the indexer responsive. Generous on purpose: the cost of a false timeout
/// (silently dropping a huge but legitimate file) is worse than a few wasted seconds on a genuinely
/// pathological one.
pub(crate) const PARSE_BUDGET: Duration = Duration::from_secs(5);

/// Extra wall-clock the caller waits beyond [`PARSE_BUDGET`] before ABANDONING the parse worker.
/// tree-sitter's progress callback only fires in the lex/advance path, so a same-position reduce
/// blowup (the H2.kt class, #210) never invokes it and the soft budget can't stop it — only giving
/// up on the worker thread does. Small margin so that when the soft cancel DOES work the worker
/// returns first (clean, no leaked thread); the leaked-worker path is the fallback for the
/// uncancellable case.
const PARSE_ABANDON_GRACE: Duration = Duration::from_secs(2);

/// Content hashes of inputs whose parse already exceeded the budget. A pathological parse is
/// UNCANCELLABLE (#210), so re-parsing the same content just spawns another doomed worker — and the
/// same file is parsed by several callers (symbols, edges, AND the chunk fallback in
/// `prepare_index_content`), so without a memo a single pathological file pays the timeout 2-3× per
/// pass (#211 review). A hit returns `None` immediately, never spawning a worker — so identical
/// content times out at most ONCE process-wide (this also covers the watcher re-reading an
/// unchanged pathological file across passes). Keyed by content, so a genuinely changed file is
/// re-tried.
static TIMED_OUT_PARSES: LazyLock<Mutex<HashSet<u64>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Count of ABANDONED parse workers still running — uncancellable parses the caller gave up on but
/// that keep pegging a core until they finish (or forever, for a truly non-terminating blowup).
/// ONLY abandoned workers are counted, NOT normal in-flight parses, so normal/healthy parsing is
/// never throttled by this cap (#211 review). A healthy file is refused only when this many leaked
/// workers are already saturating the machine.
static ABANDONED_PARSE_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Cap for [`ABANDONED_PARSE_WORKERS`]: at most ~one leaked worker per core before new parses bail
/// (each abandoned worker pegs a core, so beyond this the machine is already saturated).
static MAX_ABANDONED_PARSE_WORKERS: LazyLock<usize> =
    LazyLock::new(|| std::thread::available_parallelism().map_or(4, |n| n.get()).max(4));

fn content_key(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn memoize_timed_out(key: u64) {
    if let Ok(mut set) = TIMED_OUT_PARSES.lock() {
        set.insert(key);
    }
}

/// Parse `text` under a hard wall-clock bound, returning `None` on a genuine parse failure OR a
/// timeout — both of which the callers already treat as a parser failure (#210).
///
/// Because tree-sitter cannot be cancelled mid-reduce (#210):
/// 1. a content-keyed memo ([`TIMED_OUT_PARSES`]) short-circuits content already known to time out
///    — soft-cancelled OR hard-abandoned — so it is never parsed more than once (a file is parsed
///    by symbols, edges, AND the chunk fallback, so this avoids paying the budget 2-3× per pass);
/// 2. the parse runs on a worker thread with a SOFT progress-callback budget that cleanly cancels
///    the cancellable cases (runaway lexing, huge-but-progressing files);
/// 3. the calling thread waits only `budget + PARSE_ABANDON_GRACE` and, on an uncancellable reduce
///    explosion, ABANDONS the worker. Only abandoned-and-still-running workers count toward
///    [`ABANDONED_PARSE_WORKERS`] (via a race-free claim), so normal parsing is never throttled and
///    leaked workers still can't accumulate without bound.
pub(crate) fn parse_within_budget(
    grammar: tree_sitter::Language,
    text: &str,
    budget: Duration,
) -> Option<Tree> {
    let key = content_key(text);
    // Known-pathological content: don't spawn another doomed worker (#211 review).
    if TIMED_OUT_PARSES.lock().is_ok_and(|set| set.contains(&key)) {
        return None;
    }
    // Refuse only when leaked (abandoned) workers already saturate the machine — normal parsing
    // never increments this, so healthy files aren't dropped just because earlier files were
    // pathological (#211 review).
    if ABANDONED_PARSE_WORKERS.load(Ordering::Relaxed) >= *MAX_ABANDONED_PARSE_WORKERS {
        return None;
    }

    // Whoever flips this `false -> true` first OWNS the outcome — the worker if it finishes in
    // time, the caller if it times out first. This lets the caller count a worker as abandoned
    // exactly once and the worker un-count itself when it eventually finishes, with no race.
    let claimed = Arc::new(AtomicBool::new(false));
    let worker_claimed = Arc::clone(&claimed);
    let text = text.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().spawn(move || {
        let mut parser = Parser::new();
        let (tree, budget_cancelled) = if parser.set_language(&grammar).is_ok() {
            let deadline = Instant::now() + budget;
            let mut over_budget = |_state: &ParseState| {
                if Instant::now() >= deadline {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            let options = ParseOptions::new().progress_callback(&mut over_budget);
            let bytes = text.as_bytes();
            let len = bytes.len();
            let tree = parser.parse_with_options(
                &mut |i, _| if i < len { &bytes[i..] } else { &bytes[..0] },
                None,
                Some(options),
            );
            // tree-sitter returns `None` when the progress callback cancels an over-budget parse;
            // flag it so the caller memoizes the content (#211 review).
            let cancelled = tree.is_none() && Instant::now() >= deadline;
            (tree, cancelled)
        } else {
            (None, false)
        };
        // The receiver may already have given up (abandoned worker) — ignore the send error.
        let _ = tx.send((tree, budget_cancelled));
        // If the caller already abandoned us (it won the claim), we were counted — un-count now
        // that this thread is finally done.
        if worker_claimed.swap(true, Ordering::Relaxed) {
            ABANDONED_PARSE_WORKERS.fetch_sub(1, Ordering::Relaxed);
        }
    });
    // Thread creation can fail in a constrained environment; degrade to a parser failure rather
    // than panic (#211 review).
    if spawned.is_err() {
        return None;
    }

    match rx.recv_timeout(budget + PARSE_ABANDON_GRACE) {
        Ok((tree, budget_cancelled)) => {
            if budget_cancelled {
                memoize_timed_out(key);
            }
            tree
        },
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            memoize_timed_out(key);
            // Abandon: count this worker (until it finishes and un-counts itself) UNLESS it
            // actually completed first and we lost the claim race.
            if !claimed.swap(true, Ordering::Relaxed) {
                ABANDONED_PARSE_WORKERS.fetch_add(1, Ordering::Relaxed);
            }
            None
        },
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    pub name: String,
    pub qualified_name: String,
    /// The SEMANTIC scope path: the symbol's enclosing type/module/namespace names joined with
    /// `::`, ending in its own name (a method `new` in `impl Workspace` in `mod core` →
    /// `core::Workspace::new`; a free function → just its name). Distinct from
    /// `qualified_name` (file-path form, the stable identity for logical-symbol grouping +
    /// memory anchoring). `scope_path` ALIGNS with an edge's source-derived
    /// `target_qualified_name` (`Workspace::new`), so the resolver's strong qualified-match
    /// path fires instead of collapsing to collision-prone bare-name matching (#61).
    pub scope_path: String,
    pub kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub signature: Option<String>,
    pub docs: Option<String>,
    /// Test code — a test FILE (cross-language path conventions) or a language-specific test
    /// marker (Rust `#[test]`/`#[cfg(test)]`, Kotlin `@Test`, Python `test_*` / `*Test*` class
    /// / conftest). Persisted as `symbols.is_test` so clone detection can keep tests out of
    /// the corpus (tests are repetitive by construction, so they otherwise dominate near-clone
    /// results with noise).
    pub is_test: bool,
    pub facts: Vec<ParsedSymbolFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSymbolFact {
    pub kind: String,
    pub value: String,
}

const NAME_KINDS: &[&str] = &[
    "identifier",
    "type_identifier",
    "property_identifier",
    "field_identifier",
    "simple_identifier",
    "namespace_identifier",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserKind {
    Rust,
    TypeScript,
    Tsx,
    Kotlin,
    C,
    Cpp,
    Python,
    Markdown,
}

pub fn parser_kind(path: &Path, language: Language) -> ParserKind {
    match language {
        Language::Rust => ParserKind::Rust,
        Language::TypeScript =>
            if path.extension().and_then(|ext| ext.to_str()) == Some("tsx") {
                ParserKind::Tsx
            } else {
                ParserKind::TypeScript
            },
        Language::Kotlin => ParserKind::Kotlin,
        Language::C => ParserKind::C,
        Language::Cpp => ParserKind::Cpp,
        Language::Python => ParserKind::Python,
        Language::Markdown => ParserKind::Markdown,
    }
}

const PARSE_ERROR_MESSAGE: &str =
    "tree-sitter parse produced error nodes; partial structural index was retained";

fn grammar_for(kind: ParserKind) -> Option<tree_sitter::Language> {
    Some(match kind {
        ParserKind::Rust => tree_sitter_rust::LANGUAGE.into(),
        ParserKind::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ParserKind::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        ParserKind::Kotlin => tree_sitter_kotlin::LANGUAGE.into(),
        ParserKind::C => tree_sitter_c::LANGUAGE.into(),
        ParserKind::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        ParserKind::Python => tree_sitter_python::LANGUAGE.into(),
        ParserKind::Markdown => return None,
    })
}

/// A single tree-sitter parse of a file plus everything derived directly from the tree. The
/// full-rebuild prepare phase parses each file ONCE through this and feeds the tree to chunking,
/// symbols, and edges — instead of re-parsing the same file 4× (parse_error + chunker + symbols +
/// edges). `tree` is kept so callers can walk it (e.g. edge extraction) without re-parsing.
pub struct ParsedFile {
    tree: tree_sitter::Tree,
    pub symbols: Vec<ParsedSymbol>,
    pub has_error: bool,
}

impl ParsedFile {
    pub fn root(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// The parse-error message shape used historically by `parse_error`, or `None` if clean.
    pub fn parser_failure(&self) -> Option<String> {
        self.has_error.then(|| PARSE_ERROR_MESSAGE.to_string())
    }
}

/// Parse `text` once and collect its symbols. Returns `None` for languages without a structural
/// grammar (markdown) or if the parse fails outright.
pub fn parse_file(path: &Path, language: Language, text: &str) -> Option<ParsedFile> {
    let grammar = grammar_for(parser_kind(path, language))?;
    let tree = parse_within_budget(grammar, text, PARSE_BUDGET)?;
    let mut symbols = Vec::new();
    collect_symbols(path, language, text, tree.root_node(), &mut symbols);
    symbols.sort_by_key(|symbol| (symbol.start_byte, symbol.end_byte));
    symbols.dedup_by_key(|symbol| (symbol.start_byte, symbol.end_byte, symbol.name.clone()));
    let has_error = tree.root_node().has_error();
    Some(ParsedFile { tree, symbols, has_error })
}

pub fn parse_symbols(
    path: &Path,
    language: Language,
    text: &str,
) -> anyhow::Result<Vec<ParsedSymbol>> {
    match parse_file(path, language, text) {
        Some(parsed) => Ok(parsed.symbols),
        // Markdown (no grammar) yields no symbols; a hard parse failure is the error case.
        None if parser_kind(path, language) == ParserKind::Markdown => Ok(Vec::new()),
        None => Err(anyhow::anyhow!("tree-sitter parse failed")),
    }
}

pub fn parse_error(path: &Path, language: Language, text: &str) -> anyhow::Result<Option<String>> {
    match parse_file(path, language, text) {
        Some(parsed) => Ok(parsed.parser_failure()),
        None if parser_kind(path, language) == ParserKind::Markdown => Ok(None),
        None => Err(anyhow::anyhow!("tree-sitter parse failed")),
    }
}

fn collect_symbols(
    path: &Path,
    language: Language,
    text: &str,
    node: Node<'_>,
    out: &mut Vec<ParsedSymbol>,
) {
    if node.is_error() || node.is_missing() {
        return;
    }
    if let Some((kind, name_node)) = symbol_node(language, node, text) {
        let name = node_text(name_node, text).unwrap_or_default();
        if !name.is_empty() {
            // The span (chunk/embedding) is the matched `node`; the SIGNATURE is read from the
            // declaration node, which differs for a decorated Python def (the span starts at the
            // decorator, but the signature must be the `def`/`class` line).
            let signature_node = signature_source_node(language, node);
            out.push(make_symbol(path, language, text, node, signature_node, kind, name));
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbols(path, language, text, child, out);
    }
}

fn symbol_node<'a>(
    language: Language,
    node: Node<'a>,
    text: &str,
) -> Option<(&'static str, Node<'a>)> {
    let kind = node.kind();
    match language {
        Language::Rust => match kind {
            "function_item" => Some(("function", child_name(node)?)),
            "struct_item" => Some(("struct", child_name(node)?)),
            "enum_item" => Some(("enum", child_name(node)?)),
            "trait_item" => Some(("trait", child_name(node)?)),
            "impl_item" => Some(("impl", impl_name(node).unwrap_or(node))),
            "mod_item" => Some(("module", child_name(node)?)),
            "const_item" => Some(("const", child_name(node)?)),
            "static_item" => Some(("static", child_name(node)?)),
            "type_item" => Some(("type", child_name(node)?)),
            "macro_definition" => Some(("macro", child_name(node)?)),
            _ => None,
        },
        Language::TypeScript => match kind {
            "function_declaration" | "method_definition" | "generator_function_declaration" =>
                Some(("function", child_name(node)?)),
            "class_declaration" => Some(("class", child_name(node)?)),
            "interface_declaration" => Some(("interface", child_name(node)?)),
            "type_alias_declaration" => Some(("type", child_name(node)?)),
            "variable_declarator" | "public_field_definition" => Some(("const", child_name(node)?)),
            _ => None,
        },
        Language::Kotlin => match kind {
            "class_declaration" => Some(("class", child_name(node)?)),
            "object_declaration" => Some(("object", child_name(node)?)),
            "function_declaration" => Some(("function", child_name(node)?)),
            "property_declaration" => Some(("property", kotlin_property_name(node)?)),
            "companion_object" | "companion_object_declaration" =>
                Some(("object", companion_name(node).unwrap_or(node))),
            _ => None,
        },
        // C/C++ index DEFINITIONS, not bare declarations. A function prototype (`int foo(void);`,
        // a `declaration` with a `function_declarator`) and a bodyless type specifier — a forward
        // declaration (`struct X;`) or a use (`struct X *p`) — are NOT definitions, so they are not
        // emitted as symbols. Indexing them made `references_type` edges bind to a tiny
        // forward-decl/use occurrence instead of the real definition (#61: 18% type precision, vs
        // 85% for calls). `has_body` distinguishes a definition (`struct X { … }`) from the rest.
        Language::C => match kind {
            "function_definition" =>
                Some(("function", function_name(node).or_else(|| child_name(node))?)),
            "struct_specifier" if has_body(node) => Some(("struct", child_name(node)?)),
            "union_specifier" if has_body(node) => Some(("union", child_name(node)?)),
            "enum_specifier" if has_body(node) => Some(("enum", child_name(node)?)),
            "type_definition" => Some(("type", child_name(node)?)),
            "preproc_function_def" => Some(("macro", child_name(node)?)),
            _ => None,
        },
        Language::Cpp => match kind {
            "function_definition" =>
                Some(("function", function_name(node).or_else(|| child_name(node))?)),
            "class_specifier" if has_body(node) => Some(("class", child_name(node)?)),
            "struct_specifier" if has_body(node) => Some(("struct", child_name(node)?)),
            "union_specifier" if has_body(node) => Some(("union", child_name(node)?)),
            "enum_specifier" if has_body(node) => Some(("enum", child_name(node)?)),
            "type_definition" | "alias_declaration" => Some(("type", child_name(node)?)),
            "namespace_definition" => Some(("namespace", child_name(node)?)),
            "preproc_function_def" => Some(("macro", child_name(node)?)),
            _ => None,
        },
        Language::Python => match kind {
            // A decorated def OWNS the symbol span (so `@app.get(...)` / `@dataclass` / `@property`
            // decorator lines — which often define the API surface — are inside the chunk), using
            // the inner def's name + kind. The inner `function_definition`/`class_definition` arms
            // below are guarded so they don't ALSO emit a duplicate with the bare (decorator-less)
            // span.
            "decorated_definition" => {
                let inner = node.child_by_field_name("definition")?;
                let kind = match inner.kind() {
                    "function_definition" => "function",
                    "class_definition" => "class",
                    _ => return None,
                };
                Some((kind, child_name(inner)?))
            },
            "function_definition" if !python_parent_is_decorated(node) =>
                Some(("function", child_name(node)?)),
            "class_definition" if !python_parent_is_decorated(node) =>
                Some(("class", child_name(node)?)),
            // PEP 695 type alias (`type UserId = int`) — index the alias like Rust/TS/C++ type
            // aliases. `child_name` finds the first identifier (the alias name `UserId`).
            "type_alias_statement" => Some(("type", child_name(node)?)),
            // Constants are MODULE- or CLASS-level SCREAMING_SNAKE_CASE assignments only. Gating on
            // scope keeps function-local uppercase temporaries (`TIMEOUT = compute()`) out of the
            // symbol table, and the screaming-snake check keeps ordinary lowercase
            // locals/attributes out.
            "assignment" if python_assignment_is_const_scope(node) => {
                let target = node.child_by_field_name("left")?;
                let name = node_text(target, text)?;
                (target.kind() == "identifier" && is_screaming_snake_case(&name))
                    .then_some(("const", target))
            },
            _ => None,
        },
        Language::Markdown => None,
    }
}

/// `true` for a Python constant name (SCREAMING_SNAKE_CASE): at least one ASCII uppercase letter
/// and only uppercase / digits / underscore. Keeps `DEFAULT_NAME` but rejects `bridge_name` /
/// `Api`.
fn is_screaming_snake_case(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Whether a Python def node's parent is a `decorated_definition` (so the parent arm owns its
/// symbol span and the inner arm must not emit a duplicate).
fn python_parent_is_decorated(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| parent.kind() == "decorated_definition")
}

/// Whether a Python `assignment` is at module or class scope (a constant), vs a function-local
/// temporary. Walks ancestors to the first scope boundary: a `function_definition`/`lambda` means
/// local (not a constant); a `class_definition`/`module` means module/class level (a constant).
fn python_assignment_is_const_scope(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        match current.kind() {
            "function_definition" | "lambda" => return false,
            "class_definition" | "module" => return true,
            _ => ancestor = current.parent(),
        }
    }
    false
}

fn child_name(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(name);
    }

    let mut cursor = node.walk();
    if let Some(name) =
        node.named_children(&mut cursor).find(|child| NAME_KINDS.contains(&child.kind()))
    {
        return Some(name);
    }

    let mut cursor = node.walk();
    node.named_children(&mut cursor).find_map(|child| first_descendant_node(child, NAME_KINDS))
}

fn first_descendant_node<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            return Some(child);
        }
        if let Some(value) = first_descendant_node(child, kinds) {
            return Some(value);
        }
    }
    None
}

/// Whether a C/C++ `*_specifier` node carries its body (`field_declaration_list` /
/// `enumerator_list` via the grammar's `body` field) — i.e. it is a DEFINITION (`struct X { … }`),
/// not a forward declaration (`struct X;`) or a use (`struct X *p`). Only definitions are indexed
/// as symbols so `references_type` edges resolve to the real definition rather than a bodyless
/// occurrence (#61).
fn has_body(node: Node<'_>) -> bool {
    node.child_by_field_name("body").is_some()
}

/// The semantic scope path for a symbol node: enclosing type/module/namespace/trait names
/// (outermost first) joined with `::`, ending in the symbol's own `name`. A top-level free function
/// or type yields just its name. See [`ParsedSymbol::scope_path`].
fn scope_path(language: Language, node: Node<'_>, text: &str, name: &str) -> String {
    let mut segments = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if let Some(segment) = scope_segment(language, parent, text) {
            segments.push(segment);
        }
        current = parent.parent();
    }
    segments.reverse();
    segments.push(name.to_string());
    segments.join("::")
}

/// The scope-name contributed by an ENCLOSING node, if it introduces a named scope (a module, the
/// type an `impl` is for, a class/trait/namespace). Returns `None` for nodes that don't nest a
/// scope, so the walk skips blocks/expressions and only collects real path segments.
fn scope_segment(language: Language, node: Node<'_>, text: &str) -> Option<String> {
    let name_node = match (language, node.kind()) {
        (Language::Rust, "mod_item" | "trait_item") => child_name(node)?,
        (Language::Rust, "impl_item") => impl_name(node)?,
        (Language::TypeScript, "class_declaration" | "interface_declaration") => child_name(node)?,
        (Language::TypeScript, "internal_module" | "module" | "namespace_declaration") =>
            child_name(node)?,
        (Language::Kotlin, "class_declaration" | "object_declaration") => child_name(node)?,
        // Python nests methods in classes and closures in functions — both bound `scope_path`.
        (Language::Python, "class_definition" | "function_definition") => child_name(node)?,
        (Language::Cpp, "namespace_definition") => child_name(node)?,
        (
            Language::C | Language::Cpp,
            "struct_specifier" | "union_specifier" | "class_specifier",
        ) if has_body(node) => child_name(node)?,
        _ => return None,
    };
    node_text(name_node, text)
}

fn companion_name(node: Node<'_>) -> Option<Node<'_>> {
    for index in 0..node.child_count() {
        let Some(index) = u32::try_from(index).ok() else {
            continue;
        };
        if let Some(child) = node.child(index)
            && child.kind() == "companion"
        {
            return Some(child);
        }
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "simple_identifier" | "type_identifier"))
}

fn kotlin_property_name(node: Node<'_>) -> Option<Node<'_>> {
    child_name(kotlin_variable_declaration(node).unwrap_or(node))
}

fn kotlin_variable_declaration(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find_map(|child| {
        if child.kind() == "variable_declaration" {
            Some(child)
        } else if matches!(child.kind(), "modifiers" | "type_parameters" | "type_constraints") {
            None
        } else {
            kotlin_variable_declaration(child)
        }
    })
}

fn function_name(node: Node<'_>) -> Option<Node<'_>> {
    let declarator = first_descendant_node(node, &["function_declarator"]).unwrap_or(node);
    let name_root = declarator.child_by_field_name("declarator").unwrap_or(declarator);
    if NAME_KINDS.contains(&name_root.kind()) {
        return Some(name_root);
    }
    last_descendant_node(name_root, NAME_KINDS)
}

fn last_descendant_node<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let mut last = None;
    for child in node.named_children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            last = Some(child);
        }
        if let Some(value) = last_descendant_node(child, kinds) {
            last = Some(value);
        }
    }
    last
}

fn impl_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|child| {
        matches!(child.kind(), "type_identifier" | "generic_type" | "scoped_type_identifier")
    })
}

fn make_symbol(
    path: &Path,
    language: Language,
    text: &str,
    node: Node<'_>,
    // The declaration node the SIGNATURE is read from. Equals `node` except for a decorated Python
    // def, where `node` is the `decorated_definition` (so the chunk span includes the decorators)
    // but the signature must come from the inner `def`/`class` line.
    signature_node: Node<'_>,
    kind: &str,
    name: String,
) -> ParsedSymbol {
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();
    // tree-sitter already computed each node's 1-based line span during the parse — read it off the
    // node (O(1) struct field) instead of rescanning the file text for newlines. `row` is 0-based.
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    let scope_path = scope_path(language, node, text, &name);
    let is_test = detect_is_test(path, language, text, node, &scope_path, &name);
    ParsedSymbol {
        qualified_name: format!("{}::{name}", path.to_string_lossy().replace('\\', "/")),
        scope_path,
        name,
        kind: kind.to_string(),
        start_byte,
        end_byte,
        start_line,
        end_line,
        signature: signature_for(text, signature_node.start_byte(), signature_node.end_byte()),
        docs: docs_before(text, start_byte),
        is_test,
        facts: symbol_facts(language, text, node),
    }
}

/// Whether a symbol is test code — persisted as `symbols.is_test` so clone detection can exclude
/// it. Cross-language: a test FILE path (every language) OR a language-specific in-source marker.
/// Tests are repetitive by construction (fixture → call → assert), so leaving them in the clone
/// corpus floods near-clone results — the write-time clone check is the main consumer.
fn detect_is_test(
    path: &Path,
    language: Language,
    text: &str,
    node: Node<'_>,
    scope_path: &str,
    name: &str,
) -> bool {
    if is_test_path(path) {
        return true;
    }
    match language {
        // Rust unit tests live INLINE in source files, so the path check misses them: a `#[test]`
        // (or `#[tokio::test]`/`#[rstest]`/…) on the fn, or any ancestor `#[cfg(test)]` module
        // (catches test helpers too).
        Language::Rust =>
            rust_attribute_items(text, node)
                .iter()
                .any(|attribute| rust_attribute_is_test(attribute))
                || rust_in_cfg_test_module(node, text),
        // Kotlin: a JUnit `@Test`-family annotation on the fn (path catches `*Test.kt` /
        // `src/test/`).
        Language::Kotlin => kotlin_has_test_annotation(text, node),
        // Python: pytest/unittest by convention — `test_*` functions, methods of a `*Test*`/
        // `*TestCase` class (via scope_path), or anything in `conftest.py` (path).
        Language::Python => name.starts_with("test_") || scope_path_has_test_class(scope_path),
        // C / C++ / TypeScript: test bodies aren't named symbols (macros / closures), so the file
        // path conventions above carry these.
        _ => false,
    }
}

/// The CANONICAL cross-language test-path detector (#294) — the one reused by the indexer (the
/// `is_test` computation) AND the query layer (`staleness` test-file skip, `graph` test-callsite
/// filter, `repo_brief` support-path down-weight). A test path is any of: a test directory segment
/// (`tests`/`test`/`__tests__`/`__mocks__`/`spec`, case-insensitive), `conftest.py`, a `*.test.*` /
/// `*.spec.*` filename, or a stem like `test`/`tests` / `test_*` / `*_test` / `*_tests` / `*Test` /
/// `*Tests` / `*TestCase`. Takes `impl AsRef<Path>` so both `&Path` (parser) and `&str` (the query
/// callers' stored path strings) pass directly.
pub(crate) fn is_test_path(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    if path.components().filter_map(|component| component.as_os_str().to_str()).any(|segment| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            "tests" | "test" | "__tests__" | "__test__" | "__mocks__" | "spec" | "specs"
        )
    }) {
        return true;
    }
    let file = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if file == "conftest.py" {
        return true;
    }
    let lower = file.to_ascii_lowercase();
    if lower.contains(".test.") || lower.contains(".spec.") {
        return true;
    }
    let stem = file.split('.').next().unwrap_or(file);
    stem == "test"
        || stem == "tests"
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
        || stem.ends_with("TestCase")
}

/// A Rust attribute that marks a test function: `#[test]`, `#[tokio::test]`, `#[rstest]`,
/// `#[test_case(..)]`, etc. (a `#[cfg(test)]` is the MODULE marker — handled separately so it
/// doesn't read as a per-fn test attribute).
fn rust_attribute_is_test(attribute: &str) -> bool {
    let inner = attribute
        .trim()
        .trim_start_matches('#')
        .trim_start_matches('!')
        .trim_start_matches('[')
        .trim_end_matches(']');
    let head = inner.split(['(', '[']).next().unwrap_or_default().trim();
    let last = head.rsplit("::").next().unwrap_or(head).trim();
    last == "test" || last == "rstest" || last.starts_with("test_case")
}

/// Whether `node` is nested in any `#[cfg(test)]` module — catches inline unit tests AND their
/// helpers (a fixture fn in `#[cfg(test)] mod tests` has no `#[test]` of its own).
fn rust_in_cfg_test_module(node: Node<'_>, text: &str) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "mod_item"
            && rust_attribute_items(text, ancestor)
                .iter()
                .any(|attribute| attribute.contains("cfg") && attribute.contains("test"))
        {
            return true;
        }
        current = ancestor.parent();
    }
    false
}

/// Whether a Kotlin function carries a JUnit `@Test`-family annotation (in its `modifiers`).
fn kotlin_has_test_annotation(text: &str, node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(|child| {
        child.kind() == "modifiers"
            && node_text(child, text).as_deref().map(kotlin_modifiers_have_test).unwrap_or(false)
    })
}

fn kotlin_modifiers_have_test(modifiers: &str) -> bool {
    modifiers.split('@').skip(1).any(|annotation| {
        let name = annotation.split(['(', ' ', '\n', '\t', '\r']).next().unwrap_or_default();
        let last = name.rsplit('.').next().unwrap_or(name);
        matches!(last, "Test" | "ParameterizedTest" | "RepeatedTest" | "TestFactory")
    })
}

/// Whether a Python symbol's enclosing scope is a test class (`Test*` / `*Test` / `*TestCase`),
/// e.g. a `unittest.TestCase` subclass. `scope_path` is `Class::method`-style.
fn scope_path_has_test_class(scope_path: &str) -> bool {
    scope_path.split("::").any(|segment| {
        segment.starts_with("Test") || segment.ends_with("Test") || segment.ends_with("TestCase")
    })
}

/// The node a symbol's signature is read from. For a decorated Python def this is the inner
/// `def`/`class` (so the signature is the declaration, not the `@decorator` line the span starts
/// at); for everything else it's the matched node itself.
fn signature_source_node<'a>(language: Language, node: Node<'a>) -> Node<'a> {
    if language == Language::Python
        && node.kind() == "decorated_definition"
        && let Some(inner) = node.child_by_field_name("definition")
    {
        return inner;
    }
    node
}

fn symbol_facts(language: Language, text: &str, node: Node<'_>) -> Vec<ParsedSymbolFact> {
    if language != Language::Rust {
        return Vec::new();
    }
    let mut facts = Vec::new();
    for attribute in rust_attribute_items(text, node) {
        if rust_attribute_is_uniffi_export(&attribute) {
            facts.push(ParsedSymbolFact {
                kind: "rust_attr".to_string(),
                value: "uniffi_export".to_string(),
            });
        }
    }
    facts.sort_by(|left, right| (&left.kind, &left.value).cmp(&(&right.kind, &right.value)));
    facts.dedup();
    facts
}

fn rust_attribute_items(text: &str, node: Node<'_>) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "attribute_item" {
            attributes.push(node_text(child, text).unwrap_or_default());
        }
    }

    let mut preceding = Vec::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        if previous.kind() != "attribute_item" {
            break;
        }
        preceding.push(node_text(previous, text).unwrap_or_default());
        sibling = previous.prev_named_sibling();
    }
    preceding.reverse();
    preceding.extend(attributes);
    preceding
}

fn rust_attribute_is_uniffi_export(attribute: &str) -> bool {
    attribute.contains("uniffi::export") || attribute.contains("::uniffi::export")
}

fn node_text(node: Node<'_>, text: &str) -> Option<String> {
    node.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned)
}

fn signature_for(text: &str, start_byte: usize, end_byte: usize) -> Option<String> {
    text.get(start_byte..end_byte)?
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
}

fn docs_before(text: &str, start_byte: usize) -> Option<String> {
    let before = text.get(..start_byte)?;
    let mut docs = Vec::new();
    for line in before.lines().rev() {
        let trimmed = line.trim();
        if matches!(trimmed, "/**" | "*/") {
            continue;
        } else if let Some(doc_line) = clean_doc_comment_line(trimmed) {
            docs.push(doc_line);
        } else if trimmed.is_empty() {
            continue;
        } else {
            break;
        }
    }
    docs.reverse();
    (!docs.is_empty()).then(|| docs.join("\n"))
}

fn clean_doc_comment_line(trimmed: &str) -> Option<String> {
    let line = if trimmed.starts_with("///") {
        trimmed.trim_start_matches('/')
    } else if trimmed.starts_with('*') || trimmed.starts_with("/**") {
        trimmed.trim_start_matches('/').trim_start_matches('*').trim_end_matches('/')
    } else {
        return None;
    }
    .trim();

    (!line.is_empty()).then(|| line.to_string())
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn parse_within_budget_parses_normal_input_within_budget() {
        // The budgeted/worker-thread path must produce a tree for an ordinary file — no false
        // timeout, same result as a plain parse (#210).
        let grammar = grammar_for(ParserKind::Rust).expect("rust grammar");
        let tree = parse_within_budget(grammar, "fn main() { let x = 1 + 2; }", PARSE_BUDGET);
        assert!(tree.is_some(), "a normal file must parse within budget");
        assert!(!tree.unwrap().root_node().has_error(), "valid input parses cleanly");
    }

    #[test]
    fn parse_within_budget_zero_budget_does_not_hang() {
        // A near-zero budget must return promptly (the worker is cancelled or abandoned), never
        // hang — the whole point of the guard (#210). A large input ensures the parse can't finish
        // before the deadline check.
        let grammar = grammar_for(ParserKind::Rust).expect("rust grammar");
        let big = "fn f() { let x = vec![1, 2, 3]; }\n".repeat(20_000);
        // Returns (None on cancel/abandon, or a partial tree) — the assertion is that it RETURNS.
        let _ = parse_within_budget(grammar, &big, std::time::Duration::ZERO);
    }

    #[test]
    fn parse_within_budget_skips_content_memoized_as_timed_out() {
        // Content already recorded as pathological is short-circuited to a parser failure WITHOUT
        // spawning another doomed worker — so a single bad file is never parsed twice within a pass
        // (symbols/edges/chunk-fallback) nor re-parsed across watcher passes (#211 review). Use a
        // unique string so the process-global memo can't collide with another test.
        let src = "fn memoized_timeout_marker_8f3a() {}";
        TIMED_OUT_PARSES.lock().expect("poison-set lock").insert(content_key(src));
        let grammar = grammar_for(ParserKind::Rust).expect("rust grammar");
        assert!(
            parse_within_budget(grammar, src, PARSE_BUDGET).is_none(),
            "memoized-as-timed-out content must short-circuit to a parser failure"
        );
    }
}

#[cfg(test)]
mod is_test_detection {
    use std::path::Path;

    use super::parse_symbols;
    use crate::language::Language;

    fn is_test(path: &str, language: Language, text: &str, name: &str) -> bool {
        parse_symbols(Path::new(path), language, text)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| panic!("symbol `{name}` not parsed"))
            .is_test
    }

    #[test]
    fn rust_test_attribute_and_cfg_test_module() {
        let src = "pub fn real(a: i32) -> i32 { a + 1 }\n#[cfg(test)]\nmod tests {\nfn helper() \
                   -> i32 { 1 }\n#[test]\nfn checks_things() { assert_eq!(helper(), 1); }\n}\n";
        assert!(!is_test("src/lib.rs", Language::Rust, src, "real"), "plain fn is not a test");
        assert!(is_test("src/lib.rs", Language::Rust, src, "checks_things"), "#[test] fn");
        assert!(
            is_test("src/lib.rs", Language::Rust, src, "helper"),
            "a helper in a #[cfg(test)] module is test code too"
        );
    }

    #[test]
    fn rust_tokio_test_attribute() {
        let src = "#[tokio::test]\nasync fn runs() {}\n";
        assert!(is_test("src/lib.rs", Language::Rust, src, "runs"));
    }

    #[test]
    fn python_test_function_and_test_class() {
        let src = "def real():\n    return 1\n\ndef test_thing():\n    assert real() == \
                   1\n\nclass TestSuite:\n    def checks(self):\n        assert True\n";
        assert!(!is_test("src/app.py", Language::Python, src, "real"));
        assert!(is_test("src/app.py", Language::Python, src, "test_thing"), "test_* function");
        assert!(is_test("src/app.py", Language::Python, src, "checks"), "method of a Test* class");
    }

    #[test]
    fn kotlin_test_annotation() {
        let src = "class Thing {\n    @Test\n    fun verifies() {}\n    fun real() {}\n}\n";
        assert!(is_test("src/Main.kt", Language::Kotlin, src, "verifies"), "@Test fun");
        assert!(!is_test("src/Main.kt", Language::Kotlin, src, "real"), "plain fun");
    }

    #[test]
    fn test_file_paths_across_languages() {
        // A plain function in a conventional test FILE is test code regardless of markers/language.
        assert!(is_test("tests/integration.rs", Language::Rust, "fn anything() {}\n", "anything"));
        assert!(is_test(
            "test_app.py",
            Language::Python,
            "def anything():\n    pass\n",
            "anything"
        ));
        assert!(is_test(
            "src/__tests__/util.ts",
            Language::TypeScript,
            "function anything() {}\n",
            "anything"
        ));
        assert!(is_test("FooTest.kt", Language::Kotlin, "fun anything() {}\n", "anything"));
        assert!(is_test(
            "widget.test.ts",
            Language::TypeScript,
            "function anything() {}\n",
            "anything"
        ));
    }

    #[test]
    fn canonical_is_test_path_covers_the_union_of_conventions() {
        use super::is_test_path;
        // Directory segments (case-insensitive), incl. repo_brief's `__mocks__`.
        for p in [
            "crates/x/tests/foo.rs",
            "src/test/foo.rs",
            "web/__tests__/util.ts",
            "web/__mocks__/api.ts",
            "app/Spec/x.rb",
            "pkg/Tests/Thing.cs",
        ] {
            assert!(is_test_path(p), "dir segment: {p}");
        }
        // Filenames, incl. repo_brief's `tests.rs`/`_tests.rs` and cross-language stems.
        for p in [
            "src/widget_test.rs",
            "src/widget_tests.rs",
            "pkg/foo_test.go",
            "tests.rs",
            "test_app.py",
            "conftest.py",
            "app/Button.spec.tsx",
            "app/Button.test.tsx",
            "FooTest.kt",
            "FooTestCase.java",
        ] {
            assert!(is_test_path(p), "filename: {p}");
        }
        // Negatives — including near-misses that must NOT match.
        for p in ["src/widget.rs", "src/contest.rs", "src/latest.rs", "lib/manifest.rs"] {
            assert!(!is_test_path(p), "non-test: {p}");
        }
    }
}
