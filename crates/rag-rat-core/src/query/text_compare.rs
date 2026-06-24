//! Graph-vs-text comparison and search-hit classification: reasons/labels for graph-only &
//! text-only hits, parser-gap & false-positive detection, search doc-ranking and dedup. Support for
//! query_api's compare_graph_to_text and search.

use std::collections::BTreeSet;
use std::path::Path;

use crate::index::is_generated_path;
use crate::search::lexical::SearchHit;

pub(crate) fn rank_docs_for_symbol(
    symbol: &crate::query::symbol::SymbolHit,
    hits: &mut [SearchHit],
) {
    let source_module = module_stem(&symbol.path);
    let symbol_name = symbol.name.to_ascii_lowercase();
    let qualified_name = symbol.qualified_name.to_ascii_lowercase();
    hits.sort_by(|a, b| {
        let a_rank = docs_locality_rank(symbol, &source_module, &symbol_name, &qualified_name, a);
        let b_rank = docs_locality_rank(symbol, &source_module, &symbol_name, &qualified_name, b);
        a_rank
            .cmp(&b_rank)
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    for (idx, hit) in hits.iter_mut().enumerate() {
        hit.score = (10_000usize.saturating_sub(idx)) as f64;
    }
}

pub(crate) fn docs_locality_rank(
    symbol: &crate::query::symbol::SymbolHit,
    source_module: &str,
    symbol_name: &str,
    qualified_name: &str,
    hit: &SearchHit,
) -> u8 {
    let path = hit.path.to_ascii_lowercase();
    let summary = hit.summary.to_ascii_lowercase();
    let hit_symbol = hit.symbol_path.as_deref().unwrap_or_default().to_ascii_lowercase();
    if hit.path == symbol.path && hit_symbol == symbol.qualified_name.to_ascii_lowercase() {
        return 0;
    }
    if hit.path == symbol.path {
        return 1;
    }
    if !source_module.is_empty()
        && path.contains(source_module)
        && (summary.contains(symbol_name) || hit_symbol.contains(symbol_name))
    {
        return 2;
    }
    if summary.contains(qualified_name) || hit_symbol.contains(qualified_name) {
        return 3;
    }
    if summary.contains(symbol_name) || hit_symbol.contains(symbol_name) {
        return 4;
    }
    if !source_module.is_empty() && path.contains(source_module) {
        return 5;
    }
    9
}

pub(crate) fn module_stem(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub(crate) fn dedupe_search_hits(hits: &mut Vec<SearchHit>) {
    let mut seen = BTreeSet::new();
    hits.retain(|hit| seen.insert(hit.chunk_id));
}

pub(crate) fn bounded_summary(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect()
}

pub(crate) fn graph_only_reason(
    edge: &crate::query::graph::GraphHop,
    current_line: Option<&str>,
) -> String {
    let Some(line) = current_line else {
        return "missing_current_source_line".to_string();
    };
    if edge
        .target_qualified_name
        .as_deref()
        .is_some_and(|qualified| !qualified.is_empty() && line.contains(qualified))
    {
        return "qualified_call_pattern_mismatch".to_string();
    }
    if edge.target.as_deref().is_some_and(|target| !target.is_empty() && line.contains(target)) {
        return "imported_or_unqualified_call".to_string();
    }
    if edge
        .evidence
        .as_deref()
        .is_some_and(|evidence| !evidence.is_empty() && line.contains(evidence.trim()))
    {
        return "regex_too_narrow".to_string();
    }
    "stale_or_overbroad_graph_edge".to_string()
}

pub(crate) fn is_likely_false_positive_graph_only(
    edge: &crate::query::graph::GraphHop,
    graph_only: &crate::query::graph::GraphOnlyEdge,
) -> bool {
    if graph_only.likely_reason == "stale_or_overbroad_graph_edge" {
        return true;
    }
    edge.resolution == "target_name_fallback"
        || edge.confidence == "name_only"
        || edge.confidence == "ambiguous"
        || !edge.verified_target_symbol
}

pub(crate) fn classify_text_only_hit(
    path: &str,
    text: &str,
    parser_failure_paths: &BTreeSet<String>,
) -> &'static str {
    if parser_failure_paths.contains(path) {
        return "parser_failure";
    }
    if is_generated_path(path) {
        return "generated_text_mention";
    }
    let trimmed = text.trim_start();
    if is_comment_like_text(trimmed) {
        return "comment_text_mention";
    }
    if is_import_or_declaration_text(trimmed) {
        return "declaration_text_mention";
    }
    if crate::index::parser::is_test_path(path) && is_test_scaffolding_text(trimmed) {
        return "test_scaffolding_text_mention";
    }
    "parser_call_extraction"
}

pub(crate) fn is_likely_parser_gap_kind(kind: &str) -> bool {
    matches!(kind, "parser_call_extraction" | "parser_failure")
}

pub(crate) fn is_comment_like_text(text: &str) -> bool {
    text.starts_with("//")
        || text.starts_with("/*")
        || text.starts_with('*')
        || text.starts_with("*/")
        || text.starts_with("#")
}

pub(crate) fn is_import_or_declaration_text(text: &str) -> bool {
    text.starts_with("import ")
        || text.starts_with("export type ")
        || text.starts_with("export interface ")
        || text.starts_with("type ")
        || text.starts_with("interface ")
        || text.starts_with("declare ")
}

pub(crate) fn is_test_scaffolding_text(text: &str) -> bool {
    text.contains(".mock")
        || text.contains("jest.")
        || text.contains("jest<")
        || text.contains("expect(")
        || text.contains("toHaveBeen")
        || text.contains("describe(")
        || text.contains("it(")
        || text.contains("test(")
}

pub(crate) fn recommended_graph_text_fallback(
    parser_gaps: &[crate::query::graph::TextOnlyHit],
    graph_only_edges: &[crate::query::graph::GraphOnlyEdge],
) -> String {
    match (parser_gaps.is_empty(), graph_only_edges.is_empty()) {
        (false, false) => "both",
        (false, true) => "text",
        (true, false) => "graph",
        (true, true) => "none",
    }
    .to_string()
}

pub(crate) fn compare_pattern_match_mode(pattern: &str, symbol_name: &str) -> String {
    if symbol_name.is_empty() {
        return "regex".to_string();
    }
    let escaped_call = format!("{symbol_name}\\(");
    let plain_call = format!("{symbol_name}(");
    if pattern.contains("\\b")
        || pattern.contains("\\W")
        || pattern.contains("[^")
        || pattern.contains(&escaped_call)
        || pattern.contains(&plain_call)
    {
        return "identifier_or_call".to_string();
    }
    if pattern.contains(symbol_name) {
        return "substring_identifier".to_string();
    }
    "regex".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_hit(chunk_id: i64) -> SearchHit {
        SearchHit {
            chunk_id,
            path: String::new(),
            language: String::new(),
            kind: String::new(),
            start_line: 0,
            end_line: 0,
            symbol_path: None,
            score: 0.0,
            retrieval_mode: String::new(),
            summary: String::new(),
            graph: None,
            score_components: None,
            importance: None,
        }
    }

    #[test]
    fn module_stem_lowercases_the_file_stem() {
        assert_eq!(module_stem("crates/app/src/Runtime.rs"), "runtime");
        assert_eq!(module_stem("nested/Mod.tsx"), "mod");
        assert_eq!(module_stem(""), "");
    }

    #[test]
    fn bounded_summary_collapses_whitespace_and_caps_length() {
        assert_eq!(bounded_summary("  a\t b\n  c  "), "a b c");
        let long = "x ".repeat(300); // 300 "x " tokens -> 300 chars after join+truncate
        assert_eq!(bounded_summary(&long).chars().count(), 240);
    }

    #[test]
    fn parser_gap_kinds() {
        assert!(is_likely_parser_gap_kind("parser_call_extraction"));
        assert!(is_likely_parser_gap_kind("parser_failure"));
        assert!(!is_likely_parser_gap_kind("comment_text_mention"));
    }

    #[test]
    fn text_shape_predicates() {
        assert!(is_comment_like_text("// a comment"));
        assert!(is_comment_like_text("/* block"));
        assert!(is_comment_like_text("# python"));
        assert!(!is_comment_like_text("let x = call();"));

        assert!(is_import_or_declaration_text("import { a } from 'b'"));
        assert!(is_import_or_declaration_text("interface Foo {"));
        assert!(!is_import_or_declaration_text("foo()"));

        assert!(is_test_scaffolding_text("  expect(foo()).toBe(1)"));
        assert!(is_test_scaffolding_text("describe('x', () => {"));
        assert!(!is_test_scaffolding_text("let y = bar();"));
    }

    #[test]
    fn classify_text_only_hit_walks_the_precedence() {
        let failures = BTreeSet::from(["broken.rs".to_string()]);
        assert_eq!(classify_text_only_hit("broken.rs", "spawn();", &failures), "parser_failure");
        assert_eq!(
            classify_text_only_hit("src/a.rs", "// spawn() in a comment", &failures),
            "comment_text_mention"
        );
        assert_eq!(
            classify_text_only_hit("src/a.ts", "import { spawn } from 'x'", &failures),
            "declaration_text_mention"
        );
        assert_eq!(
            classify_text_only_hit("src/a.test.ts", "expect(spawn()).toBe(1)", &failures),
            "test_scaffolding_text_mention"
        );
        // A real call site in non-test, non-comment source is a likely parser gap.
        assert_eq!(
            classify_text_only_hit("src/a.rs", "spawn();", &failures),
            "parser_call_extraction"
        );
    }

    #[test]
    fn pattern_match_mode_classification() {
        assert_eq!(compare_pattern_match_mode(r"foo\(", "foo"), "identifier_or_call");
        assert_eq!(compare_pattern_match_mode(r"\bfoo\b", "foo"), "identifier_or_call");
        assert_eq!(compare_pattern_match_mode("foo", "foo"), "substring_identifier");
        assert_eq!(compare_pattern_match_mode("something_else", "foo"), "regex");
        assert_eq!(compare_pattern_match_mode("foo", ""), "regex");
    }

    #[test]
    fn recommended_fallback_for_empty_inputs_is_none() {
        assert_eq!(recommended_graph_text_fallback(&[], &[]), "none");
    }

    #[test]
    fn dedupe_search_hits_keeps_first_per_chunk_id() {
        let mut hits = vec![search_hit(1), search_hit(2), search_hit(1), search_hit(3)];
        dedupe_search_hits(&mut hits);
        assert_eq!(hits.iter().map(|h| h.chunk_id).collect::<Vec<_>>(), vec![1, 2, 3]);
    }
}
