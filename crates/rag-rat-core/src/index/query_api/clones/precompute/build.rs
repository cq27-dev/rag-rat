use super::storage::{open_building_generation, snapshot_clone_df_epoch};
use super::*;

impl IndexDatabase {
    /// ONE precompute pass: resume (or start) the building generation toward the current content
    /// revision, stream symbols from the resume cursor emitting verified clone edges, checkpoint
    /// per batch, and — if the walk finishes within budget — publish the generation as live.
    /// Mirrors the embedding `reconcile` pass shape (skip-when-current, budgeted batch loop,
    /// `Partial`/`Complete`).
    pub(crate) fn reconcile_clone_edges_pass(
        &self,
        options: &CloneEdgeOptions,
    ) -> anyhow::Result<CloneEdgeReport> {
        let started = Instant::now();
        let conn = self.storage.connection();
        let source_revision = self.content_revision()?;

        // Phase 0 — skip-when-current: a live generation built against the current revision +
        // normalizer is already fresh, so do nothing (no write lock churn on an idle repo).
        if !options.force
            && let Some(live) = live_generation_row(conn)?
            && live.source_revision == source_revision
            && live.normalizer_version == NORM_VERSION
            // Also require postings-completeness, so an upgrade from a pre-postings graph does not
            // skip forever with an empty `clone_subblock_postings` (review R2). A postings-less
            // live generation falls through and rebuilds a postings-full one.
            && live.postings_written
            // And the pinned df epoch (#479, same R2 shape): an epoch-less Complete generation is
            // unservable by the fast path and the delta, so skipping-as-current would strand them
            // on the fallback forever — fall through and rebuild one that pins its epoch.
            && clone_df_epoch_serves(conn, live.generation)?
            // #473: a generation that has absorbed enough in-place delta files owes a df-epoch
            // refresh — it is FRESH (deltas keep `source_revision` current) but must not
            // skip-as-current, or the drift rebuild the quiet gate scheduled would no-op forever.
            && live.delta_files_applied < CLONE_GRAPH_DRIFT_REBUILD_FILES
        {
            return Ok(CloneEdgeReport {
                status: "Current".to_string(),
                generation: live.generation,
                symbols_total: 0,
                symbols_processed: 0,
                edges_written: live.edges_written,
                source_revision,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }

        // Phase 1 — open or resume a Building generation toward THIS revision. A Building
        // generation toward a different revision (a reindex landed since it started) is
        // discarded — its symbol-id cursor is meaningless against the new symbol rows.
        let building = open_building_generation(conn, &source_revision)?;
        // A FRESH build (cursor 0 ⇒ no symbol walked ⇒ zero postings staged for this generation)
        // moves the df epoch to now (#477 review): the #473 drift rebuild exists to restore
        // sub-block selectivity, so a full build that kept the old df would reset the drift
        // counter without delivering the refresh it promises — and more generally, every fresh
        // generation's postings must be ordered by the df of ITS OWN build. The refreshed df is
        // then pinned durably in `clone_df_epoch` (#479): that snapshot — not the live
        // `clone_token_df`, which moves on incremental passes — is what the delta pass and the
        // write-time fast path read back for this generation. A RESUMED partial (cursor > 0)
        // must NOT refresh or re-snapshot: its persisted postings are ordered by the epoch its
        // build opened under.
        if building.cursor_symbol_id == 0 {
            self.refresh_clone_token_df()?;
            snapshot_clone_df_epoch(conn, building.generation)?;
        }

        // Load the scoped baseline bags + the content anchors for every scoped symbol, and build
        // the struct-hash buckets + sub-block inverted index in RAM (rebuilt each pass —
        // cheap relative to the pair emission this avoids persisting postings for). Bags are
        // ordered by the generation's PINNED epoch (#479): identical to the just-refreshed live
        // df on a fresh build, and — the case that matters — the OPEN-time order on a resumed
        // partial, whose remaining postings must match the ones already staged even if the live
        // table moved between the paused passes (Codex review of this change).
        let epoch_df = load_clone_df_epoch(conn, building.generation)?;
        let bags = load_scoped_baseline_bags_with_df(conn, &epoch_df)?;
        let symbols_total = bags.len() as u64;
        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
        let anchors = resolve_symbol_anchors(conn)?;
        let struct_buckets = build_struct_hash_buckets(&bags);
        let inverted = build_sub_block_index(&bags, CLONE_PRECOMPUTE_THETA);

        let deadline = options.max_seconds.map(|s| started + Duration::from_secs(s));
        let mut cursor = building.cursor_symbol_id;
        let mut edges_written = building.edges_written;
        let mut processed: u64 = 0;
        let mut batch: Vec<EdgeRow> = Vec::with_capacity(options.batch_size);
        let mut postings: Vec<PostingGroup> = Vec::with_capacity(options.batch_size);
        let mut budget_tripped = false;

        for bag in bags.iter().filter(|b| b.symbol_id > building.cursor_symbol_id) {
            let s = bag.symbol_id;
            let Some(s_anchor) = anchors.get(&s) else { continue }; // unscoped/raced symbol — skip

            // Struct-hash exact partners (t > s, same (struct_hash, language)) — similarity 1.0, no
            // verify.
            let mut struct_partners: BTreeSet<i64> = BTreeSet::new();
            if let Some(ids) =
                struct_buckets.get(&(bag.struct_hash.as_str(), bag.language.as_str()))
            {
                for &t in ids {
                    if t > s
                        && let Some(t_anchor) = anchors.get(&t)
                    {
                        struct_partners.insert(t);
                        batch.push(make_edge(
                            s_anchor,
                            bag.token_len,
                            t_anchor,
                            by_id[&t].token_len,
                            bag.token_len,
                            1.0,
                            "struct_hash",
                        ));
                    }
                }
            }

            // Sub-block candidates (t > s, same language, sharing a sub-block token) — verified. A
            // pair already emitted as a struct-hash exact pair is skipped (it would
            // re-verify to sim 1.0).
            // This symbol's sub-block tokens, computed ONCE: they drive both candidate generation
            // (below) and the persisted postings (further below). They are exactly what
            // `build_sub_block_index` stores per symbol, so the persisted set is parity-identical.
            let sub_tokens = sub_block_tokens(bag, CLONE_PRECOMPUTE_THETA);
            let mut candidates: BTreeSet<i64> = BTreeSet::new();
            for token in &sub_tokens {
                if let Some(ids) = inverted.get(token) {
                    for &t in ids {
                        if t > s
                            && !struct_partners.contains(&t)
                            && by_id[&t].language == bag.language
                        {
                            candidates.insert(t);
                        }
                    }
                }
            }
            for t in candidates {
                let Some(t_anchor) = anchors.get(&t) else { continue };
                let other = by_id[&t];
                if verified_clone(bag, other, CLONE_PRECOMPUTE_THETA) {
                    let ov = overlap(bag, other);
                    let max_len = bag.token_len.max(other.token_len);
                    let sim = ov as f64 / max_len as f64;
                    batch.push(make_edge(
                        s_anchor,
                        bag.token_len,
                        t_anchor,
                        other.token_len,
                        ov,
                        sim,
                        "sub_block",
                    ));
                }
            }

            // Persist this symbol's sub-block postings, content-anchored, in the SAME generation as
            // its edges. Emitted for EVERY walked symbol — including one with zero verified
            // partners (no edges) — and staged BEFORE the cursor advances, so a
            // budget-split resume can never leave a walked symbol without postings
            // (review R6). Idempotent under the content-key PK.
            if !sub_tokens.is_empty() {
                postings.push(PostingGroup { anchor: s_anchor.clone(), tokens: sub_tokens });
            }

            processed += 1;
            cursor = s;

            // Flush on EITHER accumulator filling: postings are per-symbol (far more numerous than
            // per-pair edges), so a run of high-posting / low-edge symbols must still checkpoint
            // and bound RAM. Both accumulators flush together in one transaction with
            // the cursor.
            if batch.len() >= options.batch_size || postings.len() >= options.batch_size {
                edges_written += flush_batch(
                    conn,
                    building.generation,
                    &mut batch,
                    &mut postings,
                    cursor,
                    edges_written,
                )?;
                if let Some(dl) = deadline
                    && Instant::now() >= dl
                {
                    budget_tripped = true;
                    break;
                }
            }
        }
        // Flush the remainder + checkpoint the final cursor for this pass.
        edges_written += flush_batch(
            conn,
            building.generation,
            &mut batch,
            &mut postings,
            cursor,
            edges_written,
        )?;

        let status = if budget_tripped {
            "Partial"
        } else {
            // The walk reached the last symbol: publish this generation as live, GC the rest.
            self.complete_generation(building.generation, edges_written)?;
            "Complete"
        };

        Ok(CloneEdgeReport {
            status: status.to_string(),
            generation: building.generation,
            symbols_total,
            symbols_processed: processed,
            edges_written,
            source_revision,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// Mark a generation `Complete`, flip the live pointer to it (the atomic publish), and GC every
    /// other generation (CASCADE drops their edges). The `set_meta` flip is the last write, so a
    /// reader sees either the previous live generation or this one, never a half-built one.
    pub(super) fn complete_generation(
        &self,
        generation: i64,
        edges_written: u64,
    ) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        conn.execute(
            // Seed the cached posting-row count (#830) from one COUNT at publish time — the
            // generation's postings are fully written by now, and the delta pass maintains this
            // incrementally thereafter, so this is the only full COUNT the graph ever pays.
            "UPDATE clone_graph_generations
                SET status = 'Complete', finished_at_ms = ?1, edges_written = ?2,
                    postings_written = 1,
                    postings_row_count = (SELECT COUNT(*) FROM clone_subblock_postings
                                           WHERE build_generation = ?3)
              WHERE generation = ?3",
            params![now_ms(), edges_written as i64, generation],
        )?;
        self.set_repo_meta("clone_graph_live_generation", &generation.to_string())?;
        // GC superseded generations PER REPO (A5): repo-scoped so completing repo A's generation
        // never deletes (and CASCADE-wipes the edges/postings of) a sibling repo's generations —
        // the "clone precompute on repo A leaves repo B's generation untouched" contract. This
        // real `repo_id` predicate (from V042's `clone_graph_generations.repo_id`) SUPERSEDES the
        // A3 `multiple_real_repos` seam guard. `{repo_clause}` is empty pre-A5, restoring the
        // original global sweep.
        let repo_clause = clone_generation_scope_clause(conn)?;
        conn.execute(
            &format!("DELETE FROM clone_graph_generations WHERE generation != ?1{repo_clause}"),
            params![generation],
        )?;
        Ok(())
    }
}

/// The `(struct_hash, language) -> [symbol_id]` buckets — the exact key `add_struct_hash_pairs`
/// uses, so the emitted struct-hash pairs match the live path.
fn build_struct_hash_buckets(bags: &[SymbolBag]) -> BTreeMap<(&str, &str), Vec<i64>> {
    let mut buckets: BTreeMap<(&str, &str), Vec<i64>> = BTreeMap::new();
    for bag in bags {
        buckets
            .entry((bag.struct_hash.as_str(), bag.language.as_str()))
            .or_default()
            .push(bag.symbol_id);
    }
    buckets
}

/// The `token_hash -> [symbol_id]` inverted index over sub-block tokens — the same index
/// `sub_block_candidate_pairs` builds, so a symbol's candidates match the live path.
pub(super) fn build_sub_block_index(bags: &[SymbolBag], theta: f64) -> BTreeMap<i64, Vec<i64>> {
    let mut inverted: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for bag in bags {
        for token_hash in sub_block_tokens(bag, theta) {
            inverted.entry(token_hash).or_default().push(bag.symbol_id);
        }
    }
    inverted
}

/// Resolve every scoped, non-generated symbol to its reindex-stable content anchor `(path,
/// start_byte, file_sha)`. Mirrors `load_scoped_baseline_bags`'s `symbols JOIN files` + `generated
/// = 0` scope so the anchor set covers exactly the bag symbols.
pub(super) fn resolve_symbol_anchors(conn: &Connection) -> anyhow::Result<BTreeMap<i64, Anchor>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, f.path, s.start_byte, f.sha256
           FROM symbols s
           JOIN files f ON f.id = s.file_id
          WHERE f.generated = 0",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            (row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?),
        ))
    })?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (id, anchor) = row?;
        map.insert(id, anchor);
    }
    Ok(map)
}

/// Build a content-anchored edge with endpoints in canonical `(path, start_byte)` order (the PK
/// order). `overlap`/`similarity` are symmetric; the per-endpoint `token_len`s follow the chosen
/// orientation.
pub(in super::super) fn make_edge(
    s_anchor: &Anchor,
    s_token_len: i64,
    t_anchor: &Anchor,
    t_token_len: i64,
    overlap: i64,
    similarity: f64,
    edge_source: &'static str,
) -> EdgeRow {
    let s_key = (s_anchor.0.as_str(), s_anchor.1);
    let t_key = (t_anchor.0.as_str(), t_anchor.1);
    let ((a, a_len), (b, b_len)) = if s_key <= t_key {
        ((s_anchor, s_token_len), (t_anchor, t_token_len))
    } else {
        ((t_anchor, t_token_len), (s_anchor, s_token_len))
    };
    EdgeRow {
        a_path: a.0.clone(),
        a_start_byte: a.1,
        a_file_sha: a.2.clone(),
        b_path: b.0.clone(),
        b_start_byte: b.1,
        b_file_sha: b.2.clone(),
        overlap,
        a_token_len: a_len,
        b_token_len: b_len,
        similarity,
        edge_source,
    }
}

/// Insert a batch of edges AND the walked symbols' sub-block postings (both idempotent under resume
/// via `INSERT OR IGNORE` on their content-key PKs) and checkpoint the generation's cursor + edge
/// count — all in ONE transaction, so postings, edges, and the cursor advance atomically together
/// (review R6: a symbol's postings are durable before its symbol id is checkpointed as done).
/// Returns the EDGE rows actually inserted (dedup-ignored rows don't count).
fn flush_batch(
    conn: &Connection,
    generation: i64,
    batch: &mut Vec<EdgeRow>,
    postings: &mut Vec<PostingGroup>,
    cursor: i64,
    cumulative_edges: u64,
) -> anyhow::Result<u64> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> anyhow::Result<u64> {
        let inserted = insert_edge_rows(conn, generation, batch)?;
        insert_posting_groups(conn, generation, postings)?;
        conn.execute(
            "UPDATE clone_graph_generations SET cursor_symbol_id = ?1, edges_written = ?2
              WHERE generation = ?3",
            params![cursor, (cumulative_edges + inserted) as i64, generation],
        )?;
        Ok(inserted)
    })();
    match result {
        Ok(inserted) => {
            conn.execute_batch("COMMIT")?;
            batch.clear();
            postings.clear();
            Ok(inserted)
        },
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        },
    }
}
