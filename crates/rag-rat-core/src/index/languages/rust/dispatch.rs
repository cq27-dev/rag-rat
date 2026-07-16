//! Rust dispatch-edge synthesis (#200/#207/#208) — emits the actor-channel
//! `dispatch_construct` / `dispatch_handle` graph facts. `rust_edges`
//! calls the four `pub(super)` entry points (`enum_variant_key`, `dispatch_fact`,
//! `scoped_identifier_in_value_position`, `rust_dispatch_handle_facts`); everything else is the
//! CLOSED conservative handler-call recognizer (`result_handler_calls` & friends) whose
//! false-edge-is-a-bug contract is documented in the repo memories bound here and on the parent.
use tree_sitter::Node;

use crate::index::edges::*;

/// PascalCase test for the enum/variant convention (#200): first char uppercase AND at least one
/// lowercase — so `MlReq`/`Upsert` qualify but `new`, a SCREAMING `CONST`, and a bare `T` do not.
fn is_pascal_case(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase) && name.chars().any(char::is_lowercase)
}

/// The `Enum::Variant` dispatch key from a scoped path node — its last two `::`-segments when BOTH
/// are PascalCase, else `None`. Robust to longer paths (`crate::ml::MlReq::Upsert` →
/// `MlReq::Upsert`) so a construction site and a handler arm produce the SAME key regardless of
/// import depth (#200). Splits the node TEXT on `::` rather than counting identifier children —
/// tree-sitter treats a `scoped_(type_)identifier` as a single identifier token, so
/// `identifiers_under` returns the whole path as one element.
///
/// `Self::Variant` is rewritten to `<impl type>::Variant` using the enclosing `impl` block — `Self`
/// is NOT a stable cross-file identity, so two unrelated enums each writing `Self::Ripe` would
/// otherwise collapse to one key and cross-link (the actor pattern `impl MlReq { fn enqueue() {
/// send(Self::Upsert) } }` is exactly this). With no enclosing impl type, a `Self`-headed key is
/// dropped rather than admitted under the bare `Self` head.
pub(super) fn enum_variant_key(node: Node<'_>, text: &str) -> Option<String> {
    let full = node.utf8_text(text.as_bytes()).ok()?;
    let segments: Vec<&str> =
        full.split("::").map(str::trim).filter(|segment| !segment.is_empty()).collect();
    let n = segments.len();
    if n < 2 || !is_pascal_case(segments[n - 1]) {
        return None;
    }
    let variant = segments[n - 1];
    let head = segments[n - 2];
    let head = if head == "Self" {
        enclosing_impl_type_name(node, text)?
    } else if is_pascal_case(head) {
        head.to_string()
    } else {
        return None;
    };
    Some(format!("{head}::{variant}"))
}

/// The base type name of the nearest enclosing `impl` block (`impl MlReq` / `impl Trait for MlReq`
/// / `impl Foo<T>` → `MlReq` / `Foo`), or `None` when `node` is not inside an impl. Used to resolve
/// a `Self`-headed dispatch key (#200 adversarial review). Strips generics and any module path.
fn enclosing_impl_type_name(node: Node<'_>, text: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        if ancestor.kind() == "impl_item" {
            let type_node = ancestor.child_by_field_name("type")?;
            let raw = type_node.utf8_text(text.as_bytes()).ok()?;
            let base = raw.split('<').next().unwrap_or(raw).trim();
            let last = base.rsplit("::").next().unwrap_or(base).trim();
            return (!last.is_empty()).then(|| last.to_string());
        }
        current = ancestor.parent();
    }
    None
}

/// Every TOP-LEVEL `Enum::Variant` key carried by a `match` arm pattern (#200), paired with the
/// scoped-path NODE it came from, deduped by key. Each OR-alternative contributes ONLY its outer
/// constructor — `Msg::Start | Msg::Resume` yields both, but `Msg::Wrapped(Inner::Start)` yields
/// only `Msg::Wrapped` (the nested payload `Inner::Start` is NOT a handled variant; including it
/// would let an unrelated `Inner::Start` constructor be reported as a dispatch caller). The node is
/// returned so each emitted handle fact anchors at its OWN variant span: the full-rebuild insert
/// dedups on `(from, to_name, kind, span)` (NOT evidence), so two facts for the same delegate call
/// would otherwise collapse to one. Empty for a non-enum pattern.
fn pattern_enum_variant_keys<'a>(pattern: Node<'a>, text: &str) -> Vec<(String, Node<'a>)> {
    // Unwrap `match_pattern` (which also wraps any `if`-guard) to the actual pattern / or_pattern.
    let inner = if pattern.kind() == "match_pattern" {
        let mut cursor = pattern.walk();
        pattern.named_children(&mut cursor).next().unwrap_or(pattern)
    } else {
        pattern
    };
    let mut alternatives = Vec::new();
    if inner.kind() == "or_pattern" {
        let mut cursor = inner.walk();
        alternatives.extend(inner.named_children(&mut cursor));
    } else {
        alternatives.push(inner);
    }
    let mut keys: Vec<(String, Node<'a>)> = Vec::new();
    for alternative in alternatives {
        if let Some((key, node)) = alternative_constructor_key(alternative, text)
            && !keys.iter().any(|(existing, _)| existing == &key)
        {
            keys.push((key, node));
        }
    }
    keys
}

/// The outer-constructor `Enum::Variant` key (+ its node) of ONE match-arm alternative — the type
/// of a tuple/struct variant pattern, or a bare unit-variant path. `None` for a non-enum
/// alternative (a wildcard, literal, or binding). Does NOT look inside the pattern, so a nested
/// payload variant is not mistaken for the handled variant (#200 review).
fn alternative_constructor_key<'a>(
    alternative: Node<'a>,
    text: &str,
) -> Option<(String, Node<'a>)> {
    let path = match alternative.kind() {
        "tuple_struct_pattern" | "struct_pattern" => alternative.child_by_field_name("type")?,
        "scoped_identifier" | "scoped_type_identifier" => alternative,
        _ => return None,
    };
    enum_variant_key(path, text).map(|key| (key, path))
}

/// The DELEGATE handler call(s) a `match` arm routes to (#200/#207/#208). Traces the calls whose
/// result becomes the arm's RESPONSE through a CLOSED set of value adapters (wrappers,
/// constructors, containers, field/index/cast projections) and SIMPLE `let` bindings.
///
/// Deliberately CONSERVATIVE rather than a full dataflow analysis (#208 review, ~7 rounds): a
/// precise answer needs Rust value-provenance + name-resolution + mutation tracking, which leaks at
/// every un-modeled construct. Instead, anything not in the closed model emits NOTHING (a missed
/// edge, never a false one):
/// - if the arm REBINDS a local anywhere (`x = ..` / `(x, _) = ..`), bail entirely — a mutated
///   binding's value can't be tracked path-sensitively, so no edge is synthesized for the arm;
/// - only a plain `let x = value` maps `x` to its producer; any destructuring `let` invalidates its
///   bindings (can't attribute which producer feeds which name);
/// - `if`/`match` contribute only branch RESULTS (never a condition/scrutinee); a single match-arm
///   payload binding inherits the scrutinee's producer; a side-effect statement / struct field
///   LABEL / nested handler ARGUMENT is never a handler.
fn collect_handler_calls<'a>(node: Node<'a>, text: &str, out: &mut Vec<Node<'a>>) {
    if arm_rebinds_local(node) {
        return;
    }
    result_handler_calls(node, text, &std::collections::HashMap::new(), out);
}

/// Whether the arm body MUTATES a local through any `=`/`op=` whose target subtree contains an
/// identifier — anywhere under `node`. That covers an identifier (`resp = ..`), a destructuring
/// tuple/array/struct/tuple-struct (`(resp,_) = ..`, `Out { resp } = ..`), a wrapped form
/// (`(resp) = ..`, `*p = ..`), AND a field/index store on a local (`r.id = ..`, `buf[0] = ..` —
/// which can stale a returned `r.id`/`buf[0]` projection, #208 review round 10). Any of these can
/// make a `let` binding's stored producer stale in ways that need real control-flow / scope
/// dataflow, so the whole arm BAILS rather than risk a stale/false handler edge. Conservative: a
/// field store on a non-returned place (`self.metric = ..`) also bails — accepted recall.
fn arm_rebinds_local(node: Node<'_>) -> bool {
    if matches!(node.kind(), "assignment_expression" | "compound_assignment_expr")
        && let Some(left) = node.child_by_field_name("left")
        && subtree_has_identifier(left)
    {
        return true;
    }
    // grow_stack: full-subtree recursion; grow rather than overflow on a hostile deep arm (#543).
    rag_rat_base::stack::grow_stack(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).any(arm_rebinds_local)
    })
}

/// Whether `node`'s subtree contains an `identifier` — used to decide if an assignment target can
/// rebind/stale a local (see [`arm_rebinds_local`]).
fn subtree_has_identifier(node: Node<'_>) -> bool {
    if node.kind() == "identifier" {
        return true;
    }
    rag_rat_base::stack::grow_stack(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).any(subtree_has_identifier)
    })
}

/// See [`collect_handler_calls`]. `scope` maps an in-scope binding name to the ALREADY-RESOLVED
/// handler calls its current value contributes (built in declaration order; a later `let` or
/// assignment of the same name replaces the entry). A binding read just contributes its stored
/// handlers — there is no binding→binding recursion, hence no depth/cycle guard.
fn result_handler_calls<'a>(
    node: Node<'a>,
    text: &str,
    scope: &std::collections::HashMap<String, Vec<Node<'a>>>,
    out: &mut Vec<Node<'a>>,
) {
    // grow_stack: recurses to full expression depth across many arms; wrap the whole recursion so a
    // hostile deeply-nested handler expression grows the stack rather than overflowing (#543).
    rag_rat_base::stack::grow_stack(|| result_handler_calls_impl(node, text, scope, out));
}

fn result_handler_calls_impl<'a>(
    node: Node<'a>,
    text: &str,
    scope: &std::collections::HashMap<String, Vec<Node<'a>>>,
    out: &mut Vec<Node<'a>>,
) {
    match node.kind() {
        "call_expression" => match classify_call(node, text) {
            // The handler — recorded, args NOT descended. (A method call on a scoped binding —
            // `v.into()`, `worker.run()` — is recorded too: a real handler-method like `worker.run`
            // resolves and surfaces, while a pure adapter like `v.into`/`v.clone` is a std trait
            // method that doesn't resolve to an in-corpus symbol, so it's dropped at resolution and
            // creates no edge — #208 review round 11.)
            CallRole::Delegate => out.push(node),
            CallRole::Skip => {},
            CallRole::Wrapper =>
            // A transparent wrapper / variant constructor returns its SINGLE wrapped value, so the
            // handler is whatever produced it. Descend only the lone payload arg — with MULTIPLE
            // args we can't tell which is the response (`Resp::X(handler(), metric())`). Comments
            // are NAMED children, so filter them before counting.
                if let Some(args) = node.child_by_field_name("arguments") {
                    let mut cursor = args.walk();
                    let arguments: Vec<Node<'a>> = args
                        .named_children(&mut cursor)
                        .filter(|arg| !matches!(arg.kind(), "line_comment" | "block_comment"))
                        .collect();
                    if let [only] = arguments.as_slice() {
                        result_handler_calls(*only, text, scope, out);
                    }
                },
        },
        "identifier" => {
            // A value-position read of a binding contributes its already-resolved handlers.
            if let Ok(name) = node.utf8_text(text.as_bytes())
                && let Some(handlers) = scope.get(name)
            {
                out.extend(handlers.iter().copied());
            }
        },
        "block" => {
            // Skip comments: tree-sitter exposes `line_comment`/`block_comment` as NAMED children,
            // so a trailing comment would otherwise masquerade as the block's tail expression.
            let mut cursor = node.walk();
            let children: Vec<Node<'a>> = node
                .named_children(&mut cursor)
                .filter(|child| !matches!(child.kind(), "line_comment" | "block_comment"))
                .collect();
            let Some((tail, statements)) = children.split_last() else {
                return;
            };
            // Resolve `let` bindings in declaration order. Only a PLAIN `let x = value` maps `x` to
            // its producer; a destructuring `let` (`let (a, b) = ..`, `let Out { x } = ..`) can't
            // attribute which producer feeds which binding, so it INVALIDATES its names. (The arm
            // has already been checked free of reassignment by `arm_rebinds_local`, so
            // a binding's mapped value is final.)
            let mut local = scope.clone();
            for statement in statements {
                if statement.kind() != "let_declaration" {
                    continue; // a bare side-effect statement is not a handler source
                }
                let Some(pattern) = statement.child_by_field_name("pattern") else {
                    continue;
                };
                if let Some(name) = simple_binding_name(pattern, text) {
                    let mut handlers = Vec::new();
                    if let Some(value) = statement.child_by_field_name("value") {
                        result_handler_calls(value, text, &local, &mut handlers);
                    }
                    local.insert(name, handlers);
                } else {
                    let mut names = Vec::new();
                    pattern_binding_names(pattern, text, &mut names);
                    for name in names {
                        local.remove(&name);
                    }
                }
            }
            let before = out.len();
            if tail.kind() != "let_declaration" {
                result_handler_calls(*tail, text, &local, out);
            }
            // EFFECT-ONLY fallback (#208, held feedback): a command/ack handler does its work in a
            // `?`-propagated side-effecting call and returns a FIXED value (`{ self.diarize(..)?;
            // Ok(Resp::Done) }`), so the value-trace above found nothing. Record the LAST `?`-stmt
            // whose payload is a DIRECT delegate call (`<call>.await?` / `<call>?`). Recording the
            // call DIRECTLY — not via scope-traced `result_handler_calls` — avoids resolving a
            // `let`-bound `?` (`task?`) against the final block scope, which a later shadowing
            // `let` could redirect to the wrong producer (#208 review round 11). The
            // `?` gate + direct-call requirement excludes fire-and-forget side effects
            // (`metrics::inc();`).
            if out.len() == before {
                for statement in children.iter().rev() {
                    if statement.kind() == "expression_statement"
                        && let Some(try_expr) = statement.named_child(0)
                        && try_expr.kind() == "try_expression"
                        && let Some(call) = unwrap_to_call(try_expr)
                        && matches!(classify_call(call, text), CallRole::Delegate)
                    {
                        out.push(call);
                        break;
                    }
                }
            }
        },
        "if_expression" => {
            // Branch RESULTS only — the `condition` field (a guard/scrutinee) is never a handler.
            // EXCEPT an `if let Pat = value` condition: its payload bindings are projections of
            // `value`, so the CONSEQUENCE inherits the value's handlers (like a match arm, #208).
            let consequence_scope = match node.child_by_field_name("condition") {
                Some(condition) if condition.kind() == "let_condition" => {
                    let mut scrutinee_handlers = Vec::new();
                    if let Some(value) = condition.child_by_field_name("value") {
                        result_handler_calls(value, text, scope, &mut scrutinee_handlers);
                    }
                    let mut inner = scope.clone();
                    if let Some(pattern) = condition.child_by_field_name("pattern") {
                        let mut bound = Vec::new();
                        pattern_binding_names(pattern, text, &mut bound);
                        bound.sort();
                        bound.dedup();
                        match bound.as_slice() {
                            [name] => {
                                inner.insert(name.clone(), scrutinee_handlers);
                            },
                            _ =>
                                for name in bound {
                                    inner.remove(&name);
                                },
                        }
                    }
                    inner
                },
                _ => scope.clone(),
            };
            if let Some(consequence) = node.child_by_field_name("consequence") {
                result_handler_calls(consequence, text, &consequence_scope, out);
            }
            if let Some(alternative) = node.child_by_field_name("alternative") {
                result_handler_calls(alternative, text, scope, out);
            }
        },
        "match_expression" => {
            // Each arm's RESULT only — never the scrutinee directly. An arm's pattern bindings are
            // projections of the SCRUTINEE, so they inherit the scrutinee's resolved handlers (a
            // returned payload `match load()? { Some(v) => Ok(Wrap(v)) }` traces `v` back to
            // `load`). This also overrides any outer `let` of the same name, so a
            // payload `Some(value)` never resolves to an unrelated outer `let value`
            // (#208 review).
            let scrutinee_handlers = match node.child_by_field_name("value") {
                Some(scrutinee) => {
                    let mut handlers = Vec::new();
                    result_handler_calls(scrutinee, text, scope, &mut handlers);
                    handlers
                },
                None => Vec::new(),
            };
            if let Some(body) = node.child_by_field_name("body") {
                let mut arm_cursor = body.walk();
                for arm in body.named_children(&mut arm_cursor) {
                    if arm.kind() == "match_arm"
                        && let Some(value) = arm.child_by_field_name("value")
                    {
                        let mut arm_scope = scope.clone();
                        if let Some(pattern) = arm.child_by_field_name("pattern") {
                            let mut bound = Vec::new();
                            pattern_binding_names(pattern, text, &mut bound);
                            // An or-pattern repeats the same binding per alternative
                            // (`Ok(v) | Err(v)`); dedup so it counts as the single projected
                            // payload.
                            bound.sort();
                            bound.dedup();
                            match bound.as_slice() {
                                // A single payload binding IS the projected scrutinee value —
                                // inherit its handlers. Multiple
                                // bindings can't each be the whole scrutinee
                                // (`(resp, _span)` would credit every binding with every producer),
                                // so mask them (#208 review).
                                [name] => {
                                    arm_scope.insert(name.clone(), scrutinee_handlers.clone());
                                },
                                _ =>
                                    for name in bound {
                                        arm_scope.remove(&name);
                                    },
                            }
                        }
                        result_handler_calls(value, text, &arm_scope, out);
                    }
                }
            }
        },
        "struct_expression" => {
            // Trace field VALUES and shorthand reads (`Resp { vector }`), never field LABELS
            // (`Resp { status: .. }` must not match a `status` local). ONLY when there is exactly
            // one field value — a multi-field struct can't attribute which field is the
            // returned response (`Resp { ok: handler(), metric: m() }`), so emit
            // nothing (no false edge, #208 review).
            let mut cursor = node.walk();
            let Some(fields) =
                node.named_children(&mut cursor).find(|c| c.kind() == "field_initializer_list")
            else {
                return;
            };
            let mut field_cursor = fields.walk();
            let values: Vec<Node<'a>> = fields
                .named_children(&mut field_cursor)
                .filter(|f| {
                    matches!(
                        f.kind(),
                        "field_initializer"
                            | "shorthand_field_initializer"
                            | "base_field_initializer"
                    )
                })
                .collect();
            if let [field] = values.as_slice() {
                match field.kind() {
                    "field_initializer" =>
                        if let Some(value) = field.child_by_field_name("value") {
                            result_handler_calls(value, text, scope, out);
                        },
                    _ => {
                        let mut inner_cursor = field.walk();
                        for inner in field.named_children(&mut inner_cursor) {
                            result_handler_calls(inner, text, scope, out);
                        }
                    },
                }
            }
        },
        "index_expression" => {
            // A projection `r[i]` of a result — trace ONLY the indexed receiver (`r`), never the
            // index expression (`choose_index()` selects, it doesn't produce the response).
            let mut cursor = node.walk();
            if let Some(receiver) = node.named_children(&mut cursor).next() {
                result_handler_calls(receiver, text, scope, out);
            }
        },
        "tuple_expression" | "array_expression" => {
            // A SINGLE-element container is a transparent wrapper (`(x,)`, `[x]`); a multi-element
            // one can't attribute which element is the returned response (`(handler(), metric())`),
            // so emit nothing rather than credit a discarded sibling (#208 review).
            let mut cursor = node.walk();
            let elements: Vec<Node<'a>> = node.named_children(&mut cursor).collect();
            if let [only] = elements.as_slice() {
                result_handler_calls(*only, text, scope, out);
            }
        },
        // Single-value projections/wrappers of a result (`&x`, `r.id`, `r as u32`, `*r`; the
        // `field_identifier`/type is not an `identifier`, so it contributes nothing), plus
        // control-flow and postfix wrappers — trace their children.
        "reference_expression"
        | "field_expression"
        | "type_cast_expression"
        | "unary_expression"
        | "else_clause"
        | "expression_statement"
        | "return_expression"
        | "break_expression"
        | "parenthesized_expression"
        | "unsafe_block"
        | "try_expression"
        | "await_expression" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                result_handler_calls(child, text, scope, out);
            }
        },
        _ => {},
    }
}

/// The single identifier a PLAIN `let` pattern binds (`let x` / `let mut x`), or `None` for a
/// destructuring pattern (which is invalidated, not mapped — see [`collect_handler_calls`]).
fn simple_binding_name(pattern: Node<'_>, text: &str) -> Option<String> {
    let identifier = match pattern.kind() {
        "identifier" => pattern,
        // `let mut x`: the `mut_pattern`'s first named child is the `mutable_specifier`, so find
        // the identifier child rather than taking `named_child(0)` (#208 review round 10).
        "mut_pattern" => {
            let mut cursor = pattern.walk();
            pattern.named_children(&mut cursor).find(|node| node.kind() == "identifier")?
        },
        _ => return None,
    };
    identifier.utf8_text(text.as_bytes()).ok().map(str::to_string)
}

/// Unwrap `?` / `.await` / parentheses to the underlying `call_expression`, for the effect-only
/// fallback (`<call>.await?` / `<call>?`). `None` if the payload isn't a direct call (e.g. `task?`
/// awaits a bound value — not a call — and must NOT be resolved against the block scope, #208
/// review round 11).
fn unwrap_to_call(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "call_expression" => Some(node),
        "try_expression" | "await_expression" | "parenthesized_expression" =>
            node.named_child(0).and_then(unwrap_to_call),
        _ => None,
    }
}

/// The local variable names a `let` / `match`-arm pattern BINDS — snake_case `identifier` leaves
/// and struct-pattern shorthands — excluding field labels (`field_identifier`), type/variant paths
/// (PascalCase identifiers, scoped paths), and literals. Used to map destructuring bindings to
/// their producer and to mask match-arm payloads so they don't resolve to an outer `let` (#208
/// review).
fn pattern_binding_names(pattern: Node<'_>, text: &str, out: &mut Vec<String>) {
    // grow_stack: recurses to full pattern depth across several arms; wrap the whole recursion so a
    // hostile deeply-nested destructuring pattern grows the stack rather than overflowing (#543).
    rag_rat_base::stack::grow_stack(|| pattern_binding_names_impl(pattern, text, out));
}

fn pattern_binding_names_impl(pattern: Node<'_>, text: &str, out: &mut Vec<String>) {
    match pattern.kind() {
        "shorthand_field_identifier" =>
            if let Ok(name) = pattern.utf8_text(text.as_bytes()) {
                out.push(name.to_string());
            },
        "identifier" =>
        // A binding is snake_case / `_`-led; a PascalCase identifier pattern is a unit variant or
        // const, which binds nothing.
        {
            if let Ok(name) = pattern.utf8_text(text.as_bytes())
                && name.chars().next().is_some_and(|c| c == '_' || c.is_lowercase())
            {
                out.push(name.to_string());
            }
        },
        "match_pattern" => {
            // A guarded arm's `pattern` is `match_pattern` = the pattern + an `if <guard>` whose
            // `condition` holds READS, not bindings — recurse the pattern, skip the guard (#208).
            let guard = pattern.child_by_field_name("condition");
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                if Some(child) != guard {
                    pattern_binding_names(child, text, out);
                }
            }
        },
        "tuple_struct_pattern" | "struct_pattern" => {
            // Skip the `type` path qualifier (`status::Ready(v)` / `Out { .. }`) — a lowercase
            // module segment there is NOT a binding and must not mask an outer `let` of
            // that name (#208).
            let type_field = pattern.child_by_field_name("type");
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                if Some(child) != type_field {
                    pattern_binding_names(child, text, out);
                }
            }
        },
        // A bare `scoped_identifier` pattern is a UNIT-VARIANT / const path (`status::Ready`,
        // `Mod::CONST`); its segments are a qualifier + variant, never bindings (#208 review).
        "scoped_identifier" => {},
        _ => {
            let mut cursor = pattern.walk();
            for child in pattern.named_children(&mut cursor) {
                pattern_binding_names(child, text, out);
            }
        },
    }
}

/// How `result_handler_calls` treats a `call_expression` (#200/#208 — one classifier so the
/// delegate/wrapper decision can't disagree with itself, as it did across review rounds):
/// - `Delegate`: RECORD it as the handler (a free fn `run`, a method `self.embed`, a module-pathed
///   fn `crate::ml::embed::embed_text`).
/// - `Wrapper`: a TRANSPARENT wrapper / variant constructor whose single argument IS the response —
///   `Ok`/`Some` and ANY PascalCase-tail ctor (`MlResp::Embedded`, `dto::Wrapped`, bare `Wrapped`).
///   Trace its lone payload argument.
/// - `Skip`: emit nothing — `Err`/`None` (error/absence payload), a snake-tail `Type::assoc`
///   constructor (`Vec::with_capacity`, `Resp::empty` — its arg configures, isn't the response), or
///   a UFCS associated call (`<Resp as Default>::default()`).
///
/// Classification is by the path TAIL (constructor names are PascalCase; fns/methods are
/// snake_case), which is receiver-agnostic — so `dto::Wrapped` and `Resp::Embedded` are both
/// wrappers. Accepted recall: a bare PascalCase FFI fn (`CreateFileW`) reads as a wrapper (traced
/// through, not recorded) — there is no extraction-time signal vs a tuple-struct ctor, and
/// recording it risks crediting a constructor as a handler (a false edge), which the contract
/// forbids.
enum CallRole {
    Delegate,
    Wrapper,
    Skip,
}

fn classify_call(call: Node<'_>, text: &str) -> CallRole {
    let Some(function) = call.child_by_field_name("function") else {
        return CallRole::Delegate;
    };
    let Ok(raw) = function.utf8_text(text.as_bytes()) else {
        return CallRole::Delegate;
    };
    let raw = raw.trim();
    // A LEADING `<...>` is a UFCS qualifier (`<Resp as Default>::default()`), an
    // associated/constructor call — never a handler, and its arg (if any) isn't the response.
    if raw.starts_with('<') {
        return CallRole::Skip;
    }
    let stripped = strip_generics(raw);
    let segments: Vec<&str> =
        stripped.split("::").map(str::trim).filter(|segment| !segment.is_empty()).collect();
    let Some(tail) = segments.last() else {
        return CallRole::Delegate;
    };
    if matches!(*tail, "Err" | "None") {
        return CallRole::Skip;
    }
    if matches!(*tail, "Ok" | "Some") || is_pascal_case(tail) {
        // A PascalCase tail is a variant / tuple-struct constructor (`Resp::Embedded`,
        // `dto::Wrapped`, bare `Wrapped`) — a transparent wrapper of its payload.
        return CallRole::Wrapper;
    }
    // snake_case tail: a `Type::assoc(..)` (PascalCase receiver) is a config-taking constructor;
    // a bare fn / method / module-pathed fn is the handler.
    match segments.as_slice() {
        [.., receiver, _last] if is_pascal_case(receiver) => CallRole::Skip,
        _ => CallRole::Delegate,
    }
}

/// Whether a `scoped_identifier` sits in an expression VALUE position where a unit enum-variant is
/// being produced (#200): a call argument, a `let` initializer, an assignment RHS, or a
/// `return`/`break` value. Excludes type/pattern/use/path positions (a `use` leaf, a `let Pat = …`
/// pattern, a `T::Assoc` type). Field-checked where the node could be on either side (let pattern
/// vs value, assignment left vs right).
pub(super) fn scoped_identifier_in_value_position(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "arguments" | "return_expression" | "break_expression" => true,
        "let_declaration" => parent.child_by_field_name("value") == Some(node),
        "assignment_expression" => parent.child_by_field_name("right") == Some(node),
        _ => false,
    }
}

/// Build a dispatch FACT edge candidate (#200) whose `from_symbol` is the enclosing function. For a
/// handle fact `evidence` carries the `Enum::Variant` key (safe: `resolve_symbol` never reads
/// `evidence` for a non-import kind — only `synthesize_dispatch_edges` does), and `context` carries
/// the same `target_qualified_name` / `receiver_hint` a normal call would, so the handler resolves
/// identically to a `calls_name` (e.g. `self.handle(x)` / `Self::handle(x)` bind to the right
/// method). `to_name` is the variant key (construct) or the handler name (handle). No callee range,
/// so the SCIP oracle skips these rows.
pub(super) fn dispatch_fact(
    symbols: &[IndexedSymbol],
    node: Node<'_>,
    to_name: String,
    edge_kind: EdgeKind,
    context: EdgeContext,
    evidence: Option<String>,
) -> EdgeCandidate {
    let source = containing_symbol(symbols, node.start_byte());
    EdgeCandidate {
        from_symbol_id: source.map(|symbol| symbol.id),
        from_name: source.map(|symbol| symbol.qualified_name.clone()),
        to_name,
        target_qualified_name: context.target_qualified_name,
        evidence,
        receiver_hint: context.receiver_hint,
        source_span: span_for_node(node),
        callee_span: None,
        import_scope: None,
        edge_kind,
        confidence: EdgeConfidence::NameOnly,
    }
}

/// Emit `DispatchHandle` facts for a `match_arm` whose pattern names one or more `Enum::Variant`s
/// (#200): `evidence` = the variant key, `to_name` = the arm's DELEGATING call. Conservative on
/// both ends — fires only when the pattern carries a 2-segment PascalCase enum path (an integer/`_`
/// arm yields nothing), and binds only the tail/delegate call(s) (`collect_tail_calls`), so
/// side-effect statements (`metrics::inc(); handle(x)`), nested argument calls
/// (`handle(validate(x))`), and guard/scrutinee calls (`if ready() { a() }` → `a`, never `ready`)
/// are not spurious targets. An OR-pattern arm (`A | B => handle()`) emits a fact for EACH variant.
pub(super) fn rust_dispatch_handle_facts(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    out: &mut Vec<EdgeCandidate>,
) {
    let Some(pattern) = node.child_by_field_name("pattern") else {
        return;
    };
    let keys = pattern_enum_variant_keys(pattern, text);
    if keys.is_empty() {
        return;
    }
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    let mut handler_calls = Vec::new();
    collect_handler_calls(value, text, &mut handler_calls);
    for call in &handler_calls {
        let Some(handler) = call_target_name(*call, text) else {
            continue;
        };
        let context = EdgeContext {
            target_qualified_name: target_qualified_name(*call, text),
            receiver_hint: scoped_receiver_name(*call, text),
        };
        for (key, variant_node) in &keys {
            // Anchor each fact at its OWN variant-pattern node (not the shared delegate call), so
            // distinct variants of an OR-pattern arm survive the span-keyed full-rebuild dedup. The
            // handler name + call context still come from the delegate call; from_symbol resolves
            // to the same dispatcher fn either way.
            out.push(dispatch_fact(
                symbols,
                *variant_node,
                handler.clone(),
                EdgeKind::DispatchHandle,
                context.clone(),
                Some(key.clone()),
            ));
        }
    }
}
