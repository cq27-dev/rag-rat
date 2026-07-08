//! Dream v1 findings lifecycle: the two deterministic finding builders (`coverage_gap`,
//! `stale_reference`) and the identity-keyed `dream_findings` sync (refresh / supersede / resolve /
//! revive) with repo-folded ids. Split out of the former monolithic `dream.rs` so the module index
//! (`mod.rs`) curates the surface and the v2 verification pass lives in its own sibling (`verify`).

use std::collections::{HashMap, HashSet};

use regex::Regex;
use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::DreamFinding;
use crate::query::pagerank::{self, ImportanceOptions};

/// Ranking pool `coverage_gap` pulls from [`pagerank::important_symbols`] before filtering out
/// memory-covered / test-infra rows. Sized like the ranker's own `MAX_RESULTS` cap so a repo whose
/// most-load-bearing symbols are already well-memoried still leaves enough uncovered candidates to
/// fill the finding `limit`; a symbol ranked below this pool is, by definition, not load-bearing
/// enough to be worth a coverage-gap finding.
const COVERAGE_RANK_POOL: usize = 500;

/// The finding kinds a plain `dream` run always computes (no `--verify`) — the resolve pass may
/// close a stale one even when this run produced zero of them.
pub(super) const BASE_FINDING_KINDS: &[&str] = &["coverage_gap", "stale_reference"];
/// The extra kinds the verify pass computes; only resolvable on a `--verify` run (else a plain
/// `dream` would wrongly resolve findings a prior verify run opened — the kind was not evaluated).
pub(super) const VERIFY_FINDING_KINDS: &[&str] = &["memory_unverifiable", "memory_divergence"];

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

/// `coverage_gap`: the most load-bearing symbols with no memory binding (load-bearing code with no
/// institutional memory). Importance is the CANONICAL scoped weighted PageRank
/// ([`pagerank::important_symbols`], query/pagerank.rs), NOT a raw caller in-degree — the #261 fix.
/// The old proxy `COUNT(DISTINCT e.from_symbol_id)` counted callers from `edges_data` unscoped, so
/// on a consolidated DB it summed callers from OTHER worktrees/overlays (latent on a single
/// checkout, wrong across worktrees); the PageRank facility reads `edges_data` joined to the
/// per-connection `files` scope view (active checkout only) and hydrates winners through it, so
/// both the flow and the endpoints are checkout-scoped — and it is a strictly better "load-bearing"
/// signal (edge-weighted + confidence-scaled) than raw in-degree.
///
/// We rank a generous pool ([`COVERAGE_RANK_POOL`]) and then, IN RUST, drop the memory-covered and
/// test-infra rows and take the top `limit` — the ranker has no coverage/test notion, so the filter
/// can't live in its SQL. COVERAGE mirrors the canonical memory query (query/memory/api.rs): a
/// symbol is covered if a binding matches its `symbol_id`, its logical symbol (mapped through
/// `logical_symbol_members` — a logical binding covers ALL its member variants), or its file path.
/// Test infra is filtered by `has_test_code`, NOT `path LIKE '%test%'` (which would drop real files
/// like `attestation.rs`). Every coverage read is ACTIVE-repo scoped (V042): a SIBLING repo's
/// binding — a colliding `symbol_id` rowid, or a path binding at a path this repo also has — must
/// not mark this repo's load-bearing symbol covered and suppress its finding (a false negative).
///
/// `oracle_effects: None` — `dream_run` holds only a `&Connection`, not the `IndexDatabase` state
/// (active commit / worktree id) the SCIP-oracle upgrade map is built from, so `coverage_gap` ranks
/// the heuristic confidence-weighted graph. That is the same scoped substrate `important_symbols`
/// uses, minus the optional compiler upgrade — acceptable for a heuristic worklist signal.
pub(super) fn coverage_gap(conn: &Connection, limit: usize) -> anyhow::Result<Vec<DreamFinding>> {
    // A zero budget wants zero findings — skip the coverage-set reads and the whole-graph PageRank
    // (the old `LIMIT 0` SQL was free; keep this path free too).
    if limit == 0 {
        return Ok(Vec::new());
    }
    // `{binding_clause}`/`{b_clause}` are ` AND <table>.repo_id = '<id>'` under scope, empty
    // pre-A5.
    let scope = crate::index::schema::periphery_repo_scope(conn, "repo_memories")?;
    let binding_clause =
        crate::index::schema::periphery_repo_scope_clause(&scope, "repo_memory_bindings");
    let b_clause = crate::index::schema::periphery_repo_scope_clause(&scope, "b");

    // Covered symbol ids: a direct `symbol_id` binding OR any member of a bound logical symbol.
    let mut covered_symbols: HashSet<i64> = conn
        .prepare(&format!(
            "SELECT symbol_id FROM repo_memory_bindings WHERE symbol_id IS NOT \
             NULL{binding_clause}"
        ))?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let logical_members = conn
        .prepare(&format!(
            "SELECT lsm.symbol_id FROM logical_symbol_members lsm JOIN repo_memory_bindings b ON \
             b.logical_symbol_id = lsm.logical_symbol_id WHERE b.logical_symbol_id IS NOT \
             NULL{b_clause}"
        ))?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    covered_symbols.extend(logical_members);

    let covered_paths: HashSet<String> = conn
        .prepare(&format!(
            "SELECT path FROM repo_memory_bindings WHERE path IS NOT NULL{binding_clause}"
        ))?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;

    // Test-infra files (mirrors the old `f.has_test_code = 0`). Read through the SAME `files` scope
    // view `important_symbols` hydrates paths through, so the path comparison below is consistent.
    let test_paths: HashSet<String> = conn
        .prepare("SELECT path FROM files WHERE has_test_code = 1")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;

    // Symbols that are DEPENDED UPON in the active checkout: the target of an in-scope edge with a
    // resolved caller. `important_symbols` ranks EVERY graph node, so a source-only leaf caller
    // (only outgoing edges) still gets non-zero PageRank from teleport mass and would otherwise
    // surface as a bogus coverage gap — but a coverage gap is load-bearing = depended-upon
    // code. This restores the old builder's `e.to_symbol_id` gate: only inbound symbols
    // qualify. Scoped by `source_file_id` through the same `files` view the ranker uses, so a
    // purely cross-worktree caller never counts.
    let inbound: HashSet<i64> = conn
        .prepare(
            "SELECT DISTINCT d.to_symbol_id FROM edges_data d JOIN files ON files.id = \
             d.source_file_id WHERE d.to_symbol_id IS NOT NULL AND d.from_symbol_id IS NOT NULL",
        )?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;

    let ranked = pagerank::important_symbols(conn, ImportanceOptions {
        limit: limit.max(COVERAGE_RANK_POOL),
        personalize_to: &[],
        oracle_effects: None,
    })?;

    let mut seen_subjects: HashSet<String> = HashSet::new();
    let mut out: Vec<DreamFinding> = Vec::new();
    let mut top_score = f64::MIN_POSITIVE;
    for sym in ranked.symbols {
        if out.len() >= limit {
            break;
        }
        if !inbound.contains(&sym.symbol_id)
            || covered_symbols.contains(&sym.symbol_id)
            || covered_paths.contains(&sym.path)
            || test_paths.contains(&sym.path)
        {
            continue;
        }
        // `path::<bare name>` (the last `::` segment of the qualified name) — the readable identity
        // the old `s.name`-based subject used, not the redundant `path::<fully-qualified>`. cfg
        // twins / overloads hydrate to the SAME subject from different symbol ids: emit ONE finding
        // (keep the highest-ranked), so a run never yields two findings sharing a (kind, subject).
        let bare = sym.qualified_name.rsplit("::").next().unwrap_or(sym.qualified_name.as_str());
        let subject = format!("{}::{}", sym.path, bare);
        if !seen_subjects.insert(subject.clone()) {
            continue;
        }
        // Normalize by the top uncovered score so the highest-priority gap ranks 1.0 (the old
        // `d / top_d` semantics), decaying with importance. The evidence carries NO volatile score
        // — a stable claim keeps a reviewer's accept/dismiss alive across graph churn (only
        // becoming covered, or dropping out of the load-bearing pool, resolves the
        // finding).
        if out.is_empty() {
            top_score = sym.score.max(f64::MIN_POSITIVE);
        }
        out.push(DreamFinding {
            kind: "coverage_gap".into(),
            subject,
            evidence: "load-bearing symbol (scoped weighted PageRank), no memory binding [E0]"
                .into(),
            rank: sym.score / top_score,
        });
    }
    Ok(out)
}

/// `stale_reference`: a memory body references a `.rs` path that no longer resolves against the
/// index. Suffix-aware so prose shorthand (`src/lib.rs` for `crates/x/src/lib.rs`) is NOT flagged.
pub(super) fn stale_reference(conn: &Connection) -> rusqlite::Result<Vec<DreamFinding>> {
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
pub(super) fn dream_repo_scope(conn: &Connection) -> rusqlite::Result<Option<String>> {
    crate::index::schema::periphery_repo_scope(conn, "dream_findings")
}

/// The ` AND dream_findings.repo_id = '…'` predicate for a scoped `dream_findings` read/write, or
/// `""` when unscoped.
pub(super) fn dream_repo_scope_clause(scope: &Option<String>) -> String {
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
///
/// RESOLVE IS KIND-SCOPED: the resolve pass only closes findings whose `kind` is in
/// `resolve_kinds` — the kinds this run actually COMPUTED. A run that omits a kind (e.g. a plain
/// `dream` without `--verify` skips `memory_unverifiable` / `memory_divergence`) leaves that kind's
/// open findings untouched instead of resolving them as "no longer seen". Passing the kind (not the
/// findings) is deliberate: a computed kind that legitimately produced ZERO findings this run must
/// still resolve its stale rows, so the resolve set can't be derived from `findings` alone.
pub(super) fn sync(
    conn: &Connection,
    findings: &[DreamFinding],
    now_ms: i64,
    resolve_kinds: &[&str],
) -> rusqlite::Result<(usize, usize, usize, usize)> {
    let tx = conn.unchecked_transaction()?;
    let counts = sync_in_tx(&tx, findings, now_ms, resolve_kinds)?;
    tx.commit()?;
    Ok(counts)
}

/// Collapse findings that share a `(kind, subject)` to ONE — the highest-ranked (ties keep the
/// first seen) — preserving the survivors' first-seen order. The dream lifecycle keys on
/// `(kind, subject)`, so two inputs sharing it can't both live as distinct current rows; deduping
/// here (rather than letting the loop's supersede race silently drop one) makes the survivor
/// deterministic. See the call site for the failure it guards (#261).
fn dedup_by_kind_subject(findings: &[DreamFinding]) -> Vec<&DreamFinding> {
    let mut position: HashMap<(&str, &str), usize> = HashMap::new();
    let mut order: Vec<&DreamFinding> = Vec::new();
    for f in findings {
        let key = (f.kind.as_str(), f.subject.as_str());
        match position.get(&key) {
            // Keep the winner's SLOT (first-seen order) but swap in the higher-ranked claim.
            Some(&idx) =>
                if f.rank > order[idx].rank {
                    order[idx] = f;
                },
            None => {
                position.insert(key, order.len());
                order.push(f);
            },
        }
    }
    order
}

fn sync_in_tx(
    conn: &Connection,
    findings: &[DreamFinding],
    now_ms: i64,
    resolve_kinds: &[&str],
) -> rusqlite::Result<(usize, usize, usize, usize)> {
    let (mut opened, mut refreshed, mut superseded, mut resolved) = (0, 0, 0, 0);
    // Dedup the INPUT by (kind, subject) first (#261). Two findings sharing a (kind, subject) but
    // carrying different evidence (→ different claim_hash → different id) would otherwise race in
    // the loop below: the second's `supersede_others` supersedes the first WITHIN the same run, so
    // a real finding is silently dropped (last-writer wins). Keep the highest-ranked claim
    // deterministically (ties keep the first seen) so the survivor is stable across re-runs. The
    // finding builders already avoid this (coverage_gap dedups subjects; the memory-* kinds key on
    // a unique memory id), so this is the belt-and-braces guard at the shared sync seam.
    let findings = dedup_by_kind_subject(findings);
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
        // Only resolve within the kinds this run computed — a kind not evaluated this run keeps its
        // open findings (a plain `dream` must not resolve a prior `--verify` run's findings).
        if resolve_kinds.contains(&kind.as_str()) && !seen.contains(&(kind, subject)) {
            conn.execute("UPDATE dream_findings SET status = 'resolved' WHERE id = ?1", [&id])?;
            resolved += 1;
        }
    }
    Ok((opened, refreshed, superseded, resolved))
}

/// The human verdict a reviewer applies to a dream finding via
/// `rag-rat dream <id> --accept|--dismiss|--reset`.
#[derive(Debug, Clone, Copy)]
pub enum ReviewVerdict {
    Accept,
    Dismiss,
    Reset,
}

/// The outcome of a review transition, echoed back to the operator.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewedFinding {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub status: String,
}

/// Apply a human review verdict to a dream finding by id — a full id or an unambiguous PREFIX
/// (git-style). Repo-scoped: only the active repo's findings resolve. Only a NON-terminal finding
/// (status `open`/`accepted`/`dismissed`) is reviewable — a `resolved`/`superseded`/`archived` row
/// is rejected (the world moved on; nothing to act on). `Accept`/`Dismiss` set the status +
/// `reviewed_at_ms`; `Reset` clears the human verdict (back to `open`, `reviewed_at_ms` NULL). The
/// verdict SURVIVES churn re-runs — [`sync`]'s refresh branch preserves `accepted`/`dismissed`.
///
/// Prefix resolution scans ALL of the active repo's findings (a small set) and matches in Rust — so
/// a `_`/`%` in the input can't act as a SQL `LIKE` wildcard, AND a prefix that also hits a
/// terminal row is rejected as ambiguous rather than silently acting on the one reviewable match.
pub(crate) fn review_dream_finding(
    conn: &Connection,
    id_or_prefix: &str,
    verdict: ReviewVerdict,
    now_ms: i64,
) -> anyhow::Result<ReviewedFinding> {
    let prefix = id_or_prefix.trim();
    if prefix.is_empty() {
        anyhow::bail!("a finding id is required");
    }
    let scope = dream_repo_scope(conn)?;
    let repo_clause = dream_repo_scope_clause(&scope);
    // Match the prefix against ALL of this repo's findings — reviewable AND terminal
    // (resolved/superseded/archived). A prefix that also hits a terminal row is NOT unambiguous
    // even if only one match is reviewable, so a stale short prefix can never silently act on a
    // *different* open finding than the one the user remembers. `WHERE 1` + `{repo_clause}` (which
    // starts with ` AND`) keeps every status in scope.
    let all: Vec<(String, String, String, String)> = conn
        .prepare(&format!(
            "SELECT id, kind, subject, status FROM dream_findings WHERE 1{repo_clause} ORDER BY id"
        ))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let matches: Vec<&(String, String, String, String)> =
        all.iter().filter(|(id, ..)| id.starts_with(prefix)).collect();
    let (id, kind, subject) = match matches.as_slice() {
        [(id, kind, subject, status)] => {
            if !matches!(status.as_str(), "open" | "accepted" | "dismissed") {
                anyhow::bail!(
                    "finding `{prefix}` is {status} — not reviewable (the code moved on; there's \
                     nothing to act on)"
                );
            }
            (id.clone(), kind.clone(), subject.clone())
        },
        [] => anyhow::bail!("no finding matches id `{prefix}`"),
        many => {
            let ids = many.iter().map(|(id, ..)| id.as_str()).collect::<Vec<_>>().join(", ");
            anyhow::bail!("id `{prefix}` is ambiguous — matches {} findings: {ids}", many.len());
        },
    };
    let new_status = match verdict {
        ReviewVerdict::Accept => "accepted",
        ReviewVerdict::Dismiss => "dismissed",
        ReviewVerdict::Reset => "open",
    };
    // Reset clears the human verdict; accept/dismiss stamp when it was reviewed.
    let reviewed_at = match verdict {
        ReviewVerdict::Reset => None,
        _ => Some(now_ms),
    };
    conn.execute(
        &format!(
            "UPDATE dream_findings SET status = ?2, reviewed_at_ms = ?3 WHERE id = ?1{repo_clause}"
        ),
        rusqlite::params![id, new_status, reviewed_at],
    )?;
    Ok(ReviewedFinding { id, kind, subject, status: new_status.to_string() })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{mem_db, set_repo};
    use super::*;

    // A single coverage_gap finding, synced into the active repo; returns its id.
    fn seed_one_finding(c: &Connection, subject: &str, now_ms: i64) -> String {
        let f = vec![DreamFinding {
            kind: "coverage_gap".into(),
            subject: subject.into(),
            evidence: "7 callers".into(),
            rank: 0.5,
        }];
        sync(c, &f, now_ms, BASE_FINDING_KINDS).unwrap();
        // By subject alone (not status): a re-sync after a review leaves the row non-'open', and
        // the caller still needs its id.
        c.query_row("SELECT id FROM dream_findings WHERE subject = ?1", [subject], |r| r.get(0))
            .unwrap()
    }

    fn status_and_reviewed(c: &Connection, id: &str) -> (String, Option<i64>) {
        c.query_row("SELECT status, reviewed_at_ms FROM dream_findings WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    }

    #[test]
    fn review_accept_dismiss_reset_toggle_status_and_reviewed_at() {
        // The human-review transitions (#262): the write-dead accepted/dismissed/reviewed_at_ms
        // fields are now set. Reset clears the human verdict entirely (back to open, timestamp
        // NULL).
        let c = mem_db();
        set_repo(&c, "r");
        let id = seed_one_finding(&c, "x::F", 1000);
        assert_eq!(status_and_reviewed(&c, &id), ("open".into(), None));

        let r = review_dream_finding(&c, &id, ReviewVerdict::Accept, 2000).unwrap();
        assert_eq!(r.status, "accepted");
        assert_eq!(status_and_reviewed(&c, &id), ("accepted".into(), Some(2000)));

        review_dream_finding(&c, &id, ReviewVerdict::Dismiss, 3000).unwrap();
        assert_eq!(status_and_reviewed(&c, &id), ("dismissed".into(), Some(3000)));

        review_dream_finding(&c, &id, ReviewVerdict::Reset, 4000).unwrap();
        assert_eq!(status_and_reviewed(&c, &id), ("open".into(), None), "reset clears the verdict");
    }

    #[test]
    fn review_verdict_survives_a_refresh() {
        // #262 depends on this: a re-run that STILL reports the finding must not revert the human
        // verdict — sync's refresh branch keeps accepted/dismissed.
        let c = mem_db();
        set_repo(&c, "r");
        let id = seed_one_finding(&c, "x::F", 1000);
        review_dream_finding(&c, &id, ReviewVerdict::Accept, 2000).unwrap();
        // re-sync the same finding (a refresh)
        seed_one_finding(&c, "x::F", 3000);
        assert_eq!(
            status_and_reviewed(&c, &id).0,
            "accepted",
            "refresh preserves the human verdict"
        );
    }

    #[test]
    fn review_rejects_a_terminal_finding() {
        // A resolved/superseded/archived finding is not reviewable — the code moved on.
        let c = mem_db();
        set_repo(&c, "r");
        let id = seed_one_finding(&c, "x::F", 1000);
        // A run that reports nothing resolves the open finding.
        sync(&c, &[], 2000, BASE_FINDING_KINDS).unwrap();
        assert_eq!(status_and_reviewed(&c, &id).0, "resolved");
        let err = review_dream_finding(&c, &id, ReviewVerdict::Accept, 3000).unwrap_err();
        assert!(err.to_string().contains("not reviewable"), "got: {err}");
    }

    #[test]
    fn review_prefix_matches_uniquely_and_errors_on_none_or_ambiguous() {
        // Real ids are sha256 hex (no forceable shared prefix), so insert controlled ids to
        // exercise the git-style prefix resolution.
        let c = mem_db();
        set_repo(&c, "r");
        for (id, subj) in [("abc111", "s1"), ("abc222", "s2"), ("zzz999", "s3")] {
            c.execute(
                "INSERT INTO dream_findings(repo_id, id, kind, subject, claim_hash, evidence, \
                 base_rank, status, first_seen_at_ms, last_seen_at_ms) \
                 VALUES('r',?1,'coverage_gap',?2,'ch','e',0.5,'open',1,1)",
                rusqlite::params![id, subj],
            )
            .unwrap();
        }
        assert_eq!(
            review_dream_finding(&c, "zzz", ReviewVerdict::Accept, 10).unwrap().id,
            "zzz999"
        );
        let none = review_dream_finding(&c, "qqq", ReviewVerdict::Accept, 10).unwrap_err();
        assert!(none.to_string().contains("no finding matches"), "got: {none}");
        let amb = review_dream_finding(&c, "abc", ReviewVerdict::Accept, 10).unwrap_err();
        assert!(amb.to_string().contains("ambiguous"), "got: {amb}");
    }

    #[test]
    fn review_prefix_that_also_hits_a_terminal_finding_is_ambiguous() {
        // A stale short prefix that matches BOTH an open and a resolved finding must be rejected as
        // ambiguous — never silently act on the open one (PR #440 review). The match considers ALL
        // findings, not just the reviewable set.
        let c = mem_db();
        set_repo(&c, "r");
        for (id, subj, status) in [("abcd11", "s1", "open"), ("abcd22", "s2", "resolved")] {
            c.execute(
                "INSERT INTO dream_findings(repo_id, id, kind, subject, claim_hash, evidence, \
                 base_rank, status, first_seen_at_ms, last_seen_at_ms) \
                 VALUES('r',?1,'coverage_gap',?2,'ch',?2,0.5,?3,1,1)",
                rusqlite::params![id, subj, status],
            )
            .unwrap();
        }
        let err = review_dream_finding(&c, "abcd", ReviewVerdict::Dismiss, 10).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "got: {err}");
        // the open finding must be untouched
        let (status, _) = status_and_reviewed(&c, "abcd11");
        assert_eq!(status, "open", "the open finding was not silently dismissed");
    }

    #[test]
    fn review_is_repo_scoped() {
        // A finding in repo-a is invisible to (and untouched by) a review run in repo-b.
        let c = mem_db();
        set_repo(&c, "repo-a");
        let id = seed_one_finding(&c, "x::F", 1000);
        set_repo(&c, "repo-b");
        let err = review_dream_finding(&c, &id, ReviewVerdict::Dismiss, 2000).unwrap_err();
        assert!(err.to_string().contains("no finding matches"), "got: {err}");
        set_repo(&c, "repo-a");
        assert_eq!(status_and_reviewed(&c, &id).0, "open", "repo-a's finding is untouched");
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
        sync(&c, &finding(), 1000, BASE_FINDING_KINDS).unwrap();
        set_repo(&c, "repo-b");
        // Pre-fix this is a PK violation, not an UPDATE: the scoped lookup misses repo-a's row and
        // the insert derives the same repo-blind id.
        sync(&c, &finding(), 2000, BASE_FINDING_KINDS).unwrap();

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
        let (o, r, s, res) = sync(&c, &f1, 1000, BASE_FINDING_KINDS).unwrap();
        assert_eq!((o, r, s, res), (1, 0, 0, 0), "first run opens");
        let (o, r, _, _) = sync(&c, &f1, 2000, BASE_FINDING_KINDS).unwrap();
        assert_eq!((o, r), (0, 1), "same finding refreshes, no duplicate");
        // material change -> supersede prior, open fresh
        let f2 = vec![DreamFinding {
            kind: "coverage_gap".into(),
            subject: "x::F".into(),
            evidence: "40 callers".into(),
            rank: 1.0,
        }];
        let (o, _, s, _) = sync(&c, &f2, 3000, BASE_FINDING_KINDS).unwrap();
        assert_eq!((o, s), (1, 1), "changed evidence supersedes + opens fresh");
        let opened: i64 = c
            .query_row("SELECT COUNT(*) FROM dream_findings WHERE status='open'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(opened, 1, "exactly one open finding for the (kind,subject)");
        // run with no findings -> the (kind,subject) resolves
        let (_, _, _, res) = sync(&c, &[], 4000, BASE_FINDING_KINDS).unwrap();
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
        sync(&c, &cg("10 callers", 0.1), 1000, BASE_FINDING_KINDS).unwrap();
        sync(&c, &cg("40 callers", 0.9), 2000, BASE_FINDING_KINDS).unwrap();
        sync(&c, &cg("10 callers", 0.1), 3000, BASE_FINDING_KINDS).unwrap();
        assert_eq!(count(&c, "open"), 1, "exactly one open row after flip-back");
        assert_eq!(
            open_evidence(&c),
            ("10 callers".into(), 0.1),
            "current (A) is open, not stale B"
        );
        // resolve, then reappear with the SAME evidence: must revive to open, not stay resolved.
        sync(&c, &[], 4000, BASE_FINDING_KINDS).unwrap();
        assert!(count(&c, "resolved") >= 1, "absent finding resolves");
        sync(&c, &cg("10 callers", 0.1), 5000, BASE_FINDING_KINDS).unwrap();
        assert_eq!(count(&c, "open"), 1, "reappearing resolved finding is revived to open");
    }

    // --- coverage_gap over the PageRank ranker (#261) -------------------------------------------

    fn intern(c: &Connection, value: &str) -> i64 {
        c.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", [value]).unwrap();
        c.query_row("SELECT id FROM name_strings WHERE value = ?1", [value], |r| r.get(0)).unwrap()
    }

    fn add_file(c: &Connection, path: &str, has_test_code: i64) -> i64 {
        c.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             commit_sha, worktree_id, has_test_code, repo_id) VALUES (?1, 'rust', 'source', 'h', \
             0, 0, '', '', ?2, 'r')",
            rusqlite::params![path, has_test_code],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn add_symbol(c: &Connection, file_id: i64, name: &str) -> i64 {
        let qn = intern(c, name);
        c.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte, \
             end_byte, start_line, end_line, is_test) VALUES (?1, 'rust', ?2, ?3, 'function', 0, \
             0, 0, 0, 0)",
            rusqlite::params![file_id, name, qn],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn add_call(c: &Connection, file_id: i64, from: (i64, &str), to: (i64, &str)) {
        c.execute(
            "INSERT INTO edges_data(source_file_id, from_symbol_id, to_symbol_id, from_name_id, \
             to_name_id, edge_kind_id, confidence_id, resolution_id) VALUES (?1, ?2, ?3, ?4, ?5, \
             ?6, ?7, ?7)",
            rusqlite::params![
                file_id,
                from.0,
                to.0,
                intern(c, from.1),
                intern(c, to.1),
                intern(c, "calls_name"),
                intern(c, "Exact"),
            ],
        )
        .unwrap();
    }

    #[test]
    fn coverage_gap_ranks_by_pagerank_and_excludes_covered_and_test_symbols() {
        let c = mem_db();
        set_repo(&c, "r");
        // `hub`, `covered_hub`, and `test_hub` are each called by `a` and `b` (equal in-degree), so
        // the raw-in-degree proxy could not separate them — but `covered_hub` has a memory and
        // `test_hub` lives in test infra, so only `hub` is a real gap. The PageRank ranker supplies
        // the importance; the Rust-side filter drops the covered / test rows.
        let src = add_file(&c, "src/lib.rs", 0);
        let test = add_file(&c, "src/lib_test.rs", 1);
        let hub = add_symbol(&c, src, "hub");
        let covered = add_symbol(&c, src, "covered_hub");
        let test_hub = add_symbol(&c, test, "test_hub");
        let a = add_symbol(&c, src, "a");
        let b = add_symbol(&c, src, "b");
        for (caller, cname) in [(a, "a"), (b, "b")] {
            add_call(&c, src, (caller, cname), (hub, "hub"));
            add_call(&c, src, (caller, cname), (covered, "covered_hub"));
            add_call(&c, test, (caller, cname), (test_hub, "test_hub"));
        }
        // A symbol-id binding covers `covered_hub` (repo-scoped). The binding's `memory_id` FKs to
        // `repo_memories`, so the memory row must exist first.
        c.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_at_ms, \
             updated_at_ms, source, memory_version, repo_id) VALUES ('m', 'Invariant', 't', 'b', \
             'high', 'active', 0, 0, 'agent', 'v1', 'r')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO repo_memory_bindings(memory_id, binding_kind, binding_id, symbol_id, \
             anchor_status, created_at_ms, repo_id) VALUES ('m', 'symbol', 'bnd', ?1, 'current', \
             0, 'r')",
            [covered],
        )
        .unwrap();

        let findings = coverage_gap(&c, 10).unwrap();
        let subjects: Vec<&str> = findings.iter().map(|f| f.subject.as_str()).collect();
        assert!(
            subjects.contains(&"src/lib.rs::hub"),
            "the uncovered, non-test load-bearing hub is a coverage gap: {subjects:?}"
        );
        assert!(
            !subjects.iter().any(|s| s.contains("covered_hub")),
            "a memory-covered symbol is excluded: {subjects:?}"
        );
        assert!(
            !subjects.iter().any(|s| s.contains("test_hub")),
            "a test-infra symbol is excluded: {subjects:?}"
        );
        assert!(
            !subjects.iter().any(|s| *s == "src/lib.rs::a" || *s == "src/lib.rs::b"),
            "source-only callers (no inbound edge) are NOT load-bearing and must be excluded: \
             {subjects:?}"
        );
        let hub_finding = findings.iter().find(|f| f.subject == "src/lib.rs::hub").unwrap();
        assert!(
            (hub_finding.rank - 1.0).abs() < 1e-9,
            "the top gap normalizes to rank 1.0: {}",
            hub_finding.rank
        );
        assert!(
            hub_finding.evidence.contains("PageRank"),
            "evidence cites the importance signal: {}",
            hub_finding.evidence
        );
    }

    #[test]
    fn sync_dedups_same_kind_subject_keeping_the_highest_ranked_claim() {
        let c = mem_db();
        set_repo(&c, "r");
        // Two findings, SAME (kind, subject), DIFFERENT evidence → different claim_hash → different
        // id. Before #261 the second's supersede swept the first WITHIN the run (last-writer drop).
        // Now the higher-ranked claim is the single open row and nothing is superseded.
        let findings = vec![
            DreamFinding {
                kind: "coverage_gap".into(),
                subject: "x::F".into(),
                evidence: "lo".into(),
                rank: 0.2,
            },
            DreamFinding {
                kind: "coverage_gap".into(),
                subject: "x::F".into(),
                evidence: "hi".into(),
                rank: 0.9,
            },
        ];
        let (opened, _refreshed, superseded, _resolved) =
            sync(&c, &findings, 100, BASE_FINDING_KINDS).unwrap();
        assert_eq!(opened, 1, "exactly one finding opens for the (kind, subject)");
        assert_eq!(superseded, 0, "no within-run supersede race");
        let rows: Vec<(String, String)> = c
            .prepare("SELECT status, evidence FROM dream_findings WHERE subject = 'x::F'")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), 1, "only the winning claim is persisted: {rows:?}");
        assert_eq!(rows[0].0, "open");
        assert_eq!(rows[0].1, "hi", "the higher-ranked claim wins the collision");
    }
}
