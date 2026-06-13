use std::path::Path;

use tree_sitter::Node;

use crate::language::Language;

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
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar).ok()?;
    let tree = parser.parse(text, None)?;
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
    if let Some((kind, name_node)) = symbol_node(language, node) {
        let name = node_text(name_node, text).unwrap_or_default();
        if !name.is_empty() {
            out.push(make_symbol(path, language, text, node, kind, name));
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbols(path, language, text, child, out);
    }
}

fn symbol_node(language: Language, node: Node<'_>) -> Option<(&'static str, Node<'_>)> {
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
        Language::Markdown => None,
    }
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
    ParsedSymbol {
        qualified_name: format!("{}::{name}", path.to_string_lossy().replace('\\', "/")),
        scope_path,
        name,
        kind: kind.to_string(),
        start_byte,
        end_byte,
        start_line,
        end_line,
        signature: signature_for(text, start_byte, end_byte),
        docs: docs_before(text, start_byte),
        facts: symbol_facts(language, text, node),
    }
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
