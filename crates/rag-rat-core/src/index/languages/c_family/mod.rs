use std::path::Path;

use tree_sitter::Node;

use super::{
    ErrorRecovery, ParserBackend, RecoveryContext, ResolutionPolicy, SymbolMatch, TypeBinding,
};
use crate::index::edges::IdentifierPath;
use crate::index::parser::{self, ParserKind};

mod edges;
pub(super) use edges::c_like_edges;

/// The node kinds that name a declaration when it has no `name` field (`parser::child_name`).
const NAME_KINDS: &[&str] =
    &["identifier", "type_identifier", "field_identifier", "namespace_identifier"];

/// The node kinds the identifier helpers ([`crate::index::edges::identifiers_under`] and friends)
/// collect.
const IDENTIFIER_KINDS: &[&str] = NAME_KINDS;
#[cfg(test)]
mod query_spike;

pub(super) static C_SUPPORT: C = C;
pub(super) static CPP_SUPPORT: Cpp = Cpp;

pub(super) struct C;
pub(super) struct Cpp;

/// A conditional that splits one definition across its branches (`#ifdef X` / `int f(int a) {` /
/// `#else` / `int f(int a, int b) {` / `#endif` / shared body) leaves the parser one unbalanced
/// run of text, and the rest of the file becomes one ERROR. The definitions after it still parse
/// whole; the split one is rejected by [`is_stray_conditional_directive`].
static C_ERROR_RECOVERY: ErrorRecovery = ErrorRecovery {
    file_root: "translation_unit",
    contexts: &[RecoveryContext {
        kind: "translation_unit",
        within: &[],
        legal: &[
            "function_definition",
            "type_definition",
            "struct_specifier",
            "union_specifier",
            "enum_specifier",
            "preproc_function_def",
        ],
    }],
    container_keywords: &["struct", "union"],
};

const CPP_TOP_LEVEL_DECLARATIONS: &[&str] = &[
    "function_definition",
    "template_declaration",
    "type_definition",
    "alias_declaration",
    "namespace_definition",
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
    "preproc_function_def",
];
const CPP_MEMBER_DECLARATIONS: &[&str] = &[
    "function_definition",
    "template_declaration",
    "type_definition",
    "alias_declaration",
    "class_specifier",
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
];

/// As for C. `declaration_list` is a namespace or `extern "C"` body, where top-level declarations
/// are legal; `field_declaration_list` is a class, struct or union body.
static CPP_ERROR_RECOVERY: ErrorRecovery = ErrorRecovery {
    file_root: "translation_unit",
    contexts: &[
        RecoveryContext {
            kind: "translation_unit",
            within: &[],
            legal: CPP_TOP_LEVEL_DECLARATIONS,
        },
        RecoveryContext {
            kind: "declaration_list",
            within: &[],
            legal: CPP_TOP_LEVEL_DECLARATIONS,
        },
        RecoveryContext {
            kind: "field_declaration_list",
            within: &[],
            legal: CPP_MEMBER_DECLARATIONS,
        },
    ],
    container_keywords: &["namespace", "class", "struct", "union"],
};

impl ParserBackend for C {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["enum", "function", "macro", "struct", "type", "union"]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::C
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        // Index definitions, not prototypes/forward declarations/uses: otherwise type-reference
        // edges bind to the tiny declaration occurrence instead of the real definition (#61).
        match node.kind() {
            "function_definition" => Some(("function", declarator_name(node)?)),
            "struct_specifier" if has_body(node) =>
                Some(("struct", node.child_by_field_name("name")?)),
            "union_specifier" if has_body(node) =>
                Some(("union", node.child_by_field_name("name")?)),
            "enum_specifier" if has_body(node) => Some(("enum", node.child_by_field_name("name")?)),
            "type_definition" => Some(("type", declarator_name(node)?)),
            "preproc_function_def" => Some(("macro", parser::child_name(node, NAME_KINDS)?)),
            _ => None,
        }
    }

    fn error_recovery(&self) -> Option<&'static ErrorRecovery> {
        Some(&C_ERROR_RECOVERY)
    }

    fn marks_split_declaration(&self, node: Node<'_>, text: &str) -> bool {
        is_stray_conditional_directive(node, text)
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        match node.kind() {
            "struct_specifier" | "union_specifier" if has_body(node) =>
                parser::node_text(node.child_by_field_name("name")?, text),
            _ => None,
        }
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "preproc_include"
    }
}

impl ParserBackend for Cpp {
    fn symbol_kinds(&self) -> &'static [&'static str] {
        &["class", "enum", "function", "macro", "namespace", "struct", "type", "union"]
    }

    fn parser_kind(&self, _path: &Path) -> ParserKind {
        ParserKind::Cpp
    }

    fn symbol_node<'tree>(&self, node: Node<'tree>, _text: &str) -> Option<SymbolMatch<'tree>> {
        // As for C, bodyless declarations are deliberately not symbols (#61).
        match node.kind() {
            "function_definition" => Some(("function", declarator_name(node)?)),
            "class_specifier" if has_body(node) =>
                Some(("class", node.child_by_field_name("name")?)),
            "struct_specifier" if has_body(node) =>
                Some(("struct", node.child_by_field_name("name")?)),
            "union_specifier" if has_body(node) =>
                Some(("union", node.child_by_field_name("name")?)),
            "enum_specifier" if has_body(node) => Some(("enum", node.child_by_field_name("name")?)),
            "type_definition" => Some(("type", declarator_name(node)?)),
            "alias_declaration" => Some(("type", node.child_by_field_name("name")?)),
            "namespace_definition" => Some(("namespace", node.child_by_field_name("name")?)),
            "preproc_function_def" => Some(("macro", parser::child_name(node, NAME_KINDS)?)),
            _ => None,
        }
    }

    fn for_each_declared_name<'tree>(
        &self,
        node: Node<'tree>,
        text: &str,
        emit: &mut dyn FnMut(Node<'tree>),
    ) {
        match node.kind() {
            // `template<typename T>` / `template<class... Ts>`: the binder has no field.
            "type_parameter_declaration" | "variadic_type_parameter_declaration" =>
                crate::index::edges::named_children(node)
                    .filter(|child| child.kind() == "type_identifier")
                    .for_each(emit),
            "optional_type_parameter_declaration" =>
                node.child_by_field_name("name").into_iter().for_each(emit),
            // An out-of-line definition (`struct Outer::Fwd {}`) names itself by its path's last
            // segment, the token its type reference is spelled by. A specialization
            // (`template<> struct ns::Box<int> {}`) does not: its `Box` is a use of the primary
            // template, as the unqualified spelling `Box<int>` already is.
            _ => self.for_each_declared_symbol_name(node, text, &mut |name| {
                emit(name);
                if name.kind() == "qualified_identifier"
                    && let Some(tail) =
                        IdentifierPath::member_chain(name, text, IDENTIFIER_KINDS).last_node()
                    && tail.parent().is_none_or(|parent| parent.kind() != "template_type")
                {
                    emit(tail);
                }
            }),
        }
    }

    fn error_recovery(&self) -> Option<&'static ErrorRecovery> {
        Some(&CPP_ERROR_RECOVERY)
    }

    fn marks_split_declaration(&self, node: Node<'_>, text: &str) -> bool {
        is_stray_conditional_directive(node, text)
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = match node.kind() {
            "namespace_definition" => node.child_by_field_name("name")?,
            "struct_specifier" | "union_specifier" | "class_specifier" if has_body(node) =>
                node.child_by_field_name("name")?,
            _ => return None,
        };
        parser::node_text(name, text)
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment") || node.kind() == "preproc_include"
    }
}

/// A conditional-continuation directive (`#else`, `#elif…`, `#endif`) parsed as a lone
/// `preproc_call`. Inside a well-formed conditional the grammar folds these into the
/// `preproc_if`/`preproc_ifdef` node, so a stray one means the parser read the conditional's
/// branches as one run of text — a function whose header comes from one branch and whose body
/// closes another.
fn is_stray_conditional_directive(node: Node<'_>, text: &str) -> bool {
    node.kind() == "preproc_call"
        && node
            .child_by_field_name("directive")
            .and_then(|directive| parser::node_text(directive, text))
            .is_some_and(|directive| {
                // `# endif` is the same directive: whitespace may follow the `#`.
                let name = directive.trim_start().trim_start_matches('#').trim();
                matches!(name, "else" | "elif" | "elifdef" | "elifndef" | "endif")
            })
}

fn has_body(node: Node<'_>) -> bool {
    node.child_by_field_name("body").is_some()
}

/// The name a declaration's `declarator` field declares, read down the declarator chain: through
/// pointer, reference, array, function and parenthesized declarators, then along a C++ name's own
/// `name` field (`ns::run`, `run<T>`). Parameter lists, template arguments and scopes hang off
/// that chain as other fields and are never searched, so `void (*get_handler(int sig))(int)`
/// declares `get_handler`, not `sig`, and `template<> void foo<Bar>(Bar)` declares `foo`, not
/// `Bar`. An operator is named by its `operator_name` (`operator=`). Any other declarator end
/// (`operator bool()`, a dependent name) yields `None`: no name is better than a wrong one.
fn declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut declarator = node.child_by_field_name("declarator")?;
    loop {
        if NAME_KINDS.contains(&declarator.kind()) || declarator.kind() == "operator_name" {
            return Some(declarator);
        }
        declarator = match declarator.kind() {
            "qualified_identifier" | "template_function" =>
                declarator.child_by_field_name("name")?,
            // These wrap their inner declarator or name without a field. A destructor is named by
            // its class name.
            "parenthesized_declarator" | "reference_declarator" | "destructor_name" =>
                declarator.named_child(0)?,
            _ => declarator.child_by_field_name("declarator")?,
        };
    }
}

pub(super) const RESOLVER_POLICY: ResolutionPolicy =
    ResolutionPolicy { type_binding: TypeBinding::DefinitionsOnly, ..ResolutionPolicy::DEFAULT };
