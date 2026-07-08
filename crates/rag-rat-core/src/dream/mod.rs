//! Dream Mode (#122) — deterministic memory-maintenance worklist + verification pass (dream v2).
//!
//! Surfaces findings ABOUT memories into `dream_findings`; it NEVER mutates a `repo_memories` row
//! (dream mode proposes, a human/strong-agent confirms). The deterministic finding kinds are all
//! rate-independent (the spike's commit-replay evals showed change-based signals saturate on an
//! active repo; these check structure/truth, not churn):
//!   - `coverage_gap`       — load-bearing symbols (high call-graph in-degree) with no memory
//!     binding. (`findings`)
//!   - `stale_reference`    — a memory references a `.rs` path that no longer resolves against the
//!     index (suffix-aware, so prose shorthand like `src/lib.rs` still resolves). (`findings`)
//!   - `memory_unverifiable`— a memory whose bindings are all gone/absent AND none of whose
//!     identifiers resolve anywhere in the (whole-tree) index; decided deterministically here,
//!     never by a model. (`verify`, gated behind [`DreamOptions::verify`])
//!
//! Findings are identity-keyed `(kind, subject, claim_hash)`: a re-run with the same evidence
//! REFRESHES (no duplicate); a materially-changed finding SUPERSEDES the prior one; a finding the
//! run no longer reports is RESOLVED (`findings::sync`).
//!
//! The v2 verification pass (`verify`) also exposes [`verification_queue`] and [`evidence_pack`] —
//! the deterministic substrate the phase-B model verdict pass consumes (churn-skip queue + a
//! citation-checkable evidence pack), reading the `memory_reality` / `memory_summaries` sibling
//! tables (V046+). Those sibling tables hold DERIVED, regenerable data, which is what preserves
//! dream's "never mutates a `repo_memories` row" invariant even as verification lands.

mod compact;
mod failure;
mod findings;
mod model;
mod verdict;
mod verify;

// The phase-C model compaction pass: the `CompactPass` handle `dream_run_with_passes` consumes
// (borrowed model + budget), reusing the same `VerdictModel` trait as the verdict pass.
// The evidence-pack fingerprint + the two derived-row prompt versions, re-exported
// crate-internally so the surfacing hydrator gates a stale verdict/summary on the SAME
// identity the verify queue, divergence finder, and compaction queue use — a stale-evidence or
// stale-prompt row is dropped consistently at every read seam, not just by the producer.
pub(crate) use compact::COMPACT_PROMPT_VERSION;
pub use compact::CompactPass;
// Curated crate-facing surface (mod.rs is the index, not the junk drawer): the migration
// ladder and `register_repo` adoption re-derive persisted finding ids after re-stamping
// `repo_id`.
pub use findings::{ReviewVerdict, ReviewedFinding};
pub(crate) use findings::{rederive_finding_ids, review_dream_finding};
// The phase-B model verdict pass: the out-of-process verdict-model trait + its HTTP client
// (the CLI builds one from `[llm.dream.remote]`) and the `VerdictPass` handle
// `dream_run_with_passes` consumes.
pub use model::{HttpVerdictModel, VerdictModel, provision_verdict_model};
use rusqlite::Connection;
use serde::Serialize;
pub(crate) use verdict::PROMPT_VERSION as VERDICT_PROMPT_VERSION;
pub use verdict::VerdictPass;
// Dream v2 pass-0 substrate: the churn-skip verification queue + the deterministic,
// citation-checkable evidence pack. Public so the phase-B model verdict pass (and later the
// CLI flags) consume a stable interface; the pass-0 `memory_unverifiable` decider stays
// crate-internal (folded into `dream_run`).
pub use verify::{
    EvidencePack, FileExcerpt, IdentifierResolution, VerificationQueueEntry, VerificationReason,
    evidence_pack, verification_queue,
};
pub(crate) use verify::{checked_inputs_hash, note_content_hash};

/// A finding as PRODUCED by a finding kind (`coverage_gap` / `stale_reference` /
/// `memory_unverifiable` / `memory_divergence`) — the INPUT to [`findings::sync`], which derives
/// the stable id. It carries no id/status because neither exists until the sync writes the row.
#[derive(Debug, Clone, Serialize)]
pub struct DreamFinding {
    pub kind: String,
    pub subject: String,
    pub evidence: String,
    pub rank: f64,
}

/// A finding as EMITTED in the worklist (post-sync), carrying the stable `dream_findings.id` a
/// reviewer passes to `rag-rat dream <id> --accept|--dismiss|--reset`, plus its `status` (`open` in
/// the default worklist; `accepted`/`dismissed` also shown under `--all`).
#[derive(Debug, Clone, Serialize)]
pub struct WorklistFinding {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub evidence: String,
    pub rank: f64,
    pub status: String,
}

#[derive(Debug, Default, Serialize)]
pub struct DreamReport {
    pub findings: Vec<WorklistFinding>, /* the worklist (open, ranked; + accepted/dismissed
                                         * under --all) */
    pub opened: usize,
    pub refreshed: usize,
    pub superseded: usize,
    pub resolved: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct DreamOptions {
    pub now_ms: i64,
    pub limit: usize, // max coverage_gap findings
    /// Deterministic verification pass (dream v2 pass 0): emit `memory_unverifiable` findings for
    /// active memories whose bindings are all gone/absent and none of whose identifiers resolve.
    /// OFF by default so plain `rag-rat dream` stays byte-identical to the v1 run; the model
    /// verdict / compaction passes (phases B/C) layer on top of the same `verify` substrate.
    pub verify: bool,
    /// Also emit the human-reviewed findings (`accepted` / `dismissed`) in the worklist, not just
    /// `open` — the `--all` listing, so a reviewer can see (and `--reset`) a finding they
    /// previously dismissed. Off by default: the worklist is the "needs attention" (open) set.
    pub include_reviewed: bool,
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

/// Run the deterministic dream-mode pass: compute findings, sync the worklist, return the open
/// worklist (ranked) + lifecycle counts. Writes ONLY to `dream_findings`; never touches memories.
///
/// When [`DreamOptions::verify`] is set, ALSO emits the deterministic `memory_unverifiable`
/// findings (dream v2 pass 0) AND the `memory_divergence` findings derived from the STORED
/// `memory_reality` table (phase B) through the same identity-keyed sync — off by default, so a
/// plain run is byte-identical to v1. This function does NOT run the model itself (it takes no
/// model); the model verdict pass runs in [`dream_run_with_passes`] and writes `memory_reality`
/// BEFORE this reads it. With no model ever run, the `memory_reality` verdict rows are absent, so
/// the divergence set is empty — a harmless no-op.
pub fn dream_run(conn: &Connection, opts: DreamOptions) -> anyhow::Result<DreamReport> {
    let mut findings = findings::coverage_gap(conn, opts.limit)?;
    findings.extend(findings::stale_reference(conn)?);
    // The verification finding kinds emit over ALL active memories / ALL stored verdict rows
    // (stable, rate-independent populations, exactly like the other two kinds), so folding them
    // into the identity-keyed sync gives correct resolve semantics — a memory that becomes
    // verifiable again, or whose stored verdict flips back to `current`, has its finding
    // resolved. Deriving divergence from the STORED table (not this run's budget-capped fresh
    // checks) is what keeps a merely churn-SKIPPED memory's finding from being wrongly
    // resolved.
    // The resolve sweep is scoped to the kinds THIS run computed (see `findings::sync`): base kinds
    // always; the verify kinds only when `--verify` ran. Without this, a plain `dream` after a
    // `dream --verify` would resolve the open `memory_unverifiable` / `memory_divergence` findings
    // it never re-evaluated, silently dropping real worklist items until the next verify run.
    let mut resolve_kinds: Vec<&str> = findings::BASE_FINDING_KINDS.to_vec();
    if opts.verify {
        findings.extend(verify::unverifiable_findings(conn)?);
        findings.extend(verdict::divergence_findings(conn)?);
        resolve_kinds.extend_from_slice(findings::VERIFY_FINDING_KINDS);
    }
    let (opened, refreshed, superseded, resolved) =
        findings::sync(conn, &findings, opts.now_ms, &resolve_kinds)?;

    // emit the OPEN worklist from the store (post-sync); each finding's exposed rank is its
    // base_rank DECAYED by age since first_seen (effective_rank) — a stale unreviewed finding
    // sinks below fresh ones (the documented anti-rot, now real, not just base_rank DESC). Scoped
    // to the active repo so the emitted worklist never mixes in a sibling repo's open findings.
    let repo_clause = findings::dream_repo_scope_clause(&findings::dream_repo_scope(conn)?);
    // Default worklist = the 'open' (needs-attention) set. `--all` (include_reviewed) also surfaces
    // the human-reviewed 'accepted'/'dismissed' rows so a reviewer can see + `--reset` them.
    let status_filter = if opts.include_reviewed {
        "status IN ('open','accepted','dismissed')"
    } else {
        "status = 'open'"
    };
    let mut open: Vec<WorklistFinding> = conn
        .prepare(&format!(
            "SELECT id, kind, subject, evidence, base_rank, first_seen_at_ms, status FROM \
             dream_findings WHERE {status_filter}{repo_clause}"
        ))?
        .query_map([], |r| {
            let base: f64 = r.get(4)?;
            let first_seen: i64 = r.get(5)?;
            Ok(WorklistFinding {
                id: r.get(0)?,
                kind: r.get(1)?,
                subject: r.get(2)?,
                evidence: r.get(3)?,
                rank: effective_rank(base, first_seen, opts.now_ms),
                status: r.get(6)?,
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

/// [`dream_run`] plus the phase-B model verdict pass and the phase-C model compaction pass. Both
/// model passes run BEFORE the deterministic finding computation (the CLI builds each only when
/// `[llm.dream] enabled = true`):
///   - the VERDICT pass (gated on `verify` + a supplied [`VerdictPass`]) checks the budget-capped
///     churn-skip queue and writes accepted verdicts into `memory_reality`, which [`dream_run`]
///     then reads to derive `memory_divergence` findings (so a fresh `diverged` verdict opens a
///     finding in the same run);
///   - the COMPACTION pass (gated only on a supplied [`CompactPass`] — `--compact` is independent
///     of `--verify`) rewrites un-summarized memories into `memory_summaries`.
///
/// Both are budget-capped churn-skip passes over derived sibling tables; NEITHER writes a
/// `repo_memories` column. `None`/`None` is exactly [`dream_run`]: pass 0 only, still 100%
/// deterministic — so plain `rag-rat dream` stays byte-identical.
pub fn dream_run_with_passes(
    conn: &Connection,
    opts: DreamOptions,
    verdict_pass: Option<VerdictPass<'_>>,
    compact_pass: Option<CompactPass<'_>>,
) -> anyhow::Result<DreamReport> {
    // The verdict pass writes `memory_reality`, which `dream_run` then reads to derive
    // `memory_divergence` findings — so it runs before the finding computation.
    if opts.verify
        && let Some(pass) = verdict_pass
    {
        verdict::run_verdict_pass(conn, pass, opts.now_ms)?;
    }
    // Compaction is independent of the verify findings (it writes `memory_summaries`, read only by
    // the surfacing layer), so it is gated on its own supplied pass, not on `opts.verify`.
    if let Some(pass) = compact_pass {
        compact::run_compact_pass(conn, pass, opts.now_ms)?;
    }
    dream_run(conn, opts)
}

/// Whether the model passes would call the model AT ALL this run — the zero-work guard for
/// EPHEMERAL `[llm.dream.remote]`. A fully churn-skipped (or all-uncitable) repo returns `false`,
/// so `rag-rat dream --verify/--compact` never cold-starts a paid GPU box that would then do zero
/// inference — mirroring the embedding path's "never provision for zero work" rule.
///
/// `verify` is CITABILITY-aware, not just queue-emptiness: `run_verdict_pass` records an UNCITABLE
/// entry (prose-only / all-`NOT FOUND`, no excerpts) as a terminal row WITHOUT calling the model,
/// so a queue whose every entry is uncitable is zero model work. The guard therefore builds each
/// queued entry's evidence pack and returns `true` on the FIRST citable one — matching exactly what
/// reaches the model. `compact` has no uncitable short-circuit (every queued memory is summarized),
/// so a non-empty compaction queue IS model work. Runs once before provisioning, budget-capped, and
/// short-circuits — the paid box it gates makes the extra pack builds worth it.
pub fn model_work_pending(
    conn: &Connection,
    opts: DreamOptions,
    budget: usize,
    verify: bool,
    compact: bool,
    model_id: &str,
) -> anyhow::Result<bool> {
    if verify {
        let scope = crate::index::schema::periphery_repo_scope(conn, "repo_memories")?;
        let repo_id = scope.as_deref().unwrap_or("__unassigned__");
        let mut considered = 0usize;
        for entry in verify::verification_queue(conn, opts.now_ms, usize::MAX)? {
            if considered >= budget {
                break;
            }
            let inputs_hash = verify::checked_inputs_hash(conn, &entry.memory_id, &scope)?;
            let content_hash = verify::note_content_hash(&entry.title, &entry.body);
            let failure_stamp = failure::FailureStamp {
                memory_id: &entry.memory_id,
                repo_id,
                pass: failure::DreamModelPass::Verify,
                content_hash: &content_hash,
                checked_inputs_hash: Some(&inputs_hash),
                prompt_version: verdict::PROMPT_VERSION,
                model_id,
            };
            if failure::blocking_failure_is_current(conn, &failure_stamp)? {
                continue;
            }
            considered += 1;
            if verify::evidence_pack(conn, &entry.memory_id)?.is_citable() {
                return Ok(true);
            }
        }
    }
    if compact && compact::compaction_pending(conn, budget, model_id)? {
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
pub(super) mod tests {
    use rusqlite::Connection;

    use super::*;

    /// A fresh in-memory index at the current schema — the shared fixture for the dream unit tests
    /// in this module and its `findings` / `verify` siblings.
    pub(super) fn mem_db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&c).unwrap();
        c
    }

    /// Point the connection's periphery scope at `repo_id` — mirrors the scope-context write the
    /// production open installs (and `multi_repo_scope::a5_set_active_repo`).
    pub(super) fn set_repo(c: &Connection, repo_id: &str) {
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

    #[test]
    fn worklist_surfaces_ids_and_hides_dismissed_unless_include_reviewed() {
        // memory_divergence is a VERIFY finding kind: a plain dream_run(verify=false) neither
        // recomputes nor resolves it (resolve is kind-scoped to the kinds the run computed), so
        // these synthetic rows survive the run — isolating the emit's id surfacing + status
        // filter.
        let c = mem_db();
        set_repo(&c, "r");
        for (id, subj, status) in [("d0001", "mA", "open"), ("d0002", "mB", "dismissed")] {
            c.execute(
                "INSERT INTO dream_findings(repo_id, id, kind, subject, claim_hash, evidence, \
                 base_rank, status, first_seen_at_ms, last_seen_at_ms) \
                 VALUES('r',?1,'memory_divergence',?2,'ch',?2,0.6,?3,1,1)",
                rusqlite::params![id, subj, status],
            )
            .unwrap();
        }
        let default = dream_run(&c, DreamOptions {
            now_ms: 10,
            limit: 10,
            verify: false,
            include_reviewed: false,
        })
        .unwrap();
        assert_eq!(
            default.findings.iter().map(|f| f.subject.as_str()).collect::<Vec<_>>(),
            vec!["mA"],
            "the default worklist shows only open findings",
        );
        assert_eq!(default.findings[0].id, "d0001", "the stable finding id is surfaced");
        assert_eq!(default.findings[0].status, "open");

        let all = dream_run(&c, DreamOptions {
            now_ms: 10,
            limit: 10,
            verify: false,
            include_reviewed: true,
        })
        .unwrap();
        let mut pairs: Vec<(&str, &str)> =
            all.findings.iter().map(|f| (f.subject.as_str(), f.status.as_str())).collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![("mA", "open"), ("mB", "dismissed")],
            "--all also surfaces the human-reviewed (dismissed) finding, with its status",
        );
    }

    #[test]
    fn dream_run_never_mutates_any_memory_column() {
        let c = mem_db();
        // seed a memory whose body references a gone path so stale_reference is forced to READ it
        // and emit a finding keyed on this memory (makes the assertion non-vacuous). The memory has
        // NO bindings and its identifiers do not resolve, so the verify pass ALSO emits a
        // memory_unverifiable finding for it — covering the v2 pass's reads under the invariant.
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
        // Run with the verification pass ON so the new pass-0 reads/writes are covered by the
        // invariant (memory_reality / memory_summaries writes are ALLOWED; repo_memories must stay
        // byte-identical).
        let report = dream_run(&c, DreamOptions {
            now_ms: 1000,
            limit: 10,
            verify: true,
            include_reviewed: false,
        })
        .unwrap();
        assert_eq!(before, snap(&c), "dream_run must leave EVERY repo_memories column unchanged");
        assert!(
            report.findings.iter().any(|f| f.kind == "stale_reference" && f.subject == "m1"),
            "non-vacuous: the seeded memory produced a stale_reference finding"
        );
        assert!(
            report.findings.iter().any(|f| f.kind == "memory_unverifiable" && f.subject == "m1"),
            "non-vacuous: the verify pass produced a memory_unverifiable finding"
        );
    }

    #[test]
    fn plain_run_stays_byte_identical_without_the_verify_flag() {
        let c = mem_db();
        c.execute(
            "INSERT INTO repo_memories(id, kind, title, body, confidence, status, created_by, \
             created_at_ms, updated_at_ms, source, memory_version) VALUES \
             ('m1','Invariant','t','a note with no \
             bindings','high','active','agent',1,1,'agent','v1')",
            [],
        )
        .unwrap();
        // verify OFF: no memory_unverifiable finding is emitted (v1-identical worklist).
        let report = dream_run(&c, DreamOptions {
            now_ms: 1000,
            limit: 10,
            verify: false,
            include_reviewed: false,
        })
        .unwrap();
        assert!(
            !report.findings.iter().any(|f| f.kind == "memory_unverifiable"),
            "the verify pass must be dormant when DreamOptions::verify is false"
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
