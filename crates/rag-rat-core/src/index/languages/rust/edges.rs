//! Rust graph-edge extraction for the shared structural edge walk. It recognizes calls, types,
//! constructions, imports, impl headers, and dispatch facts.
use tree_sitter::Node;

use super::dispatch;
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
    let receiver = match function.kind() {
        "field_expression" => {
            let value_node = function.child_by_field_name("value")?;
            let raw_receiver = node_text(value_node, text);
            clean_receiver_expr(&raw_receiver)?.to_string()
        },
        "scoped_identifier" => scoped_receiver_name(node, text)?,
        _ => return None,
    };
    let recv = receiver.as_str();

    if recv == "self" || recv == "Self" {
        return infer_self_type_hint(node, text);
    }

    if !is_simple_identifier(recv) {
        return None;
    }

    infer_local_var_type_hint(node, text, recv)
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
            let type_text = node_text(type_node, text);
            return clean_rust_type_name(&type_text, &[]);
        }
        current = ancestor.parent();
    }
    None
}

fn clean_rust_type_name(raw: &str, fn_generics: &[&str]) -> Option<String> {
    let s = raw.trim();
    let cleaned = clean_receiver_expr(s).unwrap_or(s);
    if cleaned.starts_with('<') || cleaned.contains(" as ") {
        return None;
    }
    let without_generics = degeneric_path(cleaned);
    let type_str = without_generics.trim();
    if type_str.is_empty() {
        return None;
    }
    // Root-relative qualifiers never appear in a symbol's container-based scope path: strip
    // `crate::`/`self::` so `fn f(w: crate::workers::Worker)` still reaches `workers::Worker` /
    // the `Worker` tail at resolve time. `super::` is relative to a module this function cannot
    // see — decline rather than misresolve (#567).
    let type_str = type_str
        .strip_prefix("crate::")
        .or_else(|| type_str.strip_prefix("self::"))
        .unwrap_or(type_str);
    if type_str.starts_with("super::") {
        return None;
    }
    if type_str.starts_with("dyn ") || type_str.starts_with("impl ") {
        return None;
    }
    if type_str.contains('(')
        || type_str.contains(')')
        || type_str.contains('[')
        || type_str.contains(']')
        || type_str.contains("for<")
        || type_str.contains("->")
        || type_str.contains('+')
        || type_str.contains('*')
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
    if fn_generics.contains(&tail) {
        return None;
    }
    if let Some((prefix, _)) = type_str.rsplit_once("::") {
        let root = prefix.split("::").next().unwrap_or(prefix);
        if fn_generics.contains(&root) {
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

    let mut fn_generics = Vec::new();
    if let Some(type_params) = function_node.child_by_field_name("type_parameters") {
        let mut cursor = type_params.walk();
        for child in type_params.named_children(&mut cursor) {
            if (child.kind() == "type_parameter" || child.kind() == "constrained_type_parameter")
                && let Some(name) = child_name_text(child, text)
            {
                fn_generics.push(name);
            }
        }
    }
    let fn_generic_refs = fn_generics.iter().map(String::as_str).collect::<Vec<_>>();

    let mut child_on_path = call_node;
    let mut ancestor = call_node.parent();
    while let Some(node) = ancestor {
        if node.kind() == "block" {
            match visible_let_binding(
                node,
                child_on_path.start_byte(),
                recv,
                text,
                &fn_generic_refs,
            ) {
                VisibleBinding::Typed(type_name, binding_start) => {
                    // Scan from the BINDING'S block, not the whole function: an assignment can
                    // only affect this binding while it is in scope, and that scope is exactly
                    // this block's subtree.
                    if is_reassigned(node, binding_start, call_start, recv, text) {
                        return None;
                    }
                    return canonical_receiver_type(type_name, call_node, text);
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
            let type_name = param.child_by_field_name("type").and_then(|type_node| {
                clean_rust_type_name(&node_text(type_node, text), &fn_generic_refs)
            })?;
            if is_reassigned(function_node, param.start_byte(), call_start, recv, text) {
                return None;
            }
            return canonical_receiver_type(type_name, call_node, text);
        }
    }

    None
}

fn canonical_receiver_type(type_name: String, call_node: Node<'_>, text: &str) -> Option<String> {
    if type_name == "Self" { infer_self_type_hint(call_node, text) } else { Some(type_name) }
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
    fn_generics: &[&str],
) -> VisibleBinding {
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
            clean_rust_type_name(&node_text(type_node, text), fn_generics)
        } else {
            constructor_owner(child.child_by_field_name("value"), text, fn_generics)
        };
        return type_name
            .map(|type_name| VisibleBinding::Typed(type_name, child.start_byte()))
            .unwrap_or(VisibleBinding::Shadowed);
    }
    VisibleBinding::Missing
}

fn constructor_owner(value: Option<Node<'_>>, text: &str, fn_generics: &[&str]) -> Option<String> {
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
    // hint to `Worker`; an opaque or unit return declines. Only a constructor defined in
    // another file falls back to the naming convention (#567).
    if !matches!(method_name, "new" | "default") {
        return None;
    }
    let owner = clean_rust_type_name(type_part, fn_generics)?;
    match same_file_constructor_return(value, text, &owner, method_name, fn_generics) {
        CtorReturn::SelfLike => Some(owner),
        CtorReturn::Other(declared) => Some(declared),
        CtorReturn::Opaque => None,
        CtorReturn::Unknown => Some(owner),
    }
}

enum CtorReturn {
    /// Declared `-> Self` (or the owner type itself) — the convention holds.
    SelfLike,
    /// Declared a different clean local type — use THAT as the receiver type.
    Other(String),
    /// Declared something this inference cannot name (generics chains, unit, `impl Trait`).
    Opaque,
    /// The constructor is not defined in this file — the declaration is not visible here.
    Unknown,
}

/// Find `impl <Owner> { fn <ctor> ... }` in THIS file and classify its declared return type.
fn same_file_constructor_return(
    node: Node<'_>,
    text: &str,
    owner: &str,
    ctor: &str,
    fn_generics: &[&str],
) -> CtorReturn {
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    // Impl candidacy is decided on CANONICAL module-qualified owner paths, never on the type
    // tail alone: one file may hold `mod a { impl Factory }` and `mod b { impl Factory }`, and a
    // tail match would classify `a::Factory::new()` through module b's constructor. Candidates
    // the canonicalization cannot tell apart (either side undecidable) still count — and MORE
    // THAN ONE surviving candidate is ambiguity, which must decline rather than fall back to
    // the naming convention (only a MISSING same-file definition keeps the convention).
    let owner_tail = qn_tail(owner);
    let owner_canonical = module_qualified_type_path(node, owner, text);
    let mut candidates: Vec<CtorReturn> = Vec::new();
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        let mut cursor = current.walk();
        for child in current.named_children(&mut cursor) {
            match child.kind() {
                "impl_item" => {
                    let Some(type_node) = child.child_by_field_name("type") else { continue };
                    let impl_type = node_text(type_node, text);
                    if qn_tail(degeneric_path(&impl_type).trim()) != owner_tail {
                        continue;
                    }
                    let Some(classified) =
                        classify_constructor_return(child, text, ctor, owner_tail, fn_generics)
                    else {
                        continue; // this impl does not define the constructor
                    };
                    let impl_canonical = module_qualified_type_path(child, &impl_type, text);
                    match (&owner_canonical, &impl_canonical) {
                        (Some(owner_path), Some(impl_path)) if owner_path != impl_path => {},
                        _ => candidates.push(classified),
                    }
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
    owner_tail: &str,
    fn_generics: &[&str],
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
        if trimmed == "Self" || qn_tail(&degeneric_path(trimmed)) == owner_tail {
            return Some(CtorReturn::SelfLike);
        }
        return Some(match clean_rust_type_name(trimmed, fn_generics) {
            Some(declared) => CtorReturn::Other(declared),
            None => CtorReturn::Opaque,
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
fn module_qualified_type_path(context: Node<'_>, raw_type: &str, text: &str) -> Option<String> {
    let degeneric = degeneric_path(raw_type.trim());
    let cleaned = degeneric.trim();
    if cleaned.is_empty() || cleaned.starts_with('<') || cleaned.contains(" as ") {
        return None;
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
        if pattern.kind() == "identifier" && node_text(pattern, text).trim() == recv {
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
    fn test_associated_new_initializer() {
        let code = r#"
            fn test() {
                let w = Worker::new();
                w.run();
            }
        "#;
        let hints = extract_call_hints(code);
        assert_eq!(hints, vec![None, Some("Worker".to_string())]);
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
        // same-tail impl must not classify it. Calls: Factory::new(), worker.run().
        assert_eq!(extract_call_hints(code), vec![None, Some("WorkerA".to_string())]);
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
