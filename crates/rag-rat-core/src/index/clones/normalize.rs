use std::collections::HashMap;

use tree_sitter::Node;

/// A leaf tree-sitter kind that names a binding/reference identifier (kept language-agnostic by
/// matching the `*identifier` suffix tree-sitter grammars use).
fn is_identifier_kind(kind: &str) -> bool {
    kind.ends_with("identifier")
}

/// A leaf kind that is a literal whose *value* must not drive matching.
fn is_literal_kind(kind: &str) -> bool {
    kind.ends_with("literal")
        || matches!(kind, "string_content" | "integer" | "float" | "number" | "char")
}

/// Baseline normalization (#215 §4): pre-order walk of the symbol subtree emitting one token per
/// node — structural node kinds for internal nodes; for leaves, an alpha-renamed `ID<n>` for
/// identifiers (numbered by first occurrence), a `LIT_<KIND>` bucket for literals, and the verbatim
/// text for operators/keywords/punctuation. Scope-independent: depends only on this node's subtree.
pub(crate) fn normalize_baseline(node: Node<'_>, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut idents: HashMap<String, usize> = HashMap::new();
    walk(node, text.as_bytes(), &mut idents, &mut out);
    out
}

fn walk(node: Node<'_>, src: &[u8], idents: &mut HashMap<String, usize>, out: &mut Vec<String>) {
    let kind = node.kind();
    if node.child_count() == 0 {
        let leaf = node.utf8_text(src).unwrap_or("");
        if is_identifier_kind(kind) {
            let next = idents.len();
            let id = *idents.entry(leaf.to_string()).or_insert(next);
            out.push(format!("ID{id}"));
        } else if is_literal_kind(kind) {
            out.push(format!("LIT_{}", kind.to_ascii_uppercase()));
        } else {
            // keyword / operator / punctuation — structural, kept verbatim
            out.push(leaf.to_string());
        }
        return;
    }
    out.push(kind.to_string());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, idents, out);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::index::parser;
    use crate::language::Language;

    fn norm(src: &str) -> Vec<String> {
        let parsed = parser::parse_file(Path::new("t.rs"), Language::Rust, src).expect("parse");
        let func = parsed.symbols.iter().find(|s| s.kind == "function").expect("a function symbol");
        let node =
            parsed.root().descendant_for_byte_range(func.start_byte, func.end_byte).expect("node");
        normalize_baseline(node, src)
    }

    #[test]
    fn renamed_identical_bodies_normalize_equal() {
        let a = "fn load_user(db: Db) -> i32 { let u = db.get(10); u + 1 }";
        let b = "fn load_order(store: Db) -> i32 { let o = store.get(10); o + 1 }";
        assert_eq!(norm(a), norm(b));
    }

    #[test]
    fn structurally_different_bodies_differ() {
        let a = "fn f(db: Db) -> i32 { let u = db.get(10); u + 1 }";
        let b = "fn g(db: Db) -> i32 { while true { } 0 }";
        assert_ne!(norm(a), norm(b));
    }

    #[test]
    fn literals_are_bucketed_not_compared_by_value() {
        let a = "fn f() -> i32 { let x = 10; x }";
        let b = "fn g() -> i32 { let y = 99999; y }";
        assert_eq!(norm(a), norm(b));
    }
}
