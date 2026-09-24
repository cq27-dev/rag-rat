//! Rust graph-edge extraction for the shared structural edge walk. It recognizes calls, types,
//! constructions, imports, impl headers, and dispatch facts.
use std::path::Path;

use tree_sitter::Node;

use super::{binders, dispatch};
use crate::index::edges::*;

pub(in crate::index::languages) fn rust_edges(
    EdgeVisit { text, node, symbols: _, path, locator }: EdgeVisit<'_, '_, '_>,
    out: &mut EdgeEmitter<'_>,
) {
    match node.kind() {
        "use_declaration" => rust_use_edges(text, node, path, out),
        "mod_item" => rust_mod_edges(text, node, path, out),
        "call_expression" => rust_call_edges(text, node, locator, out),
        "struct_expression" => rust_struct_variant_edges(text, node, locator, out),
        "scoped_identifier" => rust_unit_variant_edges(text, node, locator, out),
        "match_arm" => dispatch::rust_dispatch_handle_facts(text, node, locator, out),
        "macro_invocation" => rust_macro_edges(text, node, locator, out),
        "impl_item" => rust_impl_edges(text, node, locator, out),
        // Not `generic_type`: its `type` field is itself one of these and emits the edge, and
        // the rest of it is type arguments, each its own reference.
        "type_identifier" | "scoped_type_identifier" =>
            rust_type_reference_edges(text, node, locator, out),
        _ => {},
    }
}

fn rust_use_edges(text: &str, node: Node<'_>, path: &Path, out: &mut EdgeEmitter<'_>) {
    let names = identifiers_under(node, text, super::IDENTIFIER_KINDS);
    let is_reexport = node_text(node, text).trim_start().starts_with("pub use ");
    // Module-aware import scope (#61): a Rust `use` is scoped to its enclosing module body
    // (or block, for a block-local `use`), not the whole file. Record that scope + the
    // enclosing module's id on the dedicated import-scope columns so resolution suppresses
    // a bare reference only inside the `use`'s scope (parent-`mod` `use`s don't
    // reach a child `mod`). Top-level `use` → whole file, `MOD_FILE_ROOT`.
    let scope = enclosing_use_scope(node, text);
    // The crate-aware scope rebuilds its {leaf → root} map by re-parsing an Imports edge's
    // `evidence` with `imports::parse_use`; `parse_use` returns EVERY leaf, so ONE edge
    // carrying the FULL (untruncated) use text populates the whole map for this `use`.
    // Attach the full text to only the FIRST emitted Imports edge — the default
    // `edge_evidence` truncates at 240 chars and would drop late braced leaves (#97 item 1)
    // — and let the rest carry standard evidence, so a multi-hundred-KB `use` isn't cloned
    // into every leaf's edge (#97 item 3).
    let mut full_use_evidence = Some(use_declaration_evidence(node, text));
    for name in names {
        if !is_rust_path_keyword(&name) {
            let evidence = full_use_evidence.take().unwrap_or_else(|| edge_evidence(node, text));
            out.push(file_edge_scoped(
                path,
                node,
                name,
                Some(evidence),
                EdgeKind::Imports,
                Some(scope),
            ));
        }
    }
    if is_reexport {
        for name in identifiers_under(node, text, super::IDENTIFIER_KINDS) {
            if !is_rust_path_keyword(&name) {
                out.push(file_edge(path, node, text, name, EdgeKind::Exports));
            }
        }
    }
}

fn rust_mod_edges(text: &str, node: Node<'_>, path: &Path, out: &mut EdgeEmitter<'_>) {
    let Some(name) = child_name_text(node, text) else {
        return;
    };
    // An INLINE `mod foo { … }` carries its body range + its own id as the import scope
    // so resolution can rebuild the per-file module interval set (the ref→mod-id
    // lookup) from edges alone, WITHOUT the tree (#61) — including modules that contain
    // no `use`. A non-inline `mod foo;` has no body and introduces no scope (NULL).
    let scope = inline_mod_scope(node);
    out.push(file_edge_scoped(
        path,
        node,
        name,
        Some(edge_evidence(node, text)),
        EdgeKind::Imports,
        scope,
    ));
}

fn rust_call_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    if let Some(name) = call_target_name(node, text) {
        out.push(symbol_edge_with_context(
            locator,
            node,
            Some(text),
            name,
            EdgeKind::CallsName,
            EdgeContext {
                target_qualified_name: target_qualified_name(node, text),
                receiver_hint: scoped_receiver_name(node, text),
                receiver_type_hint: infer_rust_receiver_type_hint(node, text),
            },
            call_target_node(node).map(CalleeRange::of_node),
        ));
    }
    // A scoped call receiver is a type reference only when it names a type. By Rust
    // convention types are PascalCase, while module paths (`std::env::…`) and method
    // receivers on locals (`p.as_os_str()`) are snake_case — emitting those as
    // `references_type` produced bogus "types" like `std` and `p`. Gate on an
    // uppercase-leading receiver so `Foo::bar()` still records a type reference.
    if let Some(receiver) = scoped_receiver_name(node, text)
        && receiver.chars().next().is_some_and(char::is_uppercase)
    {
        out.push(symbol_edge(
            locator,
            node,
            receiver,
            EdgeKind::ReferencesType,
            // The type is the receiver — the LEADING `::` segment (`Foo` in `Foo::bar()`)
            // — so anchor the range on the function path's first
            // identifier, not its tail.
            node.child_by_field_name("function")
                .and_then(|node| first_identifier_node(node, super::IDENTIFIER_KINDS))
                .map(CalleeRange::of_node),
        ));
    }
    // #200 dispatch construct fact: a tuple enum-variant construction `Enum::Variant(..)`.
    // Key off the FULL call path's last two PascalCase `::` segments, so
    // `crate::m::Msg::Start(..)` still yields `Msg::Start` (the bare receiver would be
    // `crate`). `Foo::new()` / `T::CONST` are excluded (tail not PascalCase).
    if let Some(key) =
        node.child_by_field_name("function").and_then(|f| dispatch::enum_variant_key(f, text))
    {
        out.push(dispatch::dispatch_fact(
            locator,
            node,
            key,
            EdgeKind::DispatchConstruct,
            EdgeContext::default(),
            None,
        ));
    }
}

/// #200 dispatch construct fact: a struct enum-variant construction `Enum::Variant { .. }`.
fn rust_struct_variant_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    if let Some(key) =
        node.child_by_field_name("name").and_then(|n| dispatch::enum_variant_key(n, text))
    {
        out.push(dispatch::dispatch_fact(
            locator,
            node,
            key,
            EdgeKind::DispatchConstruct,
            EdgeContext::default(),
            None,
        ));
    }
}

/// #200 dispatch construct fact: a UNIT enum-variant `Enum::Stop` in a VALUE position — a call
/// argument (`send(Msg::Stop)`), a `let` initializer (`let m = Msg::Stop;`), an assignment RHS, or
/// a `return`/`break` value. Not a `call_expression`, so the call and struct arms miss it. The
/// value-position gate keeps an ordinary type/module/use path out; over-emitting for a non-enum
/// `Foo::Bar` is harmless — synthesis only joins a variant whose head is a unique in-scope `enum`,
/// so a non-enum head never yields a `dispatches` edge.
fn rust_unit_variant_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    if dispatch::scoped_identifier_in_value_position(node)
        && let Some(key) = dispatch::enum_variant_key(node, text)
    {
        out.push(dispatch::dispatch_fact(
            locator,
            node,
            key,
            EdgeKind::DispatchConstruct,
            EdgeContext::default(),
            None,
        ));
    }
}

fn rust_macro_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    if let Some(name) = first_identifier_text(node, text, super::IDENTIFIER_KINDS) {
        out.push(symbol_edge_with_context(
            locator,
            node,
            Some(text),
            name,
            EdgeKind::UsesMacro,
            EdgeContext::default(),
            first_identifier_node(node, super::IDENTIFIER_KINDS).map(CalleeRange::of_node),
        ));
    }
}

fn rust_type_reference_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    if let Some(name) = last_identifier_text(node, text, super::IDENTIFIER_KINDS) {
        out.push(symbol_edge(
            locator,
            node,
            name,
            EdgeKind::ReferencesType,
            last_identifier_node(node, super::IDENTIFIER_KINDS)
                .map(final_segment_node)
                .map(CalleeRange::of_node),
        ));
    }
}

/// `impl Trait for Type` implements the `trait` field's trait; an inherent `impl Type` references
/// the `type` field's type. Both are read from their fields — never from the header text, where
/// generic binders, lifetimes, bounds and the body's own `for` loops all look like names.
pub(super) fn rust_impl_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let (field, edge_kind) = if node.child_by_field_name("trait").is_some() {
        ("trait", EdgeKind::Implements)
    } else {
        ("type", EdgeKind::ReferencesType)
    };
    let Some(path) = node.child_by_field_name(field).and_then(impl_path) else {
        return;
    };
    let name = final_segment_node(path);
    let rendered = super::render_owner(path, text, &[]);
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        node_text(name, text),
        edge_kind,
        EdgeContext {
            target_qualified_name: rendered.contains("::").then_some(rendered),
            ..Default::default()
        },
        Some(CalleeRange::of_node(name)),
    ));
}

/// The nominal path an impl's trait or self type names — `Foo` for `&'a Foo<T>`, `a::Tr` for
/// `a::Tr<u8>`. `None` for a non-nominal self type (`()`, `[T]`, `dyn X`).
fn impl_path(node: Node<'_>) -> Option<Node<'_>> {
    let nominal = super::unwrap_impl_type(node)?;
    if nominal.kind() == "generic_type" {
        nominal.child_by_field_name("type")
    } else {
        Some(nominal)
    }
}

pub(crate) fn infer_rust_receiver_type_hint(node: Node<'_>, text: &str) -> Option<String> {
    let function = unwrap_generic_function(node.child_by_field_name("function")?);
    match function.kind() {
        // `value.method()` — the receiver is a VALUE, so a local binding of that name is what it
        // names.
        "field_expression" => {
            let value_node = function.child_by_field_name("value")?;
            let receiver = clean_receiver_expr(&node_text(value_node, text))?.to_string();
            let recv = receiver.as_str();
            if recv == "self" {
                return infer_explicit_self_type_hint(node, text);
            }
            if recv == "Self" {
                return infer_self_type_hint(node, text);
            }
            if !is_simple_identifier(recv) {
                return None;
            }
            infer_local_var_type_hint(node, text, recv)
        },
        // `Qualifier::item()` — the qualifier is resolved as a PATH, in the type/module namespace,
        // so a local variable of the same name is irrelevant to it. `mod worker { fn run() {} }`
        // beside `fn f(worker: Alpha)` makes `worker::run()` the module's function while
        // `worker.run()` is the parameter's method; reading the parameter here bound the call to
        // `Alpha::run`. Lowercase `self` is a path too — `self::helper()` names the CURRENT MODULE,
        // not the enclosing impl. `Self` is the one qualifier that does name a type.
        "scoped_identifier" => {
            let qualifier = function.child_by_field_name("path")?;
            (node_text(qualifier, text).trim() == "Self")
                .then(|| infer_self_type_hint(node, text))?
        },
        _ => None,
    }
}

fn clean_receiver_expr(raw: &str) -> Option<&str> {
    let mut s = raw.trim();
    while s.starts_with('&') || s.starts_with('*') {
        s = s[1..].trim();
        if let Some(rest) = s.strip_prefix("mut ") {
            s = rest.trim();
        }
        if let Some(rest) = strip_lifetime(s) {
            s = rest.trim();
        }
    }
    if let Some(rest) = s.strip_prefix("mut ") {
        s = rest.trim();
    }
    if s.is_empty() { None } else { Some(s) }
}

fn strip_lifetime(s: &str) -> Option<&str> {
    if s.starts_with('\'') {
        let end = s.find(|c: char| c.is_whitespace() || c == ':')?;
        Some(&s[end..])
    } else {
        None
    }
}

fn is_simple_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !s.chars().next().unwrap().is_ascii_digit()
}

fn infer_self_type_hint(node: Node<'_>, text: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "impl_item"
            && let Some(type_node) = ancestor.child_by_field_name("type")
        {
            let cleaned = clean_rust_type_name(type_node, type_node, text)?;
            // Canonical against the IMPL's own module — `mod inner { impl Worker { … } }`
            // yields `inner::Worker`, matching the method's container-based scope path.
            return module_qualified_type_path(ancestor, &cleaned, text);
        }
        current = ancestor.parent();
    }
    None
}

/// An arbitrary self type participates in the same method lookup as an ordinary parameter. In
/// particular, `self: Box<Self>` can dispatch to a wrapper-level trait method before dereferencing
/// to the impl owner, so the single-owner inference must decline it.
fn infer_explicit_self_type_hint(node: Node<'_>, text: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "function_item" {
            let parameters = ancestor.child_by_field_name("parameters")?;
            for parameter in named_children(parameters) {
                if parameter.kind() != "parameter" {
                    continue;
                }
                let Some(pattern) = parameter.child_by_field_name("pattern") else { continue };
                if pattern.kind() != "self" {
                    continue;
                }
                let type_node = parameter.child_by_field_name("type")?;
                let type_name = clean_rust_type_name(type_node, type_node, text)?;
                return canonical_receiver_type(type_name, type_node, text);
            }
            return infer_self_type_hint(node, text);
        }
        current = ancestor.parent();
    }
    None
}

/// Smart pointers that `Deref` to their contents. They cannot produce one authoritative receiver
/// hint: Rust considers methods on the wrapper before dereferencing to the inner type, and a local
/// trait may implement a method directly for `Box<Worker>` (or any sibling here).
///
/// The list is the deref-transparent wrappers common enough to matter, and it is OPEN, not closed:
/// `ManuallyDrop<T>`, `MutexGuard<'_, T>` and `Ref<'_, T>` deref to their contents too, and a
/// workspace's own smart pointer never appears here at all. Peeling by the indexed `impl Deref`
/// edges would close it, but extraction has no graph to ask. `Option<Worker>` and `Vec<Worker>`
/// are deliberately NOT among them — `Option<Worker>::run` is a compile error, so unwrapping them
/// would invent a receiver that Rust never reaches. `Mutex`/`RefCell` are out for the same reason:
/// their contents come out through `lock`/`borrow`, not deref.
const DEREF_WRAPPERS: [&str; 5] = ["Box", "Rc", "Arc", "Cow", "Pin"];

/// How the head of a wrapped type is written, which decides whether it names the standard pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WrapperSpelling {
    /// A bare `Box` — the standard one, but only while nothing nearer declares that name.
    Bare,
    /// A path rooted at a standard crate (`std::boxed::Box`, `alloc::sync::Arc`) — the standard one
    /// no matter what else is in scope, since the root cannot be shadowed by a local declaration.
    Rooted,
    /// Anything else, including a qualified path that merely ENDS in a wrapper name.
    NotAWrapper,
}

/// Classify the head of a wrapped type.
///
/// The tail alone is not enough. `custom::Box<Worker>` ends in `Box`, but it is somebody else's
/// type with its own methods, so peeling it would send `value.run()` to `Worker::run` when rustc
/// sends it to `custom::Box::run`. A qualified spelling names the standard pointer only when it is
/// rooted where the standard pointers live.
fn wrapper_spelling(head: Node<'_>, text: &str) -> WrapperSpelling {
    let Some(tail) = path_tail_node(head) else { return WrapperSpelling::NotAWrapper };
    let tail = canonical_identifier(tail, text);
    if !DEREF_WRAPPERS.contains(&tail) {
        return WrapperSpelling::NotAWrapper;
    }
    match head.kind() {
        "identifier" | "type_identifier" => WrapperSpelling::Bare,
        "scoped_identifier" | "scoped_type_identifier" => {
            let Some(path) = head.child_by_field_name("path") else {
                // Preserve the prior treatment of a leading `::Box`: the spelling has one named
                // segment even though tree-sitter represents it as a scoped path with no `path`.
                return WrapperSpelling::Bare;
            };
            let Some(root) = path_root_node(path) else { return WrapperSpelling::NotAWrapper };
            match canonical_identifier(root, text) {
                "std" | "core" | "alloc" => WrapperSpelling::Rooted,
                _ => WrapperSpelling::NotAWrapper,
            }
        },
        _ => WrapperSpelling::NotAWrapper,
    }
}

/// Whether the written type starts at a standard deref wrapper. The single-string receiver model
/// cannot preserve Rust's ordered `Box<Worker> -> Worker` lookup, so inference declines the whole
/// receiver rather than choosing either owner.
fn is_deref_wrapper(type_node: Node<'_>, context: Node<'_>, text: &str) -> bool {
    if type_node.kind() != "generic_type" {
        return false;
    }
    let Some(head) = type_node.child_by_field_name("type") else { return false };
    match wrapper_spelling(head, text) {
        // A bare name is the standard pointer only while nothing nearer declares it. A crate with
        // its own `struct Box<T>` gets its methods on the WRAPPER. An import does not block this:
        // `use std::sync::Arc;` is how the standard pointer usually arrives.
        WrapperSpelling::Bare => {
            let Some(tail) = path_tail_node(head) else { return false };
            let written_tail = text.get(tail.byte_range()).unwrap_or_default().trim();
            !declares_type_item(context, written_tail, text)
        },
        WrapperSpelling::Rooted => true,
        WrapperSpelling::NotAWrapper => false,
    }
}

/// Peel only syntax that does not change the receiver owner: references and redundant parentheses.
fn nameable_type_node(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        match node.kind() {
            "reference_type" => node = node.child_by_field_name("type")?,
            "tuple_type" if redundant_parenthesized_type(node) => {
                node = named_children(node).next()?;
            },
            "identifier"
            | "type_identifier"
            | "scoped_identifier"
            | "scoped_type_identifier"
            | "generic_type" => return Some(node),
            _ => return None,
        }
    }
}

fn redundant_parenthesized_type(node: Node<'_>) -> bool {
    if named_children(node).count() != 1 {
        return false;
    }
    let mut all = node.walk();
    !node.children(&mut all).any(|child| child.kind() == ",")
}

fn canonical_identifier<'a>(node: Node<'_>, text: &'a str) -> &'a str {
    let written = text.get(node.byte_range()).unwrap_or_default().trim();
    written.strip_prefix("r#").unwrap_or(written)
}

/// Preserve the prior receiver-hint boundary without reparsing a rendered string. Parentheses,
/// arrays, bounds, pointers and function arrows inside a generic argument made the old canonical
/// output decline; inspect their syntax tokens directly so this refactor does not silently widen
/// persisted hints.
fn has_unsupported_receiver_token(node: Node<'_>, text: &str) -> bool {
    let source = text.get(node.byte_range()).unwrap_or_default();
    if crate::index::edges::scope_grammar::strip_comments(source).contains("for<") {
        return true;
    }
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            if matches!(child.kind(), "block_comment" | "line_comment") {
                continue;
            }
            if matches!(child.kind(), "(" | ")" | "[" | "]" | "+" | "*" | "->" | "as") {
                return true;
            }
            if child.child_count() == 0 {
                let token = text.get(child.byte_range()).unwrap_or_default();
                if token.contains(['(', ')', '[', ']', '+', '*'])
                    || token.contains("->")
                    || token.contains(" as ")
                {
                    return true;
                }
            }
            stack.push(child);
        }
    }
    false
}

fn path_root_node(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        match node.kind() {
            "generic_type" => node = node.child_by_field_name("type")?,
            "scoped_identifier" | "scoped_type_identifier" => {
                node = node
                    .child_by_field_name("path")
                    .or_else(|| node.child_by_field_name("name"))?;
            },
            _ => return Some(node),
        }
    }
}

fn path_tail_node(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        match node.kind() {
            "generic_type" => node = node.child_by_field_name("type")?,
            "scoped_identifier" | "scoped_type_identifier" => {
                node = node.child_by_field_name("name")?;
            },
            _ => return Some(node),
        }
    }
}

/// The identifier a nominal type ends in, normalized the way [`super::render_owner`] normalizes an
/// identifier leaf — trimmed, `r#` stripped, NFC — so it is the same token the rendered path's tail
/// carries. `None` when the peel does not reach an identifier at all (`<W as Tr>::Assoc`, `&W`, a
/// tuple, a macro), which only the rendered path can name.
fn plain_type_tail<'a>(type_node: Node<'_>, text: &'a str) -> Option<std::borrow::Cow<'a, str>> {
    let tail = path_tail_node(type_node)?;
    if !matches!(tail.kind(), "type_identifier" | "identifier") {
        return None;
    }
    let token = text.get(tail.byte_range())?.trim();
    Some(super::nfc_ident(token.strip_prefix("r#").unwrap_or(token)))
}

/// The receiver type a declaration names, or `None` when this pass cannot name it.
///
/// `type_node` supplies the type's structure; `context` supplies lexical binders and declarations.
/// Keeping those roles separate makes unsupported syntax decline by node kind instead of letting a
/// spelling accidentally pass a string predicate.
fn clean_rust_type_name(type_node: Node<'_>, context: Node<'_>, text: &str) -> Option<String> {
    let type_node = nameable_type_node(type_node)?;
    if is_deref_wrapper(type_node, context, text) {
        return None;
    }
    if has_unsupported_receiver_token(type_node, text) {
        return None;
    }
    let rendered = super::render_owner(type_node, text, &[]);
    let type_str = rendered.trim();
    if type_str.is_empty() {
        return None;
    }
    let identity_path = degeneric_path(type_str);
    let tail = qn_tail(identity_path.trim());
    if tail.is_empty() {
        return None;
    }
    let first_char = tail.chars().next()?;
    if !first_char.is_ascii_uppercase() && type_str != "Self" {
        return None;
    }
    // The binder question is asked of the POSITION, never of a list the caller assembled: an
    // enclosing `impl`/`trait` binder is invisible from the node a caller happens to hold, and
    // every list-passing caller guessed too narrowly.
    if binders::binds_name(context, tail, text) {
        return None;
    }
    if let Some((prefix, _)) = identity_path.rsplit_once("::") {
        let root = prefix.split("::").next().unwrap_or(prefix);
        if binders::binds_name(context, root, text) {
            return None;
        }
    }
    Some(type_str.to_string())
}

fn infer_local_var_type_hint(call_node: Node<'_>, text: &str, recv: &str) -> Option<String> {
    let mut current = call_node.parent();
    let mut function_node = None;
    while let Some(ancestor) = current {
        if ancestor.kind() == "closure_expression" {
            return None;
        }
        // A `for`, `if let`/`while let`, or match-arm pattern that rebinds the receiver name
        // takes priority over every outer `let` and parameter, and its bound type (iterator
        // element, scrutinee payload) is not recoverable without type inference. Decline —
        // same rule as closures above: a hint must never survive a rebind this walk cannot
        // see. Checking ancestors only gives the scoping for free: an arm/loop binding stops
        // mattering once the call sits outside it (#567).
        if control_flow_rebinds(ancestor, text, recv) {
            return None;
        }
        if ancestor.kind() == "function_item" {
            function_node = Some(ancestor);
            break;
        }
        current = ancestor.parent();
    }
    let function_node = function_node?;

    let mut child_on_path = call_node;
    let mut ancestor = call_node.parent();
    while let Some(node) = ancestor {
        if node.kind() == "block" {
            match visible_let_binding(node, child_on_path, recv, text) {
                // Assignment changes a value, never the binding's static type. Only a lexical
                // rebind can replace this inference, and the scope walk handles those separately.
                VisibleBinding::Typed(type_name) => return Some(type_name),
                VisibleBinding::Shadowed => return None,
                VisibleBinding::Missing => {},
            }
        }
        if node == function_node {
            break;
        }
        child_on_path = node;
        ancestor = node.parent();
    }

    if let Some(params_node) = function_node.child_by_field_name("parameters") {
        for param in named_children(params_node) {
            if param.kind() != "parameter" {
                continue;
            }
            let Some(pattern) = param.child_by_field_name("pattern").or_else(|| param.child(0))
            else {
                continue;
            };
            if !pattern_binds_name(pattern, text, recv) {
                continue;
            }
            if !match_simple_pattern(pattern, text, recv) {
                return None;
            }
            let type_node = param.child_by_field_name("type")?;
            let type_name = clean_rust_type_name(type_node, type_node, text)?;
            let type_name = canonical_receiver_type(type_name, type_node, text)?;
            return Some(type_name);
        }
    }

    None
}

/// Resolve an as-written type against the lexical context where that type was declared. This must
/// happen before the hint crosses into receiver inference: a constructor declared as returning
/// `Worker` in `mod factory` means `factory::Worker` even when called from another module.
/// `Self` routes through the enclosing impl's own context.
fn canonical_receiver_type(type_name: String, context: Node<'_>, text: &str) -> Option<String> {
    if type_name == "Self" {
        infer_self_type_hint(context, text)
    } else {
        module_qualified_type_path(context, &type_name, text)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum VisibleBinding {
    Typed(String),
    Shadowed,
    Missing,
}

fn visible_let_binding(
    block: Node<'_>,
    before: Node<'_>,
    recv: &str,
    text: &str,
) -> VisibleBinding {
    // An ITEM is in scope for the WHOLE block, not from its own line onward, so a `const`, `static`
    // or `fn` named `recv` takes the name over from a parameter even where it is written BELOW the
    // call — rustc reports the parameter unused and resolves the call against the item. The scan
    // below is position-ordered because a `let` is; an item is not, and there is no expression to
    // read a type from, so the only sound answer is to decline.
    let mut sibling = before.prev_named_sibling();
    while let Some(child) = sibling {
        sibling = child.prev_named_sibling();
        if child.kind() == "macro_invocation"
            || child.named_child(0).is_some_and(|node| node.kind() == "macro_invocation")
        {
            // A statement macro can introduce a `let` for any identifier passed to it. A later
            // explicit declaration would have stopped this reverse walk first; without one, the
            // outer binding is not authoritative.
            return VisibleBinding::Shadowed;
        }
        if child.kind() != "let_declaration" {
            continue;
        }
        let Some(pattern) = child.child_by_field_name("pattern") else {
            continue;
        };
        if !pattern_binds_name(pattern, text, recv) {
            continue;
        }
        if !match_simple_pattern(pattern, text, recv) {
            return VisibleBinding::Shadowed;
        }
        if super::attribute_items(text, child).iter().any(|attribute| {
            let name = attribute
                .trim_start_matches(['#', '['])
                .trim_start()
                .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
                .next();
            matches!(name, Some("cfg" | "cfg_attr"))
        }) {
            // The index does not evaluate cfg. A conditional declaration may disappear and expose
            // an earlier binding with a different type, so neither candidate is authoritative.
            return VisibleBinding::Shadowed;
        }
        let type_name = if let Some(type_node) = child.child_by_field_name("type") {
            clean_rust_type_name(type_node, type_node, text)
                .and_then(|type_name| canonical_receiver_type(type_name, type_node, text))
        } else {
            binding_type_from_scoped_call(child.child_by_field_name("value"), text)
        };
        return type_name.map(VisibleBinding::Typed).unwrap_or(VisibleBinding::Shadowed);
    }
    if block_item_binds_value(block, recv, text) {
        VisibleBinding::Shadowed
    } else {
        VisibleBinding::Missing
    }
}

/// Whether `scope` declares an item that occupies `name` in the VALUE namespace — the namespace a
/// method receiver is resolved in.
///
/// Rust's value namespace holds `const`, `static` and `fn` items, and also the CONSTRUCTOR a unit
/// or tuple struct introduces: `struct worker;` makes `worker` a value as well as a type. A braced
/// struct declares no constructor and so binds only the type name.
fn block_item_binds_value(scope: Node<'_>, name: &str, text: &str) -> bool {
    // A `use` is an ITEM, and an item shadows an outer parameter of the same name. Checked with
    // rustc: `fn f(worker: A) { use crate::items::worker; worker.run() }` calls the IMPORTED unit
    // struct's `run`, not `A::run` — so reading the parameter there does not merely lose an edge,
    // it binds the call to the wrong owner. What the import names is unknowable here (a unit or
    // tuple constructor, an enum variant, a const, a static, a function), so the hint is DECLINED
    // rather than guessed.
    if scope_binds_name(scope, name, text) {
        return true;
    }
    scope_declares_item(scope, name, text, |item| match item.kind() {
        "const_item" | "static_item" | "function_item" => true,
        // Unit (no body) or tuple (an ordered field list); a braced body is neither.
        "struct_item" => !matches!(
            item.child_by_field_name("body").map(|body| body.kind()),
            Some("field_declaration_list")
        ),
        _ => false,
    })
}

fn block_item_binds_type(scope: Node<'_>, name: &str, text: &str) -> bool {
    scope_declares_item(scope, name, text, |item| {
        matches!(
            item.kind(),
            "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item" | "mod_item"
        )
    })
}

fn scope_declares_item(
    scope: Node<'_>,
    name: &str,
    text: &str,
    occupies: impl Fn(Node<'_>) -> bool,
) -> bool {
    named_children(scope).any(|item| {
        occupies(item)
            && child_name_text(item, text)
                .is_some_and(|declared| super::identifiers_equal(&declared, name))
    })
}

/// The type of a `let` binding initialized by a scoped call `Owner::callee(..)`, read off the
/// callee's DECLARED return type when the callee is declared in THIS file.
///
/// Nothing here requires the callee to construct anything: any same-file `function_item` of that
/// name in a tail-matching impl answers. A UFCS method call (`Store::handle(&st)` where
/// `fn handle(&self) -> Handle`) and a pure transformation (`Store::validate(st) -> Self`) type
/// their bindings exactly as a constructor does, because the declaration — not the callee's name
/// or shape — is the evidence.
fn binding_type_from_scoped_call(value: Option<Node<'_>>, text: &str) -> Option<String> {
    let value = value?;
    if value.kind() != "call_expression" {
        return None;
    }
    let function = value.child_by_field_name("function")?;
    if function.kind() != "scoped_identifier" {
        return None;
    }
    let owner_node = function.child_by_field_name("path")?;
    let callee = text.get(function.child_by_field_name("name")?.byte_range())?.trim();
    // The hint comes from the DECLARED return type, never from the callee's name. A same-file
    // `Factory::make() -> Worker` types the binding `Worker` exactly as `Factory::new()` would; an
    // opaque or unit return declines, and so does a callee declared in another file, because Rust
    // does not require any method to return `Self` and the owner name alone is not type evidence
    // (#567).
    //
    // No name filter, therefore: `from` and `with_*` used to be declined outright as
    // builder-shaped, but a builder that declares `-> Self` IS returning the owner, and one that
    // declares something else says so. Reading the declaration answers for every name, so
    // restricting to `new`/`default` only cost coverage — the convention was never what made the
    // hint sound.
    let owner = clean_rust_type_name(owner_node, function, text)?;
    same_file_declared_return(value, text, &owner, callee)
}

/// What `impl <Owner> { fn <callee> … }` declares it returns.
enum DeclaredReturn {
    /// Declared `-> Self`, or the owner type spelled out.
    Owner,
    /// Declared a different clean local type — use THAT as the receiver type.
    Other(String),
    /// Declared something this inference cannot name (generics chains, unit, `impl Trait`).
    Opaque,
}

/// The binding type an `Owner::callee(..)` call implies, from a declaration of `callee` in THIS
/// file. `None` when no same-file impl declares it, when more than one does, or when what it
/// declares is unnameable here — without a readable return declaration the inference must decline
/// rather than assume a scoped call returns its owner.
///
/// Two passes, and the ORDER carries the cost of the whole path. The tail filter walks this file's
/// impl headers, so it is bounded by how many impls the file holds. Canonicalizing the owner scans
/// every enclosing scope for a shadowing item or import, so it costs a pass over the enclosing
/// BLOCK — and every `let` in a long function asks. Most scoped calls name a callee declared in
/// another file, so those must reach their decline from the header walk alone, without paying for a
/// canonical owner path nothing will read.
fn same_file_declared_return(
    node: Node<'_>,
    text: &str,
    owner: &str,
    callee: &str,
) -> Option<String> {
    // `Self` is the one owner spelling that does not carry its type's tail, so it is resolved
    // before the tail filter. That resolution reads the enclosing impl header, not the block.
    let resolved_self = (owner == "Self").then(|| infer_self_type_hint(node, text));
    let owner = match &resolved_self {
        Some(resolved) => resolved.as_deref()?,
        None => owner,
    };
    let owner_tail = qn_tail(owner);
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut tail_matched: Vec<(Node<'_>, String)> = Vec::new();
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        for child in named_children(current) {
            match child.kind() {
                "impl_item" => {
                    let Some(type_node) = child.child_by_field_name("type") else { continue };
                    // The walk visits every impl in the file for every scoped-call binding in it,
                    // so an impl that cannot match must cost a token compare rather than a
                    // canonical render plus its allocation. The peel reaches the same identifier
                    // the rendered path's tail carries, and declines to answer for a target it
                    // cannot reduce to one — those still go the long way round.
                    if plain_type_tail(type_node, text).is_some_and(|tail| tail != owner_tail) {
                        continue;
                    }
                    let impl_type = super::render_owner(type_node, text, &[]);
                    let impl_tail = qn_tail(degeneric_path(&impl_type).trim()).to_string();
                    if impl_tail != owner_tail {
                        continue;
                    }
                    // A BLANKET impl (`impl<Factory: Build> Build for Factory`) names its own
                    // binder as the target, so its tail matches any owner spelled the same way.
                    // Counting it as a candidate is how a real declaration gets outvoted into
                    // ambiguity and its hint dropped — it implements nothing this call names.
                    if binders::binds_name(type_node, &impl_tail, text) {
                        continue;
                    }
                    tail_matched.push((child, impl_type));
                },
                // Impls can sit inside inline modules; anything else cannot contain an impl at
                // item level.
                "mod_item" | "declaration_list" => stack.push(child),
                _ => {},
            }
        }
    }
    if tail_matched.is_empty() {
        return None;
    }
    // Impl candidacy is decided on CANONICAL module-qualified owner paths, never on the type tail
    // alone: one file may hold `mod a { impl Factory }` and `mod b { impl Factory }`, and a tail
    // match would classify `a::Factory::make()` through module b's declaration. Candidates the
    // canonicalization cannot tell apart (either side undecidable) still count — and MORE THAN ONE
    // surviving candidate is ambiguity, which must decline rather than pick a traversal order.
    let owner_canonical = match &resolved_self {
        // `infer_self_type_hint` already resolved against the impl's own module; qualifying again
        // here would turn `a::Factory` into `a::a::Factory`.
        Some(_) => owner.to_string(),
        None => module_qualified_type_path(node, owner, text)?,
    };
    let mut candidates: Vec<DeclaredReturn> = Vec::new();
    for (impl_node, impl_type) in tail_matched {
        let impl_canonical = module_qualified_type_path(impl_node, &impl_type, text);
        // An impl this pass CAN place, on some other type, is not the callee's. One it cannot
        // place is not evidence either way, so it still gets a look.
        if impl_canonical.as_deref().is_some_and(|path| path != owner_canonical) {
            continue;
        }
        let Some(classified) =
            classify_declared_return(impl_node, text, callee, impl_canonical.as_deref())
        else {
            continue; // this impl does not declare the callee
        };
        candidates.push(classified);
    }
    if candidates.len() != 1 {
        return None;
    }
    match candidates.into_iter().next().expect("len checked") {
        DeclaredReturn::Owner => Some(owner_canonical),
        DeclaredReturn::Other(declared) => Some(declared),
        DeclaredReturn::Opaque => None,
    }
}

/// Classify the declared return type of `impl { fn <callee> }`, or `None` when this impl does not
/// declare the callee at all.
fn classify_declared_return(
    impl_node: Node<'_>,
    text: &str,
    callee: &str,
    impl_canonical: Option<&str>,
) -> Option<DeclaredReturn> {
    let body = impl_node.child_by_field_name("body")?;
    for item in named_children(body) {
        if item.kind() != "function_item" || child_name_text(item, text).as_deref() != Some(callee)
        {
            continue;
        }
        let Some(return_node) = item.child_by_field_name("return_type") else {
            // A callee declared to return `()` produces no value to type the binding with.
            return Some(DeclaredReturn::Opaque);
        };
        // Anchored at the RETURN node, so the binders in force are the callee's own impl and fn —
        // `impl<T> Factory<T> { fn new<U>() -> U }` returns whatever the call site instantiates.
        // The caller's binders are not in scope here and are not consulted, so a `fn test<Worker>`
        // calling a declaration that genuinely returns the concrete `Worker` still gets its hint.
        let Some(declared) = clean_rust_type_name(return_node, return_node, text) else {
            return Some(DeclaredReturn::Opaque);
        };
        if declared == "Self" {
            return Some(DeclaredReturn::Owner);
        }
        // A declared type that still carries generic arguments names no receiver: `-> Result<Self,
        // E>` would emit `Result<Self,E>`, which resolves to nothing and — being present — also
        // closes the bare-name fallback, so the call would stop resolving at all.
        if degeneric_path(&declared) != declared {
            return Some(DeclaredReturn::Opaque);
        }
        let Some(declared_canonical) = module_qualified_type_path(item, &declared, text) else {
            return Some(DeclaredReturn::Opaque);
        };
        return Some(if impl_canonical == Some(declared_canonical.as_str()) {
            DeclaredReturn::Owner
        } else {
            DeclaredReturn::Other(declared_canonical)
        });
    }
    None
}

/// The canonical module-qualified path of a type mentioned at `context`: enclosing `mod` names
/// (outermost first) resolved against the reference — `crate::` restarts at the file root,
/// `self::` keeps the current module, each leading `super::` pops one module (declining on
/// underflow), and an otherwise relative path appends to the current module. UFCS and
/// qualified-projection forms (`<T as Trait>::Out`) decline. File-local by construction: paths
/// from two files never compare here.
/// Whether a `type X = …;` visible at `context` gives `name` to something else. Scans outward the
/// way [`lexical_scope_binds_name`] does, since an alias is an item like any other.
/// Whether a type ITEM visible at `context` declares `name` — a struct, enum, union, trait, alias
/// or module of that name, nearer than any import.
fn declares_type_item(context: Node<'_>, name: &str, text: &str) -> bool {
    super::binding_scopes(context).any(|scope| block_item_binds_type(scope, name, text))
}

fn declares_type_alias(context: Node<'_>, name: &str, text: &str) -> bool {
    super::binding_scopes(context)
        .any(|scope| scope_declares_item(scope, name, text, |item| item.kind() == "type_item"))
}

fn module_qualified_type_path(context: Node<'_>, raw_type: &str, text: &str) -> Option<String> {
    let cleaned = raw_type.trim();
    let structural = degeneric_path(cleaned);
    let structural = structural.trim();
    if structural.is_empty() || structural.starts_with('<') || structural.contains(" as ") {
        return None;
    }
    // `Self::Assoc` names an associated item of the enclosing impl, not a type this canonicalizer
    // can place. (Bare `Self` is the impl's own type and routes through `infer_self_type_hint`.)
    if structural.strip_prefix("Self::").is_some() {
        return None;
    }
    // A type ALIAS is a second name for something else, and the impl blocks are on the underlying
    // type: `type Alias = Worker;` puts `run` at `Worker::run`, never at `Alias::run`. Naming the
    // alias would be worse than saying nothing, because a present-but-failing receiver type also
    // closes the bare-name fallback — the call would stop resolving at all. Expanding the alias
    // needs the right-hand side resolved in ITS own scope, which is more than this lexical pass
    // knows, so it declines and leaves the fallback open.
    if declares_type_alias(context, structural, text) {
        return None;
    }
    // An import re-roots a path at the USE's target — somewhere the lexical module chain below
    // cannot describe. Inside `mod inner`, `use crate::workers::Worker` would canonicalize
    // `Worker` to `inner::Worker`, a module that does not hold the type, and a same-tail type in
    // `inner` would then capture the call; `use dep::api as ext` would likewise turn `ext::Worker`
    // into `inner::ext::Worker`. So an import-bound ROOT SEGMENT — the only part an import can
    // bind — keeps the path AS WRITTEN, for bare and qualified forms alike. For a bare name that
    // is exactly the container-based scope a top-level declaration carries.
    //
    // Deliberately NOT decided here: whether that import leaves the workspace. Extraction sees
    // only the `use`'s own root, which cannot tell a dependency from a SIBLING WORKSPACE CRATE —
    // `use other_crate::module;` + `module::Type` is the ordinary multi-crate idiom, and declining
    // it here would destroy a hint that resolves exactly. `ReceiverTypeIdentity::classify` owns
    // that call, against the import scope's `local_crate_roots`, and an `ExternalQualified`
    // identity never binds to a local symbol. Emitting the honest path and letting the informed
    // layer decline it is what keeps the two layers from disagreeing.
    //
    // `crate`/`self`/`super` are path keywords, never import bindings, and are resolved below.
    let root = structural.split("::").next().unwrap_or(structural);
    if !matches!(root, "crate" | "self" | "super") && lexical_scope_binds_name(context, root, text)
    {
        return Some(cleaned.to_string());
    }
    let mut modules = enclosing_module_path(context, text);
    let relative = if let Some(rest) = cleaned.strip_prefix("crate::") {
        modules.clear();
        rest
    } else if let Some(rest) = cleaned.strip_prefix("self::") {
        rest
    } else {
        let mut rest = cleaned;
        while let Some(popped) = rest.strip_prefix("super::") {
            modules.pop()?;
            rest = popped;
        }
        rest
    };
    if relative.is_empty() {
        return None;
    }
    if modules.is_empty() {
        Some(relative.to_string())
    } else {
        Some(format!("{}::{relative}", modules.join("::")))
    }
}

/// Whether a `use` visible at `context` introduces `name`, scanning outward through block scopes
/// and STOPPING at the first enclosing module body.
///
/// The stop is the Rust rule, not an optimization: a `use` belongs to the module it is written in
/// and does NOT descend into a child `mod`. Walking past the boundary makes a file-root
/// `use dep::api;` look like it binds `api` inside `mod inner { mod api { … } }`, where `api` is
/// the child module — so a local type would be mistaken for an imported one. Blocks do chain
/// outward to their module; impl and function bodies are not module boundaries.
///
/// A type ITEM declared closer in is the other half of the same rule. `use dep::Worker;` at the
/// file root does not reach into `fn f() { struct Worker; … }` — the block's own declaration owns
/// the name for the whole block — so the walk stops there and reports the name as NOT imported.
/// Getting that wrong classified the receiver as external and suppressed the local edge.
fn lexical_scope_binds_name(context: Node<'_>, name: &str, text: &str) -> bool {
    // Three outcomes per scope, innermost first: a DECLARATION here settles it (the name is local,
    // not imported), an IMPORT here settles it the other way, and neither means keep walking out.
    super::binding_scopes(context)
        .find_map(|scope| {
            if block_item_binds_type(scope, name, text) {
                return Some(false);
            }
            scope_binds_name(scope, name, text).then_some(true)
        })
        .unwrap_or(false)
}

fn scope_binds_name(scope: Node<'_>, name: &str, text: &str) -> bool {
    named_children(scope).any(|item| {
        if item.kind() != "use_declaration" {
            return false;
        }
        let declaration = &text[item.byte_range()];
        if crate::index::edges::use_has_glob(declaration) {
            return true;
        }
        let declaration = rag_rat_base::canonical::nfc(&declaration.replace("r#", ""));
        let name = rag_rat_base::canonical::nfc(name);
        // Most scopes have no relevant import. Avoid the full use-tree walk unless the
        // declaration can contain this exact identifier.
        declaration.split(|ch: char| !ch.is_alphanumeric() && ch != '_').any(|part| part == name)
            && crate::index::edges::use_binds_name(&declaration, &name)
    })
}

/// Enclosing `mod` names of `node`, outermost first.
fn enclosing_module_path(node: Node<'_>, text: &str) -> Vec<String> {
    let mut modules = Vec::new();
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "mod_item"
            && let Some(name) = child_name_text(ancestor, text)
        {
            modules.push(super::canonical_identifier(&name).into_owned());
        }
        current = ancestor.parent();
    }
    modules.reverse();
    modules
}

/// Whether `node` is a control-flow construct whose pattern rebinds `recv` for the region the
/// call sits in: a `for` loop, a match arm, or an `if let`/`while let` condition (including
/// `let`-chains). Only ever called on ANCESTORS of the call, so a `true` here means the rebind is
/// in scope at the call site — with one deliberate over-approximation: a call inside the
/// scrutinee/iterator expression itself (`if let Some(w) = w.take()`) still sees the OUTER
/// binding, but is declined anyway. Conservative by design (#567).
fn control_flow_rebinds(node: Node<'_>, text: &str, recv: &str) -> bool {
    match node.kind() {
        "for_expression" | "match_arm" => node
            .child_by_field_name("pattern")
            .is_some_and(|pattern| pattern_binds_name(pattern, text, recv)),
        "if_expression" | "while_expression" => node
            .child_by_field_name("condition")
            .is_some_and(|condition| let_condition_binds(condition, text, recv)),
        _ => false,
    }
}

/// Whether any `let_condition` inside `condition` binds `recv`. Recurses through the condition
/// expression so `let`-chains (`let Some(a) = x && let Some(b) = y`) are covered.
fn let_condition_binds(condition: Node<'_>, text: &str, recv: &str) -> bool {
    rag_rat_base::stack::grow_stack(|| {
        if condition.kind() == "let_condition" {
            return condition
                .child_by_field_name("pattern")
                .is_some_and(|pattern| pattern_binds_name(pattern, text, recv));
        }
        named_children(condition).any(|child| let_condition_binds(child, text, recv))
    })
}

fn pattern_binds_name(pattern: Node<'_>, text: &str, recv: &str) -> bool {
    rag_rat_base::stack::grow_stack(|| {
        if matches!(pattern.kind(), "identifier" | "shorthand_field_identifier")
            && node_text(pattern, text).trim() == recv
        {
            return true;
        }
        named_children(pattern).any(|child| pattern_binds_name(child, text, recv))
    })
}

fn match_simple_pattern(pattern_node: Node<'_>, text: &str, recv: &str) -> bool {
    let p_text = node_text(pattern_node, text);
    let cleaned = p_text.trim();
    let name = cleaned.strip_prefix("mut ").unwrap_or(cleaned).trim();
    name == recv && is_simple_identifier(name)
}

#[cfg(test)]
#[path = "edges_tests.rs"]
mod receiver_type_hint_tests;
