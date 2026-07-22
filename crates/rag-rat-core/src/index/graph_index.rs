//! Graph/edge/logical-symbol index lifecycle: resolve edges, (re)build logical symbols, graph
//! coverage, and graph-index freshness.

use std::collections::BTreeMap;

use super::*;

/// Grouping key that collapses cfg variants / overloads of one symbol into a single logical symbol.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct LogicalSymbolKey {
    pub(super) language: String,
    pub(super) path: String,
    pub(super) name: String,
    pub(super) qualified_name: String,
    pub(super) kind: String,
    // Signature is part of the identity so that two distinct same-named symbols in one file (e.g.
    // `new` on two different impls — same `qualified_name`, different signatures) do NOT collapse
    // into one logical symbol. Genuine cfg variants share a signature, so they still group.
    pub(super) signature: Option<String>,
}

impl LogicalSymbolKey {
    pub(super) fn from(row: &LogicalSymbolMemberRow) -> Self {
        Self {
            language: row.language.clone(),
            path: row.path.clone(),
            name: row.name.clone(),
            qualified_name: row.qualified_name.clone(),
            kind: row.kind.clone(),
            signature: row.signature.clone(),
        }
    }

    /// Deterministic logical-symbol id derived from the key AND its owning `repo_id`, so it is
    /// **stable across reindex** (the table is fully rebuilt each pass; an autoincrement rowid
    /// would churn the id every time, breaking any cached id or logical-symbol-bound memory)
    /// yet **repo-distinct** (A3). Folding `repo_id` in is what prevents two repos with
    /// byte-identical file content from deriving the SAME content-only id and colliding on the
    /// `logical_symbols.id` PK in a consolidated DB. Fold — not a composite `(repo_id, id)` PK
    /// — because `id` is ALSO the scalar `sym_<hex>` wire handle and the FK/PK target of
    /// `logical_symbol_members`, `logical_symbol_monikers`, and `repo_memory_bindings`, so it
    /// MUST stay a single globally- unique scalar; a composite PK would demote `id` to
    /// non-unique and break every one of those. `repo_id` is invariant across reindex, so the
    /// id is as stable as before. A 63-bit truncation of the SHA-256 — collisions are
    /// astronomically unlikely across a repo's symbols, and a collision would surface as a loud
    /// primary-key error on rebuild rather than silent merging.
    pub(super) fn stable_id(&self, repo_id: &str) -> i64 {
        let canonical = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            repo_id,
            self.language,
            self.path,
            self.name,
            self.qualified_name,
            self.kind,
            self.signature.as_deref().unwrap_or(""),
        );
        let digest = Sha256::digest(canonical.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) >> 1) as i64
    }
}

/// Recompute every `logical_symbols` row's content-derived id from its OWN `repo_id` and re-point
/// that id everywhere it is referenced, so ids stay consistent now that
/// [`LogicalSymbolKey::stable_id`] folds `repo_id` in (A3). Returns the number of rows remapped.
///
/// WHY: on an UPGRADED DB (existing pre-fold `logical_symbols`), or when a `repo_id` changes at
/// adoption (placeholder → real), the next [`IndexDatabase::rebuild_logical_symbols`] would
/// re-derive every logical symbol under a NEW id, dangling every `repo_memory_bindings`,
/// `repo_memory_call_paths`, `logical_symbol_monikers`, and `logical_symbol_members` row that still
/// points at the OLD id (pre-V040 memories + oracle data). This migrates those references IN PLACE
/// before the first rebuild, so a bound memory resolves to the same symbol under the new id. Called
/// from the V040 migration (after the `repo_id` backfill) and from [`register_repo`] adoption
/// (after the placeholder → real re-point), both idempotent: a row already at `hash(repo_id ‖ key)`
/// is skipped, so the two calls compose without double-remapping.
///
/// The hash inputs are row-resident EXCEPT `signature`, which `logical_symbols` does not store —
/// recover it from any member's `symbols.signature` (every member of a group shares it, since the
/// signature is part of the key). A logical symbol with no live member is an orphan the next
/// rebuild would drop anyway (its binding is already effectively dead), so it is left untouched.
///
/// FK NOTE: `logical_symbol_members` carries an `ON DELETE CASCADE` FK to `logical_symbols(id)`, so
/// the caller MUST run with FK enforcement OFF (the V040 migration) or DEFERRED
/// (`PRAGMA defer_foreign_keys = ON`, the adoption transaction) — else the parent-id UPDATE trips
/// the child FK. The remap runs inside the caller's transaction (torn-safe) and uses a NEGATIVE
/// temp-id pass so a new id that equals another remapped row's OLD id can never collide on the PK
/// mid-migration.
pub(crate) fn realign_logical_symbol_ids(conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
    struct Row {
        old_id: i64,
        repo_id: String,
        language: String,
        path: String,
        name: String,
        qualified_name: Option<String>,
        kind: String,
        signature: Option<String>,
    }
    let mut stmt = conn.prepare(
        "SELECT ls.id, ls.repo_id, ls.language, ls.path, ls.logical_name,
                (SELECT value FROM name_strings WHERE id = ls.qualified_name_id),
                ls.kind,
                (SELECT s.signature FROM logical_symbol_members m
                   JOIN symbols s ON s.id = m.symbol_id
                  WHERE m.logical_symbol_id = ls.id LIMIT 1)
         FROM logical_symbols ls",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Row {
                old_id: r.get(0)?,
                repo_id: r.get(1)?,
                language: r.get(2)?,
                path: r.get(3)?,
                name: r.get(4)?,
                qualified_name: r.get(5)?,
                kind: r.get(6)?,
                signature: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut remap: Vec<(i64, i64)> = Vec::new();
    for row in rows {
        // A NULL interned `qualified_name` means the row predates the #224 backfill (rebuild would
        // itself fail on it) or is otherwise unrecoverable — skip rather than hash a wrong key.
        let Some(qualified_name) = row.qualified_name else {
            continue;
        };
        let key = LogicalSymbolKey {
            language: row.language,
            path: row.path,
            name: row.name,
            qualified_name,
            kind: row.kind,
            signature: row.signature,
        };
        let new_id = key.stable_id(&row.repo_id);
        if new_id != row.old_id {
            remap.push((row.old_id, new_id));
        }
    }

    remap_logical_symbol_ids(conn, &remap)?;
    Ok(remap.len())
}

/// Apply an old → new id remap across the PK row and every referencing table, two-phase through
/// negative temp ids: `stable_id` values are all >= 0 (`>> 1`), so a `-(i+1)` temp can never
/// collide with a real id or with another temp. Phase 1 vacates every OLD id in the remap set;
/// phase 2 lands the finals — so a new id equal to another remapped row's old id never trips the
/// PK mid-pass. (A new id equal to an already-aligned row's id is a 63-bit hash collision — the
/// same astronomically-unlikely case a plain rebuild already surfaces loudly.) Shared by
/// [`realign_logical_symbol_ids`] (`repo_id` changes: the PK row moves with its references) and
/// the key-drift heal (#493: the re-derived rows already hold the new ids, so the
/// `logical_symbols`/members updates no-op and the reference tables are the payload).
fn remap_logical_symbol_ids(
    conn: &rusqlite::Connection,
    remap: &[(i64, i64)],
) -> rusqlite::Result<()> {
    for (i, (old_id, _)) in remap.iter().enumerate() {
        let temp_id = -(i as i64 + 1);
        rewrite_logical_symbol_id(conn, *old_id, temp_id)?;
    }
    for (i, (_, new_id)) in remap.iter().enumerate() {
        let temp_id = -(i as i64 + 1);
        rewrite_logical_symbol_id(conn, temp_id, *new_id)?;
    }
    Ok(())
}

/// Move a single logical-symbol id from `from` to `to` across the PK row and every column that
/// references it — the members join table, the per-tool monikers, and the two durable-memory
/// reference tables (`repo_memory_bindings`, `repo_memory_call_paths`' start/end). See
/// [`realign_logical_symbol_ids`] for the FK-off/deferred requirement.
fn rewrite_logical_symbol_id(
    conn: &rusqlite::Connection,
    from: i64,
    to: i64,
) -> rusqlite::Result<()> {
    conn.execute("UPDATE logical_symbols SET id = ?1 WHERE id = ?2", params![to, from])?;
    conn.execute(
        "UPDATE logical_symbol_members SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
        params![to, from],
    )?;
    rewrite_logical_symbol_references(conn, from, to)
}

/// Move ONLY the durable references to a logical-symbol id — monikers, memory bindings,
/// call-path endpoints — leaving the `logical_symbols` row and its members untouched. The drift
/// heal (#493) must use this shape: after a key change, a snapshot row's OLD id can be OCCUPIED
/// by a different symbol's re-derived row (a key swap, not a hash collision), and the full
/// [`rewrite_logical_symbol_id`] would MOVE that innocent row along with the drifted reference.
fn rewrite_logical_symbol_references(
    conn: &rusqlite::Connection,
    from: i64,
    to: i64,
) -> rusqlite::Result<()> {
    // `logical_symbol_monikers` has no FK, so a DANGLING row (its logical row died in some
    // earlier wholesale rebuild; the next oracle run sweeps it) can already occupy `to` for the
    // same tool — and the plain UPDATE below would abort the whole rebuild on the PK. The moving
    // row is the one bound to the symbol that now lives at `to`, so the stale occupant loses:
    // displace exactly the colliding rows first. The correlate deliberately omits `repo_id`: a
    // logical id is repo-UNIQUE by construction (`stable_id` folds `repo_id`), so same-id +
    // same-tool is the full collision key — and the V040 migration runs this against the
    // pre-V042 moniker shape, which has no `repo_id` column yet.
    conn.execute(
        "DELETE FROM logical_symbol_monikers
          WHERE logical_symbol_id = ?1
            AND EXISTS (SELECT 1 FROM logical_symbol_monikers src
                         WHERE src.logical_symbol_id = ?2
                           AND src.tool = logical_symbol_monikers.tool)",
        params![to, from],
    )?;
    conn.execute(
        "UPDATE logical_symbol_monikers SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
        params![to, from],
    )?;
    conn.execute(
        "UPDATE repo_memory_bindings SET logical_symbol_id = ?1 WHERE logical_symbol_id = ?2",
        params![to, from],
    )?;
    conn.execute(
        "UPDATE repo_memory_call_paths SET start_logical_symbol_id = ?1
          WHERE start_logical_symbol_id = ?2",
        params![to, from],
    )?;
    conn.execute(
        "UPDATE repo_memory_call_paths SET end_logical_symbol_id = ?1
          WHERE end_logical_symbol_id = ?2",
        params![to, from],
    )?;
    Ok(())
}

/// NULL every CALL-PATH reference to a drifted id the heal could not realign (#493 review).
/// Call-path references are the exception to the self-healing ladder: `validate_call_path_binding`
/// re-checks only the stored EDGE fingerprints and NEVER consults or repairs the endpoint ids, so
/// a stale (occupied-by-another OR vanished) endpoint would be a permanent bogus `sym_8000…`
/// hydration surfaces and no validator ever fixes — no matter whether the id is sentineled or
/// left on the dead value. NULL is the supported "no recorded endpoint" state every reader
/// already guards for. Both the `repo_memory_call_paths` endpoint columns AND the
/// `repo_memory_bindings` row for `binding_kind = 'call_path'` (whose `logical_symbol_id` is the
/// start-or-end endpoint, equally ignored by the validator) are cleared. Called for EVERY
/// no-winner id — occupied and vanished alike — unlike the sentinel/delete cleanup below which is
/// occupied-only.
fn null_call_path_references(conn: &rusqlite::Connection, from: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE repo_memory_bindings SET logical_symbol_id = NULL
          WHERE logical_symbol_id = ?1 AND binding_kind = 'call_path'",
        params![from],
    )?;
    conn.execute(
        "UPDATE repo_memory_call_paths SET start_logical_symbol_id = NULL
          WHERE start_logical_symbol_id = ?1",
        params![from],
    )?;
    conn.execute(
        "UPDATE repo_memory_call_paths SET end_logical_symbol_id = NULL
          WHERE end_logical_symbol_id = ?1",
        params![from],
    )?;
    Ok(())
}

/// Move a drifted reference OFF an OCCUPIED id (a LIVE row now belonging to a DIFFERENT symbol)
/// the heal could not realign (#493) — the vanished-id case needs no such move (a dead id already
/// resolves to nothing). Call-path references are handled separately by
/// [`null_call_path_references`]; this covers the two kinds that DO self-heal on an unresolvable
/// id, so they must be pushed off the live-wrong id:
/// - `repo_memory_bindings.logical_symbol_id` (every kind EXCEPT `call_path`) parks on
///   [`VACATED_LOGICAL_SYMBOL_ID`]: an unresolvable id is exactly what makes the validate-time
///   relocation ladder run, so the binding self-heals with a visible papertrail — where leaving it
///   on the occupied LIVE id would validate as healthy and strand it silently.
/// - `logical_symbol_monikers` rows are DELETED: their PK is `(repo_id, logical_symbol_id, tool)`,
///   so two vacated ids carrying the same tool's moniker would collide on a shared sentinel and
///   abort the rebuild — and a moniker pointing at a dead symbol is worthless anyway
///   (oracle-derived, re-derived by the next `oracle run`).
fn vacate_logical_symbol_references(
    conn: &rusqlite::Connection,
    from: i64,
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM logical_symbol_monikers WHERE logical_symbol_id = ?1", params![
        from
    ])?;
    conn.execute(
        "UPDATE repo_memory_bindings SET logical_symbol_id = ?1
          WHERE logical_symbol_id = ?2 AND binding_kind != 'call_path'",
        params![VACATED_LOGICAL_SYMBOL_ID, from],
    )?;
    Ok(())
}

/// The reference-only two-phase remap the drift heal applies (#493): same negative-temp-id
/// discipline as [`remap_logical_symbol_ids`] — with occupied-id swaps in the set, a pair's
/// SOURCE can equal another pair's TARGET, so the direct rewrite would collide mid-pass — but
/// touching only the reference tables (see [`rewrite_logical_symbol_references`]).
fn remap_logical_symbol_references(
    conn: &rusqlite::Connection,
    remap: &[(i64, i64)],
) -> rusqlite::Result<()> {
    for (i, (old_id, _)) in remap.iter().enumerate() {
        let temp_id = -(i as i64 + 1);
        rewrite_logical_symbol_references(conn, *old_id, temp_id)?;
    }
    for (i, (_, new_id)) in remap.iter().enumerate() {
        let temp_id = -(i as i64 + 1);
        rewrite_logical_symbol_references(conn, temp_id, *new_id)?;
    }
    Ok(())
}

/// One REFERENCED old-derivation logical row, snapshotted before a key-drift rebuild clears the
/// table (#493): the key fields the heal matches on, plus the id the durable references still
/// hold. `signature` is recovered from the surviving members when they UNANIMOUSLY carry the same
/// non-null capture ([`UNANIMOUS_MEMBER_SIGNATURE_SQL`]); `None` when no member survives (an
/// incremental pass already replaced the file's symbols) or the members disagree — signature
/// evidence then simply cannot corroborate, and qualified-name agreement must carry the match
/// alone.
#[derive(Debug)]
pub(super) struct LogicalKeyDriftRow {
    pub(super) old_id: i64,
    path: String,
    name: String,
    qualified_name: Option<String>,
    kind: String,
    pub(super) signature: Option<String>,
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
    kind: String,
    signature: Option<String>,
}

/// Where the drift heal parks a reference it had to move OFF an occupied id without an evidence
/// winner (#493): resolves to nothing by construction (`stable_id` is >= 0 and this is far below
/// the small negative `-(i+1)` temp range the two-phase remaps use, so no phase of any pass —
/// this heal's or a later one's — can ever capture a vacated reference). A vacated binding walks
/// the validate-time relocation ladder on the next pass, exactly like a vanished id.
const VACATED_LOGICAL_SYMBOL_ID: i64 = i64::MIN;

/// Whether a [`IndexDatabase::rebuild_logical_symbols`] pass may stamp
/// `repo_meta["logical_key_version"]` (#493 review). Only a whole-corpus pass (the full rebuild,
/// the fresh-index standalone pass) re-parses EVERY file, so only it proves the repo's symbols
/// were all derived under the current key semantics. A partial pass (incremental edit sweep,
/// single-file heal, worktree overlay refresh) re-derives a SUBSET: untouched files still carry
/// old-derivation symbols whose logical ids only churn when those files are eventually
/// re-parsed — stamping there would switch the drift heal off while most of the drift is still
/// in the future, stranding those references permanently. Partial passes still RUN the heal
/// (each pass realigns whatever drift is visible, and the heal is idempotent — exact-key
/// survivors are skipped); they just must not declare the repo healed.
pub(super) enum KeyVersionStamp {
    /// The pass re-derived every file of the repo — stamp the key version after the heal.
    FullRederive,
    /// A partial pass — heal what is visible, leave the stamp for a whole-corpus pass.
    Defer,
}

/// One replacement symbol's owed membership row (#820): produced when a file rewrite kept its
/// logical-key multiset, so the grouped table needs no rebuild — only the member pointers, which
/// died with the replaced symbol rows (`logical_symbol_members` cascades on `symbols` deletes).
/// Field-for-field what [`IndexDatabase::insert_logical_group`]'s member INSERT writes.
#[derive(Debug)]
pub(super) struct LogicalMemberRelink {
    logical_symbol_id: i64,
    symbol_id: i64,
    signature_hash: Option<String>,
    start_line: i64,
    end_line: i64,
}

/// Whether one write batch's logical-symbol tail can be served by a targeted member re-link
/// instead of the whole-repo rebuild (#820). A body-only edit re-inserts a file's symbols under
/// new row ids, but when the logical KEY multiset (language, path, name, qualified name, kind,
/// signature) is unchanged, the rebuilt `logical_symbols` table would be identical — only the
/// members' `symbol_id` pointers moved. Accumulated per batch while applying the per-file
/// incremental plan; ANY non-key-stable change downgrades the whole batch to today's
/// rebuild/marker behavior.
#[derive(Debug)]
pub(super) enum LogicalGroupingUpkeep {
    /// Every change so far replaced a file's symbols under an IDENTICAL logical-key multiset:
    /// the grouped table already matches what a rebuild would produce; only these member rows
    /// are owed. NEVER a clearer of the #819 pending marker — an outstanding obligation must
    /// survive to its settle point (`rebuild_logical_symbols` is the sole clearer).
    RelinkMembers(Vec<LogicalMemberRelink>),
    /// Some change altered a file's key set (an added/removed/tombstoned file, a rename, a
    /// signature or kind change, or a mutation outside the per-file plan) — the batch owes the
    /// full rebuild, exactly the pre-#820 behavior.
    RebuildRequired,
}

impl LogicalGroupingUpkeep {
    /// Whether the batch is still on the relink path — the guard for paying the per-file key
    /// capture at all.
    pub(super) fn is_relinkable(&self) -> bool {
        matches!(self, Self::RelinkMembers(_))
    }

    /// Downgrade the batch to the full rebuild. Relinks gathered so far are dropped — the
    /// rebuild re-derives every membership anyway.
    pub(super) fn require_rebuild(&mut self) {
        *self = Self::RebuildRequired;
    }

    /// Fold one replaced file's verdict in: `None` (the key multiset changed, or the old
    /// grouping was unavailable) downgrades the whole batch; owed relinks accumulate.
    pub(super) fn absorb_replaced_file(&mut self, relinks: Option<Vec<LogicalMemberRelink>>) {
        let Some(owed) = relinks else {
            self.require_rebuild();
            return;
        };
        if let Self::RelinkMembers(pending) = self {
            pending.extend(owed);
        }
    }
}

/// The six logical-key columns of one symbol row in the scope being rewritten — exactly the
/// identity [`IndexDatabase::rebuild_logical_symbols`] groups by. `Ord` so the multiset
/// comparison is a `BTreeMap` walk; `Option` fields compare `None`-first, and `None` never
/// equals `Some` (an absent qualified name or signature is no wildcard).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ReplacedSymbolKey {
    language: String,
    path: String,
    name: String,
    qualified_name: Option<String>,
    kind: String,
    signature: Option<String>,
}

/// One logical key's grouped claim in the scope row being replaced: the logical row the old
/// members point at, and how many of THIS file's symbols it held. Per-key member counts are part
/// of the stability bar — a count change would stale the logical row's `variant_count` /
/// `group_reason`, which only the rebuild recomputes.
pub(super) struct GroupedKeyClaim {
    logical_symbol_id: i64,
    members: usize,
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

#[derive(Debug, Clone)]
pub(super) struct LogicalSymbolMemberRow {
    pub(super) symbol_id: i64,
    /// Which `files` ROW this member came from. A path can have several: worktree-overlay and
    /// commit scopes each carry their own row for the same source file. That distinction is what
    /// separates "one symbol seen in N scopes" from "N symbols in one file" when labelling a
    /// group — see [`insert_logical_group`].
    pub(super) file_id: i64,
    pub(super) path: String,
    pub(super) language: String,
    pub(super) name: String,
    pub(super) qualified_name: String,
    pub(super) kind: String,
    pub(super) signature: Option<String>,
    pub(super) start_line: i64,
    pub(super) end_line: i64,
}

impl IndexDatabase {
    pub(super) fn resolve_edges(&self) -> anyhow::Result<()> {
        edges::resolve_all_edges(self.storage.connection())
    }

    /// Resolve edges for a LINKED-WORKTREE OVERLAY pass (#219 P1): re-resolve / re-synthesize ONLY
    /// the worktree's own overlay source files, never the SHARED committed (base) rows that are
    /// merely visible in the overlay scope view. Resolution targets still span the full overlay
    /// view, so an overlay edge into a base symbol resolves correctly. The plain `resolve_edges`
    /// (base/incremental/full-rebuild) owns its scope and rewrites everything in view.
    pub(super) fn resolve_overlay_edges(&self, worktree_id: &str) -> anyhow::Result<()> {
        edges::resolve_overlay_edges(self.storage.connection(), worktree_id)
    }

    /// The #493 heal input is the drift snapshot memoized by the pass's FIRST
    /// [`Self::remove_file_in_scope`] (see [`Self::capture_drift_snapshot_before_removal`]) —
    /// a partial pass (single-file heal, incremental sweep, overlay refresh) deletes an edited
    /// file's old symbols before this rebuild runs, and with them the snapshot's member-
    /// signature evidence; a snapshot taken only here would then have nothing to corroborate a
    /// qualified-name drift with, stranding the reference permanently (the old row is cleared
    /// below, so no later pass could recover it either). When the pass removed nothing (a
    /// HEAD-move carry, a package-roots refresh), the evidence is still intact and the snapshot
    /// is captured fresh here. A leftover memo for a DIFFERENT repo (a consolidated-DB context
    /// switch) is discarded, not consumed.
    pub(super) fn rebuild_logical_symbols(&self, stamp: KeyVersionStamp) -> anyhow::Result<()> {
        #[cfg(test)]
        self.logical_symbol_rebuilds.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let memoized = self.drift_snapshot.lock().expect("drift snapshot lock").take();
        let drift_snapshot = match memoized {
            Some((repo, snapshot)) if repo == self.active_repo_id => snapshot,
            _ => self.logical_key_drift_snapshot()?,
        };
        // The insert below re-derives the COMPLETE logical-symbol table for the ACTIVE REPO from
        // its current symbols, so clear that repo's rows entirely first. A member-join
        // "rebuild set" misses logical_symbols whose members were cascade-deleted with
        // their symbols (clear_full_rebuild_tables deletes files → symbols →
        // logical_symbol_members via FK, but logical_symbols has no such FK). Those orphans
        // would then collide with the deterministic stable id on re-insert. Scoped to
        // `active_repo_id` (A3): a wholesale clear would wipe a sibling repo's grouping in
        // a consolidated DB, and the content-derived stable ids can collide across repos —
        // so the DELETE and the re-derive SELECT both filter this repo. (The
        // A6 note "constrained to the active repo slice" is folded in here, since the wholesale
        // cross-repo rebuild became incorrect the moment `logical_symbols` gained `repo_id`.)
        let conn = self.storage.connection();
        conn.execute(
            "DELETE FROM main.logical_symbol_members
             WHERE logical_symbol_id IN (SELECT id FROM main.logical_symbols WHERE repo_id = ?1)",
            params![self.active_repo_id],
        )?;
        conn.execute("DELETE FROM main.logical_symbols WHERE repo_id = ?1", params![
            self.active_repo_id
        ])?;

        // STREAM the grouping instead of materializing every symbol. The previous version built a
        // `BTreeMap<LogicalSymbolKey, Vec<row>>` over ALL symbols — at kernel scale ~3.5M rows ×
        // six owned `String`s each (plus a cloned key per group). That structure, allocated in the
        // trailing rebuild phase AFTER the edge accumulator is freed, was the dominant full-rebuild
        // peak-RSS spike (~6 GB transient at the very end of a whole-kernel index). Ordering the
        // SELECT by the key's `Ord` (language, path, name, qualified_name, kind, signature — SQLite
        // ASC sorts NULL first, matching Rust `None < Some`) then the within-group member order
        // (start_byte, end_byte, which the old per-group Vec preserved) makes each group's rows
        // arrive contiguously, so we flush a group the moment its key changes and hold only the
        // current group's members (kilobytes). Byte-identical: same grouping, same
        // `logical_symbols` insert order (ids are content-derived via `stable_id`, not rowids), and
        // the same member order, verified against the golden index.
        // Read RAW `main.files` (ALL of the active repo's scopes), NOT the per-connection `files`
        // scope VIEW. logical_symbols is per-repo but scope-INDEPENDENT within a repo; building it
        // must not depend on whichever commit/worktree scope happens to be active. When this runs
        // in a worktree-overlay context (a scope view IS installed), an unqualified `files`
        // resolves to the scoped temp view, so the DELETE + repopulate would WIPE every
        // other scope's grouping (base + sibling worktrees) and restore only the active
        // scope's — persistently breaking `sym_<hex>`-handle graph nav for base symbols
        // (the #219 review finding). Filtering `main.files.repo_id` (A3) keeps every symbol
        // in every live scope OF THIS REPO while excluding a sibling repo's rows in a
        // consolidated DB; the content-derived `stable_id` collapses cross-scope duplicates
        // into one logical symbol with per-scope members, and downstream reads stay
        // scope-filtered via the `files` view.
        // Filter `main.files.generation` to the ACTIVE generation (A6): a full rebuild leaves the
        // superseded generation's file rows in place (swept lazily by gc), so a bare `repo_id`
        // predicate would fold BOTH the dead and the live generation's symbols into one grouping.
        // `self.active_generation` is the WRITE generation on the rebuild connection — which the
        // rebuild flips to LIVE and carries forward every live overlay onto BEFORE this runs, so
        // filtering it folds the base scope + every carried-forward overlay of the live generation
        // and nothing dead. On an incremental pass it is the live generation, unchanged behavior.
        let mut stmt = conn.prepare(
            "
            SELECT symbols.id, main.files.path, symbols.language, symbols.name,
                   qn.value, symbols.kind, symbols.signature, symbols.start_line,
                   symbols.end_line, symbols.file_id
            FROM main.symbols AS symbols
            JOIN main.files ON main.files.id = symbols.file_id
            LEFT JOIN main.name_strings qn ON qn.id = symbols.qualified_name_id
            WHERE main.files.repo_id = ?1 AND main.files.generation = ?2
            ORDER BY symbols.language, main.files.path, symbols.name, qn.value,
                     symbols.kind, symbols.signature, symbols.start_byte, symbols.end_byte
            ",
        )?;
        let mut rows = stmt.query(params![self.active_repo_id, self.active_generation])?;
        let mut current: Option<(LogicalSymbolKey, Vec<LogicalSymbolMemberRow>)> = None;
        while let Some(row) = rows.next()? {
            let member = LogicalSymbolMemberRow {
                symbol_id: row.get(0)?,
                path: row.get(1)?,
                language: row.get(2)?,
                name: row.get(3)?,
                qualified_name: row.get(4)?,
                kind: row.get(5)?,
                signature: row.get(6)?,
                start_line: row.get(7)?,
                end_line: row.get(8)?,
                file_id: row.get(9)?,
            };
            // Compare the member's key fields against the current group WITHOUT allocating a key
            // per row (only per group, on a boundary).
            let same_group = current.as_ref().is_some_and(|(key, _)| {
                key.language == member.language
                    && key.path == member.path
                    && key.name == member.name
                    && key.qualified_name == member.qualified_name
                    && key.kind == member.kind
                    && key.signature == member.signature
            });
            if same_group {
                current.as_mut().expect("same_group implies Some").1.push(member);
            } else {
                if let Some((key, members)) = current.take() {
                    Self::insert_logical_group(conn, &self.active_repo_id, &key, &members)?;
                }
                let key = LogicalSymbolKey::from(&member);
                current = Some((key, vec![member]));
            }
        }
        if let Some((key, members)) = current.take() {
            Self::insert_logical_group(conn, &self.active_repo_id, &key, &members)?;
        }
        if let Some(snapshot) = drift_snapshot {
            self.heal_logical_key_drift(&snapshot)?;
            if matches!(stamp, KeyVersionStamp::FullRederive) {
                self.set_repo_meta(LOGICAL_KEY_VERSION_KEY, LOGICAL_KEY_VERSION)?;
            }
        }
        // ANY successful rebuild satisfies a pending batch-deferred obligation (#819) — the batch
        // tail, an inline overlay refresh, a heal, an incremental or full pass — so clear the
        // marker here, in the same transaction as the rebuild it accounts for. Left set, the next
        // maintenance pass would pay a second wholesale rebuild for nothing (e.g. after an
        // interrupted deferred batch whose stale grouping a standalone `index --worktree` already
        // repaired inline). A no-op DELETE when no marker is set.
        rag_rat_db::meta::delete_repo_meta(
            conn,
            &self.active_repo_id,
            super::worktree_overlay::OVERLAY_LOGICAL_REBUILD_PENDING_META,
        )?;
        Ok(())
    }

    /// Open the per-batch logical-grouping verdict (#820). Starts on the relink path unless the
    /// repo's logical-key version stamp lags [`LOGICAL_KEY_VERSION`]: a lagging stamp schedules
    /// the #493 drift heal, which only [`Self::rebuild_logical_symbols`] performs — the relink
    /// shortcut must not defer it (and must not leave the memoized drift snapshot unconsumed),
    /// so a lagging repo keeps today's rebuild on every mutating pass. One `repo_meta` read.
    pub(super) fn begin_logical_grouping_upkeep(&self) -> anyhow::Result<LogicalGroupingUpkeep> {
        let key_version_current =
            self.repo_meta(LOGICAL_KEY_VERSION_KEY)?.as_deref() == Some(LOGICAL_KEY_VERSION);
        Ok(if key_version_current {
            LogicalGroupingUpkeep::RelinkMembers(Vec::new())
        } else {
            LogicalGroupingUpkeep::RebuildRequired
        })
    }

    /// The grouped logical-key claims of the scope row at `(path, commit_sha, worktree_id)` —
    /// captured BEFORE [`Self::remove_file_in_scope`] cascades the member rows away with the
    /// symbols. `None` when the grouping cannot vouch for the file: a symbol with NO member row
    /// (the scope row was committed by an interrupted #819 batch and never regrouped — its keys
    /// are not in the grouped table, so a relink would fabricate members against missing or
    /// stale logical rows) or two same-key symbols pointing at different logical rows. The
    /// caller then falls back to the rebuild, which is always correct.
    pub(super) fn load_grouped_key_claims(
        &self,
        path: &Path,
        commit_sha: &str,
        worktree_id: &str,
    ) -> anyhow::Result<Option<BTreeMap<ReplacedSymbolKey, GroupedKeyClaim>>> {
        let conn = self.storage.connection();
        // Same joins as the rebuild's grouping SELECT (raw `main.*`, repo + generation scoped),
        // narrowed to the one scope row being replaced.
        let mut stmt = conn.prepare_cached(
            "
            SELECT s.language, f.path, s.name, qn.value, s.kind, s.signature,
                   m.logical_symbol_id
            FROM main.symbols s
            JOIN main.files f ON f.id = s.file_id
            LEFT JOIN main.name_strings qn ON qn.id = s.qualified_name_id
            LEFT JOIN main.logical_symbol_members m ON m.symbol_id = s.id
            WHERE f.repo_id = ?1 AND f.path = ?2 AND f.commit_sha = ?3 AND f.worktree_id = ?4
              AND f.generation = ?5
            ",
        )?;
        let mut rows = stmt.query(params![
            self.active_repo_id,
            rag_rat_base::paths::path_string(path),
            commit_sha,
            worktree_id,
            self.active_generation,
        ])?;
        let mut claims: BTreeMap<ReplacedSymbolKey, GroupedKeyClaim> = BTreeMap::new();
        while let Some(row) = rows.next()? {
            let key = ReplacedSymbolKey {
                language: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                qualified_name: row.get(3)?,
                kind: row.get(4)?,
                signature: row.get(5)?,
            };
            let Some(logical_symbol_id) = row.get::<_, Option<i64>>(6)? else {
                return Ok(None); // ungrouped symbol — the grouping cannot vouch for this file
            };
            match claims.entry(key) {
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let claim = entry.get_mut();
                    if claim.logical_symbol_id != logical_symbol_id {
                        return Ok(None); // same key split across logical rows — inconsistent
                    }
                    claim.members += 1;
                },
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(GroupedKeyClaim { logical_symbol_id, members: 1 });
                },
            }
        }
        Ok(Some(claims))
    }

    /// The member rows owed to the JUST-INSERTED replacement symbols at the same scope, or
    /// `None` when the rewrite changed the file's logical-key multiset — any added, removed,
    /// renamed, re-kinded or re-signatured symbol, including a count change of one key's cfg
    /// variants (which would stale `variant_count`/`group_reason`). Exactness is the
    /// correctness bar: a false key-stable verdict would leave `logical_symbol_members` missing
    /// rows for live symbols and break graph navigation, so all six key columns are compared as
    /// an exact per-file multiset. Owed rows reuse the OLD members' `logical_symbol_id` (the
    /// deterministic `stable_id` a rebuild would re-derive for the same key) and carry the
    /// replacement symbols' fresh line spans — a body edit shifts lines even when keys hold.
    pub(super) fn derive_key_stable_relinks(
        &self,
        path: &Path,
        commit_sha: &str,
        worktree_id: &str,
        replaced: &BTreeMap<ReplacedSymbolKey, GroupedKeyClaim>,
    ) -> anyhow::Result<Option<Vec<LogicalMemberRelink>>> {
        struct ReplacementSymbolSpan {
            symbol_id: i64,
            start_line: i64,
            end_line: i64,
        }
        let conn = self.storage.connection();
        let mut stmt = conn.prepare_cached(
            "
            SELECT s.language, f.path, s.name, qn.value, s.kind, s.signature,
                   s.id, s.start_line, s.end_line
            FROM main.symbols s
            JOIN main.files f ON f.id = s.file_id
            LEFT JOIN main.name_strings qn ON qn.id = s.qualified_name_id
            WHERE f.repo_id = ?1 AND f.path = ?2 AND f.commit_sha = ?3 AND f.worktree_id = ?4
              AND f.generation = ?5
            ",
        )?;
        let mut rows = stmt.query(params![
            self.active_repo_id,
            rag_rat_base::paths::path_string(path),
            commit_sha,
            worktree_id,
            self.active_generation,
        ])?;
        let mut inserted: BTreeMap<ReplacedSymbolKey, Vec<ReplacementSymbolSpan>> = BTreeMap::new();
        while let Some(row) = rows.next()? {
            let key = ReplacedSymbolKey {
                language: row.get(0)?,
                path: row.get(1)?,
                name: row.get(2)?,
                qualified_name: row.get(3)?,
                kind: row.get(4)?,
                signature: row.get(5)?,
            };
            inserted.entry(key).or_default().push(ReplacementSymbolSpan {
                symbol_id: row.get(6)?,
                start_line: row.get(7)?,
                end_line: row.get(8)?,
            });
        }
        // Every inserted key must exist in the replaced set with the SAME member count, and the
        // key-set cardinalities must match — together that is exact multiset equality.
        if inserted.len() != replaced.len() {
            return Ok(None);
        }
        let mut relinks = Vec::new();
        for (key, members) in inserted {
            let Some(claim) = replaced.get(&key) else {
                return Ok(None);
            };
            if claim.members != members.len() {
                return Ok(None);
            }
            // Hash exactly as `insert_logical_group` does (untrimmed bytes), so the relinked
            // member row is byte-identical to what the rebuild would write.
            let signature_hash = key
                .signature
                .as_deref()
                .map(|signature| rag_rat_base::hash::hex_sha256(signature.as_bytes()));
            for member in members {
                relinks.push(LogicalMemberRelink {
                    logical_symbol_id: claim.logical_symbol_id,
                    symbol_id: member.symbol_id,
                    signature_hash: signature_hash.clone(),
                    start_line: member.start_line,
                    end_line: member.end_line,
                });
            }
        }
        Ok(Some(relinks))
    }

    /// Write the owed member rows of a key-stable batch (#820) — the exact shape
    /// [`Self::insert_logical_group`]'s member INSERT writes (`cfg_expr` NULL, sha256 signature
    /// hash) — pointing the surviving logical rows at the replacement symbol ids. Runs inside
    /// the caller's write transaction, alongside the symbol rewrite it repairs. Deliberately
    /// NOT a clearer of the #819 pending marker: a relink is not a rebuild, and an outstanding
    /// obligation must survive to its settle point.
    pub(super) fn apply_logical_member_relinks(
        &self,
        relinks: &[LogicalMemberRelink],
    ) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        for relink in relinks {
            conn.prepare_cached(
                "
                INSERT INTO logical_symbol_members(
                    logical_symbol_id, symbol_id, cfg_expr, signature_hash, start_line, end_line
                )
                VALUES (?1, ?2, NULL, ?3, ?4, ?5)
                ",
            )?
            .execute(params![
                relink.logical_symbol_id,
                relink.symbol_id,
                relink.signature_hash,
                relink.start_line,
                relink.end_line,
            ])?;
        }
        Ok(())
    }

    /// Memoize the #493 drift snapshot BEFORE symbol rows are deleted, once per pass:
    /// [`Self::remove_file_in_scope`] calls this first, so whichever file replacement happens
    /// first in a pass captures the evidence while it is still complete — and every later
    /// removal in the same pass sees the memo and pays nothing. Idle passes never remove a file
    /// and never reach this, so the stale-stamp snapshot scan is paid only by passes that
    /// actually mutate files (and once). Reads only `repo_meta` when the key version is
    /// current. A memo left by a different repo's context (consolidated DB) is replaced.
    pub(super) fn capture_drift_snapshot_before_removal(&self) -> anyhow::Result<()> {
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
    /// DURABLE references (memory bindings, call-path endpoints, oracle monikers) rather than the
    /// whole table: unreferenced ids need no healing, and the reference set is dozens of rows
    /// where the table is tens of thousands. The `logical_symbols` join keys the intersection to
    /// the ACTIVE repo, so a sibling repo's references never enter a heal that queries this
    /// repo's candidates.
    pub(super) fn logical_key_drift_snapshot(
        &self,
    ) -> anyhow::Result<Option<Vec<LogicalKeyDriftRow>>> {
        if self.repo_meta(LOGICAL_KEY_VERSION_KEY)?.as_deref() == Some(LOGICAL_KEY_VERSION) {
            return Ok(None);
        }
        let conn = self.storage.connection();
        let mut stmt = conn.prepare(&format!(
            "
            SELECT ls.id, ls.path, ls.logical_name,
                   (SELECT value FROM name_strings WHERE id = ls.qualified_name_id),
                   ls.kind,
                   {UNANIMOUS_MEMBER_SIGNATURE_SQL}
            FROM main.logical_symbols ls
            WHERE ls.repo_id = ?1
              AND ls.id IN (
                  SELECT logical_symbol_id FROM repo_memory_bindings
                   WHERE logical_symbol_id IS NOT NULL
                  UNION
                  SELECT start_logical_symbol_id FROM repo_memory_call_paths
                   WHERE start_logical_symbol_id IS NOT NULL
                  UNION
                  SELECT end_logical_symbol_id FROM repo_memory_call_paths
                   WHERE end_logical_symbol_id IS NOT NULL
                  UNION
                  SELECT logical_symbol_id FROM logical_symbol_monikers
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
                    kind: row.get(4)?,
                    signature: row.get(5)?,
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
    fn heal_logical_key_drift(&self, snapshot: &[LogicalKeyDriftRow]) -> anyhow::Result<usize> {
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
                               ls.kind,
                               {UNANIMOUS_MEMBER_SIGNATURE_SQL}
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
                            kind: row.get(3)?,
                            signature: row.get(4)?,
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
        for old in snapshot {
            let old_id = old.old_id;
            if remapped.contains(&old_id) || survivor_claims.contains(&old_id) {
                continue;
            }
            null_call_path_references(conn, old_id)?;
            if occupied_old_ids.contains(&old_id) {
                vacate_logical_symbol_references(conn, old_id)?;
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
        // Snapshot the current binding_ids before mutating, so the rename loop is not walking a
        // live cursor it is also writing to.
        let stale_binding_ids: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT memory_id, binding_id FROM repo_memory_bindings
                  WHERE binding_kind = 'logical_symbol' AND logical_symbol_id = ?1",
            )?;
            stmt.query_map(params![logical_symbol_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (memory_id, old_binding_id) in stale_binding_ids {
            let updated = conn.execute(
                "UPDATE OR IGNORE repo_memory_bindings
                    SET binding_id = ?3, symbol_kind = ?4, signature_hash = ?5
                  WHERE memory_id = ?1 AND binding_kind = 'logical_symbol' AND binding_id = ?2",
                params![memory_id, old_binding_id, qualified_name, kind, signature_hash],
            )?;
            // The rename was ignored because a sibling binding of this memory already holds the
            // target qualified name: drop the stale row instead of leaving it mis-labelled.
            if updated == 0 && old_binding_id != qualified_name {
                conn.execute(
                    "DELETE FROM repo_memory_bindings
                      WHERE memory_id = ?1 AND binding_kind = 'logical_symbol' AND binding_id = ?2",
                    params![memory_id, old_binding_id],
                )?;
            }
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

    pub(super) fn graph_coverage(
        &self,
        paths: BTreeSet<String>,
    ) -> anyhow::Result<rag_rat_query::graph::GraphCoverage> {
        let indexed_files =
            self.storage
                .connection()
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))?;
        let parser_failure_paths = self.parser_failure_paths()?;
        let parser_failures = u64::try_from(parser_failure_paths.len()).unwrap_or(0);
        let known_index_gaps = parser_failure_paths
            .iter()
            .map(|failure| {
                format!(
                    "{} parser failed for {}: {}",
                    failure.language, failure.path, failure.message
                )
            })
            .collect::<Vec<_>>();
        let mut stale_files = 0_u64;
        let mut parser_coverage_for_paths = Vec::new();
        for path in paths {
            let Some(row) = self.graph_path_row(&path)? else {
                parser_coverage_for_paths.push(rag_rat_query::graph::GraphPathCoverage {
                    path,
                    language: "unknown".to_string(),
                    parser_status: "missing_from_index".to_string(),
                    graph_status: "missing_from_index".to_string(),
                    last_indexed_revision: None,
                });
                continue;
            };
            let stale = self.source_path_is_stale(&path, &row.sha256);
            if stale {
                stale_files += 1;
            }
            let parser_failed = parser_failure_paths.iter().any(|failure| failure.path == path);
            parser_coverage_for_paths.push(rag_rat_query::graph::GraphPathCoverage {
                path,
                language: row.language,
                parser_status: if parser_failed { "failed" } else { "ok" }.to_string(),
                graph_status: if stale {
                    "stale_source"
                } else if parser_failed {
                    "parser_failed"
                } else {
                    "ok"
                }
                .to_string(),
                last_indexed_revision: (!row.indexed_revision.is_empty())
                    .then_some(row.indexed_revision),
            });
        }
        Ok(rag_rat_query::graph::GraphCoverage {
            indexed_files: u64::try_from(indexed_files).unwrap_or(0),
            parser_failures,
            stale_files,
            known_index_gaps,
            parser_coverage_for_paths,
        })
    }

    fn graph_path_row(&self, path: &str) -> anyhow::Result<Option<GraphPathRow>> {
        self.storage
            .connection()
            .query_row(
                "SELECT language, sha256, indexed_revision FROM files WHERE path = ?1",
                [path],
                |row| {
                    Ok(GraphPathRow {
                        language: row.get(0)?,
                        sha256: row.get(1)?,
                        indexed_revision: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub(super) fn ensure_graph_index_current(&self) -> anyhow::Result<()> {
        if self.repo_meta("graph_index_version")?.as_deref() == Some(GRAPH_INDEX_VERSION) {
            return Ok(());
        }
        let Some(root) = self.storage.source_root().map(Path::to_path_buf) else {
            return Ok(());
        };
        self.storage.execute_batch("BEGIN IMMEDIATE TRANSACTION")?;
        let result = (|| -> anyhow::Result<()> {
            // Scope the wipe to the ACTIVE REPO's edges (A3): `graph_index_version` is per-repo, so
            // a stale/missing version for repo A must not wipe repo B's edges in a
            // consolidated DB. An edge belongs to its `source_file_id`'s repo;
            // `source_file_id` is always set (its FK is `ON DELETE CASCADE`, every edge
            // is inserted with a file id), so this removes exactly this repo's edges —
            // the same set the repopulate below (over the repo-scoped `files`
            // view) re-derives. Within the repo this matches the prior wholesale behavior; only
            // sibling repos are now spared.
            // The `generation` predicate (A6) makes the sentence above literally true: the wipe
            // removes exactly the set the repopulate re-derives (the ACTIVE generation's edges).
            // Without it, a heal would also wipe a superseded generation's edges (merely early gc)
            // — and, in the rare concurrent case of a version-bump heal racing another
            // connection's staged rebuild, the STAGED generation's edges, which nothing would
            // re-resolve.
            self.storage.connection().execute(
                "DELETE FROM edges_data
                  WHERE source_file_id IN (
                      SELECT id FROM main.files WHERE repo_id = ?1 AND generation = ?2
                  )",
                params![self.active_repo_id, self.active_generation],
            )?;
            // Repopulate the per-package import scope BEFORE re-resolving (#61). A bare
            // version-bump re-resolve would re-derive `import_scope_*` on the new edges
            // but read an empty `packages` table (V022 only ADDED the column; it did not
            // backfill `packages`), so every file would fall open to the global union and
            // the new per-package behavior would never engage on a migrated index.
            // `refresh_packages` writes the active scope's `packages` rows + the global
            // `local_crate_roots` union; `resolve_edges` below then computes each file's
            // package at load time (`load_package_roots_into_scope`) from those rows.
            self.refresh_packages(&root)?;
            let files = self.graph_reindex_files()?;
            for file in files {
                if file.kind == TargetKind::Generated || file.language == Language::Markdown {
                    continue;
                }
                let full_path = root.join(&file.path);
                let Ok(text) = fs::read_to_string(full_path) else {
                    continue;
                };
                if text.len() > edges::MAX_GRAPH_PARSE_BYTES {
                    continue;
                }
                edges::index_file_edges(
                    self.storage.connection(),
                    file.id,
                    Path::new(&file.path),
                    file.language,
                    &text,
                )?;
            }
            self.resolve_edges()?;
            self.mark_graph_index_current()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.storage.execute_batch("ROLLBACK");
        }
        result?;
        self.storage.execute_batch("COMMIT")?;
        Ok(())
    }

    pub(super) fn mark_graph_index_current(&self) -> anyhow::Result<()> {
        self.set_repo_meta("graph_index_version", GRAPH_INDEX_VERSION)
    }

    fn graph_reindex_files(&self) -> anyhow::Result<Vec<GraphReindexFile>> {
        let mut stmt = self
            .storage
            .connection()
            .prepare("SELECT id, path, language, kind FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            let language: String = row.get(2)?;
            let kind: String = row.get(3)?;
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, language, kind))
        })?;
        let mut files = Vec::new();
        for row in rows {
            let (id, path, language, kind) = row?;
            files.push(GraphReindexFile {
                id,
                path,
                language: language.parse()?,
                kind: kind.parse()?,
            });
        }
        Ok(files)
    }
}
