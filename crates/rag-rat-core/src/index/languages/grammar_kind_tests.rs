//! Every node-kind string a language backend dispatches on must name a node its pinned grammar has.
//!
//! Extractors match on kind STRINGS, and every one of those matches has a fall-through: a name the
//! grammar does not have never matches, so the construct it was meant to catch is silently
//! skipped. A typo, a name copied from a different grammar, or a grammar upgrade that renames a
//! node all fail the same quiet way — nothing errors, the index just loses those symbols, scopes
//! or edges. These tests read each backend's own source with the Rust grammar, collect the string
//! literals it compares a node kind against, and ask the compiled grammar whether each one exists.
//!
//! Reading the dispatch out of the source (rather than keeping a hand-copied list next to it) is
//! what makes the test check the real thing: a new arm is covered the moment it is written. A
//! "kind position" is any of:
//! - an arm pattern of a `match` whose scrutinee reads a kind (`node.kind()`, a `kind` binding);
//! - the patterns of `matches!(<kind>, ...)`;
//! - the other side of `==` / `!=` against a kind;
//! - the arguments of a call whose name says it takes a kind (`direct_child_of_kind(node,
//!   "init")`);
//! - the receiver of `.contains(<kind>)` and the value of a `*KIND*` const or static.
//!
//! This checks the names are SPELLED for real nodes of the right grammar; the per-language edge and
//! symbol tests cover what the dispatch does with them.

use tree_sitter::{Language as Grammar, Node, Parser};

use crate::index::edges::named_children;
use crate::index::parser::{self, ParserKind};

/// One kind-position string literal, with the line it sits on for the failure message.
#[derive(Debug)]
struct KindName {
    file: &'static str,
    line: usize,
    text: String,
}

/// A source file read as dispatch code: its name (for messages) and its text.
type Source = (&'static str, &'static str);

/// One parsed source file of the set under test.
struct Parsed {
    file: &'static str,
    source: &'static str,
    tree: tree_sitter::Tree,
}

/// What the whole source set declares, so a kind position can see through one level of naming:
/// the helpers that take a kind (`fn has_kind(node, kind: &str)`, `first_descendant_node(node,
/// kinds: &[&str])`) and the consts a dispatch checks membership in.
struct Declarations<'tree> {
    kind_taking_fns: Vec<&'static str>,
    /// Name, defining file, value.
    consts: Vec<(&'static str, &'static str, Node<'tree>)>,
}

/// The string literals in kind position across `sources`, skipping `#[cfg(test)]` / `#[test]`
/// items — test fixtures name symbol kinds and snippets, not dispatch.
fn kind_names(sources: &[Source]) -> Vec<KindName> {
    let parsed = parse_all(sources);
    let mut declarations = Declarations { kind_taking_fns: Vec::new(), consts: Vec::new() };
    for file in &parsed {
        for node in production_nodes(file) {
            collect_declaration(node, file, &mut declarations);
        }
    }
    // Backends call the shared helpers (`parser::first_descendant_node`) too. Only their
    // signatures are borrowed: a shared const is checked by the shared test, against every grammar.
    let shared = parse_all(SHARED_SOURCES);
    let mut shared_declarations = Declarations { kind_taking_fns: Vec::new(), consts: Vec::new() };
    for file in &shared {
        for node in production_nodes(file) {
            collect_declaration(node, file, &mut shared_declarations);
        }
    }
    declarations.kind_taking_fns.extend(shared_declarations.kind_taking_fns);
    let mut out = Vec::new();
    for file in &parsed {
        for node in production_nodes(file) {
            for (defined_in, literal) in kind_position_literals(node, file, &declarations) {
                let source = parsed.iter().find(|p| p.file == defined_in).expect("a parsed file");
                out.push(KindName {
                    file: defined_in,
                    line: literal.start_position().row + 1,
                    text: literal_text(literal, source.source),
                });
            }
        }
    }
    out
}

fn parse_all(sources: &[Source]) -> Vec<Parsed> {
    sources
        .iter()
        .map(|&(file, source)| {
            let mut parser = Parser::new();
            parser.set_language(&tree_sitter_rust::LANGUAGE.into()).expect("rust grammar loads");
            let tree = parser.parse(source, None).expect("backend source parses");
            Parsed { file, source, tree }
        })
        .collect()
}

/// Every node of `file` outside test items.
fn production_nodes(file: &Parsed) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut stack = vec![file.tree.root_node()];
    while let Some(node) = stack.pop() {
        if is_test_item(node, file.source) {
            continue;
        }
        out.push(node);
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    out
}

fn is_test_item(node: Node<'_>, source: &str) -> bool {
    if !matches!(node.kind(), "mod_item" | "function_item") {
        return false;
    }
    let mut previous = node.prev_sibling();
    while let Some(attribute) = previous.filter(|sibling| sibling.kind() == "attribute_item") {
        let text = text(attribute, source);
        if text.contains("cfg(test)") || text == "#[test]" {
            return true;
        }
        previous = attribute.prev_sibling();
    }
    false
}

fn collect_declaration<'tree>(
    node: Node<'tree>,
    file: &Parsed,
    declarations: &mut Declarations<'tree>,
) {
    let source = file.source;
    match node.kind() {
        "function_item" => {
            let takes_kind = node.child_by_field_name("parameters").is_some_and(|parameters| {
                named_children(parameters)
                    .filter_map(|parameter| parameter.child_by_field_name("pattern"))
                    .any(|pattern| matches!(text(pattern, source), "kind" | "kinds"))
            });
            if takes_kind && let Some(name) = node.child_by_field_name("name") {
                declarations.kind_taking_fns.push(text(name, source));
            }
        },
        "const_item" | "static_item" => {
            if let (Some(name), Some(value)) =
                (node.child_by_field_name("name"), node.child_by_field_name("value"))
            {
                declarations.consts.push((text(name, source), file.file, value));
            }
        },
        _ => {},
    }
}

/// The string-literal nodes `node` puts in kind position (its own construct only; nested
/// constructs are visited on their own).
fn kind_position_literals<'tree>(
    node: Node<'tree>,
    file: &Parsed,
    declarations: &Declarations<'tree>,
) -> Vec<(&'static str, Node<'tree>)> {
    let source = file.source;
    let here = |literals: Vec<Node<'tree>>| literals.into_iter().map(|l| (file.file, l)).collect();
    match node.kind() {
        "match_expression" => {
            let reads_kind =
                node.child_by_field_name("value").is_some_and(|value| reads_kind(value, source));
            let Some(body) = node.child_by_field_name("body").filter(|_| reads_kind) else {
                return Vec::new();
            };
            // The arm's pattern, not its `if` guard: a guard tests anything, a pattern the kind.
            here(
                named_children(body)
                    .filter_map(|arm| arm.child_by_field_name("pattern"))
                    .flat_map(|pattern| {
                        let guard = pattern.child_by_field_name("condition");
                        named_children(pattern).filter(move |part| Some(*part) != guard)
                    })
                    .flat_map(string_literals)
                    .collect(),
            )
        },
        "macro_invocation" if macro_name(node, source) == Some("matches") => {
            let Some(tokens) = named_children(node).find(|child| child.kind() == "token_tree")
            else {
                return Vec::new();
            };
            // `matches!(scrutinee, patterns)`: the scrutinee is everything before the first comma.
            let mut cursor = tokens.walk();
            let children: Vec<Node<'_>> = tokens.children(&mut cursor).collect();
            let Some(comma) = children.iter().position(|child| child.kind() == ",") else {
                return Vec::new();
            };
            let scrutinee = &source[children[0].end_byte()..children[comma].start_byte()];
            if !text_reads_kind(scrutinee) {
                return Vec::new();
            }
            here(children[comma..].iter().copied().flat_map(string_literals).collect())
        },
        "binary_expression" => {
            let (Some(left), Some(operator), Some(right)) = (
                node.child_by_field_name("left"),
                node.child_by_field_name("operator"),
                node.child_by_field_name("right"),
            ) else {
                return Vec::new();
            };
            if !matches!(operator.kind(), "==" | "!=") {
                return Vec::new();
            }
            if reads_kind(left, source) {
                here(string_literals(right))
            } else if reads_kind(right, source) {
                here(string_literals(left))
            } else {
                Vec::new()
            }
        },
        "call_expression" => {
            let (Some(function), Some(arguments)) =
                (node.child_by_field_name("function"), node.child_by_field_name("arguments"))
            else {
                return Vec::new();
            };
            let name = function
                .child_by_field_name("field")
                .or_else(|| function.child_by_field_name("name"))
                .unwrap_or(function);
            let name = text(name, source);
            if name == "contains" && named_children(arguments).any(|arg| reads_kind(arg, source)) {
                function
                    .child_by_field_name("value")
                    .map(|set| literals_through_consts(set, file, declarations))
                    .unwrap_or_default()
            } else if name.contains("kind") || declarations.kind_taking_fns.contains(&name) {
                literals_through_consts(arguments, file, declarations)
            } else {
                Vec::new()
            }
        },
        "const_item" | "static_item"
            if node
                .child_by_field_name("name")
                .is_some_and(|name| text(name, source).contains("KIND")) =>
            here(node.child_by_field_name("value").map(string_literals).unwrap_or_default()),
        _ => Vec::new(),
    }
}

/// The literals in `node`, plus those of any const of the set it names (`NAME_KINDS`,
/// `super::IDENTIFIER_KINDS`).
fn literals_through_consts<'tree>(
    node: Node<'tree>,
    file: &Parsed,
    declarations: &Declarations<'tree>,
) -> Vec<(&'static str, Node<'tree>)> {
    let source = file.source;
    let mut out: Vec<_> = string_literals(node).into_iter().map(|l| (file.file, l)).collect();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier"
            && let Some(&(_, defined_in, value)) =
                declarations.consts.iter().find(|(name, ..)| *name == text(current, source))
        {
            out.extend(string_literals(value).into_iter().map(|l| (defined_in, l)));
        }
        let mut cursor = current.walk();
        stack.extend(current.children(&mut cursor));
    }
    out
}

/// Whether `node` is an expression that evaluates to a node kind.
fn reads_kind(node: Node<'_>, source: &str) -> bool {
    text_reads_kind(text(node, source))
}

fn text_reads_kind(text: &str) -> bool {
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let text = text.trim_start_matches(['&', '*']);
    text.contains(".kind()")
        || text.contains(".grammar_name()")
        || text == "kind"
        // A span's `kind` field — but a parser `Symbol`'s `kind` is a symbol kind (`function`,
        // `precedence_group`), not a node kind.
        || (text.ends_with(".kind") && !text.contains("symbol"))
}

fn macro_name<'source>(node: Node<'_>, source: &'source str) -> Option<&'source str> {
    node.child_by_field_name("macro").map(|name| text(name, source))
}

fn string_literals(node: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "string_literal" {
            out.push(current);
            continue;
        }
        let mut cursor = current.walk();
        stack.extend(current.children(&mut cursor));
    }
    out
}

/// The literal's value: its content with `\"` / `\\` escapes (a `"\""` quote-token kind) decoded.
fn literal_text(literal: Node<'_>, source: &str) -> String {
    named_children(literal)
        .filter_map(|part| match part.kind() {
            "string_content" => Some(text(part, source)),
            "escape_sequence" => text(part, source).get(1..),
            _ => None,
        })
        .collect()
}

fn text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    node.utf8_text(source.as_bytes()).expect("utf-8 source")
}

fn grammars(kinds: &[ParserKind]) -> Vec<Grammar> {
    kinds.iter().map(|&kind| parser::grammar_for(kind).expect("a structural grammar")).collect()
}

fn has_named(grammars: &[Grammar], name: &str) -> bool {
    grammars.iter().any(|grammar| grammar.id_for_node_kind(name, true) != 0)
}

fn has_anonymous(grammars: &[Grammar], name: &str) -> bool {
    grammars.iter().any(|grammar| grammar.id_for_node_kind(name, false) != 0)
}

/// Assert every kind-position literal in `sources` names a node of `grammars` (any of them — a
/// backend serving several grammars, like TS/TSX or C/C++, may name a node only one of them has).
///
/// A word must be a NAMED node, unless it is listed in `keywords` — the keyword tokens the backend
/// deliberately dispatches on. Requiring the list is what keeps a keyword from passing for a node:
/// Kotlin has an `import` keyword AND an `import` node, and a grammar that renames the node leaves
/// the keyword behind, so a named-or-anonymous check would keep passing. Punctuation (`,`, `->`)
/// can only be a token, so it needs no listing.
fn assert_kinds_exist(
    sources: &[Source],
    grammar_kinds: &[ParserKind],
    keywords: &[&str],
) -> Vec<KindName> {
    let grammars = grammars(grammar_kinds);
    let names = kind_names(sources);
    let problems: Vec<String> = names
        .iter()
        .filter_map(|name| {
            let listed = keywords.contains(&name.text.as_str());
            let punctuation = !name.text.chars().any(char::is_alphanumeric);
            let problem =
                match (has_named(&grammars, &name.text), has_anonymous(&grammars, &name.text)) {
                    (true, _) if listed => "is a named node but listed as a keyword",
                    (true, _) => return None,
                    (false, true) if listed || punctuation => return None,
                    (false, true) =>
                        "exists only as a keyword token; list it as a keyword if that is what the \
                         dispatch means",
                    (false, false) => "is not a node kind of the grammar",
                };
            Some(format!("{}:{} `{}` {problem}", name.file, name.line, name.text))
        })
        .collect();
    assert!(
        problems.is_empty(),
        "{grammar_kinds:?} dispatch names kinds its grammar does not have — the construct they \
         were meant to match is silently skipped:\n{}",
        problems.join("\n")
    );
    let unused: Vec<&&str> =
        keywords.iter().filter(|listed| !names.iter().any(|name| name.text == **listed)).collect();
    assert!(unused.is_empty(), "keywords listed but no longer dispatched on: {unused:?}");
    names
}

/// The extractor must see the dispatch at all, or every check passes vacuously.
fn assert_found_at_least(names: &[KindName], minimum: usize) {
    assert!(
        names.len() >= minimum,
        "expected at least {minimum} kind-position literals, found {}: {names:?}",
        names.len()
    );
}

macro_rules! sources {
    ($($path:literal),+ $(,)?) => {
        &[$(($path, include_str!($path))),+]
    };
}

const ALL_GRAMMARS: &[ParserKind] = &[
    ParserKind::Rust,
    ParserKind::TypeScript,
    ParserKind::Tsx,
    ParserKind::Kotlin,
    ParserKind::C,
    ParserKind::Cpp,
    ParserKind::Python,
    ParserKind::Swift,
    ParserKind::Go,
];

/// The grammar-agnostic code every backend shares: the parser walk and the edge helpers.
const SHARED_SOURCES: &[Source] =
    sources!["mod.rs", "../parser.rs", "../edges/helpers.rs", "../edges/extract/mod.rs"];

const RUST_SOURCES: &[Source] =
    sources!["rust/mod.rs", "rust/dispatch.rs", "rust/edges.rs", "rust/binders.rs"];
const TYPESCRIPT_SOURCES: &[Source] = sources!["typescript/mod.rs", "typescript/edges.rs"];
const KOTLIN_SOURCES: &[Source] = sources!["kotlin/mod.rs", "kotlin/edges.rs"];
const C_FAMILY_SOURCES: &[Source] = sources!["c_family/mod.rs", "c_family/edges.rs"];
const PYTHON_SOURCES: &[Source] = sources!["python/mod.rs", "python/edges.rs"];
const SWIFT_SOURCES: &[Source] = sources!["swift/mod.rs", "swift/edges.rs", "swift/syntax.rs"];
const GO_SOURCES: &[Source] = sources!["go/mod.rs", "go/edges.rs"];

/// The clone engine's whole source tree, relative to this file.
const CLONE_SOURCES: &[Source] = sources![
    "../../../../rag-rat-clones/src/bag_blob.rs",
    "../../../../rag-rat-clones/src/lib.rs",
    "../../../../rag-rat-clones/src/normalize.rs",
    "../../../../rag-rat-clones/src/tokens.rs",
    "../../../../rag-rat-clones/src/refine/mod.rs",
    "../../../../rag-rat-clones/src/refine/align.rs",
    "../../../../rag-rat-clones/src/refine/budget.rs",
    "../../../../rag-rat-clones/src/refine/cache.rs",
    "../../../../rag-rat-clones/src/refine/score.rs",
    "../../../../rag-rat-clones/src/refine/signature.rs",
    "../../../../rag-rat-clones/src/refine/split.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/mod.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/alignment.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/build.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/classify.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/render.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/spans.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/statement.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/types.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/values.rs",
    "../../../../rag-rat-clones/src/refine/antiunify/widen.rs",
];

#[test]
fn every_kind_the_rust_backend_names_exists_in_the_grammar() {
    let names = assert_kinds_exist(RUST_SOURCES, &[ParserKind::Rust], &["as"]);
    assert_found_at_least(&names, 200);
}

#[test]
fn every_kind_the_typescript_backend_names_exists_in_the_grammar() {
    let names =
        assert_kinds_exist(TYPESCRIPT_SOURCES, &[ParserKind::TypeScript, ParserKind::Tsx], &[
            "export", "default", "abstract",
        ]);
    assert_found_at_least(&names, 10);
}

#[test]
fn every_kind_the_kotlin_backend_names_exists_in_the_grammar() {
    let names = assert_kinds_exist(KOTLIN_SOURCES, &[ParserKind::Kotlin], &["companion"]);
    assert_found_at_least(&names, 10);
}

#[test]
fn every_kind_the_c_family_backend_names_exists_in_the_grammar() {
    let names = assert_kinds_exist(C_FAMILY_SOURCES, &[ParserKind::C, ParserKind::Cpp], &[]);
    assert_found_at_least(&names, 20);
}

#[test]
fn every_kind_the_python_backend_names_exists_in_the_grammar() {
    let names = assert_kinds_exist(PYTHON_SOURCES, &[ParserKind::Python], &[]);
    assert_found_at_least(&names, 60);
}

#[test]
fn every_kind_the_swift_backend_names_exists_in_the_grammar() {
    let names = assert_kinds_exist(SWIFT_SOURCES, &[ParserKind::Swift], &[
        "actor",
        "class",
        "deinit",
        "enum",
        "extension",
        "init",
        "operator",
        "struct",
        "subscript",
    ]);
    assert_found_at_least(&names, 120);
}

/// The clone engine (normalizer, anti-unify classifier, signature recovery) dispatches on the kinds
/// of every grammar, so a name there must be a node of at least one of them. A per-grammar name
/// (a Rust type kind, the C `character` leaf) passes when ANY grammar has it.
#[test]
fn every_kind_the_clone_engine_names_exists_in_some_grammar() {
    let names = assert_kinds_exist(CLONE_SOURCES, ALL_GRAMMARS, &[]);
    assert_found_at_least(&names, 40);
}

#[test]
fn every_kind_the_go_backend_names_exists_in_the_grammar() {
    let names = assert_kinds_exist(GO_SOURCES, &[ParserKind::Go], &[]);
    assert_found_at_least(&names, 10);
}

/// The shared tables serve every grammar, so a name there must be a node of at least one of them.
/// Identifier and name kinds are per-backend consts, checked against their own grammar above.
#[test]
fn every_kind_the_shared_tables_name_exists_in_some_grammar() {
    let names = assert_kinds_exist(SHARED_SOURCES, ALL_GRAMMARS, &[]);
    assert_found_at_least(&names, 4);
}

/// Assert every production `.rs` file under `dir` (recursively) is read by a tripwire above, or is
/// excluded with a reason. `prefix` turns a path relative to `dir` into the spelling the source
/// lists use. Test files (`tests.rs`, `*_tests.rs`, `test_support.rs`) hold fixtures, not dispatch.
///
/// A hand-kept list goes stale the day a backend grows a file; without this check, the kinds that
/// file dispatches on would go unchecked with every tripwire still green.
fn assert_sources_complete(
    dir: &std::path::Path,
    prefix: &str,
    listed: &[&[Source]],
    excluded: &[(&str, &str)],
) {
    let mut unlisted = Vec::new();
    let mut seen_exclusions = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("source directory reads") {
            let path = entry.expect("directory entry reads").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(stem) = path.extension().filter(|ext| *ext == "rs").and(path.file_stem())
            else {
                continue;
            };
            let stem = stem.to_str().expect("utf-8 file name");
            if stem == "tests" || stem.ends_with("_tests") || stem == "test_support" {
                continue;
            }
            let relative = path.strip_prefix(dir).expect("under dir");
            let name = format!("{prefix}{}", rag_rat_base::paths::path_string(relative));
            if excluded.iter().any(|(file, _)| *file == name) {
                seen_exclusions.push(name);
            } else if !listed.iter().flat_map(|list| list.iter()).any(|(file, _)| *file == name) {
                unlisted.push(name);
            }
        }
    }
    assert!(
        unlisted.is_empty(),
        "source files no node-kind tripwire reads — add each to its backend's source list, or to \
         the exclusions with a reason: {unlisted:?}"
    );
    let stale: Vec<&&str> = excluded
        .iter()
        .map(|(file, _)| file)
        .filter(|file| !seen_exclusions.iter().any(|seen| seen == **file))
        .collect();
    assert!(stale.is_empty(), "excluded files that no longer exist: {stale:?}");
}

#[test]
fn every_backend_source_file_is_read_by_a_tripwire() {
    let languages = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/index/languages");
    assert_sources_complete(
        &languages,
        "",
        &[
            RUST_SOURCES,
            TYPESCRIPT_SOURCES,
            KOTLIN_SOURCES,
            C_FAMILY_SOURCES,
            PYTHON_SOURCES,
            SWIFT_SOURCES,
            GO_SOURCES,
            SHARED_SOURCES,
        ],
        &[
            ("markdown/mod.rs", "markdown is chunked as prose; it dispatches on no node kind"),
            (
                "c_family/query_spike.rs",
                "a `#[cfg(test)]` query prototype, not production dispatch",
            ),
        ],
    );
}

#[test]
fn every_clone_engine_source_file_is_read_by_the_tripwire() {
    let clones = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../rag-rat-clones/src");
    assert_sources_complete(&clones, "../../../../rag-rat-clones/src/", &[CLONE_SOURCES], &[]);
}
