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
                    if is_reassigned(function_node, binding_start, call_start, recv, text) {
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
    if !matches!(method_name, "new" | "default" | "from") && !method_name.starts_with("with_") {
        return None;
    }
    clean_rust_type_name(type_part, fn_generics)
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
            if child.start_byte() < call_start {
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
