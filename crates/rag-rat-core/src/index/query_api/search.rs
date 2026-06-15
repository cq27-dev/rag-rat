//! Lexical+vector search and symbol-doc retrieval on `IndexDatabase`: the `search*` entry points,
//! the [`SearchRequest`] builder behind them, and the `docs_for_*` symbol-documentation helpers.

use super::*;

/// A graph-enriched search request: the lexical query plus the graph-meta controls. Replaces the
/// `search_*_with_graph_meta[_options]` method ladder — callers fill one value (using
/// [`SearchRequest::new`] for the common defaults) instead of picking among four
/// positional-argument variants and transposing the bools.
pub struct SearchRequest<'a> {
    pub query: &'a str,
    pub limit: u32,
    pub include_generated: bool,
    pub explain: bool,
    pub graph_mode: GraphMetaMode,
    pub graph_limit: u32,
    pub options: SearchOptions,
}

impl<'a> SearchRequest<'a> {
    /// The conventional search defaults: no generated files, no explain, compact graph meta to
    /// depth 3, git + papertrail boosts on. Override individual fields with struct-update syntax.
    pub fn new(query: &'a str, limit: u32) -> Self {
        Self {
            query,
            limit,
            include_generated: false,
            explain: false,
            graph_mode: GraphMetaMode::Compact,
            graph_limit: 3,
            options: SearchOptions::default(),
        }
    }
}

impl IndexDatabase {
    pub fn search(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_with_graph_meta(SearchRequest {
            include_generated,
            ..SearchRequest::new(query, limit)
        })
    }

    pub fn search_explain(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.search_with_graph_meta(SearchRequest {
            include_generated,
            explain: true,
            ..SearchRequest::new(query, limit)
        })
    }

    /// Lexical+vector search with graph evidence and load-bearing enrichment attached. The single
    /// entry point behind `search`/`search_explain`; callers that need non-default graph depth,
    /// git/papertrail toggles, or explain mode build a [`SearchRequest`] directly.
    pub fn search_with_graph_meta(
        &self,
        request: SearchRequest<'_>,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.ensure_fts_fresh()?;
        let query = crate::search::lexical::LexicalQuery {
            query: request.query,
            limit: request.limit,
            include_generated: request.include_generated,
            explain: request.explain,
            options: request.options,
        };
        let mut hits = self.search_with_heal(&query, Heal::Allow)?;
        graph_meta::attach_to_search_hits(
            self.storage.connection(),
            &mut hits,
            request.graph_mode,
            request.graph_limit,
        )?;
        self.enrich_search_hits_with_load_bearing(&mut hits)?;
        Ok(hits)
    }

    pub fn search_hash_baseline(
        &self,
        query: &str,
        limit: u32,
        include_generated: bool,
    ) -> anyhow::Result<Vec<SearchHit>> {
        self.ensure_fts_fresh()?;
        crate::search::lexical::search_hash_baseline(
            self.storage.connection(),
            query,
            limit,
            include_generated,
        )
    }

    pub fn docs_for_symbol(&self, symbol: &str, limit: u32) -> anyhow::Result<Vec<SearchHit>> {
        self.search(symbol, limit, true)
    }

    pub fn docs_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let mut hits = self.find_local_symbol_context_hits(symbol, limit)?;
        hits.extend(self.search(&symbol.name, limit.saturating_mul(4).max(limit), true)?);
        rank_docs_for_symbol(symbol, &mut hits);
        dedupe_search_hits(&mut hits);
        hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(hits)
    }
}
