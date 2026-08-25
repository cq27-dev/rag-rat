//! Rust graph-edge extraction for the shared structural edge walk. It recognizes calls, types,
//! constructions, imports, impl headers, and dispatch facts.
use tree_sitter::Node;

use super::{binders, dispatch};
use crate::index::edges::*;

pub(in crate::index::languages) fn rust_edges(
    EdgeVisit { text, node, symbols: _, path, locator }: EdgeVisit<'_, '_, '_>,
    emit: &mut EdgeEmitter<'_>,
) {
    let out = emit;
    match node.kind() {
        "use_declaration" => {
            let names = identifiers_under(node, text);
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
                    let evidence =
                        full_use_evidence.take().unwrap_or_else(|| edge_evidence(node, text));
                    out.push(file_edge_scoped(
                        path,
                        node,
                        name,
                        Some(evidence),
                        EdgeKind::Imports,
                        EdgeConfidence::NameOnly,
                        Some(scope),
                    ));
                }
            }
            if is_reexport {
                for name in identifiers_under(node, text) {
                    if !is_rust_path_keyword(&name) {
                        out.push(file_edge(
                            path,
                            node,
                            text,
                            name,
                            EdgeKind::Exports,
                            EdgeConfidence::NameOnly,
                        ));
                    }
                }
            }
        },
        "mod_item" =>
            if let Some(name) = child_name_text(node, text) {
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
                    EdgeConfidence::NameOnly,
                    scope,
                ));
            },
        "call_expression" => {
            if let Some(name) = call_target_name(node, text) {
                out.push(symbol_edge_with_context(
                    locator,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
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
                    EdgeConfidence::NameOnly,
                    // The type is the receiver — the LEADING `::` segment (`Foo` in `Foo::bar()`)
                    // — so anchor the range on the function path's first
                    // identifier, not its tail.
                    node.child_by_field_name("function")
                        .and_then(first_identifier_node)
                        .map(CalleeRange::of_node),
                ));
            }
            // #200 dispatch construct fact: a tuple enum-variant construction `Enum::Variant(..)`.
            // Key off the FULL call path's last two PascalCase `::` segments, so
            // `crate::m::Msg::Start(..)` still yields `Msg::Start` (the bare receiver would be
            // `crate`). `Foo::new()` / `T::CONST` are excluded (tail not PascalCase).
            if let Some(key) = node
                .child_by_field_name("function")
                .and_then(|f| dispatch::enum_variant_key(f, text))
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
        },
        "struct_expression" =>
        // #200 dispatch construct fact: a struct enum-variant construction `Enum::Variant { .. }`.
        {
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
        },
        "scoped_identifier" =>
        // #200 dispatch construct fact: a UNIT enum-variant `Enum::Stop` in a VALUE position — a
        // call argument (`send(Msg::Stop)`), a `let` initializer (`let m = Msg::Stop;`), an
        // assignment RHS, or a `return`/`break` value. Not a `call_expression`, so the arms above
        // miss it. The value-position gate keeps an ordinary type/module/use path out;
        // over-emitting for a non-enum `Foo::Bar` is harmless — synthesis only joins a
        // variant whose head is a unique in-scope `enum`, so a non-enum head never yields a
        // `dispatches` edge.
        {
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
        },
        "match_arm" => dispatch::rust_dispatch_handle_facts(text, node, locator, out),
        "macro_invocation" =>
            if let Some(name) = first_identifier_text(node, text) {
                out.push(symbol_edge_with_context(
                    locator,
                    node,
                    text,
                    name,
                    EdgeKind::UsesMacro,
                    EdgeConfidence::NameOnly,
                    EdgeContext::default(),
                    first_identifier_node(node).map(CalleeRange::of_node),
                ));
            },
        "impl_item" => rust_impl_edges(text, node, locator, out),
        "type_identifier" | "scoped_type_identifier" | "generic_type" => {
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    locator,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    last_identifier_node(node).map(final_segment_node).map(CalleeRange::of_node),
                ));
            }
        },
        _ => {},
    }
}
pub(super) fn rust_impl_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let node_text = node_text(node, text);
    let header = node_text.split('{').next().unwrap_or_default();
    let type_names = header
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|part| !part.is_empty())
        .filter(|part| !matches!(*part, "impl" | "for" | "where"))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if node_text.contains(" for ") && type_names.len() >= 2 {
        let trait_name = type_names.first().cloned().unwrap_or_default();
        let type_name = type_names.last().cloned().unwrap_or_default();
        out.push(EdgeCandidate {
            from_symbol_id: locator.find(node.start_byte()).map(|symbol| symbol.id),
            from_name: Some(type_name),
            to_name: trait_name,
            target_qualified_name: None,
            evidence: Some(edge_evidence(node, text)),
            receiver_hint: None,
            receiver_type_hint: None,
            source_span: span_for_node(node),
            // The trait/type names here come from string-splitting the impl header, not from a
            // located identifier node, so there is no clean callee range to record (#67).
            callee_span: None,
            import_scope: None,
            edge_kind: EdgeKind::Implements,
            confidence: EdgeConfidence::NameOnly,
        });
    } else if let Some(type_name) = type_names.first() {
        out.push(symbol_edge(
            locator,
            node,
            type_name.clone(),
            EdgeKind::ReferencesType,
            EdgeConfidence::NameOnly,
            // `type_name` is string-split from the impl header, not a located node — no range
            // (#67).
            None,
        ));
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
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
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
                let mut cursor = node.walk();
                node = node.named_children(&mut cursor).next()?;
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
    let mut named = node.walk();
    if node.named_children(&mut named).count() != 1 {
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
        let mut cursor = params_node.walk();
        for param in params_node.named_children(&mut cursor) {
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
    let mut cursor = scope.walk();
    scope.named_children(&mut cursor).any(|item| {
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
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
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
    let mut cursor = body.walk();
    for item in body.named_children(&mut cursor) {
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
    let mut cursor = scope.walk();
    scope.named_children(&mut cursor).any(|item| {
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
        let mut cursor = condition.walk();
        condition.named_children(&mut cursor).any(|child| let_condition_binds(child, text, recv))
    })
}

fn pattern_binds_name(pattern: Node<'_>, text: &str, recv: &str) -> bool {
    rag_rat_base::stack::grow_stack(|| {
        if matches!(pattern.kind(), "identifier" | "shorthand_field_identifier")
            && node_text(pattern, text).trim() == recv
        {
            return true;
        }
        let mut cursor = pattern.walk();
        pattern.named_children(&mut cursor).any(|child| pattern_binds_name(child, text, recv))
    })
}

fn match_simple_pattern(pattern_node: Node<'_>, text: &str, recv: &str) -> bool {
    let p_text = node_text(pattern_node, text);
    let cleaned = p_text.trim();
    let name = cleaned.strip_prefix("mut ").unwrap_or(cleaned).trim();
    name == recv && is_simple_identifier(name)
}

#[cfg(test)]
mod receiver_type_hint_tests {
    use tree_sitter::Parser;

    use super::*;

    /// A `use` is an ITEM, and an item shadows an outer parameter of the same name. Verified with
    /// rustc: with `mod items { pub struct worker; impl worker { pub fn run(&self) -> u16 } }`,
    /// `fn f(worker: A) -> u16 { use crate::items::worker; worker.run() }` COMPILES — the call went
    /// to the imported type, so reading the parameter binds the call to the wrong owner.
    #[test]
    fn an_import_that_binds_the_receiver_name_declines_the_hint() {
        let shadowed = "fn f(worker: A) { use crate::items::worker; worker.run(); }";
        assert_eq!(extract_call_hints(shadowed), vec![None]);
        // An import of some OTHER name says nothing about this receiver.
        let unrelated = "fn f(worker: A) { use crate::items::other; worker.run(); }";
        assert_eq!(extract_call_hints(unrelated), vec![Some("A".to_string())]);
        // A glob may import a same-named item, so the outer parameter is no longer proven.
        let globbed = "fn f(worker: A) { use crate::items::*; worker.run(); }";
        assert_eq!(extract_call_hints(globbed), vec![None]);
        // A closer explicit declaration proves the binding despite an earlier glob.
        let explicit =
            "fn f(worker: A) { use crate::items::*; let worker: B = value; worker.run(); }";
        assert_eq!(extract_call_hints(explicit), vec![Some("B".to_string())]);
        // An aliased import binds the ALIAS, not the original name.
        let aliased = "fn f(worker: A) { use crate::items::thing as worker; worker.run(); }";
        assert_eq!(extract_call_hints(aliased), vec![None]);
    }

    /// Rust allows whitespace at any legal token boundary, so a spelling is not an identity. The
    /// receiver-type predicates are string predicates, and `Self ::Assoc` slipping past the
    /// `Self::` check produced a qualified hint whose tail could bind an unrelated concrete
    /// `Assoc::run` — while the same type spelled tightly correctly declined.
    #[test]
    fn a_spaced_path_separator_names_the_same_type_as_a_tight_one() {
        for (spaced, tight) in [
            (
                "impl Tr for W { fn f(&self, x: Self ::Assoc) { x.run(); } }",
                "impl Tr for W { fn f(&self, x: Self::Assoc) { x.run(); } }",
            ),
            ("fn f(w: a :: b :: Worker) { w.run(); }", "fn f(w: a::b::Worker) { w.run(); }"),
            ("fn f(w: crate :: Worker) { w.run(); }", "fn f(w: crate::Worker) { w.run(); }"),
        ] {
            assert_eq!(
                extract_call_hints(spaced),
                extract_call_hints(tight),
                "spacing is not identity: {spaced}"
            );
        }
    }

    #[test]
    fn receiver_type_nameability_follows_the_ast_shape() {
        assert_eq!(extract_call_hints("fn f(w: &mut (((Worker)))) { w.run(); }"), vec![Some(
            "Worker".to_string()
        )]);
        for type_name in [
            "*mut Worker",
            "(Worker, Other)",
            "[Worker; 2]",
            "dyn Service",
            "impl Service",
            "fn() -> Worker",
            "<Worker as Service>::Assoc",
            "Receiver!()",
        ] {
            let code = format!("fn f(w: {type_name}) {{ w.run(); }}");
            assert_eq!(extract_call_hints(&code), vec![None], "{type_name} is not one owner path");
        }
    }

    #[test]
    fn the_binding_owner_comes_from_the_scoped_path_node() {
        let code = r#"
            mod factory {
                impl Factory { fn new() -> Worker { Worker } }
            }
            fn f() {
                let worker = factory :: Factory :: new();
                worker.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None, Some("factory::Worker".to_string())]);
    }

    fn extract_call_hints(code: &str) -> Vec<Option<String>> {
        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE;
        parser.set_language(&language.into()).unwrap();
        let tree = parser.parse(code, None).unwrap();
        let root = tree.root_node();

        let mut hints = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                hints.push(infer_rust_receiver_type_hint(node, code));
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        hints.reverse();
        hints
    }

    #[test]
    fn test_self_in_impl() {
        let code = r#"
            impl Worker {
                fn run(&self) {
                    self.execute();
                }
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![Some("Worker".to_string())]);
    }

    #[test]
    fn an_explicit_reference_self_type_keeps_the_impl_owner() {
        let code = r#"
            impl Worker {
                fn run(self: &Self) {
                    self.execute();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("Worker".to_string())]);
    }

    #[test]
    fn an_explicit_deref_wrapper_self_type_declines_single_owner_inference() {
        for receiver in ["Box<Self>", "Rc<Self>", "Arc<Self>", "Pin<Box<Self>>"] {
            let code = format!("impl Worker {{ fn run(self: {receiver}) {{ self.execute(); }} }}");
            assert_eq!(
                extract_call_hints(&code),
                vec![None],
                "{receiver} may dispatch before dereferencing to Worker"
            );
        }
    }

    #[test]
    fn test_associated_self_in_impl() {
        let code = r#"
            impl Worker {
                fn run() {
                    Self::execute();
                }
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![Some("Worker".to_string())]);
    }

    #[test]
    fn projected_self_call_does_not_name_the_impl_owner() {
        let code = r#"
            trait Holder { type Assoc; }
            struct Worker;
            struct Factory;
            impl Holder for Factory {
                type Assoc = Worker;
                fn run() { Self::Assoc::execute(); }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None]);

        let unconditional = r#"
            fn test() {
                #[allow(unused)]
                let worker: Alpha = alpha;
                worker.run();
            }
        "#;
        assert_eq!(extract_call_hints(unconditional), vec![Some("Alpha".to_string())]);
    }

    #[test]
    fn test_simple_param() {
        let code = r#"
            fn process(worker: &mut Worker) {
                worker.run();
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![Some("Worker".to_string())]);
    }

    #[test]
    fn test_annotated_let_binding() {
        let code = r#"
            fn test() {
                let w: Worker = get_worker();
                w.run();
            }
        "#;
        let hints = extract_call_hints(code);
        // First call is get_worker() (no receiver), second call is w.run()
        assert_eq!(hints, vec![None, Some("Worker".to_string())]);
    }

    #[test]
    fn test_constructor_without_visible_declaration_declined() {
        let code = r#"
            fn test() {
                let w = Worker::new();
                w.run();
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![None, None]);
    }

    #[test]
    fn a_declared_return_of_another_type_retypes_the_hint() {
        let code = r#"
            impl Factory {
                fn new() -> Worker { Worker }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
        // The declared return type is visible in this file: the receiver is a Worker, not the
        // owner the call is scoped to.
        assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
    }

    /// The callee's NAME is not what makes the hint sound — its declared return type is — so any
    /// same-file associated item answers, whatever it is called and whatever it does.
    ///
    /// `new`/`default` used to be the only names considered, on the reasoning that Rust forces no
    /// method to return `Self`. True, and exactly why the declaration is read: a builder-shaped
    /// `with_capacity` that declares `-> Self` IS returning the owner, and one that declares
    /// something else says so in the same place. Nothing checks that the callee constructs
    /// anything, so a UFCS method call and a trait impl's associated fn answer on the same terms.
    #[test]
    fn a_same_file_declaration_answers_for_any_associated_item() {
        let self_returning = r#"
            impl Buffer {
                fn with_capacity(n: usize) -> Self { todo!() }
            }
            fn test() {
                let b = Buffer::with_capacity(8);
                b.run();
            }
        "#;
        assert_eq!(
            extract_call_hints(self_returning),
            vec![None, Some("Buffer".to_string())],
            "a `-> Self` builder types its binding like any constructor",
        );

        // And the declaration still decides WHICH type, under a name no convention covers.
        let other_returning = r#"
            impl Factory {
                fn spawn_worker() -> Worker { todo!() }
            }
            fn test() {
                let w = Factory::spawn_worker();
                w.run();
            }
        "#;
        assert_eq!(
            extract_call_hints(other_returning),
            vec![None, Some("Worker".to_string())],
            "the declared return re-types the hint regardless of the name",
        );

        // A `&self` method called through UFCS is a scoped call like any other, and its return is
        // declared in the same place — the callee constructs nothing and still answers.
        let ufcs_method = r#"
            impl Store {
                fn handle(&self) -> Handle { todo!() }
            }
            fn test(st: Store) {
                let h = Store::handle(&st);
                h.zap();
            }
        "#;
        assert_eq!(
            extract_call_hints(ufcs_method),
            vec![None, Some("Handle".to_string())],
            "a UFCS method call is typed by its declared return, not by being a constructor",
        );

        // `Self` in a TRAIT impl is the impl's `type`, not the trait — `impl From<u8> for Config`
        // returning `Self` is a `Config`, never a `From<u8>`.
        let trait_impl = r#"
            impl From<u8> for Config {
                fn from(v: u8) -> Self { todo!() }
            }
            fn test() {
                let c = Config::from(3u8);
                c.zap();
            }
        "#;
        assert_eq!(
            extract_call_hints(trait_impl),
            vec![None, Some("Config".to_string())],
            "a trait impl's `Self` is the implementing type",
        );
    }

    /// A declared return this pass cannot peel names no receiver. `-> Result<Self, E>` reads as a
    /// nameable path right up to its arguments, and carrying it through would be worse than
    /// silence: nothing resolves `Result<Self,E>`, and a receiver type that is PRESENT closes the
    /// bare-name fallback the call had without any hint at all.
    #[test]
    fn a_declared_return_carrying_generic_arguments_declines() {
        for declared in ["Result<Self, E>", "Option<Self>", "Vec<Worker>", "Result<Worker, E>"] {
            let code = format!(
                r#"
                    impl Worker {{
                        fn make() -> {declared} {{ todo!() }}
                    }}
                    fn test() {{
                        let w = Worker::make();
                        w.run();
                    }}
                "#
            );
            assert_eq!(extract_call_hints(&code), vec![None, None], "{declared}");
        }
    }

    /// One type can implement `From` many times, and every impl declares a `from`. Nothing in the
    /// call `Config::from(3u8)` says which one it reaches — the argument types would, and this pass
    /// does not read them — so the honest answer is no hint at all.
    #[test]
    fn duplicate_trait_impls_on_one_type_go_ambiguous() {
        let code = r#"
            impl From<u8> for Config {
                fn from(v: u8) -> Self { todo!() }
            }
            impl From<u16> for Config {
                fn from(v: u16) -> Config { todo!() }
            }
            fn test() {
                let c = Config::from(3u8);
                c.zap();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None, None]);
    }

    /// A binder is in force at every position inside the item that declares it, so the binder
    /// question is asked of the POSITION. These are the shapes where the enclosing item — not the
    /// function the old code looked at — is what introduces the name.
    #[test]
    fn test_binders_from_every_enclosing_item_are_declined() {
        // (source, how many call hints the fixture produces)
        let cases = [
            // The impl binds it; the function does not.
            ("impl<Entry: Runs> Bag<Entry> { fn drive(&self, item: Entry) { item.run(); } }", 1),
            // A `let` annotation under an impl binder.
            ("impl<Entry> Bag<Entry> { fn drive(&self) { let v: Entry = make(); v.run(); } }", 2),
            // A trait's default-method body sits under the TRAIT's binders.
            ("trait Feeder<Entry> { fn feed(&self, e: Entry) { e.run(); } }", 1),
            // A blanket impl binds its own Self type, so `self` names no concrete owner.
            ("impl<X: Runs> Render for X { fn render(&self) { self.tick(); } }", 1),
            // Nested: the binder comes from an impl two levels above the call.
            (
                "impl<Entry> Bag<Entry> { fn drive(&self) { if true { let v: Entry = make(); \
                 v.run(); } } }",
                2,
            ),
        ];
        for (code, hints) in cases {
            let got = extract_call_hints(code);
            assert_eq!(got.len(), hints, "fixture shape changed for {code}");
            assert!(
                got.iter().all(Option::is_none),
                "a generic binder must never be read as a concrete receiver type: {code} -> \
                 {got:?}"
            );
        }
    }

    /// The flip side: a real type of the same shape, with no binder declaring it, still resolves.
    #[test]
    fn test_a_concrete_type_is_not_mistaken_for_a_binder() {
        let code = r#"
            struct Entry;
            impl Bag { fn drive(&self, item: Entry) { item.run(); } }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("Entry".to_string())]);
    }

    /// A blanket impl's target is its own binder, so its `new` is not a candidate constructor for
    /// a same-spelled concrete owner — counting it would outvote the real one into ambiguity.
    #[test]
    fn test_a_blanket_impl_does_not_outvote_the_real_constructor() {
        let code = r#"
            impl<Factory: Build> Build for Factory {
                fn new() -> Factory { todo!() }
            }
            impl Factory {
                fn new() -> Worker { todo!() }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
    }

    /// A constructor's own generic binder is not a type name. `fn new<U>() -> U` returns whatever
    /// the CALL SITE instantiates, so reading `U` as the receiver type would hand the call to any
    /// module-level item that happens to be spelled `U`.
    #[test]
    fn test_constructor_own_generic_return_declined() {
        let code = r#"
            struct U;
            impl U { fn run(&self) {} }
            impl Factory {
                fn new<U>() -> U { todo!() }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None, None]);
    }

    /// The same rule for a binder introduced by the IMPL rather than the function.
    #[test]
    fn test_constructor_impl_generic_return_declined() {
        let code = r#"
            impl<T> Factory<T> {
                fn new() -> T { todo!() }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None, None]);
    }

    #[test]
    fn test_same_file_self_returning_constructor_confirms_the_owner() {
        let code = r#"
            impl Worker {
                fn new() -> Self { Worker }
            }
            fn test() {
                let w = Worker::new();
                w.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
    }

    #[test]
    fn test_self_constructor_resolves_against_enclosing_impl() {
        let code = r#"
            impl Worker {
                fn new() -> Self { Worker }
                fn make() {
                    let worker = Self::new();
                    worker.run();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![
            Some("Worker".to_string()),
            Some("Worker".to_string())
        ]);
    }

    #[test]
    fn test_bare_hint_is_canonical_against_the_lexical_module() {
        let code = r#"
            struct Worker;
            mod inner {
                struct Worker;
                fn f(w: &Worker) {
                    w.run();
                }
            }
        "#;
        // The parameter's `Worker` is `inner::Worker` — a bare hint would exact-match the ROOT
        // `Worker::run` scope instead. Canonicalization pins the lexical module.
        assert_eq!(extract_call_hints(code), vec![Some("inner::Worker".to_string())]);
    }

    #[test]
    fn test_raw_pointer_receiver_type_declined() {
        let code = r#"
            fn f(p: *mut Worker) {
                p.is_null();
            }
        "#;
        // `*mut Worker` is a pointer, not a dereferenced `Worker` — no hint.
        assert_eq!(extract_call_hints(code), vec![None]);
    }

    #[test]
    fn test_sibling_module_constructor_owners_stay_separate() {
        let code = r#"
            mod a {
                impl Factory {
                    fn new() -> WorkerA { WorkerA }
                }
                fn make() {
                    let worker = Factory::new();
                    worker.run();
                }
            }
            mod b {
                impl Factory {
                    fn new() -> WorkerB { WorkerB }
                }
            }
        "#;
        // The unqualified `Factory::new()` inside `mod a` is `a::Factory::new` — module b's
        // same-tail impl must not classify it, and the produced hint is canonical against the
        // call's lexical module. Calls: Factory::new(), worker.run().
        assert_eq!(extract_call_hints(code), vec![None, Some("a::WorkerA".to_string())]);
    }

    #[test]
    fn test_constructor_return_with_same_tail_uses_full_path() {
        let code = r#"
            mod a {
                impl Factory {
                    fn new() -> crate::b::Factory { crate::b::Factory }
                }
            }
            mod b {
                struct Factory;
            }
            fn make() {
                let factory = a::Factory::new();
                factory.run();
            }
        "#;
        // The declaration returns `b::Factory`, which is not self-like merely because its tail
        // matches `a::Factory`. Calls: a::Factory::new(), factory.run().
        assert_eq!(extract_call_hints(code), vec![None, Some("b::Factory".to_string())]);
    }

    #[test]
    fn test_constructor_relative_return_uses_declaration_module() {
        let code = r#"
            mod a {
                struct Worker;
                impl Factory {
                    fn new() -> Worker { Worker }
                }
            }
            mod c {
                fn make() {
                    let worker = crate::a::Factory::new();
                    worker.run();
                }
            }
        "#;
        // `Worker` is written in module a's constructor declaration, not at the call in module c.
        // Calls: crate::a::Factory::new(), worker.run().
        assert_eq!(extract_call_hints(code), vec![None, Some("a::Worker".to_string())]);
    }

    #[test]
    fn test_indistinguishable_constructor_candidates_decline() {
        let code = r#"
            #[cfg(feature = "alpha")]
            impl Factory {
                fn new() -> WorkerA { WorkerA }
            }
            #[cfg(not(feature = "alpha"))]
            impl Factory {
                fn new() -> WorkerB { WorkerB }
            }
            fn make() {
                let worker = Factory::new();
                worker.run();
            }
        "#;
        // Two same-module candidates the canonical path cannot tell apart disagree on the
        // return type — ambiguity must decline, not pick the first traversal hit.
        assert_eq!(extract_call_hints(code), vec![None, None]);
    }

    #[test]
    fn test_unit_returning_constructor_declined() {
        let code = r#"
            impl Worker {
                fn new() {}
            }
            fn test() {
                let w = Worker::new();
                w.run();
            }
        "#;
        // A same-file `new` that returns `()` constructs nothing — the binding's type is unknown.
        assert_eq!(extract_call_hints(code), vec![None, None]);
    }

    #[test]
    fn reassignment_preserves_the_declared_static_type() {
        let code = r#"
            fn test(mut w: Worker, replacement: Worker) {
                w = replacement;
                w.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("Worker".to_string())]);
    }

    #[test]
    fn reassignment_preserves_the_inferred_static_type() {
        let code = r#"
            impl Worker {
                fn new() -> Self { Worker }
            }
            fn test(replacement: Worker) {
                let mut w = Worker::new();
                w = replacement;
                w.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
    }

    #[test]
    fn test_closed_inner_scope_does_not_shadow_parameter() {
        let code = r#"
            fn test(worker: &Alpha) {
                {
                    let worker: Beta;
                }
                worker.run();
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![Some("Alpha".to_string())]);
    }

    #[test]
    fn test_unknown_same_scope_shadow_declined() {
        let code = r#"
            fn test(worker: &Alpha) {
                let worker = unknown;
                worker.run();
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![None]);
    }

    #[test]
    fn conditional_bindings_decline_the_hint() {
        let code = r#"
            fn test() {
                #[cfg(unix)]
                let worker: Alpha = alpha;
                #[cfg(windows)]
                let worker: Beta = beta;
                worker.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None]);
    }

    #[test]
    fn a_preceding_macro_may_introduce_the_receiver_binding() {
        let code = r#"
            fn test(worker: Alpha) {
                bind_worker!(worker);
                worker.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None]);

        let explicit = r#"
            fn test(worker: Alpha) {
                bind_worker!(worker);
                let worker: Beta = beta;
                worker.run();
            }
        "#;
        assert_eq!(extract_call_hints(explicit), vec![Some("Beta".to_string())]);
    }

    #[test]
    fn test_destructuring_shadow_declined() {
        let code = r#"
            fn test(worker: &Alpha) {
                let (worker, _rest): (Beta, u8) = pair;
                worker.run();
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![None]);
    }

    #[test]
    fn test_struct_pattern_shorthand_shadow_declined() {
        let code = r#"
            fn test(x: &Alpha) {
                let Point { x, y: _ } = point;
                x.run();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None]);
    }

    #[test]
    fn test_if_let_struct_pattern_shorthand_shadow_declined() {
        let code = r#"
            fn test(x: &Alpha) {
                if let Point { x, .. } = point {
                    x.run();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None]);
    }

    /// A smart pointer has an ordered receiver chain that one hint cannot represent. A local trait
    /// method on `Box<Worker>` wins before deref reaches `Worker::run`, so naming either owner as
    /// authoritative can produce a false edge.
    #[test]
    fn a_deref_wrapper_declines_single_owner_inference() {
        for wrapper in ["Box<Worker>", "Rc<Worker>", "Arc<Worker>", "std::sync::Arc<Worker>"] {
            let code = format!("fn test(w: {wrapper}) {{ w.run(); }}");
            assert_eq!(
                extract_call_hints(&code),
                vec![None],
                "{wrapper} may dispatch before dereferencing to Worker"
            );
        }
        assert_eq!(extract_call_hints("fn test(w: Cow<'a, Worker>) { w.run(); }"), vec![None]);
        assert_eq!(extract_call_hints("fn test(w: Arc<Box<Worker>>) { w.run(); }"), vec![None]);
        assert_eq!(extract_call_hints("fn test(w: Arc<Mutex<Worker>>) { w.lock(); }"), vec![None]);
    }

    /// A wrapper name is only the standard pointer while nothing nearer declares it. A crate with
    /// its own `struct Box<T>` puts `run` on the WRAPPER, so peeling would name the wrong owner —
    /// and since a present-but-failing hint also closes the fallback, it would take the call's last
    /// chance too. An import of a standard wrapper still declines because local traits can add
    /// wrapper-level methods to it.
    #[test]
    fn a_locally_declared_wrapper_name_is_not_the_standard_pointer() {
        let shadowed = r#"
            struct Box<T>(T);
            impl<T> Box<T> { fn run(&self) {} }
            fn test(w: Box<Worker>) { w.run(); }
        "#;
        assert_eq!(extract_call_hints(shadowed), vec![Some("Box<Worker>".to_string())]);
        let imported = r#"
            use std::sync::Arc;
            fn test(w: Arc<Worker>) { w.run(); }
        "#;
        assert_eq!(extract_call_hints(imported), vec![None]);
        for (declaration, receiver) in [("Box", "r#Box"), ("r#Box", "Box")] {
            let code = format!(
                "struct {declaration}<T>(T); impl<T> {declaration}<T> {{ fn run(&self) {{}} }} fn \
                 test(w: {receiver}<Worker>) {{ w.run(); }}"
            );
            assert_eq!(
                extract_call_hints(&code),
                vec![Some("Box<Worker>".to_string())],
                "raw and ordinary spellings name the same local wrapper"
            );
        }
    }

    /// A QUALIFIED head is judged by its whole path, not its tail. `custom::Box<Worker>` ends in a
    /// wrapper name while being somebody else's type, and no local declaration is in scope to say
    /// so — the declaration check looks for a bare `Box`, which a foreign module never provides.
    #[test]
    fn a_qualified_head_is_a_wrapper_only_when_it_is_rooted_where_the_wrappers_live() {
        let foreign = r#"
            fn test(w: custom::Box<Worker>) { w.run(); }
        "#;
        assert_eq!(extract_call_hints(foreign), vec![Some("custom::Box<Worker>".to_string())]);
        // The standard crates are the roots that name the real pointer, but that pointer may carry
        // a local trait method before deref reaches the inner type.
        for rooted in ["std::boxed::Box", "alloc::sync::Arc", "core::pin::Pin"] {
            let code = format!("struct Box<T>(T);\nfn test(w: {rooted}<Worker>) {{ w.run(); }}");
            assert_eq!(
                extract_call_hints(&code),
                vec![None],
                "{rooted} has more than one possible receiver layer"
            );
        }
        for raw in ["r#Box<Worker>", "r#std::boxed::Box<Worker>"] {
            let code = format!("fn test(w: {raw}) {{ w.run(); }}");
            assert_eq!(extract_call_hints(&code), vec![None], "raw spelling is the same wrapper");
        }
    }

    /// Only the deref-transparent wrappers peel. `Option<Worker>::run` is a compile error, so
    /// unwrapping one would invent a receiver Rust never reaches.
    #[test]
    fn a_container_that_does_not_deref_keeps_its_own_name() {
        for container in ["Option<Worker>", "Vec<Worker>", "Result<Worker, E>"] {
            let code = format!("fn test(w: {container}) {{ w.run(); }}");
            assert_eq!(
                extract_call_hints(&code),
                vec![Some(container.replace(", ", ","))],
                "{container} does not deref"
            );
        }
    }

    /// The qualifier of a scoped call is resolved as a PATH, so a local binding of that name says
    /// nothing about it. rustc confirms the split: with `mod worker { fn run() -> u16 }` beside
    /// `fn f(worker: Alpha)`, `worker::run()` is the module's function while `worker.run()` is the
    /// parameter's method. Reading the parameter for the path form bound the call to `Alpha::run`.
    #[test]
    fn a_path_qualifier_is_not_a_value_receiver() {
        let code = r#"
            mod worker { pub fn run() {} }
            fn test(worker: &Alpha) {
                worker::run();
                worker.run();
            }
        "#;
        // The path call declines; the method call on the same name still reads the parameter.
        assert_eq!(extract_call_hints(code), vec![None, Some("Alpha".to_string())]);
    }

    /// Lowercase `self::` is a path to the CURRENT MODULE, not the enclosing impl —
    /// `self::helper()` calls the module's free function. Only `Self::` names the type.
    #[test]
    fn a_self_path_qualifier_is_the_module_not_the_type() {
        let code = r#"
            fn helper() {}
            impl Worker {
                fn go(&self) {
                    self::helper();
                    Self::make();
                    self.run();
                }
                fn make() {}
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![
            None,
            Some("Worker".to_string()),
            Some("Worker".to_string())
        ]);
    }

    /// A type alias is a second name for something else, and the impls are on what it names. A hint
    /// of `Alias` probes `Alias::run`, finds nothing, and — because a present receiver type also
    /// closes the bare-name fallback — takes the call's last chance with it. Declining leaves that
    /// chance open.
    #[test]
    fn a_local_type_alias_declines_rather_than_naming_itself() {
        let code = r#"
            type Alias = Worker;
            fn test(w: Alias) { w.run(); }
        "#;
        assert_eq!(extract_call_hints(code), vec![None]);
        // A type of the same name that is NOT an alias still resolves normally.
        let concrete = r#"
            struct Alias;
            fn test(w: Alias) { w.run(); }
        "#;
        assert_eq!(extract_call_hints(concrete), vec![Some("Alias".to_string())]);
    }

    /// A block ITEM owns its name for the whole block, including above its own line. rustc reports
    /// the parameter unused here and resolves `worker` to the const, so reading the parameter's
    /// type would bind the call to the wrong method. There is no expression to type the item from,
    /// so the hint declines.
    #[test]
    fn a_value_item_declared_below_the_call_still_shadows_the_parameter() {
        for item in [
            "const worker: Beta = Beta;",
            "static worker: Beta = Beta;",
            "fn worker() {}",
            // A unit or tuple struct introduces a CONSTRUCTOR, so it takes the value namespace
            // too.
            "struct worker;",
            "struct worker(u8);",
        ] {
            let code = format!(
                r#"
                fn test(worker: &Alpha) {{
                    worker.run();
                    {item}
                }}
            "#
            );
            assert_eq!(extract_call_hints(&code), vec![None], "{item} shadows the parameter");
        }
    }

    /// The same rule in the TYPE namespace, on the import side: a block-local `struct Worker`
    /// shadows the module's own `use dep::Worker`, so the receiver is the local type and must not
    /// keep the import's bare form — which `ReceiverTypeIdentity` would classify as external and
    /// suppress. Module-qualifying it is what matches the local `impl`'s own scope, since a
    /// function body contributes no scope segment.
    #[test]
    fn a_block_local_type_shadows_its_modules_import() {
        let code = r#"
            mod inner {
                use dep::Worker;
                fn test() {
                    struct Worker;
                    impl Worker { fn run(&self) {} }
                    let w: Worker = value;
                    w.run();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("inner::Worker".to_string())]);
    }

    /// The control: with no local declaration the same import still wins, bare.
    #[test]
    fn an_import_survives_a_block_that_declares_nothing() {
        let code = r#"
            mod inner {
                use dep::Worker;
                fn test() {
                    let w: Worker = value;
                    w.run();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("Worker".to_string())]);
    }

    /// An imported name keeps its BARE form rather than picking up the lexical module: `Url`
    /// here is NOT `inner::Url`. Whether the import leaves the workspace is not decided at
    /// extraction — see `receiver_type_identity_classification` and
    /// `external_receiver_type_hint_never_binds_locally` for the layer that declines it.
    #[test]
    fn test_inline_module_import_keeps_the_bare_name() {
        let code = r#"
            mod inner {
                use url::Url;
                fn test(url: &Url) {
                    url.join("child");
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("Url".to_string())]);
    }

    /// An impl body is not a module boundary, so a file-root `use` is in scope inside it.
    #[test]
    fn test_impl_method_sees_module_level_import() {
        let code = r#"
            use url::Url;
            struct Client;
            impl Client {
                fn join(u: Url) {
                    u.join("next");
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("Url".to_string())]);
    }

    /// A `crate::`-rooted import is KNOWN local, but that does not make the lexical module its
    /// owner: prefixing the module chain here would produce `inner::Worker`, a module that does
    /// not hold the type, and a same-tail `Worker` inside `inner` would then capture the call.
    /// The imported name keeps its BARE form — the scope a top-level declaration actually carries.
    #[test]
    fn test_inline_module_local_import_drops_the_lexical_module_prefix() {
        let code = r#"
            mod inner {
                use crate::workers::Worker;
                fn test(w: &Worker) {
                    w.run();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("Worker".to_string())]);
    }

    /// `self::`/`super::` roots are resolved against the USE's module, not the reference's, so
    /// they get the same treatment as `crate::`.
    #[test]
    fn test_relative_local_imports_keep_the_bare_name() {
        for import in ["use self::workers::Worker;", "use super::workers::Worker;"] {
            let code = format!("mod inner {{ {import} fn test(w: &Worker) {{ w.run(); }} }}");
            assert_eq!(
                extract_call_hints(&code),
                vec![Some("Worker".to_string())],
                "{import} must not pick up the lexical module prefix"
            );
        }
    }

    /// An explicitly-written local path is NOT an import — it names its own owner verbatim, so it
    /// still earns the fully qualified hint. This is the boundary the rule above must not cross.
    #[test]
    fn test_inline_module_explicit_path_still_earns_a_qualified_hint() {
        let code = r#"
            mod inner {
                use crate::workers::Worker;
                fn test(w: &crate::workers::Worker) {
                    w.run();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("workers::Worker".to_string())]);
    }

    /// An import binds the ROOT of a qualified path just as it binds a bare name. Prefixing the
    /// lexical module onto `ext::Url` would mint `inner::ext::Url` — a locally-rooted path whose
    /// tail retry can land on any local `Url`. The written path is what resolution can classify.
    #[test]
    fn test_inline_module_qualified_alias_keeps_the_written_path() {
        let code = r#"
            mod inner {
                use url::api as ext;
                fn test(w: &ext::Url) {
                    w.join("child");
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("ext::Url".to_string())]);
    }

    /// A SIBLING WORKSPACE CRATE is the case extraction must not adjudicate. A `text::Decoder`
    /// reached through `use libb::text;` resolves EXACTLY against the sibling's symbols, because
    /// the import scope knows `libb` is a local crate root. Extraction sees only the `use`'s own
    /// root, which looks identical to a third-party dependency — so it emits the written path and
    /// leaves the call to `ReceiverTypeIdentity::classify`. Declining here would destroy a
    /// correct resolution.
    #[test]
    fn test_sibling_workspace_crate_import_keeps_the_written_path() {
        let code = r#"
            use libb::text;
            fn drive(d: &text::Decoder) {
                d.decode();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("text::Decoder".to_string())]);
    }

    /// A `use` belongs to the module it is written in and does NOT descend into a child `mod`.
    /// The file-root `use url::api;` is out of scope inside `mod inner`, where `api` is the child
    /// module — so the type is the LOCAL `inner::api::Url`, and the outward walk must stop at the
    /// module boundary rather than mistake it for the import.
    #[test]
    fn test_a_parent_modules_import_does_not_reach_into_a_child_module() {
        let code = r#"
            use url::api;
            mod inner {
                pub mod api { pub struct Url; }
                fn test(u: &api::Url) {
                    u.join("child");
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("inner::api::Url".to_string())]);
    }

    /// `Self::Assoc` is an associated item of the enclosing impl, not a placeable type — a hint
    /// of `Self::Inner` would tail-retry onto any local `Inner`.
    #[test]
    fn test_self_qualified_associated_type_declined() {
        let code = r#"
            struct Holder;
            impl Holder {
                fn test(w: Self::Inner) {
                    w.run();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None]);
    }

    /// The same rule with a local import: `use crate::workers;` re-roots `workers::Worker` at the
    /// crate root, so the path stands AS WRITTEN — the lexical `inner` is not its owner.
    #[test]
    fn test_inline_module_qualified_local_import_keeps_the_written_path() {
        let code = r#"
            mod inner {
                use crate::workers;
                fn test(w: &workers::Worker) {
                    w.run();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![Some("workers::Worker".to_string())]);
    }

    #[test]
    fn test_closure_receiver_declined() {
        let code = r#"
            fn test(worker: &Worker) {
                let _closure = || worker.run();
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![None]);
    }

    #[test]
    fn test_crate_qualified_param_type_strips_the_root() {
        let code = r#"
            fn test(w: &crate::workers::Worker) {
                w.run();
            }
        "#;
        // `crate::` never appears in a container-based scope path — the stored hint drops it.
        assert_eq!(extract_call_hints(code), vec![Some("workers::Worker".to_string())]);
    }

    #[test]
    fn test_super_qualified_param_type_declined() {
        let code = r#"
            fn test(w: &super::Worker) {
                w.run();
            }
        "#;
        // `super::` is relative to a module the extractor cannot resolve — decline.
        assert_eq!(extract_call_hints(code), vec![None]);
    }

    #[test]
    fn test_for_loop_rebind_declined() {
        let code = r#"
            fn test(worker: &Alpha) {
                for worker in fetch_betas() {
                    worker.run();
                }
                worker.report();
            }
        "#;
        // fetch_betas() has no receiver; the loop-rebound worker.run() must decline; the call
        // after the loop is back in the parameter's scope and sees Alpha again.
        assert_eq!(extract_call_hints(code), vec![None, None, Some("Alpha".to_string())]);
    }

    #[test]
    fn test_if_let_rebind_declined() {
        let code = r#"
            fn test(msg: &Alpha) {
                if let Some(msg) = incoming() {
                    msg.send();
                }
            }
        "#;
        // incoming() has no receiver; msg.send() must not inherit the Alpha parameter.
        assert_eq!(extract_call_hints(code), vec![None, None]);
    }

    #[test]
    fn test_while_let_rebind_declined() {
        let code = r#"
            fn test(job: &Alpha) {
                while let Some(job) = queue_pop() {
                    job.execute();
                }
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![None, None]);
    }

    #[test]
    fn test_match_arm_rebind_scoped_per_arm() {
        let code = r#"
            fn test(event: &Alpha) {
                match next_event() {
                    Some(event) => event.apply(),
                    None => event.apply(),
                }
            }
        "#;
        // The first arm rebinds `event` (decline); the second arm's pattern does not, so the
        // Alpha parameter is still the receiver there — arm scoping is per-arm, not per-match.
        assert_eq!(extract_call_hints(code), vec![None, None, Some("Alpha".to_string())]);
    }

    #[test]
    fn test_generic_and_trait_object_receivers_declined() {
        let generic = r#"
            fn test<T>(worker: T) {
                worker.run();
            }
        "#;
        let trait_object = r#"
            fn test(worker: &dyn Service) {
                worker.run();
            }
        "#;
        assert_eq!(extract_call_hints(generic), vec![None]);
        assert_eq!(extract_call_hints(trait_object), vec![None]);
    }

    #[test]
    fn concrete_generic_receiver_arguments_are_preserved() {
        let code = r#"
            fn test(first: Foo<u8>, second: Foo<u16>) {
                first.run::<u8>();
                second.run::<u16>();
            }
        "#;
        assert_eq!(extract_call_hints(code), vec![
            Some("Foo<u8>".to_string()),
            Some("Foo<u16>".to_string()),
        ]);
    }

    #[test]
    fn generic_arguments_keep_the_existing_nameability_boundary() {
        for type_name in [
            "Envelope<(Worker, Other)>",
            "Matrix<[u8; 4]>",
            "Callback<fn() -> Worker>",
            "Marker<'*'>",
            "Envelope<Ty!{\"for<\"}>",
            "Envelope<Ty!{for<}>",
        ] {
            let code = format!("fn f(value: {type_name}) {{ value.run(); }}");
            assert_eq!(
                extract_call_hints(&code),
                vec![None],
                "the structural refactor must not widen persisted hints"
            );
        }
    }

    #[test]
    fn canonical_constructor_returns_still_decline_aliases_and_binders() {
        let raw_alias = r#"
            struct Worker;
            type r#Alias = Worker;
            struct Factory;
            impl Factory { fn new() -> r#Alias { Worker } }
            fn f() { let worker = Factory::new(); worker.run(); }
        "#;
        assert_eq!(extract_call_hints(raw_alias), vec![None, None]);

        let decomposed = "Cafe\u{301}";
        let generic = format!(
            "struct Worker; struct Factory; impl Factory {{ fn new<{decomposed}>(value: \
             {decomposed}) -> {decomposed} {{ value }} }} fn f() {{ let worker = \
             Factory::new(Worker); worker.run(); }}"
        );
        assert_eq!(extract_call_hints(&generic), vec![None, None]);
    }

    #[test]
    fn canonical_constructor_returns_recognize_raw_and_nfc_imports() {
        let raw = r#"
            mod dep { pub struct Worker; }
            mod inner {
                use crate::dep::r#Worker;
                struct Factory;
                impl Factory { fn new() -> r#Worker { todo!() } }
                fn f() { let worker = Factory::new(); worker.run(); }
            }
        "#;
        assert_eq!(extract_call_hints(raw), vec![None, Some("Worker".to_string())]);

        let decomposed = "Cafe\u{301}";
        let imported = format!(
            "mod dep {{ pub struct Café; }} mod inner {{ use crate::dep::{decomposed}; struct \
             Factory; impl Factory {{ fn new() -> {decomposed} {{ todo!() }} }} fn f() {{ let \
             value = Factory::new(); value.run(); }} }}"
        );
        assert_eq!(extract_call_hints(&imported), vec![None, Some("Café".to_string())]);
    }

    #[test]
    fn qualified_constructor_modules_use_canonical_identifiers() {
        let raw = r#"
            mod r#type {
                pub struct Worker;
                pub struct Factory;
                impl Factory { pub fn new() -> Worker { Worker } }
            }
            fn f() { let worker = r#type::Factory::new(); worker.run(); }
        "#;
        assert_eq!(extract_call_hints(raw), vec![None, Some("type::Worker".to_string())]);

        let decomposed = "Cafe\u{301}";
        let unicode = format!(
            "mod {decomposed} {{ pub struct Worker; pub struct Factory; impl Factory {{ pub fn \
             new() -> Worker {{ Worker }} }} }} fn f() {{ let worker = \
             {decomposed}::Factory::new(); worker.run(); }}"
        );
        assert_eq!(extract_call_hints(&unicode), vec![None, Some("Café::Worker".to_string())]);
    }

    #[test]
    fn comments_do_not_make_a_receiver_type_unnameable() {
        assert_eq!(extract_call_hints("fn f(w: Foo</* note */ Worker>) { w.run(); }"), vec![Some(
            "Foo<Worker>".to_string()
        )]);
        assert_eq!(extract_call_hints("fn f(w: &/* note */ Worker) { w.run(); }"), vec![Some(
            "Worker".to_string()
        )]);
    }

    #[test]
    fn turbofish_dot_calls_keep_receiver_context() {
        let code = "fn test(worker: Worker) { worker.run::<u8>(); factory().run(); }";
        let calls = edge_candidates(
            std::path::Path::new("lib.rs"),
            rag_rat_base::language::Language::Rust,
            code,
            &[],
        )
        .unwrap()
        .into_iter()
        .filter(|edge| edge.edge_kind == EdgeKind::CallsName && edge.to_name == "run")
        .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);

        let typed =
            calls.iter().find(|edge| edge.receiver_hint.as_deref() == Some("worker")).unwrap();
        assert_eq!(typed.receiver_type_hint.as_deref(), Some("Worker"));
        assert_eq!(typed.target_qualified_name.as_deref(), Some("worker::run"));

        let unknown =
            calls.iter().find(|edge| edge.receiver_hint.as_deref() == Some("factory()")).unwrap();
        assert_eq!(unknown.receiver_type_hint, None);
        assert_eq!(unknown.target_qualified_name.as_deref(), Some("factory()::run"));
    }

    #[test]
    fn receiver_hint_scan_handles_twenty_thousand_nearby_bindings() {
        let mut code = String::from("fn test() {");
        for _ in 0..20_000 {
            code.push_str("let worker = value; worker.run();");
        }
        code.push('}');

        let hints = extract_call_hints(&code);
        assert_eq!(hints.len(), 20_000);
        assert!(hints.into_iter().all(|hint| hint.is_none()));
    }

    /// A plain-identifier initializer never enters the same-file declaration scan, so the case
    /// above pins only the backward `let` walk. This one pins the scan itself, on the answer it
    /// gives most often: the callee is declared in another file.
    ///
    /// The ceiling is the ORDER inside that scan. Deciding "no impl here declares it" reads this
    /// file's impl headers; canonicalizing the owner instead scans every enclosing scope for a
    /// shadowing item or import, which is a pass over the enclosing block. Pay the second one
    /// before the first and the cost is bindings x block size, and this fixture stops finishing.
    #[test]
    fn receiver_hint_scan_handles_twenty_thousand_scoped_call_initializers() {
        let mut code = String::from("fn test() {");
        for _ in 0..20_000 {
            code.push_str("let worker = Owner::make(); worker.run();");
        }
        code.push('}');

        let hints = extract_call_hints(&code);
        assert_eq!(hints.len(), 40_000);
        assert!(hints.into_iter().all(|hint| hint.is_none()));
    }

    /// The case above holds no impl at all, so its scan stops at the header walk. This one pins
    /// where that walk leads: an impl whose tail matches sends the binding on to canonicalize its
    /// owner — a pass over the enclosing block — and the file's other impls are re-walked for every
    /// binding that asks. Both terms are paid under any callee name now, where the `new`/`default`
    /// gate used to return before either, so the header walk has to decline a non-matching impl on
    /// an identifier compare rather than a canonical render.
    #[test]
    fn receiver_hint_scan_handles_a_matching_impl_among_thousands() {
        let mut code = String::from("impl Owner { fn make() -> Self { todo!() } }\n");
        for i in 0..2_000 {
            code.push_str(&format!("impl Other{i} {{ fn make() -> Self {{ todo!() }} }}\n"));
        }
        code.push_str("fn test() {");
        for _ in 0..1_000 {
            code.push_str("let worker = Owner::make(); worker.run();");
        }
        code.push('}');

        let hints = extract_call_hints(&code);
        assert_eq!(hints.len(), 2_000);
        assert_eq!(
            hints.iter().filter(|hint| hint.as_deref() == Some("Owner")).count(),
            1_000,
            "the one impl whose tail matches types every binding",
        );
    }
}
