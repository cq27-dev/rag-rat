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
    assert_eq!(call.confidence, rag_rat_db::EdgeConfidence::NameOnly);
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
    assert_eq!(call.confidence, rag_rat_db::EdgeConfidence::NameOnly);
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
    assert!(!has(&e, EdgeKind::Implements, "Meta"), "metaclass kwarg must not be a base: {e:?}");
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
    assert!(!has(&e, EdgeKind::CallsName, "key"), "index var wrongly recorded as callee: {e:?}");
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
    assert!(has(&e, EdgeKind::CallsName, "requires_auth"), "bare decorator edge missing: {e:?}");
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
    assert!(has(&e, EdgeKind::ReferencesType, "str"), "annotation ReferencesType missing: {e:?}");
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
    assert!(has(&e, EdgeKind::Imports, "Timeout"), "the dependency edge is still emitted: {e:?}");
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
    let e = edges("def load():\n    from .models import User as Account\n    return Account()\n");
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
    let src = "from .models import User as Account\nif flag:\n    Account = other()\nAccount()\n";
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
    let src = "from .models import User as Account\ndef f():\n    class Account:\n        pass\n";
    let e = edges(src);
    assert_eq!(alias_scope_end(&e, "User"), src.len(), "a nested redef must not shrink scope");
}

#[test]
fn alias_scope_ignores_an_attribute_assignment() {
    // `Account.config = x` mutates an attribute; it does NOT rebind `Account` itself, so the
    // alias scope must not stop there.
    let src = "from .models import User as Account\nAccount.config = 1\nAccount()\n";
    let e = edges(src);
    assert_eq!(alias_scope_end(&e, "User"), src.len(), "an attribute set must not shrink scope");
}

#[test]
fn alias_scope_starts_after_a_definition_before_the_import() {
    // A `class Account` BEFORE the import does not shadow it (the import is the later binding):
    // scope_start is the import, scope_end runs to EOF.
    let src = "class Account:\n    pass\n\n\nfrom .models import User as Account\nAccount()\n";
    let e = edges(src);
    let scope = aliased_import(&e, "User").and_then(|edge| edge.import_scope).expect("scope");
    assert_eq!(scope.scope_start, src.find("from .models").unwrap(), "scope starts at the import");
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
        "from mod import Account as Acct\n(a, (b, c)) = g()\nresult\nimport Acct.deep\nAcct = 5\n",
    );
    // Running the scan is the point (it walks the wrapped helpers on normal input); the import
    // target is still recorded, and the local `as` alias is never itself an import.
    assert!(has(&e, EdgeKind::Imports, "Account"), "aliased import target: {e:?}");
    assert!(!has(&e, EdgeKind::Imports, "Acct"), "alias is not an import target: {e:?}");
}
