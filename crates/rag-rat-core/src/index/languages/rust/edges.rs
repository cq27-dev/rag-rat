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
    if function.kind() != "field_expression" {
        return None;
    }
    let value_node = function.child_by_field_name("value")?;
    let raw_recv = node_text(value_node, text);
    let recv = clean_receiver_expr(&raw_recv)?;

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
        if ancestor.kind() == "impl_item" {
            if let Some(type_node) = ancestor.child_by_field_name("type") {
                let type_text = node_text(type_node, text);
                return clean_rust_type_name(&type_text, &[]);
            }
        }
        current = ancestor.parent();
    }
    None
}

fn clean_rust_type_name(raw: &str, fn_generics: &[&str]) -> Option<String> {
    let s = raw.trim();
    let cleaned = clean_receiver_expr(s).unwrap_or(s);
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
    let mut scope_node = None;
    while let Some(ancestor) = current {
        if matches!(ancestor.kind(), "function_item" | "closure_expression") {
            scope_node = Some(ancestor);
            break;
        }
        current = ancestor.parent();
    }
    let scope_node = scope_node?;

    let mut fn_generics = Vec::new();
    if scope_node.kind() == "function_item" {
        if let Some(type_params) = scope_node.child_by_field_name("type_parameters") {
            let mut cursor = type_params.walk();
            for child in type_params.named_children(&mut cursor) {
                if child.kind() == "type_parameter" || child.kind() == "constrained_type_parameter"
                {
                    if let Some(name) = child_name_text(child, text) {
                        fn_generics.push(name);
                    }
                }
            }
        }
    }

    let mut candidate_type: Option<String> = None;
    let mut binding_start_byte: usize = 0;

    if scope_node.kind() == "function_item" {
        if let Some(params_node) = scope_node.child_by_field_name("parameters") {
            let mut cursor = params_node.walk();
            for param in params_node.named_children(&mut cursor) {
                if param.kind() == "parameter" {
                    let pattern = param.child_by_field_name("pattern").or_else(|| param.child(0));
                    if let Some(pattern_node) = pattern {
                        if match_simple_pattern(pattern_node, text, recv) {
                            if let Some(type_node) = param.child_by_field_name("type") {
                                let type_text = node_text(type_node, text);
                                let fn_gen_refs =
                                    fn_generics.iter().map(|s| s.as_str()).collect::<Vec<_>>();
                                if let Some(ct) = clean_rust_type_name(&type_text, &fn_gen_refs) {
                                    candidate_type = Some(ct);
                                    binding_start_byte = param.start_byte();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    find_let_bindings(
        scope_node,
        call_start,
        recv,
        text,
        &fn_generics,
        &mut candidate_type,
        &mut binding_start_byte,
    );

    let candidate_type = candidate_type?;

    if is_reassigned(scope_node, binding_start_byte, call_start, recv, text) {
        return None;
    }

    Some(candidate_type)
}

fn match_simple_pattern(pattern_node: Node<'_>, text: &str, recv: &str) -> bool {
    let p_text = node_text(pattern_node, text);
    let cleaned = p_text.trim();
    let name = cleaned.strip_prefix("mut ").unwrap_or(cleaned).trim();
    name == recv && is_simple_identifier(name)
}

fn find_let_bindings(
    node: Node<'_>,
    call_start: usize,
    recv: &str,
    text: &str,
    fn_generics: &[String],
    candidate_type: &mut Option<String>,
    binding_start_byte: &mut usize,
) {
    if node.start_byte() >= call_start {
        return;
    }
    if node.kind() == "let_declaration" {
        if let Some(pattern_node) = node.child_by_field_name("pattern") {
            if match_simple_pattern(pattern_node, text, recv) {
                let fn_gen_refs = fn_generics.iter().map(|s| s.as_str()).collect::<Vec<_>>();
                if let Some(type_node) = node.child_by_field_name("type") {
                    let type_text = node_text(type_node, text);
                    if let Some(ct) = clean_rust_type_name(&type_text, &fn_gen_refs) {
                        *candidate_type = Some(ct);
                        *binding_start_byte = node.start_byte();
                    } else {
                        *candidate_type = None;
                        *binding_start_byte = node.start_byte();
                    }
                } else if let Some(val_node) = node.child_by_field_name("value") {
                    if val_node.kind() == "call_expression" {
                        if let Some(fn_node) = val_node.child_by_field_name("function") {
                            if fn_node.kind() == "scoped_identifier" {
                                let fn_text = node_text(fn_node, text);
                                if let Some((type_part, method_part)) = fn_text.rsplit_once("::") {
                                    let method_name = method_part.trim();
                                    if method_name == "new"
                                        || method_name == "default"
                                        || method_name == "from"
                                        || method_name.starts_with("with_")
                                    {
                                        if let Some(ct) =
                                            clean_rust_type_name(type_part, &fn_gen_refs)
                                        {
                                            *candidate_type = Some(ct);
                                            *binding_start_byte = node.start_byte();
                                        } else {
                                            *candidate_type = None;
                                            *binding_start_byte = node.start_byte();
                                        }
                                    } else {
                                        *candidate_type = None;
                                        *binding_start_byte = node.start_byte();
                                    }
                                }
                            } else {
                                *candidate_type = None;
                                *binding_start_byte = node.start_byte();
                            }
                        }
                    } else {
                        *candidate_type = None;
                        *binding_start_byte = node.start_byte();
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.start_byte() < call_start {
            find_let_bindings(
                child,
                call_start,
                recv,
                text,
                fn_generics,
                candidate_type,
                binding_start_byte,
            );
        }
    }
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
    if *found || node.start_byte() >= call_start {
        return;
    }
    if node.start_byte() > binding_start && node.kind() == "assignment_expression" {
        if let Some(left) = node.child_by_field_name("left") {
            let left_text = node_text(left, text);
            let cleaned = left_text.trim();
            let name = cleaned.strip_prefix('*').unwrap_or(cleaned).trim();
            if name == recv {
                *found = true;
                return;
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.start_byte() < call_start {
            check_assignment(child, binding_start, call_start, recv, text, found);
        }
    }
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
}
