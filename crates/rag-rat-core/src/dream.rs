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

fn finding_id(kind: &str, subject: &str, ch: &str) -> String {
    let mut h = Sha256::new();
    h.update(kind.as_bytes());
    h.update([0x1f]);
    h.update(subject.as_bytes());
    h.update([0x1f]);
    h.update(ch.as_bytes());
    hex16(&h.finalize())
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
    let mut stmt = conn.prepare(
        "SELECT s.name, f.path, COUNT(DISTINCT e.from_symbol_id) AS d FROM edges_data e JOIN \
         symbols s ON s.id = e.to_symbol_id JOIN files f ON f.id = s.file_id WHERE e.to_symbol_id \
         IS NOT NULL AND e.from_symbol_id IS NOT NULL AND f.has_test_code = 0 AND e.to_symbol_id \
         NOT IN (SELECT symbol_id FROM repo_memory_bindings WHERE symbol_id IS NOT NULL) AND \
         e.to_symbol_id NOT IN (SELECT lsm.symbol_id FROM logical_symbol_members lsm JOIN \
         repo_memory_bindings b ON b.logical_symbol_id = lsm.logical_symbol_id WHERE \
         b.logical_symbol_id IS NOT NULL) AND f.path NOT IN (SELECT path FROM \
         repo_memory_bindings WHERE path IS NOT NULL) GROUP BY e.to_symbol_id ORDER BY d DESC \
         LIMIT ?1",
    )?;
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
    let mut stmt =
        conn.prepare("SELECT id, body FROM repo_memories WHERE status IN ('active', 'stale')")?;
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

/// Sync findings into `dream_findings` with the identity-keyed lifecycle (refresh / supersede /
/// resolve), ATOMICALLY: all writes run in one transaction so a mid-run failure can't leave a torn
/// worklist (the per-finding upserts + the resolve pass commit together or not at all). Returns
/// (opened, refreshed, superseded, resolved).
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
    for f in findings {
        let ch = claim_hash(&f.kind, &f.subject, &f.evidence);
        seen.insert((f.kind.clone(), f.subject.clone()));
        // Look up by the EXACT claim (incl. its current status) — NOT status-blind, or an evidence
        // flip-back (A→B→A) or a resolved-then-reappears would match a terminal row and silently
        // refresh it, stranding the actually-current finding.
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT id, status FROM dream_findings WHERE kind = ?1 AND subject = ?2 AND \
                 claim_hash = ?3",
                rusqlite::params![f.kind, f.subject, ch],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        // supersede any OTHER current row for this (kind, subject) — used by both the revive and
        // the brand-new branches (the world changed, so a prior verdict no longer applies).
        let supersede_others = |id: &str| {
            conn.execute(
                "UPDATE dream_findings SET status = 'superseded', superseded_by = ?1 WHERE kind = \
                 ?2 AND subject = ?3 AND id != ?1 AND status IN ('open','accepted','dismissed')",
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
                let id = finding_id(&f.kind, &f.subject, &ch);
                conn.execute(
                    "INSERT INTO dream_findings(id, kind, subject, claim_hash, evidence, \
                     base_rank, status, first_seen_at_ms, last_seen_at_ms) \
                     VALUES(?1,?2,?3,?4,?5,?6,'open',?7,?7)",
                    rusqlite::params![id, f.kind, f.subject, ch, f.evidence, f.rank, now_ms],
                )?;
                opened += 1;
                superseded += supersede_others(&id)?;
            },
        }
    }
    // resolve: any current finding whose (kind, subject) was not reported this run -> drift gone
    let current: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT id, kind, subject FROM dream_findings WHERE status IN \
             ('open','accepted','dismissed')",
        )?
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
    // sinks below fresh ones (the documented anti-rot, now real, not just base_rank DESC).
    let mut open: Vec<DreamFinding> = conn
        .prepare(
            "SELECT kind, subject, evidence, base_rank, first_seen_at_ms FROM dream_findings \
             WHERE status = 'open'",
        )?
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
