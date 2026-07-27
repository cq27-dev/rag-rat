//! Index garbage collection on `IndexDatabase`: prune file/chunk/embedding/symbol/edge rows for
//! git contexts that are no longer live, plus the [`GcReport`] it returns.

use rag_rat_db::meta::scoped_table_row_count;
use serde::Serialize;

use super::*;
use crate::index::rebuild::StagedSweep;

#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    pub files_pruned: u64,
    pub chunks_pruned: u64,
    pub files_remaining: u64,
    pub chunks_remaining: u64,
    /// True when no live context could be determined and pruning was skipped (nothing deleted).
    pub skipped: bool,
}

impl IndexDatabase {
    /// Garbage-collect index rows for git contexts that are no longer live. Keeps the active
    /// commit and overlay of every worktree reported by `git worktree list` (plus this
    /// connection's active context) and prunes file/chunk/embedding/symbol/edge rows for any
    /// other commit. Never prunes when no live context can be determined (non-git, git error).
    pub fn garbage_collect(&self) -> anyhow::Result<GcReport> {
        let mut live_commits = Vec::new();
        let mut live_worktrees = Vec::new();
        if let Some(root) = self.storage.source_root() {
            let (commits, worktrees) = live_worktree_contexts(root);
            live_commits.extend(commits);
            live_worktrees.extend(worktrees);
        }
        // Always keep this connection's active context, even if git enumeration missed it.
        if !self.active_commit_sha.is_empty() {
            live_commits.push(self.active_commit_sha.clone());
        }
        if !self.active_worktree_id.is_empty() {
            live_worktrees.push(self.active_worktree_id.clone());
        }
        live_commits.sort();
        live_commits.dedup();
        live_worktrees.sort();
        live_worktrees.dedup();
        let report = self.prune_to_live(&live_commits, &live_worktrees)?;
        // #828 §9.1: gc runs under the write flock every GC_EVERY_PASSES passes — the low-cadence
        // home for the content-digest parity self-check. It recomputes the digest from scan and, on
        // the (fail-closed-so-unexpected) mismatch a trigger/migration regression would cause,
        // reseeds `content_digest_state` in place. Deliberately does NOT re-stamp fts/clone stamps.
        self.verify_content_digest_parity()?;
        Ok(report)
    }

    /// Prune file rows (and their derived rows) whose `commit_sha` and `worktree_id` are both
    /// outside the live sets, after unconditionally sweeping the repo's DEAD file generations
    /// (which need no git context — see the sweep comment below). Refuses CONTEXT pruning when
    /// both live sets are empty, so a missing live set never wipes the index. `parser_failures`
    /// (path-keyed, generation-less) fall only with a dead CONTEXT — never with a dead
    /// generation, whose paths live on ([`StagedSweep`]).
    pub fn prune_to_live(
        &self,
        live_commits: &[String],
        live_worktrees: &[String],
    ) -> anyhow::Result<GcReport> {
        let conn = self.storage.connection();
        // Counts are SCOPED to the active repo (the DELETEs below always were): on a consolidated
        // DB a whole-table count would report the union across every repo — `files_remaining: 2`
        // from a repo owning one file — and a sibling's index pass committing between the
        // before/after reads would skew the derived pruned counts (saturating_sub can zero them).
        let files_before = scoped_table_row_count(conn, "files", &self.active_repo_id)?;
        let chunks_before = scoped_chunk_row_count(conn, &self.active_repo_id)?;
        // A6 (P2 review): sweep this repo's DEAD file GENERATIONS FIRST, and UNCONDITIONALLY —
        // BEFORE the empty-live-sets early return below. Generation liveness needs only
        // `repo_id` + the repo's live-generation pointer, never a git context, so the "no live
        // context ⇒ refuse to prune" guard (which protects CONTEXT pruning from a missing live
        // set) must not gate it: behind the guard, a non-git / plain-directory index would leak a
        // full generation of files/chunks/symbols/FTS rows on EVERY rebuild, unbounded.
        //
        // DEADNESS IS `generation != live` UNDER A LOCK PRECONDITION (batch-5 P2, superseding the
        // batch-3 `< live` form): non-live generations split into SUPERSEDED (`< live`) and
        // ABANDONED staging (`> live` — a failed rebuild's committed-but-never-flipped waves; a
        // persistently failing tail would otherwise leak a full staged copy PER RETRY,
        // unreclaimable by a below-live sweep). Sweeping ABOVE live is safe exactly when no
        // rebuild is mid-flight — and the PER-REPO WRITE FLOCK is that proof: every production
        // rebuild entry runs under it (CLI `index`, the watcher/maintenance passes; verified —
        // the only flock-less rebuild callers are tests), so a collector HOLDING the flock knows
        // any above-live rows are abandoned, not in-progress.
        //
        // INVARIANT (documented precondition, not asserted): callers of `garbage_collect` /
        // `prune_to_live` hold this repo's write flock. All production callers do — `Cmd::Gc`
        // (batch-4 belt), `run_maintenance_pass`, and the watcher's pass all acquire it before
        // opening. A hypothetical flock-less embedded collector must fall back to the
        // lockless-safe `< live` form — do NOT weaken this predicate's precondition silently.
        // (The allocator still guarantees staging sits strictly above live:
        // `next_files_generation` = max(row-MAX, live) + 1.) `StagedSweep::DeadGeneration` keeps
        // the cascade off the generation-less `parser_failures` (the same paths live on in the
        // current generation). Scoped to the active repo, so a sibling's generations are
        // untouched.
        conn.execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS staged_file_ids(id INTEGER PRIMARY KEY);
            DELETE FROM temp.staged_file_ids;
            ",
        )?;
        let live_generation =
            rag_rat_db::schema::live_files_generation(conn, &self.active_repo_id)?;
        conn.execute(
            "INSERT OR IGNORE INTO temp.staged_file_ids(id)
             SELECT id FROM main.files WHERE repo_id = ?1 AND generation != ?2",
            params![self.active_repo_id, live_generation],
        )?;
        self.delete_staged_files_cascade(StagedSweep::DeadGeneration)?;
        conn.execute_batch("DELETE FROM temp.staged_file_ids;")?;
        if live_commits.is_empty() && live_worktrees.is_empty() {
            let files_remaining = scoped_table_row_count(conn, "files", &self.active_repo_id)?;
            let chunks_remaining = scoped_chunk_row_count(conn, &self.active_repo_id)?;
            return Ok(GcReport {
                files_pruned: files_before.saturating_sub(files_remaining),
                chunks_pruned: chunks_before.saturating_sub(chunks_remaining),
                files_remaining,
                chunks_remaining,
                // Context pruning was skipped (no live context could be determined); the
                // generation sweep above needs no context and ran regardless.
                skipped: true,
            });
        }
        conn.execute_batch(
            "
            CREATE TEMP TABLE IF NOT EXISTS gc_live_commits(sha TEXT PRIMARY KEY);
            DELETE FROM temp.gc_live_commits;
            CREATE TEMP TABLE IF NOT EXISTS gc_live_worktrees(id TEXT PRIMARY KEY);
            DELETE FROM temp.gc_live_worktrees;
            DELETE FROM temp.staged_file_ids;
            ",
        )?;
        {
            let mut stmt =
                conn.prepare("INSERT OR IGNORE INTO temp.gc_live_commits(sha) VALUES (?1)")?;
            for sha in live_commits {
                stmt.execute([sha])?;
            }
        }
        {
            let mut stmt =
                conn.prepare("INSERT OR IGNORE INTO temp.gc_live_worktrees(id) VALUES (?1)")?;
            for id in live_worktrees {
                stmt.execute([id])?;
            }
        }
        // A file survives if its commit is live OR its worktree overlay is live. Empty-string
        // keys never appear in the live sets, so unkeyed rows are pruned. Scoped to the ACTIVE REPO
        // (A3): the live sets are THIS repo's live contexts, so without the `repo_id` predicate
        // every OTHER repo's files (whose commits/worktrees are legitimately absent from
        // this repo's live sets) would be staged and cascade-deleted — gc of one repo would
        // wipe every sibling. The repo_id filter is what makes `garbage_collect` per-repo.
        conn.execute(
            "
            INSERT OR IGNORE INTO temp.staged_file_ids(id)
            SELECT id FROM main.files
            WHERE repo_id = ?1
              AND commit_sha NOT IN (SELECT sha FROM temp.gc_live_commits)
              AND worktree_id NOT IN (SELECT id FROM temp.gc_live_worktrees)
            ",
            [self.active_repo_id.as_str()],
        )?;
        // Context-death cascade: this staging holds only rows whose (commit, worktree) fell out of
        // every live set, so path-keyed satellite state (`parser_failures`) goes with them. The
        // dead-GENERATION sweep already ran above, unconditionally.
        self.delete_staged_files_cascade(StagedSweep::DeadContext)?;
        conn.execute_batch("DELETE FROM temp.staged_file_ids;")?;
        // A removed worktree's overlay refresh basis (#577) follows its overlay rows out.
        self.prune_worktree_overlay_basis_outside(live_worktrees)?;
        // Since #248 `edge_oracle` is content-keyed with NO `edges_data` FK (it survives reindex,
        // the moniker model), so the file/edge prune above no longer cascades verdicts
        // away. Dangling verdicts are harmless for correctness — every read joins live
        // `edges` by content key, so a verdict whose edge is gone never resolves — but the
        // per-file incremental reindex (`remove_file_in_scope`) DELETEs a changed file's
        // edges every pass and would otherwise let those orphan rows grow without bound.
        // Sweep them GLOBALLY here (rows with no live edge in ANY scope; a checkout-scoped
        // sweep would wrongly delete a sibling worktree's live verdict). `oracle_runs` is
        // keyed by `(commit_sha, worktree_id)` directly — nothing cascades it either — so
        // prune a dead checkout's run rows with the SAME live sets, so a run and the
        // edges it produced are dropped together.
        rag_rat_oracle::prune_edge_oracle_without_live_edge(conn)?;
        // Per-repo (A5): `prune_oracle_runs_outside_scope` now filters `oracle_runs.repo_id` (its
        // own column since V042), so it deletes only THIS repo's run rows that fall outside THIS
        // repo's live sets — a sibling repo's runs are legitimately absent from this repo's live
        // set but are no longer touched. That real `repo_id` predicate SUPERSEDES the old V042-seam
        // `multiple_real_repos` guard, so the prune runs unconditionally again (exactly as it did
        // on a single-repo DB before scoping).
        rag_rat_oracle::prune_oracle_runs_outside_scope(conn, live_commits, live_worktrees)?;
        // #114: `external_symbols` is checkout-keyed like `oracle_runs` (nothing cascades it), so a
        // dead checkout's dependency contracts need the SAME per-repo, dead-scope prune — otherwise
        // signature/doc payloads for retired branches/worktrees accumulate without bound.
        rag_rat_oracle::prune_external_symbols_outside_scope(conn, live_commits, live_worktrees)?;
        // #357: `embedding_cache` is content-keyed too (survives reindex, like the oracle above),
        // so it needs the SAME global sweep — drop vectors no live chunk references in ANY
        // context (a sibling worktree / branch may still use one, so this must not be
        // checkout-scoped) that are also past the switch-back grace. ORDERING: this MUST stay AFTER
        // `delete_staged_files_cascade` above, so a just-removed dead context's chunk_embeddings
        // are already gone and this cycle also reclaims their now-unreferenced vectors.
        // (Running it before the cascade is still safe — it would just keep dead vectors
        // one extra cycle.)
        crate::index::ai::prune_embedding_cache_unreferenced(conn)?;
        // Dictionary hygiene (#79, extended #224): drop `name_strings` values nothing references
        // any more. The pool has NO FKs by design (see the schema comment), so orphans
        // accumulate as edges/symbols are pruned; the vocabulary is small, but gc is the
        // natural rate-limited home for the sweep. Every referencing column must appear
        // here — a missed column would null its strings out from under live rows. #224
        // added `symbols.qualified_name_id` and `logical_symbols.qualified_name_id`
        // (interned symbol qnames live in this same pool now); omitting them would delete a
        // pool entry a live symbol points at and null its qname out — the exact footgun
        // this comment warns about (regression test:
        // gc_preserves_a_name_strings_entry_referenced_only_by_a_symbol).
        //
        // SHARED-POOL INVARIANT (A3): `name_strings` stays GLOBAL — it is NOT repo-sliced. Liveness
        // is the UNION over ALL repos: the referencing subqueries below read `edges_data` /
        // `symbols` / `logical_symbols` across every repo, so a value one repo still
        // references is kept even while gc runs for a different repo. Scoping this sweep to
        // the active repo would delete interned strings out from under a sibling repo's
        // live rows. The same union-liveness rule governs the other content-addressed
        // shared tables gc touches — `embedding_cache`
        // (`prune_embedding_cache_unreferenced`, keyed by content hash, swept over live chunks in
        // ANY scope) — and `chunk_text_dict`, whose immutable decode dictionaries are never swept
        // at all (a version any repo's blob references must survive), so it needs no
        // per-repo logic.
        conn.execute(
            "
            DELETE FROM main.name_strings
            WHERE id NOT IN (
                SELECT from_name_id FROM main.edges_data WHERE from_name_id IS NOT NULL
                UNION SELECT to_name_id FROM main.edges_data
                UNION SELECT target_qualified_name_id FROM main.edges_data
                    WHERE target_qualified_name_id IS NOT NULL
                UNION SELECT receiver_hint_id FROM main.edges_data
                    WHERE receiver_hint_id IS NOT NULL
                UNION SELECT receiver_type_hint_id FROM main.edges_data
                    WHERE receiver_type_hint_id IS NOT NULL
                UNION SELECT resolution_id FROM main.edges_data
                UNION SELECT edge_kind_id FROM main.edges_data
                UNION SELECT confidence_id FROM main.edges_data
                UNION SELECT qualified_name_id FROM main.symbols
                    WHERE qualified_name_id IS NOT NULL
                UNION SELECT qualified_name_id FROM main.logical_symbols
                    WHERE qualified_name_id IS NOT NULL
            )
            ",
            [],
        )?;
        let files_remaining = scoped_table_row_count(conn, "files", &self.active_repo_id)?;
        let chunks_remaining = scoped_chunk_row_count(conn, &self.active_repo_id)?;
        Ok(GcReport {
            files_pruned: files_before.saturating_sub(files_remaining),
            chunks_pruned: chunks_before.saturating_sub(chunks_remaining),
            files_remaining,
            chunks_remaining,
            skipped: false,
        })
    }
}
