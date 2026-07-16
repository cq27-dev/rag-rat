//! Python graph-edge extraction, co-located with Python's parser and resolver policy.
//! `python_edges` walks the CST. Its private helpers recognize PEP 604 unions, static base types,
//! and relative imports.
use std::path::Path;

use tree_sitter::Node;

use crate::index::edges::extract::*;
use crate::index::edges::*;

pub(super) fn python_edges(
    text: &str,
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    path: &Path,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        // `from <module> import <name|name as alias>, ...` — emit Imports edges to the MODULE and
        // to each imported NAME, never the local alias. A relative import (`.sessions`)
        // normalizes to its dotted tail (the leading dots aren't identifiers), so the
        // module name is recorded separately from any `as` alias.
        "import_from_statement" => {
            let module = node.child_by_field_name("module_name");
            if let Some(module) = module
                && let Some(name) = last_identifier_text(module, text)
            {
                out.push(file_edge(
                    path,
                    module,
                    text,
                    name,
                    EdgeKind::Imports,
                    EdgeConfidence::NameOnly,
                ));
            }
            // Record an alias for the rebind ONLY when the from-import is BOTH module-bound AND
            // RELATIVE (`from .compat import X as Y`). Two gates:
            //  - module-bound: a `def`/`class`-nested import binds the alias only in that local
            //    scope, so a whole-file alias scope would rebind unrelated same-name references; it
            //    binds file-wide at top level and inside transparent `if`/`try`/`with`/`for`
            //    blocks.
            //  - relative: a relative import provably names an IN-CORPUS sibling module, so
            //    rebinding its alias to the in-corpus target is safe. An ABSOLUTE import (`from
            //    urllib3.util import Timeout as TimeoutSauce`) is usually EXTERNAL — rebinding
            //    `TimeoutSauce` → bare `Timeout` would mis-bind to a same-named LOCAL class
            //    (`requests.exceptions .Timeout`), a real precision regression measured on
            //    psf/requests (#174 review). Distinguishing absolute-in-corpus from
            //    absolute-external needs a Python package model we don't have; relative is the
            //    correct-by-construction in-corpus subset.
            // A non-recorded alias still emits its plain target Imports edge (dependency captured).
            let record_alias =
                is_python_module_bound(node) && python_from_import_is_relative(node, text);
            let import_start = node.start_byte();
            // The module root is needed to bound the alias's scope at the next module-scope
            // rebinding of the alias name (#174 review) — see `python_import_target`.
            let module_root = record_alias.then(|| python_module_root(node)).flatten();
            let module_id = module.map(|m| m.id());
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if Some(child.id()) == module_id {
                    continue;
                }
                python_import_target(
                    child,
                    text,
                    path,
                    record_alias,
                    import_start,
                    module_root,
                    out,
                );
            }
        },
        // `import <module>` / `import <module> as alias` — Imports edge to the module, not the
        // alias.
        "import_statement" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                python_import_target(child, text, path, false, node.start_byte(), None, out);
            }
        },
        // Function / method / constructor call. Mirror the C handler: the callee is the LAST
        // identifier under the `function` child (`f()` → `f`, `obj.method()` → `method`), the
        // receiver is the first (recorded only as a NameOnly hint — never claimed as exact;
        // resolving it is the oracle's job, not the heuristic's).
        "call" => {
            let function = node.child_by_field_name("function").unwrap_or(node);
            let identifiers = identifiers_under(function, text);
            let identifier_nodes = identifier_nodes_under(function);
            // `handlers[key]()` — the callee is the subscript RESULT, not the index variable
            // `last()` would pick. There's no clean callee identifier, so emit nothing (a wrong
            // `calls_name key` is worse than a missing edge).
            if function.kind() == "subscript" {
                // fall through to recursion without emitting a call edge
            } else if let Some(name) = identifiers.last().cloned() {
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: dotted_qualified_name(&identifiers),
                        receiver_hint: identifiers
                            .first()
                            .filter(|_| identifiers.len() > 1)
                            .cloned(),
                    },
                    identifier_nodes.last().copied().map(CalleeRange::of_node),
                ));
            }
        },
        // `class Foo(Base, Generic[T], metaclass=Meta)` — each POSITIONAL base is an Implements +
        // ReferencesType edge. Keyword (`metaclass=`) and splat (`*bases`/`**kw`) arguments are not
        // superclasses, and a parameterized base resolves to its head (`Generic`, not `T`).
        "class_definition" =>
            if let Some(supers) = node.child_by_field_name("superclasses") {
                let mut cursor = supers.walk();
                for base in supers.named_children(&mut cursor) {
                    if matches!(base.kind(), "keyword_argument" | "list_splat" | "dictionary_splat")
                    {
                        continue;
                    }
                    // Emit Implements ONLY for a STATIC head — a plain identifier, or an attribute
                    // whose receiver chain is all identifiers and not `self`/`cls` (`pkg.Base`),
                    // after unwrapping generic/subscript/paren wrappers (#172 review). A DYNAMIC
                    // base has no compile-time class — `factory()`,
                    // `factory().Base`, `self.Base`, `Base if flag else Other`,
                    // a lambda, … — so claiming an Implements edge would
                    // let the Python class preference mis-bind it to a same-named local class. An
                    // allowlist (vs blocklisting each dynamic form) is robust to new expression
                    // kinds.
                    //
                    // Implements targets the base HEAD's LEAF name (`Base` for
                    // `pkg.Base`/`Generic[T]` → `Generic`); ReferencesType
                    // (below) covers the head and every type argument. The edge
                    // is bare-name (no qualified context): a module-qualified base `pkg.Base`
                    // is resolved by the leaf `Base` exactly like a bare base, because a top-level
                    // Python class's `scope_path` is the bare name, not `pkg::Base`. The cost is
                    // that an EXTERNAL `pkg.Base`/bare imported base can still
                    // bind a same-named local class — the general "Python has
                    // no external-import suppression" gap (#172/#174
                    // review), which needs an in-corpus Python module model to close, not a
                    // per-base special case.
                    if let Some(head) = python_static_base_head(base, text)
                        && let Some(name) = last_identifier_text(head, text)
                    {
                        let callee = last_identifier_node(head)
                            .map(final_segment_node)
                            .map(CalleeRange::of_node);
                        out.push(symbol_edge(
                            symbols,
                            base,
                            name,
                            EdgeKind::Implements,
                            EdgeConfidence::NameOnly,
                            callee,
                        ));
                    }
                    emit_python_type_refs(base, symbols, text, out);
                }
            },
        // Type annotations (`x: T`, `-> T`) wrap their type in a `type` node.
        // `emit_python_type_refs` walks the whole type expression — generics (`Box[Item]`),
        // qualified generics (`typing.Optional[Api]`), unions (`A | B`), nested
        // (`Optional[list[Api]]`), `Callable` param lists — emitting a ReferencesType per
        // referenced type. The alias NAME in a `type X = …` is skipped (it's a definition,
        // not a reference). String forward refs (`-> "Api"`) carry no identifier → no edge.
        "type" if !python_is_type_alias_name(node) => {
            emit_python_type_refs(node, symbols, text, out);
        },
        // A bare decorator (`@requires_auth`, `@pytest.fixture`) is an identifier/attribute, not a
        // `call` — applying it is a call-like dependency, so emit a NameOnly call edge. A
        // parenthesized decorator (`@foo(...)`) is a `call` child, already handled by the call arm
        // via recursion.
        "decorator" => {
            if let Some(inner) = node.named_child(0)
                && matches!(inner.kind(), "identifier" | "attribute")
                && let Some(name) = last_identifier_text(inner, text)
            {
                // Preserve the qualifier so a qualified decorator (`@pytest.fixture`) carries its
                // `pytest` receiver + dotted path — same context the call arm records — so the
                // resolver doesn't fall back to a bare local `fixture` of the same name.
                let identifiers = identifiers_under(inner, text);
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    text,
                    name,
                    EdgeKind::CallsName,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        target_qualified_name: dotted_qualified_name(&identifiers),
                        receiver_hint: identifiers
                            .first()
                            .filter(|_| identifiers.len() > 1)
                            .cloned(),
                    },
                    last_identifier_node(inner).map(final_segment_node).map(CalleeRange::of_node),
                ));
            }
        },
        _ => {},
    }
}

/// Emit `ReferencesType` edges for a subscript-form generic's type arguments, RECURSIVELY so nested
/// generics (`typing.Optional[list[Api]]` → both `list` and `Api`) are all referenced. Each arg's
/// head is emitted, then its own subscript args are walked; non-subscript args terminate.
/// Emit a `ReferencesType` edge for every type referenced in a Python type expression, walking the
/// whole shape: `type`/`generic_type`/`subscript`/`binary_operator` (PEP 604 `A |
/// B`)/`list`/`tuple` (`Callable[[int, A], B]`)/`type_parameter` recurse into their children;
/// `identifier`/`attribute`/ `dotted_name` are leaf references (the dotted tail is the referenced
/// type). Anything else (string forward refs, `None`, literals) terminates with no edge. This
/// single walker handles plain, generic, qualified, union, nested, and callable annotations
/// uniformly.
fn emit_python_type_refs(
    node: Node<'_>,
    symbols: &[IndexedSymbol],
    text: &str,
    out: &mut Vec<EdgeCandidate>,
) {
    match node.kind() {
        "type" | "generic_type" | "subscript" | "binary_operator" | "list" | "tuple"
        | "type_parameter" => {
            // grow_stack: a deeply-nested annotation (a PEP 604 union `A|A|…`, nested generics)
            // recurses to full subtree depth here — grow rather than overflow (#543).
            crate::index::grow_stack(|| {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    emit_python_type_refs(child, symbols, text, out);
                }
            });
        },
        "identifier" =>
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    symbols,
                    node,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    last_identifier_node(node).map(final_segment_node).map(CalleeRange::of_node),
                ));
            },
        // A QUALIFIED type reference (`pkg.Account`, `a.b.C`, `Account.Inner`): record the receiver
        // AND a dotted qualified name. The receiver_hint marks it qualified (so a bare-leaf alias
        // rebind skips it — `pkg.Account` is NOT the local alias `Account`) AND lets a
        // RECEIVER-alias rebind rewrite the root: `Account.Inner` with `Account` an alias
        // for `User` resolves `User::Inner`, not bare `Inner` (#174 review). Without the
        // qualified name the rebind would have nothing to rewrite and resolution would fall
        // back to the ambiguous bare tail.
        "attribute" | "dotted_name" =>
            if let Some(name) = last_identifier_text(node, text) {
                let identifiers = identifiers_under(node, text);
                let receiver = node
                    .child_by_field_name("object")
                    .map(|object| node_text(object, text))
                    .or_else(|| identifiers.first().cloned());
                out.push(symbol_edge_with_context(
                    symbols,
                    node,
                    "",
                    name,
                    EdgeKind::ReferencesType,
                    EdgeConfidence::NameOnly,
                    EdgeContext {
                        receiver_hint: receiver,
                        target_qualified_name: dotted_qualified_name(&identifiers),
                    },
                    last_identifier_node(node).map(final_segment_node).map(CalleeRange::of_node),
                ));
            },
        _ => {},
    }
}

/// Whether `node` is the alias NAME being DEFINED in `type X = …` (the first child of a
/// `type_alias_statement`) — a definition, not a reference, so it must not emit a `ReferencesType`
/// self-edge. The value side (the second `type`) is referenced normally.
fn python_is_type_alias_name(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "type_alias_statement"
            && parent.named_child(0).map(|first| first.id()) == Some(node.id())
    })
}

/// The STATIC head node of a Python base-class expression — a plain `identifier`, or an
/// `attribute`/ `dotted_name` whose receiver chain is all identifiers (`pkg.Base`) — after
/// unwrapping the wrappers `python_type_head` strips (`type`/`generic_type`/`subscript`) plus
/// parentheses. `None` for a DYNAMIC base whose runtime class isn't a compile-time name: a `call`
/// (`factory()`), an attribute off a call (`factory().Base`), a `conditional_expression` (`Base if
/// flag else Other`), a lambda, a binary operator, etc. (#172 review). Only a static head should
/// claim an `Implements` edge — an allowlist, so a NEW dynamic expression form is excluded by
/// default rather than mis-bound by the Python class preference. Bounded loop guards a pathological
/// tree.
fn python_static_base_head<'a>(base: Node<'a>, text: &str) -> Option<Node<'a>> {
    let mut node = base;
    for _ in 0..8 {
        node = match node.kind() {
            "type" | "generic_type" | "parenthesized_expression" => node.named_child(0)?,
            "subscript" => node.child_by_field_name("value")?,
            "identifier" => return Some(node),
            "attribute" | "dotted_name" =>
                return python_attribute_is_static(node, text).then_some(node),
            // call / conditional_expression / lambda / binary_operator / … → no static class.
            _ => return None,
        };
    }
    None
}

/// Whether an `attribute`/`dotted_name` chain is purely static (`pkg.Base`, `a.b.C`) — every
/// receiver is an identifier or another attribute, never a `call`/`subscript` (`factory().Base` is
/// dynamic). A `self`/`cls`-rooted chain (`self.Base`) is DYNAMIC: the base comes off the runtime
/// instance, not a compile-time class (#172 review). Bounded loop guards a pathological tree.
fn python_attribute_is_static(node: Node<'_>, text: &str) -> bool {
    let mut current = node;
    for _ in 0..16 {
        match current.kind() {
            "identifier" => return !matches!(node_text(current, text).as_str(), "self" | "cls"),
            "dotted_name" =>
                return !matches!(
                    first_identifier_text(current, text).as_deref(),
                    Some("self" | "cls")
                ),
            "attribute" => match current.child_by_field_name("object") {
                Some(object) => current = object,
                None => return false,
            },
            _ => return false,
        }
    }
    false
}

/// Emit an Imports edge for one import clause (`dotted_name` or `aliased_import`), targeting the
/// imported name's dotted tail — NEVER the `as` alias (which is a local binding, not the import). A
/// comma / parenthesized list (`from pkg import A, B`) nests its clauses under an `import_list`, so
/// recurse into that.
/// Whether a Python import statement binds its names in the MODULE namespace: true at the top level
/// and inside top-level `if`/`try`/`with`/`for` blocks (so `try: from x import Y as Z except` still
/// scopes `Z` file-wide), false inside a `def`/`class`/lambda body (those bind locally). Walks
/// ancestors to the `module` root, treating block statements as transparent (#174 review).
fn is_python_module_bound(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        match ancestor.kind() {
            "module" => return true,
            "function_definition" | "class_definition" | "lambda" => return false,
            _ => current = ancestor.parent(),
        }
    }
    false
}

/// Whether a `from … import …` statement is RELATIVE (`from . import x`, `from .compat import y`,
/// `from ..pkg import z`) rather than absolute (`from urllib3.util import …`). A relative import
/// names a sibling/parent module WITHIN the same Python package, so its alias is almost always
/// in-corpus and safe to rebind; an absolute import may be external (#174 review). Detected by a
/// `relative_import` module node, with a leading-dot text fallback for the `from . import x` shape.
///
/// LIMITATION (documented): when the index root/targets cover only a SUBpackage, a parent relative
/// import (`from ..models import X`) can still point OUTSIDE the indexed targets, and the rebind
/// would resolve the bare target against an unrelated in-corpus symbol. Closing this needs an
/// in-corpus Python module/package model (resolve the relative module to an indexed file) — the
/// same machinery the absolute-in-corpus case wants — which rag-rat does not have yet.
fn python_from_import_is_relative(node: Node<'_>, text: &str) -> bool {
    if node.child_by_field_name("module_name").is_some_and(|m| m.kind() == "relative_import") {
        return true;
    }
    node_text(node, text)
        .strip_prefix("from")
        .map(|rest| rest.trim_start())
        .is_some_and(|rest| rest.starts_with('.'))
}

/// The enclosing `module` node (Python file root), walking up from `node`. `None` only for a
/// detached node with no module ancestor.
fn python_module_root(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = Some(node);
    while let Some(ancestor) = current {
        if ancestor.kind() == "module" {
            return Some(ancestor);
        }
        current = ancestor.parent();
    }
    None
}

/// The byte at which the next MODULE-SCOPE rebinding of `name` strictly after `after_byte` takes
/// effect, or `None` (#174 review). Bounds a from-import alias's scope: Python is order-dependent,
/// so a later `name = …` / `def name` / `class name` / `type name = …` / re-import reassigns the
/// name and the alias must not rebind references past that point. Only UNCONDITIONAL top-level
/// statements (DIRECT children of the `module`) count: a rebinding inside a `def`/`class` body is
/// local, and one inside a conditional block (`if`/`try`/…) isn't guaranteed — the `try: import …
/// except: import …` fallback is left ambiguous downstream instead of shadowed by byte order.
/// Bindings BEFORE the import are excluded by `after_byte`.
fn python_next_module_binding(
    module: Node<'_>,
    name: &str,
    after_byte: usize,
    text: &str,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut cursor = module.walk();
    for child in module.named_children(&mut cursor) {
        if let Some(byte) = python_rebinding_effective_byte(child, name, text)
            && byte > after_byte
        {
            best = Some(best.map_or(byte, |current: usize| current.min(byte)));
        }
    }
    best
}

/// The byte at which a top-level statement's rebinding of `name` TAKES EFFECT (past which the alias
/// is dead), or `None` if it doesn't rebind `name` (#174 review). For an ASSIGNMENT this is the
/// statement END — the right-hand side still sees the old alias (`Account = Account()` resolves its
/// RHS to the import) — for a `def`/`class`/`type`/import it is the START (the def's own body refs
/// are a separate scope; the rare header-annotation case stays bound to the new name). A bare
/// annotation without a value (`Account: T`) records `__annotations__` but does NOT bind `name`, so
/// it is skipped. A plain `import a.b.c` binds the TOP-LEVEL `a`, never the dotted tail.
fn python_rebinding_effective_byte(node: Node<'_>, name: &str, text: &str) -> Option<usize> {
    match node.kind() {
        "function_definition" | "class_definition" | "type_alias_statement" => node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0))
            .and_then(|name_node| last_identifier_text(name_node, text))
            .filter(|defined| defined == name)
            .map(|_| node.start_byte()),
        "expression_statement" => node.named_child(0).and_then(|inner| {
            // grow_stack: uniform depth guard (#543); shallow today, no-op fast path.
            crate::index::grow_stack(|| python_rebinding_effective_byte(inner, name, text))
        }),
        "assignment"
            if node.child_by_field_name("right").is_some()
                && node
                    .child_by_field_name("left")
                    .is_some_and(|left| python_assignment_target_binds(left, name, text)) =>
            Some(node.end_byte()),
        "import_from_statement" | "import_statement" =>
            python_import_binds_name(node, name, text).then(|| node.start_byte()),
        // `del Account` at module scope removes the binding, so the alias is dead from there (#174
        // review). `del Account.attr` / `del Account[i]` do NOT unbind `Account` itself —
        // `python_assignment_target_binds` returns false for attribute/subscript targets.
        "delete_statement" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .any(|target| python_assignment_target_binds(target, name, text))
                .then(|| node.start_byte())
        },
        _ => None,
    }
}

/// Whether a target binds the bare name `name` — a plain `identifier`, a tuple/list-unpacking
/// target containing it (`name, other = …`), a starred target (`*name, rest = …`), or a `del`
/// expression list. An `attribute`/`subscript` target (`name.attr`, `name[i]`) does NOT rebind
/// `name`.
fn python_assignment_target_binds(target: Node<'_>, name: &str, text: &str) -> bool {
    match target.kind() {
        "identifier" => node_text(target, text) == name,
        "pattern_list" | "tuple_pattern" | "list_pattern" | "splat_pattern"
        | "list_splat_pattern" | "expression_list" => crate::index::grow_stack(|| {
            // grow_stack: nested unpacking (`a, (b, (c, …))`) recurses to full depth (#543).
            let mut cursor = target.walk();
            target
                .named_children(&mut cursor)
                .any(|element| python_assignment_target_binds(element, name, text))
        }),
        _ => false,
    }
}

/// Whether an import statement binds `name`. The bound name depends on the import FORM (#174
/// review): a `from m import T as name` / `from m import name` binds the imported LEAF, but a plain
/// `import a.b.c` binds only the TOP-LEVEL `a` (Python doesn't bind the dotted tail) — `import
/// a.b.c as name` binds the alias. So an `import other.Account` does NOT rebind `Account`.
fn python_import_binds_name(node: Node<'_>, name: &str, text: &str) -> bool {
    let from_import = node.kind() == "import_from_statement";
    let module_id = node.child_by_field_name("module_name").map(|module| module.id());
    // grow_stack: uniform depth guard for a tree descender (#543); `import_list` doesn't nest
    // deeply today, so this is a no-op fast path.
    crate::index::grow_stack(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).any(|child| {
            if Some(child.id()) == module_id {
                return false;
            }
            match child.kind() {
                "aliased_import" => child
                    .child_by_field_name("alias")
                    .and_then(|alias| last_identifier_text(alias, text))
                    .is_some_and(|alias| alias == name),
                // from-import: the imported leaf (`from m import Account`). plain import: the
                // top-level segment of the dotted module path (`import other.Account` binds
                // `other`).
                "dotted_name" if from_import =>
                    last_identifier_text(child, text).is_some_and(|leaf| leaf == name),
                "dotted_name" =>
                    first_identifier_text(child, text).is_some_and(|root| root == name),
                "import_list" => python_import_binds_name(child, name, text),
                _ => false,
            }
        })
    })
}

fn python_import_target(
    child: Node<'_>,
    text: &str,
    path: &Path,
    record_alias: bool,
    import_start: usize,
    module_root: Option<Node<'_>>,
    out: &mut Vec<EdgeCandidate>,
) {
    if child.kind() == "import_list" {
        // grow_stack: uniform depth guard for a tree descender (#543); `import_list` doesn't nest
        // deeply today, so this is a no-op fast path, but the invariant stays uniform.
        crate::index::grow_stack(|| {
            let mut cursor = child.walk();
            for clause in child.named_children(&mut cursor) {
                python_import_target(
                    clause,
                    text,
                    path,
                    record_alias,
                    import_start,
                    module_root,
                    out,
                );
            }
        });
        return;
    }
    // `from <module> import <target> as <alias>` — a SYMBOL alias (#174). Emit the Imports edge to
    // the target (so the in-corpus dependency is recorded) but carry the alias in `evidence` + an
    // import scope, so resolution can rebind a later `alias` reference to `target`. Recorded only
    // for a top-level import (`record_alias`, checked by the caller); `import x as m` (module
    // alias) is a qualified-resolution problem, left out of scope.
    if record_alias
        && child.kind() == "aliased_import"
        && let Some(target_node) = child.child_by_field_name("name")
        && let Some(target) = last_identifier_text(target_node, text)
        && let Some(alias_node) = child.child_by_field_name("alias")
        && let Some(alias) = last_identifier_text(alias_node, text)
    {
        // The alias binding is valid from the import until the name is REBOUND at module scope —
        // Python is order-dependent, so a later `alias = …` / `def alias` / `class alias` /
        // re-import reassigns the name and the alias must not rebind references past that point
        // (#174 review). `scope_end` is that next module-scope rebinding, else end of file. Only
        // module-scope bindings count (a binding inside a def/class body is local), and the scan is
        // ordered by byte, so a definition BEFORE the import does not shrink the scope.
        let scope_end = module_root
            .and_then(|root| python_next_module_binding(root, &alias, import_start, text))
            .unwrap_or(text.len());
        let scope =
            ImportScopeRange { scope_start: import_start, scope_end, mod_id: MOD_FILE_ROOT };
        out.push(file_edge_scoped(
            path,
            target_node,
            target,
            Some(alias),
            EdgeKind::Imports,
            EdgeConfidence::NameOnly,
            Some(scope),
        ));
        return;
    }
    let target = match child.kind() {
        "aliased_import" => child.child_by_field_name("name"),
        "dotted_name" => Some(child),
        _ => None,
    };
    if let Some(target) = target
        && let Some(name) = last_identifier_text(target, text)
    {
        out.push(file_edge(path, target, text, name, EdgeKind::Imports, EdgeConfidence::NameOnly));
    }
}

#[cfg(test)]
mod python_edge_tests {
    use std::path::Path;

    use rag_rat_base::language::Language;

    use super::*;

    fn edges(src: &str) -> Vec<EdgeCandidate> {
        // No symbol table needed: the syntactic pass emits NameOnly candidates regardless of
        // resolution, which is exactly the signal these assertions check.
        syntactic_edges(Path::new("src/Main.py"), Language::Python, src, &[]).unwrap()
    }

    fn has(edges: &[EdgeCandidate], kind: EdgeKind, name: &str) -> bool {
        edges.iter().any(|e| e.edge_kind == kind && e.to_name == name)
    }

    #[test]
    fn relative_import_normalized_and_alias_not_treated_as_import() {
        let e = edges("from .sessions import Session as ClientSession\n");
        // The relative module `.sessions` normalizes to its dotted tail; the imported symbol is
        // recorded separately.
        assert!(has(&e, EdgeKind::Imports, "sessions"), "module import missing: {e:?}");
        assert!(has(&e, EdgeKind::Imports, "Session"), "imported name missing: {e:?}");
        // The local `as` alias is NOT an import target.
        assert!(!has(&e, EdgeKind::Imports, "ClientSession"), "alias wrongly imported: {e:?}");
    }

    #[test]
    fn plain_and_dotted_imports_use_module_not_alias() {
        let e = edges("from requests.adapters import HTTPAdapter\nimport urllib3 as http\n");
        assert!(has(&e, EdgeKind::Imports, "adapters"), "dotted module tail missing: {e:?}");
        assert!(has(&e, EdgeKind::Imports, "HTTPAdapter"));
        assert!(has(&e, EdgeKind::Imports, "urllib3"), "import module missing: {e:?}");
        assert!(!has(&e, EdgeKind::Imports, "http"), "import alias wrongly recorded: {e:?}");
    }

    #[test]
    fn method_call_records_receiver_hint_at_name_only_not_exact() {
        let e = edges("def f():\n    http.disable_warnings()\n");
        let call = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::CallsName && c.to_name == "disable_warnings")
            .expect("method call edge");
        assert_eq!(call.receiver_hint.as_deref(), Some("http"));
        // The heuristic must NOT claim exact resolution for a member call — that's the oracle's
        // job.
        assert_eq!(call.confidence, EdgeConfidence::NameOnly);
    }

    #[test]
    fn alias_call_emits_name_only_call_edge() {
        // A call through an imported alias is a NameOnly call to the alias name (resolving the
        // alias to its imported symbol is the resolver/oracle's job, not the syntactic
        // pass).
        let e = edges("def make():\n    return ClientSession()\n");
        let call = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::CallsName && c.to_name == "ClientSession")
            .expect("alias call edge");
        assert_eq!(call.confidence, EdgeConfidence::NameOnly);
    }

    #[test]
    fn base_class_emits_implements_and_references_type() {
        let e = edges("class Api(Session):\n    pass\n");
        assert!(has(&e, EdgeKind::Implements, "Session"), "base class Implements missing: {e:?}");
        assert!(
            has(&e, EdgeKind::ReferencesType, "Session"),
            "base class ReferencesType missing: {e:?}"
        );
    }

    #[test]
    fn generic_base_resolves_to_base_not_type_arg() {
        // `class Repo(Generic[T])` — the base is `Generic`, NOT the type argument `T`.
        let e = edges("class Repo(Generic[T]):\n    pass\n");
        assert!(has(&e, EdgeKind::Implements, "Generic"), "base should be Generic: {e:?}");
        assert!(!has(&e, EdgeKind::Implements, "T"), "must not resolve to type arg T: {e:?}");
    }

    #[test]
    fn call_shaped_base_emits_no_implements() {
        // `class Sub(factory())` — a DYNAMIC base (the head is a callable, not the base class). No
        // Implements edge (the resolver's class preference would mis-bind it), but the base call is
        // still captured as a CallsName so the dependency on `factory` isn't lost (#172 review).
        let e = edges("class Sub(factory()):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "factory"),
            "a call-shaped base must NOT emit Implements: {e:?}"
        );
        assert!(
            has(&e, EdgeKind::CallsName, "factory"),
            "the base call is still captured as a CallsName: {e:?}"
        );
    }

    #[test]
    fn parenthesized_call_shaped_base_emits_no_implements() {
        // `class Sub((factory()))` — tree-sitter nests the `call` under a
        // `parenthesized_expression`, so the immediate-kind check missed it (#172 review round 2).
        // Still a DYNAMIC base: no Implements, but the call dependency is captured.
        let e = edges("class Sub((factory())):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "factory"),
            "a parenthesized call-shaped base must NOT emit Implements: {e:?}"
        );
        assert!(
            has(&e, EdgeKind::CallsName, "factory"),
            "the base call is still captured as a CallsName: {e:?}"
        );
    }

    #[test]
    fn subscript_call_shaped_base_emits_no_implements() {
        // `class Sub(factory()[T])` — tree-sitter exposes the base as a `subscript` whose value is
        // the `call`; `python_type_head` unwraps the subscript to that call, so the dynamic-base
        // check must unwrap it too (#172 review round 3). Still dynamic: no Implements.
        let e = edges("class Sub(factory()[T]):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "factory"),
            "a subscript-on-call base must NOT emit Implements: {e:?}"
        );
        assert!(
            has(&e, EdgeKind::CallsName, "factory"),
            "the base call is still captured as a CallsName: {e:?}"
        );
    }

    #[test]
    fn attribute_on_call_base_emits_no_implements() {
        // `class Sub(factory().Base)` — the base is an `attribute` whose receiver is a `call`, so
        // `Base` comes off the factory RESULT, not a static class (#172 review round 4). Dynamic:
        // no Implements; subscripted `factory().Base[T]` is the same shape under a
        // subscript.
        let e = edges("class Sub(factory().Base):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "Base"),
            "an attribute on a call result must NOT emit Implements: {e:?}"
        );
        let e = edges("class Sub(factory().Base[T]):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "Base"),
            "a subscripted attribute on a call result must NOT emit Implements: {e:?}"
        );
    }

    #[test]
    fn static_attribute_base_emits_a_bare_implements() {
        // `class Sub(pkg.Base)` — a static qualified base (receiver is a module, not a call) is a
        // real superclass; the Implements edge targets the LEAF `Base` and is BARE (no qualified
        // context) so it resolves like a bare base — a top-level Python class's `scope_path` is the
        // bare name, not `pkg::Base` (#172 review).
        let e = edges("class Sub(pkg.Base):\n    pass\n");
        let imp = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::Implements && c.to_name == "Base")
            .expect("qualified base Implements edge");
        assert_eq!(imp.receiver_hint, None, "qualified base implements is bare-name");
        assert_eq!(imp.target_qualified_name, None, "qualified base implements is bare-name");
    }

    #[test]
    fn self_qualified_base_emits_no_implements() {
        // `class Sub(self.Base)` (e.g. inside a method) — the base comes off the runtime instance,
        // not a compile-time class, so it is DYNAMIC: no Implements (#172 review).
        let e = edges(
            "class Outer:\n    def make(self):\n        class Sub(self.Base):\n            pass\n",
        );
        assert!(
            !has(&e, EdgeKind::Implements, "Base"),
            "a `self.`-rooted base must NOT emit Implements: {e:?}"
        );
    }

    #[test]
    fn conditional_expression_base_emits_no_implements() {
        // `class Sub(Base if flag else Other)` — the runtime base is expression-dependent, so it is
        // NOT a static class; emitting an Implements to the last identifier (`Other`) would let the
        // class preference mis-bind it (#172 review). The allowlist excludes the whole expression.
        let e = edges("class Sub(Base if flag else Other):\n    pass\n");
        assert!(
            !has(&e, EdgeKind::Implements, "Other"),
            "a conditional-expression base must NOT emit Implements: {e:?}"
        );
        assert!(
            !has(&e, EdgeKind::Implements, "Base"),
            "a conditional-expression base must NOT emit Implements: {e:?}"
        );
    }

    #[test]
    fn parenthesized_class_base_still_emits_implements() {
        // `class Sub((Base))` — a parenthesized but STATIC base is a real superclass, so the
        // Implements edge must survive the dynamic-base guard.
        let e = edges("class Sub((Base)):\n    pass\n");
        assert!(
            has(&e, EdgeKind::Implements, "Base"),
            "a parenthesized class base must still emit Implements: {e:?}"
        );
    }

    #[test]
    fn metaclass_keyword_argument_is_not_a_base_class() {
        // `class Model(Base, metaclass=Meta)` — `Base` is a base; `metaclass=Meta` is not.
        let e = edges("class Model(Base, metaclass=Meta):\n    pass\n");
        assert!(has(&e, EdgeKind::Implements, "Base"), "Base should be a base: {e:?}");
        assert!(
            !has(&e, EdgeKind::Implements, "Meta"),
            "metaclass kwarg must not be a base: {e:?}"
        );
    }

    #[test]
    fn generic_annotation_anchors_the_head_type() {
        // `x: Box[Item]` references `Box` (the head), which would otherwise be invisible.
        let e = edges("def f(x: Box[Item]) -> None:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "Box"), "annotation head Box missing: {e:?}");
    }

    #[test]
    fn qualified_generic_annotation_emits_head_and_arg() {
        // `x: typing.Optional[Api]` is a `subscript` — emit BOTH the head (`Optional`) and the type
        // argument (`Api`); the latter is a plain expression the recursion otherwise misses.
        let e = edges("def f(x: typing.Optional[Api]) -> None:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "Optional"), "head Optional missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "Api"), "type arg Api missing: {e:?}");
    }

    #[test]
    fn union_annotation_emits_both_operands() {
        // PEP 604 `A | B` — both operands are referenced types (was: only the last).
        let e = edges("def f(x: A | B) -> None:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "A"), "union operand A missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "B"), "union operand B missing: {e:?}");
    }

    #[test]
    fn subscript_callee_does_not_record_the_index() {
        // `handlers[key]()` — `key` is the index, not the callee; emit no bogus call edge.
        let e = edges("def f():\n    handlers[key]()\n");
        assert!(
            !has(&e, EdgeKind::CallsName, "key"),
            "index var wrongly recorded as callee: {e:?}"
        );
    }

    #[test]
    fn generic_base_class_emits_type_args() {
        // `class C(Mapping[str, Api])` — Implements the head `Mapping`, ReferencesType the arg
        // `Api`.
        let e = edges("class C(Mapping[str, Api]):\n    pass\n");
        assert!(has(&e, EdgeKind::Implements, "Mapping"), "base head Implements missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "Api"), "base type arg Api missing: {e:?}");
        assert!(!has(&e, EdgeKind::Implements, "Api"), "type arg must not be Implements: {e:?}");
    }

    #[test]
    fn type_alias_does_not_self_reference() {
        // `type UserId = int` references `int`, NOT the alias name `UserId` being defined.
        let e = edges("type UserId = int\n");
        assert!(has(&e, EdgeKind::ReferencesType, "int"), "alias value int missing: {e:?}");
        assert!(!has(&e, EdgeKind::ReferencesType, "UserId"), "alias name self-referenced: {e:?}");
    }

    #[test]
    fn nested_qualified_generic_annotation_emits_all_heads() {
        // `typing.Optional[list[Api]]` → Optional + list + the nested project type Api.
        let e = edges("def f(x: typing.Optional[list[Api]]) -> None:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "Optional"), "Optional missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "list"), "list missing: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "Api"), "nested Api missing: {e:?}");
    }

    #[test]
    fn bare_decorator_emits_a_call_edge() {
        // `@requires_auth` (no parens) is a dependency of the decorated symbol.
        let e = edges("@requires_auth\ndef handler():\n    pass\n");
        assert!(
            has(&e, EdgeKind::CallsName, "requires_auth"),
            "bare decorator edge missing: {e:?}"
        );
    }

    #[test]
    fn qualified_bare_decorator_keeps_its_receiver() {
        // `@pytest.fixture` records the `pytest` receiver so it can't fall back to a local
        // `fixture`.
        let e = edges("@pytest.fixture\ndef t():\n    pass\n");
        let edge = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::CallsName && c.to_name == "fixture")
            .expect("qualified decorator edge");
        assert_eq!(edge.receiver_hint.as_deref(), Some("pytest"));
    }

    #[test]
    fn multi_import_emits_each_imported_name() {
        // `from pkg import A, B` nests the names under an `import_list`.
        let e = edges("from pkg import A, B\n");
        assert!(has(&e, EdgeKind::Imports, "A"), "import A missing: {e:?}");
        assert!(has(&e, EdgeKind::Imports, "B"), "import B missing: {e:?}");
    }

    #[test]
    fn annotation_emits_references_type() {
        let e = edges("def f(url: str) -> None:\n    pass\n");
        assert!(
            has(&e, EdgeKind::ReferencesType, "str"),
            "annotation ReferencesType missing: {e:?}"
        );
    }

    /// The Imports edge carrying a from-import alias (`from m import User as Account`): its
    /// `to_name` is the imported target, `evidence` is the alias, and — unlike the plain dependency
    /// edge (whose evidence defaults to its own name) — it carries an import scope so resolution
    /// can rebind alias references (#174). The import scope is what marks it as an alias
    /// binding.
    fn aliased_import<'e>(edges: &'e [EdgeCandidate], target: &str) -> Option<&'e EdgeCandidate> {
        edges.iter().find(|e| {
            e.edge_kind == EdgeKind::Imports && e.to_name == target && e.import_scope.is_some()
        })
    }

    #[test]
    fn top_level_relative_from_import_alias_carries_scope_and_evidence() {
        // A module-level RELATIVE `from .models import User as Account` binds `Account` file-wide
        // and is safe to rebind (the module is provably in-corpus) (#174 review).
        let e = edges("from .models import User as Account\n");
        let imp = aliased_import(&e, "User").expect("aliased import edge for User");
        assert_eq!(imp.evidence.as_deref(), Some("Account"), "alias must ride on evidence");
        assert!(imp.import_scope.is_some(), "module-level alias must carry an import scope");
    }

    #[test]
    fn absolute_external_from_import_alias_is_not_recorded() {
        // `from urllib3.util import Timeout as TimeoutSauce` — an ABSOLUTE import, usually
        // EXTERNAL. Rebinding `TimeoutSauce` → bare `Timeout` would mis-bind to a
        // same-named LOCAL class (the measured psf/requests precision regression), so no
        // alias scope is recorded — only the plain dependency edge to the target (#174
        // review). The dotted module tail still imports.
        let e = edges("from urllib3.util import Timeout as TimeoutSauce\n");
        assert!(
            has(&e, EdgeKind::Imports, "Timeout"),
            "the dependency edge is still emitted: {e:?}"
        );
        assert!(
            aliased_import(&e, "Timeout").is_none(),
            "an absolute (external) alias must NOT carry a rebind scope: {e:?}"
        );
    }

    #[test]
    fn try_block_from_import_alias_is_module_bound() {
        // The `try: import X as Y except ImportError:` fallback pattern binds in the MODULE
        // namespace — a top-level `try` block is transparent (#174 review).
        let e = edges(
            "try:\n    from .fast import Engine as DB\nexcept ImportError:\n    from .slow import \
             Engine as DB\n",
        );
        let imp = aliased_import(&e, "Engine").expect("aliased import edge for Engine");
        assert_eq!(imp.evidence.as_deref(), Some("DB"));
        assert!(imp.import_scope.is_some(), "try-block alias must still be module-scoped");
    }

    #[test]
    fn function_nested_from_import_alias_is_not_recorded() {
        // An import inside a `def` binds the alias LOCALLY, not file-wide — recording it whole-file
        // would rebind unrelated same-name references, so no alias is carried (#174 review). The
        // plain dependency edge to the target is still emitted.
        let e =
            edges("def load():\n    from .models import User as Account\n    return Account()\n");
        assert!(
            has(&e, EdgeKind::Imports, "User"),
            "the dependency edge to the target is still emitted: {e:?}"
        );
        assert!(
            aliased_import(&e, "User").is_none(),
            "a function-nested alias must not be recorded file-wide: {e:?}"
        );
    }

    #[test]
    fn qualified_type_reference_carries_a_receiver_hint() {
        // `x: pkg.Account` is a qualified type reference — the receiver hint marks it so the alias
        // rebind skips it (`pkg.Account` is not the local alias `Account`) (#174 review).
        let e = edges("def f(x: pkg.Account) -> None:\n    pass\n");
        let ref_edge = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::ReferencesType && c.to_name == "Account")
            .expect("qualified type reference edge");
        assert_eq!(ref_edge.receiver_hint.as_deref(), Some("pkg"));
    }

    /// The alias edge's `scope_end` — where extraction decided the alias stops applying because the
    /// name is rebound at module scope (#174 review).
    fn alias_scope_end(edges: &[EdgeCandidate], target: &str) -> usize {
        aliased_import(edges, target)
            .and_then(|e| e.import_scope)
            .expect("aliased import edge with a scope")
            .scope_end
    }

    #[test]
    fn alias_scope_runs_to_eof_with_no_redefinition() {
        // No later rebinding of `Account`: the alias is in effect for the whole file.
        let src = "from .models import User as Account\nAccount()\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.len(), "scope should run to EOF");
    }

    #[test]
    fn alias_scope_ends_at_a_later_class_redefinition() {
        // `class Account` after the import reassigns the name at module scope; the alias must stop
        // there so a later `Account()` resolves to the local class, not the import.
        let src = "from .models import User as Account\nclass Account:\n    pass\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.find("class Account").unwrap());
    }

    #[test]
    fn alias_scope_ends_at_a_later_module_assignment() {
        // A plain lowercase module assignment (`Account = ...`) is NOT indexed as a symbol, so only
        // this extraction-time scan catches it — the round-2 `latest_def_before` could not. The
        // scope ends at the assignment's END, not its start, so the RHS still sees the alias
        // (`Account = Account()` resolves its RHS to the import, #174 review).
        let src = "from .models import User as Account\nAccount = make_account()\n";
        let e = edges(src);
        let stmt = "Account = make_account()";
        let expected = src.find(stmt).unwrap() + stmt.len();
        assert_eq!(
            alias_scope_end(&e, "User"),
            expected,
            "scope ends after the assignment, not before"
        );
    }

    #[test]
    fn alias_scope_ends_at_a_starred_unpacking_rebinding() {
        // `*Account, rest = rows` rebinds `Account` at module scope through a starred target (#174
        // review) — the scan must look inside `splat_pattern`/`list_splat_pattern`.
        let src = "from .models import User as Account\n*Account, rest = rows\n";
        let e = edges(src);
        let stmt = "*Account, rest = rows";
        let expected = src.find(stmt).unwrap() + stmt.len();
        assert_eq!(alias_scope_end(&e, "User"), expected, "a starred unpack must end the scope");
    }

    #[test]
    fn alias_scope_ends_at_a_module_level_del() {
        // `del Account` removes the binding, so the alias is dead from there (#174 review).
        let src = "from .models import User as Account\ndel Account\nAccount()\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.find("del Account").unwrap());
    }

    #[test]
    fn alias_scope_ignores_a_del_of_an_attribute() {
        // `del Account.cache` removes an attribute, NOT the `Account` binding — the alias survives.
        let src = "from .models import User as Account\ndel Account.cache\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "a `del` of an attribute must not end the scope"
        );
    }

    #[test]
    fn alias_scope_covers_the_rhs_of_its_own_rebinding() {
        // `Account = Account()` — the RHS `Account()` evaluates BEFORE the new binding takes
        // effect, so it must still be inside the alias scope (#174 review). The RHS call's
        // byte is < scope_end.
        let src = "from .models import User as Account\nAccount = Account()\n";
        let e = edges(src);
        let scope_end = alias_scope_end(&e, "User");
        let rhs_call = src.rfind("Account()").unwrap();
        assert!(rhs_call < scope_end, "the RHS reference must fall within the alias scope");
    }

    #[test]
    fn alias_scope_ends_at_a_later_type_alias() {
        // PEP 695 `type Account = int` rebinds the name at module scope (#174 review).
        let src = "from .models import User as Account\ntype Account = int\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.find("type Account").unwrap());
    }

    #[test]
    fn alias_scope_ignores_a_value_less_annotation() {
        // `Account: type[User]` records `__annotations__` but does NOT bind `Account` (no value),
        // so the alias is still in effect afterward (#174 review).
        let src = "from .models import User as Account\nAccount: type[User]\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "a value-less annotation must not shrink scope"
        );
    }

    #[test]
    fn alias_scope_ignores_a_plain_dotted_import() {
        // `import other.Account` binds the top-level `other`, never the dotted tail `Account`, so
        // it must not end an `Account` alias (#174 review).
        let src = "from .models import User as Account\nimport other.Account\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "a plain dotted import of the tail must not shrink scope"
        );
    }

    #[test]
    fn alias_scope_ignores_a_conditional_rebinding() {
        // A rebinding inside a top-level `if`/`try` block isn't guaranteed to execute, so it must
        // not shrink the alias scope by byte order (#174 review) — the ambiguity is left to
        // the resolver.
        let src =
            "from .models import User as Account\nif flag:\n    Account = other()\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "a conditional rebinding must not shrink scope"
        );
    }

    #[test]
    fn qualified_type_reference_carries_a_qualified_name() {
        // `x: Account.Inner` — a qualified type ref carries BOTH the receiver hint and a dotted
        // qualified name, so a receiver-alias rebind can rewrite the root to `User::Inner` (#174
        // review).
        let e = edges("def f(x: Account.Inner) -> None:\n    pass\n");
        let ref_edge = e
            .iter()
            .find(|c| c.edge_kind == EdgeKind::ReferencesType && c.to_name == "Inner")
            .expect("qualified type reference edge");
        assert_eq!(ref_edge.receiver_hint.as_deref(), Some("Account"));
        assert_eq!(ref_edge.target_qualified_name.as_deref(), Some("Account::Inner"));
    }

    #[test]
    fn alias_scope_ignores_a_nested_redefinition() {
        // A `class Account` INSIDE a function binds locally, not at module scope, so it must not
        // shrink the alias scope (the round-2 whole-symbol scan wrongly counted nested defs).
        let src =
            "from .models import User as Account\ndef f():\n    class Account:\n        pass\n";
        let e = edges(src);
        assert_eq!(alias_scope_end(&e, "User"), src.len(), "a nested redef must not shrink scope");
    }

    #[test]
    fn alias_scope_ignores_an_attribute_assignment() {
        // `Account.config = x` mutates an attribute; it does NOT rebind `Account` itself, so the
        // alias scope must not stop there.
        let src = "from .models import User as Account\nAccount.config = 1\nAccount()\n";
        let e = edges(src);
        assert_eq!(
            alias_scope_end(&e, "User"),
            src.len(),
            "an attribute set must not shrink scope"
        );
    }

    #[test]
    fn alias_scope_starts_after_a_definition_before_the_import() {
        // A `class Account` BEFORE the import does not shadow it (the import is the later binding):
        // scope_start is the import, scope_end runs to EOF.
        let src = "class Account:\n    pass\n\n\nfrom .models import User as Account\nAccount()\n";
        let e = edges(src);
        let scope = aliased_import(&e, "User").and_then(|edge| edge.import_scope).expect("scope");
        assert_eq!(
            scope.scope_start,
            src.find("from .models").unwrap(),
            "scope starts at the import"
        );
        assert_eq!(scope.scope_end, src.len(), "a redef before the import does not shrink scope");
    }

    #[test]
    fn nested_type_annotations_emit_type_refs_via_the_recursive_walk() {
        // Exercises emit_python_type_refs recursing through generic/subscript/union nodes (#543
        // grow_stack-wrapped) on NORMAL input.
        let e = edges("def f(x: dict[str, list[int]]) -> Account | Other:\n    pass\n");
        assert!(has(&e, EdgeKind::ReferencesType, "dict"), "generic type ref: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "int"), "nested subscript type ref: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "Account"), "union member type ref: {e:?}");
        assert!(has(&e, EdgeKind::ReferencesType, "Other"), "union member type ref: {e:?}");
    }

    #[test]
    fn parenthesized_import_list_records_each_target() {
        // Exercises python_import_target recursing into `import_list` (#543 grow_stack-wrapped).
        let e = edges("from pkg import (Alpha, Beta as B, Gamma)\n");
        assert!(has(&e, EdgeKind::Imports, "Alpha"), "first list member: {e:?}");
        assert!(has(&e, EdgeKind::Imports, "Beta"), "aliased list member target: {e:?}");
        assert!(has(&e, EdgeKind::Imports, "Gamma"), "third list member: {e:?}");
        assert!(!has(&e, EdgeKind::Imports, "B"), "alias must not be an import target: {e:?}");
    }

    #[test]
    fn alias_scope_scan_walks_unpacking_and_expression_statements() {
        // The aliased import triggers the module-scope rebinding scan (python_next_module_binding →
        // python_rebinding_effective_byte over statements → python_assignment_target_binds on a
        // nested unpacking target). All #543 grow_stack-wrapped; this runs them on normal input.
        // The plain dotted `import Acct.deep` makes the scan's dotted-name arm run
        // `first_identifier_text` (checking whether the root `Acct` rebinds the alias).
        let e = edges(
            "from mod import Account as Acct\n(a, (b, c)) = g()\nresult\nimport Acct.deep\nAcct = \
             5\n",
        );
        // Running the scan is the point (it walks the wrapped helpers on normal input); the import
        // target is still recorded, and the local `as` alias is never itself an import.
        assert!(has(&e, EdgeKind::Imports, "Account"), "aliased import target: {e:?}");
        assert!(!has(&e, EdgeKind::Imports, "Acct"), "alias is not an import target: {e:?}");
    }
}
