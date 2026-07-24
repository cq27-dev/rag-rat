use super::*;

impl IndexDatabase {
    /// True when the precomputed graph is ABSENT or STALE vs the current content. The background
    /// watcher / maintenance tails wrap this in [`Self::clone_graph_rebuild_due`] (the #472
    /// quiet-window gate); explicit callers (`clones --precompute`) read the staleness directly.
    pub fn pending_clone_graph(&self) -> anyhow::Result<bool> {
        let revision = self.content_revision()?;
        self.clone_graph_stale_against(&revision)
    }

    /// The staleness core of [`Self::pending_clone_graph`], against an already-computed
    /// `content_revision()` so the quiet gate pays the digest once per probe.
    /// Whether the live clone generation is stale against the CURRENT `content_revision()` — the
    /// revision-keyed staleness the quiet gate reads. `false` for a graph that looks fresh, which
    /// is why a revision-neutral `SelfHeal` Escalate needs a forced rebuild (#830,
    /// `watch::pass`): the drift is real but the revision — hence this check — cannot see it.
    pub(crate) fn clone_graph_stale(&self) -> anyhow::Result<bool> {
        let revision = self.content_revision()?;
        self.clone_graph_stale_against(&revision)
    }

    fn clone_graph_stale_against(&self, content_revision: &str) -> anyhow::Result<bool> {
        let conn = self.storage.connection();
        let Some(live) = live_generation_row(conn)? else {
            return Ok(true); // no completed generation yet
        };
        Ok(live.source_revision != content_revision
            || live.normalizer_version != NORM_VERSION
            // A live generation built before postings existed (`postings_written = 0`) is pending
            // so one rebuild pass fills `clone_subblock_postings` — else an upgraded DB with an
            // already-Complete clone graph would keep an empty postings table forever (review R2).
            || !live.postings_written
            // Same self-heal contract for the df epoch (#479, the R2 shape): a Complete
            // generation without its `clone_df_epoch` rows (the V051 backfill's empty-df edge, a
            // torn epoch) is unservable by the fast path and the delta — it must read as pending
            // so one rebuild re-pins it, not skip as current forever.
            || !clone_df_epoch_serves(conn, live.generation)?)
    }

    /// The background quiet-window gate for the clone-graph tail (#472): true when the graph is
    /// pending AND the content revision has been stable for `quiet_ms`. Under sustained editing
    /// `content_revision()` moves every pass, so an ungated tail discards the in-flight Building
    /// generation and rebuilds the whole graph from symbol 0 each time (measured ~1 GB of DB
    /// writes per pass); the gate defers the rebuild until the churn pauses. Bookkeeping lives in
    /// per-repo meta (`clone_graph_quiet_*`), so the watcher and the hook-driven CLI `maintenance`
    /// share one window across processes:
    /// - graph current → drop any armed candidate, not due;
    /// - stale revision != armed candidate (or nothing armed) → (re-)arm the window, not due;
    /// - stale revision == armed candidate for ≥ `quiet_ms` → due.
    ///
    /// `probe_without_candidate = false` lets an idle pass skip the probe — and its
    /// content-revision digest — entirely when no deferred rebuild is owed (nothing armed).
    /// `quiet_ms = 0` disables the gate (a pending graph is immediately due). Explicit rebuild
    /// paths (`clones --precompute`, full `index`) bypass the gate entirely.
    pub fn clone_graph_rebuild_due(
        &self,
        quiet_ms: i64,
        probe_without_candidate: bool,
    ) -> anyhow::Result<bool> {
        self.clone_graph_rebuild_due_at(now_ms(), quiet_ms, probe_without_candidate)
    }

    /// [`Self::clone_graph_rebuild_due`] with the clock injected.
    ///
    /// "Owed" here means a FULL rebuild: the graph is stale in a way the #473 delta can't settle
    /// (absent / normalizer bump / postings gap), OR the live generation has absorbed enough
    /// delta files ([`CLONE_GRAPH_DRIFT_REBUILD_FILES`]) that its frozen df epoch owes a refresh.
    /// A merely revision-stale-but-delta-eligible graph also arms here — if the delta settles it
    /// first, the next probe sees it current and disarms. `content_revision()` is now an O(1) read
    /// (#828), so the probe and the same pass's clone delta each recompute it freely — no pinned
    /// digest is threaded between them.
    pub(crate) fn clone_graph_rebuild_due_at(
        &self,
        now_ms: i64,
        quiet_ms: i64,
        probe_without_candidate: bool,
    ) -> anyhow::Result<bool> {
        let candidate = self.clone_graph_quiet_candidate()?;
        if candidate.is_none() && !probe_without_candidate {
            return Ok(false);
        }
        let revision = self.content_revision()?;
        let drifted = {
            let conn = self.storage.connection();
            live_generation_row(conn)?
                .is_some_and(|live| live.delta_files_applied >= CLONE_GRAPH_DRIFT_REBUILD_FILES)
        };
        let due = if !self.clone_graph_stale_against(&revision)? && !drifted {
            if candidate.is_some() {
                self.clear_clone_graph_quiet_candidate()?;
            }
            false
        } else if quiet_ms == 0 {
            true
        } else {
            match candidate {
                Some((armed_revision, since_ms)) if armed_revision == revision =>
                    now_ms.saturating_sub(since_ms) >= quiet_ms,
                _ => {
                    self.set_repo_meta(CLONE_GRAPH_QUIET_REVISION_META, &revision)?;
                    self.set_repo_meta(CLONE_GRAPH_QUIET_SINCE_META, &now_ms.to_string())?;
                    false
                },
            }
        };
        Ok(due)
    }

    /// Whether the #472 quiet gate holds an armed candidate. Watch-level tests assert the #817
    /// posture through this (an overlay-only pass neither probes nor arms) — `repo_meta` is not
    /// reachable from outside the `index` module tree.
    #[cfg(test)]
    pub(crate) fn clone_graph_quiet_candidate_armed(&self) -> bool {
        self.clone_graph_quiet_candidate().ok().flatten().is_some()
    }

    /// The armed quiet-window candidate, if any: the stale revision under observation and when it
    /// was first seen. A torn/corrupt pair reads as absent (the next probe re-arms it).
    fn clone_graph_quiet_candidate(&self) -> anyhow::Result<Option<(String, i64)>> {
        let Some(revision) = self.repo_meta(CLONE_GRAPH_QUIET_REVISION_META)? else {
            return Ok(None);
        };
        let since_ms = self
            .repo_meta(CLONE_GRAPH_QUIET_SINCE_META)?
            .and_then(|since| since.parse::<i64>().ok());
        Ok(since_ms.map(|since_ms| (revision, since_ms)))
    }

    fn clear_clone_graph_quiet_candidate(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        rag_rat_db::meta::delete_repo_meta(
            conn,
            &self.active_repo_id,
            CLONE_GRAPH_QUIET_REVISION_META,
        )?;
        rag_rat_db::meta::delete_repo_meta(
            conn,
            &self.active_repo_id,
            CLONE_GRAPH_QUIET_SINCE_META,
        )?;
        Ok(())
    }

    /// The live clone-graph generation the write-time postings fast path may read from —
    /// `Some(gen)` ONLY when the persisted postings are safe to serve, `None` otherwise (→ the
    /// caller uses the RAM fallback). Eligibility is EXACT-freshness, deliberately STRICTER
    /// than the `find_clones` edge fast path's "mildly-stale-OK" (review R1): the postings cover
    /// exactly the file set of `source_revision`, so a generation drifted from
    /// `content_revision()` could disagree with what the live index would compute — a silent
    /// missed near-clone. So require:
    /// - a `Complete` live generation (the meta live-pointer only ever names a Complete one),
    /// - `normalizer_version == NORM_VERSION`,
    /// - `postings_written` (a postings-complete, postings-aware generation — review R2),
    /// - `source_revision == content_revision()` EXACTLY (not merely present), AND
    /// - the generation's `clone_df_epoch` rows exist (#479 — the order to read the postings by).
    pub fn clone_check_indexed_generation(&self) -> anyhow::Result<Option<i64>> {
        // BASE-SCOPE ONLY. The clone graph (edges + postings) is built in the BASE scope —
        // maintenance restores it before the clone-graph pass, and `content_revision()` is GLOBAL
        // over `main.files` so it CANNOT encode which scope produced the postings. Under a
        // linked-worktree OVERLAY the postings cover only base-scope symbols, while the RAM
        // fallback reads the overlay's branch-only symbols; serving the fast path there
        // would silently miss overlay near-clones. So disable it under a linked overlay —
        // those scopes fall back to the correct, overlay-scoped RAM build.
        if self.active_scope_is_linked_overlay() {
            return Ok(None);
        }
        let conn = self.storage.connection();
        let Some(live) = live_generation_row(conn)? else {
            return Ok(None);
        };
        let eligible = live.normalizer_version == NORM_VERSION
            && live.postings_written
            && live.source_revision == self.content_revision()?
            // #479: the postings are ordered by the generation's pinned epoch; without the epoch
            // rows (a pre-V051 build the backfill could not cover) the reader cannot reproduce
            // that order — fall back rather than silently miss near-clones.
            && clone_df_epoch_serves(conn, live.generation)?;
        Ok(eligible.then_some(live.generation))
    }
}
