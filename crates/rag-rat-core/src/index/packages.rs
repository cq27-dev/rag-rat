//! Package-root refresh (crate-aware import scope) for the active checkout.

use super::*;

impl IndexDatabase {
    /// Rewrite the `packages` rows for the ACTIVE scope from the corpus's Cargo manifests, then
    /// persist the per-repo local-crate-root union to `repo_meta.local_crate_roots` (#61
    /// per-package import scope). Returns whether the package map changed (so an incremental
    /// pass can trigger a re-resolve only when it must).
    ///
    /// "Changed" is reported when EITHER the global union changed OR any package's per-scope
    /// `(manifest_dir, local_roots_json)` set changed. Reporting only the global union missed a
    /// per-package alias move/add/remove that leaves the union identical (e.g. moving a path-dep
    /// alias from member A to member B): the union is the same, but A and B now resolve `use
    /// alias::…` differently, so the edges must be re-resolved. A union-only signal left those
    /// results stale until an unrelated Rust edit or a full rebuild.
    ///
    /// Scoped by `(active_commit_sha, active_worktree_id)` like `files`: each pass owns its scope's
    /// rows (DELETE + reinsert), and a sibling worktree's package rows are untouched. The file→
    /// package mapping is NOT persisted onto `files`: `load_package_roots_into_scope` (resolve.rs)
    /// computes it at LOAD time by longest-`manifest_dir`-prefix over the active scope's `packages`
    /// rows + the active files. Persisting a `files.package_id` pointer was the #106 multi-worktree
    /// leak — a clean file is a SHARED commit-scope row (`commit_sha=HEAD, worktree_id=''`) read by
    /// every worktree at that commit, while a package row is worktree-scoped, so one worktree's
    /// refresh stamped its package ids onto the shared rows another worktree then followed (and the
    /// DELETE-and-reinsert churns the AUTOINCREMENT ids each pass, invalidating any sibling's
    /// pointer). Computing the mapping at load reads each scope's OWN `packages`, so no
    /// cross-worktree pointer exists to leak.
    pub(super) fn refresh_packages(&self, root: &Path) -> anyhow::Result<bool> {
        let (global_roots, packages) = super::edges::scan_packages(root);
        let conn = self.storage.connection();
        // Snapshot this scope's existing package map BEFORE the DELETE so a per-package alias
        // change (same global union) is still detected as a change → forces a re-resolve.
        let previous_package_map: std::collections::BTreeMap<String, String> = {
            let mut stmt = conn.prepare(
                "SELECT manifest_dir, local_roots_json FROM packages WHERE commit_sha = ?1 AND \
                 worktree_id = ?2",
            )?;
            let rows = stmt
                .query_map(params![self.active_commit_sha, self.active_worktree_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
            rows.collect::<Result<_, _>>()?
        };
        // Replace this scope's package rows. The id is reassigned each rebuild, which is fine — the
        // file→package mapping is computed at LOAD time from `manifest_dir`, not from a persisted
        // id.
        conn.execute("DELETE FROM packages WHERE commit_sha = ?1 AND worktree_id = ?2", params![
            self.active_commit_sha,
            self.active_worktree_id
        ])?;
        // The freshly-written map, compared against `previous_package_map` below.
        let mut current_package_map: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for package in &packages {
            let roots_json = serde_json::to_string(
                &package.local_roots.iter().collect::<std::collections::BTreeSet<_>>(),
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO packages(manifest_dir, commit_sha, worktree_id, \
                 local_roots_json) VALUES (?1, ?2, ?3, ?4)",
                params![
                    package.manifest_dir,
                    self.active_commit_sha,
                    self.active_worktree_id,
                    roots_json
                ],
            )?;
            current_package_map.insert(package.manifest_dir.clone(), roots_json);
        }
        let package_map_changed = current_package_map != previous_package_map;

        let serialized = {
            let mut sorted: Vec<&str> = global_roots.iter().map(String::as_str).collect();
            sorted.sort_unstable();
            sorted.join("\n")
        };
        let union_changed = self.set_repo_meta_if_changed("local_crate_roots", &serialized)?;
        Ok(union_changed || package_map_changed)
    }
}
