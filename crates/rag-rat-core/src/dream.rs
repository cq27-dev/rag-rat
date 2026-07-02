//! Dream Mode (#122) — deterministic memory-maintenance worklist.
//!
//! Surfaces findings ABOUT memories into `dream_findings`; it NEVER mutates a `repo_memories` row
//! (dream mode proposes, a human/strong-agent confirms). Two v1 finding kinds, both deterministic
//! and rate-independent (the spike's commit-replay evals showed change-based signals saturate on an
//! active repo; these check structure/truth, not churn):
//!   - `coverage_gap`   — load-bearing symbols (high call-graph in-degree) with no memory binding.
//!   - `stale_reference`— a memory references a `.rs` path that no longer resolves against the
//!     index (suffix-aware, so prose shorthand like `src/lib.rs` still resolves).
//!
//! Findings are identity-keyed `(kind, subject, claim_hash)`: a re-run with the same evidence
//! REFRESHES (no duplicate); a materially-changed finding SUPERSEDES the prior one; a finding the
//! run no longer reports is RESOLVED. Lifecycle verified in the spike prototype before this port.

use std::collections::HashSet;

use regex::Regex;
use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize)]
pub struct DreamFinding {
    pub kind: String,
    pub subject: String,
    pub evidence: String,
    pub rank: f64,
}

#[derive(Debug, Default, Serialize)]
pub struct DreamReport {
    pub findings: Vec<DreamFinding>, // the open worklist, ranked
    pub opened: usize,
    pub refreshed: usize,
    pub superseded: usize,
    pub resolved: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct DreamOptions {
    pub now_ms: i64,
    pub limit: usize, // max coverage_gap findings
}

/// Half-life for worklist rank decay: an unreviewed finding loses half its rank every 2 weeks, so a
/// stale item sinks below fresh ones instead of squatting the top forever (anti-rot).
const RANK_HALF_LIFE_MS: f64 = 14.0 * 86_400_000.0;

/// `base_rank` decayed by age since the finding was first surfaced (`first_seen_at_ms`). This is
/// the effective ordering rank the worklist exposes — the schema's documented age-decay, made real.
fn effective_rank(base_rank: f64, first_seen_at_ms: i64, now_ms: i64) -> f64 {
    let age = (now_ms - first_seen_at_ms).max(0) as f64;
    base_rank * 0.5_f64.powf(age / RANK_HALF_LIFE_MS)
}

fn claim_hash(kind: &str, subject: &str, evidence: &str) -> String {
    let mut h = Sha256::new();
    h.update(kind.as_bytes());
    h.update([0x1f]);
    h.update(subject.as_bytes());
    h.update([0x1f]);
    h.update(evidence.as_bytes());
    hex16(&h.finalize())
}

/// Derive a finding id with the owning repo FOLDED into the hash (the `logical_symbols.stable_id`
/// precedent, A3). The id column stays a GLOBAL `TEXT PRIMARY KEY` while the lifecycle lookups are
/// repo-scoped — without the fold, two repos producing the same `(kind, subject, claim_hash)`
/// derive the SAME id, and the second repo's scoped lookup (which no longer sees the sibling's row)
/// takes the INSERT path straight into a PK violation. Folding keeps ids globally unique and
/// coordination-free with no PK shape change.
pub(crate) fn repo_folded_finding_id(repo_id: &str, kind: &str, subject: &str, ch: &str) -> String {
    let mut h = Sha256::new();
    h.update(repo_id.as_bytes());
    h.update([0x1f]);
    h.update(kind.as_bytes());
    h.update([0x1f]);
    h.update(subject.as_bytes());
    h.update([0x1f]);
    h.update(ch.as_bytes());
    hex16(&h.finalize())
}

/// The runtime id derivation: repo-folded on the post-A5 schema (`scope` is `Some`), the original
/// repo-blind hash pre-A5 (no `repo_id` column — single-repo by construction, ids stay compatible).
fn finding_id(scope: &Option<String>, kind: &str, subject: &str, ch: &str) -> String {
    if let Some(repo_id) = scope {
        return repo_folded_finding_id(repo_id, kind, subject, ch);
    }
    let mut h = Sha256::new();
    h.update(kind.as_bytes());
    h.update([0x1f]);
    h.update(subject.as_bytes());
    h.update([0x1f]);
    h.update(ch.as_bytes());
    hex16(&h.finalize())
}

/// Re-derive every persisted finding id under its row's CURRENT `repo_id` (and remap the in-table
/// `superseded_by` references — the only place a finding id is persisted outside the `id` column
/// itself). Called from the V042 migration (after the periphery backfill) and from `register_repo`
/// adoption (after the periphery re-point), mirroring `realign_logical_symbol_ids`: whenever a
/// row's `repo_id` changes, the id it SHOULD have changes with it. Idempotent (an already-derived
/// id re-derives to itself) and cheap (the worklist is small). Callers guard on the `repo_id`
/// column existing.
pub(crate) fn rederive_finding_ids(conn: &Connection) -> rusqlite::Result<()> {
    let rows: Vec<(String, String, String, String, String)> = conn
        .prepare("SELECT id, kind, subject, claim_hash, repo_id FROM dream_findings")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut remap = Vec::new();
    for (old_id, kind, subject, ch, repo_id) in rows {
        let new_id = repo_folded_finding_id(&repo_id, &kind, &subject, &ch);
        if new_id != old_id {
            remap.push((old_id, new_id));
        }
    }
    for (old_id, new_id) in &remap {
        conn.execute("UPDATE dream_findings SET id = ?2 WHERE id = ?1", [old_id, new_id])?;
    }
    for (old_id, new_id) in &remap {
        conn.execute("UPDATE dream_findings SET superseded_by = ?2 WHERE superseded_by = ?1", [
            old_id, new_id,
        ])?;
    }
    Ok(())
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// `coverage_gap`: top call-graph in-degree symbols with no memory binding (load-bearing code with
/// no institutional memory). Importance = DISTINCT caller count (`edges_data` carries several rows
/// per caller pair, so `COUNT(*)` would over-count). COVERAGE mirrors the canonical memory query
/// (query/memory/api.rs): a callee is covered if a binding matches its `symbol_id`, its logical
/// symbol (mapped through `logical_symbol_members` — a logical_symbol binding covers ALL its member
/// variants, not just the one stored `symbol_id`), or its file path. Test infra is filtered by
/// `has_test_code`, NOT an unanchored `path LIKE '%test%'` (which would drop real files like
/// `attestation.rs`/`latest.rs`). All filters run IN SQL *before* the LIMIT so the budget is spent
/// on ELIGIBLE rows. The `files` join resolves to the active-checkout `temp.files` view so callees
/// are checkout-scoped; the caller in-degree is not yet scoped (follow-up: reuse the scoped
/// `important_symbols` PageRank instead of this raw in-degree proxy).
fn coverage_gap(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<DreamFinding>> {
    // The COVERAGE exclusion subqueries read `repo_memory_bindings` and must be scoped to the
    // ACTIVE repo (V042): a SIBLING repo's binding — a colliding `symbol_id` rowid, or a path
    // binding at a path this repo also has (the same-path case) — would otherwise mark this repo's
    // load-bearing symbol as covered and SUPPRESS its coverage-gap finding (a false negative).
    // `{binding_clause}`/`{b_clause}` empty pre-A5.
    let scope = crate::index::schema::periphery_repo_scope(conn, "repo_memories")?;
    let binding_clause =
        crate::index::schema::periphery_repo_scope_clause(&scope, "repo_memory_bindings");
    let b_clause = crate::index::schema::periphery_repo_scope_clause(&scope, "b");
    let mut stmt = conn.prepare(&format!(
        "SELECT s.name, f.path, COUNT(DISTINCT e.from_symbol_id) AS d FROM edges_data e JOIN \
         symbols s ON s.id = e.to_symbol_id JOIN files f ON f.id = s.file_id WHERE e.to_symbol_id \
         IS NOT NULL AND e.from_symbol_id IS NOT NULL AND f.has_test_code = 0 AND e.to_symbol_id \
         NOT IN (SELECT symbol_id FROM repo_memory_bindings WHERE symbol_id IS NOT \
         NULL{binding_clause}) AND e.to_symbol_id NOT IN (SELECT lsm.symbol_id FROM \
         logical_symbol_members lsm JOIN repo_memory_bindings b ON b.logical_symbol_id = \
         lsm.logical_symbol_id WHERE b.logical_symbol_id IS NOT NULL{b_clause}) AND f.path NOT IN \
         (SELECT path FROM repo_memory_bindings WHERE path IS NOT NULL{binding_clause}) GROUP BY \
         e.to_symbol_id ORDER BY d DESC LIMIT ?1"
    ))?;
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let top_d = rows.first().map(|(_, _, d)| *d).unwrap_or(1).max(1) as f64;
    Ok(rows
        .into_iter()
        .map(|(name, path, d)| DreamFinding {
            kind: "coverage_gap".into(),
            subject: format!("{path}::{name}"),
            evidence: format!("{d} distinct callers, no memory binding [E0 importance proxy]"),
            rank: d as f64 / top_d,
        })
        .collect())
}

/// `stale_reference`: a memory body references a `.rs` path that no longer resolves against the
/// index. Suffix-aware so prose shorthand (`src/lib.rs` for `crates/x/src/lib.rs`) is NOT flagged.
fn stale_reference(conn: &Connection) -> rusqlite::Result<Vec<DreamFinding>> {
    let all_paths: HashSet<String> = conn
        .prepare("SELECT path FROM files")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let resolves = |p: &str| {
        all_paths.contains(p) || all_paths.iter().any(|fp| fp.ends_with(&format!("/{p}")))
    };
    let re = Regex::new(r"\b((?:crates|apps|tools|src)/[\w./-]+\.rs)\b").expect("static regex");

    let mut out = Vec::new();
    // 'stale'-status memories are still LIVE (just flagged) and are the ones most likely to hold a
    // moved/deleted path — scan them too, matching the memory layer's status IN ('active','stale').
    // Scoped to the ACTIVE repo (V042): a sibling repo's memory referencing a path that does not
    // resolve in THIS repo's index must not surface as this repo's stale_reference finding.
    let scope = crate::index::schema::periphery_repo_scope(conn, "repo_memories")?;
    let repo_clause = crate::index::schema::periphery_repo_scope_clause(&scope, "repo_memories");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, body FROM repo_memories WHERE status IN ('active', 'stale'){repo_clause}"
    ))?;
    let mems: Vec<(String, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (id, body) in mems {
        let bytes = body.as_bytes();
        let mut gone: Vec<String> = re
            .captures_iter(&body)
            .filter_map(|c| c.get(1))
            // skip refs embedded in a URL or a longer path (preceded by '/' or ':') — a common
            // false-positive source; a genuine reference is preceded by whitespace/punctuation.
            // (Rust's regex has no lookbehind, so this is a post-filter on the match start.)
            .filter(|m| m.start() == 0 || !matches!(bytes[m.start() - 1], b'/' | b':'))
            .map(|m| m.as_str().to_string())
            .filter(|p| !resolves(p))
            .collect();
        gone.sort();
        gone.dedup();
        if !gone.is_empty() {
            out.push(DreamFinding {
                kind: "stale_reference".into(),
                subject: id,
                evidence: format!("references unresolved path(s): {} [E0]", gone.join(", ")),
                rank: 0.5,
            });
        }
    }
    Ok(out)
}

/// The active `repo_id` to scope the `dream_findings` lifecycle by (V042), or `None` on the pre-A5
/// schema (the column is absent, so the reads/writes run unscoped — the original repo-global SQL).
/// Probes `dream_findings` — see `schema::periphery_repo_scope`.
fn dream_repo_scope(conn: &Connection) -> rusqlite::Result<Option<String>> {
    crate::index::schema::periphery_repo_scope(conn, "dream_findings")
}

/// The ` AND dream_findings.repo_id = '…'` predicate for a scoped `dream_findings` read/write, or
/// `""` when unscoped.
fn dream_repo_scope_clause(scope: &Option<String>) -> String {
    crate::index::schema::periphery_repo_scope_clause(scope, "dream_findings")
}

/// Sync findings into `dream_findings` with the identity-keyed lifecycle (refresh / supersede /
/// resolve), ATOMICALLY: all writes run in one transaction so a mid-run failure can't leave a torn
/// worklist (the per-finding upserts + the resolve pass commit together or not at all). Returns
/// (opened, refreshed, superseded, resolved).
///
/// SCOPE (A5): every lookup / supersede / resolve / list here filters `dream_findings.repo_id`, and
/// the INSERT stamps it, so a run in one repo of a consolidated DB never reads, supersedes, or
/// resolves a sibling repo's worklist rows. The id is derived by [`repo_folded_finding_id`] — the
/// repo is IN the hash, so two repos minting the same `(kind, subject, claim_hash)` never collide
/// on the global `id` PK (the scoped lookup cannot see a sibling's row, so a repo-blind id would
/// turn that case into a PK violation on insert rather than the pre-scoping silent UPDATE).
fn sync(
    conn: &Connection,
    findings: &[DreamFinding],
    now_ms: i64,
) -> rusqlite::Result<(usize, usize, usize, usize)> {
    let tx = conn.unchecked_transaction()?;
    let counts = sync_in_tx(&tx, findings, now_ms)?;
    tx.commit()?;
    Ok(counts)
}

fn sync_in_tx(
    conn: &Connection,
    findings: &[DreamFinding],
    now_ms: i64,
) -> rusqlite::Result<(usize, usize, usize, usize)> {
    let (mut opened, mut refreshed, mut superseded, mut resolved) = (0, 0, 0, 0);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    // Resolve the active-repo scope once for the whole sync (all statements below share it): the
    // read predicate `{repo_clause}` and the INSERT's `{repo_col}`/`{repo_val}` stamp prefix.
    let scope = dream_repo_scope(conn)?;
    let repo_clause = dream_repo_scope_clause(&scope);
    let (repo_col, repo_val) = match &scope {
        Some(repo_id) => ("repo_id, ".to_string(), format!("'{}', ", repo_id.replace('\'', "''"))),
        None => (String::new(), String::new()),
    };
    for f in findings {
        let ch = claim_hash(&f.kind, &f.subject, &f.evidence);
        seen.insert((f.kind.clone(), f.subject.clone()));
        // Look up by the EXACT claim (incl. its current status) — NOT status-blind, or an evidence
        // flip-back (A→B→A) or a resolved-then-reappears would match a terminal row and silently
        // refresh it, stranding the actually-current finding. Scoped to the active repo so a
        // sibling's identically-keyed row can't hijack this repo's re-resolution.
        let existing: Option<(String, String)> = conn
            .query_row(
                &format!(
                    "SELECT id, status FROM dream_findings WHERE kind = ?1 AND subject = ?2 AND \
                     claim_hash = ?3{repo_clause}"
                ),
                rusqlite::params![f.kind, f.subject, ch],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        // supersede any OTHER current row for this (kind, subject) — used by both the revive and
        // the brand-new branches (the world changed, so a prior verdict no longer applies). Scoped
        // so it never supersedes a sibling repo's current finding sharing this (kind, subject).
        let supersede_others = |id: &str| {
            conn.execute(
                &format!(
                    "UPDATE dream_findings SET status = 'superseded', superseded_by = ?1 WHERE \
                     kind = ?2 AND subject = ?3 AND id != ?1 AND status IN \
                     ('open','accepted','dismissed'){repo_clause}"
                ),
                rusqlite::params![id, f.kind, f.subject],
            )
        };
        match existing {
            // same claim still current: refresh, preserving any human verdict (accepted/dismissed)
            Some((id, status)) if matches!(status.as_str(), "open" | "accepted" | "dismissed") => {
                conn.execute(
                    "UPDATE dream_findings SET last_seen_at_ms = ?1, base_rank = ?2 WHERE id = ?3",
                    rusqlite::params![now_ms, f.rank, id],
                )?;
                refreshed += 1;
            },
            // same claim was terminal (resolved/superseded/archived) but REAPPEARED: revive to open
            // (UNIQUE(kind,subject,claim_hash) forbids a fresh insert) + supersede other current
            // rows.
            Some((id, _terminal)) => {
                conn.execute(
                    "UPDATE dream_findings SET status = 'open', superseded_by = NULL, \
                     last_seen_at_ms = ?1, base_rank = ?2 WHERE id = ?3",
                    rusqlite::params![now_ms, f.rank, id],
                )?;
                opened += 1;
                superseded += supersede_others(&id)?;
            },
            // brand-new claim for this (kind, subject): open fresh + supersede prior current rows
            None => {
                let id = finding_id(&scope, &f.kind, &f.subject, &ch);
                conn.execute(
                    &format!(
                        "INSERT INTO dream_findings({repo_col}id, kind, subject, claim_hash, \
                         evidence, base_rank, status, first_seen_at_ms, last_seen_at_ms) \
                         VALUES({repo_val}?1,?2,?3,?4,?5,?6,'open',?7,?7)"
                    ),
                    rusqlite::params![id, f.kind, f.subject, ch, f.evidence, f.rank, now_ms],
                )?;
                opened += 1;
                superseded += supersede_others(&id)?;
            },
        }
    }
    // resolve: any current finding whose (kind, subject) was not reported this run -> drift gone.
    // Scoped so this repo's run only resolves ITS OWN worklist, never a sibling repo's open
    // findings.
    let current: Vec<(String, String, String)> = conn
        .prepare(&format!(
            "SELECT id, kind, subject FROM dream_findings WHERE status IN \
             ('open','accepted','dismissed'){repo_clause}"
        ))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, kind, subject) in current {
        if !seen.contains(&(kind, subject)) {
            conn.execute("UPDATE dream_findings SET status = 'resolved' WHERE id = ?1", [&id])?;
            resolved += 1;
        }
    }
    Ok((opened, refreshed, superseded, resolved))
}

/// Run the deterministic dream-mode pass: compute findings, sync the worklist, return the open
/// worklist (ranked) + lifecycle counts. Writes ONLY to `dream_findings`; never touches memories.
pub fn dream_run(conn: &Connection, opts: DreamOptions) -> rusqlite::Result<DreamReport> {
    let mut findings = coverage_gap(conn, opts.limit)?;
    findings.extend(stale_reference(conn)?);
    let (opened, refreshed, superseded, resolved) = sync(conn, &findings, opts.now_ms)?;

    // emit the OPEN worklist from the store (post-sync); each finding's exposed rank is its
    // base_rank DECAYED by age since first_seen (effective_rank) — a stale unreviewed finding
    // sinks below fresh ones (the documented anti-rot, now real, not just base_rank DESC). Scoped
    // to the active repo so the emitted worklist never mixes in a sibling repo's open findings.
    let repo_clause = dream_repo_scope_clause(&dream_repo_scope(conn)?);
    let mut open: Vec<DreamFinding> = conn
        .prepare(&format!(
            "SELECT kind, subject, evidence, base_rank, first_seen_at_ms FROM dream_findings \
             WHERE status = 'open'{repo_clause}"
        ))?
        .query_map([], |r| {
            let base: f64 = r.get(3)?;
            let first_seen: i64 = r.get(4)?;
            Ok(DreamFinding {
                kind: r.get(0)?,
                subject: r.get(1)?,
                evidence: r.get(2)?,
                rank: effective_rank(base, first_seen, opts.now_ms),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // deterministic order: effective rank desc, then subject (ties — e.g. fresh stale_reference at
    // base 0.5 — are stable across runs/reindexes instead of arbitrary SQLite row order).
    open.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.subject.cmp(&b.subject))
    });
    Ok(DreamReport { findings: open, opened, refreshed, superseded, resolved })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&c).unwrap();
        c
    }

    /// Point the connection's periphery scope at `repo_id` — mirrors the scope-context write the
    /// production open installs (and `multi_repo_scope::a5_set_active_repo`).
    fn set_repo(c: &Connection, repo_id: &str) {
        c.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        c.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [repo_id],
        )
        .unwrap();
    }

    /// A5 finding: `finding_id` folds the owning repo. Two repos minting the SAME
    /// `(kind, subject, claim_hash)` derive DISTINCT ids — a repo-blind id would make the second
    /// repo's sync explode on the global `id` PK, because its repo-scoped lookup cannot see the
    /// sibling's row and takes the INSERT path.
    #[test]
    fn two_repos_minting_the_same_finding_do_not_collide_on_the_global_id() {
        let c = mem_db();
        let finding = || {
            vec![DreamFinding {
                kind: "coverage_gap".into(),
                subject: "x::F".into(),
                evidence: "7 callers".into(),
                rank: 0.5,
            }]
        };
        set_repo(&c, "repo-a");
        sync(&c, &finding(), 1000).unwrap();
        set_repo(&c, "repo-b");
        // Pre-fix this is a PK violation, not an UPDATE: the scoped lookup misses repo-a's row and
        // the insert derives the same repo-blind id.
        sync(&c, &finding(), 2000).unwrap();

        let rows: Vec<(String, String)> = c
            .prepare(
                "SELECT id, repo_id FROM dream_findings WHERE status = 'open' ORDER BY repo_id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), 2, "each repo holds its own open finding: {rows:?}");
        assert_eq!(rows[0].1, "repo-a");
        assert_eq!(rows[1].1, "repo-b");
        assert_ne!(rows[0].0, rows[1].0, "the two findings carry DISTINCT repo-folded ids");
    }

    /// `rederive_finding_ids` re-points every id (and the in-table `superseded_by` references) to
    /// the repo-folded derivation, and is idempotent — the V042 migration and adoption both call
    /// it after re-stamping `repo_id`.
    #[test]
    fn rederive_finding_ids_repoints_ids_and_superseded_by() {
        let c = mem_db();
        // Two rows under `repo-x` carrying LEGACY (repo-blind) ids, chained by superseded_by.
        c.execute_batch(
            "INSERT INTO dream_findings(id, kind, subject, claim_hash, evidence, base_rank, \
             status, superseded_by, first_seen_at_ms, last_seen_at_ms, repo_id)
             VALUES ('oldid1', 'coverage_gap', 'x::F', 'c1', 'ev1', 0.5, 'superseded', 'oldid2', \
             0, 0, 'repo-x'),
                    ('oldid2', 'coverage_gap', 'x::F', 'c2', 'ev2', 0.6, 'open', NULL, 0, 0, \
             'repo-x');",
        )
        .unwrap();
        rederive_finding_ids(&c).unwrap();

        let new1 = repo_folded_finding_id("repo-x", "coverage_gap", "x::F", "c1");
        let new2 = repo_folded_finding_id("repo-x", "coverage_gap", "x::F", "c2");
        let ids: Vec<(String, Option<String>)> = c
            .prepare("SELECT id, superseded_by FROM dream_findings ORDER BY claim_hash")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids[0].0, new1, "row 1's id re-derived under its repo");
        assert_eq!(ids[1].0, new2, "row 2's id re-derived under its repo");
        assert_eq!(
            ids[0].1.as_deref(),
            Some(new2.as_str()),
            "the superseded_by reference is remapped to the re-derived id"
        );
        // Idempotent: a second pass moves nothing.
        rederive_finding_ids(&c).unwrap();
        let again: Vec<String> = c
            .prepare("SELECT id FROM dream_findings ORDER BY claim_hash")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(again, vec![new1, new2], "re-derivation is idempotent");
    }

    #[test]
    fn sync_is_idempotent_and_supersedes_on_change() {
        let c = mem_db();
        let f1 = vec![DreamFinding {
            kind: "coverage_gap".into(),
            subject: "x::F".into(),
            evidence: "10 callers".into(),
            rank: 1.0,
        }];
        let (o, r, s, res) = sync(&c, &f1, 1000).unwrap();
        assert_eq!((o, r, s, res), (1, 0, 0, 0), "first run opens");
        let (o, r, _, _) = sync(&c, &f1, 2000).unwrap();
        assert_eq!((o, r), (0, 1), "same finding refreshes, no duplicate");
        // material change -> supersede prior, open fresh
        let f2 = vec![DreamFinding {
            kind: "coverage_gap".into(),
            subject: "x::F".into(),
            evidence: "40 callers".into(),
            rank: 1.0,
        }];
        let (o, _, s, _) = sync(&c, &f2, 3000).unwrap();
        assert_eq!((o, s), (1, 1), "changed evidence supersedes + opens fresh");
        let opened: i64 = c
            .query_row("SELECT COUNT(*) FROM dream_findings WHERE status='open'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(opened, 1, "exactly one open finding for the (kind,subject)");
        // run with no findings -> the (kind,subject) resolves
        let (_, _, _, res) = sync(&c, &[], 4000).unwrap();
        assert_eq!(res, 1, "absent finding resolves");
    }

    #[test]
    fn flip_back_and_resolved_reappears_keep_the_worklist_correct() {
        let c = mem_db();
        let cg = |ev: &str, rank: f64| {
            vec![DreamFinding {
                kind: "coverage_gap".into(),
                subject: "x::F".into(),
                evidence: ev.into(),
                rank,
            }]
        };
        let open_evidence = |c: &Connection| -> (String, f64) {
            c.query_row(
                "SELECT evidence, base_rank FROM dream_findings WHERE status='open'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let count = |c: &Connection, status: &str| -> i64 {
            c.query_row("SELECT COUNT(*) FROM dream_findings WHERE status=?1", [status], |r| {
                r.get(0)
            })
            .unwrap()
        };
        // A -> B -> A: the flip-back must leave A current with A's evidence, NOT strand B as open.
        sync(&c, &cg("10 callers", 0.1), 1000).unwrap();
        sync(&c, &cg("40 callers", 0.9), 2000).unwrap();
        sync(&c, &cg("10 callers", 0.1), 3000).unwrap();
        assert_eq!(count(&c, "open"), 1, "exactly one open row after flip-back");
        assert_eq!(
            open_evidence(&c),
            ("10 callers".into(), 0.1),
            "current (A) is open, not stale B"
        );
        // resolve, then reappear with the SAME evidence: must revive to open, not stay resolved.
        sync(&c, &[], 4000).unwrap();
        assert!(count(&c, "resolved") >= 1, "absent finding resolves");
        sync(&c, &cg("10 callers", 0.1), 5000).unwrap();
        assert_eq!(count(&c, "open"), 1, "reappearing resolved finding is revived to open");
    }

    #[test]
    fn dream_run_never_mutates_any_memory_column() {
        let c = mem_db();
        // seed a memory whose body references a gone path so stale_reference is forced to READ it
        // and emit a finding keyed on this memory (makes the assertion non-vacuous).
        c.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version) VALUES \
             ('m1','Invariant','t','refs \
             crates/ghost/src/vanished.rs','high','active','agent',1,1,'agent','v1')",
            [],
        )
        .unwrap();
        // snapshot every column dream could plausibly mutate (stronger than a row COUNT: an
        // in-place UPDATE to body/status/etc. would pass a count check but fail this).
        let snap = |c: &Connection| {
            c.query_row(
                "SELECT body, status, confidence, kind, title, created_by, created_at_ms, \
                 updated_at_ms, source, memory_version FROM repo_memories WHERE id='m1'",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, String>(8)?,
                        r.get::<_, String>(9)?,
                    ))
                },
            )
            .unwrap()
        };
        let before = snap(&c);
        let report = dream_run(&c, DreamOptions { now_ms: 1000, limit: 10 }).unwrap();
        assert_eq!(before, snap(&c), "dream_run must leave EVERY repo_memories column unchanged");
        assert!(
            report.findings.iter().any(|f| f.kind == "stale_reference" && f.subject == "m1"),
            "non-vacuous: the seeded memory produced a stale_reference finding"
        );
    }

    #[test]
    fn rank_decays_with_age() {
        let hl = RANK_HALF_LIFE_MS as i64;
        assert!((effective_rank(1.0, 0, 0) - 1.0).abs() < 1e-9, "no decay at age 0");
        assert!((effective_rank(1.0, 0, hl) - 0.5).abs() < 0.01, "one half-life halves the rank");
        assert!(
            effective_rank(1.0, 0, 2 * hl) < effective_rank(1.0, 0, hl),
            "older unreviewed findings sink further"
        );
    }
}
