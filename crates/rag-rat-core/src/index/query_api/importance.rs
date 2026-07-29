//! Symbol-importance query surface on `IndexDatabase`: PageRank-ranked `important_symbols` (with
//! seed resolution for personalization) and the load-bearing-callee enrichment of search/symbol/
//! neighbor hits.

use rag_rat_query::pagerank::ImportantSymbolsResult;
use rusqlite::OptionalExtension;

use super::*;

/// Inputs to [`IndexDatabase::important_symbols`]. The seed (`personalize`) takes names, paths, or
/// `sym_<hex>` handles; `auto_seed_from_diff` is the MCP-only default (seed from the current git
/// diff when no explicit seed is given) — the CLI passes `false` so it stays global-by-default. The
/// intentional MCP/CLI divergence is acceptance-invariant #1.
pub struct ImportantSymbolsRequest {
    pub limit: usize,
    pub personalize: Vec<String>,
    pub auto_seed_from_diff: bool,
}

/// Explicit seed selectors resolved to in-graph symbol ids, plus the count that resolved to nothing
/// (ambiguous / missing — skipped, not fatal).
struct ResolvedSeeds {
    symbol_ids: Vec<i64>,
    unresolved: u64,
}

/// The git-diff auto-seed, mapped through the scoped `files` view, with provenance counts.
#[derive(Default)]
struct DiffSeed {
    symbol_ids: Vec<i64>,
    changed_paths: u64,
    indexed_paths: u64,
    skipped: rag_rat_query::pagerank::SkippedSeeds,
}

/// What one changed path contributed to the diff seed.
enum ChangedPathSymbols {
    /// The path is indexed (non-generated) in the active scope; its symbol ids (possibly empty for
    /// a config/markdown file or a parser gap).
    Symbols(Vec<i64>),
    /// The path is indexed as a generated artifact — deliberately excluded from the seed.
    Generated,
    /// The path is not in the active scope's `files` view at all.
    None,
}

impl IndexDatabase {
    /// Rank load-bearing symbols by weighted PageRank over the active checkout's edge graph
    /// (#108), returning the labeled [`ImportantSymbolsResult`] (mode + seed provenance) per the
    /// spec's "three scales". `personalize` biases importance toward the seed symbols; empty =
    /// global.
    ///
    /// When a SCIP oracle run exists for this checkout, ranking uses the compiler-verified graph
    /// (contradicted edges dropped, upgrades retargeted, confirmed/upgraded edges weighted above
    /// heuristic) — otherwise the heuristic graph with confidence weighting. The oracle lookup is
    /// gated on a run existing, so absent oracle data it costs nothing (no scan).
    ///
    /// Seed resolution happens HERE, at the query boundary, because it needs both the symbol index
    /// (name/ref/handle → id) and git (the working-set diff); `query::pagerank` stays a pure
    /// ranking primitive over raw ids. Seed precedence:
    /// - explicit `request.personalize` (names / refs / `sym_<hex>` handles) →
    ///   `SeedKind::Explicit`;
    /// - else, if `request.auto_seed_from_diff` (the MCP default), the current git diff →
    ///   `SeedKind::GitDiff`;
    /// - else (the CLI default, or an explicit empty/`global` selector) → global, un-seeded.
    ///
    /// A seed intent that resolves to NO in-graph symbol (bad names only, or a diff with no indexed
    /// symbols) does NOT hard-error: it falls through to global ranking but REPORTS the
    /// fall-through (`mode = global …` + `reason` + the diff counts), so the caller sees WHY it
    /// was un-seeded.
    pub fn important_symbols(
        &self,
        request: ImportantSymbolsRequest,
    ) -> anyhow::Result<ImportantSymbolsResult> {
        use rag_rat_query::pagerank::{ImportanceMode, SeedKind, SeedSource, SkippedSeeds};

        let oracle_effects = self.symbol_importance_oracle_effects()?;
        // Heuristic-only ranking (no oracle run for this checkout) earns a one-line nudge that
        // compiler-grade ranking is available. The config-unaware wording lives here; CLI/MCP swap
        // in the auto-run variant when `[oracle] auto_run` is on.
        let ranking_hint: Option<String> = oracle_effects
            .is_none()
            .then(|| rag_rat_query::pagerank::RANKING_HINT_RUN_ORACLE.to_string());
        let rank = |seed: &[i64]| -> anyhow::Result<rag_rat_query::pagerank::RankedImportance> {
            rag_rat_query::pagerank::important_symbols(
                self.storage.connection(),
                rag_rat_query::pagerank::ImportanceOptions {
                    limit: request.limit,
                    personalize_to: seed,
                    oracle_effects: oracle_effects.as_ref(),
                },
            )
        };

        // Explicit names/paths/ids win over the auto-diff default.
        if !request.personalize.is_empty() {
            let resolved = self.resolve_seed_selectors(&request.personalize)?;
            let ranked = rank(&resolved.symbol_ids)?;
            let seed_source = SeedSource {
                kind: SeedKind::Explicit,
                // Explicit seeds are names, not paths — no path population to report.
                changed_paths: 0,
                indexed_paths: 0,
                symbol_seed_count: resolved.symbol_ids.len() as u64,
                effective_seed_count: ranked.effective_seed_count,
                skipped: SkippedSeeds { no_symbols: resolved.unresolved, ..Default::default() },
            };
            // No seed reached the graph — either nothing resolved, or the resolved symbols are not
            // endpoints of any edge — so the ranking is actually global. Label it Global and say
            // why, rather than implying it is personalized to the named symbols. (#142 review)
            if ranked.effective_seed_count == 0 {
                let reason = if resolved.symbol_ids.is_empty() {
                    "no named symbols resolved to the active scope"
                } else {
                    "named symbols are not connected in the graph"
                };
                return Ok(ImportantSymbolsResult {
                    mode: ImportanceMode::Global,
                    seed_source: Some(seed_source),
                    reason: Some(reason.to_string()),
                    diff_paths_considered: None,
                    diff_paths_with_symbols: None,
                    ranking_hint: ranking_hint.clone(),
                    symbols: ranked.symbols,
                });
            }
            return Ok(ImportantSymbolsResult {
                mode: ImportanceMode::PersonalizedToChanges,
                seed_source: Some(seed_source),
                reason: None,
                diff_paths_considered: None,
                diff_paths_with_symbols: None,
                ranking_hint,
                symbols: ranked.symbols,
            });
        }

        // No explicit seed. CLI stays global-by-default; only the MCP default auto-seeds from diff.
        if !request.auto_seed_from_diff {
            return Ok(ImportantSymbolsResult {
                mode: ImportanceMode::Global,
                seed_source: None,
                reason: None,
                diff_paths_considered: None,
                diff_paths_with_symbols: None,
                ranking_hint: ranking_hint.clone(),
                symbols: rank(&[])?.symbols,
            });
        }

        let diff = self.diff_seed()?;
        let ranked = rank(&diff.symbol_ids)?;
        // The diff produced no effective graph seed — either no changed path mapped to an indexed
        // symbol (markdown/config/generated/deleted-only, parser gaps) OR the symbols it resolved
        // are isolated in the graph — so the ranking is actually global. Report it with counts and
        // the reason rather than mislabeling it personalized. (#142 review)
        if ranked.effective_seed_count == 0 {
            let reason = if diff.symbol_ids.is_empty() {
                "no symbols found in current diff"
            } else {
                "diff symbols are not connected in the graph"
            };
            return Ok(ImportantSymbolsResult {
                mode: ImportanceMode::Global,
                seed_source: Some(SeedSource {
                    kind: SeedKind::GitDiff,
                    changed_paths: diff.changed_paths,
                    indexed_paths: diff.indexed_paths,
                    symbol_seed_count: diff.symbol_ids.len() as u64,
                    effective_seed_count: 0,
                    skipped: diff.skipped,
                }),
                reason: Some(reason.to_string()),
                diff_paths_considered: Some(diff.changed_paths),
                diff_paths_with_symbols: Some(diff.indexed_paths),
                ranking_hint: ranking_hint.clone(),
                symbols: ranked.symbols,
            });
        }
        Ok(ImportantSymbolsResult {
            mode: ImportanceMode::PersonalizedToChanges,
            seed_source: Some(SeedSource {
                kind: SeedKind::GitDiff,
                changed_paths: diff.changed_paths,
                indexed_paths: diff.indexed_paths,
                symbol_seed_count: diff.symbol_ids.len() as u64,
                effective_seed_count: ranked.effective_seed_count,
                skipped: diff.skipped,
            }),
            reason: None,
            diff_paths_considered: None,
            diff_paths_with_symbols: None,
            ranking_hint,
            symbols: ranked.symbols,
        })
    }

    /// Resolve a mixed list of explicit seed selectors (numeric symbol ids, symbol paths, or bare
    /// names) to in-index symbol ids at the query boundary. A numeric string is a raw symbol id; a
    /// name that resolves to nothing in the active scope is SKIPPED (counted in `unresolved`),
    /// never fatal — one bad name must not sink the whole call. Resolution order per
    /// non-numeric entry: `symbol_path` (EXACT qualified name) first; only if that resolves to
    /// exactly one symbol do we use it — otherwise we fall through to a bare-NAME lookup.
    ///
    /// Personalization is a teleport SET, not a single-symbol picker: a bare name therefore seeds
    /// ALL of its in-scope matches (the type PLUS its `impl` blocks/methods all carry the type's
    /// name — that whole entity is exactly what we want to bias toward), rather than skipping on
    /// ambiguity the way a `memory rebind`-style resolver would. Skipping on >1 match was the Phase
    /// 4 UX bug: any type with impls (essentially every type) matched >1 symbol, so the
    /// headline `--personalize <Type>` resolved to nothing and silently fell back to global
    /// ranking.
    fn resolve_seed_selectors(&self, selectors: &[String]) -> anyhow::Result<ResolvedSeeds> {
        use rag_rat_query::symbol::SymbolSelector;

        // Cap per-name expansion so a very common name (matched by hundreds of symbols) can't flood
        // the teleport set and wash out the signal. 25 comfortably covers a type plus its impls/
        // methods (the intended entity) while bounding pathological names; the overall `symbol_ids`
        // is sort+dedup'd below so a name and an explicit id that overlap don't double-count.
        const PER_NAME_SEED_CAP: u32 = 25;

        let mut symbol_ids = Vec::new();
        let mut unresolved = 0_u64;
        for raw in selectors {
            let entry = raw.trim();
            if entry.is_empty() {
                continue;
            }
            // An opaque `sym_<hex>` handle — what every symbol-returning tool now emits as the id
            // to feed back here (#149). Resolve the logical symbol to its in-scope
            // member rowids. A raw numeric id is deliberately NOT accepted: the wire
            // dropped `symbol_id` (reindex-churned), so a bare number would be a stale
            // rowid that silently seeds the wrong symbol.
            if entry.starts_with("sym_") {
                let Some(logical_symbol_id) = rag_rat_base::serde_big_id::parse_sym_handle(entry)
                else {
                    unresolved += 1;
                    continue;
                };
                let by_handle = SymbolSelector {
                    logical_symbol_id: Some(logical_symbol_id),
                    symbol_id: None,
                    symbol_path: None,
                    symbol: None,
                    language: None,
                    allow_ambiguous: true,
                    limit: PER_NAME_SEED_CAP,
                };
                // Importance seeding spans the whole graph; generated bindings are legitimate
                // nodes here, so keep them (`include_generated: true`) — the #202 filter is for
                // user-facing symbol search, not PageRank seeds. (A logical-id selector isn't
                // filtered anyway, but pass it explicitly so the intent survives a refactor.)
                let members = rag_rat_query::symbol::lookup_candidates(
                    self.storage.connection(),
                    &by_handle,
                    true,
                )?
                .candidates;
                if members.is_empty() {
                    unresolved += 1;
                } else {
                    symbol_ids.extend(members.into_iter().map(|hit| hit.symbol_id));
                }
                continue;
            }
            // Try `symbol_path` (EXACT qualified name) FIRST: an unambiguous fully-qualified name
            // resolves to exactly one symbol and we use it as-is. `allow_ambiguous: false` makes a
            // multi-candidate qualified name resolve to `Err(disambiguation)` → fall through to the
            // bare-name expansion below; a missing one is `Ok(None)` → also fall through.
            let by_path = SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: Some(entry.to_string()),
                symbol: None,
                language: None,
                allow_ambiguous: false,
                limit: 8,
            };
            if let Ok(Some(hit)) = self.select_symbol(&by_path)? {
                symbol_ids.push(hit.symbol_id);
                continue;
            }
            // Bare-NAME fallback: resolve to ALL in-scope matches (capped) and seed every one.
            // `symbol_candidates` → `lookup_candidates` reads through the per-connection scoped
            // `files` view (overlay rows win, non-active checkouts excluded), so these ids are
            // already scope-correct — keep that path; do not query raw tables.
            let by_name = SymbolSelector {
                symbol_path: None,
                symbol: Some(entry.to_string()),
                allow_ambiguous: true,
                limit: PER_NAME_SEED_CAP,
                ..by_path
            };
            // Use the UNENRICHED lookup: seed resolution only needs `symbol_id`, and the enriched
            // `symbol_candidates` would fetch the oracle effect map + run a fan-in query per hit —
            // a whole-graph oracle scan per seed name, repeated, all discarded here (#142 review).
            let candidates = rag_rat_query::symbol::lookup_candidates(
                self.storage.connection(),
                &by_name,
                true, // whole-graph seeding keeps generated nodes (see by_handle note above)
            )?
            .candidates;
            if candidates.is_empty() {
                unresolved += 1;
            } else {
                symbol_ids.extend(candidates.into_iter().map(|hit| hit.symbol_id));
            }
        }
        symbol_ids.sort_unstable();
        symbol_ids.dedup();
        Ok(ResolvedSeeds { symbol_ids, unresolved })
    }

    /// Auto-seed from the current git diff (the MCP default). Maps the changed paths through the
    /// per-connection scoped `files` view to in-scope symbol ids, bucketing the paths that
    /// contribute no seed (deleted, generated, no-symbols) for provenance.
    fn diff_seed(&self) -> anyhow::Result<DiffSeed> {
        let Some(root) = self.storage.source_root() else {
            // No source root → no working tree to diff (e.g. a bare/copied index). Treat as an
            // empty diff, not an error: the caller falls through to global with a reason.
            return Ok(DiffSeed::default());
        };
        // A configured source root that is not a git worktree (or has no HEAD, or git is absent)
        // must NOT fail the whole tool: auto-seed-from-diff is a best-effort default, so treat any
        // git error as an empty diff and let the caller fall through to global — mirroring the
        // other index paths that tolerate missing git with empty metadata. (#142 review)
        let Ok(changed) = crate::index::git_changed_paths(root) else {
            return Ok(DiffSeed::default());
        };
        let changed_paths = (changed.changed.len() + changed.deleted.len()) as u64;
        let mut seed = DiffSeed { changed_paths, ..Default::default() };
        // Deleted/renamed-away paths can never carry an in-scope symbol — count and skip them.
        seed.skipped.deleted = changed.deleted.len() as u64;

        let mut symbol_ids = Vec::new();
        for path in &changed.changed {
            let path = crate::index::path_string_for_seed(path);
            match self.symbol_ids_for_changed_path(&path)? {
                ChangedPathSymbols::Symbols(ids) if !ids.is_empty() => {
                    seed.indexed_paths += 1;
                    symbol_ids.extend(ids);
                },
                // Indexed as a generated artifact: real but deliberately excluded from the seed.
                ChangedPathSymbols::Generated => seed.skipped.generated += 1,
                // In the working set but contributes no in-scope symbol (config/markdown, parser
                // gap, or not indexed at all).
                ChangedPathSymbols::Symbols(_) | ChangedPathSymbols::None =>
                    seed.skipped.no_symbols += 1,
            }
        }
        symbol_ids.sort_unstable();
        symbol_ids.dedup();
        seed.symbol_ids = symbol_ids;
        Ok(seed)
    }

    /// Map ONE changed path to its in-scope symbol ids, classifying via the per-connection scoped
    /// `files` view. SCOPED-VIEW REQUIREMENT (#89): the JOIN goes through `files` (the TEMP VIEW
    /// installed per connection — overlay rows win, other commits/worktrees excluded), NEVER raw
    /// `main.symbols`/`main.files`. Querying raw tables here would seed PageRank from symbols
    /// belonging to a non-active checkout (or shadowed committed rows), corrupting a per-scope
    /// ranking with cross-scope identity — the exact failure the scope view exists to prevent.
    fn symbol_ids_for_changed_path(&self, path: &str) -> anyhow::Result<ChangedPathSymbols> {
        let conn = self.storage.connection();
        // First: is the path indexed in the active scope at all, and is it generated? `files` is
        // the scoped view, so a path outside the active checkout returns no row → `None`.
        let generated: Option<bool> = conn
            .query_row("SELECT generated FROM files WHERE path = ?1", [path], |row| {
                row.get::<_, i64>(0).map(|flag| flag != 0)
            })
            .optional()?;
        let Some(generated) = generated else {
            return Ok(ChangedPathSymbols::None);
        };
        if generated {
            return Ok(ChangedPathSymbols::Generated);
        }
        // SCOPED-VIEW REQUIREMENT (#89): join symbols to the `files` scope view, not raw tables, so
        // only symbols of the ACTIVE version of this file become PageRank seeds.
        let mut stmt = conn.prepare(
            "SELECT symbols.id
             FROM symbols
             JOIN files ON files.id = symbols.file_id
             WHERE files.path = ?1",
        )?;
        let ids =
            stmt.query_map([path], |row| row.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ChangedPathSymbols::Symbols(ids))
    }

    /// Build the `edge_id -> EdgeOracleEffect` map that makes [`Self::important_symbols`]
    /// SCIP-aware, merging current+in-scope verdicts across every oracle tool that has a run in
    /// this checkout. Returns `None` when no run exists — the common case, where ranking pays zero
    /// oracle cost (one existence probe short-circuits the per-tool version lookups + the
    /// whole-graph verdict scan). Maps `OracleResolutionKind` to a ranking effect here so
    /// `query::pagerank` stays free of oracle types:
    /// - the compiler resolved an in-corpus target (`Upgrade` or `Contradict` with a resolved
    ///   symbol) → **retarget** the edge there (the compiler's answer, whether it upgrades an
    ///   unconfirmed edge or overrides a wrong heuristic one);
    /// - `Confirm` → verify the heuristic target in place;
    /// - `ResolvedExternal`, or a `Contradict` with no in-corpus target → **drop** the phantom edge
    ///   (the real callee is out of corpus);
    /// - an `Upgrade` with no resolved target → leave the edge heuristic (unconfirmed, not refuted
    ///   — #82 finding 2).
    fn symbol_importance_oracle_effects(
        &self,
    ) -> anyhow::Result<
        Option<std::collections::HashMap<i64, rag_rat_query::pagerank::EdgeOracleEffect>>,
    > {
        use rag_rat_oracle::OracleResolutionKind as Kind;
        use rag_rat_query::pagerank::EdgeOracleEffect;
        // CPU gate: one scoped existence query, so the dominant "no oracle ever" path skips the
        // per-tool version lookups and the whole-graph verdict scan entirely.
        if !rag_rat_oracle::any_run_in_scope(
            self.storage.connection(),
            &self.active_commit_sha,
            &self.active_worktree_id,
        )? {
            return Ok(None);
        }
        let mut effects: Option<std::collections::HashMap<i64, EdgeOracleEffect>> = None;
        // CANONICAL tools first (#534): canonical verdict sets are disjoint across languages, but a
        // patch tool (`ra-lsp`) overlaps its canonical counterpart on the same Rust edges. So
        // first-writer-wins per edge_id gives batch-wins-on-overlap — deterministically, off the
        // declared authority rather than `ALL`'s declaration order.
        let mut tools = rag_rat_oracle::OracleTool::ALL.to_vec();
        tools.sort_by_key(|tool| tool.authority());
        for tool in tools {
            let Some(version) = self.latest_oracle_run_version(tool)? else {
                continue;
            };
            let verdicts = rag_rat_oracle::current_oracle_verdicts_all(
                self.storage.connection(),
                tool,
                &version,
                &self.active_commit_sha,
                &self.active_worktree_id,
            )?;
            let map = effects.get_or_insert_with(std::collections::HashMap::new);
            for (edge_id, (kind, resolved_symbol_id)) in verdicts {
                let effect = match (kind, resolved_symbol_id) {
                    // Out-of-corpus callee: the in-repo target is a phantom either way.
                    (Kind::ResolvedExternal, _) | (Kind::Contradict, None) =>
                        EdgeOracleEffect::Drop,
                    (Kind::Confirm, _) => EdgeOracleEffect::Confirm,
                    // Compiler resolved an in-corpus target — trust it over the heuristic.
                    (Kind::Upgrade | Kind::Contradict, Some(id)) => EdgeOracleEffect::Retarget(id),
                    // Upgrade we can't name a target for: leave the edge heuristic.
                    (Kind::Upgrade, None) => continue,
                };
                // First writer wins: the batch tool's effect stands over a live duplicate for
                // the same edge (batch is canonical); disjoint-language batch tools never
                // collide with each other.
                map.entry(edge_id).or_insert(effect);
            }
        }
        Ok(effects)
    }

    /// The active-scope symbol id for a qualified name, resolved THROUGH the per-connection `files`
    /// scope view so a foreign scope's same-named symbol never matches (the same #89 discipline the
    /// fan-in query uses). `None` when no in-scope symbol has that qualified name. When more than
    /// one in-scope symbol shares the name (overloads / cfg twins) the lowest id is returned —
    /// the fan-in is computed per concrete symbol id, and the load-bearing signal is a coarse
    /// bucket, so picking a stable representative is acceptable for the enrichment.
    pub(crate) fn active_symbol_id_for_qualified_name(
        &self,
        qualified_name: &str,
    ) -> anyhow::Result<Option<i64>> {
        Ok(self
            .storage
            .connection()
            .query_row(
                "SELECT s.id FROM symbols s
                 JOIN files ON files.id = s.file_id
                 WHERE s.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
                 ORDER BY s.id
                 LIMIT 1",
                [qualified_name],
                |row| row.get::<_, i64>(0),
            )
            .optional()?)
    }

    /// Build the load-bearing oracle context ONCE for an enrichment call: reuse the same gated
    /// verdict map `important_symbols` uses (a single existence probe short-circuits the
    /// no-oracle-ever path), and hold it for the whole pass so no symbol triggers its own verdict
    /// scan. The returned owned map is borrowed into an [`OracleContext`] per symbol below.
    pub(super) fn load_bearing_oracle_effects(
        &self,
    ) -> anyhow::Result<
        Option<std::collections::HashMap<i64, rag_rat_query::pagerank::EdgeOracleEffect>>,
    > {
        self.symbol_importance_oracle_effects()
    }

    /// Attach the LOCAL structural-load enrichment (scoped weighted fan-in — the third importance
    /// scale, NOT PageRank) to `impact_surface` neighbors. The neighbor whose load we score is the
    /// edge's FAR end: for a CALLER hop that's `from_symbol`, for a CALLEE hop that's `to_symbol`.
    /// The oracle effect map is fetched ONCE and reused across every hop.
    pub(super) fn enrich_neighbors_with_load_bearing(
        &self,
        callers: &mut [rag_rat_query::graph::GraphHop],
        callees: &mut [rag_rat_query::graph::GraphHop],
    ) -> anyhow::Result<()> {
        use rag_rat_query::load_bearing::{self, OracleContext};
        // Nothing to enrich → don't pay the oracle lookup. (#142 review)
        if callers.is_empty() && callees.is_empty() {
            return Ok(());
        }
        let effects = self.load_bearing_oracle_effects()?;
        let oracle = OracleContext { effects: effects.as_ref() };
        let enrich = |hop: &mut rag_rat_query::graph::GraphHop,
                      neighbor: Option<&str>|
         -> anyhow::Result<()> {
            let Some(name) = neighbor else { return Ok(()) };
            let Some(symbol_id) = self.active_symbol_id_for_qualified_name(name)? else {
                return Ok(());
            };
            hop.importance = load_bearing::scoped_weighted_fan_in(
                self.storage.connection(),
                symbol_id,
                &oracle,
            )?;
            Ok(())
        };
        for hop in callers.iter_mut() {
            let neighbor = hop.from_symbol.clone();
            enrich(hop, neighbor.as_deref())?;
        }
        for hop in callees.iter_mut() {
            let neighbor = hop.to_symbol.clone().or_else(|| hop.target_qualified_name.clone());
            enrich(hop, neighbor.as_deref())?;
        }
        Ok(())
    }

    /// Attach the load-bearing enrichment to search hits, scoring each hit's symbol (resolved from
    /// `chunk.symbol_path`, which is the chunk's qualified name) through the active scope. Hits
    /// with no symbol, or whose symbol has no in-scope in-edges, are left un-enriched. One
    /// oracle fetch for the whole batch.
    pub(super) fn enrich_search_hits_with_load_bearing(
        &self,
        hits: &mut [SearchHit],
    ) -> anyhow::Result<()> {
        use rag_rat_query::load_bearing::{self, OracleContext};
        // Nothing enrichable → don't pay the (whole-graph) oracle lookup. A result made entirely of
        // file/doc chunks with no `symbol_path` (common for Markdown/config) would otherwise scan
        // every oracle verdict and then skip every hit. (#142 review)
        if hits.iter().all(|hit| hit.symbol_path.is_none()) {
            return Ok(());
        }
        let effects = self.load_bearing_oracle_effects()?;
        let oracle = OracleContext { effects: effects.as_ref() };
        for hit in hits.iter_mut() {
            let Some(symbol_path) = hit.symbol_path.clone() else { continue };
            let Some(symbol_id) = self.active_symbol_id_for_qualified_name(&symbol_path)? else {
                continue;
            };
            hit.importance = load_bearing::scoped_weighted_fan_in(
                self.storage.connection(),
                symbol_id,
                &oracle,
            )?;
        }
        Ok(())
    }

    /// Attach the load-bearing enrichment to `symbol_lookup` hits (each carries its own
    /// `symbol_id`). One oracle fetch for the whole batch.
    pub(super) fn enrich_symbol_hits_with_load_bearing(
        &self,
        hits: &mut [rag_rat_query::symbol::SymbolHit],
    ) -> anyhow::Result<()> {
        use rag_rat_query::load_bearing::{self, OracleContext};
        // Nothing to enrich → don't pay the oracle lookup. (#142 review)
        if hits.is_empty() {
            return Ok(());
        }
        let effects = self.load_bearing_oracle_effects()?;
        let oracle = OracleContext { effects: effects.as_ref() };
        for hit in hits.iter_mut() {
            hit.importance = load_bearing::scoped_weighted_fan_in(
                self.storage.connection(),
                hit.symbol_id,
                &oracle,
            )?;
        }
        Ok(())
    }
}
