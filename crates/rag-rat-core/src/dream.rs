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

const COVERAGE_GAP_SCAN: usize = 400; // in-degree candidates to consider before filtering/limiting

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
/// no institutional memory). Importance proxy = raw in-degree; rank normalized to the top.
fn coverage_gap(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<DreamFinding>> {
    let covered_syms: HashSet<i64> = conn
        .prepare("SELECT DISTINCT symbol_id FROM repo_memory_bindings WHERE symbol_id IS NOT NULL")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let covered_paths: HashSet<String> = conn
        .prepare("SELECT DISTINCT path FROM repo_memory_bindings WHERE path IS NOT NULL")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut stmt = conn.prepare(
        "SELECT to_symbol_id, COUNT(*) AS d FROM edges_data WHERE to_symbol_id IS NOT NULL GROUP \
         BY to_symbol_id ORDER BY d DESC LIMIT ?1",
    )?;
    let candidates: Vec<(i64, i64)> = stmt
        .query_map([COVERAGE_GAP_SCAN as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let top_d = candidates.first().map(|(_, d)| *d).unwrap_or(1).max(1) as f64;

    let mut out = Vec::new();
    for (sym_id, d) in candidates {
        if out.len() >= limit {
            break;
        }
        if covered_syms.contains(&sym_id) {
            continue;
        }
        let row = conn
            .query_row(
                "SELECT s.name, f.path FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.id \
                 = ?1",
                [sym_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        let Some((name, path)) = row else { continue };
        if covered_paths.contains(&path) || path.to_lowercase().contains("test") {
            continue; // test infra is high-in-degree but not a real coverage gap (spike eval: 4/20)
        }
        out.push(DreamFinding {
            kind: "coverage_gap".into(),
            subject: format!("{path}::{name}"),
            evidence: format!("{d} callers, no memory binding [E0 importance proxy]"),
            rank: d as f64 / top_d,
        });
    }
    Ok(out)
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
    let mut stmt = conn.prepare("SELECT id, body FROM repo_memories WHERE status = 'active'")?;
    let mems: Vec<(String, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (id, body) in mems {
        let mut gone: Vec<String> = re
            .captures_iter(&body)
            .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
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
/// resolve). Returns (opened, refreshed, superseded, resolved).
fn sync(
    conn: &Connection,
    findings: &[DreamFinding],
    now_ms: i64,
) -> rusqlite::Result<(usize, usize, usize, usize)> {
    let (mut opened, mut refreshed, mut superseded, mut resolved) = (0, 0, 0, 0);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for f in findings {
        let ch = claim_hash(&f.kind, &f.subject, &f.evidence);
        seen.insert((f.kind.clone(), f.subject.clone()));
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM dream_findings WHERE kind = ?1 AND subject = ?2 AND claim_hash = \
                 ?3",
                rusqlite::params![f.kind, f.subject, ch],
                |r| r.get(0),
            )
            .ok();
        if existing.is_some() {
            conn.execute(
                "UPDATE dream_findings SET last_seen_at_ms = ?1, base_rank = ?2 WHERE kind = ?3 \
                 AND subject = ?4 AND claim_hash = ?5",
                rusqlite::params![now_ms, f.rank, f.kind, f.subject, ch],
            )?;
            refreshed += 1;
            continue;
        }
        // new claim for this (kind, subject): supersede any current (open/verdict) finding, open
        // fresh
        let id = finding_id(&f.kind, &f.subject, &ch);
        conn.execute(
            "INSERT INTO dream_findings(id, kind, subject, claim_hash, evidence, base_rank, \
             status, first_seen_at_ms, last_seen_at_ms) VALUES(?1,?2,?3,?4,?5,?6,'open',?7,?7)",
            rusqlite::params![id, f.kind, f.subject, ch, f.evidence, f.rank, now_ms],
        )?;
        opened += 1;
        superseded += conn.execute(
            "UPDATE dream_findings SET status = 'superseded', superseded_by = ?1 WHERE kind = ?2 \
             AND subject = ?3 AND id != ?1 AND status NOT IN ('resolved','superseded','archived')",
            rusqlite::params![id, f.kind, f.subject],
        )?;
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

    // emit the OPEN worklist from the store (post-sync), ranked
    let mut open: Vec<DreamFinding> = conn
        .prepare(
            "SELECT kind, subject, evidence, base_rank FROM dream_findings WHERE status = 'open'",
        )?
        .query_map([], |r| {
            Ok(DreamFinding {
                kind: r.get(0)?,
                subject: r.get(1)?,
                evidence: r.get(2)?,
                rank: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    open.sort_by(|a, b| b.rank.partial_cmp(&a.rank).unwrap_or(std::cmp::Ordering::Equal));
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
    fn dream_run_writes_findings_never_touches_memories() {
        let c = mem_db();
        let before: i64 =
            c.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        let report = dream_run(&c, DreamOptions { now_ms: 1000, limit: 10 }).unwrap();
        let after: i64 =
            c.query_row("SELECT COUNT(*) FROM repo_memories", [], |r| r.get(0)).unwrap();
        assert_eq!(before, after, "dream_run must never mutate repo_memories");
        // empty index -> no findings, but the call succeeds and the table is usable
        assert!(report.findings.is_empty());
    }
}
