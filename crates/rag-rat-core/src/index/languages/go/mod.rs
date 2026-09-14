use std::path::Path;

use tree_sitter::Node;

use super::{ParserBackend, SymbolMatch};
use crate::index::edges::named_children;
use crate::index::parser::{self, ParserKind};

mod edges;
pub(super) use edges::{RESOLVER_POLICY, go_edges};

pub(super) static SUPPORT: Go = Go;

pub(super) struct Go;

impl ParserBackend for Go {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["const", "function", "interface", "method", "struct", "type", "var"]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Go
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        symbol_node(node)
    }

    /// Go has no nesting for the declarations we index — every `func`, `type`, `const`, and `var`
    /// spec is a direct child of the file (or of a `type_declaration` / `var_declaration` grouping
    /// node that introduces no scope of its own). The one real scope Go DOES have is the method
    /// receiver: `func (s *Server) Start()` is `Server`'s method exactly as Rust's
    /// `impl Server { fn start() }` is, and the receiver type is the only thing that distinguishes
    /// two same-named methods on different types in one file.
    ///
    /// The receiver lives ON the `method_declaration` node rather than on an ancestor, so unlike
    /// Rust's `impl_item` it cannot be picked up by the ancestor walk in `parser::scope_path`.
    /// `symbol_name` therefore qualifies the method name itself (`Server.Start`), and this returns
    /// `None` for every node — Go contributes no ancestor-derived scope segments.
    fn scope_segment(&self, _node: Node<'_>, _text: &str) -> Option<String> {
        None
    }

    /// A method's identity in Go is receiver type + name: `Start` alone collides across every type
    /// in the package that has one. Qualifying with the receiver (`Server.Start`) is the same
    /// disambiguation Rust gets for free from its `impl` ancestor, using Go's own selector syntax
    /// so the rendered name is what a Go developer would write to call it.
    fn symbol_name(&self, node: Node<'_>, name_node: Node<'_>, text: &str) -> String {
        let name = parser::node_text(name_node, text).unwrap_or_default();
        if node.kind() != "method_declaration" {
            return name;
        }
        match receiver_type_name(node, text) {
            Some(receiver) if !name.is_empty() => format!("{receiver}.{name}"),
            _ => name,
        }
    }

    /// `const ( A = 1; B = 2 )` and `var x, y int` bind SEVERAL names under one spec node — the
    /// grammar marks `const_spec`/`var_spec`'s `name` field `multiple: true`. `child_by_field_name`
    /// would return only the first, silently dropping every later binding.
    fn for_each_symbol<'tree>(
        &self,
        node: Node<'tree>,
        text: &str,
        emit: &mut dyn FnMut(Node<'tree>, SymbolMatch<'tree>),
    ) {
        let kind = match node.kind() {
            "const_spec" => "const",
            "var_spec" => "var",
            _ => {
                if let Some(symbol) = self.symbol_node(node, text) {
                    emit(node, symbol);
                }
                return;
            },
        };
        let mut cursor = node.walk();
        let names = node.children_by_field_name("name", &mut cursor).collect::<Vec<_>>();
        super::emit_bindings(node, kind, names, emit);
    }

    /// Import declarations carry no symbol a reader would search for, and Go's convention of a
    /// single grouped `import ( ... )` block at the top of every file makes them pure plumbing —
    /// the same reason the Swift backend excludes its `import_declaration`.
    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "import_declaration"
    }
}

/// Classify one Go declaration node.
///
/// `const_spec` / `var_spec` are deliberately absent: they can bind multiple names and are handled
/// by `for_each_symbol` above. Everything here binds exactly one name.
fn symbol_node(node: Node<'_>) -> Option<SymbolMatch<'_>> {
    match node.kind() {
        "function_declaration" => Some(("function", parser::child_name(node)?)),
        // The grammar makes `receiver` a REQUIRED field of `method_declaration`, so the node kind
        // alone already means "func with a receiver" — no extra receiver check is needed to
        // separate it from a plain `function_declaration`.
        "method_declaration" => Some(("method", parser::child_name(node)?)),
        // `type Foo struct{...}` / `type Foo interface{...}` / `type Foo Bar` all parse as
        // `type_spec`; only the `type` field says which. `type Foo = Bar` is a DIFFERENT node kind
        // (`type_alias`) in tree-sitter-go and is always a plain type.
        "type_spec" => {
            let kind = match node.child_by_field_name("type").map(|child| child.kind()) {
                Some("struct_type") => "struct",
                Some("interface_type") => "interface",
                _ => "type",
            };
            Some((kind, parser::child_name(node)?))
        },
        "type_alias" => Some(("type", parser::child_name(node)?)),
        _ => None,
    }
}

/// The receiver TYPE name of a `method_declaration` — `Server` for every one of `(s Server)`,
/// `(s *Server)`, and `(s *Server[T])`.
///
/// The receiver is a `parameter_list` holding one `parameter_declaration` whose `type` field is the
/// receiver type. That type is wrapped by `pointer_type` for the (overwhelmingly common) pointer
/// receiver and by `generic_type` for a generic one, so both wrappers are unwrapped to reach the
/// bare `type_identifier`.
fn receiver_type_name(node: Node<'_>, text: &str) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let declaration =
        named_children(receiver).find(|child| child.kind() == "parameter_declaration")?;
    let mut current = declaration.child_by_field_name("type")?;
    // Bounded: each step strips exactly one wrapper and a receiver type nests at most a couple
    // deep (`*Server[T]`), so this cannot spin on a malformed tree.
    loop {
        current = match current.kind() {
            // `pointer_type` exposes its pointee as an unnamed-field child, not a named field.
            "pointer_type" => current.named_child(0)?,
            "generic_type" => current.child_by_field_name("type")?,
            _ => break,
        };
    }
    parser::node_text(current, text)
}

#[cfg(test)]
mod tests {
    use rag_rat_base::language::Language;
    use tree_sitter::Parser;

    use super::{Go, SUPPORT};
    use crate::index::edges::named_children;
    use crate::index::languages::ParserBackend;
    use crate::index::parser::{self, ParserKind};

    /// Every `(kind, name)` the Go backend emits for `source`, in document order — the same
    /// `for_each_symbol` + `symbol_name` pair the real parser walk drives, so these tests exercise
    /// the production path rather than a test-only reimplementation of it.
    fn symbols(source: &str) -> Vec<(&'static str, String)> {
        rag_rat_base::stack::grow_stack(|| symbols_impl(source))
    }

    fn symbols_impl(source: &str) -> Vec<(&'static str, String)> {
        let mut parser = Parser::new();
        parser
            .set_language(&parser::grammar_for(ParserKind::Go).expect("go grammar"))
            .expect("set go language");
        let tree = parser.parse(source, None).expect("parse go source");

        let mut found = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            // Mirrors `parser::collect_symbols`: error subtrees are pruned rather than descended
            // (this backend recovers no symbols from them) and missing nodes are skipped.
            if node.is_error() || node.is_missing() {
                continue;
            }
            SUPPORT.for_each_symbol(node, source, &mut |_span, (kind, name_node)| {
                let name = SUPPORT.symbol_name(node, name_node, source);
                if !name.is_empty() {
                    found.push((kind, name));
                }
            });
            // Children are pushed in REVERSE so the LIFO stack pops them in document order —
            // the same traversal `parser::collect_symbols` runs, so `found` needs no later sort.
            let children = named_children(node).collect::<Vec<_>>();
            stack.extend(children.into_iter().rev());
        }
        found
    }

    #[test]
    fn struct_declaration_produces_a_struct_symbol() {
        // Arrange
        let source = "package main\n\ntype Server struct {\n\tAddr string\n}\n";

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![("struct", "Server".to_string())]);
    }

    #[test]
    fn interface_declaration_produces_an_interface_symbol() {
        // Arrange
        let source = "package main\n\ntype Reader interface {\n\tRead(p []byte) (int, error)\n}\n";

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![("interface", "Reader".to_string())]);
    }

    #[test]
    fn function_declaration_produces_a_function_symbol() {
        // Arrange
        let source = "package main\n\nfunc Serve(addr string) error {\n\treturn nil\n}\n";

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![("function", "Serve".to_string())]);
    }

    #[test]
    fn pointer_receiver_method_produces_a_method_symbol_qualified_by_its_receiver() {
        // Arrange
        let source = "package main\n\nfunc (s *Server) Start() error {\n\treturn nil\n}\n";

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![("method", "Server.Start".to_string())]);
    }

    #[test]
    fn value_receiver_method_produces_a_method_symbol() {
        // Arrange
        let source = "package main\n\nfunc (s Server) Addr() string {\n\treturn s.addr\n}\n";

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![("method", "Server.Addr".to_string())]);
    }

    /// Two methods of the same name on different receivers must stay distinguishable — the whole
    /// reason `symbol_name` qualifies with the receiver type.
    #[test]
    fn same_named_methods_on_different_receivers_stay_distinct() {
        // Arrange
        let source = concat!(
            "package main\n\n",
            "func (s *Server) Close() error { return nil }\n",
            "func (c *Client) Close() error { return nil }\n",
        );

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![
            ("method", "Server.Close".to_string()),
            ("method", "Client.Close".to_string()),
        ]);
    }

    /// A generic receiver wraps the type in `generic_type` on top of `pointer_type`; both wrappers
    /// must be unwrapped or the receiver would read as `Stack[T]` (or fail outright).
    #[test]
    fn generic_pointer_receiver_resolves_to_the_bare_receiver_type() {
        // Arrange
        let source = "package main\n\nfunc (s *Stack[T]) Push(v T) {}\n";

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![("method", "Stack.Push".to_string())]);
    }

    /// A `func` assigned to a local is a `func_literal`, not a `function_declaration` — it must not
    /// be indexed as a top-level function.
    #[test]
    fn function_literals_are_not_indexed_as_declarations() {
        // Arrange
        let source = "package main\n\nfunc outer() {\n\tinner := func() {}\n\t_ = inner\n}\n";

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![("function", "outer".to_string())]);
    }

    #[test]
    fn type_alias_and_named_type_both_produce_a_type_symbol() {
        // Arrange
        let source = "package main\n\ntype Celsius float64\n\ntype Alias = Celsius\n";

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![("type", "Celsius".to_string()), ("type", "Alias".to_string())]);
    }

    /// `const_spec` / `var_spec` mark their `name` field `multiple` in the grammar; a
    /// `child_by_field_name` implementation would silently keep only `Ready` and `x`.
    #[test]
    fn grouped_const_and_var_specs_emit_every_binding() {
        // Arrange
        let source = concat!(
            "package main\n\n",
            "const (\n\tReady = 1\n\tDone = 2\n)\n\n",
            "var x, y int\n",
        );

        // Act
        let found = symbols(source);

        // Assert
        assert_eq!(found, vec![
            ("const", "Ready".to_string()),
            ("const", "Done".to_string()),
            ("var", "x".to_string()),
            ("var", "y".to_string()),
        ]);
    }

    #[test]
    fn package_only_file_produces_no_symbols() {
        // Arrange
        let source = "package main\n";

        // Act
        let found = symbols(source);

        // Assert
        assert!(found.is_empty(), "expected no symbols, got {found:?}");
    }

    #[test]
    fn malformed_source_produces_no_panic() {
        // Arrange
        let source = "package !!!\n\nfunc ( { struct interface }}} type = = =\n\x00\u{feff}";

        // Act
        let found = symbols(source);

        // Assert — bounded error recovery: the parse yields ERROR nodes that the walk prunes, so
        // no symbol survives, and crucially nothing panics on the way (NUL / BOM bytes included).
        assert!(found.is_empty(), "expected no symbols from malformed source, got {found:?}");
    }

    #[test]
    fn imports_and_comments_are_plumbing() {
        // Arrange
        let source = "package main\n\n// a doc comment\nimport \"fmt\"\n";
        let mut parser = Parser::new();
        parser
            .set_language(&parser::grammar_for(ParserKind::Go).expect("go grammar"))
            .expect("set go language");
        let tree = parser.parse(source, None).expect("parse go source");
        let children = named_children(tree.root_node()).collect::<Vec<_>>();

        // Act
        let plumbing = children
            .iter()
            .filter(|child| SUPPORT.is_plumbing_node(**child))
            .map(|child| child.kind())
            .collect::<Vec<_>>();

        // Assert
        assert_eq!(plumbing, vec!["comment", "import_declaration"]);
    }

    #[test]
    fn declared_symbol_kinds_are_the_agreed_set() {
        // Arrange / Act
        let kinds = Go.symbol_kinds();

        // Assert
        assert_eq!(kinds, &["const", "function", "interface", "method", "struct", "type", "var"]);
    }

    #[test]
    fn go_backend_reports_the_go_parser_kind() {
        // Arrange / Act
        let kind = Go.parser_kind(std::path::Path::new("main.go"));

        // Assert
        assert_eq!(kind, ParserKind::Go);
        assert!(parser::grammar_for(kind).is_some());
        assert_eq!(Language::Go.as_db_str(), "go");
    }
}
