//! Rust graph-edge extraction for the shared structural edge walk. It recognizes calls, types,
//! constructions, imports, impl headers, and dispatch facts.
use tree_sitter::Node;

use super::{binders, dispatch};
use crate::index::edges::extract::*;
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
    let function = node.child_by_field_name("function")?;
    match function.kind() {
        // `value.method()` — the receiver is a VALUE, so a local binding of that name is what it
        // names.
        "field_expression" => {
            let value_node = function.child_by_field_name("value")?;
            let receiver = clean_receiver_expr(&node_text(value_node, text))?.to_string();
            let recv = receiver.as_str();
            if recv == "self" || recv == "Self" {
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
        "scoped_identifier" => (scoped_receiver_name(node, text)? == "Self")
            .then(|| infer_self_type_hint(node, text))?,
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
            let cleaned =
                clean_rust_type_name(&super::render_owner(type_node, text, &[]), type_node, text)?;
            // Canonical against the IMPL's own module — `mod inner { impl Worker { … } }`
            // yields `inner::Worker`, matching the method's container-based scope path.
            return module_qualified_type_path(ancestor, &cleaned, text);
        }
        current = ancestor.parent();
    }
    None
}

/// Smart pointers that `Deref` to their contents, so a method call on one is found on the INNER
/// type. `Box<Worker>`, `Rc<Worker>` and `Arc<Worker>` all send `w.run()` to `Worker::run`;
/// `Cow<'a, T>` likewise, and `Pin<P>` derefs to `P::Target`, so `Pin<Box<Worker>>` reaches
/// `Worker` through two peels. All checked against rustc.
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
fn wrapper_spelling(head: &str) -> WrapperSpelling {
    let path: Vec<&str> = head.trim().trim_start_matches("::").split("::").map(str::trim).collect();
    let Some(tail) = path.last() else { return WrapperSpelling::NotAWrapper };
    if !DEREF_WRAPPERS.contains(tail) {
        return WrapperSpelling::NotAWrapper;
    }
    match path.as_slice() {
        [_] => WrapperSpelling::Bare,
        [root, ..] if matches!(*root, "std" | "core" | "alloc") => WrapperSpelling::Rooted,
        _ => WrapperSpelling::NotAWrapper,
    }
}

/// Strip deref-transparent wrappers off a type so the receiver names the type the method is on.
///
/// Without this, `fn f(w: Box<Worker>)` yields the hint `Box`, which is worse than no hint at all:
/// `Box::run` matches nothing AND a present-but-failed local receiver type suppresses the bare-name
/// fallback, so a call that used to resolve stops resolving.
fn peel_deref_wrapper<'a>(type_str: &'a str, context: Node<'_>, text: &str) -> &'a str {
    let mut current = type_str.trim();
    loop {
        let Some(open) = current.find('<') else { return current };
        let head = current[..open].trim_end();
        if !current.trim_end().ends_with('>') {
            return current;
        }
        match wrapper_spelling(head) {
            // A bare name is the standard pointer only while nothing nearer declares it. A crate
            // with its own `struct Box<T>` gets its methods on the WRAPPER, so peeling would name
            // the wrong owner. Only a local DECLARATION blocks it: an import does not, because
            // `use std::sync::Arc;` is how the real one usually arrives.
            WrapperSpelling::Bare if declares_type_item(context, qn_tail(head), text) => {
                return current;
            },
            WrapperSpelling::NotAWrapper => return current,
            WrapperSpelling::Bare | WrapperSpelling::Rooted => {},
        }
        let inner = current.trim_end();
        let inner = inner[open + 1..inner.len() - 1].trim();
        // `Cow<'a, T>` leads with a lifetime, so the LAST argument is the type. A wrapper with more
        // than one type argument is not one of these by construction.
        let last = match top_level_arguments(inner).pop() {
            Some(argument) if !argument.is_empty() => argument,
            _ => return current,
        };
        current = last;
    }
}

/// Split a generic argument list on its own top-level commas.
fn top_level_arguments(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    crate::index::edges::scope_grammar::scan(inner, |at, ch, depth, span| {
        if span.is_code() && depth.is_top() && ch == ',' {
            out.push(inner[start..at].trim());
            start = at + 1;
        }
    });
    out.push(inner[start..].trim());
    out
}

/// Drop the whitespace around a path separator, which Rust allows and which changes nothing about
/// the type: `Self ::Assoc` and `Self::Assoc` name one associated item.
///
/// Literals stay untouched — the scanner already knows where they are, and a `::` inside one is
/// text, not a separator.
fn normalize_separators(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut skip = 0usize;
    crate::index::edges::scope_grammar::scan(raw, |at, ch, _, span| {
        if at < skip {
            return;
        }
        if span.is_code() && ch == ':' && raw[at..].starts_with("::") {
            while out.ends_with(char::is_whitespace) {
                out.pop();
            }
            out.push_str("::");
            skip = at + 2;
            // Whitespace AFTER the separator is dropped by the same rule on the next token, so
            // consume it here rather than leaving `Self:: Assoc`.
            let rest = &raw[skip..];
            skip += rest.len() - rest.trim_start().len();
            return;
        }
        out.push(ch);
    });
    out
}

/// The receiver type a declaration names, or `None` when this pass cannot name it.
///
/// Callers hand in a string the canonical printer produced, not raw source. Rust lets whitespace
/// sit anywhere a token boundary is legal, so `Self ::Assoc` and `Self::Assoc` are one type spelled
/// two ways — and the checks below are string predicates. Reading raw text made every one of them
/// sensitive to spelling: the `Self::` check missed the spaced form and handed back a qualified
/// hint whose tail could bind an unrelated concrete type. Canonicalizing once, here, is what keeps
/// each predicate from having to re-derive it.
fn clean_rust_type_name(raw: &str, at: Node<'_>, text: &str) -> Option<String> {
    let normalized = normalize_separators(raw);
    let s = normalized.trim();
    // A raw-pointer TYPE is not a dereferenced expression: `*mut Worker` must decline here,
    // before the expression cleaner strips the `*` and the pointer masquerades as `Worker`.
    if s.starts_with('*') {
        return None;
    }
    let cleaned = clean_receiver_expr(s).unwrap_or(s);
    if cleaned.starts_with('<') || cleaned.contains(" as ") {
        return None;
    }
    let cleaned = peel_deref_wrapper(cleaned, at, text);
    let without_generics = degeneric_path(cleaned);
    let type_str = without_generics.trim();
    if type_str.is_empty() {
        return None;
    }
    if type_str.starts_with("dyn ") || type_str.starts_with("impl ") {
        return None;
    }
    if type_str.contains(['(', ')', '[', ']', '+', '*'])
        || type_str.contains("for<")
        || type_str.contains("->")
    {
        return None;
    }
    let tail = qn_tail(type_str);
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
    if binders::binds_name(at, tail, text) {
        return None;
    }
    if let Some((prefix, _)) = type_str.rsplit_once("::") {
        let root = prefix.split("::").next().unwrap_or(prefix);
        if binders::binds_name(at, root, text) {
            return None;
        }
    }
    Some(type_str.to_string())
}

fn infer_local_var_type_hint(call_node: Node<'_>, text: &str, recv: &str) -> Option<String> {
    let call_start = call_node.start_byte();
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
            match visible_let_binding(node, child_on_path.start_byte(), recv, text) {
                VisibleBinding::Typed(type_name, binding_start) => {
                    // Scan from the BINDING'S block, not the whole function: an assignment can
                    // only affect this binding while it is in scope, and that scope is exactly
                    // this block's subtree.
                    if is_reassigned(node, binding_start, call_start, recv, text) {
                        return None;
                    }
                    return Some(type_name);
                },
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
            let type_name =
                clean_rust_type_name(&super::render_owner(type_node, text, &[]), type_node, text)?;
            let type_name = canonical_receiver_type(type_name, type_node, text)?;
            if is_reassigned(function_node, param.start_byte(), call_start, recv, text) {
                return None;
            }
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
    Typed(String, usize),
    Shadowed,
    Missing,
}

fn visible_let_binding(
    block: Node<'_>,
    before_byte: usize,
    recv: &str,
    text: &str,
) -> VisibleBinding {
    // An ITEM is in scope for the WHOLE block, not from its own line onward, so a `const`, `static`
    // or `fn` named `recv` takes the name over from a parameter even where it is written BELOW the
    // call — rustc reports the parameter unused and resolves the call against the item. The scan
    // below is position-ordered because a `let` is; an item is not, and there is no expression to
    // read a type from, so the only sound answer is to decline.
    if block_item_binds_value(block, recv, text) {
        return VisibleBinding::Shadowed;
    }
    let mut cursor = block.walk();
    let children = block
        .named_children(&mut cursor)
        .filter(|child| child.end_byte() <= before_byte)
        .collect::<Vec<_>>();
    for child in children.into_iter().rev() {
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
        let type_name = if let Some(type_node) = child.child_by_field_name("type") {
            clean_rust_type_name(&super::render_owner(type_node, text, &[]), type_node, text)
                .and_then(|type_name| canonical_receiver_type(type_name, type_node, text))
        } else {
            constructor_owner(child.child_by_field_name("value"), text)
        };
        return type_name
            .map(|type_name| VisibleBinding::Typed(type_name, child.start_byte()))
            .unwrap_or(VisibleBinding::Shadowed);
    }
    VisibleBinding::Missing
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
        occupies(item) && child_name_text(item, text).is_some_and(|declared| declared == name)
    })
}

fn constructor_owner(value: Option<Node<'_>>, text: &str) -> Option<String> {
    let value = value?;
    if value.kind() != "call_expression" {
        return None;
    }
    let function = value.child_by_field_name("function")?;
    if function.kind() != "scoped_identifier" {
        return None;
    }
    let function_text = node_text(function, text);
    let (type_part, method_part) = function_text.rsplit_once("::")?;
    let method_name = method_part.trim();
    // Only the two strongest conventions survive: Rust does not force ANY method to return
    // `Self`, so `from`/`with_*` (routinely builder- or conversion-shaped) are declined
    // outright, and `new`/`default` are verified against the constructor's DECLARED return type
    // whenever it is visible in this file — a same-file `Factory::new() -> Worker` re-types the
    // hint to `Worker`; an opaque or unit return declines. A constructor defined in another file
    // also declines: Rust does not require `new` or `default` to return `Self`, so the owner name
    // alone is not type evidence (#567).
    if !matches!(method_name, "new" | "default") {
        return None;
    }
    let owner = clean_rust_type_name(type_part, function, text)?;
    let owner_canonical = canonical_receiver_type(owner.clone(), value, text)?;
    match same_file_constructor_return(value, text, &owner_canonical, method_name) {
        CtorReturn::SelfLike => Some(owner_canonical),
        CtorReturn::Other(declared) => Some(declared),
        CtorReturn::Opaque | CtorReturn::Unknown => None,
    }
}

enum CtorReturn {
    /// Declared `-> Self` (or the owner type itself) — the convention holds.
    SelfLike,
    /// Declared a different clean local type — use THAT as the receiver type.
    Other(String),
    /// Declared something this inference cannot name (generics chains, unit, `impl Trait`).
    Opaque,
    /// The constructor is not defined in this file — without its return declaration, inference
    /// must decline rather than assume a constructor-like name returns the owner.
    Unknown,
}

/// Find `impl <Owner> { fn <ctor> ... }` in THIS file and classify its declared return type.
fn same_file_constructor_return(node: Node<'_>, text: &str, owner: &str, ctor: &str) -> CtorReturn {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    // Impl candidacy is decided on CANONICAL module-qualified owner paths, never on the type
    // tail alone: one file may hold `mod a { impl Factory }` and `mod b { impl Factory }`, and a
    // tail match would classify `a::Factory::new()` through module b's constructor. Candidates
    // the canonicalization cannot tell apart (either side undecidable) still count — and MORE
    // THAN ONE surviving candidate is ambiguity, which must decline rather than fall back to
    // the naming convention. Missing or ambiguous same-file definitions decline the hint.
    let owner_tail = qn_tail(owner);
    // `constructor_owner` already resolved this against the call's lexical module. Re-applying
    // module qualification here would turn `a::Factory` into `a::a::Factory`.
    let owner_canonical = owner.to_string();
    let mut candidates: Vec<CtorReturn> = Vec::new();
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            match child.kind() {
                "impl_item" => {
                    let Some(type_node) = child.child_by_field_name("type") else { continue };
                    let impl_type = node_text(type_node, text);
                    let impl_tail = qn_tail(degeneric_path(&impl_type).trim()).to_string();
                    if impl_tail != owner_tail {
                        continue;
                    }
                    // A BLANKET impl (`impl<Factory: Build> Build for Factory`) names its own
                    // binder as the target, so its tail matches any owner spelled the same way.
                    // Counting it as a candidate is how a real constructor gets outvoted into
                    // `Opaque` and its hint dropped — it implements nothing this call constructs.
                    if binders::binds_name(type_node, &impl_tail, text) {
                        continue;
                    }
                    let impl_canonical = module_qualified_type_path(child, &impl_type, text);
                    // An impl this pass CAN place, on some other type, is not this constructor's.
                    // One it cannot place is not evidence either way, so it still gets a look.
                    if impl_canonical.as_deref().is_some_and(|path| path != owner_canonical) {
                        continue;
                    }
                    let Some(classified) =
                        classify_constructor_return(child, text, ctor, impl_canonical.as_deref())
                    else {
                        continue; // this impl does not define the constructor
                    };
                    candidates.push(classified);
                },
                // Constructors can sit inside inline modules; anything else cannot contain an
                // impl at item level.
                "mod_item" | "declaration_list" => stack.push(child),
                _ => {},
            }
        }
    }
    match candidates.len() {
        0 => CtorReturn::Unknown,
        1 => candidates.into_iter().next().expect("len checked"),
        _ => CtorReturn::Opaque,
    }
}

/// Classify the declared return type of `impl { fn <ctor> }`, or `None` when this impl does not
/// define the constructor at all.
fn classify_constructor_return(
    impl_node: Node<'_>,
    text: &str,
    ctor: &str,
    impl_canonical: Option<&str>,
) -> Option<CtorReturn> {
    let body = impl_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for item in body.named_children(&mut cursor) {
        if item.kind() != "function_item" || child_name_text(item, text).as_deref() != Some(ctor) {
            continue;
        }
        let Some(return_node) = item.child_by_field_name("return_type") else {
            // A "constructor" declared to return `()` constructs nothing.
            return Some(CtorReturn::Opaque);
        };
        let declared = node_text(return_node, text);
        let trimmed = declared.trim();
        if trimmed == "Self" {
            return Some(CtorReturn::SelfLike);
        }
        // Anchored at the RETURN node, so the binders in force are the constructor's own impl
        // and fn — `impl<T> Factory<T> { fn new<U>() -> U }` returns whatever the call site
        // instantiates. The caller's binders are not in scope here and are not consulted, so
        // `fn test<Worker>(..)` calling a constructor that genuinely returns the concrete
        // `Worker` still gets its hint.
        let Some(declared) = clean_rust_type_name(trimmed, return_node, text) else {
            return Some(CtorReturn::Opaque);
        };
        let Some(declared_canonical) = module_qualified_type_path(item, &declared, text) else {
            return Some(CtorReturn::Opaque);
        };
        return Some(if impl_canonical == Some(declared_canonical.as_str()) {
            CtorReturn::SelfLike
        } else {
            CtorReturn::Other(declared_canonical)
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
    let degeneric = degeneric_path(raw_type.trim());
    let cleaned = degeneric.trim();
    if cleaned.is_empty() || cleaned.starts_with('<') || cleaned.contains(" as ") {
        return None;
    }
    // `Self::Assoc` names an associated item of the enclosing impl, not a type this canonicalizer
    // can place. (Bare `Self` is the impl's own type and routes through `infer_self_type_hint`.)
    if cleaned.strip_prefix("Self::").is_some() {
        return None;
    }
    // A type ALIAS is a second name for something else, and the impl blocks are on the underlying
    // type: `type Alias = Worker;` puts `run` at `Worker::run`, never at `Alias::run`. Naming the
    // alias would be worse than saying nothing, because a present-but-failing receiver type also
    // closes the bare-name fallback — the call would stop resolving at all. Expanding the alias
    // needs the right-hand side resolved in ITS own scope, which is more than this lexical pass
    // knows, so it declines and leaves the fallback open.
    if declares_type_alias(context, cleaned, text) {
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
    let root = cleaned.split("::").next().unwrap_or(cleaned);
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
    modules.extend(relative.split("::").map(str::to_string));
    Some(modules.join("::"))
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
        // Most scopes have no relevant import. Avoid the full use-tree walk unless the
        // declaration can contain this exact identifier.
        declaration.split(|ch: char| !ch.is_alphanumeric() && ch != '_').any(|part| part == name)
            && crate::index::edges::use_binds_name(declaration, name)
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
            modules.push(name);
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

fn is_reassigned(
    scope_node: Node<'_>,
    binding_start: usize,
    call_start: usize,
    recv: &str,
    text: &str,
) -> bool {
    let mut found = false;
    check_assignment(scope_node, binding_start, call_start, recv, text, &mut found);
    found
}

fn check_assignment(
    node: Node<'_>,
    binding_start: usize,
    call_start: usize,
    recv: &str,
    text: &str,
    found: &mut bool,
) {
    rag_rat_base::stack::grow_stack(|| {
        if *found || node.start_byte() >= call_start {
            return;
        }
        if node.start_byte() > binding_start
            && node.kind() == "assignment_expression"
            && let Some(left) = node.child_by_field_name("left")
        {
            let left_text = node_text(left, text);
            let cleaned = left_text.trim();
            let name = cleaned.strip_prefix('*').unwrap_or(cleaned).trim();
            if name == recv {
                *found = true;
                return;
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            // Only the (binding, call) byte window can hold a relevant assignment — pruning BOTH
            // bounds keeps this scan proportional to the code between binding and call instead
            // of the whole enclosing scope. This runs once per candidate hint, the hottest part
            // of receiver inference on large functions.
            if child.start_byte() < call_start && child.end_byte() > binding_start {
                check_assignment(child, binding_start, call_start, recv, text, found);
            }
        }
    });
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
        // A GLOB does not count. It cannot be resolved here, and declining on one would silence
        // every receiver in the ordinary files that open with `use super::*;`.
        let globbed = "fn f(worker: A) { use crate::items::*; worker.run(); }";
        assert_eq!(extract_call_hints(globbed), vec![Some("A".to_string())]);
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
    fn test_same_file_factory_return_type_overrides_the_convention() {
        let code = r#"
            impl Factory {
                fn new() -> Worker { Worker }
            }
            fn test() {
                let w = Factory::new();
                w.run();
            }
        "#;
        // The declared return type is visible in this file: the receiver is a Worker, not a
        // Factory — the naming convention must lose to the declaration.
        assert_eq!(extract_call_hints(code), vec![None, Some("Worker".to_string())]);
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
    /// a same-spelled concrete owner — counting it would outvote the real one into `Opaque`.
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
    fn test_from_and_builder_initializers_declined() {
        let code = r#"
            fn test(source: u8) {
                let a = Worker::from(source);
                a.run();
                let b = Worker::with_capacity(4);
                b.run();
            }
        "#;
        // Rust does not require `from`/`with_*` to return `Self` — no convention inference.
        assert_eq!(extract_call_hints(code), vec![None, None, None, None]);
    }

    #[test]
    fn test_reassignment_declined() {
        let code = r#"
            fn test() {
                let mut w = Worker::new();
                w = OtherWorker::new();
                w.run();
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![None, None, None]);
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

    /// A smart pointer that derefs to its contents is not the receiver — the method is on what it
    /// holds. Naming the wrapper is worse than declining: `Box::run` matches nothing, and a
    /// present-but-failed local receiver type also suppresses the bare-name fallback, so the call
    /// stops resolving at all.
    #[test]
    fn a_deref_wrapper_names_the_type_it_holds() {
        for wrapper in ["Box<Worker>", "Rc<Worker>", "Arc<Worker>", "std::sync::Arc<Worker>"] {
            let code = format!("fn test(w: {wrapper}) {{ w.run(); }}");
            assert_eq!(
                extract_call_hints(&code),
                vec![Some("Worker".to_string())],
                "{wrapper} dispatches to Worker"
            );
        }
        // A lifetime leads `Cow`'s arguments; the type is the last one. Nesting peels to the type
        // whose method is actually reached.
        assert_eq!(extract_call_hints("fn test(w: Cow<'a, Worker>) { w.run(); }"), vec![Some(
            "Worker".to_string()
        )]);
        assert_eq!(extract_call_hints("fn test(w: Arc<Box<Worker>>) { w.run(); }"), vec![Some(
            "Worker".to_string()
        )]);
        // `Arc<Mutex<T>>` stops at `Mutex` — that is where `lock` lives; `Mutex` does not deref.
        assert_eq!(extract_call_hints("fn test(w: Arc<Mutex<Worker>>) { w.lock(); }"), vec![Some(
            "Mutex".to_string()
        )]);
    }

    /// A wrapper name is only the standard pointer while nothing nearer declares it. A crate with
    /// its own `struct Box<T>` puts `run` on the WRAPPER, so peeling would name the wrong owner —
    /// and since a present-but-failing hint also closes the fallback, it would take the call's last
    /// chance too. An IMPORT does not block the peel: `use std::sync::Arc;` is how the real one
    /// usually arrives, and declining there would lose the common case to protect the rare one.
    #[test]
    fn a_locally_declared_wrapper_name_is_not_the_standard_pointer() {
        let shadowed = r#"
            struct Box<T>(T);
            impl<T> Box<T> { fn run(&self) {} }
            fn test(w: Box<Worker>) { w.run(); }
        "#;
        assert_eq!(extract_call_hints(shadowed), vec![Some("Box".to_string())]);
        // The import spelling still peels, because that is how the real pointer is named.
        let imported = r#"
            use std::sync::Arc;
            fn test(w: Arc<Worker>) { w.run(); }
        "#;
        assert_eq!(extract_call_hints(imported), vec![Some("Worker".to_string())]);
    }

    /// A QUALIFIED head is judged by its whole path, not its tail. `custom::Box<Worker>` ends in a
    /// wrapper name while being somebody else's type, and no local declaration is in scope to say
    /// so — the declaration check looks for a bare `Box`, which a foreign module never provides.
    #[test]
    fn a_qualified_head_is_a_wrapper_only_when_it_is_rooted_where_the_wrappers_live() {
        let foreign = r#"
            fn test(w: custom::Box<Worker>) { w.run(); }
        "#;
        assert_eq!(extract_call_hints(foreign), vec![Some("custom::Box".to_string())]);
        // The standard crates are the roots that do name the real pointer, and a local `struct Box`
        // cannot shadow a path that names its own root.
        for rooted in ["std::boxed::Box", "alloc::sync::Arc", "core::pin::Pin"] {
            let code = format!("struct Box<T>(T);\nfn test(w: {rooted}<Worker>) {{ w.run(); }}");
            assert_eq!(
                extract_call_hints(&code),
                vec![Some("Worker".to_string())],
                "{rooted} names the standard wrapper"
            );
        }
    }

    /// Only the deref-transparent wrappers peel. `Option<Worker>::run` is a compile error, so
    /// unwrapping one would invent a receiver Rust never reaches.
    #[test]
    fn a_container_that_does_not_deref_keeps_its_own_name() {
        for container in ["Option<Worker>", "Vec<Worker>", "Result<Worker, E>"] {
            let code = format!("fn test(w: {container}) {{ w.run(); }}");
            let head = container.split('<').next().unwrap().to_string();
            assert_eq!(extract_call_hints(&code), vec![Some(head)], "{container} does not deref");
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
}
