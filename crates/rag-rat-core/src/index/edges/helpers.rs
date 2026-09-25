use std::collections::BTreeSet;

use super::*;
use crate::index::collect_rows;

pub(crate) fn target_qualified_name(node: Node<'_>, text: &str) -> Option<String> {
    let function = node.child_by_field_name("function").unwrap_or(node);
    let value = degeneric_path(&node_text(function, text));
    (value.contains("::") || value.contains('.')).then(|| value.replace('.', "::"))
}

/// Prepared source-owner lookup for edge extraction.
///
/// Construction sweeps symbol interval boundaries once and records the selected owner for each
/// run. Lookups then binary-search those runs instead of scanning every symbol for every edge.
pub(crate) struct SymbolLocator<'symbols> {
    symbols: &'symbols [IndexedSymbol],
    runs: Vec<(usize, Option<usize>)>,
}

impl<'symbols> SymbolLocator<'symbols> {
    pub(crate) fn new(symbols: &'symbols [IndexedSymbol]) -> Self {
        let mut events = Vec::with_capacity(symbols.len().saturating_mul(2));
        for (index, symbol) in symbols.iter().enumerate() {
            events.push((symbol.start_byte, true, index));
            if let Some(after_end) = symbol.end_byte.checked_add(1) {
                events.push((after_end, false, index));
            }
        }
        // `false < true`, so an interval ending immediately before a new one starts is removed
        // before the new interval is selected at that byte.
        events.sort_unstable_by_key(|&(byte, add, index)| (byte, add, index));

        let key = |index: usize| {
            let symbol = &symbols[index];
            (symbol.end_byte.saturating_sub(symbol.start_byte), index)
        };
        let is_special =
            |index: usize| matches!(symbols[index].kind.as_str(), "const" | "property" | "static");
        let mut active = BTreeSet::new();
        let mut active_non_special = BTreeSet::new();
        let mut runs = Vec::with_capacity(events.len());
        let mut cursor = 0;
        while cursor < events.len() {
            let byte = events[cursor].0;
            while cursor < events.len() && events[cursor].0 == byte {
                let (_, add, index) = events[cursor];
                if add {
                    active.insert(key(index));
                    if !is_special(index) {
                        active_non_special.insert(key(index));
                    }
                } else {
                    active.remove(&key(index));
                    if !is_special(index) {
                        active_non_special.remove(&key(index));
                    }
                }
                cursor += 1;
            }
            let selected = active.first().map(|&(_, index)| index).and_then(|smallest| {
                if is_special(smallest) {
                    active_non_special.first().map(|&(_, index)| index).or(Some(smallest))
                } else {
                    Some(smallest)
                }
            });
            if runs.last().is_none_or(|&(_, previous)| previous != selected) {
                runs.push((byte, selected));
            }
        }
        Self { symbols, runs }
    }

    pub(crate) fn find(&self, byte: usize) -> Option<&'symbols IndexedSymbol> {
        let index = self.find_index_by(byte, || {});
        index.map(|index| &self.symbols[index])
    }

    fn find_index_by(&self, byte: usize, mut probe: impl FnMut()) -> Option<usize> {
        let mut low = 0;
        let mut high = self.runs.len();
        while low < high {
            probe();
            let middle = low + (high - low) / 2;
            if self.runs[middle].0 <= byte {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low.checked_sub(1).and_then(|index| self.runs[index].1)
    }
}

/// The smallest-span symbol whose byte range covers `byte` (a call/reference site's owner). When
/// that smallest is a `const`/`property`/`static` — a value binding whose initializer is where the
/// call actually lives, so the enclosing definition is the better owner — the smallest-span
/// container that ISN'T one of those is preferred instead, falling back to the value binding when
/// none exists.
///
/// One O(S) pass, no per-call allocation or sort (#519): the old filter-into-`Vec` + stable
/// sort-by-span ran once per edge candidate (O(E·S) plus an alloc+sort each). Tracking the two
/// running minima gives byte-identical selection — a strict `<` update keeps the FIRST symbol of an
/// equal-span tie, exactly as the stable sort did.
#[cfg(test)]
fn containing_symbol(symbols: &[IndexedSymbol], byte: usize) -> Option<&IndexedSymbol> {
    let span = |symbol: &IndexedSymbol| symbol.end_byte.saturating_sub(symbol.start_byte);
    let is_special =
        |symbol: &IndexedSymbol| matches!(symbol.kind.as_str(), "const" | "property" | "static");
    let mut smallest: Option<&IndexedSymbol> = None;
    let mut smallest_non_special: Option<&IndexedSymbol> = None;
    for symbol in symbols {
        if symbol.start_byte > byte || symbol.end_byte < byte {
            continue;
        }
        if smallest.is_none_or(|best| span(symbol) < span(best)) {
            smallest = Some(symbol);
        }
        if !is_special(symbol) && smallest_non_special.is_none_or(|best| span(symbol) < span(best))
        {
            smallest_non_special = Some(symbol);
        }
    }
    let smallest = smallest?;
    if is_special(smallest) { smallest_non_special.or(Some(smallest)) } else { Some(smallest) }
}
/// A node's named children, in order, owning the cursor that tree-sitter's
/// [`Node::named_children`] borrows — so a child scan reads as one expression. Whole-tree walks
/// that deliberately reuse ONE cursor across every node keep calling the method directly.
pub(crate) fn named_children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut cursor = node.walk();
    let mut on_child = cursor.goto_first_child();
    std::iter::from_fn(move || {
        while on_child {
            let child = cursor.node();
            on_child = cursor.goto_next_sibling();
            if child.is_named() {
                return Some(child);
            }
        }
        None
    })
}

/// A trailing turbofish (`f::<T>`) wraps the callee path in a `generic_function`; unwrap it so the
/// type arguments' identifiers / byte ranges aren't mistaken for the callee.
pub(crate) fn unwrap_generic_function(function: Node<'_>) -> Node<'_> {
    if function.kind() == "generic_function" {
        function.child_by_field_name("function").unwrap_or(function)
    } else {
        function
    }
}

/// The callee NAME token of a Rust call, read from grammar fields: a plain `identifier`, the
/// `name` of a `scoped_identifier` (`a::b::c`), or the `field` of a `field_expression`
/// (`recv.method`), through parentheses and a turbofish (`f::<T>`). Every other callee is a VALUE
/// with no name of its own — a subscript (`handlers[key](x)`), a call result (`make(a)(b)`), a `?`
/// (`get(k)?(x)`), a tuple field (`self.0(x)`), a deref, a closure — and yields `None`: the
/// identifiers inside it are an index, an argument or a parameter, and no callee name is better
/// than a wrong one.
fn rust_callee_name_node(call: Node<'_>) -> Option<Node<'_>> {
    let mut callee = call.child_by_field_name("function")?;
    loop {
        callee = match callee.kind() {
            "parenthesized_expression" => callee.named_child(0)?,
            "generic_function" => callee.child_by_field_name("function")?,
            "identifier" => return Some(callee),
            "scoped_identifier" => return callee.child_by_field_name("name"),
            "field_expression" =>
                return callee
                    .child_by_field_name("field")
                    .filter(|field| field.kind() == "field_identifier"),
            _ => return None,
        };
    }
}

/// The name a Rust call expression calls — see [`rust_callee_name_node`].
pub(crate) fn call_target_name(node: Node<'_>, text: &str) -> Option<String> {
    rust_callee_name_node(node)
        .and_then(|name| name.utf8_text(text.as_bytes()).ok())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}
/// The callee identifier node for a call expression — the same token [`call_target_name`] names,
/// returned as a node so its byte range can be recorded (SCIP occurrences key on the identifier's
/// position, #67).
pub(crate) fn call_target_node(node: Node<'_>) -> Option<Node<'_>> {
    rust_callee_name_node(node)
}
/// The name a call hangs off — the head of `Type::method` / `receiver.method`.
///
/// The `::` split is [`scope_grammar::segments`]' TOP-LEVEL one. `degeneric_path` deliberately
/// keeps a `::` that sits inside a `(…)`/`[…]` group, because there it is argument text rather
/// than a path separator, so splitting on every `::` would take the head from the arguments:
/// `conn.execute("…", rusqlite::params![…]).unwrap` hangs off `conn`, not off the `rusqlite` in
/// its argument list, and `stmt.query_map(.., |row| row.get::<_, String>(0)).unwrap` hangs off
/// `stmt`, not off the `get` buried in the closure.
pub(crate) fn scoped_receiver_name(node: Node<'_>, text: &str) -> Option<String> {
    let function = node.child_by_field_name("function").unwrap_or(node);
    let value = degeneric_path(&node_text(function, text));
    let head = match scope_grammar::segments(&value).as_slice() {
        [head, _, ..] => head,
        _ if value.contains('.') => value.split('.').next()?,
        _ => return None,
    };
    Some(short_name(head.trim()).to_string()).filter(|name| !name.is_empty())
}
pub(crate) fn child_name_text(node: Node<'_>, text: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|child| child.utf8_text(text.as_bytes()).ok())
        .map(ToOwned::to_owned)
}
/// The first identifier under `node`, in document order. `kinds` is the calling backend's
/// identifier node kinds (its `IDENTIFIER_KINDS`), as in every identifier helper here: each
/// grammar names its identifiers differently, so the backend states them.
pub(crate) fn first_identifier_text(node: Node<'_>, text: &str, kinds: &[&str]) -> Option<String> {
    // grow_stack: this recurses to full subtree depth; a hostile deeply-nested callee must grow
    // the stack, not overflow it (#543).
    rag_rat_base::stack::grow_stack(|| {
        for child in named_children(node) {
            if kinds.contains(&child.kind()) {
                return child.utf8_text(text.as_bytes()).ok().map(ToOwned::to_owned);
            }
            if let Some(value) = first_identifier_text(child, text, kinds) {
                return Some(value);
            }
        }
        None
    })
}
pub(crate) fn last_identifier_text(node: Node<'_>, text: &str, kinds: &[&str]) -> Option<String> {
    identifiers_under(node, text, kinds).into_iter().last()
}

/// An identifier token captured together with the source text at that exact node.
///
/// Keeping both representations in one segment prevents callers from walking a subtree twice and
/// relying on two parallel vectors to stay positionally aligned.
pub(crate) struct IdentifierSegment<'tree> {
    node: Node<'tree>,
    text: String,
}

/// The identifier segments of a member / callee chain, qualifier first.
pub(crate) struct IdentifierPath<'tree> {
    segments: Vec<IdentifierSegment<'tree>>,
    /// Where the written qualified path starts. A member that spells its own scope
    /// (`w->Widget::run`) restarts the path: the receiver is still `w`, but the qualified target
    /// is `Widget::run`, not `w::Widget::run`. `None` when the chain was cut at an unnamed value
    /// and no member restarted it: `foo(bar).x.baz` is not a path `x::baz`.
    qualified_from: Option<usize>,
    /// The first segment hangs off an unnamed value (`foo(bar).x.baz`, `this.d.e`), so it is a
    /// member, not a receiver.
    unnamed_root: bool,
}

/// Fields that hold the member NAME of a member-access node, across grammars: TS/JS
/// `member_expression.property`, Python `attribute.attribute`, C/C++/Rust/Go `field`.
const MEMBER_FIELDS: &[&str] = &["property", "attribute", "field"];
/// Fields that hold the QUALIFIER a member hangs off: TS/Python `object`, C/C++ `argument`, Rust
/// `value`, C++ `scope`, Rust `path`, Go `operand`.
const QUALIFIER_FIELDS: &[&str] = &["object", "argument", "value", "scope", "path", "operand"];
/// Nodes whose member is their `name` field: C++ `ns::f` / `f<T>` / `.template f<T>` and a
/// template scope `Foo<T>::`. Only these: plenty of non-chain nodes have a `name` field (a named
/// function expression, a keyword argument), and they are not a path.
const NAMED_MEMBER_KINDS: &[&str] =
    &["qualified_identifier", "template_function", "template_method", "template_type"];
/// Nodes without fields whose named children ARE the chain, qualifier first and member last:
/// Kotlin `navigation_expression` / `qualified_identifier`, Python `dotted_name`, C++
/// `dependent_name` (the `template` wrapper around a member).
const FLAT_CHAIN_KINDS: &[&str] =
    &["navigation_expression", "dotted_name", "qualified_identifier", "dependent_name"];

impl<'tree> IdentifierPath<'tree> {
    /// The member chain a callee (or a qualified type / tag name) spells, read from grammar
    /// fields: qualifier fields, then the member field. Arguments, lambdas and template argument
    /// lists are never fields of the chain, so their identifiers can never become the callee,
    /// the qualifier or the receiver.
    ///
    /// A qualifier that is not itself a chain — a call result (`foo(bar).baz`), a subscript, a
    /// `this` — cuts the chain there: `baz` hangs off a value with no name, so the path is just
    /// `baz`, with no receiver and no qualified target. A member that is not an identifier
    /// (`obj.#private`) yields an EMPTY path: no callee is better than a wrong one.
    pub(crate) fn member_chain(node: Node<'tree>, text: &str, kinds: &[&str]) -> Self {
        let mut path = Self { segments: Vec::new(), qualified_from: Some(0), unnamed_root: false };
        collect_member_chain(node, text, kinds, &mut path);
        path
    }

    pub(crate) fn len(&self) -> usize {
        self.segments.len()
    }

    pub(crate) fn first_text(&self) -> Option<&str> {
        self.segments.first().map(|segment| segment.text.as_str())
    }

    pub(crate) fn receiver_text(&self) -> Option<&str> {
        self.first_text().filter(|_| self.has_receiver())
    }

    pub(crate) fn receiver_node(&self) -> Option<Node<'tree>> {
        self.first_node().filter(|_| self.has_receiver())
    }

    fn has_receiver(&self) -> bool {
        self.len() > 1 && !self.unnamed_root
    }

    pub(crate) fn last_text(&self) -> Option<&str> {
        self.segments.last().map(|segment| segment.text.as_str())
    }

    pub(crate) fn first_node(&self) -> Option<Node<'tree>> {
        self.segments.first().map(|segment| segment.node)
    }

    pub(crate) fn last_node(&self) -> Option<Node<'tree>> {
        self.segments.last().map(|segment| segment.node)
    }

    pub(crate) fn qualified_name(&self) -> Option<String> {
        let qualified = &self.segments[self.qualified_from?..];
        (qualified.len() > 1).then(|| {
            qualified.iter().map(|segment| segment.text.as_str()).collect::<Vec<_>>().join("::")
        })
    }

    /// Drop everything collected from `len` on — a chain that failed below that point.
    fn truncate(&mut self, len: usize) {
        self.segments.truncate(len);
        self.qualified_from = self.qualified_from.map(|from| from.min(len));
    }
}

/// One chain node split into the qualifiers it hangs off and its member.
struct ChainParts<'tree> {
    qualifiers: Vec<Node<'tree>>,
    member: Node<'tree>,
    /// The member is a value access (a `MEMBER_FIELDS` field: `w->Widget::run`, `obj.run`), not
    /// the next segment of a scope path (`a::b::c`, `pkg.mod`). Only a value access can spell a
    /// scope of its own that restarts the qualified path.
    member_is_value_access: bool,
}

/// The qualifiers and the member of one chain node, or `None` when the node is not a chain.
fn member_chain_parts(node: Node<'_>) -> Option<ChainParts<'_>> {
    let qualifiers = || {
        QUALIFIER_FIELDS
            .iter()
            .find_map(|field| node.child_by_field_name(field))
            .into_iter()
            .collect()
    };
    if let Some(member) = MEMBER_FIELDS.iter().find_map(|field| node.child_by_field_name(field)) {
        return Some(ChainParts { qualifiers: qualifiers(), member, member_is_value_access: true });
    }
    if NAMED_MEMBER_KINDS.contains(&node.kind())
        && let Some(member) = node.child_by_field_name("name")
    {
        return Some(ChainParts {
            qualifiers: qualifiers(),
            member,
            member_is_value_access: false,
        });
    }
    if !FLAT_CHAIN_KINDS.contains(&node.kind()) {
        return None;
    }
    let mut qualifiers = named_children(node).collect::<Vec<_>>();
    let member = qualifiers.pop()?;
    Some(ChainParts { qualifiers, member, member_is_value_access: false })
}

/// Append `node`'s chain to `path`. Returns false — with `path` exactly as it was on entry — when
/// `node` does not end in an identifier.
fn collect_member_chain<'tree>(
    node: Node<'tree>,
    text: &str,
    kinds: &[&str],
    path: &mut IdentifierPath<'tree>,
) -> bool {
    if kinds.contains(&node.kind()) {
        return match node.utf8_text(text.as_bytes()) {
            Ok(value) if !value.is_empty() => {
                path.segments.push(IdentifierSegment { node, text: value.to_string() });
                true
            },
            _ => false,
        };
    }
    // A postfix operator can sit where the callee or qualifier belongs, and its operand IS the
    // chain: kotlin-ng binds prefix/postfix operators tighter than a call or a navigation
    // (`!isX()`, `a!!.b()` are a `unary_expression`), and TypeScript's non-null assertion
    // `a!.b()` is a field-less `non_null_expression`. Other grammars cannot place a bare unary
    // there without parentheses.
    let operand = match node.kind() {
        "unary_expression" => node.child_by_field_name("argument"),
        "non_null_expression" => node.named_child(0),
        _ => None,
    };
    if let Some(operand) = operand {
        return rag_rat_base::stack::grow_stack(|| {
            collect_member_chain(operand, text, kinds, path)
        });
    }
    let Some(ChainParts { qualifiers, member, member_is_value_access }) = member_chain_parts(node)
    else {
        return false;
    };
    let start = path.len();
    // grow_stack: a chain nests once per member access; a hostile file can make that deep (#543).
    rag_rat_base::stack::grow_stack(|| {
        for qualifier in qualifiers {
            if !collect_member_chain(qualifier, text, kinds, path) {
                // The member hangs off an unnamed value: nothing to its left is its qualifier, and
                // nothing it leads is a written path.
                path.truncate(start);
                path.qualified_from = None;
                path.unnamed_root |= start == 0;
            }
        }
        let member_start = path.len();
        if !collect_member_chain(member, text, kinds, path) {
            path.truncate(start);
            return false;
        }
        // `w->Widget::run`: the member spells its own scope, so the written path restarts there.
        // A scope node's member (`a::b::c`, which nests as `a` + `b::c`) is the path continuing.
        if member_is_value_access && path.len() - member_start > 1 {
            path.qualified_from = Some(member_start);
        }
        true
    })
}

#[cfg(test)]
mod identifier_path_tests {
    use super::*;

    /// A misspelled field name reads as an absent child, which silently cuts every chain through
    /// it — so every field the chain walk reads must be a field of some grammar.
    #[test]
    fn every_member_chain_field_exists_in_some_grammar() {
        use crate::index::parser::{self, ParserKind};
        let grammars = [
            ParserKind::Rust,
            ParserKind::TypeScript,
            ParserKind::Tsx,
            ParserKind::Kotlin,
            ParserKind::C,
            ParserKind::Cpp,
            ParserKind::Python,
            ParserKind::Swift,
            ParserKind::Go,
        ]
        .map(|kind| parser::grammar_for(kind).expect("grammar"));
        for field in MEMBER_FIELDS.iter().chain(QUALIFIER_FIELDS).chain(&["name", "argument"]) {
            assert!(
                grammars.iter().any(|grammar| grammar.field_id_for_name(field).is_some()),
                "no grammar has a `{field}` field"
            );
        }
    }

    #[test]
    fn captures_text_and_nodes_in_one_ordered_path() {
        let source = "client.api.run();";
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let statement = tree.root_node().named_child(0).unwrap();
        let call = statement.named_child(0).unwrap();
        let callee = call.child_by_field_name("function").unwrap();

        let path =
            IdentifierPath::member_chain(callee, source, &["identifier", "property_identifier"]);

        assert_eq!(path.len(), 3);
        assert_eq!(path.first_text(), Some("client"));
        assert_eq!(path.last_text(), Some("run"));
        assert_eq!(path.qualified_name().as_deref(), Some("client::api::run"));
        let last = path.last_node().unwrap();
        assert_eq!(&source[last.byte_range()], "run");
    }
}

pub(crate) fn identifiers_under(node: Node<'_>, text: &str, kinds: &[&str]) -> Vec<String> {
    identifier_nodes_under(node, kinds)
        .into_iter()
        .filter_map(|identifier| identifier.utf8_text(text.as_bytes()).ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}
/// Node-returning twin of [`first_identifier_text`]: the first identifier-kind node in document
/// order, so its byte range can be recorded for the SCIP join (#67). Same traversal, so the node it
/// returns is exactly the token whose text [`first_identifier_text`] would have produced.
pub(crate) fn first_identifier_node<'tree>(
    node: Node<'tree>,
    kinds: &[&str],
) -> Option<Node<'tree>> {
    rag_rat_base::stack::grow_stack(|| {
        for child in named_children(node) {
            if kinds.contains(&child.kind()) {
                return Some(child);
            }
            if let Some(found) = first_identifier_node(child, kinds) {
                return Some(found);
            }
        }
        None
    })
}
/// Node-returning twin of [`last_identifier_text`]: the last identifier-kind node under `node`.
pub(crate) fn last_identifier_node<'tree>(
    node: Node<'tree>,
    kinds: &[&str],
) -> Option<Node<'tree>> {
    identifier_nodes_under(node, kinds).into_iter().last()
}
/// Node-returning twin of [`identifiers_under`]: every identifier-kind node under `node`, in the
/// same document order, so the callee (`.last()`) and receiver (`.first()`) nodes line up 1:1 with
/// the strings the TS/Kotlin/C extractors already pick out of [`identifiers_under`]. The byte range
/// of the matching node is what the SCIP join keys on (#67).
pub(crate) fn identifier_nodes_under<'tree>(node: Node<'tree>, kinds: &[&str]) -> Vec<Node<'tree>> {
    let mut out = Vec::new();
    collect_identifier_nodes(node, kinds, &mut out);
    out
}
fn collect_identifier_nodes<'tree>(node: Node<'tree>, kinds: &[&str], out: &mut Vec<Node<'tree>>) {
    if kinds.contains(&node.kind()) {
        out.push(node);
        return;
    }
    rag_rat_base::stack::grow_stack(|| {
        for child in named_children(node) {
            collect_identifier_nodes(child, kinds, out);
        }
    });
}
/// Narrow a scoped/dotted identifier node (`scoped_identifier` / `scoped_type_identifier`, whose
/// text is the full `a::b::c`) down to its final segment — the callee/type name `c`. The
/// tree-sitter grammars expose the tail as the `name` field; without it (a plain `identifier`) the
/// node is already the final segment, so return it unchanged. This makes the recorded byte range
/// cover only the callee identifier, matching how `short_name` collapses the printed name and how
/// SCIP keys the occurrence.
pub(crate) fn final_segment_node(node: Node<'_>) -> Node<'_> {
    node.child_by_field_name("name").unwrap_or(node)
}
pub(crate) fn is_rust_path_keyword(value: &str) -> bool {
    matches!(value, "self" | "super" | "crate")
}
pub(crate) fn looks_like_type_name(value: &str) -> bool {
    value.chars().next().is_some_and(char::is_uppercase)
}
pub(crate) fn node_text(node: Node<'_>, text: &str) -> String {
    node.utf8_text(text.as_bytes()).unwrap_or_default().to_string()
}
pub(crate) fn edge_evidence(node: Node<'_>, text: &str) -> String {
    node_text(node, text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}
/// Whitespace-normalized but UNTRUNCATED node text, for a `use_declaration`'s evidence. The
/// crate-aware import scope re-parses this with `imports::parse_use` (#61); the 240-char cap in
/// [`edge_evidence`] would drop the late leaves of a long braced `use foo::{…, X}`, leaving those
/// names un-suppressed (#97 item 1). A `use` statement is bounded and small, so storing it in full
/// is cheap.
pub(crate) fn use_declaration_evidence(node: Node<'_>, text: &str) -> String {
    node_text(node, text).split_whitespace().collect::<Vec<_>>().join(" ")
}
pub(crate) fn short_name(name: &str) -> &str {
    name.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(name)
}
pub(crate) fn symbols_for_file(
    conn: &Connection,
    file_id: i64,
) -> anyhow::Result<Vec<IndexedSymbol>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, symbols.language, symbols.name, qn.value, symbols.kind,
               symbols.start_byte, symbols.end_byte, symbols.start_line, symbols.end_line, \
         COALESCE(symbols.scope_path, '')
        FROM symbols
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE file_id = ?1
        ORDER BY symbols.start_byte, symbols.end_byte
        ",
    )?;
    let rows = stmt.query_map([file_id], symbol_row)?;
    collect_rows(rows)
}
/// Every symbol in the ACTIVE CHECKOUT, ordered by qualified name. The `files` join goes through
/// the per-connection scoped TEMP VIEW (overlay wins, dead scopes excluded) — resolving against
/// raw `symbols` duplicated every symbol whenever multiple scopes coexist and collapsed edge
/// resolution (#89).
pub(crate) fn all_symbols(conn: &Connection) -> anyhow::Result<Vec<IndexedSymbol>> {
    // Scope the edge-resolution CANDIDATE POOL to the active repo (A3). This SELECT joins `files` —
    // the per-connection scope VIEW when one is installed (rebuild / incremental / overlay), which
    // already filters `repo_id`. But the bare-open graph refresh (`IndexDatabase::open` →
    // `ensure_graph_index_current`) runs with NO scope view, so `files` resolves to the unscoped
    // `main.files` and would pull EVERY repo's symbols into the pool in a consolidated DB — letting
    // repo A's edges resolve onto repo B's symbols. The `main.files` sub-select pins the pool to
    // the active repo in both cases: redundant under a scope view (view rows ⊆ active repo),
    // and the sole repo predicate on the view-less path. `active_repo_id` falls back to
    // `sole_repo_id` when no context is installed, matching the bare-open scope.
    let active_repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    // Also pin the pool to the active GENERATION (A6): a full rebuild leaves the superseded
    // generation's rows in place until gc, so a bare `repo_id` pin would pull a dead generation's
    // symbols into the pool. `active_generation` reads the connection's scope context (the WRITE
    // generation on the rebuild connection, so edge resolution during a rebuild resolves onto the
    // symbols it is building) and falls back to the repo's LIVE generation from `repo_meta` on the
    // view-less bare-open heal path — matching the `active_repo_id` fallback exactly.
    let active_generation = rag_rat_db::schema::active_generation(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, symbols.file_id, symbols.language, symbols.name, qn.value, symbols.kind,
               symbols.start_byte, symbols.end_byte, symbols.start_line, symbols.end_line, \
         COALESCE(symbols.scope_path, '')
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
        WHERE symbols.file_id IN (
                  SELECT id FROM main.files WHERE repo_id = ?1 AND generation = ?2
              )
        ORDER BY qn.value
        ",
    )?;
    let rows = stmt.query_map(rusqlite::params![active_repo_id, active_generation], symbol_row)?;
    collect_rows(rows)
}
/// Every local binding (`parser::LocalBinding`) in the active checkout, scoped exactly as
/// [`all_symbols`] scopes the symbols it merges into.
pub(crate) fn all_local_bindings(conn: &Connection) -> anyhow::Result<Vec<LocalBindingFields>> {
    let active_repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let active_generation = rag_rat_db::schema::active_generation(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT local_bindings.file_id, files.language, files.path, local_bindings.name,
               local_bindings.kind, local_bindings.scope_path, local_bindings.start_byte,
               local_bindings.end_byte
        FROM local_bindings
        JOIN files ON files.id = local_bindings.file_id
        WHERE local_bindings.file_id IN (
                  SELECT id FROM main.files WHERE repo_id = ?1 AND generation = ?2
              )
        ",
    )?;
    let rows = stmt.query_map(rusqlite::params![active_repo_id, active_generation], |row| {
        Ok(LocalBindingFields {
            file_id: row.get(0)?,
            language: row.get(1)?,
            path: row.get(2)?,
            name: row.get(3)?,
            kind: row.get(4)?,
            scope_path: row.get(5)?,
            start: usize::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
            end: usize::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        })
    })?;
    collect_rows(rows)
}
pub(crate) fn symbol_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedSymbol> {
    let start_byte = usize::try_from(row.get::<_, i64>(6)?).unwrap_or(0);
    let end_byte = usize::try_from(row.get::<_, i64>(7)?).unwrap_or(0);
    Ok(IndexedSymbol {
        id: row.get(0)?,
        file_id: row.get(1)?,
        language: row.get(2)?,
        name: row.get(3)?,
        qualified_name: row.get(4)?,
        kind: row.get(5)?,
        start_byte,
        end_byte,
        start_line: row.get(8)?,
        end_line: row.get(9)?,
        scope_path: row.get(10)?,
    })
}
/// Intern one string into `name_strings`, returning its id (#79). `INSERT OR IGNORE` + lookup —
/// two cached statements; callers on bulk paths wrap this in [`EdgeStringInterner`] so repeats
/// hit a process-side map instead of the b-tree.
pub(crate) fn intern_edge_string(conn: &Connection, value: &str) -> anyhow::Result<i64> {
    conn.prepare_cached("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)")?
        .execute([value])?;
    conn.prepare_cached("SELECT id FROM name_strings WHERE value = ?1")?
        .query_row([value], |row| row.get(0))
        .map_err(Into::into)
}

pub(crate) fn intern_edge_string_opt(
    conn: &Connection,
    value: Option<&str>,
) -> anyhow::Result<Option<i64>> {
    value.map(|value| intern_edge_string(conn, value)).transpose()
}

/// A process-side memo over [`intern_edge_string`] for bulk paths: the graph vocabulary is small
/// (tens of thousands of distinct strings against millions of edges), so the map stays tiny while
/// saving two b-tree probes per repeated string.
#[derive(Default)]
pub(crate) struct EdgeStringInterner {
    cache: std::collections::HashMap<String, i64>,
}

impl EdgeStringInterner {
    pub(crate) fn get(&mut self, conn: &Connection, value: &str) -> anyhow::Result<i64> {
        if let Some(id) = self.cache.get(value) {
            return Ok(*id);
        }
        let id = intern_edge_string(conn, value)?;
        self.cache.insert(value.to_string(), id);
        Ok(id)
    }

    pub(crate) fn get_opt(
        &mut self,
        conn: &Connection,
        value: Option<&str>,
    ) -> anyhow::Result<Option<i64>> {
        value.map(|value| self.get(conn, value)).transpose()
    }
}

pub(crate) fn insert_candidates(
    conn: &Connection,
    file_id: i64,
    candidates: Vec<EdgeCandidate>,
) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        let to_name = candidate.to_name.trim();
        if to_name.is_empty() {
            continue;
        }
        let key = (
            candidate.from_symbol_id,
            candidate.from_name.clone(),
            to_name.to_string(),
            candidate.edge_kind,
            candidate.source_span.start_byte,
            candidate.source_span.end_byte,
        );
        if !seen.insert(key) {
            continue;
        }
        // prepare_cached: this INSERT runs once per edge. conn.execute
        // recompiles the SQL every call; the cached statement compiles once per connection.
        // NULL when the candidate has no callee identifier range (non-call / file-level edges)
        // (#67).
        let (callee_start_byte, callee_end_byte) = match candidate.callee_span {
            Some(range) => (
                Some(i64::try_from(range.start_byte).unwrap_or(0)),
                Some(i64::try_from(range.end_byte).unwrap_or(0)),
            ),
            None => (None, None),
        };
        // Module-aware import scope (#61): the dedicated columns, NULL on non-import edges so the
        // oracle's `callee_start_byte IS NOT NULL` candidate filter never sees them.
        let (import_scope_start_byte, import_scope_end_byte, import_mod_id) =
            match candidate.import_scope {
                Some(scope) => (
                    Some(i64::try_from(scope.scope_start).unwrap_or(0)),
                    Some(i64::try_from(scope.scope_end).unwrap_or(0)),
                    Some(scope.mod_id),
                ),
                None => (None, None, None),
            };
        // Direct interned write to edges_data (#79): the view's INSTEAD OF insert would work but
        // costs 8 dictionary probes per row in SQL, and `last_insert_rowid` does not survive an
        // INSTEAD OF trigger.
        let from_name_id = intern_edge_string_opt(conn, candidate.from_name.as_deref())?;
        let to_name_id = intern_edge_string(conn, to_name)?;
        let target_qualified_name_id =
            intern_edge_string_opt(conn, candidate.target_qualified_name.as_deref())?;
        let receiver_hint_id = intern_edge_string_opt(conn, candidate.receiver_hint.as_deref())?;
        let receiver_type_hint_id =
            intern_edge_string_opt(conn, candidate.receiver_type_hint.as_deref())?;
        let edge_kind_id = intern_edge_string(conn, candidate.edge_kind.as_db_str())?;
        let confidence_id = intern_edge_string(conn, candidate.confidence.as_db_str())?;
        let resolution_id =
            intern_edge_string(conn, super::EdgeResolution::Unresolved.as_db_str())?;
        let hidden =
            super::edge_hidden_flag(candidate.edge_kind, super::EdgeResolution::Unresolved);
        conn.prepare_cached(
            "
            INSERT INTO edges_data(
                source_file_id, from_symbol_id, from_name_id, to_name_id,
                target_qualified_name_id, evidence, receiver_hint_id, receiver_type_hint_id,
                source_start_line, source_end_line, source_start_byte, source_end_byte,
                callee_start_byte, callee_end_byte,
                import_scope_start_byte, import_scope_end_byte, import_mod_id,
                edge_kind_id, confidence_id, resolution_id, hidden
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19, ?20, ?21)
            ",
        )?
        .execute(params![
            file_id,
            candidate.from_symbol_id,
            from_name_id,
            to_name_id,
            target_qualified_name_id,
            candidate.evidence,
            receiver_hint_id,
            receiver_type_hint_id,
            candidate.source_span.start_line,
            candidate.source_span.end_line,
            candidate.source_span.start_byte,
            candidate.source_span.end_byte,
            callee_start_byte,
            callee_end_byte,
            import_scope_start_byte,
            import_scope_end_byte,
            import_mod_id,
            edge_kind_id,
            confidence_id,
            resolution_id,
            hidden,
        ])?;
    }
    Ok(())
}
pub(crate) fn span_for_node(node: Node<'_>) -> EdgeSpan {
    EdgeSpan {
        start_line: i64::try_from(node.start_position().row).unwrap_or(i64::MAX).saturating_add(1),
        end_line: i64::try_from(node.end_position().row).unwrap_or(i64::MAX).saturating_add(1),
        start_byte: i64::try_from(node.start_byte()).unwrap_or(i64::MAX),
        end_byte: i64::try_from(node.end_byte()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod containing_symbol_tests {
    use super::*;

    fn sym(id: i64, kind: &str, start: usize, end: usize) -> IndexedSymbol {
        IndexedSymbol {
            id,
            file_id: 0,
            language: "rust".to_string(),
            name: format!("s{id}"),
            qualified_name: format!("s{id}"),
            scope_path: String::new(),
            kind: kind.to_string(),
            start_byte: start,
            end_byte: end,
            start_line: 0,
            end_line: 0,
        }
    }

    /// The pre-#519 filter-into-`Vec` + stable-sort-by-span implementation, kept verbatim so the
    /// single-pass rewrite can be proven byte-identical (same selected id, same tie-break) across a
    /// battery of inputs — including degenerate equal-span sets a real parse can't produce.
    fn reference(symbols: &[IndexedSymbol], byte: usize) -> Option<&IndexedSymbol> {
        let mut matches = symbols
            .iter()
            .filter(|symbol| symbol.start_byte <= byte && symbol.end_byte >= byte)
            .collect::<Vec<_>>();
        matches.sort_by_key(|symbol| symbol.end_byte.saturating_sub(symbol.start_byte));
        let first = matches.first().copied()?;
        if matches!(first.kind.as_str(), "const" | "property" | "static") {
            matches
                .iter()
                .copied()
                .find(|symbol| {
                    symbol.id != first.id
                        && !matches!(symbol.kind.as_str(), "const" | "property" | "static")
                })
                .or(Some(first))
        } else {
            Some(first)
        }
    }

    fn assert_equiv(symbols: &[IndexedSymbol], byte: usize) {
        let got = containing_symbol(symbols, byte).map(|symbol| symbol.id);
        let want = reference(symbols, byte).map(|symbol| symbol.id);
        assert_eq!(got, want, "byte={byte}");
        let prepared = SymbolLocator::new(symbols).find(byte).map(|symbol| symbol.id);
        assert_eq!(prepared, want, "prepared locator at byte={byte}");
    }

    #[test]
    fn smallest_enclosing_wins_over_a_nested_chain() {
        let symbols =
            [sym(1, "module", 0, 100), sym(2, "function", 10, 90), sym(3, "struct", 40, 60)];
        assert_eq!(containing_symbol(&symbols, 50).map(|s| s.id), Some(3), "innermost");
        assert_eq!(containing_symbol(&symbols, 20).map(|s| s.id), Some(2));
        assert_eq!(containing_symbol(&symbols, 5).map(|s| s.id), Some(1));
        assert!(containing_symbol(&symbols, 200).is_none(), "outside every span");
    }

    #[test]
    fn boundary_bytes_are_inclusive() {
        let symbols = [sym(1, "function", 10, 20)];
        assert_eq!(containing_symbol(&symbols, 10).map(|s| s.id), Some(1), "start is inclusive");
        assert_eq!(containing_symbol(&symbols, 20).map(|s| s.id), Some(1), "end is inclusive");
        assert!(containing_symbol(&symbols, 9).is_none());
        assert!(containing_symbol(&symbols, 21).is_none());
    }

    #[test]
    fn const_property_static_reselects_the_smallest_non_special_container() {
        // A `const` is the smallest span at the byte, but a non-special container exists → pick it.
        let symbols = [sym(1, "function", 0, 100), sym(2, "const", 40, 60)];
        assert_eq!(containing_symbol(&symbols, 50).map(|s| s.id), Some(1), "reselect the fn");
        assert_equiv(&symbols, 50);
    }

    #[test]
    fn a_const_with_no_non_special_container_stays_selected() {
        let symbols = [sym(1, "const", 40, 60)];
        assert_eq!(containing_symbol(&symbols, 50).map(|s| s.id), Some(1));
        assert_equiv(&symbols, 50);
    }

    #[test]
    fn reselection_prefers_the_smallest_non_special_container() {
        // Two non-special containers wrap a `const`; the SMALLER-span one is chosen (not just any).
        let symbols =
            [sym(1, "module", 0, 200), sym(2, "function", 30, 70), sym(3, "property", 45, 55)];
        assert_eq!(containing_symbol(&symbols, 50).map(|s| s.id), Some(2), "smallest non-special");
        assert_equiv(&symbols, 50);
    }

    #[test]
    fn equal_span_containers_keep_the_first_in_array_order() {
        // Degenerate (unreachable from a real parse): two containers with an identical span at the
        // byte. Both `containing_symbol` and the reference must pick the FIRST in array order.
        let symbols = [sym(7, "function", 10, 20), sym(8, "function", 10, 20)];
        assert_eq!(containing_symbol(&symbols, 15).map(|s| s.id), Some(7));
        assert_equiv(&symbols, 15);
    }

    #[test]
    fn matches_the_reference_across_a_swept_battery() {
        // Overlapping/nested/sibling spans of mixed kinds; sweep every byte and every reordering-
        // sensitive case to pin byte-identical selection against the old implementation.
        let symbols = [
            sym(1, "module", 0, 100),
            sym(2, "const", 0, 100),
            sym(3, "function", 20, 80),
            sym(4, "static", 20, 80),
            sym(5, "struct", 40, 60),
            sym(6, "property", 40, 60),
            sym(7, "function", 50, 50),
        ];
        for byte in 0..=110 {
            assert_equiv(&symbols, byte);
        }
    }

    #[test]
    fn prepared_lookup_is_logarithmic_in_symbol_count() {
        let symbols = (0..4_096)
            .map(|index| sym(index, "function", index as usize * 2, index as usize * 2))
            .collect::<Vec<_>>();
        let locator = SymbolLocator::new(&symbols);

        let mut probes = 0;
        let selected = locator.find_index_by(8_190, || probes += 1);
        assert_eq!(selected.map(|index| symbols[index].id), Some(4_095));
        assert!(probes <= 14, "{probes} probes for {} symbols", symbols.len());
    }
}

#[cfg(test)]
mod scoped_receiver_name_tests {
    use super::*;

    /// The outermost `call_expression` in a parsed snippet — the one whose `function` field spans
    /// a whole method chain, which is where a nested `::` can appear.
    ///
    /// Breadth-first, so the shallowest match is the outermost one, and iterative so it is not a
    /// recursive tree descender (#543).
    fn outermost_call<'tree>(root: Node<'tree>) -> Option<Node<'tree>> {
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(node) = queue.pop_front() {
            if node.kind() == "call_expression" {
                return Some(node);
            }
            let mut cursor = node.walk();
            queue.extend(node.children(&mut cursor));
        }
        None
    }

    fn receiver_of(expression: &str) -> Option<String> {
        let source = format!("fn probe() {{ {expression}; }}");
        let parsed = crate::index::parser::parse_file(
            std::path::Path::new("probe.rs"),
            Language::Rust,
            &source,
        )?;
        scoped_receiver_name(outermost_call(parsed.root())?, &source)
    }

    /// A `::` inside the ARGUMENT list is not a path separator, so the head of the path — the name
    /// the call hangs off — is never taken from the arguments.
    #[test]
    fn a_nested_separator_does_not_move_the_receiver_into_the_arguments() {
        assert_eq!(
            receiver_of(
                r#"conn.execute("INSERT INTO t VALUES (?1)", rusqlite::params![x]).unwrap()"#
            )
            .as_deref(),
            Some("conn"),
            "a `::` path spelled in the arguments"
        );
        assert_eq!(
            receiver_of("stmt.query_map([], |row| row.get::<_, String>(0)).unwrap()").as_deref(),
            Some("stmt"),
            "a turbofish inside a closure inside the arguments"
        );
        assert_eq!(
            receiver_of("items.iter().map(|v| v.parse::<i64>()).collect::<Vec<_>>()").as_deref(),
            Some("items"),
            "a turbofish in the arguments AND one on the outermost callee"
        );
    }

    /// A TOP-LEVEL `::` still separates, and still outranks a later `.`.
    #[test]
    fn a_top_level_separator_still_names_the_owner() {
        assert_eq!(receiver_of("Worker::spawn(cfg).run()").as_deref(), Some("Worker"));
        assert_eq!(receiver_of("a::b::Worker::spawn()").as_deref(), Some("a"));
        assert_eq!(receiver_of("Vec::<u8>::new()").as_deref(), Some("Vec"));
        assert_eq!(receiver_of("worker.run()").as_deref(), Some("worker"));
        assert_eq!(receiver_of("bare()").as_deref(), None, "no separator at all");
    }
}
