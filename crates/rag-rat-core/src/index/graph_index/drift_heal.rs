//! Logical-key drift healing: snapshot the referenced old-derivation rows before a rebuild,
//! then re-point each reference at its unique re-derived successor.

use super::references::{
    bindings_lens_capable, column_present, null_call_path_references,
    remap_logical_symbol_references, table_present, vacate_logical_symbol_references,
};
use crate::index::*;

/// One REFERENCED old-derivation logical row, snapshotted before a key-drift rebuild clears the
/// table (#493): the key fields the heal matches on, plus the id the durable references still
/// hold. `signature` is recovered from the surviving members when they UNANIMOUSLY carry the same
/// non-null capture ([`UNANIMOUS_MEMBER_SIGNATURE_SQL`]); `None` when no member survives (an
/// incremental pass already replaced the file's symbols) or the members disagree — signature
/// evidence then simply cannot corroborate, and qualified-name agreement must carry the match
/// alone.
#[derive(Debug)]
pub(in crate::index) struct LogicalKeyDriftRow {
    pub(in crate::index) old_id: i64,
    path: String,
    name: String,
    qualified_name: Option<String>,
    scope_path: Option<String>,
    kind: String,
    pub(in crate::index) signature: Option<String>,
}

impl LogicalKeyDriftRow {
    /// The identity fields a survivor probe compares against — everything in the logical key
    /// except the id. A re-derived row still carrying this exact key is an UNCHANGED symbol; a
    /// key-mismatched survivor is a key swap (a different symbol occupying the old id).
    fn key(&self) -> DriftKey {
        DriftKey {
            path: self.path.clone(),
            name: self.name.clone(),
            qualified_name: self.qualified_name.clone(),
            scope_path: self.scope_path.clone(),
            kind: self.kind.clone(),
            signature: self.signature.clone(),
        }
    }
}

/// The comparable identity of a logical row (its key minus the id), shared by the snapshot row
/// and the survivor probe so the equality check is one `==` rather than a five-field tuple that
/// trips `clippy::type_complexity`.
#[derive(Debug, PartialEq, Eq)]
struct DriftKey {
    path: String,
    name: String,
    qualified_name: Option<String>,
    scope_path: Option<String>,
    kind: String,
    signature: Option<String>,
}

/// A re-derived logical row competing to inherit a drifted reference — see
/// [`IndexDatabase::heal_logical_key_drift`]. Carries `kind` so the strict (kind-agreeing)
/// subset is split in memory from one kind-relaxed fetch, keeping cross-kind contenders visible
/// for the decoy veto ([`conflicting_drift_evidence`]).
struct LogicalKeyDriftCandidate {
    id: i64,
    kind: String,
    qualified_name: Option<String>,
    signature: Option<String>,
}

/// The group-signature evidence subquery shared by the drift snapshot, the survivor probe, and
/// the candidate scan (#493): a group's signature is evidence only when EVERY member carries the
/// SAME non-null capture. The `COUNT(*) = COUNT(s.signature)` guard is load-bearing —
/// `COUNT(DISTINCT)` alone ignores NULLs, so a partially captured group (one member recaptured,
/// one signature-less) would read as unanimous on the sole non-null value and hand its references
/// to whichever overload that member happened to match. Expects the enclosing query to alias
/// `logical_symbols` as `ls`.
const UNANIMOUS_MEMBER_SIGNATURE_SQL: &str = "
    (SELECT CASE WHEN COUNT(*) = COUNT(s.signature) AND COUNT(DISTINCT s.signature) = 1
                 THEN MIN(s.signature) END
       FROM logical_symbol_members m
       JOIN symbols s ON s.id = m.symbol_id
      WHERE m.logical_symbol_id = ls.id)";

/// Whether one evidence axis (qualified name / signature) corroborates a drift match: both sides
/// present and equal. An unrecoverable side (`None`) is NO evidence, never a wildcard — matching
/// on absence is how a heal would guess.
fn drift_evidence_agrees(old: &Option<String>, new: &Option<String>) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => old == new,
        _ => false,
    }
}

/// The unique realign target among EVIDENCE-ELIGIBLE candidates, or `None` for ambiguity: one
/// eligible candidate wins outright; several narrow to those agreeing on BOTH axes and only a
/// unique survivor wins — overload twins whose evidence drifted on both axes stay unmatched for
/// the validate-time relocation ladder, because a confidently mis-anchored memory is worse than a
/// flagged one.
fn unique_drift_winner(
    old: &LogicalKeyDriftRow,
    eligible: &[&LogicalKeyDriftCandidate],
) -> Option<i64> {
    match eligible {
        [only] => Some(only.id),
        [] => None,
        several => {
            let both: Vec<&LogicalKeyDriftCandidate> = several
                .iter()
                .copied()
                .filter(|candidate| {
                    drift_evidence_agrees(&old.qualified_name, &candidate.qualified_name)
                        && drift_evidence_agrees(&old.signature, &candidate.signature)
                })
                .collect();
            match both.as_slice() {
                [only] => Some(only.id),
                _ => None,
            }
        },
    }
}

/// Whether the kind-relaxed pool holds CONFLICTING evidence against a strict-pass winner (#493
/// review): the winner is carried by ONE axis while some other pool member agrees on the axis
/// the winner lacks. This is the kind-drift decoy shape — a kind-classification bump moves the
/// true symbol OFF the old kind while a same-(path, name, qualified-name) twin stays on it; the
/// twin wins the strict pass on qualified name alone and would silently inherit the reference,
/// with the true row (agreeing on signature) never consulted. Split evidence is ambiguity, and
/// ambiguity falls to the relocation ladder. A both-axes winner has no missing axis, so nothing
/// can veto it.
fn conflicting_drift_evidence(
    old: &LogicalKeyDriftRow,
    pool: &[LogicalKeyDriftCandidate],
    winner_id: i64,
) -> bool {
    let Some(winner) = pool.iter().find(|candidate| candidate.id == winner_id) else {
        return false;
    };
    let winner_qual = drift_evidence_agrees(&old.qualified_name, &winner.qualified_name);
    let winner_sig = drift_evidence_agrees(&old.signature, &winner.signature);
    pool.iter().filter(|candidate| candidate.id != winner_id).any(|candidate| {
        (!winner_qual && drift_evidence_agrees(&old.qualified_name, &candidate.qualified_name))
            || (!winner_sig && drift_evidence_agrees(&old.signature, &candidate.signature))
    })
}

impl IndexDatabase {
    /// Memoize the #493 drift snapshot BEFORE symbol rows are deleted, once per pass:
    /// [`Self::remove_file_in_scope`] calls this first, so whichever file replacement happens
    /// first in a pass captures the evidence while it is still complete — and every later
    /// removal in the same pass sees the memo and pays nothing. Idle passes never remove a file
    /// and never reach this, so the stale-stamp snapshot scan is paid only by passes that
    /// actually mutate files (and once). Reads only `repo_meta` when the key version is
    /// current. A memo left by a different repo's context (consolidated DB) is replaced.
    pub(in crate::index) fn capture_drift_snapshot_before_removal(&self) -> anyhow::Result<()> {
        let mut slot = self.drift_snapshot.lock().expect("drift snapshot lock");
        let current = matches!(slot.as_ref(), Some((repo, _)) if repo == &self.active_repo_id);
        if !current {
            *slot = Some((self.active_repo_id.clone(), self.logical_key_drift_snapshot()?));
        }
        Ok(())
    }

    /// The #493 drift snapshot: `Some(referenced old rows)` when this repo's stamped
    /// `logical_key_version` lags [`LOGICAL_KEY_VERSION`] (including the never-stamped first
    /// rebuild under a stamping binary), `None` when the derivation is current. Captured via
    /// [`Self::capture_drift_snapshot_before_removal`] before a pass's first file removal (the
    /// member-signature evidence lives in the rows being deleted), or fresh at
    /// [`Self::rebuild_logical_symbols`] when nothing was removed. Bounded by the
    /// DURABLE references (memory bindings, call-path endpoints and edge callees, oracle monikers)
    /// rather than the whole table: unreferenced ids need no healing, and the reference set is
    /// dozens of rows where the table is tens of thousands. The `logical_symbols` join keys the
    /// intersection to the ACTIVE repo, so a sibling repo's references never enter a heal that
    /// queries this repo's candidates.
    pub(in crate::index) fn logical_key_drift_snapshot(
        &self,
    ) -> anyhow::Result<Option<Vec<LogicalKeyDriftRow>>> {
        if self.repo_meta(LOGICAL_KEY_VERSION_KEY)?.as_deref() == Some(LOGICAL_KEY_VERSION) {
            return Ok(None);
        }
        let conn = self.storage.connection();
        // The snapshot is bounded to ids holding a DURABLE reference — healing an id nothing points
        // at would be wasted work. That makes this list the definition of "durable reference", so a
        // reference table missing from it is never healed at all, no matter what the remap covers
        // (#810). Both additions below are guarded because a store can predate their tables.
        let mut reference_sources = String::from(
            "
                  SELECT logical_symbol_id FROM repo_memory_bindings
                   WHERE logical_symbol_id IS NOT NULL
                  UNION
                  SELECT start_logical_symbol_id FROM repo_memory_call_paths
                   WHERE start_logical_symbol_id IS NOT NULL
                  UNION
                  SELECT end_logical_symbol_id FROM repo_memory_call_paths
                   WHERE end_logical_symbol_id IS NOT NULL
                  UNION
                  SELECT logical_symbol_id FROM logical_symbol_monikers",
        );
        if table_present(conn, "repo_node_edges")? {
            reference_sources.push_str(
                "
                  UNION
                  SELECT target_logical_symbol_id FROM repo_node_edges
                   WHERE target_logical_symbol_id IS NOT NULL",
            );
        }
        if column_present(conn, "repo_memory_call_path_edges", "callee_logical_symbol_id")? {
            reference_sources.push_str(
                "
                  UNION
                  SELECT callee_logical_symbol_id FROM repo_memory_call_path_edges
                   WHERE callee_logical_symbol_id IS NOT NULL",
            );
        }
        if table_present(conn, "papertrail_distill_anchors")? {
            // Anchors hold the OPAQUE `sym_<hex>` handle as TEXT, so the match runs the other way:
            // format the candidate id and compare. SQLite's `%x` is byte-identical to the Rust
            // `format_sym_handle` encoding, and `stable_id` is a shifted sha256 so ids are never
            // negative — the two agree over the whole domain in play.
            reference_sources.push_str(
                "
                  UNION
                  SELECT anchored.id FROM main.logical_symbols anchored
                   WHERE EXISTS (
                       SELECT 1 FROM papertrail_distill_anchors a
                        WHERE a.logical_symbol_id = 'sym_' || format('%x', anchored.id))",
            );
        }
        let mut stmt = conn.prepare(&format!(
            "
            SELECT ls.id, ls.path, ls.logical_name,
                   (SELECT value FROM name_strings WHERE id = ls.qualified_name_id),
                   (SELECT COALESCE(s.scope_path, '') FROM logical_symbol_members m
                      JOIN symbols s ON s.id = m.symbol_id
                     WHERE m.logical_symbol_id = ls.id LIMIT 1),
                   ls.kind,
                   {UNANIMOUS_MEMBER_SIGNATURE_SQL},
                   ls.language
            FROM main.logical_symbols ls
            WHERE ls.repo_id = ?1
              AND ls.id IN ({reference_sources}
              )
            "
        ))?;
        let rows = stmt
            .query_map(params![self.active_repo_id], |row| {
                Ok(LogicalKeyDriftRow {
                    old_id: row.get(0)?,
                    path: row.get(1)?,
                    name: row.get(2)?,
                    qualified_name: row.get(3)?,
                    scope_path: row.get::<_, Option<String>>(4)?,
                    kind: row.get(5)?,
                    signature: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(rows))
    }

    /// Realign drifted durable references onto the freshly re-derived logical ids (#493). A
    /// snapshot row is drifted when its old id vanished from the rebuilt table OR survives with a
    /// DIFFERENT key (another symbol's re-derived key occupying it — a key swap). Candidates are
    /// the new rows sharing `(path, name, kind)` — falling back to `(path, name)` only when the
    /// strict pass has NO eligible candidate, which is what a kind-classification drift looks
    /// like; a strict winner carried by a single evidence axis is vetoed when a cross-kind row
    /// agrees on the axis it lacks ([`conflicting_drift_evidence`]). The EVIDENCE rule mirrors
    /// the #491/#494 relocation discriminators ([`drift_evidence_agrees`] /
    /// [`unique_drift_winner`]): qualified-name or signature agreement makes a candidate
    /// eligible, ambiguity matches nothing and falls to the validate-time relocation ladder.
    /// Pairs are bijective (two old ids resolving to one new id drop both) and applied
    /// REFERENCE-ONLY via [`remap_logical_symbol_references`] — the re-derived rows already own
    /// the new ids, and an occupying row must stay put.
    pub(super) fn heal_logical_key_drift(
        &self,
        snapshot: &[LogicalKeyDriftRow],
    ) -> anyhow::Result<usize> {
        if snapshot.is_empty() {
            return Ok(0);
        }
        let conn = self.storage.connection();
        // The survivor and candidate probes hit (id) and (repo_id, path, logical_name) once per
        // snapshot row — and a moniker-bearing repo can snapshot close to the whole table, so an
        // unindexed candidate probe would go quadratic inside the rebuild transaction. The index
        // is TRANSIENT: built for this pass, dropped before returning — a persistent index would
        // tax every rebuild's wholesale DELETE + re-INSERT for an event that fires once per
        // derivation bump. Inside a caller's transaction an error path rolls it back with
        // everything else; outside one, the IF NOT EXISTS / IF EXISTS pair converges on the next
        // heal.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS logical_symbols_drift_heal_idx
                ON logical_symbols(repo_id, path, logical_name)",
        )?;
        let mut remap: Vec<(i64, i64)> = Vec::new();
        // Target ids already OWNED by a snapshot row that keeps them — an unchanged survivor, or
        // a drifted row whose evidence winner is its own (occupied) id. A remap pair landing on a
        // claimed id competes with the owner's existing references (the MERGE shape: two old
        // symbols collapsing into one surviving row), so it must drop to the relocation ladder,
        // which relocates with a visible papertrail instead of a silent heal.
        let mut survivor_claims: std::collections::HashSet<i64> = std::collections::HashSet::new();
        // Old ids whose row survived with a DIFFERENT key — another symbol owns them now. If the
        // evidence below cannot realign such a reference, it must still be VACATED off the
        // occupied id (see the tail of this fn).
        let mut occupied_old_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
        // In-place winners (evidence winner == own old id): no remap, but their bind-time
        // discriminators may still be stale under the new derivation — refresh them at the tail
        // alongside the remap targets.
        let mut in_place_winners: Vec<i64> = Vec::new();
        for old in snapshot {
            // A surviving id is proof of an unchanged symbol ONLY when the surviving row still
            // carries the snapshotted key: after a key change, a DIFFERENT symbol's re-derived
            // key can occupy the old id (a key swap — e.g. two same-path/name rows exchanging
            // kind under a kind-classification fix). A key-mismatched survivor is drifted like a
            // vanished id; its references realign by evidence while the occupying row stays put.
            let survivor: Option<DriftKey> = conn
                .query_row(
                    &format!(
                        "
                        SELECT ls.path, ls.logical_name,
                               (SELECT value FROM name_strings WHERE id = ls.qualified_name_id),
                               (SELECT COALESCE(s.scope_path, '') FROM logical_symbol_members m
                                  JOIN symbols s ON s.id = m.symbol_id
                                 WHERE m.logical_symbol_id = ls.id LIMIT 1),
                               ls.kind,
                               {UNANIMOUS_MEMBER_SIGNATURE_SQL},
                               ls.language
                        FROM main.logical_symbols ls
                        WHERE ls.id = ?1 AND ls.repo_id = ?2
                        "
                    ),
                    params![old.old_id, self.active_repo_id],
                    |row| {
                        Ok(DriftKey {
                            path: row.get(0)?,
                            name: row.get(1)?,
                            qualified_name: row.get(2)?,
                            scope_path: row.get::<_, Option<String>>(3)?,
                            kind: row.get(4)?,
                            signature: row.get(5)?,
                        })
                    },
                )
                .optional()?;
            match survivor {
                Some(key) if key == old.key() => {
                    survivor_claims.insert(old.old_id);
                    continue;
                },
                // The old id is OCCUPIED by a different symbol's row: whatever the evidence
                // decides below, the references must not stay parked on it — a live wrong id
                // validates as healthy and the ladder never runs.
                Some(_) => {
                    occupied_old_ids.insert(old.old_id);
                },
                None => {},
            }
            // Strict pass: candidates share (path, name, kind). When the KIND ITSELF drifted (a
            // kind-classification bump — an advertised LOGICAL_KEY_VERSION reason), no strict
            // candidate can agree; fall back to the kind-RELAXED pool so qualified-name /
            // signature agreement can still carry the realign. Relaxation only ever widens an
            // EMPTY eligible set: a strict pass that found agreeing-but-ambiguous candidates
            // must not widen the pool (more candidates cannot disambiguate), and kind stays the
            // primary discriminator wherever it survives — the #491 struct/impl twins are told
            // apart by kind first. A strict winner is still checked against the FULL pool for
            // conflicting cross-kind evidence ([`conflicting_drift_evidence`], the decoy veto).
            let pool = self.drift_eligible_candidates(old)?;
            let strict: Vec<&LogicalKeyDriftCandidate> =
                pool.iter().filter(|candidate| candidate.kind == old.kind).collect();
            let winner = if strict.is_empty() {
                let relaxed: Vec<&LogicalKeyDriftCandidate> = pool.iter().collect();
                unique_drift_winner(old, &relaxed)
            } else {
                unique_drift_winner(old, &strict)
                    .filter(|id| !conflicting_drift_evidence(old, &pool, *id))
            };
            // A winner equal to the old id (the occupying row IS the evidence match) needs no
            // remap — and must not enter the two-phase pass as a self-cycle — but it claims its
            // id like any survivor.
            match winner {
                Some(new_id) if new_id != old.old_id => remap.push((old.old_id, new_id)),
                Some(claimed) => {
                    survivor_claims.insert(claimed);
                    in_place_winners.push(claimed);
                },
                None => {},
            }
        }
        // Bijectivity: a target claimed by more than one old id — including a survivor that
        // keeps it — is ambiguous evidence; drop every contending pair rather than guess which
        // reference inherits it.
        let mut target_claims: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::new();
        for (_, new_id) in &remap {
            *target_claims.entry(*new_id).or_insert(0) += 1;
        }
        remap.retain(|(_, new_id)| target_claims[new_id] == 1 && !survivor_claims.contains(new_id));
        // Clean up every snapshot id that found NO winner — neither remapped, nor an unchanged
        // survivor, nor an in-place winner (`survivor_claims`; its references already belong where
        // they are). Two DISTINCT cleanups by reference kind, because their validators differ:
        //  - Call-path references (endpoints + the `call_path` binding row) have NO validate-time
        //    re-resolution, so a stale endpoint id is permanent whether the old id is OCCUPIED
        //    (live, another symbol) or VANISHED (dead) — NULL them in BOTH cases
        //    (`null_call_path_references`, #493 review).
        //  - Logical/symbol/moniker bindings SELF-HEAL on an unresolvable id (the relocation ladder
        //    / gone status), so they only need pushing OFF an OCCUPIED id — a LIVE wrong id would
        //    otherwise validate as healthy. A VANISHED id is already dead, so the ladder handles it
        //    and no sentinel is needed (`vacate_logical_symbol_references`, occupied-only).
        // MUST run BEFORE the remap: an occupied id can be another drifted reference's legitimate
        // target, and a post-remap cleanup would wipe the freshly realigned references with the
        // stale ones.
        let remapped: std::collections::HashSet<i64> =
            remap.iter().map(|(old_id, _)| *old_id).collect();
        let capable = bindings_lens_capable(conn)?;
        for old in snapshot {
            let old_id = old.old_id;
            if remapped.contains(&old_id) || survivor_claims.contains(&old_id) {
                continue;
            }
            null_call_path_references(conn, old_id, capable)?;
            if occupied_old_ids.contains(&old_id) {
                vacate_logical_symbol_references(conn, old_id, capable)?;
            }
        }
        remap_logical_symbol_references(conn, &remap)?;
        // A realigned reference still carries its bind-time discriminators — `binding_id` (the
        // qualified name), `symbol_kind`, `signature_hash` — from the OLD derivation. Validation
        // treats a live id as current and never repairs those fields, so a LATER churn or
        // relocation would search with stale evidence and miss (or mis-pick) the twin. Refresh
        // them from the row each reference now points at — remap targets and in-place winners
        // alike; exact-key survivors are skipped because an unchanged key implies unchanged
        // discriminators.
        for (_, new_id) in &remap {
            self.refresh_logical_binding_discriminators(*new_id)?;
        }
        for id in in_place_winners {
            self.refresh_logical_binding_discriminators(id)?;
        }
        conn.execute_batch("DROP INDEX IF EXISTS logical_symbols_drift_heal_idx")?;
        Ok(remap.len())
    }

    /// Rewrite every logical binding at `logical_symbol_id` to the relocation discriminators of
    /// the row it now points at (#493 review): `binding_id` = the live qualified name,
    /// `symbol_kind` = the logical row's kind, `signature_hash` = the sha256 of the unanimous
    /// member signature — the same values [`rag_rat_query::memory::resolve_logical_symbol_binding`]
    /// captures at bind time and the validate-time ladder writes on a relocation.
    ///
    /// `binding_id` is part of the `(memory_id, binding_kind, binding_id)` PK, so a rename can
    /// collide with a SIBLING logical binding of the SAME memory that already holds the target
    /// qualified name (a memory bound to the anchor under two derivations). The multi-row
    /// `UPDATE OR IGNORE` would silently SKIP the colliding row, stranding it with stale
    /// discriminators on a now-live id — validate then reads it as current and never repairs it.
    /// So this walks the affected rows one by one and, on an ignored rename, DELETES the stale
    /// duplicate — exactly the PK-collision cleanup `validate_memories` performs.
    fn refresh_logical_binding_discriminators(&self, logical_symbol_id: i64) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        let row: Option<(Option<String>, String, Option<String>)> = conn
            .query_row(
                &format!(
                    "
                    SELECT (SELECT value FROM name_strings WHERE id = ls.qualified_name_id),
                           ls.kind,
                           {UNANIMOUS_MEMBER_SIGNATURE_SQL}
                    FROM main.logical_symbols ls
                    WHERE ls.id = ?1 AND ls.repo_id = ?2
                    "
                ),
                params![logical_symbol_id, self.active_repo_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((Some(qualified_name), kind, signature)) = row else {
            return Ok(());
        };
        let signature_hash =
            signature.map(|sig| rag_rat_base::hash::hex_sha256(sig.trim().as_bytes()));
        // Every binding at this id takes the live discriminators as its RESOLUTION — where this
        // store finds the target and what it landed on. The authored columns are the author's and
        // never change here (#1297): nothing is renamed, so two bindings of one memory collapsing
        // onto one name collide with nothing, and nothing reaches `anchors/1`. A binding whose
        // memory is not materialized here (the drain keeps a removed synced memory's bindings) is
        // refreshed like any other: the heal runs once per key derivation and nothing repeats it
        // when the memory returns. What the row had not resolved yet is carried from the authored
        // value, since a resolved row's shadows are its whole view.
        let changed = conn.execute(
            "UPDATE repo_memory_bindings
                SET resolved = 1, resolved_binding_id = ?2, resolved_symbol_kind = ?3,
                    resolved_signature_hash = ?4,
                    resolved_path = IIF(resolved, resolved_path, path),
                    resolved_start_line = IIF(resolved, resolved_start_line, start_line),
                    resolved_end_line = IIF(resolved, resolved_end_line, end_line),
                    resolved_moniker_tool_version =
                        IIF(resolved, resolved_moniker_tool_version, moniker_tool_version)
              WHERE binding_kind = 'logical_symbol' AND logical_symbol_id = ?1",
            params![logical_symbol_id, qualified_name, kind, signature_hash],
        )? > 0;
        if changed {
            rag_rat_db::meta::bump_lens_revisions(conn, &self.active_repo_id, &[
                rag_rat_db::meta::LENS_ENRICHMENT_REVISION_META,
                rag_rat_db::meta::LENS_MEMORIES_REVISION_META,
            ])?;
        }
        Ok(())
    }

    /// The re-derived rows a drifted reference may realign onto, EVIDENCE-FILTERED: candidates
    /// share the snapshot row's `(path, name)` and are eligible only when the qualified name
    /// agrees OR the signature agrees (both sides present and equal; an unrecoverable side is no
    /// evidence, never a wildcard). Fetched kind-RELAXED in one probe; the caller splits the
    /// strict (kind-agreeing) subset in memory, so cross-kind contenders stay visible for the
    /// decoy veto ([`conflicting_drift_evidence`]) without a second scan.
    fn drift_eligible_candidates(
        &self,
        old: &LogicalKeyDriftRow,
    ) -> anyhow::Result<Vec<LogicalKeyDriftCandidate>> {
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(&format!(
            "
            SELECT ls.id, ls.kind,
                   (SELECT value FROM name_strings WHERE id = ls.qualified_name_id),
                   {UNANIMOUS_MEMBER_SIGNATURE_SQL}
            FROM main.logical_symbols ls
            WHERE ls.repo_id = ?1 AND ls.path = ?2 AND ls.logical_name = ?3
            "
        ))?;
        let candidates = stmt
            .query_map(params![self.active_repo_id, old.path, old.name], |row| {
                Ok(LogicalKeyDriftCandidate {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    qualified_name: row.get(2)?,
                    signature: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(candidates
            .into_iter()
            .filter(|candidate| {
                drift_evidence_agrees(&old.qualified_name, &candidate.qualified_name)
                    || drift_evidence_agrees(&old.signature, &candidate.signature)
            })
            .collect())
    }
}
