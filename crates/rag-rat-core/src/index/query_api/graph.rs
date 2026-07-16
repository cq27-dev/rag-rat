//! Graph-query surface on `IndexDatabase`: caller/callee traversal (find_callers / trace_callees),
//! impact_surface / ffi_surface, and the graph-vs-text / graph-vs-scip completeness comparisons.

use super::{annotate_completeness_with_externals, resolved_external_label, *};

impl IndexDatabase {
    pub fn ffi_surface(&self, limit: u32) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::query::impact::ffi_surface(self.storage.connection(), limit)
    }

    pub fn find_callers(
        &self,
        symbol: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        crate::query::graph::traverse(self.storage.connection(), symbol, true, limit)
    }

    /// `check_library_usage` (#114): join the active checkout's `resolved-external` call sites to
    /// the `external_symbols` dependency contracts, surface each dependency's current
    /// signature/docs as context, and assert deprecated-but-compiling usage. Read-only;
    /// requires an `oracle run` to have populated the side tables (else a `NoOracleRun` /
    /// `NoExternalSymbols` status).
    pub fn check_library_usage(
        &self,
        opts: &crate::index::oracle::LibraryUsageOptions,
    ) -> anyhow::Result<crate::index::oracle::LibraryUsageReport> {
        crate::index::oracle::check_library_usage(
            self.storage.connection(),
            &self.active_commit_sha,
            &self.active_worktree_id,
            opts,
        )
    }

    pub fn find_callers_with_options(
        &self,
        symbol: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        let options = self.graph_options_with_logical_group(options)?;
        self.traverse_with_oracle(symbol, true, limit, &options)
    }

    /// Upgrade graph hops to the `Compiler` confidence tier where a CURRENT, in-scope `edge_oracle`
    /// verdict covers the edge — the read-side surfacing for `trace_callees` / `find_callers` /
    /// `impact_surface`. The heuristic `edges` row is NEVER mutated (side-table invariant); this
    /// JOINs `edge_oracle` (scoped to the active checkout AND filtered to current content via
    /// `file_sha == files.sha256`) at read time and rewrites only the in-memory hop's display
    /// fields:
    /// - `Upgrade`/`Confirm` (in-corpus compiler resolution) → `confidence = "compiler"` with
    ///   `resolution_reason = "scip:<tool>@<version>"`. A drifted file's verdict is excluded by the
    ///   current-content filter, so the hop reverts to heuristic display (never `compiler`).
    /// - `ResolvedExternal` → `resolved_external = "resolved-external(<package>)"` + the reason;
    ///   the confidence stays heuristic (the callee is outside the corpus, not an in-corpus
    ///   upgrade).
    /// - `Contradict` is NOT surfaced as `compiler`: the oracle disagrees with the heuristic
    ///   target, so promoting it would assert a resolution we don't stand behind — it stays
    ///   heuristic (`compare_graph_to_scip` is where contradictions surface).
    ///
    /// Verdicts bind to whichever files are the ACTIVE version of the checkout, not to
    /// commit-scoped rows specifically: on a clean tree that's the committed `(sha,'')` rows;
    /// on a dirty tree the worktree-dirty `('',wt)` overlay rows shadow them. The shared
    /// active-checkout predicate (the #82 P0 fix) selects exactly one version per file, so a
    /// verdict surfaces only for the version in play — a verdict written against a file that
    /// has since been overlaid (or that belongs to a non-active checkout) drops out of scope
    /// and the hop reverts to heuristic, with no special-casing.
    /// Returns whether any hop was PROMOTED to the `compiler` tier — i.e. whether enrichment
    /// changed a hop's `effective_confidence_rank`. Only an `Upgrade`/`Confirm` that became
    /// `compiler` changes ranking; a `ResolvedExternal` sets a label but leaves the confidence
    /// heuristic, so it does NOT count. The caller uses this to decide whether the
    /// overfetch+re-sort is needed: with no promotion the heuristic order + the caller's
    /// original `limit` are already correct and must be left untouched (#82 P2 — the
    /// unconditional re-sort changed truncation membership on EVERY query, including repos with
    /// no oracle run).
    fn enrich_hops_with_oracle(
        &self,
        hops: &mut [crate::query::graph::GraphHop],
    ) -> anyhow::Result<bool> {
        if hops.is_empty() {
            return Ok(false);
        }
        // Merge verdicts from EVERY oracle backend that has a run in this checkout, not just
        // rust-analyzer (#176): a mixed-language repo has rust-analyzer/scip-clang/scip-python
        // runs, and a Python (or C) edge's `compiler` tier lives under that tool's verdicts. An
        // edge belongs to one language, so the per-tool verdict sets are disjoint — merging can't
        // collide.
        let conn = self.storage.connection();
        let runs =
            oracle::latest_runs_in_scope(conn, &self.active_commit_sha, &self.active_worktree_id)?;
        if runs.is_empty() {
            // No oracle run for this checkout — nothing to surface, all hops stay heuristic.
            return Ok(false);
        }
        let edge_ids = hops.iter().map(|hop| hop.edge_id).collect::<Vec<_>>();
        let mut verdicts = std::collections::HashMap::new();
        for (tool, tool_version) in &runs {
            verdicts.extend(oracle::current_oracle_verdicts_for_edges(
                conn,
                *tool,
                tool_version,
                &self.active_commit_sha,
                &self.active_worktree_id,
                &edge_ids,
            )?);
        }
        if verdicts.is_empty() {
            return Ok(false);
        }
        let mut promoted_any = false;
        for hop in hops.iter_mut() {
            let Some(verdict) = verdicts.get(&hop.edge_id) else {
                continue;
            };
            match verdict.kind {
                oracle::OracleResolutionKind::Upgrade | oracle::OracleResolutionKind::Confirm => {
                    // Hydrate the hop's target from the compiler's `resolved_symbol_id` BEFORE
                    // promoting to `compiler` — for an `Upgrade` on a `NameOnly`/`Ambiguous` edge
                    // the heuristic target is missing or wrong, so promoting confidence without
                    // moving the target would attach the compiler tier to a heuristic/absent
                    // target (#82 finding 2). If the resolved symbol can't be surfaced (it was
                    // deleted/reinserted, so its qualified name is gone — though the def-drift gate
                    // in `edge_oracle_current_predicate` already filters that case), do NOT
                    // promote.
                    let Some(resolved_name) = verdict.resolved_qualified_name.clone() else {
                        continue;
                    };
                    hop.to_symbol = Some(resolved_name.clone());
                    hop.target_qualified_name = Some(resolved_name);
                    hop.verified_target_symbol = true;
                    hop.confidence = "compiler".to_string();
                    hop.resolution_reason = Some(verdict.resolution_reason());
                    promoted_any = true;
                },
                oracle::OracleResolutionKind::ResolvedExternal => {
                    hop.resolved_external = verdict.resolved_external_label();
                    hop.resolution_reason = Some(verdict.resolution_reason());
                },
                // The oracle disagrees with the heuristic target — do not promote to `compiler`.
                oracle::OracleResolutionKind::Contradict => {},
            }
        }
        Ok(promoted_any)
    }

    /// Traverse, surface the `Compiler` tier, then rank-and-truncate so a compiler-upgraded edge is
    /// never dropped by the heuristic limit (#82 finding 4).
    ///
    /// The heuristic `traverse_with_options` orders by heuristic confidence and applies `LIMIT`
    /// BEFORE oracle enrichment runs — so a low-confidence edge the compiler would upgrade to
    /// `compiler` (the tier ABOVE `exact`) can fall below the cutoff and never be fetched, even
    /// though it should outrank the `exact`/`syntactic` neighbors that displaced it. To fix the
    /// ordering we OVERFETCH (traverse with an inflated cap), enrich the larger candidate set,
    /// RE-SORT by EFFECTIVE confidence (`compiler` > `exact` > `syntactic` > `name_only` >
    /// `ambiguous`) with a stable tiebreak on the heuristic order, and only THEN truncate to
    /// `limit`. The overfetch cap is bounded so a huge `limit` can't blow up the candidate set; an
    /// edge upgraded beyond the overfetch window is the residual we accept (the heuristic already
    /// ranked it far down, and the window is generous).
    fn traverse_with_oracle(
        &self,
        symbol: &str,
        reverse: bool,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        let overfetch = crate::query::graph::oracle_overfetch_limit(limit);
        let mut hops = crate::query::graph::traverse_with_options(
            self.storage.connection(),
            symbol,
            reverse,
            overfetch,
            options,
        )?;
        let promoted = self.enrich_hops_with_oracle(&mut hops)?;
        // Only re-rank when a hop was actually PROMOTED to `compiler` (#82 P2). With no promotion
        // the heuristic SQL order already ranks the candidates correctly, so re-sorting would only
        // perturb truncation membership for free (it demotes `match_tier` to a within-confidence
        // tiebreak) — including on every query in a repo with no oracle run. When nothing was
        // promoted, keep the heuristic order and the caller's original `limit`: the overfetched set
        // is in heuristic order, so its first `limit` rows ARE the original top-`limit`.
        if promoted {
            // Stable sort by effective (post-enrichment) confidence so a `compiler` upgrade rises
            // above the heuristic `exact`/`syntactic` edges that out-ranked it in the SQL ORDER BY.
            // Stable keeps the heuristic order (the `match_tier` primary key) within a tier.
            hops.sort_by_key(|hop| crate::query::graph::effective_confidence_rank(&hop.confidence));
        }
        hops.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        Ok(hops)
    }

    pub fn trace_callees(
        &self,
        symbol: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        crate::query::graph::traverse(self.storage.connection(), symbol, false, limit)
    }

    pub fn trace_callees_with_options(
        &self,
        symbol: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<Vec<crate::query::graph::GraphHop>> {
        let options = self.graph_options_with_logical_group(options)?;
        self.traverse_with_oracle(symbol, false, limit, &options)
    }

    pub fn graph_traversal_report(
        &self,
        tool: &str,
        symbol: &crate::query::symbol::SymbolHit,
        reverse: bool,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<crate::query::graph::GraphTraversalReport> {
        let options = self.graph_options_with_logical_group(options)?;
        // Overfetch + enrich + re-rank + truncate so a compiler-upgraded edge survives the limit
        // (#82 finding 4). `traversal_summary` below still describes the FULL matching population
        // (its own COUNT query, independent of the returned window), so passing the truncated
        // `results.len()` as the returned count stays correct.
        let results =
            self.traverse_with_oracle(&symbol.qualified_name, reverse, limit, &options)?;
        let mut summary = crate::query::graph::traversal_summary(
            self.storage.connection(),
            &symbol.qualified_name,
            reverse,
            limit,
            &options,
            results.len(),
        )?;
        // Make `completeness_risk` quantitative where the oracle covers the unresolved neighbors:
        // append a clause like "2 of 7 unresolved are resolved-external: libc, tokio". This is a
        // read-time annotation derived from the surfaced `resolved-external` verdicts — the risk
        // *level* string is unchanged; the clause just tells the caller how much of the gap is a
        // known external dependency rather than a resolver miss.
        annotate_completeness_with_externals(&mut summary, &results);
        let (logical_symbol, variants) =
            self.read_graph_logical_symbol(options.logical_symbol_id)?;
        let mut paths = BTreeSet::new();
        paths.insert(symbol.path.clone());
        for result in &results {
            if let Some(callsite) = &result.callsite {
                paths.insert(callsite.path.clone());
            }
        }
        let mut coverage = self.graph_coverage(paths)?;
        if summary.unresolved > 0 {
            coverage.known_index_gaps.push(format!(
                "{} unresolved qualified callsites match the requested final segment but are not \
                 verified to this symbol",
                summary.unresolved
            ));
        }
        Ok(crate::query::graph::GraphTraversalReport {
            query: crate::query::graph::GraphTraversalQuery {
                tool: tool.to_string(),
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: options.logical_symbol_id,
                symbol_path: symbol.qualified_name.clone(),
                resolution: options.resolution_mode.as_str().to_string(),
            },
            logical_symbol,
            variants,
            summary,
            coverage,
            results,
        })
    }

    pub fn compare_graph_to_text(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        pattern: &str,
        limit: u32,
        options: &crate::query::graph::GraphTraversalOptions,
        include_tests: bool,
    ) -> anyhow::Result<crate::query::graph::CompareGraphTextReport> {
        let regex = Regex::new(pattern)?;
        let options = self.graph_options_with_logical_group(options)?;
        let mut graph_edges = crate::query::graph::traverse_with_options(
            self.storage.connection(),
            &symbol.qualified_name,
            true,
            limit,
            &options,
        )?;
        if !include_tests {
            graph_edges.retain(|edge| {
                edge.callsite
                    .as_ref()
                    .is_none_or(|callsite| !crate::index::parser::is_test_path(&callsite.path))
            });
        }
        let (logical_symbol, variants) =
            self.read_graph_logical_symbol(options.logical_symbol_id)?;
        let text_hits = self.find_regex_hits(pattern, &regex, include_tests)?;
        let text_by_location = text_hits
            .iter()
            .map(|hit| ((hit.path.clone(), hit.line), hit))
            .collect::<BTreeMap<_, _>>();
        let graph_by_location = graph_edges
            .iter()
            .filter_map(|edge| {
                edge.callsite
                    .as_ref()
                    .map(|callsite| ((callsite.path.clone(), callsite.line), edge))
            })
            .collect::<BTreeMap<_, _>>();

        let mut paths = BTreeSet::new();
        paths.insert(symbol.path.clone());
        for hit in &text_hits {
            paths.insert(hit.path.clone());
        }
        for edge in &graph_edges {
            if let Some(callsite) = &edge.callsite {
                paths.insert(callsite.path.clone());
            }
        }

        let parser_failure_paths = self
            .parser_failure_paths()?
            .into_iter()
            .map(|failure| failure.path)
            .collect::<BTreeSet<_>>();
        let mut matched_hits = Vec::new();
        let mut text_only_hits = Vec::new();
        let mut likely_parser_gaps = Vec::new();
        for hit in &text_hits {
            if let Some(edge) = graph_by_location.get(&(hit.path.clone(), hit.line)) {
                matched_hits.push(crate::query::graph::MatchedGraphTextHit {
                    path: hit.path.clone(),
                    line: hit.line,
                    text: hit.text.clone(),
                    target: edge.target.clone(),
                    edge_kind: edge.edge_kind.clone(),
                    confidence: edge.confidence.clone(),
                    resolution: edge.resolution.clone(),
                });
            } else {
                let gap_kind = classify_text_only_hit(&hit.path, &hit.text, &parser_failure_paths);
                let text_only_hit = crate::query::graph::TextOnlyHit {
                    path: hit.path.clone(),
                    line: hit.line,
                    text: hit.text.clone(),
                    reason: if gap_kind == "parser_call_extraction" || gap_kind == "parser_failure"
                    {
                        "no graph edge extracted"
                    } else {
                        "text mention outside graph-call evidence"
                    }
                    .to_string(),
                    likely_gap: gap_kind.to_string(),
                };
                if is_likely_parser_gap_kind(gap_kind) {
                    likely_parser_gaps.push(text_only_hit.clone());
                }
                text_only_hits.push(text_only_hit);
            }
        }

        let mut graph_only_edges = Vec::new();
        let mut likely_false_positives = Vec::new();
        for edge in &graph_edges {
            let Some(callsite) = &edge.callsite else {
                continue;
            };
            if text_by_location.contains_key(&(callsite.path.clone(), callsite.line)) {
                continue;
            }
            let current_line = self.read_current_line_text(&callsite.path, callsite.line)?;
            let graph_only = crate::query::graph::GraphOnlyEdge {
                path: callsite.path.clone(),
                line: callsite.line,
                target: edge.target.clone(),
                edge_kind: edge.edge_kind.clone(),
                confidence: edge.confidence.clone(),
                resolution: edge.resolution.clone(),
                evidence: edge.evidence.clone(),
                reason: "graph edge exists but pattern did not match text".to_string(),
                likely_reason: graph_only_reason(edge, current_line.as_deref()),
            };
            if is_likely_false_positive_graph_only(edge, &graph_only) {
                likely_false_positives.push(graph_only.clone());
            }
            graph_only_edges.push(graph_only);
        }
        let complete = likely_parser_gaps.is_empty() && likely_false_positives.is_empty();
        let recommended_fallback =
            recommended_graph_text_fallback(&likely_parser_gaps, &graph_only_edges);
        let pattern_match_mode = compare_pattern_match_mode(pattern, &symbol.name);
        let mut warnings = Vec::new();
        if pattern_match_mode == "substring_identifier" {
            warnings.push(format!(
                "pattern may match identifiers that merely contain `{}`; use an identifier \
                 boundary or escaped call suffix for exact text auditing",
                symbol.name
            ));
        }

        Ok(crate::query::graph::CompareGraphTextReport {
            query: crate::query::graph::CompareGraphTextQuery {
                symbol_id: Some(symbol.symbol_id),
                logical_symbol_id: options.logical_symbol_id,
                symbol_path: symbol.qualified_name.clone(),
                pattern: pattern.to_string(),
                resolution: options.resolution_mode.as_str().to_string(),
            },
            logical_symbol,
            variants,
            summary: crate::query::graph::CompareGraphTextSummary {
                graph_hits: u64::try_from(graph_edges.len()).unwrap_or(u64::MAX),
                graph_edges: u64::try_from(graph_edges.len()).unwrap_or(u64::MAX),
                text_hits: u64::try_from(text_hits.len()).unwrap_or(u64::MAX),
                matched: u64::try_from(matched_hits.len()).unwrap_or(u64::MAX),
                graph_only: u64::try_from(graph_only_edges.len()).unwrap_or(u64::MAX),
                text_only: u64::try_from(text_only_hits.len()).unwrap_or(u64::MAX),
                text_mentions: u64::try_from(text_only_hits.len() - likely_parser_gaps.len())
                    .unwrap_or(u64::MAX),
                likely_parser_gaps: u64::try_from(likely_parser_gaps.len()).unwrap_or(u64::MAX),
                likely_false_positives: u64::try_from(likely_false_positives.len())
                    .unwrap_or(u64::MAX),
                likely_index_gaps: u64::try_from(likely_parser_gaps.len()).unwrap_or(u64::MAX),
                complete,
                recommended_fallback,
                pattern_match_mode,
                warnings,
            },
            coverage: self.graph_coverage(paths)?,
            matched_hits,
            text_only_hits,
            graph_only_edges,
            likely_parser_gaps,
            likely_false_positives,
        })
    }

    /// `compare_graph_to_scip` — report where tree-sitter and the compiler (SCIP) DISAGREE on edge
    /// resolution: the `Contradict` verdicts in `edge_oracle` (the heuristic resolved an edge to an
    /// in-corpus target the compiler says is wrong, OR resolved in-corpus while the compiler placed
    /// the callee in a dependency). A user diagnostic + our own resolver-debugging instrument,
    /// sibling of `compare_graph_to_text`.
    ///
    /// Scoped + current ONLY: reads through the store's scope+current join, so a sibling worktree's
    /// or a drifted/dirty file's verdict is never reported. When no oracle run has populated this
    /// checkout, `no_oracle_data` is set and the contradiction list is empty (the graph isn't
    /// "verified to agree" — there's just nothing to compare). The heuristic `edges` row is never
    /// mutated; this is pure read-time diffing.
    pub fn compare_graph_to_scip(
        &self,
    ) -> anyhow::Result<crate::query::graph::CompareGraphScipReport> {
        let conn = self.storage.connection();
        // Compare against EVERY backend with a run in this checkout, not just rust-analyzer (#176):
        // a mixed-language repo's contradictions span tools (a C edge under scip-clang, a Python
        // edge under scip-python). Verdict sets are disjoint by edge language, so aggregating is a
        // plain concatenation.
        let runs =
            oracle::latest_runs_in_scope(conn, &self.active_commit_sha, &self.active_worktree_id)?;
        let mut summary = crate::query::graph::CompareGraphScipSummary::default();
        let mut contradictions = Vec::new();
        if runs.is_empty() {
            summary.no_oracle_data = true;
            summary.warnings.push(
                "no oracle run for this checkout; run `rag-rat oracle run` to populate compiler \
                 verdicts before comparing"
                    .to_string(),
            );
            return Ok(crate::query::graph::CompareGraphScipReport {
                query: crate::query::graph::CompareGraphScipQuery {
                    tool: String::new(),
                    tool_version: None,
                    commit_sha: self.active_commit_sha.clone(),
                    worktree_id: self.active_worktree_id.clone(),
                },
                summary,
                contradictions,
            });
        }
        for (tool, version) in &runs {
            let comparisons = oracle::current_oracle_comparisons(
                conn,
                *tool,
                version,
                &self.active_commit_sha,
                &self.active_worktree_id,
            )?;
            summary.verdicts_examined += u64::try_from(comparisons.len()).unwrap_or(u64::MAX);
            for comparison in comparisons {
                if comparison.kind != oracle::OracleResolutionKind::Contradict {
                    continue;
                }
                contradictions.push(crate::query::graph::GraphScipContradiction {
                    edge_id: comparison.edge_id,
                    edge_kind: comparison.edge_kind,
                    heuristic_confidence: crate::query::graph::normalize_confidence(
                        &comparison.heuristic_confidence,
                    )
                    .to_string(),
                    heuristic_target: comparison.heuristic_target,
                    callee_name: comparison.callee_name,
                    // Label `resolved-external` ONLY for a contradiction the compiler resolved
                    // OUTSIDE the corpus (`resolved_symbol_id IS NULL`). A Rust SCIP symbol carries
                    // a crate/package component even for the LOCAL crate (`scip-rust crate
                    // held-mini …`), so deriving the label from `scip_symbol`
                    // alone would mislabel an IN-CORPUS contradiction (the
                    // compiler resolved to a *different* in-corpus symbol) as
                    // `resolved-external(<local-crate>)` (#82 finding 1). An in-corpus
                    // contradiction is a same-corpus disagreement, not an external placement.
                    resolved_external: comparison
                        .resolved_symbol_id
                        .is_none()
                        .then(|| resolved_external_label(&comparison.scip_symbol))
                        .flatten(),
                    scip_symbol: comparison.scip_symbol,
                    callsite: Some(crate::query::graph::Callsite {
                        path: comparison.callsite_path,
                        line: comparison.callsite_line,
                        span: [comparison.callsite_line, comparison.callsite_line],
                    }),
                });
            }
        }
        // A run exists for this checkout but produced ZERO in-scope verdicts to compare. This is
        // NOT "the compiler agrees with the graph" — it is "the run found nothing in this
        // checkout's scope," which is exactly the silent-no-op symptom the #82 P0 scope bug
        // produced (the active-checkout predicate matched no file rows). Surface it so a
        // run-but-empty result is distinguishable from a genuine all-agree.
        if summary.verdicts_examined == 0 {
            summary.warnings.push(
                "oracle run exists for this checkout but examined 0 in-scope verdicts — nothing \
                 to compare (this is run-but-empty, not compiler-agrees); re-run `rag-rat oracle \
                 run` if you expected verdicts"
                    .to_string(),
            );
        }
        summary.contradictions = u64::try_from(contradictions.len()).unwrap_or(u64::MAX);
        Ok(crate::query::graph::CompareGraphScipReport {
            query: crate::query::graph::CompareGraphScipQuery {
                // The tools (and their versions) that contributed verdicts, joined — the report now
                // spans every backend with a run, not a single hardcoded tool.
                tool: runs.iter().map(|(tool, _)| tool.as_db_str()).collect::<Vec<_>>().join(","),
                tool_version: Some(
                    runs.iter().map(|(_, version)| version.clone()).collect::<Vec<_>>().join(","),
                ),
                commit_sha: self.active_commit_sha.clone(),
                worktree_id: self.active_worktree_id.clone(),
            },
            summary,
            contradictions,
        })
    }

    fn read_graph_logical_symbol(
        &self,
        logical_symbol_id: Option<i64>,
    ) -> anyhow::Result<(
        Option<crate::query::graph::LogicalSymbol>,
        Vec<crate::query::graph::LogicalSymbolVariant>,
    )> {
        let Some(logical_symbol_id) = logical_symbol_id else {
            return Ok((None, Vec::new()));
        };
        let Some(logical) = crate::query::symbol::lookup_logical_by_id(
            self.storage.connection(),
            logical_symbol_id,
        )?
        else {
            return Ok((None, Vec::new()));
        };
        let variants = crate::query::symbol::logical_members(
            self.storage.connection(),
            logical.logical_symbol_id,
        )?
        .into_iter()
        .map(|member| crate::query::graph::LogicalSymbolVariant {
            symbol_id: member.symbol_id,
            cfg_expr: member.cfg_expr,
            signature_hash: member.signature_hash,
            start_line: member.start_line,
            end_line: member.end_line,
        })
        .collect::<Vec<_>>();
        Ok((
            Some(crate::query::graph::LogicalSymbol {
                logical_symbol_id: logical.logical_symbol_id,
                qualified_name: logical.qualified_name,
                variant_count: logical.variant_count,
                group_reason: logical.group_reason,
            }),
            variants,
        ))
    }

    pub(super) fn graph_options_with_logical_group(
        &self,
        options: &crate::query::graph::GraphTraversalOptions,
    ) -> anyhow::Result<crate::query::graph::GraphTraversalOptions> {
        if options.logical_symbol_id.is_some() {
            return Ok(options.clone());
        }
        let Some(symbol_id) = options.symbol_id else {
            return Ok(options.clone());
        };
        let Some(logical) =
            crate::query::symbol::logical_for_symbol_id(self.storage.connection(), symbol_id)?
        else {
            return Ok(options.clone());
        };
        let mut options = options.clone();
        options.logical_symbol_id = Some(logical.logical_symbol_id);
        Ok(options)
    }

    pub(super) fn find_local_symbol_context_hits(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
    ) -> anyhow::Result<Vec<SearchHit>> {
        // The text-mention fallback runs `chunk_fts MATCH` (#77) — you can't LIKE a compressed blob
        // — so the FTS index must be fresh first (same precondition as search/impact). Omitted when
        // the symbol name has no FTS token (then it's symbol_path matching only).
        self.ensure_fts_fresh()?;
        let conn = self.storage.connection();
        let name_like = format!("%{}%", symbol.name);
        let fts = crate::query::impact::fts_phrase_query(&symbol.name);
        let text_clause = if fts.is_some() {
            "OR chunks.id IN (SELECT c2.id FROM chunks AS c2 JOIN chunk_fts ON chunk_fts.rowid = \
             c2.id WHERE chunk_fts MATCH ?4)"
        } else {
            ""
        };
        let sql = format!(
            "
            SELECT chunks.id, files.path, files.language, files.kind,
                   chunks.start_line, chunks.end_line, chunks.symbol_path,
                   chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version
            FROM chunks
            JOIN files ON files.id = chunks.file_id
            JOIN chunk_text ON chunk_text.chunk_id = chunks.id
            WHERE files.path = ?1
              AND (
                chunks.symbol_path = ?2
                OR chunks.symbol_path LIKE ?3
                {text_clause}
              )
            ORDER BY
              CASE
                WHEN chunks.symbol_path = ?2 THEN 0
                WHEN chunks.symbol_path LIKE ?3 THEN 1
                ELSE 2
              END,
              chunks.start_line
            LIMIT ?5
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        // ?4 is bound unconditionally (unreferenced when `text_clause` is empty); decompress the
        // snippet text in the post-loop (decompress can't cross the rusqlite closure).
        let rows = stmt.query_map(
            params![
                symbol.path,
                symbol.qualified_name,
                name_like,
                fts.unwrap_or_default(),
                i64::from(limit.max(1)),
            ],
            |row| {
                Ok((
                    SearchHit {
                        chunk_id: row.get(0)?,
                        path: row.get(1)?,
                        language: row.get(2)?,
                        kind: row.get(3)?,
                        start_line: row.get(4)?,
                        end_line: row.get(5)?,
                        symbol_path: row.get(6)?,
                        score: 1.0,
                        retrieval_mode: "lexical".to_string(),
                        summary: String::new(),
                        graph: None,
                        score_components: None,
                        importance: None,
                    },
                    rag_rat_db::text_compression::ChunkTextRow {
                        blob: row.get(7)?,
                        raw_len: row.get(8)?,
                        dict_version: row.get(9)?,
                    },
                ))
            },
        )?;
        let collected = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let dicts = crate::query::chunk_text_dicts(conn)?;
        let mut decoder = rag_rat_db::text_compression::ChunkTextDecoder::new(&dicts);
        let mut hits = Vec::with_capacity(collected.len());
        for (mut hit, text_row) in collected {
            hit.summary = bounded_summary(&text_row.resolve(&mut decoder)?);
            hits.push(hit);
        }
        Ok(hits)
    }

    pub fn impact_surface(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        // impact's chunk-mention evidence runs `chunk_fts MATCH` (#77 Phase 1), so the FTS index
        // must be fresh first — same precondition search enforces before its MATCH queries.
        // #582: the papertrail-rationale section also ranks github_fts — heal-and-retry.
        crate::index::retry_once_on_fts_corruption(
            || {
                self.ensure_fts_fresh()?;
                crate::query::impact::impact_surface(self.storage.connection(), query, limit)
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn impact_surface_with_options(
        &self,
        query: &str,
        limit: u32,
        resolution_mode: crate::query::graph::GraphResolutionMode,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::index::retry_once_on_fts_corruption(
            || {
                self.ensure_fts_fresh()?;
                crate::query::impact::impact_surface_with_options(
                    self.storage.connection(),
                    query,
                    limit,
                    resolution_mode,
                )
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn impact_surface_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
        resolution_mode: crate::query::graph::GraphResolutionMode,
    ) -> anyhow::Result<Vec<crate::query::impact::ImpactItem>> {
        crate::index::retry_once_on_fts_corruption(
            || {
                self.ensure_fts_fresh()?;
                crate::query::impact::impact_surface_for_symbol(
                    self.storage.connection(),
                    symbol,
                    limit,
                    resolution_mode,
                )
            },
            || self.heal_corrupt_fts(),
        )
    }

    pub fn impact_surface_report_for_selected_symbol(
        &self,
        symbol: &crate::query::symbol::SymbolHit,
        limit: u32,
        options: &crate::query::impact::ImpactSurfaceOptions,
    ) -> anyhow::Result<crate::query::impact::ImpactSurfaceReport> {
        // Only the text sections (tests / docs / text-fallback) run `chunk_fts MATCH`; the report
        // builder's neighbors come from the graph, not FTS. So skip the FTS refresh when the caller
        // excludes every text section (e.g. MCP `include: ["git"]`) — no point rebuilding FTS for a
        // report that won't read it. (The non-report wrappers always need it: the flat builder runs
        // an ungated textual fallback.)
        if options.include_tests || options.include_docs || options.include_text_fallback {
            self.ensure_fts_fresh()?;
        }
        // Change coupling is a DerivedIndex table (windowed file-pair co-change): lazily self-heal
        // it on read, exactly where `ensure_fts_fresh` heals the text sections — but only
        // when the git evidence lane is requested (the same `include_git` gate the coupling
        // section rides). Kept OFF the index pass's terminal transaction; the read path is
        // its recompute trigger.
        if options.include_git {
            self.ensure_coupling_fresh()?;
        }
        // Surface the `Compiler` tier on impact's direct graph neighbors too (same read-side JOIN
        // as trace_callees/find_callers). The enrichment is now injected INTO the builder so it
        // runs over the OVERFETCHED candidate set before the re-rank + limit truncation (#82
        // finding
        // 4) and before the memory-evidence edge-id collection — so a compiler-upgraded neighbor
        // can't be dropped by the heuristic limit, and downstream counts see the final window.
        // #582: the text sections rank chunk_fts and the papertrail sections rank github_fts —
        // heal-and-retry the report build on shadow corruption.
        let mut report = crate::index::retry_once_on_fts_corruption(
            || {
                crate::query::impact::impact_surface_report_for_symbol(
                    self.storage.connection(),
                    symbol,
                    limit,
                    options,
                    |hops| self.enrich_hops_with_oracle(hops),
                )
            },
            || self.heal_corrupt_fts(),
        )?;
        // Attach the LOCAL structural-load signal (scoped weighted fan-in — the third importance
        // scale, NOT PageRank) to the direct graph neighbors AFTER the oracle re-rank + truncate,
        // so it scores exactly the neighbors the report returns. One gated oracle fetch is
        // reused across all neighbors.
        self.enrich_neighbors_with_load_bearing(
            &mut report.direct_semantic_callers,
            &mut report.direct_semantic_callees,
        )?;
        // #148: flag how many of the result's files are dirty relative to the index, so the impact
        // surface isn't read as current when the working tree has moved under it. Flag-only here
        // (not heal): impact spans an unbounded neighbor set, so an inline heal can't be bounded
        // the way symbol_lookup's matched-file heal is — the agent re-runs symbol_lookup
        // (which heals) for a specific symbol if it needs fresh positions. Covers the
        // selected symbol's file, the direct caller/callee call-site files, and the
        // current-source item sections.
        let mut result_paths = vec![symbol.path.clone()];
        for hop in
            report.direct_semantic_callers.iter().chain(report.direct_semantic_callees.iter())
        {
            if let Some(callsite) = &hop.callsite {
                result_paths.push(callsite.path.clone());
            }
        }
        // A direct callee resolves to a DEFINITION in another file; if THAT file changed, the
        // resolution is stale even though the call-site file didn't — so add each returned callee's
        // target definition file, not just its call site (#151 review).
        for hop in report.direct_semantic_callees.iter() {
            if let Some(name) = hop.to_symbol.as_deref().or(hop.target_qualified_name.as_deref())
                && let Some(path) = self.file_for_qualified_name(name)?
            {
                result_paths.push(path);
            }
        }
        for item in report
            .import_export_dependents
            .iter()
            .chain(report.tests_touching_symbol_path.iter())
            .chain(report.docs_mentioning_symbol_path.iter())
            .chain(report.text_fallback_hits.iter())
            // A co-changed file that's dirty-since-index must count toward `stale_files` too, else
            // the caller trusts a surface whose coupling lane has moved under it (#566 finding 2).
            .chain(report.files_co_changed_with_symbol_path.iter())
        {
            result_paths.push(item.path.clone());
        }
        report.completeness_and_caveats.stale_files =
            u64::try_from(self.stale_source_paths(&result_paths)?.len()).unwrap_or(u64::MAX);
        Ok(report)
    }

    /// Lazy self-heal for the windowed change-coupling table (V056) — the DerivedIndex twin of
    /// `ensure_fts_fresh`: recompute from `git_file_changes` when the `git_coupling_stamp` lags the
    /// git-history freshness key (cursor snapshot) + params version, else a cheap stamp-only no-op.
    /// The stored table is pure git-history, so a files-view change never triggers this.
    /// Write-bearing (same posture as `ensure_fts_fresh` / `store_blame`), so it stays OFF the
    /// index pass's terminal transaction.
    pub(crate) fn ensure_coupling_fresh(&self) -> anyhow::Result<()> {
        crate::index::change_coupling::ensure_coupling_fresh(
            self.storage.connection(),
            rag_rat_base::time::now_ms(),
        )
    }
}
