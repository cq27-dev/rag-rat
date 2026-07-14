use std::path::Path;

use tree_sitter::Node;

use super::{EdgeBackend, KindPreference, ParserBackend, ResolverPolicy, SymbolMatch};
use crate::index::edges::{EdgeCandidate, IndexedSymbol};
use crate::index::parser::{self, ParserKind};

mod edges;
pub(super) mod syntax;

pub(super) static SUPPORT: Swift = Swift;

pub(super) struct Swift;

fn is_local_qualified_root(root: &str) -> bool {
    matches!(root, "Self" | "self" | "super")
}

impl ParserBackend for Swift {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &[
            "actor",
            "class",
            "constructor",
            "enum",
            "enum_case",
            "extension",
            "function",
            "macro",
            "operator",
            "precedence_group",
            "property",
            "protocol",
            "struct",
            "type",
        ]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Swift
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        symbol_node(node)
    }

    fn symbol_name(&self, node: Node<'_>, name_node: Node<'_>, text: &str) -> String {
        let name = parser::node_text(name_node, text).unwrap_or_default();
        let is_extension = node.kind() == "class_declaration"
            && node
                .child_by_field_name("declaration_kind")
                .is_some_and(|kind| kind.kind() == "extension");
        if is_extension { format!("extension {name}") } else { name }
    }

    fn for_each_symbol<'tree>(
        &self,
        node: Node<'tree>,
        text: &str,
        emit: &mut dyn FnMut(Node<'tree>, SymbolMatch<'tree>),
    ) {
        if matches!(node.kind(), "property_declaration" | "protocol_property_declaration") {
            let mut cursor = node.walk();
            let patterns = node.children_by_field_name("name", &mut cursor).collect::<Vec<_>>();
            let bindings = patterns.into_iter().flat_map(property_names).collect::<Vec<_>>();
            let multiple_bindings = bindings.len() > 1;
            for name in bindings {
                // A multi-binding declaration needs one symbol/chunk per binding. Use the bound
                // identifier as its unique span; single-binding properties retain the complete
                // declaration span and the caller always takes its signature from there.
                emit(if multiple_bindings { name } else { node }, ("property", name));
            }
        } else if node.kind() == "enum_entry" {
            let mut cursor = node.walk();
            let names = node.children_by_field_name("name", &mut cursor).collect::<Vec<_>>();
            let multiple_cases = names.len() > 1;
            for name in names {
                emit(if multiple_cases { name } else { node }, ("enum_case", name));
            }
        } else if let Some(symbol) = self.symbol_node(node, text) {
            emit(node, symbol);
        }
    }

    fn for_each_recovered_symbol<'tree>(
        &self,
        node: Node<'tree>,
        text: &str,
        emit: &mut dyn FnMut(Node<'tree>, SymbolMatch<'tree>),
    ) {
        if let Some(name) = recovered_precedence_group_name(node, text) {
            // tree-sitter-swift 0.7 recovers a valid comma-separated higherThan/lowerThan clause
            // as an ERROR node. Preserve the declaration symbol so recovered dependency edges
            // still have their real precedence-group owner.
            emit(node, ("precedence_group", name));
        }
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        match node.kind() {
            "class_declaration" | "protocol_declaration" =>
                syntax::qualified_name(parser::child_name(node)?, text),
            "function_declaration" => parser::node_text(parser::child_name(node)?, text),
            "init_declaration" => parser::node_text(direct_child_of_kind(node, "init")?, text),
            "deinit_declaration" => parser::node_text(direct_child_of_kind(node, "deinit")?, text),
            "subscript_declaration" =>
                parser::node_text(direct_child_of_kind(node, "subscript")?, text),
            _ => None,
        }
    }

    /// Swift's two test frameworks, neither of which the path heuristic alone catches:
    /// swift-testing marks tests with an `@Test` / `@Suite` ATTRIBUTE, and XCTest by inheriting
    /// `XCTestCase` with `test`-prefixed methods. A test living outside a `Tests/` directory —
    /// an XCTestCase beside the code it exercises, a `@Test` in a sample target — is otherwise
    /// indexed as production source and never demoted in search or `repo_brief`. Every other
    /// language with a test convention (Rust `#[test]`, Kotlin `@Test`, Python `test_*`)
    /// already overrides this; Swift declared none.
    fn is_test_symbol(&self, text: &str, node: Node<'_>, scope_path: &str, name: &str) -> bool {
        has_test_attribute(node, text)
            || inherits_xctest_case(node, text)
            || (name.starts_with("test") && scope_path_has_test_type(scope_path))
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "import_declaration"
    }
}

/// A swift-testing `@Test` / `@Suite` attribute on this declaration. tree-sitter-swift nests
/// attributes under a `modifiers` child (`modifiers` → `attribute` → `user_type`), so this descends
/// one level rather than scanning the declaration's direct children. Matched on the attribute's own
/// name node, so `@TestHarness` does not read as `@Test`.
fn has_test_attribute(node: Node<'_>, text: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| child.kind() == "modifiers")
        .flat_map(|modifiers| {
            let mut inner = modifiers.walk();
            modifiers.children(&mut inner).collect::<Vec<_>>()
        })
        .filter(|child| child.kind() == "attribute")
        .any(|attribute| {
            syntax::identifier_nodes(attribute)
                .first()
                .and_then(|name| parser::node_text(*name, text))
                .is_some_and(|name| matches!(name.as_str(), "Test" | "Suite"))
        })
}

/// An XCTest base class in this type's inheritance clause (`class FooTests: XCTestCase`). The
/// conformance list is where XCTest declares itself; there is no attribute to key on.
fn inherits_xctest_case(node: Node<'_>, text: &str) -> bool {
    if node.kind() != "class_declaration" {
        return false;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| child.kind() == "inheritance_specifier")
        .filter_map(|specifier| parser::node_text(specifier, text))
        .any(|specifier| specifier.trim_end_matches("()").ends_with("XCTestCase"))
}

/// A `test`-prefixed method inside a type whose name reads as a test type (`FooTests`,
/// `FooTestCase`) — the XCTest convention, mirroring the Python backend's
/// `scope_path_has_test_class` for members of a class the parser sees only by name.
fn scope_path_has_test_type(scope_path: &str) -> bool {
    scope_path
        .split("::")
        .any(|segment| segment.ends_with("Tests") || segment.ends_with("TestCase"))
}

fn recovered_precedence_group_name<'tree>(node: Node<'tree>, text: &str) -> Option<Node<'tree>> {
    (node.kind() == "ERROR"
        && parser::node_text(node, text)?.trim_start().starts_with("precedencegroup"))
    .then(|| syntax::identifier_nodes(node).into_iter().next())
    .flatten()
}

fn symbol_node(node: Node<'_>) -> Option<SymbolMatch<'_>> {
    match node.kind() {
        "class_declaration" => {
            let declaration_kind = node.child_by_field_name("declaration_kind")?;
            let kind = match declaration_kind.kind() {
                "actor" => "actor",
                "class" => "class",
                "enum" => "enum",
                "extension" => "extension",
                "struct" => "struct",
                _ => return None,
            };
            Some((kind, parser::child_name(node)?))
        },
        "protocol_declaration" => Some(("protocol", parser::child_name(node)?)),
        "function_declaration" | "protocol_function_declaration" =>
            Some(("function", parser::child_name(node)?)),
        "init_declaration" => Some(("constructor", direct_child_of_kind(node, "init")?)),
        "deinit_declaration" => Some(("function", direct_child_of_kind(node, "deinit")?)),
        "subscript_declaration" => Some(("function", direct_child_of_kind(node, "subscript")?)),
        "typealias_declaration" | "associatedtype_declaration" =>
            Some(("type", parser::child_name(node)?)),
        "macro_declaration" => Some(("macro", parser::child_name(node)?)),
        "operator_declaration" => Some(("operator", operator_name(node)?)),
        "precedence_group_declaration" => Some(("precedence_group", parser::child_name(node)?)),
        _ => None,
    }
}

fn operator_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut saw_operator_keyword = false;
    (0..node.child_count()).find_map(|index| {
        let child = node.child(u32::try_from(index).ok()?)?;
        if !saw_operator_keyword {
            saw_operator_keyword = child.kind() == "operator";
            return None;
        }
        if child.is_extra() {
            return None;
        }
        syntax::is_operator_token(child.kind()).then_some(child)
    })
}

fn property_names(pattern: Node<'_>) -> Vec<Node<'_>> {
    let mut names = Vec::new();
    let mut stack = vec![pattern];
    let mut cursor = pattern.walk();
    while let Some(node) = stack.pop() {
        if let Some(name) = node.child_by_field_name("bound_identifier") {
            names.push(name);
            continue;
        }
        // Destructuring patterns are aliases in tree-sitter-swift and do not expose the
        // `bound_identifier` field. A binding is a pattern whose sole named child is its
        // identifier. Tuple labels are direct identifier children of the OUTER pattern alongside
        // nested pattern children, so this shape deliberately excludes them.
        if node.kind() == "pattern"
            && node.named_child_count() == 1
            && let Some(name) =
                node.named_child(0).filter(|child| child.kind() == "simple_identifier")
        {
            names.push(name);
            continue;
        }
        let mut children = node.named_children(&mut cursor).collect::<Vec<_>>();
        children.reverse();
        stack.extend(children);
    }
    names
}

fn direct_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    (0..node.child_count()).find_map(|index| {
        let index = u32::try_from(index).ok()?;
        node.child(index).filter(|child| child.kind() == kind)
    })
}

impl EdgeBackend for Swift {
    fn edges(
        &self,
        text: &str,
        node: Node<'_>,
        symbols: &[IndexedSymbol],
        path: &Path,
        out: &mut Vec<EdgeCandidate>,
    ) {
        edges::swift_edges(text, node, symbols, path, out);
    }
}

impl ResolverPolicy for Swift {
    fn preferred_kinds(&self, edge_kind: &str) -> Option<KindPreference> {
        let symbol_kinds: &'static [&'static str] = match edge_kind {
            "calls_name" => &["function", "method", "constructor", "enum_case"],
            "constructs" => &["struct", "enum", "class", "object", "actor"],
            "implements" => &["protocol", "class"],
            "references_type" =>
                &["struct", "enum", "type", "class", "object", "actor", "protocol"],
            "uses_macro" => &["macro"],
            "uses_operator" => &["operator"],
            "uses_precedence_group" => &["precedence_group"],
            _ => return None,
        };
        Some(KindPreference { symbol_kinds, same_language_only: true })
    }

    fn collapse_same_named_declarations(&self) -> bool {
        false
    }

    fn reference_is_unresolvable(&self, edge_kind: &str, name: &str) -> bool {
        edge_kind == "imports" || (edge_kind == "calls_name" && operator_has_builtin_meaning(name))
    }

    fn suppress_unresolved_reference(&self, edge_kind: &str, evidence: Option<&str>) -> bool {
        matches!(edge_kind, "uses_macro" | "references_type")
            && evidence.is_some_and(|source| source.trim_start().starts_with('@'))
    }

    fn is_local_qualified_root(&self, root: &str) -> bool {
        is_local_qualified_root(root)
    }
}

fn operator_has_builtin_meaning(name: &str) -> bool {
    matches!(
        name,
        "=" | "+"
            | "-"
            | "*"
            | "/"
            | "%"
            | "=="
            | "!="
            | "==="
            | "!=="
            | ">"
            | "<"
            | ">="
            | "<="
            | "&&"
            | "||"
            | "!"
            | "&"
            | "|"
            | "^"
            | "~"
            | "<<"
            | ">>"
            | "+="
            | "-="
            | "*="
            | "/="
            | "%="
            | "&="
            | "|="
            | "^="
            | "<<="
            | ">>="
            | "&+"
            | "&-"
            | "&*"
            | "&+="
            | "&-="
            | "&*="
            | "~="
            | "??"
            | "..."
            | "..<"
    )
}
