//! Python graph-edge extraction, co-located with Python's parser and resolver policy.
//! `python_edges` walks the CST. Its private helpers recognize PEP 604 unions, static base types,
//! and relative imports.
use std::path::Path;

use tree_sitter::Node;

use crate::index::edges::*;

pub(in crate::index::languages) fn python_edges(
    EdgeVisit { text, node, symbols: _, path, locator }: EdgeVisit<'_, '_, '_>,
    out: &mut EdgeEmitter<'_>,
) {
    match node.kind() {
        "import_from_statement" => python_from_import_edges(text, node, path, out),
        "import_statement" => python_import_statement_edges(text, node, path, out),
        "call" => python_call_edges(text, node, locator, out),
        "class_definition" => python_class_edges(text, node, locator, out),
        // Type annotations (`x: T`, `-> T`) wrap their type in a `type` node.
        // `emit_python_type_refs` walks the whole type expression — generics (`Box[Item]`),
        // qualified generics (`typing.Optional[Api]`), unions (`A | B`), nested
        // (`Optional[list[Api]]`), `Callable` param lists — emitting a ReferencesType per
        // referenced type. The alias NAME in a `type X = …` is skipped (it's a definition,
        // not a reference). String forward refs (`-> "Api"`) carry no identifier → no edge.
        "type" if !python_is_type_alias_name(node) => {
            emit_python_type_refs(node, locator, text, out);
        },
        "decorator" => python_decorator_edges(text, node, locator, out),
        _ => {},
    }
}

/// `from <module> import <name|name as alias>, ...` — emit Imports edges to the MODULE and to each
/// imported NAME, never the local alias. A relative import (`.sessions`) normalizes to its dotted
/// tail (the leading dots aren't identifiers), so the module name is recorded separately from any
/// `as` alias.
fn python_from_import_edges(text: &str, node: Node<'_>, path: &Path, out: &mut EdgeEmitter<'_>) {
    let module = node.child_by_field_name("module_name");
    if let Some(module) = module
        && let Some(name) = last_identifier_text(module, text)
    {
        out.push(file_edge(path, module, text, name, EdgeKind::Imports));
    }
    // Record an alias for the rebind ONLY when the from-import is BOTH module-bound AND
    // RELATIVE (`from .compat import X as Y`). Two gates:
    //  - module-bound: a `def`/`class`-nested import binds the alias only in that local scope, so a
    //    whole-file alias scope would rebind unrelated same-name references; it binds file-wide at
    //    top level and inside transparent `if`/`try`/`with`/`for` blocks.
    //  - relative: a relative import provably names an IN-CORPUS sibling module, so rebinding its
    //    alias to the in-corpus target is safe. An ABSOLUTE import (`from urllib3.util import
    //    Timeout as TimeoutSauce`) is usually EXTERNAL — rebinding `TimeoutSauce` → bare `Timeout`
    //    would mis-bind to a same-named LOCAL class (`requests.exceptions .Timeout`), a real
    //    precision regression measured on psf/requests (#174 review). Distinguishing
    //    absolute-in-corpus from absolute-external needs a Python package model we don't have;
    //    relative is the correct-by-construction in-corpus subset.
    // A non-recorded alias still emits its plain target Imports edge (dependency captured).
    let record_alias = is_python_module_bound(node) && python_from_import_is_relative(node, text);
    let import_start = node.start_byte();
    // The module root is needed to bound the alias's scope at the next module-scope
    // rebinding of the alias name (#174 review) — see `python_import_target`.
    let module_root = record_alias.then(|| python_module_root(node)).flatten();
    let module_id = module.map(|m| m.id());
    for child in named_children(node) {
        if Some(child.id()) == module_id {
            continue;
        }
        python_import_target(child, text, path, record_alias, import_start, module_root, out);
    }
}

/// `import <module>` / `import <module> as alias` — Imports edge to the module, not the alias.
fn python_import_statement_edges(
    text: &str,
    node: Node<'_>,
    path: &Path,
    out: &mut EdgeEmitter<'_>,
) {
    for child in named_children(node) {
        python_import_target(child, text, path, false, node.start_byte(), None, out);
    }
}

/// Function / method / constructor call. Mirror the C handler: the callee is the LAST identifier
/// under the `function` child (`f()` → `f`, `obj.method()` → `method`), the receiver is the first
/// (recorded only as a NameOnly hint — never claimed as exact; resolving it is the oracle's job,
/// not the heuristic's).
fn python_call_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let function = node.child_by_field_name("function").unwrap_or(node);
    let identifiers = IdentifierPath::under(function, text);
    // `handlers[key]()` — the callee is the subscript RESULT, not the index variable
    // `last()` would pick. There's no clean callee identifier, so emit nothing (a wrong
    // `calls_name key` is worse than a missing edge).
    if function.kind() == "subscript" {
        // fall through to recursion without emitting a call edge
    } else if let Some(name) = identifiers.last_text().map(ToOwned::to_owned) {
        out.push(symbol_edge_with_context(
            locator,
            node,
            Some(text),
            name,
            EdgeKind::CallsName,
            EdgeContext {
                target_qualified_name: identifiers.qualified_name(),
                receiver_hint: identifiers
                    .first_text()
                    .filter(|_| identifiers.len() > 1)
                    .map(ToOwned::to_owned),
                ..Default::default()
            },
            identifiers.last_node().map(CalleeRange::of_node),
        ));
    }
}

/// `class Foo(Base, Generic[T], metaclass=Meta)` — each POSITIONAL base is an Implements +
/// ReferencesType edge. Keyword (`metaclass=`) and splat (`*bases`/`**kw`) arguments are not
/// superclasses, and a parameterized base resolves to its head (`Generic`, not `T`).
fn python_class_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(supers) = node.child_by_field_name("superclasses") else {
        return;
    };
    for base in named_children(supers) {
        if matches!(base.kind(), "keyword_argument" | "list_splat" | "dictionary_splat") {
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
            let callee =
                last_identifier_node(head).map(final_segment_node).map(CalleeRange::of_node);
            out.push(symbol_edge(locator, base, name, EdgeKind::Implements, callee));
        }
        emit_python_type_refs(base, locator, text, out);
    }
}

/// A bare decorator (`@requires_auth`, `@pytest.fixture`) is an identifier/attribute, not a `call`
/// — applying it is a call-like dependency, so emit a NameOnly call edge. A parenthesized
/// decorator (`@foo(...)`) is a `call` child, already handled by the call arm via recursion.
fn python_decorator_edges(
    text: &str,
    node: Node<'_>,
    locator: &SymbolLocator<'_>,
    out: &mut EdgeEmitter<'_>,
) {
    let Some(inner) = node.named_child(0) else {
        return;
    };
    if !matches!(inner.kind(), "identifier" | "attribute") {
        return;
    }
    let identifiers = IdentifierPath::under(inner, text);
    let Some(name) = identifiers.last_text().map(ToOwned::to_owned) else {
        return;
    };
    // Preserve the qualifier so a qualified decorator (`@pytest.fixture`) carries its
    // `pytest` receiver + dotted path — same context the call arm records — so the
    // resolver doesn't fall back to a bare local `fixture` of the same name.
    out.push(symbol_edge_with_context(
        locator,
        node,
        Some(text),
        name,
        EdgeKind::CallsName,
        EdgeContext {
            target_qualified_name: identifiers.qualified_name(),
            receiver_hint: identifiers
                .first_text()
                .filter(|_| identifiers.len() > 1)
                .map(ToOwned::to_owned),
            ..Default::default()
        },
        identifiers.last_node().map(final_segment_node).map(CalleeRange::of_node),
    ));
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
    locator: &SymbolLocator<'_>,
    text: &str,
    out: &mut EdgeEmitter<'_>,
) {
    match node.kind() {
        "type" | "generic_type" | "subscript" | "binary_operator" | "list" | "tuple"
        | "type_parameter" => {
            // grow_stack: a deeply-nested annotation (a PEP 604 union `A|A|…`, nested generics)
            // recurses to full subtree depth here — grow rather than overflow (#543).
            rag_rat_base::stack::grow_stack(|| {
                for child in named_children(node) {
                    emit_python_type_refs(child, locator, text, out);
                }
            });
        },
        "identifier" =>
            if let Some(name) = last_identifier_text(node, text) {
                out.push(symbol_edge(
                    locator,
                    node,
                    name,
                    EdgeKind::ReferencesType,
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
        "attribute" | "dotted_name" => {
            let identifiers = IdentifierPath::under(node, text);
            if let Some(name) = identifiers.last_text().map(ToOwned::to_owned) {
                let receiver = node
                    .child_by_field_name("object")
                    .map(|object| node_text(object, text))
                    .or_else(|| identifiers.first_text().map(ToOwned::to_owned));
                out.push(symbol_edge_with_context(
                    locator,
                    node,
                    None,
                    name,
                    EdgeKind::ReferencesType,
                    EdgeContext {
                        receiver_hint: receiver,
                        target_qualified_name: identifiers.qualified_name(),
                        ..Default::default()
                    },
                    identifiers.last_node().map(final_segment_node).map(CalleeRange::of_node),
                ));
            }
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
    for child in named_children(module) {
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
            rag_rat_base::stack::grow_stack(|| python_rebinding_effective_byte(inner, name, text))
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
        "delete_statement" => named_children(node)
            .any(|target| python_assignment_target_binds(target, name, text))
            .then(|| node.start_byte()),
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
        | "list_splat_pattern" | "expression_list" => rag_rat_base::stack::grow_stack(|| {
            // grow_stack: nested unpacking (`a, (b, (c, …))`) recurses to full depth (#543).
            named_children(target)
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
    rag_rat_base::stack::grow_stack(|| {
        named_children(node).any(|child| {
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
    out: &mut EdgeEmitter<'_>,
) {
    if child.kind() == "import_list" {
        // grow_stack: uniform depth guard for a tree descender (#543); `import_list` doesn't nest
        // deeply today, so this is a no-op fast path, but the invariant stays uniform.
        rag_rat_base::stack::grow_stack(|| {
            for clause in named_children(child) {
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
        out.push(file_edge(path, target, text, name, EdgeKind::Imports));
    }
}

#[cfg(test)]
#[path = "edges_tests.rs"]
mod python_edge_tests;
