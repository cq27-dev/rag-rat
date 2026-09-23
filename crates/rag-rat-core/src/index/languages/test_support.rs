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
            deep_call: ("func deep_marker_fn() { ", "() }\n"),
        },
        Language::Markdown => return None,
    })
}

/// Every language with fixtures, with them.
pub(crate) fn fixtures() -> impl Iterator<Item = (Language, LanguageFixture)> {
    Language::all().iter().filter_map(|&language| Some((language, fixture(language)?)))
}
