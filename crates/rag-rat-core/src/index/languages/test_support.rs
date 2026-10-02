//! One source fixture set per structural language, behind an exhaustive `match` on [`Language`].
//!
//! Per-language tests that loop over a hand-written list silently skip a language nobody added to
//! the list. Driving them from [`fixture`] makes a new `Language` variant fail to compile until it
//! has fixtures, and so reach every test that consumes them.

use rag_rat_base::language::Language;

/// Sources exercising one language's parser, edge walk and embedding-policy classifier.
pub(crate) struct LanguageFixture {
    /// A file path with the language's extension.
    pub(crate) path: &'static str,
    /// Imports and comments only, at least 80 chars: the low-signal (plumbing) span case.
    pub(crate) plumbing: &'static str,
    /// One real definition, at least 80 chars: the signal span case.
    pub(crate) definition: &'static str,
    /// A source that parses with an ERROR node around a call to `target`.
    pub(crate) malformed: &'static str,
    /// The edges the walk recovers from `malformed` by descending into its ERROR nodes.
    pub(crate) malformed_recovered: &'static [(&'static str, &'static str)],
    /// A broken declaration followed by declarations that parse whole, some inside a container:
    /// the case the symbol walk's ERROR recovery exists for.
    pub(crate) broken_declaration: &'static str,
    /// Every symbol parsed from `broken_declaration`, as `(kind, scope path)` in source order.
    pub(crate) broken_declaration_symbols: &'static [(&'static str, &'static str)],
    /// Value bindings, types and functions declared at file level, as members, and inside
    /// function bodies and closures: the case the local-variable policy decides. In a language
    /// with local variables, every `function_scopes` kind wraps one here with no other
    /// function scope around it, so dropping any one kind changes the parse.
    pub(crate) local_declarations: &'static str,
    /// Every symbol parsed from `local_declarations`, as `(kind, scope path)` in source order. A
    /// local variable is absent; any other declaration in a function body keeps its base path.
    pub(crate) local_declarations_symbols: &'static [(&'static str, &'static str)],
    /// The text before and after a callee `g` wrapped in nested parentheses, forming a function
    /// named `deep_marker_fn`: see [`Self::deep_call`].
    deep_call: (&'static str, &'static str),
}

impl LanguageFixture {
    /// `deep_marker_fn` calling a callee inside `depth` nested parentheses: a tree thousands of
    /// nodes deep for the stack-safety tests.
    pub(crate) fn deep_call(&self, depth: usize) -> String {
        let (before, after) = self.deep_call;
        format!("{before}{}g{}{after}", "(".repeat(depth), ")".repeat(depth))
    }
}

/// The fixtures for `language`, or `None` for a language with no structural parse (Markdown).
pub(crate) fn fixture(language: Language) -> Option<LanguageFixture> {
    Some(match language {
        Language::Rust => LanguageFixture {
            path: "s.rs",
            plumbing: "use std::collections::HashMap;\nuse std::fmt::Debug;\n// a descriptive \
                       comment line\nuse std::io::Read;\nuse std::sync::Arc;\n",
            definition:
                "pub fn real_function(input: i32) -> i32 {\n    let value = input + 1;\n    \
                 println!(\"{}\", value);\n    another_call(value)\n}\n",
            malformed: "fn f() { if { target(); } }\n",
            malformed_recovered: &[("calls_name", "target")],
            // tree-sitter-rust repairs the header with a MISSING `)`: no ERROR node forms.
            broken_declaration: "fn ok(){}\nfn broken( { target(); }\nfn after(){}\nimpl K { fn \
                                 m(){} }\n",
            broken_declaration_symbols: &[
                ("function", "ok"),
                ("function", "broken"),
                ("function", "after"),
                ("impl", "K"),
                ("function", "K::m"),
            ],
            local_declarations: "const TOP: i32 = 1;\nfn f() {\n    let x = 1;\n    const C: i32 \
                                 = 2;\n    static S: i32 = 3;\n    struct Local;\n    impl Local \
                                 { fn lm() {} }\n    impl K { fn km() {} }\n    fn nested() { \
                                 impl Local { fn nm() {} } }\n    mod inner { pub struct Q; }\n    \
                                 impl inner::Q { fn z() {} }\n    let c = || { const IN_CLOSURE: \
                                 i32 = 4; struct InClosure; };\n}\nimpl K {\n    const ASSOC: i32 \
                                 = 5;\n    fn m() { struct MethodLocal; }\n}\n",
            // Rust indexes no `let`, and a `const` or `static` in a function body is an item, not a
            // variable: nothing is dropped. Every local declaration keeps its base path (#1496).
            local_declarations_symbols: &[
                ("const", "TOP"),
                ("function", "f"),
                ("const", "C"),
                ("static", "S"),
                ("struct", "Local"),
                ("impl", "Local"),
                ("function", "Local::lm"),
                ("impl", "K"),
                ("function", "K::km"),
                ("function", "nested"),
                ("impl", "Local"),
                ("function", "Local::nm"),
                ("module", "inner"),
                ("struct", "inner::Q"),
                ("impl", "inner::Q"),
                ("function", "inner::Q::z"),
                ("const", "IN_CLOSURE"),
                ("struct", "InClosure"),
                ("impl", "K"),
                ("const", "K::ASSOC"),
                ("function", "K::m"),
                ("struct", "K::MethodLocal"),
            ],
            deep_call: ("fn deep_marker_fn() { ", "(); }\n"),
        },
        Language::TypeScript => LanguageFixture {
            path: "s.ts",
            plumbing: "import defaultThing from 'a';\n// a descriptive comment line here \
                       now\nimport { namedThing } from 'b';\nimport * as ns from 'c';\n",
            definition: "export function realFunction(input: number): number {\n    const value = \
                         input + 1;\n    return value + compute(value);\n}\n",
            malformed: "function broken( { target(); }\n",
            malformed_recovered: &[],
            // The rest of the file after `broken(` becomes a top-level ERROR. `K` parses whole
            // beneath it and is recovered with its method. `after` is not: the parser folded its
            // name into a malformed method header, so no declaration node for it exists.
            // Recovering it needs text-level recovery, tracked in #1486.
            broken_declaration: "function ok(){}\nfunction broken( { target(); }\nfunction \
                                 after(){}\nclass K { m(){} }\n",
            broken_declaration_symbols: &[("function", "ok"), ("class", "K"), ("function", "K::m")],
            local_declarations: "const TOP = 1;\nclass K {\n  field = 1;\n  m() { const x = 1; \
                                 class MethodLocal {} }\n}\nfunction f() {\n  let x = 1;\n  class \
                                 Local {}\n  function nested() {}\n  const g = () => { var \
                                 inClosure = 1; };\n  abstract class D { abstract q: number; w = \
                                 1 }\n  const E = class { z = 1 };\n}\nfunction* gen() { let \
                                 inGen = 1; }\nconst fe = function () { let inFe = 1; };\nconst \
                                 ge = function* () { let inGe = 1; };\nconst af = () => { let \
                                 inArrow = 1; };\nclass S { static { let inStatic = 1; } }\n",
            // Every local variable is dropped; a local class or function keeps its base path
            // (#1496), and so does a member of a class body in a function, which is no local.
            local_declarations_symbols: &[
                ("const", "TOP"),
                ("class", "K"),
                ("const", "K::field"),
                ("function", "K::m"),
                ("class", "K::MethodLocal"),
                ("function", "f"),
                ("class", "Local"),
                ("function", "nested"),
                ("const", "q"),
                ("const", "w"),
                ("const", "z"),
                ("function", "gen"),
                ("const", "fe"),
                ("const", "ge"),
                ("const", "af"),
                ("class", "S"),
            ],
            deep_call: ("function deep_marker_fn() { ", "(); }\n"),
        },
        Language::Kotlin => LanguageFixture {
            path: "s.kt",
            plumbing: "package com.example.app\nimport kotlin.collections.List\n// a descriptive \
                       comment line here now\nimport kotlin.io.println\n",
            definition: "fun realFunction(input: Int): Int {\n    val value = input + 1\n    \
                         return value + compute(value)\n}\n",
            malformed: "fun broken( { target() }\n",
            malformed_recovered: &[],
            // kotlin-ng cannot parse a member on its class's opening line: `f` lands in an ERROR
            // inside the class body and is recovered with its class scope.
            broken_declaration: "class A { fun f() {} }\nfun after() {}\n",
            broken_declaration_symbols: &[
                ("class", "A"),
                ("function", "A::f"),
                ("function", "after"),
            ],
            local_declarations: "val top = 1\nclass K {\n  val field = 1\n  fun m() {\n    val x \
                                 = 1\n    class MethodLocal\n  }\n}\nfun f() {\n  val x = 1\n  \
                                 class Local\n  fun nested() {}\n  val g = { val inLambda = 1 }\n  \
                                 val o = object { val member = 1 }\n}\nval lam = { val inLambda = \
                                 1 }\nval anon = fun() { val inAnon = 1 }\nclass C() {\n  \
                                 constructor(a: Int) : this() { val inCtor = 1 }\n  init { val \
                                 inInit = 1 }\n  val p: Int\n    get() { val inGetter = 1; return \
                                 1 }\n  var q: Int = 0\n    set(v) { val inSetter = v }\n}\n",
            // Every local variable is dropped; a local class or function keeps its base path
            // (#1496), and so does a member of an object literal in a function, which is no local.
            local_declarations_symbols: &[
                ("property", "top"),
                ("class", "K"),
                ("property", "K::field"),
                ("function", "K::m"),
                ("class", "K::MethodLocal"),
                ("function", "f"),
                ("class", "Local"),
                ("function", "nested"),
                ("property", "member"),
                ("property", "lam"),
                ("property", "anon"),
                ("class", "C"),
                ("property", "C::p"),
                ("property", "C::q"),
            ],
            deep_call: ("fun deep_marker_fn() { ", "() }\n"),
        },
        Language::C => LanguageFixture {
            path: "s.c",
            plumbing: "#include <stdio.h>\n#include \"local_header.h\"\n// a descriptive comment \
                       line here now goes on\n#include <string.h>\n",
            definition: "int real_function(int input) {\n    int value = input + 1;\n    return \
                         value + compute(value);\n}\n",
            malformed: "void broken( { target(); }\n",
            malformed_recovered: &[("calls_name", "target")],
            // The conditional splits `f` across its branches and the file becomes one ERROR.
            // `after` parses whole beneath it; the second `f`, whose body closes the other
            // branch, is not taken for a definition.
            broken_declaration:
                "#ifdef X\nint f(int a) {\n#else\nint f(int a, int b) {\n#endif\n  return \
                 a;\n}\nint after(void){ return 0; }\n",
            broken_declaration_symbols: &[("function", "after")],
            local_declarations: "int top = 1;\nstruct S { int a; };\nvoid f(void) {\n  int x = \
                                 1;\n  struct Local { int a; };\n}\n",
            // C declares no variable, at file level or local, as a symbol. A local type keeps its
            // base path (#1496).
            local_declarations_symbols: &[("struct", "S"), ("function", "f"), ("struct", "Local")],
            deep_call: ("void deep_marker_fn() { ", "(); }\n"),
        },
        Language::Cpp => LanguageFixture {
            path: "s.cpp",
            plumbing: "#include <vector>\n#include <string>\n// a descriptive comment line here \
                       now goes on and on\n#include <memory>\n",
            definition: "int real_function(int input) {\n    int value = input + 1;\n    return \
                         value + compute(value);\n}\n",
            malformed: "void broken( { target(); }\n",
            malformed_recovered: &[("calls_name", "target")],
            // As for C.
            broken_declaration:
                "#ifdef X\nint f(int a) {\n#else\nint f(int a, int b) {\n#endif\n  return \
                 a;\n}\nint after(void){ return 0; }\n",
            broken_declaration_symbols: &[("function", "after")],
            local_declarations: "int top = 1;\nclass K {\n  int field;\n  void m() { struct \
                                 MethodLocal {}; }\n};\nvoid f() {\n  int x = 1;\n  class Local \
                                 {};\n  auto g = []() { struct InLambda {}; };\n}\n",
            // As for C, no variable is a symbol, and a local type keeps its base path (#1496).
            local_declarations_symbols: &[
                ("class", "K"),
                ("function", "K::m"),
                ("struct", "K::MethodLocal"),
                ("function", "f"),
                ("class", "Local"),
                ("struct", "InLambda"),
            ],
            deep_call: ("void deep_marker_fn() { ", "(); }\n"),
        },
        Language::Python => LanguageFixture {
            path: "s.py",
            plumbing: "import os\nimport sys\nfrom collections import defaultdict\n# a \
                       descriptive comment line here now goes on\n",
            definition: "def real_function(input):\n    value = input + 1\n    result = value + \
                         compute(value)\n    return result\n",
            malformed: "def broken(:\n    target()\n",
            malformed_recovered: &[],
            // tree-sitter-python repairs the header with a MISSING `)`: no ERROR node forms.
            broken_declaration: "def ok(): pass\ndef broken(:\n    target()\ndef after(): \
                                 pass\nclass K:\n    def m(self): pass\n",
            broken_declaration_symbols: &[
                ("function", "ok"),
                ("function", "broken"),
                ("function", "after"),
                ("class", "K"),
                ("function", "K::m"),
            ],
            local_declarations: "TOP = 1\nclass K:\n    LIMIT = 2\n    def m(self):\n        \
                                 class MethodLocal: pass\ndef f():\n    X = 1\n    class Local: \
                                 pass\n    def nested(): pass\n    g = lambda: 1\n",
            // Python already dropped local variables and scoped local definitions under their
            // function.
            local_declarations_symbols: &[
                ("const", "TOP"),
                ("class", "K"),
                ("const", "K::LIMIT"),
                ("function", "K::m"),
                ("class", "K::m::MethodLocal"),
                ("function", "f"),
                ("class", "f::Local"),
                ("function", "f::nested"),
            ],
            deep_call: ("def deep_marker_fn():\n    ", "()\n"),
        },
        Language::Swift => LanguageFixture {
            path: "s.swift",
            plumbing: "import Foundation\nimport Dispatch\n// a descriptive comment line here now \
                       goes on long enough\nimport Observation\n",
            definition: "func realFunction(_ input: Int) -> Int {\n    let value = input + 1\n    \
                         return value + compute(value)\n}\n",
            malformed: "func f() { if { target() } }\n",
            malformed_recovered: &[("calls_name", "target")],
            // tree-sitter-swift repairs the header with a MISSING `)`: no ERROR node forms.
            broken_declaration: "func ok(){}\nfunc broken( { target() }\nfunc after(){}\nclass K \
                                 { func m(){} }\n",
            broken_declaration_symbols: &[
                ("function", "ok"),
                ("function", "broken"),
                ("function", "after"),
                ("class", "K"),
                ("function", "K::m"),
            ],
            local_declarations: "let top = 1\nclass K {\n  var field = 1\n  func m() {\n    let x \
                                 = 1\n    class MethodLocal {}\n  }\n}\nfunc f() {\n  let x = 1\n  \
                                 class Local {}\n  func nested() {}\n  let g = { let inClosure = \
                                 1 }\n}\nlet lam = { let inLambda = 1 }\nvar short: Int { let \
                                 inShort = 1; return inShort }\nclass C {\n  init() { let inInit \
                                 = 1 }\n  deinit { let inDeinit = 1 }\n  subscript(i: Int) -> Int \
                                 { struct InSub {}; return i }\n  var sp: Int { let inShort = 1; \
                                 return inShort }\n  var cp: Int {\n    get { let inGet = 1; \
                                 return 1 }\n    set { let inSet = 1 }\n    _modify { let \
                                 inModify = 1 }\n  }\n  var ob: Int = 0 {\n    willSet { let \
                                 inWill = 1 }\n    didSet { let inDid = 1 }\n  }\n}\n",
            // Every local variable is dropped; a local type or function keeps its base path, which
            // Swift already scopes under its function (#1496).
            local_declarations_symbols: &[
                ("property", "top"),
                ("class", "K"),
                ("property", "K::field"),
                ("function", "K::m"),
                ("class", "K::m::MethodLocal"),
                ("function", "f"),
                ("class", "f::Local"),
                ("function", "f::nested"),
                // Each of these bodies is the only function scope around its local.
                ("property", "lam"),
                ("property", "short"),
                ("class", "C"),
                ("constructor", "C::init"),
                ("function", "C::deinit"),
                ("function", "C::subscript"),
                // A subscript names its body, which is a computed property.
                ("struct", "C::subscript::InSub"),
                ("property", "C::sp"),
                ("property", "C::cp"),
                ("property", "C::ob"),
            ],
            deep_call: ("func deep_marker_fn() { ", "() }\n"),
        },
        Language::Go => LanguageFixture {
            path: "s.go",
            plumbing: "import (\n\t\"fmt\"\n\t\"os\"\n)\n// a descriptive comment line here now \
                       goes on long enough\nimport \"strings\"\n",
            definition: "func realFunction(input int) int {\n\tvalue := input + 1\n\treturn value \
                         + compute(value)\n}\n",
            malformed: "func broken( { target() }\n",
            malformed_recovered: &[],
            // tree-sitter-go repairs the header with a MISSING `)`: no ERROR node forms.
            broken_declaration: "package p\nfunc ok(){}\nfunc broken( { target() }\nfunc \
                                 after(){}\ntype K struct{}\nfunc (k K) m(){}\n",
            broken_declaration_symbols: &[
                ("function", "ok"),
                ("function", "broken"),
                ("function", "after"),
                ("struct", "K"),
                ("method", "K.m"),
            ],
            local_declarations: "package p\nvar Top = 1\nconst (\n\tA = 1\n)\nfunc f() {\n\tvar x \
                                 = 1\n\tconst c = 2\n\ttype local struct{}\n\tg := func() { var \
                                 inClosure = 3 }\n}\ntype K struct{ F int }\nfunc (k K) m() \
                                 {\n\tvar inMethod = 1\n\ttype methodLocal int\n}\nvar fl = \
                                 func() { var inLiteral = 1 }\n",
            // Every local variable is dropped; a local type keeps its base path (#1496). Go has no
            // named nested function, and a struct field is not a symbol.
            local_declarations_symbols: &[
                ("var", "Top"),
                ("const", "A"),
                ("function", "f"),
                ("struct", "local"),
                ("struct", "K"),
                ("method", "K.m"),
                ("type", "methodLocal"),
                ("var", "fl"),
            ],
            deep_call: ("func deep_marker_fn() { ", "() }\n"),
        },
        Language::Markdown => return None,
    })
}

/// Every language with fixtures, with them.
pub(crate) fn fixtures() -> impl Iterator<Item = (Language, LanguageFixture)> {
    Language::all().iter().filter_map(|&language| Some((language, fixture(language)?)))
}

/// One extracted edge as a regression test pins it: kind, target name, written qualified target
/// and receiver hint.
pub(crate) type EdgeFact = (crate::index::edges::EdgeKind, String, Option<String>, Option<String>);

/// The syntactic edges of `kind` extracted from `source`, in walk order.
pub(crate) fn edge_facts(
    path: &str,
    language: Language,
    source: &str,
    kind: crate::index::edges::EdgeKind,
) -> Vec<EdgeFact> {
    crate::index::edges::syntactic_edges(std::path::Path::new(path), language, source, &[])
        .expect("edge extraction")
        .into_iter()
        .filter(|edge| edge.edge_kind == kind)
        .map(|edge| (edge.edge_kind, edge.to_name, edge.target_qualified_name, edge.receiver_hint))
        .collect()
}

/// An [`EdgeFact`] spelled with string literals.
pub(crate) fn fact(
    kind: crate::index::edges::EdgeKind,
    name: &str,
    qualified: Option<&str>,
    receiver: Option<&str>,
) -> EdgeFact {
    (kind, name.to_string(), qualified.map(ToOwned::to_owned), receiver.map(ToOwned::to_owned))
}
