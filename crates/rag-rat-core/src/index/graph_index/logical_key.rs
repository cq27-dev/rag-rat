//! Logical-symbol identity and grouping: the key that collapses cfg variants into one
//! logical symbol, and the full / scoped / key-stable ways the grouping is kept current.

use std::collections::BTreeMap;

use rag_rat_base::checkout::CheckoutRef;

use crate::index::*;

/// Grouping key that collapses cfg variants / overloads of one symbol into a single logical symbol.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::index) struct LogicalSymbolKey {
    pub(in crate::index) language: String,
    pub(in crate::index) path: String,
    pub(in crate::index) name: String,
    pub(in crate::index) qualified_name: String,
    pub(in crate::index) scope_path: String,
    pub(in crate::index) kind: String,
    // Signature is part of the identity so that two distinct same-named symbols in one file (e.g.
    // `new` on two different impls — same `qualified_name`, different signatures) do NOT collapse
    // into one logical symbol. Genuine cfg variants share a signature, so they still group.
    pub(in crate::index) signature: Option<String>,
}

impl From<&LogicalSymbolMemberRow> for LogicalSymbolKey {
    fn from(row: &LogicalSymbolMemberRow) -> Self {
        Self {
            language: row.language.clone(),
            path: row.path.clone(),
            name: row.name.clone(),
            qualified_name: row.qualified_name.clone(),
            scope_path: row.scope_path.clone(),
            kind: row.kind.clone(),
            signature: row.signature.clone(),
        }
    }
}

impl LogicalSymbolKey {
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
    pub(in crate::index) fn stable_id(&self, repo_id: &str) -> i64 {
        let canonical = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            repo_id,
            self.language,
            self.path,
            self.name,
            self.qualified_name,
            self.scope_path,
            self.kind,
            self.signature.as_deref().unwrap_or(""),
        );
        let digest = Sha256::digest(canonical.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        (u64::from_be_bytes(bytes) >> 1) as i64
    }
}

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
pub(in crate::index) enum KeyVersionStamp {
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
pub(in crate::index) struct LogicalMemberRelink {
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
pub(in crate::index) enum LogicalGroupingUpkeep {
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
    pub(in crate::index) fn is_relinkable(&self) -> bool {
        matches!(self, Self::RelinkMembers(_))
    }

    /// Downgrade the batch to the full rebuild. Relinks gathered so far are dropped — the
    /// rebuild re-derives every membership anyway.
    pub(in crate::index) fn require_rebuild(&mut self) {
        *self = Self::RebuildRequired;
    }

    /// Fold one replaced file's verdict in: `None` (the key multiset changed, or the old
    /// grouping was unavailable) downgrades the whole batch; owed relinks accumulate.
    pub(in crate::index) fn absorb_replaced_file(
        &mut self,
        relinks: Option<Vec<LogicalMemberRelink>>,
    ) {
        let Some(owed) = relinks else {
            self.require_rebuild();
            return;
        };
        if let Self::RelinkMembers(pending) = self {
            pending.extend(owed);
        }
    }
}

/// The seven logical-key columns of one symbol row in the scope being rewritten — exactly the
/// identity [`IndexDatabase::rebuild_logical_symbols`] groups by. `Ord` so the multiset
/// comparison is a `BTreeMap` walk; `Option` fields compare `None`-first, and `None` never
/// equals `Some` (an absent qualified name or signature is no wildcard).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::index) struct ReplacedSymbolKey {
    language: String,
    path: String,
    name: String,
    qualified_name: Option<String>,
    scope_path: Option<String>,
    kind: String,
    signature: Option<String>,
}

/// One logical key's grouped claim in the scope row being replaced: the logical row the old
/// members point at, and how many of THIS file's symbols it held. Per-key member counts are part
/// of the stability bar — a count change would stale the logical row's `variant_count` /
/// `group_reason`, which only the rebuild recomputes.
pub(in crate::index) struct GroupedKeyClaim {
    logical_symbol_id: i64,
    members: usize,
}

#[derive(Debug, Clone)]
pub(in crate::index) struct LogicalSymbolMemberRow {
    pub(in crate::index) symbol_id: i64,
    /// Which `files` ROW this member came from. A path can have several: worktree-overlay and
    /// commit scopes each carry their own row for the same source file. That distinction is what
    /// separates "one symbol seen in N scopes" from "N symbols in one file" when labelling a
    /// group — see [`insert_logical_group`].
    pub(in crate::index) file_id: i64,
    pub(in crate::index) path: String,
    pub(in crate::index) language: String,
    pub(in crate::index) name: String,
    pub(in crate::index) qualified_name: String,
    pub(in crate::index) scope_path: String,
    pub(in crate::index) kind: String,
    pub(in crate::index) signature: Option<String>,
    pub(in crate::index) start_line: i64,
    pub(in crate::index) end_line: i64,
}

impl IndexDatabase {
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
    pub(in crate::index) fn rebuild_logical_symbols(
        &self,
        stamp: KeyVersionStamp,
    ) -> anyhow::Result<()> {
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

        // Re-derive the WHOLE repo's grouping from its current symbols via the shared streaming
        // grouper (no path filter). The DELETE above cleared the prior rows, so re-insert cannot
        // collide on the content-derived `stable_id` PK.
        self.regroup_logical_symbols(conn, "")?;
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
            crate::index::worktree_overlay::OVERLAY_LOGICAL_REBUILD_PENDING_META,
        )?;
        Ok(())
    }

    /// Stream the active repo's live-generation symbols — optionally narrowed by `extra_where`, an
    /// `AND`-able predicate over `main.files`/`symbols` (a hardcoded literal, never user input) —
    /// grouped by the logical key, inserting each group via [`Self::insert_logical_group`]. Shared
    /// by the whole-repo [`Self::rebuild_logical_symbols`] (`extra_where = ""`) and the
    /// path-scoped [`Self::rederive_changed_logical_symbols`] (#826) so both derive
    /// BYTE-IDENTICAL groupings for the paths they cover — same key order, same member order,
    /// same content-derived ids.
    ///
    /// STREAM the grouping instead of materializing every symbol. The previous version built a
    /// `BTreeMap<LogicalSymbolKey, Vec<row>>` over ALL symbols — at kernel scale ~3.5M rows × six
    /// owned `String`s each (plus a cloned key per group). That structure, allocated in the
    /// trailing rebuild phase AFTER the edge accumulator is freed, was the dominant
    /// full-rebuild peak-RSS spike (~6 GB transient at the very end of a whole-kernel index).
    /// Ordering the SELECT by the key's `Ord` (language, path, name, qualified_name, kind,
    /// signature — SQLite ASC sorts NULL first, matching Rust `None < Some`) then the
    /// within-group member order (start_byte, end_byte, which the old per-group Vec preserved)
    /// makes each group's rows arrive contiguously, so we flush a group the moment its key
    /// changes and hold only the current group's members (kilobytes). Byte-identical: same
    /// grouping, same `logical_symbols` insert order (ids are content-derived via `stable_id`,
    /// not rowids), and the same member order.
    ///
    /// Read RAW `main.files` (ALL of the active repo's scopes), NOT the per-connection `files`
    /// scope VIEW. logical_symbols is per-repo but scope-INDEPENDENT within a repo; building it
    /// must not depend on whichever commit/worktree scope happens to be active. When this runs
    /// in a worktree-overlay context (a scope view IS installed), an unqualified `files`
    /// resolves to the scoped temp view, so the DELETE + repopulate would WIPE every other
    /// scope's grouping (base + sibling worktrees) and restore only the active scope's —
    /// persistently breaking `sym_<hex>`-handle graph nav for base symbols (the #219 review
    /// finding). Filtering `main.files.repo_id` (A3) keeps every symbol in every live scope OF
    /// THIS REPO while excluding a sibling repo's rows in a consolidated DB; the
    /// content-derived `stable_id` collapses cross-scope duplicates into one logical symbol
    /// with per-scope members, and downstream reads stay scope-filtered via the `files` view.
    /// This is also why the #826 path-scoped re-derive can run under an OVERLAY scope view and
    /// still regroup a changed path across ALL its scopes.
    ///
    /// Filter `main.files.generation` to the ACTIVE generation (A6): a full rebuild leaves the
    /// superseded generation's file rows in place (swept lazily by gc), so a bare `repo_id`
    /// predicate would fold BOTH the dead and the live generation's symbols into one grouping.
    /// `self.active_generation` is the WRITE generation on the rebuild connection — which the
    /// rebuild flips to LIVE and carries forward every live overlay onto BEFORE this runs, so
    /// filtering it folds the base scope + every carried-forward overlay of the live generation
    /// and nothing dead. On an incremental pass it is the live generation, unchanged behavior.
    fn regroup_logical_symbols(
        &self,
        conn: &rusqlite::Connection,
        extra_where: &str,
    ) -> anyhow::Result<()> {
        let mut stmt = conn.prepare(&format!(
            "
            SELECT symbols.id, main.files.path, symbols.language, symbols.name,
                   qn.value, COALESCE(symbols.scope_path, ''), symbols.kind, symbols.signature,
                   symbols.start_line, symbols.end_line, symbols.file_id
            FROM main.symbols AS symbols
            JOIN main.files ON main.files.id = symbols.file_id
            LEFT JOIN main.name_strings qn ON qn.id = symbols.qualified_name_id
            WHERE main.files.repo_id = ?1 AND main.files.generation = ?2{extra_where}
            ORDER BY symbols.language, main.files.path, symbols.name, qn.value,
                     COALESCE(symbols.scope_path, ''), symbols.kind, symbols.signature,
                     symbols.start_byte, symbols.end_byte
            "
        ))?;
        let mut rows = stmt.query(params![self.active_repo_id, self.active_generation])?;
        // SQL keeps `(language, path)` contiguous. Normalize and sort ONE file partition at a
        // time: `path` is part of LogicalSymbolKey, so no group can cross this boundary. This
        // preserves the exact normalized-key ordering without retaining the whole corpus's symbol
        // strings in Rust memory.
        let mut members_sorted = Vec::new();
        while let Some(row) = rows.next()? {
            let language: String = row.get(2)?;
            let scope_path: String = row.get(5)?;
            let member = LogicalSymbolMemberRow {
                symbol_id: row.get(0)?,
                path: row.get(1)?,
                language,
                name: row.get(3)?,
                qualified_name: row.get(4)?,
                scope_path,
                kind: row.get(6)?,
                signature: row.get(7)?,
                start_line: row.get(8)?,
                end_line: row.get(9)?,
                file_id: row.get(10)?,
            };
            if members_sorted.last().is_some_and(|previous: &LogicalSymbolMemberRow| {
                previous.language != member.language || previous.path != member.path
            }) {
                Self::insert_logical_partition(conn, &self.active_repo_id, &mut members_sorted)?;
            }
            members_sorted.push(member);
        }
        Self::insert_logical_partition(conn, &self.active_repo_id, &mut members_sorted)
    }

    /// Sort and insert one `(language, path)` partition using the exact normalized logical key.
    fn insert_logical_partition(
        conn: &rusqlite::Connection,
        repo_id: &str,
        members_sorted: &mut Vec<LogicalSymbolMemberRow>,
    ) -> anyhow::Result<()> {
        // SQL orders the RAW scope, while grouping compares the NORMALIZED one. Re-sort this file
        // partition so equal-normalized rows are adjacent; spans/id are deterministic member order.
        members_sorted.sort_by(|a, b| {
            (&a.language, &a.path, &a.name, &a.qualified_name, &a.scope_path, &a.kind)
                .cmp(&(&b.language, &b.path, &b.name, &b.qualified_name, &b.scope_path, &b.kind))
                .then_with(|| a.signature.cmp(&b.signature))
                .then_with(|| {
                    (a.start_line, a.end_line, a.symbol_id).cmp(&(
                        b.start_line,
                        b.end_line,
                        b.symbol_id,
                    ))
                })
        });
        let mut current: Option<(LogicalSymbolKey, Vec<LogicalSymbolMemberRow>)> = None;
        for member in members_sorted.drain(..) {
            // Compare the member's key fields against the current group WITHOUT allocating a key
            // per row (only per group, on a boundary).
            let same_group = current.as_ref().is_some_and(|(key, _)| {
                key.language == member.language
                    && key.path == member.path
                    && key.name == member.name
                    && key.qualified_name == member.qualified_name
                    && key.scope_path == member.scope_path
                    && key.kind == member.kind
                    && key.signature == member.signature
            });
            if same_group {
                current.as_mut().expect("same_group implies Some").1.push(member);
            } else {
                if let Some((key, members)) = current.take() {
                    Self::insert_logical_group(conn, repo_id, &key, &members)?;
                }
                let key = LogicalSymbolKey::from(&member);
                current = Some((key, vec![member]));
            }
        }
        if let Some((key, members)) = current.take() {
            Self::insert_logical_group(conn, repo_id, &key, &members)?;
        }
        Ok(())
    }

    /// #826: re-derive ONLY the `logical_symbols` of the paths staged in `temp.logical_rederive_paths`
    /// (the files this pass rewrote / removed / healed), instead of the whole repo. A logical
    /// symbol is confined to ONE path (`path` is a key field, folded into `stable_id`), so a
    /// changed file only affects its own path's groups; every other path's rows are left
    /// byte-identical. The DELETE (members first, then their parents — mirroring
    /// [`Self::rebuild_logical_symbols`] rather than leaning on the cascade) clears the changed
    /// paths' rows, incl. any orphaned by the pass's symbol removals; the regroup re-derives
    /// them from current symbols across ALL of each path's scopes (raw `main.files`). NO #493
    /// drift heal and NO #819 marker clear — the caller gates on
    /// [`Self::can_scope_logical_rederive`], which is false whenever either is owed.
    pub(in crate::index) fn rederive_changed_logical_symbols(&self) -> anyhow::Result<()> {
        let conn = self.storage.connection();
        conn.execute(
            "DELETE FROM main.logical_symbol_members
             WHERE logical_symbol_id IN (
                 SELECT id FROM main.logical_symbols
                 WHERE repo_id = ?1 AND path IN (SELECT path FROM temp.logical_rederive_paths)
             )",
            params![self.active_repo_id],
        )?;
        conn.execute(
            "DELETE FROM main.logical_symbols
             WHERE repo_id = ?1 AND path IN (SELECT path FROM temp.logical_rederive_paths)",
            params![self.active_repo_id],
        )?;
        self.regroup_logical_symbols(
            conn,
            " AND main.files.path IN (SELECT path FROM temp.logical_rederive_paths)",
        )
    }

    /// #826: whether the pass may replace the whole-repo [`Self::rebuild_logical_symbols`] with the
    /// path-scoped [`Self::rederive_changed_logical_symbols`]. False when a #493 drift heal is owed
    /// (the logical-key version stamp lags — the heal is a cross-file reference remap the scoped
    /// path does NOT perform) or a #819 deferred whole-repo rebuild is pending (a scoped
    /// re-derive would not satisfy it, and `rebuild_logical_symbols` is the sole clearer of
    /// that marker). Two `repo_meta` reads.
    pub(in crate::index) fn can_scope_logical_rederive(&self) -> anyhow::Result<bool> {
        let version_current =
            self.repo_meta(LOGICAL_KEY_VERSION_KEY)?.as_deref() == Some(LOGICAL_KEY_VERSION);
        let rebuild_pending = self
            .repo_meta(crate::index::worktree_overlay::OVERLAY_LOGICAL_REBUILD_PENDING_META)?
            .is_some();
        Ok(version_current && !rebuild_pending)
    }

    /// Open the per-batch logical-grouping verdict (#820). Starts on the relink path unless the
    /// repo's logical-key version stamp lags [`LOGICAL_KEY_VERSION`]: a lagging stamp schedules
    /// the #493 drift heal, which only [`Self::rebuild_logical_symbols`] performs — the relink
    /// shortcut must not defer it (and must not leave the memoized drift snapshot unconsumed),
    /// so a lagging repo keeps today's rebuild on every mutating pass. One `repo_meta` read.
    pub(in crate::index) fn begin_logical_grouping_upkeep(
        &self,
    ) -> anyhow::Result<LogicalGroupingUpkeep> {
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
    pub(in crate::index) fn load_grouped_key_claims(
        &self,
        path: &Path,
        checkout: CheckoutRef<'_>,
    ) -> anyhow::Result<Option<BTreeMap<ReplacedSymbolKey, GroupedKeyClaim>>> {
        let CheckoutRef { commit_sha, worktree_id } = checkout;
        let conn = self.storage.connection();
        // Same joins as the rebuild's grouping SELECT (raw `main.*`, repo + generation scoped),
        // narrowed to the one scope row being replaced.
        let mut stmt = conn.prepare_cached(
            "
            SELECT s.language, f.path, s.name, qn.value, COALESCE(s.scope_path, ''), s.kind,
                   s.signature, m.logical_symbol_id
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
            let language: String = row.get(0)?;
            let key = ReplacedSymbolKey {
                path: row.get(1)?,
                name: row.get(2)?,
                qualified_name: row.get(3)?,
                scope_path: row.get::<_, Option<String>>(4)?,
                language,
                kind: row.get(5)?,
                signature: row.get(6)?,
            };
            let Some(logical_symbol_id) = row.get::<_, Option<i64>>(7)? else {
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
    pub(in crate::index) fn derive_key_stable_relinks(
        &self,
        path: &Path,
        checkout: CheckoutRef<'_>,
        replaced: &BTreeMap<ReplacedSymbolKey, GroupedKeyClaim>,
    ) -> anyhow::Result<Option<Vec<LogicalMemberRelink>>> {
        let CheckoutRef { commit_sha, worktree_id } = checkout;
        struct ReplacementSymbolSpan {
            symbol_id: i64,
            start_line: i64,
            end_line: i64,
        }
        let conn = self.storage.connection();
        let mut stmt = conn.prepare_cached(
            "
            SELECT s.language, f.path, s.name, qn.value, COALESCE(s.scope_path, ''), s.kind,
                   s.signature, s.id, s.start_line, s.end_line
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
            let language: String = row.get(0)?;
            let key = ReplacedSymbolKey {
                path: row.get(1)?,
                name: row.get(2)?,
                qualified_name: row.get(3)?,
                scope_path: row.get::<_, Option<String>>(4)?,
                language,
                kind: row.get(5)?,
                signature: row.get(6)?,
            };
            inserted.entry(key).or_default().push(ReplacementSymbolSpan {
                symbol_id: row.get(7)?,
                start_line: row.get(8)?,
                end_line: row.get(9)?,
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
    pub(in crate::index) fn apply_logical_member_relinks(
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
}
