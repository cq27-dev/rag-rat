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

/// Parse the corpus profiles from an `oracle-corpora.toml` string. Fails closed on an empty result:
/// a typo'd table name (`[[corpora]]` instead of `[[corpus]]`) deserializes to zero corpora, which
/// would silently turn the CI matrix into a no-op and skip the health gate — so require at least
/// one.
pub fn load_corpora(toml_str: &str) -> anyhow::Result<Vec<CorpusProfile>> {
    let corpora = toml::from_str::<CorpusFile>(toml_str)?.corpus;
    anyhow::ensure!(
        !corpora.is_empty(),
        "oracle-corpora.toml has no [[corpus]] entries (wrong table name?)"
    );
    Ok(corpora)
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

    // Inline sample for the deterministic logic/health tests — kept IN-CRATE so they're packaging-
    // safe (no `include_str!` of a file outside the `rag-rat-core` package). The committed
    // `tools/oracle-corpora.toml` is validated separately at runtime (`committed_corpora_file`).
    const SAMPLE: &str = r#"
[[corpus]]
corpus_id = "rust-semver"
tier      = "small"
repo      = "https://github.com/dtolnay/semver"
rev       = "1.0.26"
tool      = "rust-analyzer"
prepare   = ["cargo fetch"]
bindings  = { rust = ["src"] }
health    = { expected_min_heuristic_edges = 50, expected_min_oracle_examined = 20, expected_max_skipped_drifted = 0, expected_min_symbols_with_moniker = 10, timeout_minutes = 8 }

[[corpus]]
corpus_id = "linux-kernel"
tier      = "heavy"
repo      = "https://github.com/torvalds/linux"
rev       = "v7.0"
tool      = "scip-clang"
prepare   = ["make defconfig"]
bindings  = { c = ["."] }
health    = { expected_min_heuristic_edges = 50000, expected_min_oracle_examined = 5000, expected_max_skipped_drifted = 0, expected_min_symbols_with_moniker = 1000, timeout_minutes = 120 }
"#;

    /// Path to the committed corpora file (repo `tools/`), relative to this crate's manifest.
    fn committed_corpora_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/oracle-corpora.toml")
    }

    #[test]
    fn load_rejects_empty_or_misnamed_corpus_array() {
        // A typo'd table name parses to zero corpora — must fail closed, not yield a no-op matrix.
        assert!(load_corpora("[[corpora]]\ncorpus_id = \"x\"\n").is_err());
        assert!(load_corpora("").is_err());
    }

    #[test]
    fn sample_loads_and_selects_by_id_and_tier() {
        let corpora = load_corpora(SAMPLE).unwrap();
        let small: Vec<&str> =
            corpora_for_tier(&corpora, "small").iter().map(|c| c.corpus_id.as_str()).collect();
        assert_eq!(small, ["rust-semver"]);
        let heavy: Vec<&str> =
            corpora_for_tier(&corpora, "heavy").iter().map(|c| c.corpus_id.as_str()).collect();
        assert_eq!(heavy, ["linux-kernel"]);
        assert!(corpus_by_id(&corpora, "nope").is_none());
    }

    /// Runtime-fs (not `include_str!`) so the crate stays packaging-safe: the committed file lives
    /// in the repo `tools/`, outside the published crate. Skips when absent (e.g. a packaged
    /// crate); when present it's the review tripwire — the committed corpora's ids/tiers +
    /// golden hashes.
    #[test]
    fn committed_corpora_file() {
        let Ok(toml_str) = std::fs::read_to_string(committed_corpora_path()) else {
            return; // not in a repo checkout (packaged crate) — nothing to validate.
        };
        let corpora = load_corpora(&toml_str).unwrap();
        let ids: Vec<&str> = corpora.iter().map(|c| c.corpus_id.as_str()).collect();
        assert_eq!(ids, [
            "rust-time",
            "c-libuv",
            "py-rich",
            "ts-rxjs",
            "cpp-yaml",
            "rust-cargo",
            "linux-kernel"
        ]);
        assert_eq!(corpus_by_id(&corpora, "py-rich").unwrap().tool, "scip-python");
        assert_eq!(corpus_by_id(&corpora, "ts-rxjs").unwrap().tool, "scip-typescript");
        assert_eq!(corpus_by_id(&corpora, "cpp-yaml").unwrap().tool, "scip-clang");

        // GOLDEN per-profile hashes: an edit to any corpus field changes its hash (and makes prior
        // reports incomparable) — recompute deliberately when intended.
        let hashes: Vec<(&str, String)> =
            corpora.iter().map(|c| (c.corpus_id.as_str(), c.hash())).collect();
        assert_eq!(hashes, vec![
            ("rust-time", GOLDEN_RUST_TIME.to_string()),
            ("c-libuv", GOLDEN_C_LIBUV.to_string()),
            ("py-rich", GOLDEN_PY_RICH.to_string()),
            ("ts-rxjs", GOLDEN_TS_RXJS.to_string()),
            ("cpp-yaml", GOLDEN_CPP_YAML.to_string()),
            ("rust-cargo", GOLDEN_RUST_CARGO.to_string()),
            ("linux-kernel", GOLDEN_LINUX_KERNEL.to_string()),
        ]);
    }

    // Pinned from the committed `tools/oracle-corpora.toml` (see `committed_corpora_file`).
    const GOLDEN_RUST_TIME: &str =
        "ba5a37328901f0b1964c51bc8cff3c729d07070a0ce0d61f9934ea7790ca6b64";
    const GOLDEN_C_LIBUV: &str = "b34ef742c7d4b5efc02bdb95a8457719be9a0dd5621485e11c7d42d2e534d965";
    const GOLDEN_PY_RICH: &str = "0a4a22be9817ff26b549119b098d23004e5a8b4d7817126e5084f9d730f1efda";
    const GOLDEN_TS_RXJS: &str = "492436f3827cbf9afe206b5feb03227388521db68a5965c1c3e904f68f0e1109";
    const GOLDEN_CPP_YAML: &str =
        "2b6d2ec7f00b34e330116bf1b93fd51416926ac3d30e24dac766f0f8fb910f58";
    const GOLDEN_RUST_CARGO: &str =
        "60452736340151a253001bb5c33cc83efa2a4ceabba4d42a227d3188d7761d79";
    const GOLDEN_LINUX_KERNEL: &str =
        "9b64c26095bbf672884e8ca3c8d93bab44444f702904a0c8b35cd9feccd80fb6";

    fn report_with(
        total_edges: u64,
        verdicts: u64,
        skipped_drifted: u64,
        monikers: u64,
    ) -> OracleResolutionReport {
        let profile = corpus_by_id(&load_corpora(SAMPLE).unwrap(), "rust-semver").unwrap().clone();
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
        let profile = corpus_by_id(&load_corpora(SAMPLE).unwrap(), "rust-semver").unwrap().clone();
        // edges 100>=50, verdicts 30>=20, drift 0<=0, monikers 15>=10.
        let report = report_with(100, 30, 0, 15);
        assert!(check_corpus_health(&profile, &report).is_empty());
    }

    #[test]
    fn each_threshold_violation_is_reported() {
        let profile = corpus_by_id(&load_corpora(SAMPLE).unwrap(), "rust-semver").unwrap().clone();
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
