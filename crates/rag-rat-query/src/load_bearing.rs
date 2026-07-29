//! Scoped weighted fan-in — the THIRD, LOCAL importance scale (#108 / importance-ux-overhaul Phase
//! 3). A cheap, per-result-symbol structural signal that rides along in tools agents already call
//! (`impact_surface` neighbors, `semantic_search` / `symbol_lookup` hits) — NOT global PageRank.
//!
//! ```text
//! scoped_weighted_fan_in(symbol) =
//!     Σ  edge_weight(kind) × confidence_factor(confidence) × oracle_factor(verdict?)
//!     over the symbol's IN-edges VISIBLE IN THE ACTIVE files SCOPE
//! ```
//!
//! It reuses the `query::pagerank` weight tables ([`edge_weight`] / [`confidence_factor`] /
//! [`COMPILER_FACTOR`]) verbatim — single source of truth — but is a *different metric on a
//! different scope*: a local weighted in-degree, never the global/transitive PageRank flow. The
//! result is LABELED ([`ImportanceEnrichment::label`] = `"local structural load"`) so an agent can
//! never mistake it for a PageRank score (the spec's "three scales" anti-pattern). The PageRank
//! scales live in `important_symbols`; this one rides along.
//!
//! Why no whole-graph pass: the in-degree is computed only for the symbols a result already holds,
//! one bounded query each, so enrichment costs nothing on a result with no symbols and stays cheap
//! on a handful. Oracle data is fetched ONCE per enrichment call and threaded through (no
//! per-symbol verdict scan); when no oracle run exists for the active checkout the factor is `1.0`
//! (pure heuristic) and costs nothing — the caller gates on a run existing.

use std::collections::{BTreeSet, HashMap};

use rusqlite::{Connection, params_from_iter};
use serde::Serialize;

use crate::pagerank::{COMPILER_FACTOR, EdgeOracleEffect, confidence_factor, edge_weight};

/// The load-bearing bucket for a symbol's scoped weighted fan-in. Buckets travel where raw floats
/// do not: the raw `score` drifts with graph density, language, parser coverage, and scope size, so
/// it is kept only for sorting — the BUCKET is the stable, comparable signal an agent reads.
///
/// Thresholds (on the raw weighted in-degree) are HEURISTIC and TUNABLE. They are absolute (not a
/// percentile — a percentile pass would mean ranking the whole graph per call, which defeats the
/// point of a local enrichment). Calibration intuition: a `calls_name` edge at `Exact` confidence
/// contributes `1.0 × 1.0 = 1.0`; a `NameOnly` guess `1.0 × 0.4 = 0.4`; an `imports` edge `0.3`.
/// So `Low` ≈ a symbol nothing-much depends on, `Medium` ≈ a few real callers, `High` ≈ a
/// genuinely depended-upon helper, `Critical` ≈ a hub many things call. See [`Self::THRESHOLDS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LoadBearingBucket {
    Low,
    Medium,
    High,
    Critical,
}

impl LoadBearingBucket {
    /// `(lower_bound, bucket)` pairs, ascending. A score `>=` a row's bound and `<` the next row's
    /// falls in that bucket; `Low` is the open floor. HEURISTIC + TUNABLE (see the type doc).
    const THRESHOLDS: [(f64, LoadBearingBucket); 3] = [
        (2.0, LoadBearingBucket::Medium),
        (5.0, LoadBearingBucket::High),
        (12.0, LoadBearingBucket::Critical),
    ];

    /// Bucket a raw scoped-weighted-fan-in score against [`Self::THRESHOLDS`].
    pub fn from_score(score: f64) -> Self {
        let mut bucket = LoadBearingBucket::Low;
        for (lower, candidate) in Self::THRESHOLDS {
            if score >= lower {
                bucket = candidate;
            }
        }
        bucket
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

impl Serialize for LoadBearingBucket {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The best SCIP tier observed among a symbol's in-edges, when an oracle run covers any of them.
/// Today the only non-heuristic tier is `compiler` (a confirmed/upgraded in-edge), so the enum has
/// one variant — it exists so the field is a self-describing label (`oracle_tier: "compiler"`)
/// rather than a bare bool, and so a later backend can add tiers without changing the field shape.
// `rename_all = "snake_case"` so the serialized label matches `as_str()` and the documented
// contract (`"compiler"`), not the variant's `"Compiler"` — MCP/CLI consumers key off the lower
// form. (#142 review)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleTier {
    /// At least one in-edge was confirmed/upgraded by a SCIP oracle — compiler-grade.
    Compiler,
}

impl OracleTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compiler => "compiler",
        }
    }
}

/// The local structural-load signal attached to a result symbol. LABELED so it is never confused
/// with PageRank: `label` / `signal` are fixed contract strings naming the scale.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ImportanceEnrichment {
    /// EXACTLY `"local structural load"` — the agent-facing name of this (third) scale. Never
    /// `"global importance"` or anything implying comparability with PageRank.
    pub label: &'static str,
    /// EXACTLY `"scoped weighted fan-in"` — how the score was computed.
    pub signal: &'static str,
    /// Raw weighted in-degree, for sorting. Drifts with scope/language/density — prefer `bucket`
    /// for any comparison.
    pub score: f64,
    pub bucket: LoadBearingBucket,
    /// The best SCIP tier among the in-edges, if an oracle run covered any of them; else `None`
    /// (pure heuristic).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oracle_tier: Option<OracleTier>,
}

impl ImportanceEnrichment {
    const LABEL: &'static str = "local structural load";
    const SIGNAL: &'static str = "scoped weighted fan-in";
}

/// Pre-fetched oracle data for an enrichment call: the per-`edge_id` [`EdgeOracleEffect`] map,
/// built ONCE by the caller (`IndexDatabase`) and reused across every symbol in the result — never
/// a verdict scan per symbol. `None` = no oracle run for the active checkout, so the heuristic path
/// runs with `oracle_factor = 1.0` and no oracle lookups happen at all (gated upstream).
pub struct OracleContext<'a> {
    pub effects: Option<&'a HashMap<i64, EdgeOracleEffect>>,
}

impl<'a> OracleContext<'a> {
    /// A heuristic-only context (no oracle run) — the dominant path.
    pub fn none() -> Self {
        Self { effects: None }
    }
}

/// One in-edge's contribution to the scoped weighted fan-in, after applying its oracle verdict (if
/// any). A `None` means the edge was DROPPED (a contradicted / resolved-external phantom — it
/// doesn't count toward load), matching how `important_symbols` drops the same verdicts.
struct InEdgeContribution {
    weight: f64,
    /// Whether this edge counts at the compiler tier (confirm/upgrade) — used to set
    /// `oracle_tier`.
    compiler: bool,
}

/// Apply an in-edge's oracle verdict to its heuristic weight, mirroring
/// `pagerank::important_symbols`'s per-edge logic so the two scales agree on what a verdict does.
/// `to_symbol_id` is the symbol being scored (S) — the heuristic target the in-edge query selected
/// on (`WHERE d.to_symbol_id = S`):
/// - `Drop` (contradict / resolved-external) → the edge is a phantom, it contributes nothing;
/// - `Confirm` → the compiler confirmed S as the target; the in-edge counts at the compiler tier;
/// - `Retarget(t)` where `t == S` → the rare case where the oracle resolved the edge back onto S
///   itself; counts at the compiler tier, same as `Confirm`;
/// - `Retarget(t)` where `t != S` → **DROP** (`None`): the oracle resolved the edge to `t`, NOT S.
///   The edge makes the caller depend on `t`, not S — the heuristic mis-attributed it to S, so it
///   is not an in-edge of S and must not inflate S's fan-in (the global graph build MOVES such an
///   edge to its resolved target, the behavior `important_symbols` produces);
/// - no verdict → `edge_weight × confidence_factor` (pure heuristic).
fn in_edge_contribution(
    kind: &str,
    confidence: &str,
    edge_id: i64,
    to_symbol_id: i64,
    oracle: &OracleContext<'_>,
) -> Option<InEdgeContribution> {
    match oracle.effects.and_then(|m| m.get(&edge_id)) {
        Some(EdgeOracleEffect::Drop) => None,
        // A retarget onto a DIFFERENT symbol is not an in-edge of S — drop it (no false inflation).
        Some(EdgeOracleEffect::Retarget(t)) if *t != to_symbol_id => None,
        // `Confirm`, or a `Retarget` that lands back on S itself: confirmed in-edge of S.
        Some(EdgeOracleEffect::Confirm) | Some(EdgeOracleEffect::Retarget(_)) =>
            Some(InEdgeContribution { weight: edge_weight(kind) * COMPILER_FACTOR, compiler: true }),
        None => Some(InEdgeContribution {
            weight: edge_weight(kind) * confidence_factor(confidence),
            compiler: false,
        }),
    }
}

/// Compute the scoped weighted fan-in for a single symbol: the sum of its in-edges' weights, where
/// in-edges are read THROUGH the active per-connection `files` scope view (never `main.*`), so the
/// score reflects only dependencies visible in the active checkout. Returns `None` when the symbol
/// has no visible in-edge contribution at all (so the caller can leave `importance` absent rather
/// than emit a meaningless `score: 0`).
///
/// ## Accepted oracle limitation (keyed on the HEURISTIC target)
/// The in-edge query is keyed on the heuristic target (`WHERE d.to_symbol_id = S`). Oracle
/// retargets are honored to EXCLUDE mis-attributed in-edges — an edge whose heuristic target is S
/// but which the oracle `Retarget`s onto some OTHER symbol is dropped (see
/// [`in_edge_contribution`]) so it never falsely inflates S's fan-in. They are NOT used to chase IN
/// edges retargeted ONTO S: an edge whose heuristic target is some other symbol but which the
/// oracle resolves to S is not selected by this query, so S's fan-in can slightly UNDERCOUNT
/// oracle-retargeted-in dependencies. Picking those up would require scanning beyond the symbol's
/// own in-edges (a reverse index), which we deliberately do NOT build — the signal is advisory and
/// bucketed, and must stay a cheap per-symbol local query. Removing the false inflation is the
/// correctness win; the residual undercount is bounded and acceptable for a bucketed advisory
/// signal.
pub fn scoped_weighted_fan_in(
    conn: &Connection,
    to_symbol_id: i64,
    oracle: &OracleContext<'_>,
) -> anyhow::Result<Option<ImportanceEnrichment>> {
    // #89: in-edges are joined THROUGH the per-connection scoped `files` view
    // (`JOIN files ON files.id = edges_data.source_file_id`), NOT raw `main.edges`/`main.files`, so
    // the same symbol identity scores DIFFERENTLY per active scope and a foreign scope's edges
    // never leak in. `name_strings` resolves the kind/confidence ids to names for the weight
    // tables; `d.id` keys the optional SCIP-oracle effect lookup.
    let mut stmt = conn.prepare(
        "SELECT d.id, ek.value, cf.value
         FROM edges_data d
         JOIN files ON files.id = d.source_file_id
         JOIN name_strings ek ON ek.id = d.edge_kind_id
         JOIN name_strings cf ON cf.id = d.confidence_id
         WHERE d.to_symbol_id = ?1
           -- Materialized visibility (#734): excludes suppressed candidates and the internal
           -- dispatch FACT rows (#200 — the handle fact duplicates the dispatcher's existing
           -- calls_name, so counting it would double-weight the handler). The synthesized
           -- `dispatches` edge IS counted.
           AND d.hidden = 0",
    )?;
    let rows = stmt
        .query_map([to_symbol_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut score = 0.0_f64;
    let mut counted = false;
    let mut compiler = false;
    for (edge_id, kind, confidence) in &rows {
        let Some(contribution) =
            in_edge_contribution(kind, confidence, *edge_id, to_symbol_id, oracle)
        else {
            continue;
        };
        score += contribution.weight;
        counted = true;
        compiler |= contribution.compiler;
    }
    if !counted {
        return Ok(None);
    }
    Ok(Some(ImportanceEnrichment {
        label: ImportanceEnrichment::LABEL,
        signal: ImportanceEnrichment::SIGNAL,
        score: crate::round_score(score),
        bucket: LoadBearingBucket::from_score(score),
        oracle_tier: compiler.then_some(OracleTier::Compiler),
    }))
}

/// Batched sibling of [`scoped_weighted_fan_in`]. It preserves the same active-scope, visibility,
/// weighting, and oracle semantics while loading requested symbols' in-edges in bounded queries.
pub fn scoped_weighted_fan_in_many(
    conn: &Connection,
    symbol_ids: &[i64],
    oracle: &OracleContext<'_>,
) -> anyhow::Result<HashMap<i64, ImportanceEnrichment>> {
    if symbol_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let unique_symbol_ids = symbol_ids.iter().copied().collect::<BTreeSet<_>>();
    let unique_symbol_ids = unique_symbol_ids.into_iter().collect::<Vec<_>>();
    let mut scores: HashMap<i64, (f64, bool)> = HashMap::new();
    for symbol_chunk in unique_symbol_ids.chunks(900) {
        let marks = symbol_chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT d.id, d.to_symbol_id, ek.value, cf.value
             FROM edges_data d
             JOIN files ON files.id = d.source_file_id
             JOIN name_strings ek ON ek.id = d.edge_kind_id
             JOIN name_strings cf ON cf.id = d.confidence_id
             WHERE d.to_symbol_id IN ({marks}) AND d.hidden = 0"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(symbol_chunk), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (edge_id, symbol_id, kind, confidence) = row?;
            let Some(contribution) =
                in_edge_contribution(&kind, &confidence, edge_id, symbol_id, oracle)
            else {
                continue;
            };
            let score = scores.entry(symbol_id).or_default();
            score.0 += contribution.weight;
            score.1 |= contribution.compiler;
        }
    }
    Ok(scores
        .into_iter()
        .map(|(symbol_id, (score, compiler))| {
            (symbol_id, ImportanceEnrichment {
                label: ImportanceEnrichment::LABEL,
                signal: ImportanceEnrichment::SIGNAL,
                score: crate::round_score(score),
                bucket: LoadBearingBucket::from_score(score),
                oracle_tier: compiler.then_some(OracleTier::Compiler),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use rag_rat_core::index::install_scope_view;
    use rag_rat_db::schema;
    use rusqlite::params;

    use super::*;

    /// #142 review: the wire label for the oracle tier must match `as_str()` and the documented
    /// contract (`"compiler"`), not the variant name `"Compiler"` — MCP/CLI consumers key off the
    /// lower form.
    #[test]
    fn oracle_tier_serializes_as_lowercase_compiler() {
        assert_eq!(OracleTier::Compiler.as_str(), "compiler");
        assert_eq!(
            serde_json::to_value(OracleTier::Compiler).unwrap(),
            serde_json::json!("compiler")
        );
        // …and through the enrichment that actually carries it on the wire.
        let enrichment = ImportanceEnrichment {
            label: "local structural load",
            signal: "scoped weighted fan-in",
            score: 1.0,
            bucket: LoadBearingBucket::High,
            oracle_tier: Some(OracleTier::Compiler),
        };
        let value = serde_json::to_value(&enrichment).unwrap();
        assert_eq!(value["oracle_tier"], serde_json::json!("compiler"));
    }

    /// A connection with the schema applied and a single active scope (committed `sha`, no
    /// worktree), ready for fan-in queries against the `files` view.
    fn scoped_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &rag_rat_core::index::migration_hooks()).unwrap();
        conn
    }

    /// Insert a file row for `(commit_sha, worktree_id)` and return its id.
    fn add_file(conn: &Connection, path: &str, commit: &str, worktree: &str) -> i64 {
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               commit_sha, worktree_id)
             VALUES (?1, 'rust', 'source', 'h', 0, 0, ?2, ?3)",
            params![path, commit, worktree],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Insert a symbol in `file_id`, returning its id.
    fn add_symbol(conn: &Connection, file_id: i64, name: &str, qualified: &str) -> i64 {
        // #224: qualified_name interned into name_strings.
        conn.execute("INSERT OR IGNORE INTO name_strings(value) VALUES (?1)", params![qualified])
            .unwrap();
        conn.execute(
            "INSERT INTO symbols(file_id, language, name, qualified_name_id, kind, start_byte,
                                 end_byte, signature, docs)
             VALUES (?1, 'rust', ?2, (SELECT id FROM name_strings WHERE value = ?3),
                     'function', 0, 10, NULL, NULL)",
            params![file_id, name, qualified],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// Insert an in-edge `from → to` of `kind` at `confidence` in `source_file_id`, returning the
    /// edge id (for keying an oracle-effects map).
    fn add_edge(
        conn: &Connection,
        source_file_id: i64,
        from: i64,
        to: i64,
        kind: &str,
        confidence: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO edges(source_file_id, from_symbol_id, to_symbol_id, to_name,
                               target_qualified_name, edge_kind, confidence)
             VALUES (?1, ?2, ?3, 'x', 'a::x', ?4, ?5)",
            params![source_file_id, from, to, kind, confidence],
        )
        .unwrap();
        // `edges` is an INSTEAD OF INSERT view over `edges_data`; `last_insert_rowid()` after the
        // trigger reflects an inner `name_strings` insert, not the edge row — read the real
        // `edges_data.id` so the oracle-effects map keys on the value `scoped_weighted_fan_in`'s
        // `d.id` query returns.
        conn.query_row("SELECT MAX(id) FROM edges_data", [], |row| row.get::<_, i64>(0)).unwrap()
    }

    fn fan_in(conn: &Connection, to_symbol_id: i64) -> Option<ImportanceEnrichment> {
        scoped_weighted_fan_in(conn, to_symbol_id, &OracleContext::none()).unwrap()
    }

    #[test]
    fn bucketing_thresholds() {
        assert_eq!(LoadBearingBucket::from_score(0.0), LoadBearingBucket::Low);
        assert_eq!(LoadBearingBucket::from_score(1.99), LoadBearingBucket::Low);
        assert_eq!(LoadBearingBucket::from_score(2.0), LoadBearingBucket::Medium);
        assert_eq!(LoadBearingBucket::from_score(4.99), LoadBearingBucket::Medium);
        assert_eq!(LoadBearingBucket::from_score(5.0), LoadBearingBucket::High);
        assert_eq!(LoadBearingBucket::from_score(11.99), LoadBearingBucket::High);
        assert_eq!(LoadBearingBucket::from_score(12.0), LoadBearingBucket::Critical);
        assert_eq!(LoadBearingBucket::from_score(1000.0), LoadBearingBucket::Critical);
    }

    #[test]
    fn more_and_heavier_in_edges_score_higher() {
        let conn = scoped_conn();
        let f = add_file(&conn, "a.rs", "c", "");
        let hub = add_symbol(&conn, f, "hub", "a::hub");
        let weak = add_symbol(&conn, f, "weak", "a::weak");
        let mut callers = Vec::new();
        for i in 0..6 {
            callers.push(add_symbol(&conn, f, &format!("c{i}"), &format!("a::c{i}")));
        }
        // hub: six exact `calls_name` in-edges (6 × 1.0 × 1.0 = 6.0).
        for &c in &callers {
            add_edge(&conn, f, c, hub, "calls_name", "Exact");
        }
        // weak: one name-only call in-edge (1 × 1.0 × 0.4 = 0.4).
        add_edge(&conn, f, callers[0], weak, "calls_name", "NameOnly");
        install_scope_view(&conn, "c", "").unwrap();

        let hub_score = fan_in(&conn, hub).unwrap();
        let weak_score = fan_in(&conn, weak).unwrap();
        assert!(
            hub_score.score > weak_score.score,
            "more/heavier in-edges score higher: {hub_score:?} vs {weak_score:?}"
        );
        assert_eq!(hub_score.label, "local structural load");
        assert_eq!(hub_score.signal, "scoped weighted fan-in");
        assert_eq!(hub_score.bucket, LoadBearingBucket::High, "6.0 ⇒ High");
        assert_eq!(weak_score.bucket, LoadBearingBucket::Low, "0.4 ⇒ Low");
    }

    #[test]
    fn edge_kind_and_confidence_weighting_reflected() {
        let conn = scoped_conn();
        let f = add_file(&conn, "a.rs", "c", "");
        let called = add_symbol(&conn, f, "called", "a::called");
        let imported = add_symbol(&conn, f, "imported", "a::imported");
        let caller = add_symbol(&conn, f, "caller", "a::caller");
        // A `calls_name` in-edge (weight 1.0) vs an `imports` in-edge (weight 0.3), same
        // confidence.
        add_edge(&conn, f, caller, called, "calls_name", "Exact");
        add_edge(&conn, f, caller, imported, "imports", "Exact");
        install_scope_view(&conn, "c", "").unwrap();

        let called = fan_in(&conn, called).unwrap();
        let imported = fan_in(&conn, imported).unwrap();
        assert!(
            called.score > imported.score,
            "the call in-edge outweighs the import in-edge: {called:?} vs {imported:?}"
        );
    }

    #[test]
    fn no_in_edges_is_none() {
        let conn = scoped_conn();
        let f = add_file(&conn, "a.rs", "c", "");
        let leaf = add_symbol(&conn, f, "leaf", "a::leaf");
        install_scope_view(&conn, "c", "").unwrap();
        assert!(fan_in(&conn, leaf).is_none(), "a symbol nothing depends on has no fan-in");
    }

    #[test]
    fn batched_fan_in_accepts_more_than_sqlites_variable_limit() {
        let conn = scoped_conn();
        let file = add_file(&conn, "a.rs", "c", "");
        let caller = add_symbol(&conn, file, "caller", "a::caller");
        let target = add_symbol(&conn, file, "target", "a::target");
        add_edge(&conn, file, caller, target, "calls_name", "Exact");
        install_scope_view(&conn, "c", "").unwrap();
        let targets = (1..=40_000).collect::<Vec<_>>();

        let scores = scoped_weighted_fan_in_many(&conn, &targets, &OracleContext::none()).unwrap();
        assert_eq!(scores.get(&target).map(|score| score.score), Some(1.0));
    }

    #[test]
    fn oracle_confirm_lifts_score_and_sets_tier() {
        // One name-only in-edge: heuristic weight 1.0 × 0.4 = 0.4. With a Confirm verdict it counts
        // at the compiler tier (1.0 × 1.2 = 1.2) AND sets oracle_tier = compiler.
        let conn = scoped_conn();
        let f = add_file(&conn, "a.rs", "c", "");
        let target = add_symbol(&conn, f, "target", "a::target");
        let caller = add_symbol(&conn, f, "caller", "a::caller");
        let edge = add_edge(&conn, f, caller, target, "calls_name", "NameOnly");
        install_scope_view(&conn, "c", "").unwrap();

        let heuristic = fan_in(&conn, target).unwrap();
        assert_eq!(heuristic.oracle_tier, None, "no oracle run ⇒ pure heuristic, no tier");

        let effects = HashMap::from([(edge, EdgeOracleEffect::Confirm)]);
        let lifted =
            scoped_weighted_fan_in(&conn, target, &OracleContext { effects: Some(&effects) })
                .unwrap()
                .unwrap();
        assert!(
            lifted.score > heuristic.score,
            "the confirmed in-edge lifts the score: {lifted:?} vs {heuristic:?}"
        );
        assert_eq!(lifted.oracle_tier, Some(OracleTier::Compiler));
    }

    #[test]
    fn oracle_drop_removes_a_phantom_in_edge() {
        // Two in-edges; the oracle contradicts one (Drop). The dropped edge contributes nothing, so
        // the score reflects only the surviving in-edge.
        let conn = scoped_conn();
        let f = add_file(&conn, "a.rs", "c", "");
        let target = add_symbol(&conn, f, "target", "a::target");
        let real = add_symbol(&conn, f, "real", "a::real");
        let phantom = add_symbol(&conn, f, "phantom", "a::phantom");
        add_edge(&conn, f, real, target, "calls_name", "Exact");
        let dropped = add_edge(&conn, f, phantom, target, "calls_name", "Exact");
        install_scope_view(&conn, "c", "").unwrap();

        let both = fan_in(&conn, target).unwrap();
        let effects = HashMap::from([(dropped, EdgeOracleEffect::Drop)]);
        let one = scoped_weighted_fan_in(&conn, target, &OracleContext { effects: Some(&effects) })
            .unwrap()
            .unwrap();
        assert!(one.score < both.score, "dropping a phantom in-edge lowers the score: {one:?}");
    }

    #[test]
    fn oracle_retarget_to_other_symbol_does_not_count_but_retarget_to_self_does() {
        // The in-edge query selects on the HEURISTIC target S. An oracle `Retarget(other)` means
        // the edge actually points at `other`, not S — the heuristic mis-attributed it — so
        // it must NOT count toward S. A `Retarget(S)` (resolved back onto S itself) DOES
        // count, at the compiler tier, like a `Confirm`.
        let conn = scoped_conn();
        let f = add_file(&conn, "a.rs", "c", "");
        let target = add_symbol(&conn, f, "target", "a::target");
        let other = add_symbol(&conn, f, "other", "a::other");
        let caller = add_symbol(&conn, f, "caller", "a::caller");
        // One in-edge whose heuristic target is S (`target`).
        let edge = add_edge(&conn, f, caller, target, "calls_name", "Exact");
        install_scope_view(&conn, "c", "").unwrap();

        // Heuristic baseline: the single in-edge counts (1.0 × 1.0 = 1.0).
        let heuristic = fan_in(&conn, target).unwrap();

        // Oracle retargets the edge ONTO a DIFFERENT symbol: it is not an in-edge of S, so S's
        // fan-in drops to nothing.
        let to_other = HashMap::from([(edge, EdgeOracleEffect::Retarget(other))]);
        assert!(
            scoped_weighted_fan_in(&conn, target, &OracleContext { effects: Some(&to_other) })
                .unwrap()
                .is_none(),
            "a retarget onto a DIFFERENT symbol must not count toward S's fan-in"
        );

        // Oracle retargets the edge back ONTO S itself: counts at the compiler tier, lifting the
        // score above the heuristic and setting oracle_tier = compiler.
        let to_self = HashMap::from([(edge, EdgeOracleEffect::Retarget(target))]);
        let lifted =
            scoped_weighted_fan_in(&conn, target, &OracleContext { effects: Some(&to_self) })
                .unwrap()
                .unwrap();
        assert!(
            lifted.score > heuristic.score,
            "a retarget back onto S counts at the compiler tier: {lifted:?} vs {heuristic:?}"
        );
        assert_eq!(lifted.oracle_tier, Some(OracleTier::Compiler));
    }

    /// Acceptance invariant #2 (the critical cross-scope guard): the SAME symbol identity exists in
    /// two scopes (two worktree overlays of the same file) with DIFFERENT incoming edges. The
    /// enrichment must compute a DIFFERENT scoped-weighted-fan-in per active scope and must NOT
    /// leak edges across scopes — it reads the active `files` view, nothing else. Modeled on
    /// `worktree_package_roots_do_not_leak_across_scopes`.
    #[test]
    fn fan_in_does_not_leak_across_scopes() {
        let conn = scoped_conn();
        let wt_a = "/wt-a";
        let wt_b = "/wt-b";
        // Two worktree overlays of the same path (commit_sha empty, worktree set — the
        // dirty-overlay shape `install_scope_view` selects on for the active worktree).
        // Each carries its OWN copy of the same symbol identity `a::Thing`.
        let file_a = add_file(&conn, "src/lib.rs", "", wt_a);
        let file_b = add_file(&conn, "src/lib.rs", "", wt_b);
        let thing_a = add_symbol(&conn, file_a, "Thing", "a::Thing");
        let thing_b = add_symbol(&conn, file_b, "Thing", "a::Thing");
        // Worktree A: THREE exact call in-edges into its Thing (3 × 1.0 = 3.0 ⇒ Medium).
        for i in 0..3 {
            let c = add_symbol(&conn, file_a, &format!("a{i}"), &format!("a::a{i}"));
            add_edge(&conn, file_a, c, thing_a, "calls_name", "Exact");
        }
        // Worktree B: ONE exact call in-edge into its Thing (1 × 1.0 = 1.0 ⇒ Low).
        let cb = add_symbol(&conn, file_b, "b0", "a::b0");
        add_edge(&conn, file_b, cb, thing_b, "calls_name", "Exact");

        // Active scope = worktree A: only A's edges are visible.
        install_scope_view(&conn, "", wt_a).unwrap();
        let a = fan_in(&conn, thing_a).unwrap();
        assert_eq!(a.bucket, LoadBearingBucket::Medium, "A sees its 3 in-edges: {a:?}");
        // B's symbol is OUT of scope here — its file isn't in A's `files` view, so its in-edges are
        // invisible: querying B's symbol id under A's scope yields nothing.
        assert!(fan_in(&conn, thing_b).is_none(), "B's symbol is invisible under A's scope");

        // Switch active scope = worktree B: now only B's single edge is visible.
        install_scope_view(&conn, "", wt_b).unwrap();
        let b = fan_in(&conn, thing_b).unwrap();
        assert_eq!(b.bucket, LoadBearingBucket::Low, "B sees its 1 in-edge: {b:?}");
        assert!(
            b.score < a.score,
            "the same identity scores DIFFERENTLY per scope, no leak: A={a:?} B={b:?}"
        );
        assert!(fan_in(&conn, thing_a).is_none(), "A's symbol is invisible under B's scope");
    }
}
