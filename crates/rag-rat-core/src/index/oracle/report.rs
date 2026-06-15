//! The before/after **resolution report** contract (C0): a typed, versioned, provenance-stamped
//! view of one corpus oracle run, suitable for cross-run Δ comparison and CI surfacing.
//!
//! This module owns the *schema* — the JSON shape, its version, the corpus-profile identity + hash,
//! and the pure assembly of an [`OracleResolutionReport`] from a run's already-computed counts. It
//! deliberately holds no SQL and no I/O: the live computation (reading the index for the heuristic
//! "before" counts and the moniker tally) and the CLI/BMF surface are layered on top in C2, against
//! this locked contract.
//!
//! Why a frozen contract first: a Δ-vs-main comparison is only meaningful when both sides share the
//! same corpus, tool version, and report semantics. [`OracleResolutionReport`] therefore carries a
//! [`REPORT_SCHEMA_VERSION`] and a [`CorpusProfile::hash`] so a consumer can *refuse* to diff
//! mismatched reports rather than silently comparing apples to oranges.

use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::OracleReport;
use super::run::OracleEvalMetrics;

/// Schema version for [`OracleResolutionReport`]. **Bump on any change to the report's shape or to
/// the semantics of a field** (e.g. a denominator redefinition). A Δ consumer must refuse to diff
/// two reports whose `report_schema_version` differ — the numbers are no longer comparable.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Per-corpus health thresholds. The runner fails a corpus whose oracle run falls outside these
/// even when the underlying command exits 0 — catching "scip emitted almost nothing" / "venv didn't
/// resolve deps" / a silently-broken parse. Part of the profile, so it is covered by the profile
/// hash (a threshold change is a profile change).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorpusHealth {
    /// Minimum heuristic edges the index must carry (catches a broken parse / empty index).
    pub expected_min_heuristic_edges: u64,
    /// Minimum edge candidates the oracle must examine (catches "scip emitted almost nothing").
    pub expected_min_oracle_examined: u64,
    /// Maximum tolerated drifted-and-skipped candidates (default 0: any drift means SCIP and the
    /// index disagree on file content, so the run is untrustworthy and must fail).
    pub expected_max_skipped_drifted: u64,
    /// Minimum logical symbols that gained a moniker (catches "scip-python ran but resolved
    /// nothing", the venv-not-installed failure mode).
    pub expected_min_symbols_with_moniker: u64,
    /// Wall-clock budget for the whole corpus run; the runner wraps the run in this timeout.
    pub timeout_minutes: u64,
}

/// Identity + reproducibility inputs for one corpus oracle run. Two reports are comparable only
/// when their [`CorpusProfile::hash`] match; the hash covers every field here, so a changed repo /
/// rev / tool / prepare step / binding / threshold yields a different hash and an *incomparable*
/// verdict rather than a silently-wrong Δ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorpusProfile {
    /// Stable corpus id (e.g. `"py-requests"`).
    pub corpus_id: String,
    /// `"small"` (per-PR gate) or `"heavy"` (release → Bencher).
    pub tier: String,
    /// Source repo URL.
    pub repo: String,
    /// Pinned revision (commit SHA / tag).
    pub rev: String,
    /// SCIP tool id (matches [`OracleTool::as_db_str`]).
    pub tool: String,
    /// Per-language prerequisite commands run before indexing (e.g. `cargo fetch`, venv install).
    pub prepare: Vec<String>,
    /// Target bindings (`language -> [paths]`); a `BTreeMap` so iteration/serialization order is
    /// stable for hashing.
    pub bindings: BTreeMap<String, Vec<String>>,
    /// Per-corpus health thresholds.
    pub health: CorpusHealth,
}

impl CorpusProfile {
    /// A stable content hash over the whole profile. Deterministic: derived from canonical JSON
    /// (struct field order is fixed; `bindings` is a `BTreeMap`, so key order is sorted). Used as
    /// `corpus_profile_hash` in the report and as the comparability key for Δ-vs-baseline.
    pub fn hash(&self) -> String {
        // serde_json over a struct emits fields in declaration order and BTreeMap keys in sorted
        // order, so this byte stream is stable for a given profile value.
        let canonical = serde_json::to_vec(self).expect("CorpusProfile is always serializable");
        let digest = Sha256::digest(&canonical);
        hex_lower(&digest)
    }
}

/// Run-time provenance not derivable from the index: which tool build, which engine build, which
/// checkout, and the SCIP-vs-disk content fingerprint. Stamped into the report so a number can be
/// traced to exactly what produced it.
#[derive(Debug, Clone, Default)]
pub struct RunProvenance {
    /// Version string of the SCIP tool that produced the index.
    pub tool_version: String,
    /// `rag-rat` build commit (engine version that computed the report).
    pub rag_rat_commit: String,
    /// Active-checkout worktree id.
    pub worktree_id: String,
    /// SCIP-vs-disk content fingerprint captured the instant the tool exited (`production_sha`).
    pub production_sha: String,
}

/// Heuristic "before" vs compiler "after" edge resolution. `resolved_before` is the heuristic's
/// in-corpus resolutions (exact + same-file-name); `resolved_after` adds the oracle's recoveries
/// (`upgraded` in-corpus + `resolved_external`). Denominator is `total_edges` (edge candidates
/// carrying a callee range) for both rates — stated here so it is part of the contract, not an
/// implementation detail.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResolutionDelta {
    /// Edge candidates with a callee range — the denominator for both resolved rates.
    pub total_edges: u64,
    /// Heuristic in-corpus resolutions before the oracle (exact + same-file-name).
    pub resolved_before: u64,
    /// `resolved_before` + oracle upgrades (in-corpus) + resolved-external.
    pub resolved_after: u64,
    /// Low-confidence (`NameOnly`/`Ambiguous`) edges before the oracle.
    pub unresolved_before: u64,
}

impl ResolutionDelta {
    /// Resolved fraction before the oracle (`resolved_before / total_edges`). 0 edges → 1.0
    /// (vacuous), matching the engine's hit-rate convention.
    pub fn resolved_rate_before(&self) -> f64 {
        ratio(self.resolved_before, self.total_edges)
    }

    /// Resolved fraction after the oracle (`resolved_after / total_edges`).
    pub fn resolved_rate_after(&self) -> f64 {
        ratio(self.resolved_after, self.total_edges)
    }
}

/// The full, versioned, provenance-stamped before/after resolution report for one corpus run. This
/// is the JSON contract consumers (the Δ glue script, Bencher BMF derivation) read. rag-rat emits
/// it as JSON/TOON only — never Markdown (that is a glue concern).
#[derive(Debug, Clone, Serialize)]
pub struct OracleResolutionReport {
    // --- schema + identity envelope (every consumer checks these before comparing) ---
    pub report_schema_version: u32,
    pub corpus_profile_hash: String,
    pub corpus_id: String,
    pub tier: String,
    pub repo: String,
    pub rev: String,
    pub tool: String,
    pub tool_version: String,
    pub rag_rat_commit: String,
    pub worktree_id: String,
    pub production_sha: String,
    pub skipped_drifted: u64,
    // --- the measurement ---
    /// Before/after edge resolution counts.
    pub resolution: ResolutionDelta,
    /// Verdict transition counts (the after side): unresolved→upgraded,
    /// unresolved→resolved_external, exact→confirmed, exact→contradicted.
    pub upgraded: u64,
    pub resolved_external: u64,
    pub confirmed: u64,
    pub contradicted: u64,
    /// Symbols enriched with a SCIP moniker (`logical_symbol_monikers` rows).
    pub symbols_with_moniker: u64,
    /// Precision / recall / recovery metrics (denominator semantics on [`OracleEvalMetrics`]).
    pub metrics: OracleEvalMetrics,
}

impl OracleResolutionReport {
    /// Assemble the report from a run's already-computed pieces. Pure (no I/O): the caller supplies
    /// the profile, run provenance, the run's [`OracleReport`] (for run-only counts the side tables
    /// can't reproduce — `skipped_drifted`, the recall call counts), the diffed
    /// [`OracleEvalMetrics`], the heuristic "before" resolution counts, and the moniker tally. C2
    /// wires the live index reads that produce `before` and `symbols_with_moniker`; the fixture-DB
    /// tests there exercise that SQL. Here the mapping itself is locked and tested.
    pub fn assemble(
        profile: &CorpusProfile,
        provenance: &RunProvenance,
        run: &OracleReport,
        metrics: &OracleEvalMetrics,
        before: ResolutionBefore,
        symbols_with_moniker: u64,
    ) -> Self {
        let resolution = ResolutionDelta {
            total_edges: before.total_edges,
            resolved_before: before.resolved_in_corpus,
            // After = heuristic in-corpus resolutions + oracle in-corpus upgrades + external.
            resolved_after: before.resolved_in_corpus + run.upgraded + run.resolved_external,
            unresolved_before: before.unresolved,
        };
        Self {
            report_schema_version: REPORT_SCHEMA_VERSION,
            corpus_profile_hash: profile.hash(),
            corpus_id: profile.corpus_id.clone(),
            tier: profile.tier.clone(),
            repo: profile.repo.clone(),
            rev: profile.rev.clone(),
            tool: profile.tool.clone(),
            tool_version: provenance.tool_version.clone(),
            rag_rat_commit: provenance.rag_rat_commit.clone(),
            worktree_id: provenance.worktree_id.clone(),
            production_sha: provenance.production_sha.clone(),
            skipped_drifted: run.skipped_drifted,
            resolution,
            upgraded: run.upgraded,
            resolved_external: run.resolved_external,
            confirmed: run.confirmed,
            contradicted: run.contradicted,
            symbols_with_moniker,
            metrics: metrics.clone(),
        }
    }

    /// Whether two reports are comparable for a Δ: same schema version AND same corpus profile
    /// hash. A consumer that gets `false` must omit the delta (treat the baseline as
    /// unavailable) rather than subtract incomparable numbers.
    pub fn comparable_to(&self, baseline: &OracleResolutionReport) -> bool {
        self.report_schema_version == baseline.report_schema_version
            && self.corpus_profile_hash == baseline.corpus_profile_hash
    }
}

/// The heuristic "before" resolution counts, read from the index by C2. Kept as a small struct so
/// the report assembly stays pure and trivially testable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolutionBefore {
    /// Edge candidates with a callee range (the denominator).
    pub total_edges: u64,
    /// Heuristic in-corpus resolutions (exact + same-file-name).
    pub resolved_in_corpus: u64,
    /// Low-confidence (`NameOnly`/`Ambiguous`) edges.
    pub unresolved: u64,
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 { 1.0 } else { numerator as f64 / denominator as f64 }
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> CorpusProfile {
        let mut bindings = BTreeMap::new();
        bindings.insert("python".to_string(), vec!["src/requests".to_string()]);
        CorpusProfile {
            corpus_id: "py-requests".to_string(),
            tier: "small".to_string(),
            repo: "https://github.com/psf/requests".to_string(),
            rev: "abc123".to_string(),
            tool: "scip-python".to_string(),
            prepare: vec![
                "python -m venv .venv".to_string(),
                ".venv/bin/pip install .".to_string(),
            ],
            bindings,
            health: CorpusHealth {
                expected_min_heuristic_edges: 50,
                expected_min_oracle_examined: 20,
                expected_max_skipped_drifted: 0,
                expected_min_symbols_with_moniker: 10,
                timeout_minutes: 8,
            },
        }
    }

    fn provenance() -> RunProvenance {
        RunProvenance {
            tool_version: "scip-python 0.6.6".to_string(),
            rag_rat_commit: "deadbeef".to_string(),
            worktree_id: "wt-1".to_string(),
            production_sha: "sha-of-scip".to_string(),
        }
    }

    #[test]
    fn profile_hash_is_stable_golden() {
        // GOLDEN: pins the hash of `sample_profile()`. If a profile field's value or the hashing
        // scheme changes, this fails — making an accidental profile/semantics edit visible in
        // review (the whole point of `corpus_profile_hash`). Recompute deliberately when
        // intended.
        let h = sample_profile().hash();
        assert_eq!(h.len(), 64, "sha256 hex is 64 chars");
        assert_eq!(
            h, "f57f8f99349cb69f651cbd0d290260df21056b3574f2ad3796df74ccc19db577",
            "golden corpus_profile_hash drift — recompute intentionally if the profile changed"
        );
    }

    #[test]
    fn profile_hash_changes_when_any_field_changes() {
        let base = sample_profile().hash();
        let mut p = sample_profile();
        p.rev = "different".to_string();
        assert_ne!(base, p.hash(), "rev change must change the hash");
        let mut p = sample_profile();
        p.health.expected_min_symbols_with_moniker = 11;
        assert_ne!(base, p.hash(), "a health threshold change must change the hash");
        let mut p = sample_profile();
        p.bindings.insert("rust".to_string(), vec!["src".to_string()]);
        assert_ne!(base, p.hash(), "a bindings change must change the hash");
    }

    /// A run report carrying one of every verdict transition + monikers + drift, plus a
    /// before-count.
    fn report_for(
        before: ResolutionBefore,
        metrics: OracleEvalMetrics,
        monikers: u64,
    ) -> OracleResolutionReport {
        let run = OracleReport {
            edges_examined: 100,
            upgraded: 3,
            resolved_external: 2,
            confirmed: 7,
            contradicted: 1,
            oracle_only_calls: 4,
            covered_calls: 16,
            skipped_drifted: 5,
            skipped_local: 0,
            no_occurrence: 0,
            rows_written: 13,
            monikers_written: monikers,
            status: "Completed".to_string(),
        };
        OracleResolutionReport::assemble(
            &sample_profile(),
            &provenance(),
            &run,
            &metrics,
            before,
            monikers,
        )
    }

    #[test]
    fn assemble_maps_every_verdict_transition() {
        let before = ResolutionBefore { total_edges: 100, resolved_in_corpus: 60, unresolved: 40 };
        let metrics = OracleEvalMetrics {
            precision: 0.875,
            recall: 0.8,
            name_only_recovery_rate: 0.075,
            oracle_upgradeable_fraction: 0.125,
            confirmed: 7,
            contradicted: 1,
            upgraded: 3,
            resolved_external: 2,
            covered_calls: 16,
            oracle_only_calls: 4,
        };
        let r = report_for(before, metrics, 11);

        // unresolved → upgraded / resolved_external
        assert_eq!(r.upgraded, 3);
        assert_eq!(r.resolved_external, 2);
        // exact → confirmed / contradicted
        assert_eq!(r.confirmed, 7);
        assert_eq!(r.contradicted, 1);
        // oracle-only calls + skipped drifted (run-only, not reconstructable from side tables)
        assert_eq!(r.metrics.oracle_only_calls, 4);
        assert_eq!(r.skipped_drifted, 5);
        // symbols enriched with monikers
        assert_eq!(r.symbols_with_moniker, 11);
        // before/after resolution: after = before + upgraded + resolved_external
        assert_eq!(r.resolution.resolved_before, 60);
        assert_eq!(r.resolution.resolved_after, 60 + 3 + 2);
        assert_eq!(r.resolution.unresolved_before, 40);
        assert!((r.resolution.resolved_rate_before() - 0.60).abs() < 1e-9);
        assert!((r.resolution.resolved_rate_after() - 0.65).abs() < 1e-9);
    }

    #[test]
    fn report_carries_schema_and_provenance_envelope() {
        let before = ResolutionBefore { total_edges: 10, resolved_in_corpus: 5, unresolved: 5 };
        let r = report_for(before, OracleEvalMetrics::default(), 0);
        let v: serde_json::Value = serde_json::to_value(&r).expect("report serializes");
        for key in [
            "report_schema_version",
            "corpus_profile_hash",
            "corpus_id",
            "tier",
            "repo",
            "rev",
            "tool",
            "tool_version",
            "rag_rat_commit",
            "worktree_id",
            "production_sha",
            "skipped_drifted",
        ] {
            assert!(v.get(key).is_some(), "report JSON missing envelope key `{key}`");
        }
        assert_eq!(v["report_schema_version"], REPORT_SCHEMA_VERSION);
        assert_eq!(v["tool"], "scip-python");
        assert_eq!(v["tool_version"], "scip-python 0.6.6");
    }

    #[test]
    fn comparability_keys_on_version_and_profile_hash() {
        let before = ResolutionBefore { total_edges: 10, resolved_in_corpus: 5, unresolved: 5 };
        let a = report_for(before, OracleEvalMetrics::default(), 0);
        let b = report_for(before, OracleEvalMetrics::default(), 0);
        assert!(a.comparable_to(&b), "same profile + version → comparable");

        // A different profile hash → not comparable (Δ must be omitted).
        let mut incomparable = report_for(before, OracleEvalMetrics::default(), 0);
        incomparable.corpus_profile_hash = "different".to_string();
        assert!(!a.comparable_to(&incomparable));

        // A different schema version → not comparable.
        let mut newer = report_for(before, OracleEvalMetrics::default(), 0);
        newer.report_schema_version = REPORT_SCHEMA_VERSION + 1;
        assert!(!a.comparable_to(&newer));
    }
}
