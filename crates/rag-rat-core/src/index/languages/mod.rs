//! Structural language implementations.
//!
//! Each language owns its grammar-specific parser classification, edge extraction, and optional
//! resolver policy in one package. The parser walk, edge walk, and resolver orchestration stay in
//! their subsystem modules and depend only on the narrow traits defined here.

#[cfg(test)]
use std::collections::BTreeSet;
use std::path::Path;

use rag_rat_base::language::Language;
use tree_sitter::Node;

use super::edges::{EdgeCandidate, IndexedSymbol};
use super::parser::ParserKind;

mod c_family;
mod kotlin;
mod markdown;
mod python;
mod rust;
mod swift;
mod typescript;

pub(super) type SymbolMatch<'tree> = (&'static str, Node<'tree>);

/// Grammar-specific declaration and scope recognition for one node visited by the shared parser
/// walk.
pub(super) trait ParserBackend: Sync {
    fn parser_kind(&self, path: &Path) -> ParserKind;

    /// Every symbol kind this backend can emit — the declared contract behind [`all_symbol_kinds`].
    ///
    /// Required (not defaulted) on purpose: downstream consumers rank and filter by symbol kind,
    /// and a kind nobody downstream knows about silently sorts into the "unknown" bucket.
    /// Making this part of the trait means a new language cannot be added without stating its
    /// kinds, and `symbol_kind_rank_covers_every_indexed_kind` then fails until the ranking
    /// accounts for them. The parser walk debug-asserts that what a backend emits is what it
    /// declared here.
    fn symbol_kinds(&self) -> &'static [&'static str];

    fn symbol_node<'tree>(&self, node: Node<'tree>, text: &str) -> Option<SymbolMatch<'tree>>;

    fn symbol_name(&self, _node: Node<'_>, name_node: Node<'_>, text: &str) -> String {
        super::parser::node_text(name_node, text).unwrap_or_default()
    }

    fn for_each_symbol<'tree>(
        &self,
        node: Node<'tree>,
        text: &str,
        emit: &mut dyn FnMut(Node<'tree>, SymbolMatch<'tree>),
    ) {
        if let Some(symbol) = self.symbol_node(node, text) {
            emit(node, symbol);
        }
    }

    fn for_each_recovered_symbol<'tree>(
        &self,
        _node: Node<'tree>,
        _text: &str,
        _emit: &mut dyn FnMut(Node<'tree>, SymbolMatch<'tree>),
    ) {
    }

    fn scope_segment(&self, node: Node<'_>, text: &str) -> Option<String>;

    fn is_test_symbol(&self, _text: &str, _node: Node<'_>, _scope_path: &str, _name: &str) -> bool {
        false
    }

    fn signature_source_node<'tree>(&self, node: Node<'tree>) -> Node<'tree> {
        node
    }

    fn symbol_facts(&self, _text: &str, _node: Node<'_>) -> Vec<super::parser::ParsedSymbolFact> {
        Vec::new()
    }

    fn is_plumbing_node(&self, node: Node<'_>) -> bool {
        node.kind().contains("comment")
    }
}

/// Grammar-specific edge recognition for one node visited by the shared depth-safe edge walk.
pub(super) trait EdgeBackend: Sync {
    fn edges(
        &self,
        text: &str,
        node: Node<'_>,
        symbols: &[IndexedSymbol],
        path: &Path,
        out: &mut Vec<EdgeCandidate>,
    );
}

#[derive(Clone, Copy)]
pub(super) struct KindPreference {
    pub(super) symbol_kinds: &'static [&'static str],
    pub(super) same_language_only: bool,
}

/// Language-semantic resolution rules that cannot be derived from generic symbol kinds.
pub(super) trait ResolverPolicy: Sync {
    fn preferred_kinds(&self, edge_kind: &str) -> Option<KindPreference>;

    fn collapse_same_named_declarations(&self) -> bool {
        true
    }

    fn import_edges_carry_aliases(&self) -> bool {
        false
    }

    fn rebind_import_alias(&self, _request: ImportAliasRequest<'_>) -> ImportAliasRebind {
        ImportAliasRebind::default()
    }

    fn reference_is_unresolvable(&self, _edge_kind: &str, _name: &str) -> bool {
        false
    }

    /// A target kind the BARE-NAME fallback may bind only when the reference itself carried no
    /// qualifier and no receiver — i.e. only for the syntactic shape that is actual evidence for
    /// that kind.
    ///
    /// The qualified/receiver-bearing shapes are unaffected: they resolve through the qualified
    /// path (`Status.idle` → the `Status::idle` case) and only fall back to the bare name when
    /// that path finds nothing — which is exactly where a wrong bind would be manufactured.
    fn kind_requires_unqualified_reference(&self, _edge_kind: &str, _target_kind: &str) -> bool {
        false
    }

    fn suppress_unresolved_reference(&self, _edge_kind: &str, _evidence: Option<&str>) -> bool {
        false
    }

    fn type_reference_requires_type_definition(&self) -> bool {
        false
    }

    fn allow_type_receiver_fallback(&self) -> bool {
        false
    }

    fn allow_value_receiver_fallback(&self) -> bool {
        false
    }

    fn rejects_qualified_root(&self, _root: &str) -> bool {
        false
    }

    fn is_local_qualified_root(&self, _root: &str) -> bool {
        false
    }
}

pub(super) struct ImportAliasRequest<'a> {
    pub(super) to_name: &'a str,
    pub(super) target_qualified_name: Option<&'a str>,
    pub(super) receiver_hint: Option<&'a str>,
    pub(super) lookup: &'a mut dyn FnMut(&str) -> Option<String>,
}

#[derive(Default)]
pub(super) struct ImportAliasRebind {
    pub(super) name: Option<String>,
    pub(super) target_qualified_name: Option<String>,
    pub(super) receiver_hint: Option<String>,
}

pub(super) fn target_matches_policy(
    source_language: Option<&str>,
    edge_kind: &str,
    target_language: &str,
    target_kind: &str,
) -> bool {
    let Some(source_language) = source_language.and_then(|name| name.parse::<Language>().ok())
    else {
        return true;
    };
    let Some(preference) =
        resolver_policy(source_language).and_then(|policy| policy.preferred_kinds(edge_kind))
    else {
        return true;
    };
    (!preference.same_language_only || target_language == source_language.as_str())
        && preference.symbol_kinds.contains(&target_kind)
}

pub(super) fn parser_backend(language: Language) -> &'static dyn ParserBackend {
    match language {
        Language::Rust => &rust::SUPPORT,
        Language::TypeScript => &typescript::SUPPORT,
        Language::Kotlin => &kotlin::SUPPORT,
        Language::C => &c_family::C_SUPPORT,
        Language::Cpp => &c_family::CPP_SUPPORT,
        Language::Python => &python::SUPPORT,
        Language::Swift => &swift::SUPPORT,
        Language::Markdown => &markdown::SUPPORT,
    }
}

pub(super) fn edge_backend(language: Language) -> Option<&'static dyn EdgeBackend> {
    match language {
        Language::Rust => Some(&rust::SUPPORT),
        Language::TypeScript => Some(&typescript::SUPPORT),
        Language::Kotlin => Some(&kotlin::SUPPORT),
        Language::C => Some(&c_family::C_SUPPORT),
        Language::Cpp => Some(&c_family::CPP_SUPPORT),
        Language::Python => Some(&python::SUPPORT),
        Language::Swift => Some(&swift::SUPPORT),
        Language::Markdown => None,
    }
}

pub(super) fn resolver_policy(language: Language) -> Option<&'static dyn ResolverPolicy> {
    match language {
        Language::Rust => Some(&rust::SUPPORT),
        Language::Kotlin => Some(&kotlin::SUPPORT),
        Language::C => Some(&c_family::C_SUPPORT),
        Language::Cpp => Some(&c_family::CPP_SUPPORT),
        Language::Python => Some(&python::SUPPORT),
        Language::Swift => Some(&swift::SUPPORT),
        _ => None,
    }
}

pub(super) fn resolver_policy_for_name(
    language: Option<&str>,
) -> Option<&'static dyn ResolverPolicy> {
    language.and_then(|name| name.parse::<Language>().ok()).and_then(resolver_policy)
}

pub(super) fn is_plumbing_node(language: Language, node: Node<'_>) -> bool {
    parser_backend(language).is_plumbing_node(node)
}

/// Every symbol kind ANY registered language can emit, deduplicated and sorted — the single source
/// of truth for the completeness tests guarding downstream code that must handle the full kind
/// universe (today: the `symbol_lookup` kind ranking). Derived from the backends themselves, so
/// registering a language adds its kinds here automatically and the consumers' completeness tests
/// fail until they account for them.
///
/// Test-only: the production consumer is a static table, and this exists to PROVE that table covers
/// the registry. [`ParserBackend::symbol_kinds`] itself is live in every build — the parser walk
/// debug-asserts each emitted kind against it.
#[cfg(test)]
pub(crate) fn all_symbol_kinds() -> BTreeSet<&'static str> {
    Language::all()
        .iter()
        .flat_map(|&language| parser_backend(language).symbol_kinds())
        .copied()
        .collect()
}

pub(super) fn requires_same_language_target(
    source_language: Option<&str>,
    edge_kind: &str,
) -> bool {
    source_language
        .and_then(|language| language.parse::<Language>().ok())
        .and_then(resolver_policy)
        .and_then(|policy| policy.preferred_kinds(edge_kind))
        .is_some_and(|preference| preference.same_language_only)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rag_rat_base::language::Language;

    use super::{edge_backend, parser_backend};
    use crate::index::parser::{self, ParserKind};

    #[test]
    fn registry_agrees_with_structural_capabilities() {
        for &language in Language::all() {
            let path = match language {
                Language::TypeScript => Path::new("src/App.tsx"),
                _ => Path::new("src/file"),
            };
            let kind = parser_backend(language).parser_kind(path);
            assert_eq!(kind == ParserKind::Markdown, language == Language::Markdown);
            assert_eq!(parser::grammar_for(kind).is_some(), language != Language::Markdown);
            assert_eq!(edge_backend(language).is_some(), language != Language::Markdown);
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use std::collections::HashSet;

    use super::all_symbol_kinds;

    /// The class tripwire (#635): EVERY symbol kind any language backend can emit must carry an
    /// explicit rank. Driven off `languages::all_symbol_kinds()` — the backends' own declaration —
    /// so registering a language with a new kind (a Swift `protocol`, a C++ `namespace`) reddens
    /// HERE, at the point the ranking would silently dump it into the unknown bucket, instead of
    /// shipping a lookup that sorts it below `impl`. Ranking is a downstream consumer of the
    /// language registry; this is what keeps the two from drifting.
    #[test]
    fn symbol_kind_rank_covers_every_indexed_kind() {
        let ranked: HashSet<&str> =
            rag_rat_query::symbol::SYMBOL_KIND_RANK.iter().map(|(kind, _)| *kind).collect();
        let unranked: Vec<&str> =
            all_symbol_kinds().into_iter().filter(|kind| !ranked.contains(kind)).collect();
        assert!(
            unranked.is_empty(),
            "symbol kinds emitted by a language backend but never ranked in \
             rag_rat_query::symbol::SYMBOL_KIND_RANK (they would sort into the unknown bucket, \
             below `impl`): {unranked:?}"
        );
    }
}
