//! Corpus profiles for the multi-language SCIP-oracle runner (#164, C1): load the declarative
//! `tools/oracle-corpora.toml`, select corpora by id / tier, and evaluate one run's report against
//! its corpus health thresholds — the **fail-on-nonsense gate** that catches "scip emitted almost
//! nothing" / "venv didn't resolve deps" / a broken parse even when the underlying command exits 0.
//!
//! Pure data + logic: parsing a provided string and comparing numbers. The CLI / shell runner do
//! the I/O (read the file, clone the repo, run the oracle) and act on the returned violations.

use serde::Deserialize;

use super::report::{CorpusProfile, OracleResolutionReport};

/// The `[[corpus]]` array shape of `oracle-corpora.toml`.
#[derive(Debug, Clone, Deserialize)]
struct CorpusFile {
    #[serde(default)]
    corpus: Vec<CorpusProfile>,
}

/// Parse the corpus profiles from an `oracle-corpora.toml` string.
pub fn load_corpora(toml_str: &str) -> anyhow::Result<Vec<CorpusProfile>> {
    Ok(toml::from_str::<CorpusFile>(toml_str)?.corpus)
}

/// The corpus with this id, if any.
pub fn corpus_by_id<'a>(corpora: &'a [CorpusProfile], id: &str) -> Option<&'a CorpusProfile> {
    corpora.iter().find(|corpus| corpus.corpus_id == id)
}

/// The corpora in a tier (`"small"` for the per-PR matrix, `"heavy"` for release/Bencher).
pub fn corpora_for_tier<'a>(corpora: &'a [CorpusProfile], tier: &str) -> Vec<&'a CorpusProfile> {
    corpora.iter().filter(|corpus| corpus.tier == tier).collect()
}

/// One failed health threshold. A non-empty list means the run is untrustworthy and the runner must
/// exit non-zero, even if the oracle command itself succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthViolation {
    /// The threshold name (stable, for machine/log use).
    pub check: &'static str,
    /// `actual vs expected` detail for the human-facing message.
    pub detail: String,
}

/// Evaluate a run's [`OracleResolutionReport`] against the corpus's health thresholds. Empty result
/// = healthy. `timeout_minutes` is enforced by the runner around the whole run (wall-clock), not
/// here — this checks only what the report carries.
pub fn check_corpus_health(
    profile: &CorpusProfile,
    report: &OracleResolutionReport,
) -> Vec<HealthViolation> {
    let health = &profile.health;
    let mut violations = Vec::new();

    // Edge candidates the heuristic produced — near-zero means a broken parse / empty index.
    let edges = report.resolution.total_edges;
    if edges < health.expected_min_heuristic_edges {
        violations.push(HealthViolation {
            check: "min_heuristic_edges",
            detail: format!("{edges} < {}", health.expected_min_heuristic_edges),
        });
    }

    // Verdicts the oracle produced (the edges it actually had an opinion on) — near-zero means the
    // SCIP index resolved almost nothing (e.g. a tool failure or, for Python, an unresolved venv).
    let examined =
        report.confirmed + report.contradicted + report.upgraded + report.resolved_external;
    if examined < health.expected_min_oracle_examined {
        violations.push(HealthViolation {
            check: "min_oracle_examined",
            detail: format!("{examined} < {}", health.expected_min_oracle_examined),
        });
    }

    // Drift means the SCIP and the index disagree on file content — the numbers are untrustworthy.
    if report.skipped_drifted > health.expected_max_skipped_drifted {
        violations.push(HealthViolation {
            check: "max_skipped_drifted",
            detail: format!("{} > {}", report.skipped_drifted, health.expected_max_skipped_drifted),
        });
    }

    // Symbols the SCIP enriched with a moniker — the canonical "did the tool resolve deps" signal.
    if report.symbols_with_moniker < health.expected_min_symbols_with_moniker {
        violations.push(HealthViolation {
            check: "min_symbols_with_moniker",
            detail: format!(
                "{} < {}",
                report.symbols_with_moniker, health.expected_min_symbols_with_moniker
            ),
        });
    }

    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::oracle::{
        OracleEvalMetrics, OracleReport, ResolutionBefore, ResolutionDelta, RunProvenance,
    };

    const CORPORA: &str = include_str!("../../../../../tools/oracle-corpora.toml");

    #[test]
    fn committed_corpora_load_with_expected_ids_and_tiers() {
        let corpora = load_corpora(CORPORA).unwrap();
        let ids: Vec<&str> = corpora.iter().map(|c| c.corpus_id.as_str()).collect();
        assert_eq!(ids, ["rust-semver", "c-cjson", "py-requests", "rust-cargo", "linux-kernel"]);

        let small: Vec<&str> =
            corpora_for_tier(&corpora, "small").iter().map(|c| c.corpus_id.as_str()).collect();
        assert_eq!(small, ["rust-semver", "c-cjson", "py-requests"]);
        let heavy: Vec<&str> =
            corpora_for_tier(&corpora, "heavy").iter().map(|c| c.corpus_id.as_str()).collect();
        assert_eq!(heavy, ["rust-cargo", "linux-kernel"]);

        let requests = corpus_by_id(&corpora, "py-requests").unwrap();
        assert_eq!(requests.tool, "scip-python");
        assert_eq!(requests.bindings.get("python"), Some(&vec!["src/requests".to_string()]));
    }

    #[test]
    fn committed_corpus_profile_hashes_are_pinned() {
        // GOLDEN: pin each committed profile's hash. An edit to any corpus field changes its hash
        // (and makes prior reports incomparable) — recompute deliberately when the change is
        // intended. This is the review tripwire for an accidental corpus/threshold edit.
        let corpora = load_corpora(CORPORA).unwrap();
        let hashes: Vec<(String, String)> =
            corpora.iter().map(|c| (c.corpus_id.clone(), c.hash())).collect();
        assert_eq!(hashes, vec![
            ("rust-semver".to_string(), GOLDEN_RUST_SEMVER.to_string()),
            ("c-cjson".to_string(), GOLDEN_C_CJSON.to_string()),
            ("py-requests".to_string(), GOLDEN_PY_REQUESTS.to_string()),
            ("rust-cargo".to_string(), GOLDEN_RUST_CARGO.to_string()),
            ("linux-kernel".to_string(), GOLDEN_LINUX_KERNEL.to_string()),
        ]);
    }

    // Filled in from the first test run (see `committed_corpus_profile_hashes_are_pinned`).
    const GOLDEN_RUST_SEMVER: &str =
        "7973c3d62bbea9fbbdf3d7a4380fb7d9ccdbf2659a3503c9267eb9f9f329bb97";
    const GOLDEN_C_CJSON: &str = "685a33345247c2ef310b7671fb9e2a55a7cb7c577fcd29fecac436ff0634c9dc";
    const GOLDEN_PY_REQUESTS: &str =
        "76abdb4592d1e1997f133fb5cd185d85b57851e8a82e085268e45d4d16c2c832";
    const GOLDEN_RUST_CARGO: &str =
        "60452736340151a253001bb5c33cc83efa2a4ceabba4d42a227d3188d7761d79";
    const GOLDEN_LINUX_KERNEL: &str =
        "5c075d0a7847a063e7a6a0439e108e804700d154223ce9eedbf74d98448d0f15";

    fn report_with(
        total_edges: u64,
        verdicts: u64,
        skipped_drifted: u64,
        monikers: u64,
    ) -> OracleResolutionReport {
        let profile = corpus_by_id(&load_corpora(CORPORA).unwrap(), "rust-semver").unwrap().clone();
        // Split `verdicts` across the four kinds arbitrarily (the gate sums them).
        let run = OracleReport {
            confirmed: verdicts,
            skipped_drifted,
            monikers_written: monikers,
            ..Default::default()
        };
        let before =
            ResolutionBefore { total_edges, resolved_in_corpus: 0, unresolved: total_edges };
        let mut report = OracleResolutionReport::assemble(
            &profile,
            &RunProvenance::default(),
            &run,
            &OracleEvalMetrics::default(),
            before,
            monikers,
        );
        // `assemble` derives `resolution` from `before`; ensure total_edges is what we set.
        report.resolution = ResolutionDelta { total_edges, ..report.resolution };
        report
    }

    #[test]
    fn healthy_report_has_no_violations() {
        let profile = corpus_by_id(&load_corpora(CORPORA).unwrap(), "rust-semver").unwrap().clone();
        // edges 100>=50, verdicts 30>=20, drift 0<=0, monikers 15>=10.
        let report = report_with(100, 30, 0, 15);
        assert!(check_corpus_health(&profile, &report).is_empty());
    }

    #[test]
    fn each_threshold_violation_is_reported() {
        let profile = corpus_by_id(&load_corpora(CORPORA).unwrap(), "rust-semver").unwrap().clone();
        // All four below/over threshold.
        let report = report_with(10, 5, 3, 2);
        let checks: Vec<&str> =
            check_corpus_health(&profile, &report).iter().map(|v| v.check).collect();
        assert_eq!(checks, [
            "min_heuristic_edges",
            "min_oracle_examined",
            "max_skipped_drifted",
            "min_symbols_with_moniker",
        ]);
    }
}
