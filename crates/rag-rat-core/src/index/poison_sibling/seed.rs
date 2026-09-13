//! Seeding (and clearing) the poison sibling's tripwire rows.

use rusqlite::{Connection, params};

use super::*;

/// Clear then insert the full tripwire row set for the poison sibling. Runs under `foreign_keys =
/// ON` (the live rebuild connection), so inserts are parent→child and clears child→parent.
/// Deliberately touches NO registry table (`repos`/`repo_roots`/`repo_meta`) — see the module docs.
pub(crate) fn seed_sibling(conn: &Connection) -> anyhow::Result<()> {
    clear_sibling(conn)?;

    // Resolve the primary-repo path the SAME-PATH tripwires collide onto BEFORE seeding any sibling
    // rows (so the poison rows never win the `ORDER BY path` pick).
    let collision_path = primary_collision_path(conn)?;

    // --- git history (git_file_changes FKs git_commits(repo_id, hash); git_commits has NO FK to
    // repos, so this needs no registry row) ---
    conn.execute(
        "INSERT INTO git_commits(hash, author_name, author_email, authored_at_s, committed_at_s, \
         subject, body, changed_file_count, repo_id)
         VALUES (?1, ?2, ?2, 0, 0, ?3, '', 1, ?4)",
        params![
            POISON_COMMIT,
            format!("{POISON_PREFIX}author"),
            format!("{POISON_PREFIX}subject"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
         repo_id)
         VALUES (?1, ?2, 0, 0, 'modified', ?3)",
        params![POISON_COMMIT, format!("{POISON_PREFIX}change.rs"), POISON_REPO_ID],
    )?;

    // --- direct-scoped core tables ---
    // A6: seed the sibling's files at generation 0 — the sibling's OWN live generation (it has no
    // `repo_meta[live_files_generation]`, so its live generation reads 0). This is DISTINCT from
    // the primary repo's post-rebuild live generation (>= 1, since the seed runs at the rebuild
    // tail after the flip), so a repo-UNSCOPED dead-generation sweep — `WHERE generation !=
    // <primary live>` missing the `repo_id` predicate — would delete these rows and trip
    // `assert_sibling_intact`. That is the exact class the generation gc sweep must never regress
    // (and the reason the sibling carries no `repo_meta` live-generation pointer of its own).
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation)
         VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, '', ?4, 0)",
        params![
            format!("{POISON_PREFIX}file.rs"),
            format!("{POISON_PREFIX}sha"),
            POISON_COMMIT,
            POISON_REPO_ID
        ],
    )?;
    let file_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO packages(manifest_dir, commit_sha, worktree_id, local_roots_json, repo_id)
         VALUES (?1, '', '', '[]', ?2)",
        params![format!("{POISON_PREFIX}pkg"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO parser_failures(repo_id, path, language, message) VALUES (?1, ?2, 'rust', ?3)",
        params![POISON_REPO_ID, format!("{POISON_PREFIX}fail.rs"), format!("{POISON_PREFIX}msg")],
    )?;
    conn.execute(
        "INSERT INTO logical_symbols(id, language, path, logical_name, qualified_name_id, kind, \
         variant_count, group_reason, repo_id)
         VALUES (?1, 'rust', ?2, ?3, NULL, 'function', 1, ?4, ?5)",
        params![
            POISON_LOGICAL_ID,
            format!("{POISON_PREFIX}file.rs"),
            format!("{POISON_PREFIX}symbol"),
            format!("{POISON_PREFIX}group"),
            POISON_REPO_ID
        ],
    )?;

    // --- children hung off the poison file (transitively scoped through files.repo_id) ---
    conn.execute(
        "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
         end_byte, start_line, end_line, is_test)
         VALUES (?1, 'rust', ?2, NULL, 'function', 0, 0, 0, 0, 0)",
        params![file_id, format!("{POISON_PREFIX}symbol")],
    )?;
    let symbol_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO chunks(file_id, chunk_kind, start_byte, end_byte, start_line, end_line, \
         text_hash, source_revision, anchor_version, normalized_hash, start_boundary_hash, \
         end_boundary_hash, start_context_hash, end_context_hash, context_radius, \
         embedding_policy, embedding_priority)
         VALUES (?1, 'symbol', 0, 0, 0, 0, ?2, '', 0, ?2, '', '', '', '', 0, 'none', 0)",
        params![file_id, format!("{POISON_PREFIX}chunkhash")],
    )?;
    let chunk_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO docs(chunk_id, source_kind, heading_path, repo_id) VALUES (?1, 'markdown', \
         ?2, ?3)",
        params![chunk_id, format!("{POISON_PREFIX}heading"), POISON_REPO_ID],
    )?;

    // --- one edge whose source file is the poison file (scoped via source_file_id → files) ---
    conn.execute_batch(&format!(
        "INSERT OR IGNORE INTO name_strings(value) VALUES
            ('{POISON_PREFIX}from'), ('{POISON_PREFIX}to'), ('{POISON_PREFIX}calls'),
            ('{POISON_PREFIX}conf'), ('{POISON_PREFIX}res');"
    ))?;
    let name_id = |value: &str| -> rusqlite::Result<i64> {
        conn.query_row("SELECT id FROM name_strings WHERE value = ?1", [value], |row| row.get(0))
    };
    conn.execute(
        "INSERT INTO edges_data(source_file_id, from_name_id, to_name_id, edge_kind_id, \
         confidence_id, resolution_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            file_id,
            name_id(&format!("{POISON_PREFIX}from"))?,
            name_id(&format!("{POISON_PREFIX}to"))?,
            name_id(&format!("{POISON_PREFIX}calls"))?,
            name_id(&format!("{POISON_PREFIX}conf"))?,
            name_id(&format!("{POISON_PREFIX}res"))?,
        ],
    )?;

    // --- children hung off the poison logical symbol (scoped via logical_symbols.repo_id) ---
    conn.execute(
        "INSERT INTO logical_symbol_members(logical_symbol_id, symbol_id, start_line, end_line)
         VALUES (?1, ?2, 0, 0)",
        params![POISON_LOGICAL_ID, symbol_id],
    )?;
    // logical_symbol_monikers has NO repo_id and NO FK; it is scoped only by the join to
    // logical_symbols. This row is the tripwire for the oracle moniker clear/count (round-6 P2 #3).
    // Since V042 `logical_symbol_monikers` carries its OWN `repo_id` (the count/clear/write now
    // filter it directly rather than joining `logical_symbols`), stamp it the sibling id.
    conn.execute(
        "INSERT INTO logical_symbol_monikers(logical_symbol_id, tool, tool_version, moniker, \
         computed_at, repo_id)
         VALUES (?1, 'scip-rust', ?2, ?3, 0, ?4)",
        params![
            POISON_LOGICAL_ID,
            format!("{POISON_PREFIX}ver"),
            format!("{POISON_PREFIX}moniker"),
            POISON_REPO_ID
        ],
    )?;

    // --- papertrail (V060): every provider-neutral table + the standalone `papertrail_fts`
    // mirror carries a `repo_id` column, so a sibling row is valid (these caches carry NO FK to
    // `repos`) and any unscoped papertrail read/count/delete trips a tripwire. The two items
    // share `POISON_ITEM_KEY` under DIFFERENT `item_kind`s, so a read that drops the kind from
    // the natural key also trips. ---
    conn.execute(
        "INSERT INTO papertrail_refs(tracker, project, item_key, ref_kind, source_kind, \
         source_path, source_text, discovered_at_ms, repo_id)
         VALUES ('github', ?1, ?2, 'closing', 'file', ?3, ?4, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}path.rs"),
            format!("{POISON_PREFIX}reftext"),
            POISON_REPO_ID
        ],
    )?;
    // V073 (#702): a sibling closing edge for the SAME external pair — an unscoped closing-edge
    // read would adopt the sibling's attested closer.
    conn.execute(
        "INSERT OR IGNORE INTO papertrail_closing_edges(tracker, project, issue_kind, issue_key, \
         closer_kind, closer_key, source, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'issue', ?2, 'commit', ?3, 'provider', 0, ?4)",
        params![POISON_PROJECT, POISON_ITEM_KEY, format!("{POISON_PREFIX}sha"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'issue', ?2, 'http://x', 'open', ?3, ?4, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}title"),
            format!("{POISON_PREFIX}body"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, title, \
         body, merged_at, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'change_request', ?2, 'http://x', 'open', ?3, ?4, NULL, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}prtitle"),
            format!("{POISON_PREFIX}prbody"),
            POISON_REPO_ID
        ],
    )?;
    // The three legacy comment shapes in the unified table: a plain thread comment, a review
    // event (review_state), and a file-anchored review comment (anchor_path).
    conn.execute(
        "INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, comment_id, url, \
         body, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'issue', ?2, ?3, 'http://x', ?4, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}comment_id_1"),
            format!("{POISON_PREFIX}comment"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, comment_id, url, \
         body, review_state, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'change_request', ?2, ?3, NULL, ?4, 'commented', 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}comment_id_2"),
            format!("{POISON_PREFIX}review"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO papertrail_comments(tracker, project, item_kind, item_key, comment_id, url, \
         body, anchor_path, synced_at_ms, repo_id)
         VALUES ('github', ?1, 'change_request', ?2, ?3, 'http://x', ?4, ?5, 0, ?6)",
        params![
            POISON_PROJECT,
            POISON_ITEM_KEY,
            format!("{POISON_PREFIX}comment_id_3"),
            format!("{POISON_PREFIX}revcomment"),
            format!("{POISON_PREFIX}anchored.rs"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO papertrail_sync_cursor(tracker, project, high_mark_at, repo_id)
         VALUES ('github', ?1, ?2, ?3)",
        params![POISON_PROJECT, format!("{POISON_PREFIX}mark"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO papertrail_item_tags(tracker, project, item_kind, item_key, tag, repo_id)
         VALUES ('github', ?1, 'issue', ?2, ?3, ?4)",
        params![POISON_PROJECT, POISON_ITEM_KEY, format!("{POISON_PREFIX}itemtag"), POISON_REPO_ID],
    )?;
    // papertrail_fts mirror rows, seeded EXACTLY as the INCREMENTAL writers (`store_item` /
    // `store_comment`) and the whole-table `papertrail::rebuild_fts` derive them from the five
    // base rows above: item rows carry the item title, comment rows carry COALESCE(anchor_path,
    // '') in the title slot and COALESCE(url, '') in the url slot. A full mirror rebuild DELETEs
    // everything and re-derives, so seeding anything else would strand the intact check on a
    // vanished row set; `papertrail_fts_tripwires_survive_a_mirror_rebuild` pins this
    // equivalence. `classification` is recomputed by `insert_fts` (`classify_text`) at
    // re-derivation and is deliberately NOT pinned by the tripwires.
    let insert_poison_fts = |item_kind: &str,
                             doc_kind: &str,
                             comment_id: &str,
                             url: &str,
                             title: &str,
                             body: String| {
        conn.execute(
            "INSERT INTO papertrail_fts(tracker, project, item_kind, item_key, doc_kind, \
             comment_id, url, title, body, classification, repo_id)
                 VALUES ('github', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'other', ?9)",
            params![
                POISON_PROJECT,
                item_kind,
                POISON_ITEM_KEY,
                doc_kind,
                comment_id,
                url,
                title,
                body,
                POISON_REPO_ID
            ],
        )
    };
    insert_poison_fts(
        "issue",
        "item",
        "",
        "http://x",
        &format!("{POISON_PREFIX}title"),
        format!("{POISON_PREFIX}body"),
    )?;
    insert_poison_fts(
        "change_request",
        "item",
        "",
        "http://x",
        &format!("{POISON_PREFIX}prtitle"),
        format!("{POISON_PREFIX}prbody"),
    )?;
    insert_poison_fts(
        "issue",
        "comment",
        &format!("{POISON_PREFIX}comment_id_1"),
        "http://x",
        "",
        format!("{POISON_PREFIX}comment"),
    )?;
    insert_poison_fts(
        "change_request",
        "comment",
        &format!("{POISON_PREFIX}comment_id_2"),
        "",
        "",
        format!("{POISON_PREFIX}review"),
    )?;
    insert_poison_fts(
        "change_request",
        "comment",
        &format!("{POISON_PREFIX}comment_id_3"),
        "http://x",
        &format!("{POISON_PREFIX}anchored.rs"),
        format!("{POISON_PREFIX}revcomment"),
    )?;

    // --- A5 periphery (V042): repo memories (+ bindings / tags / FTS mirror), oracle runs, edge
    // oracle, clone generations / token-df / refinements, dream findings, and reconcile attempts
    // each gained a `repo_id` column in V042, so a sibling row is now valid and any unscoped
    // periphery read/count/delete trips a tripwire. `repo_memory_tags` has NO `repo_id` of its own
    // (it scopes transitively via `memory_id` → `repo_memories.repo_id`), so it hangs off the
    // poison memory. ---
    conn.execute(
        "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
         updated_at_ms, source, memory_version, repo_id)
         VALUES (?1, 'Invariant', ?2, ?3, 'high', 'active', 0, 0, 'agent', 'v1', ?4)",
        params![
            POISON_MEMORY_ID,
            format!("{POISON_PREFIX}title"),
            format!("{POISON_PREFIX}body"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, anchor_status, \
         created_at_ms, repo_id)
         VALUES (?1, 'path', ?2, 'current', 0, ?3)",
        params![POISON_MEMORY_ID, format!("{POISON_PREFIX}bind"), POISON_REPO_ID],
    )?;
    conn.execute("INSERT INTO repo_memory_tags(memory_id, tag) VALUES (?1, ?2)", params![
        POISON_MEMORY_ID,
        format!("{POISON_PREFIX}tag")
    ])?;
    conn.execute(
        "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
         VALUES (?1, ?2, ?3, ?4, 'Invariant', ?5)",
        params![
            POISON_REPO_ID,
            POISON_MEMORY_ID,
            format!("{POISON_PREFIX}title"),
            format!("{POISON_PREFIX}body"),
            format!("{POISON_PREFIX}tag")
        ],
    )?;
    // A sibling typed edge (#464, V049): owned by the poison repo, authored on the poison memory.
    // Any UNSCOPED `edges_from` / `edges_into` / count would surface it and trip a tripwire.
    conn.execute(
        "INSERT INTO repo_node_edges(edge_key, repo_id, source_node_id, relation, target_repo_id, \
         target_kind, target_anchor, target_node_id, anchor_status, created_at_ms)
         VALUES (?1, ?2, ?3, 'depends_on', ?2, 'node', ?4, NULL, 'unresolved', 0)",
        params![
            format!("{POISON_PREFIX}edge_key"),
            POISON_REPO_ID,
            POISON_MEMORY_ID,
            format!("{POISON_PREFIX}target"),
        ],
    )?;
    conn.execute(
        "INSERT INTO oracle_runs(tool, tool_version, commit_sha, worktree_id, started_at, status, \
         stats_json, repo_id)
         VALUES ('scip-rust', ?1, ?2, '', 0, 'complete', '{}', ?3)",
        params![format!("{POISON_PREFIX}ver"), POISON_COMMIT, POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO edge_oracle(repo_id, source_path, source_start_byte, source_end_byte, \
         callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
         scip_symbol, kind, computed_at)
         VALUES (?1, ?2, 0, 0, 0, 0, 'calls_name', ?3, 'scip-rust', ?4, ?5, 'resolved', 0)",
        params![
            POISON_REPO_ID,
            format!("{POISON_PREFIX}edge.rs"),
            format!("{POISON_PREFIX}sha"),
            format!("{POISON_PREFIX}ver"),
            format!("{POISON_PREFIX}scip")
        ],
    )?;
    conn.execute(
        "INSERT INTO clone_graph_generations(generation, status, theta_floor, normalizer_kind, \
         normalizer_version, source_revision, started_at_ms, repo_id)
         VALUES (?1, 'Complete', 0.7, 'baseline', 1, ?2, 0, ?3)",
        params![POISON_GENERATION, format!("{POISON_PREFIX}rev"), POISON_REPO_ID],
    )?;
    conn.execute(
        "INSERT INTO clone_token_df(repo_id, normalizer_kind, token_hash, df)
         VALUES (?1, 'baseline', ?2, 1)",
        params![POISON_REPO_ID, POISON_GENERATION],
    )?;
    conn.execute(
        "INSERT INTO clone_refinements(repo_id, class_key, language, refine_mode, template, \
         variation_points_json, proposed_signature_json, confidence, anti_unify_coverage, \
         lcs_ratio, refactorability, norm_version, alignment_version, created_at_ms, lcs_sampled)
         VALUES (?1, ?2, 'rust', 'exact', ?3, '[]', '{}', 'high', 0.0, 0.0, 0.0, 1, 1, 0, 0)",
        params![POISON_REPO_ID, format!("{POISON_PREFIX}class"), format!("{POISON_PREFIX}tmpl")],
    )?;
    conn.execute(
        "INSERT INTO dream_findings(id, kind, subject, claim_hash, evidence, base_rank, \
         first_seen_at_ms, last_seen_at_ms, repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, 0.0, 0, 0, ?6)",
        params![
            format!("{POISON_PREFIX}dream"),
            format!("{POISON_PREFIX}kind"),
            format!("{POISON_PREFIX}subj"),
            format!("{POISON_PREFIX}claim"),
            format!("{POISON_PREFIX}ev"),
            POISON_REPO_ID
        ],
    )?;
    conn.execute(
        "INSERT INTO reconcile_attempts(started_at_ms, status, repo_id) VALUES (0, ?1, ?2)",
        params![format!("{POISON_PREFIX}status"), POISON_REPO_ID],
    )?;

    // --- Dream v2 verification siblings: each carries its own `repo_id`, so a sibling row is now
    // valid and any unscoped verification-queue / evidence / summary / failure read/count/delete
    // trips a tripwire. They all hang off the poison memory id. ---
    conn.execute(
        "INSERT INTO memory_reality(memory_id, repo_id, content_hash, checked_at_ms)
         VALUES (?1, ?2, ?3, 0)",
        params![POISON_MEMORY_ID, POISON_REPO_ID, format!("{POISON_PREFIX}bodyhash")],
    )?;
    conn.execute(
        "INSERT INTO memory_summaries(memory_id, repo_id, content_hash, summary, generated_at_ms)
         VALUES (?1, ?2, ?3, ?4, 0)",
        params![
            POISON_MEMORY_ID,
            POISON_REPO_ID,
            format!("{POISON_PREFIX}bodyhash"),
            format!("{POISON_PREFIX}summary")
        ],
    )?;
    conn.execute(
        "INSERT INTO memory_model_failures(memory_id, repo_id, pass, content_hash, model_id, \
         prompt_version, reason, failed_at_ms)
         VALUES (?1, ?2, 'compact', ?3, ?4, ?5, 'summary_guard_rejected', 0)",
        params![
            POISON_MEMORY_ID,
            POISON_REPO_ID,
            format!("{POISON_PREFIX}bodyhash"),
            format!("{POISON_PREFIX}model"),
            format!("{POISON_PREFIX}prompt")
        ],
    )?;

    // --- SAME-PATH tripwires: sibling rows whose PATH (or path+sha / path+byte) deliberately
    // collides with a real primary row (`collision_path`). A join-by-<key> aggregate that reads a
    // scoped table without a `repo_id` predicate attributes these to the active repo. The
    // DISTINCT-PATH rows above cannot catch that class — their `zz_poison_` paths never match a
    // primary path. Each row is under `POISON_REPO_ID`, so `clear_sibling`'s existing
    // `WHERE repo_id = POISON_REPO_ID` deletes them; the intact check pins each by its own sentinel
    // column (not by path). ---
    // files: caught by any unscoped `main.files` read that groups/joins by path (the scope view
    // would exclude the sibling, so only a view-bypassing path read leaks). No children hung off
    // it.
    // Generation 0 (the sibling's live generation), as for the distinct-path file above.
    conn.execute(
        "INSERT INTO main.files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
         commit_sha, worktree_id, repo_id, generation)
         VALUES (?1, 'rust', 'source', ?2, 0, 0, ?3, '', ?4, 0)",
        params![collision_path, POISON_SAMEPATH_SHA, POISON_COMMIT, POISON_REPO_ID],
    )?;
    // git_file_changes at the shared path (off the sibling commit): caught by an unscoped churn /
    // co-change / history aggregate that joins `git_file_changes.path` onto the scoped `files`.
    conn.execute(
        "INSERT INTO git_file_changes(commit_hash, path, additions, deletions, change_kind, \
         repo_id)
         VALUES (?1, ?2, ?3, 0, 'modified', ?4)",
        params![POISON_COMMIT, collision_path, POISON_SAMEPATH_ADDITIONS, POISON_REPO_ID],
    )?;
    // parser_failures at the shared path (PK is (repo_id, path), so this is distinct from the
    // distinct-path failure): caught by an unscoped parser-failure read joined/grouped by path.
    conn.execute(
        "INSERT INTO parser_failures(repo_id, path, language, message) VALUES (?1, ?2, 'rust', ?3)",
        params![POISON_REPO_ID, collision_path, POISON_SAMEPATH_MSG],
    )?;
    // papertrail_refs at the shared source_path: THE canonical join-by-path leak (the
    // `papertrail_ref_counts` CTE in `repo_brief::file_rows`). Distinct `item_key` from the
    // distinct-path ref, so `idx_papertrail_refs_unique` is satisfied.
    conn.execute(
        "INSERT INTO papertrail_refs(tracker, project, item_key, ref_kind, source_kind, \
         source_path, source_text, discovered_at_ms, repo_id)
         VALUES ('github', ?1, ?2, 'closing', 'file', ?3, ?4, 0, ?5)",
        params![
            POISON_PROJECT,
            POISON_SAMEPATH_ITEM_KEY,
            collision_path,
            POISON_SAMEPATH_REFTEXT,
            POISON_REPO_ID
        ],
    )?;
    // repo_memory_bindings at the shared `path` (V042): a SECOND path-binding off the poison memory
    // whose `path` column is a REAL primary path — caught by the memory path-join readers
    // (`repo_brief::memory_counts_by_path`, `orientation` memory titles, `memory::memories_for_*`)
    // if they forget the `repo_id` predicate. The distinct-path binding leaves `path` NULL, so it
    // never reaches those `path IS NOT NULL` readers. Distinct `binding_id` for the PK.
    conn.execute(
        "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, path, \
         anchor_status, created_at_ms, repo_id)
         VALUES (?1, 'path', ?2, ?3, 'current', 0, ?4)",
        params![POISON_MEMORY_ID, POISON_SAMEPATH_BIND, collision_path, POISON_REPO_ID],
    )?;
    // edge_oracle at the shared (source_path, file_sha) (V042): the `edge_oracle`→`files` metric
    // join keys on `files.path = source_path AND files.sha256 = file_sha`, so it is scoped only
    // transitively through the `files` view — a sibling row whose path AND sha match a real primary
    // file (an identical vendored/shared file across repos) leaks unless the read filters
    // `edge_oracle.repo_id`. `collision_sha` is the primary file's sha at `collision_path` (or the
    // fallback when no primary file exists — then nothing collides).
    let collision_sha: String = conn.query_row(
        "SELECT COALESCE(
             (SELECT sha256 FROM main.files WHERE path = ?1 AND repo_id != ?2 ORDER BY sha256 \
         LIMIT 1),
             ?3)",
        params![collision_path, POISON_REPO_ID, POISON_SAMEPATH_SHA],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT INTO edge_oracle(repo_id, source_path, source_start_byte, source_end_byte, \
         callee_start_byte, callee_end_byte, edge_kind, file_sha, tool, tool_version, \
         scip_symbol, kind, computed_at)
         VALUES (?1, ?2, 0, 0, 0, 0, 'calls_name', ?3, 'scip-rust', ?4, ?5, 'resolved', 0)",
        params![
            POISON_REPO_ID,
            collision_path,
            collision_sha,
            format!("{POISON_PREFIX}ver"),
            POISON_SAMEPATH_SCIP
        ],
    )?;

    // --- registry rows (A7): register the sibling as a REAL repo ONLY on a DB that already holds a
    // real fixture repo (a git fixture). This is the genuinely multi-repo shape A7 makes the
    // default, and it makes an unscoped `repos`/`repo_roots`/`repo_meta` read/count/delete trip a
    // tripwire. On a placeholder-only (non-git) fixture the sibling stays registry-less — a second
    // real repo would hijack `sole_repo_id` for the many fixtures that rely on it (see the module
    // docs). `repo_roots` / `repo_meta` FK `repos(repo_id)` ON DELETE CASCADE, so insert the parent
    // `repos` row FIRST (the connection runs `foreign_keys = ON`). ---
    if primary_is_real(conn)? {
        conn.execute(
            "INSERT INTO repos(repo_id, display_name, registered_at_ms) VALUES (?1, ?2, 0)",
            params![POISON_REPO_ID, format!("{POISON_PREFIX}name")],
        )?;
        conn.execute(
            "INSERT INTO repo_roots(repo_id, root, registered_at_ms) VALUES (?1, ?2, 0)",
            params![POISON_REPO_ID, POISON_REPO_ROOT],
        )?;
        conn.execute("INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)", params![
            POISON_REPO_ID,
            POISON_META_KEY,
            POISON_META_VALUE
        ])?;
    }

    Ok(())
}

/// Whether the DB already holds a REAL fixture repo — a `repos` row that is neither the
/// `__unassigned__` placeholder nor the poison sibling itself. Gates whether [`seed_sibling`]
/// registers the sibling as a real repo and whether [`sibling_tripwires`] appends the registry
/// tripwires (see the module docs). Stable across the mutation under test: the fixture's own real
/// repo row is never the target of the unscoped-read leaks the harness hunts, so a seed-time and an
/// assert-time evaluation agree — a leak that deletes the SIBLING's registry rows is still caught.
pub(super) fn primary_is_real(conn: &Connection) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM repos WHERE repo_id != ?1 AND repo_id != ?2",
        params![rag_rat_base::repo_identity::LEGACY_REPO_ID, POISON_REPO_ID],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// The lexicographically-first REAL (non-sibling) indexed path — the primary-repo path the
/// SAME-PATH tripwires collide onto. Falls back to [`POISON_SAMEPATH_FALLBACK`] when the fixture
/// indexed no files (an empty repo): there is then nothing to collide with, but the seeded rows
/// still guard against an unscoped DELETE. Deterministic (`ORDER BY path`), so a meta-test can
/// re-resolve the same path. Resolve it BEFORE seeding the sibling's own rows (all under
/// `POISON_REPO_ID`, so `repo_id != POISON_REPO_ID` also excludes them defensively).
fn primary_collision_path(conn: &Connection) -> anyhow::Result<String> {
    let path: String = conn.query_row(
        "SELECT COALESCE(
             (SELECT path FROM main.files WHERE repo_id != ?1 ORDER BY path LIMIT 1),
             ?2)",
        params![POISON_REPO_ID, POISON_SAMEPATH_FALLBACK],
        |row| row.get(0),
    )?;
    Ok(path)
}

/// Remove every poison-sibling row, child→parent, so [`seed_sibling`] is idempotent across repeated
/// rebuilds on one DB. Explicit child-first order works whether or not the FK cascades fire.
fn clear_sibling(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(&format!(
        "DELETE FROM logical_symbol_monikers WHERE logical_symbol_id = {POISON_LOGICAL_ID};
         DELETE FROM logical_symbol_members WHERE logical_symbol_id = {POISON_LOGICAL_ID};
         DELETE FROM logical_symbols WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM docs WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM edges_data WHERE source_file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM chunks WHERE file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM symbols WHERE file_id IN (SELECT id FROM main.files WHERE repo_id = \
         '{POISON_REPO_ID}');
         DELETE FROM parser_failures WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM packages WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM git_file_changes WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM git_commits WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM main.files WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_refs WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_items WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_comments WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_sync_cursor WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_item_tags WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM papertrail_fts WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM reconcile_attempts WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM memory_reality WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM memory_summaries WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM memory_model_failures WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM dream_findings WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM clone_refinements WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM clone_token_df WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM clone_graph_generations WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM edge_oracle WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM oracle_runs WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repo_memory_fts WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repo_memory_tags WHERE memory_id = '{POISON_MEMORY_ID}';
         DELETE FROM repo_memory_bindings WHERE repo_id = '{POISON_REPO_ID}';
         -- #464: cleared EXPLICITLY (not just via the source FK cascade) so a reseed is idempotent
         -- even with `foreign_keys` off or an orphaned row — else the next INSERT trips the \
         edge_key PK.
         DELETE FROM repo_node_edges WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repo_memories WHERE repo_id = '{POISON_REPO_ID}';
         -- Registry rows (A7): child-first (repo_meta/repo_roots FK repos ON DELETE CASCADE), so a
         -- re-seed on a git fixture starts from a clean slate. No-op when the sibling was never
         -- registered (a placeholder-only fixture).
         DELETE FROM repo_meta WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repo_roots WHERE repo_id = '{POISON_REPO_ID}';
         DELETE FROM repos WHERE repo_id = '{POISON_REPO_ID}';"
    ))?;
    Ok(())
}
