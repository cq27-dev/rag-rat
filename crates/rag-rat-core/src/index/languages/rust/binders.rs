//! Generic-binder scope for Rust type mentions.
//!
//! A written type is a nameable type only relative to the binders in force AT THE NODE WHERE IT
//! IS WRITTEN — `impl<Entry: Runs> Bag<Entry> { fn drive(&self, item: Entry) }` mentions `Entry`,
//! and that `Entry` is not the `struct Entry` next door. Getting this wrong always fails the same
//! way: the binder is taken for a concrete type, and receiver resolution binds the call to an
//! unrelated same-named symbol at `Syntactic` confidence.
//!
//! The predicate takes the NODE, never a pre-collected list of names. A list has to be assembled
//! by a caller who guesses which item introduced the binders — and every caller that guessed
//! guessed too narrowly (the function's own binders, or an empty slice), because the enclosing
//! `impl` and `trait` are invisible from the node the caller happened to hold. Asking the tree
//! makes the position itself the answer, so a near-miss node inside the same item nest still
//! yields the correct verdict. This mirrors Swift's `swift_name_is_type_parameter_in_scope`,
//! which closed the same class on that language.

use tree_sitter::Node;

use crate::index::edges::{child_name_text, node_text};

/// Whether `name` is a generic parameter bound by any item enclosing `at` (inclusive).
///
/// Walks outward to the file root: every item that can carry `type_parameters` contributes —
/// `impl`, `fn`, `trait`, and the type items — so a binder introduced two levels up is still in
/// force. Lifetimes and const parameters are collected too; neither survives
/// [`super::edges::clean_rust_type_name`] as a type name today, so they are here for honesty
/// rather than reach.
pub(super) fn binds_name(at: Node<'_>, name: &str, text: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut current = Some(at);
    while let Some(node) = current {
        if let Some(parameters) = node.child_by_field_name("type_parameters")
            && parameter_list_binds(parameters, name, text)
        {
            return true;
        }
        current = node.parent();
    }
    false
}

/// Whether a `type_parameters` list binds `name`.
///
/// `type_parameter` covers `T`, `T: Bound`, and `T = Default` alike — the grammar has no separate
/// constrained/optional kind (verified against tree-sitter-rust: `constrained_type_parameter` and
/// `optional_type_parameter` are not node kinds), so one arm is the whole type-parameter story.
fn parameter_list_binds(parameters: Node<'_>, name: &str, text: &str) -> bool {
    let mut cursor = parameters.walk();
    parameters.named_children(&mut cursor).any(|parameter| match parameter.kind() {
        // `r#T` and `T` are one parameter, and the occurrence side strips the prefix before
        // matching — so this has to strip it too, or a raw-spelled binder is substituted by the
        // owner renderer and missed by this membership test.
        "type_parameter" | "const_parameter" => child_name_text(parameter, text)
            .is_some_and(|declared| declared.strip_prefix("r#").unwrap_or(&declared) == name),
        "lifetime_parameter" | "lifetime" => {
            let lifetime = node_text(parameter, text);
            lifetime.split(':').next().unwrap_or_default().trim() == name
        },
        _ => false,
    })
}
