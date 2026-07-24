use super::*;

impl IndexDatabase {
    /// Whether a live (non-deleted) OVERLAY source row for `path` exists in `worktree_id`'s scope —
    /// the gate for removing a now-non-indexable BRANCH-ONLY overlay row that has no base row to
    /// shadow (#679). Distinct from [`Self::base_scope_has_path`] (which probes the base scope).
    pub(super) fn overlay_source_row_exists(
        &self,
        path: &Path,
        worktree_id: &str,
    ) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM main.files WHERE repo_id = ?1 AND path = ?2 AND \
             commit_sha = '' AND worktree_id = ?3 AND kind != 'deleted' AND generation = ?4)",
            params![self.active_repo_id, path_string(path), worktree_id, self.active_generation],
            |row| row.get(0),
        )?)
    }

    /// Whether a live-generation tombstone for `path` already exists in `worktree_id`'s overlay
    /// scope — the idle-safe guard before writing one, so a re-run on a static worktree writes
    /// nothing. A direct `main.files` probe with explicit `repo_id` (A3) + `generation` (A6)
    /// predicates: only a tombstone at THIS connection's live generation suppresses the write.
    pub(super) fn overlay_tombstone_exists(
        &self,
        path: &Path,
        worktree_id: &str,
    ) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM main.files WHERE repo_id = ?1 AND path = ?2 AND \
             commit_sha = '' AND worktree_id = ?3 AND kind = 'deleted' AND generation = ?4)",
            params![self.active_repo_id, path_string(path), worktree_id, self.active_generation],
            |row| row.get(0),
        )?)
    }

    /// Whether the BASE scope has a live (non-deleted) row for `path` at `base_sha` — the gate for
    /// shadowing a base row with an overlay tombstone (there is nothing to shadow otherwise).
    pub(super) fn base_scope_has_path(&self, base_sha: &str, path: &Path) -> anyhow::Result<bool> {
        Ok(self.storage.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM main.files WHERE repo_id = ?1 AND path = ?2 AND \
             commit_sha = ?3 AND worktree_id = '' AND kind != 'deleted' AND generation = ?4)",
            params![self.active_repo_id, path_string(path), base_sha, self.active_generation],
            |row| row.get(0),
        )?)
    }

    /// Decide how a whole-delta refresh sources the COMMITTED half of its candidate set (#825).
    /// When both CURRENT heads (re-read here, from the already-opened repos) still equal the
    /// recorded #577 basis, the base↔linked tree diff would reproduce exactly the last complete
    /// refresh's candidate outcome — materialized as the worktree's current overlay rows — so the
    /// walk is skipped and those rows seed the candidates instead. Only complete refreshes record
    /// the basis, only refreshes (or basis-clearing path-scoped passes) mutate overlay rows, and
    /// a commit racing this read lands the safe way (the stored pair mismatches → full diff).
    ///
    /// Forced to the full diff when the branch config's targets may drift against the base
    /// fingerprint: a re-target/re-language changes categorization without moving a HEAD and can
    /// surface committed branch-only files that never produced a row, which the row seed provably
    /// cannot see. (`compute_linked_worktree_delta` itself falls back on a dirty `.gitignore`
    /// edit, the other row-blind case.)
    pub(super) fn resolve_committed_delta_source(
        &self,
        overlay: &ResolvedOverlayScope,
        config: &Config,
    ) -> anyhow::Result<CommittedDeltaSource> {
        let heads_unchanged = self.worktree_overlay_basis(&overlay.worktree_id)?.is_some_and(
            |(recorded_base, recorded_linked)| {
                recorded_base == overlay.base_sha
                    && recorded_linked == git_context::repo_head_sha(&overlay.linked_repo)
            },
        );
        if !heads_unchanged || self.overlay_targets_may_drift(&config.targets)? {
            return Ok(CommittedDeltaSource::TreeDiff);
        }
        Ok(CommittedDeltaSource::UnchangedSinceBasis {
            shadowed_paths: self.list_overlay_shadowed_paths(&overlay.worktree_id)?,
        })
    }

    /// Every path `worktree_id`'s overlay claims at the live generation — source rows AND
    /// tombstones (`kind = 'deleted'`), config-root-relative: the materialized `shadowing_paths`
    /// outcome of the last complete refresh, i.e. the committed-candidate seed when both HEADs
    /// still match the recorded basis (#825). Tombstones MUST be included, or the seeded pass's
    /// prune would drop them and un-shadow branch-deleted base files.
    fn list_overlay_shadowed_paths(&self, worktree_id: &str) -> anyhow::Result<Vec<PathBuf>> {
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(
            "SELECT path FROM main.files WHERE repo_id = ?1 AND commit_sha = '' AND worktree_id = \
             ?2 AND generation = ?3",
        )?;
        let paths = stmt
            .query_map(params![self.active_repo_id, worktree_id, self.active_generation], |row| {
                row.get::<_, String>(0)
            })?
            .map(|row| row.map(PathBuf::from))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(paths)
    }

    /// Existing file rows in a scope as `path → (sha256, language, kind)` — for the identity-aware
    /// idle-safe skip above. A direct `main.files` probe (bypasses the repo-scoped view), so it
    /// carries the `repo_id` predicate explicitly (A3): today's sole caller passes a non-empty
    /// (path-derived, globally unique) `worktree_id`, but this is the documented reusable primitive
    /// — a base-scope caller (`commit_sha`, `''`) would otherwise skip files on the strength of a
    /// fork sibling's sha.
    pub(super) fn scope_file_identities(
        &self,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<HashMap<String, (String, String, String)>> {
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(
            "SELECT path, sha256, language, kind FROM main.files
             WHERE repo_id = ?1 AND commit_sha = ?2 AND worktree_id = ?3",
        )?;
        let rows =
            stmt.query_map(params![self.active_repo_id, commit_sha, worktree_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?),
                ))
            })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    /// Config-aware overlay reconcile: extra `(readable, tombstone)` candidates for files whose
    /// overlay state under `config` (the BRANCH overlay config) differs from the base index for a
    /// reason the content-based delta (`compute_linked_worktree_delta`, tree-diff + status) cannot
    /// see — a branch config change that RE-LANGUAGES, newly TARGETS, or DROPS a byte-identical
    /// file. Mirrors discovery's `(language, kind)` staleness for the overlay (#659 review). Two
    /// directions, both EXCLUDING `covered` (paths the content delta already reconciles):
    ///  - walk the branch checkout's target files (the branch config over `source_root`): a file
    ///    the base index LACKS (newly-targetable) or whose stored base identity DIFFERS
    ///    (re-languaged) → readable, so the overlay row carries the branch identity; a file whose
    ///    base identity already matches shows through the base row and needs no overlay;
    ///  - base rows the branch config NO LONGER targets → tombstone, so the stale base row is
    ///    shadowed instead of showing through.
    ///
    /// Read-only unless there is REAL divergence; on a later pass the row already exists in the
    /// overlay scope, so the identity-aware skip in `index_explicit_paths_from_root` makes it
    /// write-free. The caller GATES this on the branch target fingerprint differing from the base's
    /// ([`IndexDatabase::overlay_targets_may_drift`]), so a no-divergent-config worktree never runs
    /// the walk (#577 event-scoping).
    pub(super) fn overlay_target_config_reconcile(
        &self,
        base_sha: &str,
        config: &Config,
        source_root: &Path,
        covered: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        // Base-scope identity: path → (language, kind).
        let base_identity: HashMap<PathBuf, (String, String)> = {
            let conn = self.storage.connection();
            let mut stmt = conn.prepare(
                "SELECT path, language, kind FROM main.files
                 WHERE repo_id = ?1 AND commit_sha = ?2 AND worktree_id = '' AND generation = ?3
                   AND kind != 'deleted'",
            )?;
            stmt.query_map(params![self.active_repo_id, base_sha, self.active_generation], |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    (row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                ))
            })?
            .collect::<Result<_, _>>()?
        };
        let mut readable = Vec::new();
        let mut visited = BTreeSet::new();
        // Direction 1 — every file the BRANCH config targets in the checkout. `collect_index_files`
        // walks the branch config's targets over `source_root` (the linked checkout), honoring its
        // `.gitignore`, exactly like the base walker.
        let walk_config = Config { root: source_root.to_path_buf(), ..config.clone() };
        for file in collect_index_files(&walk_config)? {
            let rel = file.relative_path;
            if covered.contains(&rel) {
                continue;
            }
            visited.insert(rel.clone());
            let branch_identity =
                (file.language.as_str().to_string(), file.kind.as_str().to_string());
            if base_identity.get(&rel) == Some(&branch_identity) {
                continue; // the base row already carries the branch identity → it shows through
            }
            readable.push(rel); // newly-targetable OR re-languaged → shadow with the branch parse
        }
        // Direction 2 — base rows the branch config NO LONGER targets (the walk never reached them,
        // and the branch config doesn't claim them) → shadow the stale base row. A base row still
        // targeted but absent in the checkout is a content deletion the delta covers.
        let mut tombstones = Vec::new();
        for rel in base_identity.keys() {
            if covered.contains(rel) || visited.contains(rel) {
                continue;
            }
            if target_for_path(config, rel).is_none() {
                tombstones.push(rel.clone());
            }
        }
        Ok((readable, tombstones))
    }
}
