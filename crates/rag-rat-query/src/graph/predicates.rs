use super::*;

// #224 ALIAS CONTRACT: symbol qualified-names are interned into `name_strings`, so these predicate
// fragments reference the joined pool value `from_qn.value` / `to_qn.value` (NOT a
// `from_symbols.qualified_name` column, which no longer exists). EVERY query that splices a
// fragment from this module MUST also provide the matching `LEFT JOIN name_strings from_qn ON
// from_qn.id = from_symbols.qualified_name_id` (and/or `to_qn`) for the alias it uses — `reverse_*`
// use `to_qn`, `forward_*` use `from_qn`. The interned-id arms (`edges.to_name_id = (SELECT id FROM
// name_strings WHERE value = ?3)`) are an edge-side pool lookup and are independent of these symbol
// joins.

/// The guard EVERY query that admits `uses_operator` edges must carry.
///
/// An UNRESOLVED `uses_operator` row is a BUILT-IN operator token — Swift emits one for every `+`,
/// `==`, `!` in the repo — not a use of an in-repo `operator` declaration. Admitting those rows
/// into a query that falls back to matching by NAME makes every `Int + Int` expression a "caller"
/// of any same-named custom operator, flooding callers/impact with dependencies that do not exist.
/// A resolved row (`to_symbol_id IS NOT NULL`) is by definition bound to a real operator symbol, so
/// requiring resolution is what separates the two.
///
/// Valid wherever the edges table is named or aliased `edges`. `forward_visibility_filter` /
/// `reverse_visibility_filter` below encode the same rule in positive form, inside their OR-chains,
/// and `graph_meta` splices this guard directly.
/// `search_and_read_chunk_attach_bounded_graph_evidence` poisons the index with one unresolved
/// operator edge and asserts EVERY consumer — search graph metadata and all three `impact_surface`
/// resolution modes — leaves it out; impact was the lane that had been missing the guard.
pub(crate) const RESOLVED_OPERATOR_ONLY: &str =
    "(edges.edge_kind != 'uses_operator' OR edges.to_symbol_id IS NOT NULL)";

pub(crate) fn validate_edge_kinds(edge_kinds: &[String]) -> anyhow::Result<()> {
    for edge_kind in edge_kinds {
        if !OPTIONAL_EDGE_KINDS.contains(&edge_kind.as_str()) {
            anyhow::bail!("unknown graph edge kind `{edge_kind}`");
        }
    }
    Ok(())
}
pub(crate) fn traversal_params(
    symbol: &str,
    limit: u32,
    edge_kinds: &[String],
    options: &GraphTraversalOptions,
    unique_short_name: bool,
) -> Vec<String> {
    let qualified = symbol.to_string();
    let short = short_name(symbol).to_string();
    let fuzzy_qualified = format!("%::{qualified}");
    // `?4` gates the by-SHORT-NAME arms, and those live ONLY in the `Fuzzy` predicate and tier —
    // no `Exact` or `Syntactic` arm reads this slot. Deriving it from the seed's SHAPE alone
    // therefore switched the arm off for every path-qualified seed, which is every symbol a tool
    // selects, so `resolution: "fuzzy"` was indistinguishable from `syntactic` in reverse (#1199).
    // Fuzzy is the caller explicitly asking for the loosest match, so the seed's shape must not
    // veto it; a name-only caller stays legible because that arm scores `match_tier` 2 and
    // `resolution_label` reports it as `target_name_fallback`.
    let allow_name_fallback = (options.resolution_mode == GraphResolutionMode::Fuzzy
        || !is_qualified_symbol(symbol))
    .to_string();
    let mut params = vec![
        qualified,
        fuzzy_qualified,
        short,
        allow_name_fallback,
        limit.to_string(),
        options.symbol_id.unwrap_or(-1).to_string(),
        unique_short_name.to_string(),
        options.logical_symbol_id.unwrap_or(-1).to_string(),
    ];
    params.extend(edge_kinds.iter().cloned());
    params
}
pub(crate) fn quoted_placeholders(count: usize) -> String {
    (0..count).map(|index| format!("?{}", index + 9)).collect::<Vec<_>>().join(", ")
}
/// Ceiling on how many oracle-seeded edges [`reverse_oracle_seeded_edge_ids`] splices into one
/// reverse predicate. Matches the ceiling `oracle_overfetch_limit` puts on the candidate window,
/// so a pathological fan-in cannot materialize an unbounded literal list; the residual beyond it
/// is the same accepted loss as an edge upgraded beyond the overfetch window.
///
/// The query carries an `ORDER BY edges.id` so which rows the cap keeps is a property of the DATA,
/// not of the plan SQLite happened to pick — a repeat execution against an unchanged index keeps
/// the same subset. `traversal_summary` computes the set ONCE and feeds it to both its own
/// queries, so its promise not to count a seeded caller as a hidden unresolved candidate holds
/// whatever the cap dropped. `traverse_with_options` executes separately, on a read connection
/// with no transaction spanning the pair, so a reindex landing between the two can leave the
/// returned hops and the summary describing marginally different populations — self-correcting on
/// the next call.
const ORACLE_SEED_EDGE_CAP: usize = 5000;

/// The live `edges.id` values the COMPILER binds to a reverse traversal's seed — the rows the
/// heuristic arms structurally cannot reach.
///
/// A `calls_name` the resolver could not bind carries `to_symbol_id IS NULL` and, as its only
/// attribution, the callee name AS WRITTEN AT THE CALL SITE (`h::add_logical_symbol` for a
/// `h.add_logical_symbol(..)` method call). That is a receiver-qualified name, never the
/// definition's file-path qualified name, so the unresolved arm of [`reverse_predicate`] cannot
/// match it and the caller is invisible — even when `edge_oracle` already holds a compiler verdict
/// naming the seed exactly. Oracle enrichment runs AFTER this SQL and only rewrites rows the query
/// already returned, so SEEDING is the only point at which a verdict can ADD a caller (#1197).
///
/// ATTRIBUTION is the verdict's `resolved_symbol_id` matched against the SAME seed set the
/// heuristic arms use — logical membership when the traversal carries a logical id, else the seed
/// symbol plus its qualified-name TWINS, exactly the expansion [`reverse_predicate`]'s `Exact` arm
/// applies to `to_symbol_id`. A same-qualified-name sibling therefore CAN satisfy the non-logical
/// arm; that arm is no tighter than the twin set, and the claim to check against is only that the
/// arm never attributes on the target name AS WRITTEN — the attribution that lets an unrelated
/// `h::target` call site land here. Every overload the index groups carries a logical symbol, and
/// on that path membership decides alone.
///
/// CURRENCY must admit no row the display path then refuses: the read-side enrichment
/// (`rag_rat_oracle::store`'s surfacing predicates) would decline to promote it, leaving a caller
/// asserted with no visible evidence and — because the oracle tier sorts first — displacing
/// genuine matches under the caller's `LIMIT`. The three gates it applies are mirrored here: the
/// verdict belongs to the LATEST run of its tool in the active checkout; its callsite file still
/// hashes to `file_sha` and joins a live edge by the six-column content key; and its
/// `resolved_symbol_id` is still LIVE IN THIS CHECKOUT. The last gate is why BOTH seed subqueries
/// join the scoped `files` view instead of reading `symbols` / `logical_symbol_members` raw — edit
/// the file that DEFINES the seed and the pre-edit `symbols` row survives, shadowed by the
/// worktree overlay but still carrying the qualified name the twin arm matches, and still the
/// `resolved_symbol_id` the verdict points at. Only `Upgrade`/`Confirm` count — a `Contradict` is a
/// disagreement we do not act on, and a `ResolvedExternal` has no `resolved_symbol_id` to match.
/// `edges.to_symbol_id` is never written back; only which rows the reverse query may seed from
/// changes.
///
/// CROSS-TOOL ARBITRATION IS THE GATE THIS DOES NOT MIRROR. The display path merges the tools in
/// AUTHORITY order and keeps the FIRST writer per edge, so exactly one tool's verdict decides an
/// edge; this query takes the UNION over every tool with a current run. The two agree while a
/// single backend is in play. With two — a canonical batch tool beside a live LSP patch tool
/// covering the same Rust edges — a LOSING tool's `upgrade` can seed an edge whose winning verdict
/// is a `contradict` (the hop returns at oracle tier carrying no compiler evidence) or an upgrade
/// naming a DIFFERENT symbol (the hop returns labelled `compiler` under a seed it does not call).
/// Arbitrating here needs the per-tool authority ranking, which lives in `rag-rat-oracle`; closing
/// the gap means moving this seed behind that crate, not adding a fourth spelling of a table this
/// crate cannot see (#1215).
///
/// SCOPE is reverse `traverse_with_options` / `traversal_summary` — which is also what the
/// SYMBOL-SELECTED `impact_surface` report traverses (`impact_surface_report_for_symbol` →
/// `oracle_ranked_neighbors`), so its `direct_semantic_callers` carry the seeded rows too. The FLAT
/// `Vec<ImpactItem>` lane — a free-text query, or the `allow_ambiguous` fallback — instead runs
/// `impact::neighbors::graph_neighbors`, whose own reverse predicate has no oracle arm and which
/// gets no enrichment pass either, so that lane still answers the pre-seed caller count (#1214).
pub(crate) fn reverse_oracle_seeded_edge_ids(
    conn: &Connection,
    symbol: &str,
    options: &GraphTraversalOptions,
) -> anyhow::Result<Vec<i64>> {
    // `Exact` is the caller asking for heuristic-verified targets only; an unresolved edge has no
    // place in it regardless of what the compiler concluded.
    if options.resolution_mode == GraphResolutionMode::Exact {
        return Ok(Vec::new());
    }
    let (Some(commit_sha), Some(worktree_id)) = (
        rag_rat_db::schema::connection_context_value(conn, "commit_sha"),
        rag_rat_db::schema::connection_context_value(conn, "worktree_id"),
    ) else {
        // No scope context at all (a raw connection) — there is no checkout to key a run on.
        return Ok(Vec::new());
    };
    // EMPTY-EMPTY IS THE BARE OPEN, not a checkout. `IndexDatabase::open` (the MCP database read
    // path, `doctor`, tests) writes BOTH context keys as `''` and serves a `files` view spanning
    // every commit and worktree of the repo. Keying runs on that pair would pair a repo-wide
    // callsite population with `oracle_runs.commit_sha = '' AND worktree_id = ''`, so a run ever
    // recorded under an empty checkout key would seed across every checkout in the DB. Only the
    // PAIR is disqualifying: a non-git index has an empty `commit_sha` beside a real
    // `worktree_id`, and its runs are legitimately keyed on that.
    if commit_sha.is_empty() && worktree_id.is_empty() {
        return Ok(Vec::new());
    }
    let scope = rag_rat_db::schema::periphery_repo_scope(conn, "oracle_runs")?;
    // The seed set: logical membership when the traversal has a logical id (the same attribution
    // rule the logical arms use), else the seed symbol itself plus its qualified-name twins. Both
    // forms resolve their symbols THROUGH the scoped `files` view — see CURRENCY above: the raw
    // `symbols` / `logical_symbol_members` tables still hold the shadowed rows of a re-indexed
    // definition, and a verdict pointing at one of those is a verdict the display path refuses.
    let seed = if options.logical_symbol_id.is_some() {
        "edge_oracle.resolved_symbol_id IN (
            SELECT members.symbol_id
            FROM logical_symbol_members members
            JOIN symbols ON symbols.id = members.symbol_id
            JOIN files ON files.id = symbols.file_id
            WHERE members.logical_symbol_id = ?4
         )"
    } else {
        "edge_oracle.resolved_symbol_id IN (
            SELECT symbols.id
            FROM symbols
            JOIN files ON files.id = symbols.file_id
            WHERE symbols.id = ?4
               OR symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?3)
         )"
    };
    // SECOND SPELLING (invariant): the run-currency and content-key gates below say the same thing
    // as `rag_rat_oracle::store`'s `edge_oracle_scope_join` + `edge_oracle_current_predicate` +
    // `edge_oracle_def_current_predicate`, which is what the read-side enrichment applies. They are
    // separate SQL because this crate does not depend on `rag-rat-oracle` and because the gates
    // there are written against the RAW `files` table with explicit commit/worktree bind slots,
    // while every join here goes through the connection's scoped `files` view. Change one and the
    // other must move with it, or this query seeds callers enrichment then declines to promote.
    let sql = format!(
        "
        SELECT edges.id
        FROM edge_oracle
        JOIN oracle_runs ON oracle_runs.tool = edge_oracle.tool
                        AND oracle_runs.tool_version = edge_oracle.tool_version
                        AND oracle_runs.commit_sha = ?1
                        AND oracle_runs.worktree_id = ?2{runs_repo}
                        AND oracle_runs.id = (
                            SELECT MAX(latest.id) FROM oracle_runs latest
                            WHERE latest.tool = edge_oracle.tool
                              AND latest.commit_sha = ?1
                              AND latest.worktree_id = ?2{latest_repo}
                        )
        JOIN files ON files.path = edge_oracle.source_path
                  AND files.sha256 = edge_oracle.file_sha
        JOIN edges ON edges.source_file_id = files.id
                  AND edges.source_start_byte = edge_oracle.source_start_byte
                  AND edges.source_end_byte = edge_oracle.source_end_byte
                  AND edges.callee_start_byte = edge_oracle.callee_start_byte
                  AND edges.callee_end_byte = edge_oracle.callee_end_byte
                  AND edges.edge_kind = edge_oracle.edge_kind
        WHERE edge_oracle.kind IN ('upgrade', 'confirm')
          AND {seed}{verdict_repo}
        ORDER BY edges.id
        LIMIT {ORACLE_SEED_EDGE_CAP}
        ",
        runs_repo = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "oracle_runs"),
        latest_repo = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "latest"),
        verdict_repo = rag_rat_db::schema::periphery_repo_scope_clause(&scope, "edge_oracle"),
    );
    let seed_id = options.logical_symbol_id.or(options.symbol_id).unwrap_or(-1);
    let mut stmt = conn.prepare(&sql)?;
    let ids = stmt
        .query_map(rusqlite::params![commit_sha, worktree_id, symbol, seed_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

/// [`reverse_oracle_seeded_edge_ids`] as a rowid `IN` list, or `None` when the oracle has nothing
/// to add — the case in every repo without an oracle run, where the traversal SQL is then byte-for-
/// byte what it was before.
///
/// The ids are SPLICED, not bound: they are `i64`s read straight back out of SQLite, and an
/// uncorrelated rowid `IN` list is what lets the planner keep driving the multi-index OR over the
/// other seed arms. Binding them instead would push the edge-kind placeholders (`?9`…) around for
/// every caller.
pub(crate) fn oracle_seed_in_list(edge_ids: &[i64]) -> Option<String> {
    if edge_ids.is_empty() {
        return None;
    }
    let ids = edge_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
    Some(format!("edges.id IN ({ids})"))
}

pub(crate) fn reverse_predicate(
    mode: GraphResolutionMode,
    logical: bool,
    oracle_edge_ids: &[i64],
) -> String {
    let oracle_arm = oracle_seed_in_list(oracle_edge_ids)
        .map(|list| format!("\n                 OR {list}"))
        .unwrap_or_default();
    if logical {
        return match mode {
            GraphResolutionMode::Exact => "edges.to_symbol_id IS NOT NULL
                 AND edges.to_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )"
            .to_string(),
            // LOGICAL ATTRIBUTION RULE (mirrored in `forward_source_predicate`): a RESOLVED
            // endpoint is attributed by logical membership ALONE. The name arm applies only where
            // `to_symbol_id IS NULL` — an edge the resolver could not bind, whose recorded target
            // name is the only attribution there is (and shared with any same-name sibling). Were
            // the name arm to run on resolved rows too it would hand this symbol an edge the
            // resolver bound to a DIFFERENT symbol of the same qualified name, which is the
            // overload collapse the logical seed exists to end (#1028). Dropping the arm entirely
            // is the opposite error: this SQL runs BEFORE the read-side oracle enrichment, which
            // rewrites hops in memory and never touches the `edges` row — so an unresolved edge
            // refused here is one no compiler verdict can put back, in any later run. That is why
            // the oracle arm seeds by VERDICT here rather than leaving recovery to enrichment; it
            // attributes on `resolved_symbol_id`, so it admits none of the sibling collapse the
            // name arm is gated against.
            GraphResolutionMode::Syntactic => format!(
                "(edges.to_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                  )
                  OR (edges.to_symbol_id IS NULL
                      AND edges.target_qualified_name_id =
                            (SELECT id FROM name_strings WHERE value = ?1)){oracle_arm})"
            ),
            GraphResolutionMode::Fuzzy => format!(
                "edges.to_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )
                 OR to_symbols.name = ?3
                 OR to_qn.value = ?1
                 OR to_qn.value LIKE ?2
                 OR edges.target_qualified_name = ?1
                 OR edges.target_qualified_name LIKE ?2
                 OR (?4 = 'true' AND edges.to_name_id =
                        (SELECT id FROM name_strings WHERE value = ?3)){oracle_arm}"
            ),
        };
    }
    // #682 INDEXED-SEED CONTRACT: the Exact/Syntactic seed branches compare the `edges` view's raw
    // dictionary/id columns (`to_symbol_id`, `target_qualified_name_id`) against a constant — a
    // bare id or a `(SELECT id FROM name_strings WHERE value = ?)` — so the planner drives a
    // MULTI-INDEX OR over `idx_edges_to_symbol` + `idx_edges_target_qname` instead of
    // full-scanning `edges_data` through the view's value joins (`to_qn.value`,
    // `edges.target_qualified_name`). Faithful transform of the value-column forms:
    // `to_qn.value = ?1` (the to-symbol's qualified name) is exactly the edges whose
    // `to_symbol_id` is a symbol with that interned qualified name, and `to_symbols.name = ?3`
    // those whose to-symbol has that short name. Fuzzy keeps the `LIKE` forms (opt-in,
    // inherently non-sargable). See the raw-id note in `ensure_edges_view`.
    match mode {
        GraphResolutionMode::Exact => "edges.to_symbol_id IS NOT NULL
             AND (edges.to_symbol_id = ?6
                  OR edges.to_symbol_id IN (
                     SELECT id FROM symbols
                     WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)))"
            .to_string(),
        GraphResolutionMode::Syntactic => format!(
            "(edges.to_symbol_id = ?6
              OR edges.to_symbol_id IN (
                 SELECT id FROM symbols
                 WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1))
              OR (?7 = 'true' AND edges.to_symbol_id IN (
                 SELECT id FROM symbols WHERE name = ?3))
              OR edges.target_qualified_name_id = (SELECT id FROM name_strings WHERE value = \
             ?1){oracle_arm})"
        ),
        GraphResolutionMode::Fuzzy => format!(
            "to_symbols.name = ?3
             OR to_qn.value = ?1
             OR to_qn.value LIKE ?2
             OR edges.target_qualified_name = ?1
             OR edges.target_qualified_name LIKE ?2
             OR (?4 = 'true' AND edges.to_name_id =
                    (SELECT id FROM name_strings WHERE value = ?3)){oracle_arm}"
        ),
    }
}
/// `match_tier`, the traversal's PRIMARY sort key — and therefore what survives the caller's
/// `LIMIT`.
///
/// The oracle clause sits LAST, after the heuristic arms: a seeded edge that also matches one of
/// them keeps the label that arm earned it (`resolution_label` reads this tier), while one no
/// heuristic arm can explain is scored 0 rather than falling to the `ELSE` bucket. Leaving it in
/// `ELSE` would rank compiler-attributed callers BELOW every name guess and let the overfetch
/// window truncate exactly the rows the seeding was added to recover.
pub(crate) fn reverse_tier(mode: GraphResolutionMode, oracle_edge_ids: &[i64]) -> String {
    let oracle_tier = oracle_seed_in_list(oracle_edge_ids)
        .map(|list| format!("\n                WHEN {list} THEN 0"))
        .unwrap_or_default();
    match mode {
        GraphResolutionMode::Exact => "0".to_string(),
        GraphResolutionMode::Syntactic => format!(
            "CASE
                WHEN edges.to_symbol_id IS NOT NULL THEN 0
                WHEN edges.target_qualified_name = ?1 THEN 1{oracle_tier}
                ELSE 4
             END"
        ),
        GraphResolutionMode::Fuzzy => format!(
            "CASE
                WHEN edges.to_symbol_id IS NOT NULL THEN 0
                WHEN edges.target_qualified_name = ?1 OR edges.target_qualified_name LIKE ?2 THEN 1
                WHEN ?4 = 'true' AND edges.to_name_id =
                    (SELECT id FROM name_strings WHERE value = ?3) THEN 2{oracle_tier}
                ELSE 4
             END"
        ),
    }
}
pub(crate) fn forward_source_predicate(mode: GraphResolutionMode, logical: bool) -> &'static str {
    if logical {
        return match mode {
            GraphResolutionMode::Exact =>
                "edges.from_symbol_id IS NOT NULL
                 AND edges.from_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )",
            // Mirror of the logical attribution rule on `reverse_predicate`: membership decides a
            // resolved source, the name arm covers only a source the resolver left unbound — a
            // file-level edge, or a body whose enclosing symbol was not indexed. `from_name` holds
            // the ENCLOSING SYMBOL'S QUALIFIED NAME, which two overloads share, so an ungated arm
            // would put a sibling overload's outgoing edges on this symbol's callee list.
            GraphResolutionMode::Syntactic =>
                "edges.from_symbol_id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )
                 OR (edges.from_symbol_id IS NULL
                     AND edges.from_name_id = (SELECT id FROM name_strings WHERE value = ?1))",
            GraphResolutionMode::Fuzzy =>
                "from_symbols.id IN (
                    SELECT symbol_id
                    FROM logical_symbol_members
                    WHERE logical_symbol_id = ?8
                 )
                 OR from_symbols.name = ?3
                 OR from_qn.value = ?1
                 OR from_qn.value LIKE ?2
                 OR edges.from_name = ?1
                 OR edges.from_name LIKE ?2",
        };
    }
    // #682 INDEXED-SEED CONTRACT (mirror of reverse_predicate): Exact/Syntactic seed on the raw
    // `edges` id columns (`from_symbol_id`, `from_name_id`) against a constant so the planner
    // drives `idx_edges_from_symbol` + `idx_edges_from_name` instead of scanning `edges_data`
    // through the view's `from_qn.value` / `edges.from_name` value joins. Fuzzy keeps the
    // `LIKE` forms.
    match mode {
        GraphResolutionMode::Exact =>
            "edges.from_symbol_id IS NOT NULL
             AND (edges.from_symbol_id = ?6
                  OR edges.from_symbol_id IN (
                     SELECT id FROM symbols
                     WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)))",
        GraphResolutionMode::Syntactic =>
            "edges.from_symbol_id = ?6
             OR edges.from_symbol_id IN (
                SELECT id FROM symbols
                WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1))
             OR (?7 = 'true' AND edges.from_symbol_id IN (
                SELECT id FROM symbols WHERE name = ?3))
             OR edges.from_name_id = (SELECT id FROM name_strings WHERE value = ?1)",
        GraphResolutionMode::Fuzzy =>
            "from_symbols.name = ?3
             OR from_qn.value = ?1
             OR from_qn.value LIKE ?2
             OR edges.from_name = ?1
             OR edges.from_name LIKE ?2",
    }
}
pub(crate) fn forward_target_filter(
    mode: GraphResolutionMode,
    options: &GraphTraversalOptions,
) -> &'static str {
    match mode {
        GraphResolutionMode::Exact => "edges.to_symbol_id IS NOT NULL",
        GraphResolutionMode::Syntactic =>
            if options.include_unresolved {
                "1 = 1"
            } else if options.include_macros {
                "
                edges.to_symbol_id IS NOT NULL
                OR edges.target_qualified_name IS NOT NULL
                OR edges.edge_kind = 'uses_macro'
                "
            } else {
                "edges.to_symbol_id IS NOT NULL OR edges.target_qualified_name IS NOT NULL"
            },
        GraphResolutionMode::Fuzzy => "1 = 1",
    }
}
pub(crate) fn forward_visibility_filter(options: &GraphTraversalOptions) -> &'static str {
    match (options.include_unresolved, options.include_macros, options.include_common_methods) {
        (true, true, true) => "1 = 1",
        (true, true, false) =>
            "
            (
                edges.edge_kind != 'calls_name'
                OR edges.to_name NOT IN (
                    'clone', 'map', 'map_err', 'and_then', 'unwrap_or', 'unwrap_or_else',
                    'to_string', 'to_owned', 'as_ref', 'as_mut', 'get', 'insert',
                    'new', 'default', 'into', 'from', 'iter', 'collect', 'unwrap',
                    'expect', 'ok', 'err'
                )
                OR edges.to_symbol_id IS NOT NULL
            )
            ",
        (true, false, true) => "edges.edge_kind != 'uses_macro'",
        (true, false, false) =>
            "
            edges.edge_kind != 'uses_macro'
            AND (
                edges.edge_kind != 'calls_name'
                OR edges.to_name NOT IN (
                    'clone', 'map', 'map_err', 'and_then', 'unwrap_or', 'unwrap_or_else',
                    'to_string', 'to_owned', 'as_ref', 'as_mut', 'get', 'insert',
                    'new', 'default', 'into', 'from', 'iter', 'collect', 'unwrap',
                    'expect', 'ok', 'err'
                )
                OR edges.to_symbol_id IS NOT NULL
            )
            ",
        (false, true, true) =>
            "
            (
                edges.edge_kind = 'calls_name'
                AND (
                    edges.to_symbol_id IS NOT NULL
                    OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT NULL)
                )
            )
            OR (
                edges.edge_kind = 'constructs'
                AND edges.to_symbol_id IS NOT NULL
            )
            OR (
                edges.edge_kind = 'uses_operator'
                AND edges.to_symbol_id IS NOT NULL
            )
            OR edges.edge_kind = 'uses_macro'
            OR edges.edge_kind NOT IN ('calls_name', 'constructs', 'uses_operator')
            ",
        (false, true, false) =>
            "
            (
                edges.edge_kind = 'calls_name'
                AND (
                    edges.to_symbol_id IS NOT NULL
                    OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT NULL)
                )
                AND (
                    edges.to_name NOT IN (
                        'clone', 'map', 'map_err', 'and_then', 'unwrap_or', 'unwrap_or_else',
                        'to_string', 'to_owned', 'as_ref', 'as_mut', 'get', 'insert',
                        'new', 'default', 'into', 'from', 'iter', 'collect', 'unwrap',
                        'expect', 'ok', 'err'
                    )
                    OR edges.to_symbol_id IS NOT NULL
                )
            )
            OR (
                edges.edge_kind = 'constructs'
                AND edges.to_symbol_id IS NOT NULL
            )
            OR (
                edges.edge_kind = 'uses_operator'
                AND edges.to_symbol_id IS NOT NULL
            )
            OR edges.edge_kind = 'uses_macro'
            OR edges.edge_kind NOT IN ('calls_name', 'constructs', 'uses_operator')
            ",
        (false, false, true) =>
            "
            edges.edge_kind != 'uses_macro'
            AND (
                (
                    edges.edge_kind = 'calls_name'
                    AND (
                        edges.to_symbol_id IS NOT NULL
                        OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT \
             NULL)
                    )
                )
                OR (
                    edges.edge_kind = 'constructs'
                    AND edges.to_symbol_id IS NOT NULL
                )
                OR (
                    edges.edge_kind = 'uses_operator'
                    AND edges.to_symbol_id IS NOT NULL
                )
                OR edges.edge_kind NOT IN ('calls_name', 'constructs', 'uses_operator')
            )
            ",
        (false, false, false) =>
            "
            edges.edge_kind != 'uses_macro'
            AND (
                (
                    edges.edge_kind = 'calls_name'
                    AND (
                        edges.to_symbol_id IS NOT NULL
                        OR (edges.confidence = 'Syntactic' AND edges.target_qualified_name IS NOT \
             NULL)
                    )
                    AND (
                        edges.to_name NOT IN (
                            'clone', 'map', 'map_err', 'and_then', 'unwrap_or', 'unwrap_or_else',
                            'to_string', 'to_owned', 'as_ref', 'as_mut', 'get', 'insert',
                            'new', 'default', 'into', 'from', 'iter', 'collect', 'unwrap',
                            'expect', 'ok', 'err'
                        )
                        OR edges.to_symbol_id IS NOT NULL
                    )
                )
                OR (
                    edges.edge_kind = 'constructs'
                    AND edges.to_symbol_id IS NOT NULL
                )
                OR (
                    edges.edge_kind = 'uses_operator'
                    AND edges.to_symbol_id IS NOT NULL
                )
                OR edges.edge_kind NOT IN ('calls_name', 'constructs', 'uses_operator')
            )
            ",
    }
}
pub fn unique_symbol_name(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    // GENERATION-SCOPED via the `files` view (batch 6, count-scoping class): an unscoped
    // `COUNT(*) FROM symbols` counts the SAME live name once per generation during a
    // dead-generation window (post-flip pre-gc, or crashed pre-flip staging) → count == 2 →
    // "not unique" → the caller's `unique_short_name` gate DISABLES the short-name fallback
    // branch and suppresses a genuine LIVE hop from the returned rows. Joining the scoped view
    // counts only the live generation's symbols, matching the row queries in `traverse` /
    // `traversal_summary`.
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS symbol_count FROM symbols
         JOIN files ON files.id = symbols.file_id
         WHERE symbols.name = ?1",
        [name],
        |row| row.get("symbol_count"),
    )?;
    Ok(count == 1)
}
/// Whether an unresolved call site's callee short name, as WRITTEN, can only have meant this seed.
///
/// The seed's own definitions are not rivals for its name. A `#[cfg]`-split pair, or any overload
/// group, puts several `symbols` rows under one short name while naming ONE callable — so
/// [`unique_symbol_name`], which counts every scoped row of that name, reads such a seed as
/// ambiguous and a caller that gates on it gives up on exactly the symbols whose call sites the
/// resolver is likeliest to have left unbound. The honest question is whether any symbol OUTSIDE
/// the seed carries the name: logical membership when the traversal has a logical id, else the seed
/// symbol plus its qualified-name twins — the same seed set [`reverse_predicate`] expands.
///
/// A shared name (`get`, `new`) still answers false, since the rival definitions sit outside the
/// seed by construction.
///
/// GENERATION-SCOPED via the `files` view, for the reason spelled out on [`unique_symbol_name`].
pub(crate) fn short_name_identifies_seed_alone(
    conn: &Connection,
    symbol: &str,
    options: &GraphTraversalOptions,
) -> anyhow::Result<bool> {
    let outside_seed = if options.logical_symbol_id.is_some() {
        "symbols.id NOT IN (
            SELECT symbol_id FROM logical_symbol_members WHERE logical_symbol_id = ?2
         )"
    } else {
        // `IS NOT` is SQLite's null-safe inequality: a symbol with no interned qualified name, or a
        // seed name absent from the pool, must still count as outside the seed rather than vanish.
        "symbols.id != ?2
         AND symbols.qualified_name_id IS NOT (SELECT id FROM name_strings WHERE value = ?3)"
    };
    // Bound per branch: the logical arm never mentions `?3`, and SQLite counts a statement's
    // parameters by the highest index it references, so passing the seed name there is rejected.
    let seed_id = options.logical_symbol_id.or(options.symbol_id).unwrap_or(-1);
    let mut params = vec![
        rusqlite::types::Value::from(short_name(symbol).to_string()),
        rusqlite::types::Value::from(seed_id),
    ];
    if options.logical_symbol_id.is_none() {
        params.push(rusqlite::types::Value::from(symbol.to_string()));
    }
    let outside_seed_count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) AS outside_seed_count FROM symbols
             JOIN files ON files.id = symbols.file_id
             WHERE symbols.name = ?1 AND ({outside_seed})"
        ),
        params_from_iter(params),
        |row| row.get("outside_seed_count"),
    )?;
    Ok(outside_seed_count == 0)
}
/// How many active-scope symbols a NAME seed expands to under the `Syntactic` predicates above.
///
/// Lives beside those predicates because it answers the same question they do and must not drift
/// from them: a symbol is a seed match when its qualified name IS `symbol`, or — only while the
/// seed's short name is unique, which is the `?7` gate the predicates themselves carry — when its
/// name is that short name. A caller that counts the qualified arm alone reports zero for the
/// unqualified seed a traversal resolves perfectly well through the short-name arm.
///
/// GENERATION-SCOPED via the `files` view, for the reason spelled out on `unique_symbol_name`.
pub fn syntactic_seed_symbol_count(conn: &Connection, symbol: &str) -> anyhow::Result<u64> {
    let short = short_name(symbol);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS symbol_count FROM symbols
         JOIN files ON files.id = symbols.file_id
         LEFT JOIN name_strings qn ON qn.id = symbols.qualified_name_id
         WHERE qn.value = ?1 OR (?2 = 1 AND symbols.name = ?3)",
        rusqlite::params![symbol, unique_symbol_name(conn, short)?, short],
        |row| row.get("symbol_count"),
    )?;
    Ok(u64::try_from(count).unwrap_or(0))
}
pub(crate) fn resolution_label(
    mode: GraphResolutionMode,
    stored: String,
    tier: i64,
    verified_target_symbol: bool,
) -> String {
    if mode == GraphResolutionMode::Exact && verified_target_symbol {
        return "exact".to_string();
    }
    if stored != "unresolved" {
        return stored;
    }
    match tier {
        1 => "target_qualified_suffix".to_string(),
        2 => "target_name_fallback".to_string(),
        _ => stored,
    }
}
pub(crate) fn short_name(symbol: &str) -> &str {
    symbol.rsplit([':', '.', '#', '/']).find(|part| !part.is_empty()).unwrap_or(symbol)
}
pub(crate) fn is_qualified_symbol(symbol: &str) -> bool {
    symbol.contains("::")
        || symbol.contains(".rs:")
        || symbol.contains(".ts:")
        || symbol.contains(".tsx:")
        || symbol.contains(".kt:")
        || symbol.contains('/')
}
