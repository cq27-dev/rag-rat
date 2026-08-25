//! Rust dispatch-edge synthesis (#200/#207/#208) — emits the actor-channel
//! `dispatch_construct` / `dispatch_handle` graph facts. `rust_edges`
//! calls the four `pub(super)` entry points (`enum_variant_key`, `dispatch_fact`,
//! `scoped_identifier_in_value_position`, `rust_dispatch_handle_facts`); everything else is the
//! CLOSED conservative handler-call recognizer (`result_handler_calls` & friends) whose
//! false-edge-is-a-bug contract is documented in the repo memories bound here and on the parent.
use tree_sitter::Node;
use unicode_ident::is_xid_continue;

use crate::index::edges::extract::EdgeEmitter;
use crate::index::edges::*;

/// PascalCase test for the enum/variant convention (#200): the segment's LEADING IDENTIFIER starts
/// with an uppercase char AND carries at least one lowercase — so `MlReq`/`Upsert` qualify but
/// `new`, a SCREAMING `CONST`, and a bare `T` do not.
/// The leading scan follows Rust's `XID_Continue` rule, so decomposed identifiers keep their
/// combining marks instead of being truncated at the first non-alphanumeric code point. `_`
/// needs no special case: it is connector punctuation (Pc), already part of `XID_Continue`.
///
/// Only that leading identifier is weighed because a segment can carry a method chain glued onto
/// its head (`TOOL_NAMES.iter().map`, `NOW.elapsed`). The appended method names supply the very
/// lowercase the test looks for, so reading the whole segment takes a constant for a variant
/// constructor and bills the call as a transparent wrapper it is not (#1124).
fn is_pascal_case(name: &str) -> bool {
    let mut head = name.chars().take_while(|&character| is_xid_continue(character));
    head.next().is_some_and(char::is_uppercase) && head.any(char::is_lowercase)
}

/// A whole Rust identifier is SCREAMING_SNAKE_CASE when its non-underscore portion starts with an
/// uppercase character, contains no lowercase characters, and every character follows the same
/// `XID_Continue` boundary used by [`is_pascal_case`]. Leading underscores are permitted: names
/// such as `_DEFAULT` are conventional associated constants too.
fn is_screaming_const_identifier(name: &str) -> bool {
    let name = name.strip_prefix("r#").unwrap_or(name).trim_start_matches('_');
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_uppercase()
        && is_xid_continue(first)
        && characters.all(|character| is_xid_continue(character) && !character.is_lowercase())
}

/// Peel the wrappers that sit between a method-chain link and the value it reads WITHOUT changing
/// that value's identity: a turbofish (`generic_function`), a `?`, an `.await`, and redundant
/// parentheses. A chain's role must not depend on which of these the source happens to spell —
/// `POOL.get()?.execute(sql)`, `POOL.get().await.execute(sql)` and `POOL.get().execute(sql)` all
/// glue `execute` onto the same produced value, so a guard that recognizes only the bare form
/// hands the other two to the delegate fall-through and records the adapter (#1124).
fn unwrap_transparent(node: Node<'_>) -> Node<'_> {
    let mut current = unwrap_generic_function(node);
    while matches!(
        current.kind(),
        "try_expression" | "await_expression" | "parenthesized_expression"
    ) {
        let Some(inner) = current.named_child(0) else {
            return current;
        };
        current = unwrap_generic_function(inner);
    }
    current
}

/// Walk the Rust AST's nested `field_expression` nodes from the call's callee inward, following an
/// intermediate `call_expression` only when its own callee is another field expression — a
/// constructor/function-call root ends the walk there, because that call's RESULT is the value the
/// rest of the chain adapts. `None` when the callee is not a method chain at all (a bare name, a
/// path, a `Type::assoc` call). Insensitive to trivia and to every [`unwrap_transparent`] wrapper,
/// so comments, generic arguments, `?` and `.await` around the member-access dot cannot move a
/// verdict.
fn method_chain_root(function: Node<'_>) -> Option<Node<'_>> {
    let mut current = unwrap_transparent(function);
    if current.kind() != "field_expression" {
        return None;
    }
    while current.kind() == "field_expression" {
        let value = unwrap_transparent(current.child_by_field_name("value")?);
        if value.kind() != "call_expression" {
            current = value;
            continue;
        }
        let inner_function = unwrap_transparent(value.child_by_field_name("function")?);
        current = if inner_function.kind() == "field_expression" { inner_function } else { value };
    }
    Some(current)
}

/// Whether the callee is a method chain whose ROOT receiver is a SCREAMING_SNAKE constant —
/// `LIMIT.min(cap).max(..)`, `crate::config::BASE.to_string()`, `Handler::DEFAULT.run(..)`,
/// `<Handler as Runner>::DEFAULT.run(..)`. Such a chain adapts the constant's VALUE, so it is never
/// the handler.
///
/// The verdict is read off the root and nothing else. Whether the qualifier names the OWNING TYPE
/// or the MODULE the constant lives in cannot be told apart at extraction time except by the case
/// of a path segment, and that guess is wrong for an uppercase module, a type alias, and a
/// bracketed UFCS qualifier — so making the two spellings disagree only moves the error around.
/// It also would not help: `Resp::DEFAULT.with_body(handle(cap))` and
/// `Handler::DEFAULT.run(normalize(input))` are the same expression modulo identifier spelling, so
/// nothing in the AST distinguishes a builder from a dispatch. Under the false-edge-is-a-bug
/// contract an undecidable case emits nothing (#1124).
fn chain_root_is_constant(function: Node<'_>, text: &str) -> bool {
    let Some(root) = method_chain_root(function) else {
        return false;
    };
    let name = match root.kind() {
        "identifier" => root,
        "scoped_identifier" => match root.child_by_field_name("name") {
            Some(name) => name,
            None => return false,
        },
        _ => return false,
    };
    name.utf8_text(text.as_bytes()).is_ok_and(is_screaming_const_identifier)
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
/// - `Delegate`: RECORD it as the handler (a free fn `run`, a method `self.embed`, or a
///   module-pathed fn `crate::ml::embed::embed_text`).
/// - `Wrapper`: a TRANSPARENT wrapper / variant constructor whose single argument IS the response —
///   `Ok`/`Some`, or ANY PascalCase-tail ctor (`MlResp::Embedded`, `dto::Wrapped`, bare `Wrapped`).
///   Trace its lone payload argument.
/// - `Skip`: emit nothing — `Err`/`None` (error/absence payload), a snake-tail `Type::assoc`
///   constructor (`Vec::with_capacity`, `Resp::empty` — its arg configures, isn't the response), a
///   UFCS associated call (`<Resp as Default>::default()`), or an adapter tail glued onto another
///   call's result or onto a SCREAMING constant — `LIMIT.min(cap).max(..)`,
///   `crate::config::BASE.to_string()`, `Handler::DEFAULT.with_body(..)`, and every other spelling
///   of those roots — since it adapts a value, and recording the trailing method would bind it to
///   an unrelated same-named symbol (#1124).
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
    let stripped = degeneric_path(raw);
    // TOP-LEVEL segments only. A `::` inside a `(…)`/`[…]` group is argument text, which
    // `degeneric_path` keeps, so splitting on every `::` would read a longer path than the source
    // wrote and take the tail from the arguments: `Transaction::new_unchecked(&conn,
    // TransactionBehavior::Immediate).unwrap` is a `Type::assoc(..)` constructor, not a call whose
    // tail is the PascalCase `Immediate).unwrap`.
    let segments: Vec<&str> = scope_grammar::segments(&stripped)
        .into_iter()
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();
    let Some(tail) = segments.last() else {
        return CallRole::Delegate;
    };
    // A method chain rooted at a SCREAMING constant is decided by that ROOT, in one place, so no
    // spelling of the path to the constant can route otherwise identical expressions to different
    // verdicts. It runs before the segment fallbacks below because those read the callee's TEXT,
    // where a chained method glued onto the constant lands in the tail segment
    // (`crate::config::BASE.to_string` → tail `BASE.to_string`, receiver `config`) and reaches the
    // bare-callee delegate fall-through (#1124).
    if chain_root_is_constant(function, text) {
        return CallRole::Skip;
    }
    // A LEADING `<...>` is a UFCS qualifier (`<Resp as Default>::default()`), an
    // associated/constructor call — never a handler, and its arg (if any) isn't the response.
    if raw.starts_with('<') {
        return CallRole::Skip;
    }
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
        // The fall-through is reserved for a BARE callee. A method glued onto ANOTHER CALL'S
        // RESULT is an adapter tail (`list.len().saturating_sub(..)`,
        // `make_worker().run(input)`): it adapts a value into another value — never the handler,
        // and not a wrapper either, since its argument is adapter input rather than the response.
        // Recording the trailing method would bind it, via bare-name fallback, to an unrelated
        // same-named repository symbol — a false persisted edge (#1124 maintainer feedback). A
        // CONSTANT-rooted chain is already decided above, whatever its root's spelling.
        _ if is_chained_adapter_tail(function) => CallRole::Skip,
        _ => CallRole::Delegate,
    }
}

/// Whether the call's callee is a method glued onto ANOTHER CALL'S RESULT — AST-checked, never
/// textual. That receiver holds an already-produced value, so the trailing method adapts it rather
/// than handling anything.
///
/// A method on a plain binding/`self`/field receiver (`worker.run`, `self.embed`) is a bare callee:
/// that receiver holds the handler. A chain rooted at a SCREAMING constant never reaches here —
/// [`chain_root_is_constant`] decides it earlier, on the root, so that `LIMIT.min(cap).max(..)` and
/// `crate::config::LIMIT.min(cap).max(..)` cannot be answered differently. The receiver is read
/// through [`unwrap_transparent`], so `make_worker()?.run(x)` and `make_worker().run(x)` agree too.
fn is_chained_adapter_tail(function: Node<'_>) -> bool {
    let function = unwrap_transparent(function);
    if function.kind() != "field_expression" {
        return false;
    }
    let Some(value) = function.child_by_field_name("value") else {
        return false;
    };
    unwrap_transparent(value).kind() == "call_expression"
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
    locator: &SymbolLocator<'_>,
    node: Node<'_>,
    to_name: String,
    edge_kind: EdgeKind,
    context: EdgeContext,
    evidence: Option<String>,
) -> EdgeCandidate {
    let source = locator.find(node.start_byte());
    EdgeCandidate {
        from_symbol_id: source.map(|symbol| symbol.id),
        from_name: source.map(|symbol| symbol.qualified_name.clone()),
        to_name,
        target_qualified_name: context.target_qualified_name,
        evidence,
        receiver_hint: context.receiver_hint,
        receiver_type_hint: context.receiver_type_hint,
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
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
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
            ..Default::default()
        };
        for (key, variant_node) in &keys {
            // Anchor each fact at its OWN variant-pattern node (not the shared delegate call), so
            // distinct variants of an OR-pattern arm survive the span-keyed full-rebuild dedup. The
            // handler name + call context still come from the delegate call; from_symbol resolves
            // to the same dispatcher fn either way.
            out.push(dispatch_fact(
                locator,
                *variant_node,
                handler.clone(),
                EdgeKind::DispatchHandle,
                context.clone(),
                Some(key.clone()),
            ));
        }
    }
}

#[cfg(test)]
mod classify_call_tests {
    use super::*;

    /// Breadth-first, so the shallowest match is the outermost call, and iterative so it is not a
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

    fn role_of(expression: &str) -> CallRole {
        let source = format!("fn probe() {{ {expression}; }}");
        let parsed = crate::index::parser::parse_file(
            std::path::Path::new("probe.rs"),
            rag_rat_base::language::Language::Rust,
            &source,
        )
        .expect("probe parses");
        classify_call(outermost_call(parsed.root()).expect("a call expression"), &source)
    }

    /// A `::` inside the ARGUMENT list is not a path separator, so the classified TAIL is the real
    /// callee rather than the last argument fragment. `Transaction::new_unchecked(..)` is a
    /// `Type::assoc(..)` constructor (Skip); reading its tail as the PascalCase `Immediate).unwrap`
    /// would bill it as a transparent variant wrapper instead.
    #[test]
    fn a_nested_separator_does_not_move_the_callee_into_the_arguments() {
        assert!(matches!(
            role_of("Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap()"),
            CallRole::Skip
        ));
        // The handler is `handle`; `Resp::Embedded` is its argument, not its callee.
        assert!(matches!(role_of("handle(Resp::Embedded(payload))"), CallRole::Delegate));
    }

    /// The ordinary shapes keep their roles.
    #[test]
    fn top_level_paths_keep_their_roles() {
        assert!(matches!(role_of("Resp::Embedded(body)"), CallRole::Wrapper));
        assert!(matches!(role_of("Ok(body)"), CallRole::Wrapper));
        assert!(matches!(role_of("Err(problem)"), CallRole::Skip));
        assert!(matches!(role_of("Config::from_env(path)"), CallRole::Skip));
        assert!(matches!(role_of("render_page(body)"), CallRole::Delegate));
        assert!(matches!(role_of("<Resp as Default>::default()"), CallRole::Skip));
    }

    /// A `SCREAMING_CONST` head carrying a method chain is not a transparent variant constructor
    /// (#1124). The chain's method names supply the lowercase the case test looks for, so reading
    /// the whole segment would bill `TOOL_NAMES.iter().map(..)` as a wrapper and trace through the
    /// constant instead of classifying the call on its own shape.
    #[test]
    fn a_screaming_const_with_a_method_chain_is_not_a_wrapper() {
        // A MODULE path names where the constant lives, not an owner that could receive the
        // dispatch, so the chain adapts the constant's value exactly as the bare spelling does.
        assert!(matches!(
            role_of("crate::tools::TOOL_NAMES.iter().map(|name| describe(name))"),
            CallRole::Skip
        ));
        // A BARE constant receiver names no owner either — the trailing method is an adapter over
        // the constant's value rather than a handler, so neither a wrapper nor a delegate.
        assert!(matches!(role_of("NOW.elapsed()"), CallRole::Skip));
    }

    /// A method glued onto ANOTHER CALL'S RESULT (`LIMIT.min(cap).max(..)`,
    /// `list.len().saturating_sub(..)`) or onto a BARE SCREAMING constant (`BASE.to_string()`) is
    /// an adapter tail: it adapts a value, it is never the handler, and it is not a transparent
    /// wrapper either — its argument is adapter input, not the response. Only a BARE callee (a
    /// free function, a path, or a method on a plain binding/`self` receiver) may fall through to
    /// `Delegate`; otherwise the trailing method name is recorded as the handler and bare-name
    /// fallback binds it to an unrelated same-named repository symbol — a false persisted edge
    /// (#1124 maintainer feedback).
    #[test]
    fn an_adapter_tail_is_neither_a_delegate_nor_a_wrapper() {
        assert!(matches!(role_of("LIMIT.min(cap).max(1)"), CallRole::Skip));
        assert!(matches!(role_of("BACKENDS.len().saturating_sub(1)"), CallRole::Skip));
        assert!(matches!(role_of("items(count).len().max(1)"), CallRole::Skip));
        assert!(matches!(role_of("make_worker().run(input)"), CallRole::Skip));
        // A single method on a bare SCREAMING constant is the same adapter shape without the
        // second link — `BASE.to_string()`, `SWEEP_FALLBACK_CONCURRENCY.min(cap)`.
        assert!(matches!(role_of("BASE.to_string()"), CallRole::Skip));
        assert!(matches!(role_of("LIMIT.min(cap)"), CallRole::Skip));
        assert!(matches!(role_of("_BASE.to_string()"), CallRole::Skip));
        // Bare callees keep their delegation: a free function, and a method on a plain
        // binding/`self`/field receiver (`worker.run` IS the handler — #208 review round 11).
        assert!(matches!(role_of("run(input)"), CallRole::Delegate));
        assert!(matches!(role_of("worker.run(input)"), CallRole::Delegate));
        assert!(matches!(role_of("self.embed(input)"), CallRole::Delegate));
        assert!(matches!(role_of("self.worker.run(input)"), CallRole::Delegate));
    }

    /// The role of a chained adapter tail is fixed by the chain's ROOT, never by how that root is
    /// SPELLED (#1124 maintainer feedback). A constant reached through a module path is the same
    /// receiver as the bare identifier — `crate::config::LIMIT`, `self::LIMIT`, `super::LIMIT`, an
    /// aliased `cfg::LIMIT` and an arbitrarily long `crate::a::b::c::LIMIT` all adapt a value, so
    /// every one of them skips. Only a qualifier that names the OWNING TYPE makes the chained
    /// method the dispatch, and that stays true however long the path to the type is. The verdicts
    /// are collected before asserting, so one failure names EVERY misclassified spelling rather
    /// than stopping at the first.
    #[test]
    fn a_chained_adapter_tail_skips_however_its_root_is_spelled() {
        let skip_forms = [
            // The maintainer's reproduction: bare and crate-qualified spellings of one expression.
            "LIMIT.min(cap).max(1)",
            "crate::config::LIMIT.min(cap).max(1)",
            // `self::`/`super::`-relative and aliased module paths are the same receiver again.
            "self::LIMIT.min(cap).max(1)",
            "super::LIMIT.min(cap).max(1)",
            "super::config::LIMIT.min(cap).max(1)",
            "cfg::LIMIT.min(cap).max(1)",
            "crate::a::b::c::LIMIT.min(cap).max(1)",
            // A single method glued onto the constant is the same shape without the second link.
            "BASE.to_string()",
            "crate::config::BASE.to_string()",
            "self::BASE.to_string()",
            "super::consolidate::BASE.to_string()",
            "crate::a::b::c::BASE.to_string()",
            "crate::config::_BASE.to_string()",
            // The iterator-adapter chain the classifier was first written for.
            "TOOL_NAMES.iter().map(|name| describe(name))",
            "crate::tools::TOOL_NAMES.iter().map(|name| describe(name))",
            // A qualifier that names the OWNING TYPE is not distinguishable from a module path
            // except by the case of a segment, and the case guess is wrong for an uppercase
            // module, a type alias, and a bracketed UFCS qualifier. Both spellings skip.
            "Handler::DEFAULT.run(input)",
            "crate::ml::Handler::DEFAULT.run(input)",
            "crate::a::b::Handler::DEFAULT.build().run(input)",
            "<Handler as Runner>::DEFAULT.run(input)",
            "<u32 as Bounded>::MAX.min(cap)",
            "u32::MAX.min(cap)",
            // A `?`, an `.await` or a paren between the links wraps a value without changing its
            // identity — the same adapter, so the same verdict.
            "LIMIT.min(cap)?.max(1)",
            "crate::config::LIMIT.min(cap)?.max(1)",
            "POOL.get(url).await.text()",
            "(LIMIT.min(cap)).max(1)",
            "(LIMIT).wrapping_mul(cap)",
            "make_worker()?.run(input)",
            "make_worker().await.run(input)",
            "(make_worker()).run(input)",
        ];
        let delegate_forms = [
            // A bare callee keeps delegating whatever the receiver is called.
            "worker.run(input)",
            "self.embed(input)",
            "self.worker.run(input)",
        ];
        let mut misclassified = Vec::new();
        misclassified
            .extend(skip_forms.into_iter().filter(|form| !matches!(role_of(form), CallRole::Skip)));
        misclassified.extend(
            delegate_forms.into_iter().filter(|form| !matches!(role_of(form), CallRole::Delegate)),
        );
        assert!(misclassified.is_empty(), "misclassified root spellings: {misclassified:?}");
    }

    /// A method chain glued onto an ASSOCIATED CONSTANT records no handler (#1124), and no
    /// spelling around the member-access dot changes that. Whether the qualifier names the owning
    /// TYPE (`Handler::DEFAULT`) or the MODULE the constant lives in (`crate::config::LIMIT`) is
    /// not decidable at extraction time except by the case of a path segment, and a builder
    /// (`Resp::DEFAULT.with_body(handle(cap))`) is not decidable from a dispatch
    /// (`Handler::DEFAULT.run(normalize(input))`) at all — the two are the same expression modulo
    /// identifier spelling. Under the false-edge-is-a-bug contract an undecidable case emits
    /// nothing. The verdicts are collected before asserting, so one failure names EVERY
    /// misclassified form.
    #[test]
    fn an_associated_constant_method_chain_records_no_handler() {
        // Trivia is not part of Rust's token identity. Generate the same four semantic forms
        // through several legal spellings around the member-access dot, including comments and a
        // line break. The leading underscore is part of the SCREAMING_SNAKE convention, not a
        // reason to fall back to the PascalCase receiver guard.
        let qualifiers = [
            "Handler::DEFAULT",
            "Handler::_DEFAULT",
            "<Handler as Runner>::DEFAULT",
            "<Handler as Runner>::_DEFAULT",
        ];
        let trivia = [".", " . ", "\n .\n", " /* receiver */ . ", "\n /* receiver */\n .\t"];
        let mut chain_forms = Vec::new();
        for qualifier in qualifiers {
            for separator in trivia {
                chain_forms.push(format!("{qualifier}{separator}run(input)"));
            }
        }
        // A nested field chain and a turbofish must use the same token structure, too.
        chain_forms.extend([
            "Handler::DEFAULT . build.ship(input)".to_string(),
            "<Handler as Runner>::_DEFAULT /* receiver */ . run::<u8>(input)".to_string(),
            // A leading underscore is valid on a lowercase method as well as on the associated
            // constant.  Ordinary and qualified UFCS forms must both retain delegation.
            "Handler::DEFAULT._run(input)".to_string(),
            "<Handler as Runner>::_DEFAULT._run::<u8>(input)".to_string(),
            // Actual method calls interrupt the field-expression chain.  The final method still
            // delegates through the associated-constant receiver, including turbofish calls,
            // another associated constant in an argument, comments/trivia, and multiple levels.
            "Handler::DEFAULT.build().run(input)".to_string(),
            "Handler::DEFAULT.build::<u8>().run::<u8>(input)".to_string(),
            "Handler::DEFAULT.combine(Other::DEFAULT).run(input)".to_string(),
            "<Handler as Runner>::DEFAULT.build().configure()._run::<u8>(input)".to_string(),
            // A `?`, an `.await` or a paren between the links wraps a value without changing its
            // identity, so it cannot hand the chain to the delegate fall-through.
            "Handler::DEFAULT.build()?.run(input)".to_string(),
            "Handler::DEFAULT.build().await.run(input)".to_string(),
            "(Handler::DEFAULT.build()).run(input)".to_string(),
            "Handler::DEFAULT.build()?.configure().await.run::<u8>(input)".to_string(),
            // An argument that nests a produced call is argument shape, not evidence: a builder
            // and a dispatch are indistinguishable here, so neither records anything.
            "Handler::DEFAULT.run(normalize(input))".to_string(),
            "Resp::DEFAULT.with_body(handle(cap))".to_string(),
            "<Resp as Build>::DEFAULT.with_body(handle(cap))".to_string(),
            "<Handler as Runner>::_DEFAULT /* receiver */ . build /* call */ () /* between */ . \
             run::<u8>(input)"
                .to_string(),
            "Handler::r#DEFAULT.r#build::<u8>().r#run(input)".to_string(),
            "Handler::A\u{203F}DEFAULT.build().r\u{203F}un::<u8>(input)".to_string(),
        ]);

        let skip_forms = [
            "Handler::new(input)",
            "<Resp as Default>::default()",
            "Handler::default.run(input)",
            "Handler::DEFAULT.Run(input)",
            "Handler::DEFAULT.build.Run(input)",
            "Handler::DEFAULT._Run(input)",
            "Handler::DEFAULT(input)",
            "Handler::DEFAULT(input).run(input)",
            "Handler::new(input).run(input)",
            "Handler::new(input).configure().run(input)",
            "Err(problem)",
            "None()",
        ];
        let wrapper_forms = ["Ok(body)", "Some(body)"];
        let mut misclassified = Vec::new();
        misclassified.extend(
            chain_forms
                .iter()
                .filter(|expression| !matches!(role_of(expression), CallRole::Skip))
                .map(String::as_str),
        );
        misclassified.extend(
            skip_forms
                .into_iter()
                .filter(|expression| !matches!(role_of(expression), CallRole::Skip)),
        );
        misclassified.extend(
            wrapper_forms
                .into_iter()
                .filter(|expression| !matches!(role_of(expression), CallRole::Wrapper)),
        );
        assert!(misclassified.is_empty(), "misclassified forms: {misclassified:?}");
    }

    /// The case test reads the segment's leading IDENTIFIER, so a glued-on method chain cannot lend
    /// its lowercase to a constant head, while an acronym-led type keeps its leading uppercase run.
    /// A head that is genuinely PascalCase stays a constructor however the rest of the segment is
    /// spelled — including a trailing method name carrying an underscore, which is the conservative
    /// reading for the false-edge contract.
    #[test]
    fn the_case_test_reads_the_segments_leading_identifier() {
        for name in ["TOOL_NAMES", "TOOL_NAMES.iter().map", "NOW", "NOW.elapsed", "T", "run"] {
            assert!(!is_pascal_case(name), "{name}");
        }
        for name in [
            "MlReq",
            "Upsert",
            "HTTPResponse",
            "Wrapped(payload).to_string",
            "A\u{301}bc",
            "A\u{301}bc.iter().map",
        ] {
            assert!(is_pascal_case(name), "{name}");
        }

        // A decomposed combining mark is a valid XID_Continue character even though it is not
        // alphanumeric. The full call must therefore remain a transparent constructor wrapper,
        // allowing the nested handler call to be traced.
        assert!(matches!(role_of("A\u{301}ccent(handle())"), CallRole::Wrapper));
    }

    #[test]
    fn the_case_test_distinguishes_unicode_identifier_boundaries() {
        // U+203F UNDERTIE is XID_Continue but not alphanumeric: connector punctuation must stay
        // in the leading identifier so its lowercase suffix still makes this PascalCase.
        assert!(is_pascal_case("A\u{203F}bc"));

        // U+00B2 SUPERSCRIPT TWO is alphanumeric but not XID_Continue: the Rust identifier rule
        // must stop before it rather than inheriting the old alphanumeric predicate's suffix.
        assert!(!is_pascal_case("A\u{00B2}bc"));

        // U+05B0 HEBREW POINT SHEVA is both alphanumeric and XID_Continue: retain the ordinary
        // alphanumeric case as a no-regression anchor while using the XID_Continue scan.
        assert!(is_pascal_case("A\u{05B0}bc"));
    }

    #[test]
    fn enum_variant_key_keeps_decomposed_xid_identifiers() {
        for (expression, expected) in [
            ("A\u{301}bc::Upsert(payload)", "A\u{301}bc::Upsert"),
            ("Msg::A\u{301}bc(payload)", "Msg::A\u{301}bc"),
        ] {
            let source = format!("fn probe() {{ {expression}; }}");
            let parsed = crate::index::parser::parse_file(
                std::path::Path::new("probe.rs"),
                rag_rat_base::language::Language::Rust,
                &source,
            )
            .expect("probe parses");
            let function = outermost_call(parsed.root())
                .and_then(|call| call.child_by_field_name("function"))
                .expect("the expression has a callable path");

            assert_eq!(enum_variant_key(function, &source).as_deref(), Some(expected));
        }
    }
}
