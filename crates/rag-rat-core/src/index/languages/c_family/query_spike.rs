//! Query-backed C/C++ edge-discovery prototype for issue #942.
//!
//! Queries only discover nodes. Candidate normalization, source ownership, and emission continue
//! through the production C-family backend so the comparison isolates traversal strategy.

use std::path::Path;
use std::sync::OnceLock;

use rag_rat_base::language::Language;
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

use super::c_like_edges;
use crate::index::edges::extract::{EdgeEmitter, EdgeVisit};
use crate::index::edges::{EdgeCandidate, IndexedSymbol, SymbolLocator};
use crate::index::parser::{self, ParserKind};
use crate::index::symbols;

const C_QUERY_SOURCE: &str = r#"
(preproc_include) @edge
(call_expression) @edge
(type_identifier) @edge
"#;

const CPP_QUERY_SOURCE: &str = r#"
(preproc_include) @edge
(call_expression) @edge
[
  (type_identifier)
  (qualified_identifier)
  (namespace_identifier)
] @edge
"#;

static C_QUERY: OnceLock<Result<Query, String>> = OnceLock::new();
static CPP_QUERY: OnceLock<Result<Query, String>> = OnceLock::new();

fn compiled_query(language: Language) -> Result<&'static Query, &'static str> {
    let (cell, parser_kind, source) = match language {
        Language::C => (&C_QUERY, ParserKind::C, C_QUERY_SOURCE),
        Language::Cpp => (&CPP_QUERY, ParserKind::Cpp, CPP_QUERY_SOURCE),
        _ => return Err("query spike only supports C and C++"),
    };
    cell.get_or_init(|| {
        let grammar = parser::grammar_for(parser_kind).expect("C-family grammar");
        Query::new(&grammar, source).map_err(|error| error.to_string())
    })
    .as_ref()
    .map_err(String::as_str)
}

fn query_edges(path: &Path, language: Language, text: &str) -> anyhow::Result<Vec<EdgeCandidate>> {
    let parsed = parser::parse_file(path, language, text)
        .ok_or_else(|| anyhow::anyhow!("C-family fixture exceeded the parse budget"))?;
    let prepared = symbols::from_parsed(&parsed.symbols);
    let indexed = IndexedSymbol::local_from_prepared(language, &prepared);
    query_edges_from_root(path, language, text, parsed.root(), &indexed)
}

fn query_edges_from_root(
    path: &Path,
    language: Language,
    text: &str,
    root: Node<'_>,
    symbols: &[IndexedSymbol],
) -> anyhow::Result<Vec<EdgeCandidate>> {
    let query = compiled_query(language).map_err(anyhow::Error::msg)?;
    let locator = SymbolLocator::new(symbols);
    let mut candidates = Vec::new();
    let mut emit = EdgeEmitter::new(&mut candidates);
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, text.as_bytes());
    while let Some(query_match) = matches.next() {
        for capture in query_match.captures {
            c_like_edges(
                EdgeVisit { text, node: capture.node, symbols, path, locator: &locator },
                &mut emit,
            );
        }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::hint::black_box;
    use std::path::PathBuf;
    use std::time::Instant;

    use super::*;
    use crate::index::edges::extract::{collect_edges, syntactic_edges};

    const C_FIXTURE: &str = r#"
#include <stddef.h>
typedef struct Item Item;
struct Item { size_t count; };
Item *make_item(size_t count) {
    Item *item = allocate_item(count);
    item->count = normalize_count(count);
    return item;
}
"#;

    const CPP_FIXTURE: &str = r#"
#include <vector>
namespace demo {
class Item {};
std::vector<Item> make_items() {
    auto item = factory::make<Item>();
    return std::vector<Item>{item};
}
}
"#;

    fn by_kind(candidates: &[EdgeCandidate]) -> BTreeMap<&str, usize> {
        let mut counts = BTreeMap::new();
        for candidate in candidates {
            *counts.entry(candidate.edge_kind.as_str()).or_default() += 1;
        }
        counts
    }

    fn source_files(root: &Path, extensions: &[&str]) -> std::io::Result<Vec<PathBuf>> {
        let mut pending = vec![root.to_path_buf()];
        let mut files = Vec::new();
        while let Some(path) = pending.pop() {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    pending.push(path);
                } else if path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extensions.contains(&extension))
                {
                    files.push(path);
                }
            }
        }
        files.sort();
        Ok(files)
    }

    #[test]
    fn queries_compile_once_for_each_grammar() {
        assert!(compiled_query(Language::C).is_ok());
        assert!(compiled_query(Language::Cpp).is_ok());
        assert!(std::ptr::eq(
            compiled_query(Language::C).unwrap(),
            compiled_query(Language::C).unwrap()
        ));
        assert!(std::ptr::eq(
            compiled_query(Language::Cpp).unwrap(),
            compiled_query(Language::Cpp).unwrap()
        ));
    }

    #[test]
    fn query_discovery_matches_manual_output_by_edge_kind() -> anyhow::Result<()> {
        for (path, language, source) in
            [("fixture.c", Language::C, C_FIXTURE), ("fixture.cpp", Language::Cpp, CPP_FIXTURE)]
        {
            let path = Path::new(path);
            let parsed = parser::parse_file(path, language, source).expect("fixture parse");
            let prepared = symbols::from_parsed(&parsed.symbols);
            let indexed = IndexedSymbol::local_from_prepared(language, &prepared);
            let manual = syntactic_edges(path, language, source, &indexed)?;
            let queried = query_edges(path, language, source)?;
            assert_eq!(by_kind(&queried), by_kind(&manual), "{language:?}");
            assert_eq!(format!("{queried:#?}"), format!("{manual:#?}"), "{language:?}");
        }
        Ok(())
    }

    /// Differential corpus harness used for the pinned `c-libuv` and `cpp-yaml` oracle corpora.
    #[test]
    #[ignore = "set RAG_RAT_QUERY_SPIKE_CORPUS and RAG_RAT_QUERY_SPIKE_LANGUAGE"]
    fn query_spike_corpus_differential() -> anyhow::Result<()> {
        let root = PathBuf::from(std::env::var("RAG_RAT_QUERY_SPIKE_CORPUS")?);
        let (language, extensions) = match std::env::var("RAG_RAT_QUERY_SPIKE_LANGUAGE")?.as_str() {
            "c" => (Language::C, &["c", "h"][..]),
            "cpp" => (Language::Cpp, &["cc", "cpp", "cxx", "h", "hh", "hpp", "hxx"][..]),
            value => anyhow::bail!("unsupported language {value:?}"),
        };
        let mut files = 0;
        let mut manual_totals = BTreeMap::<String, usize>::new();
        let mut query_totals = BTreeMap::<String, usize>::new();
        let mut changed_files = 0;
        let mut manual_elapsed = std::time::Duration::ZERO;
        let mut query_elapsed = std::time::Duration::ZERO;
        for path in source_files(&root, extensions)? {
            let source = fs::read_to_string(&path)?;
            let parsed = parser::parse_file(&path, language, &source)
                .ok_or_else(|| anyhow::anyhow!("{} exceeded parse budget", path.display()))?;
            let prepared = symbols::from_parsed(&parsed.symbols);
            let indexed = IndexedSymbol::local_from_prepared(language, &prepared);
            let started = Instant::now();
            let mut manual = Vec::new();
            collect_edges(language, &source, parsed.root(), &indexed, &path, &mut manual);
            manual_elapsed += started.elapsed();
            let started = Instant::now();
            let queried = query_edges_from_root(&path, language, &source, parsed.root(), &indexed)?;
            query_elapsed += started.elapsed();
            for candidate in &manual {
                *manual_totals.entry(candidate.edge_kind.as_str().to_owned()).or_default() += 1;
            }
            for candidate in &queried {
                *query_totals.entry(candidate.edge_kind.as_str().to_owned()).or_default() += 1;
            }
            if format!("{manual:#?}") != format!("{queried:#?}") {
                changed_files += 1;
                eprintln!("changed {}", path.display());
            }
            files += 1;
        }
        eprintln!(
            "language={language:?} files={files} changed_files={changed_files} \
             manual={manual_totals:?} query={query_totals:?} manual_ns={} query_ns={}",
            manual_elapsed.as_nanos(),
            query_elapsed.as_nanos()
        );
        anyhow::ensure!(files > 0, "corpus root contained no matching source files");
        assert_eq!(query_totals, manual_totals);
        assert_eq!(changed_files, 0);
        Ok(())
    }

    /// Run directly under a heap profiler to compare the two discovery strategies without parsing:
    ///
    /// `RAG_RAT_QUERY_SPIKE_MODE=manual|query cargo test --release -p rag-rat-core
    /// query_spike_benchmark --no-default-features -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement harness for issue #942"]
    fn query_spike_benchmark() -> anyhow::Result<()> {
        let mode = std::env::var("RAG_RAT_QUERY_SPIKE_MODE")
            .map_err(|_| anyhow::anyhow!("set RAG_RAT_QUERY_SPIKE_MODE=manual|query"))?;
        let fixtures = [
            (Path::new("fixture.c"), Language::C, C_FIXTURE),
            (Path::new("fixture.cpp"), Language::Cpp, CPP_FIXTURE),
        ];
        let parsed = fixtures
            .iter()
            .map(|&(path, language, source)| {
                let parsed = parser::parse_file(path, language, source).expect("fixture parse");
                let prepared = symbols::from_parsed(&parsed.symbols);
                let indexed = IndexedSymbol::local_from_prepared(language, &prepared);
                (path, language, source, parsed, indexed)
            })
            .collect::<Vec<_>>();

        const ITERATIONS: usize = 10_000;
        let started = Instant::now();
        let mut emitted = 0;
        for _ in 0..ITERATIONS {
            for (path, language, source, parsed, indexed) in &parsed {
                let candidates = match mode.as_str() {
                    "manual" => {
                        let mut candidates = Vec::new();
                        collect_edges(
                            *language,
                            source,
                            parsed.root(),
                            indexed,
                            path,
                            &mut candidates,
                        );
                        candidates
                    },
                    "query" =>
                        query_edges_from_root(path, *language, source, parsed.root(), indexed)?,
                    _ => anyhow::bail!("RAG_RAT_QUERY_SPIKE_MODE must be manual or query"),
                };
                emitted += black_box(candidates.len());
            }
        }
        eprintln!(
            "mode={mode} iterations={ITERATIONS} emitted={emitted} elapsed_ns={}",
            started.elapsed().as_nanos()
        );
        Ok(())
    }
}
