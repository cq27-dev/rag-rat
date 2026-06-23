//! Content-addressed refinement cache (#215 Plan 4a Task 4).
//!
//! [`refinement_key`] derives a STABLE key from the inputs that determine a refinement (language,
//! refine mode, the version pins, and the member `struct_hash` multiset) — NOT from the read-side
//! `class_key` (which is location-derived: `ref@path:start-end`). Two clone classes with the same
//! structural content therefore share a refinement even when they live at different locations, and
//! the key survives a reindex that reassigns rowids.
//!
//! The cache is split into a pure read ([`refine_lookup`], a SELECT — RO-connection-safe) and a
//! compute-then-write ([`refine_compute_and_store`], the expensive LCS work + INSERT). Keeping them
//! separate lets the caller probe the cache on a read-only connection WITHOUT triggering the
//! expensive compute, then surface an `SQLITE_READONLY` error so the MCP dispatcher retries
//! read-write — the compute runs exactly once, on the writable retry. [`refine_class`] is the
//! convenience read-through (lookup → compute-and-store) for callers that already hold a writable
//! connection.

use rusqlite::{Connection, OptionalExtension};

use super::align::class_lcs_ratio;
use super::antiunify::{align_to_anchor, anti_unify, resolve_anchor_idx};
use super::score::{Confidence, confidence_v2, metavar_profile, refactorability_v2};
use super::signature::propose_signature;
use crate::index::clones::refine::RefineMember;
use crate::index::clones::{ALIGNMENT_VERSION, NORM_VERSION};

/// The 4a refine mode. Baseline token space only; the SCIP-aware mode is Plan 3/4b.
const REFINE_MODE: &str = "baseline";

/// A computed (or cache-hit) refinement for one clone class.
///
/// Plan 4b fills the anti-unification payload: the rendered `template`, the serialized
/// `variation_points_json` + `proposed_signature_json`, and the REAL `anti_unify_coverage`
/// (`fixed_spine_columns / total_spine_columns` — no longer the 4a `lcs_ratio` proxy). The scoring
/// outputs (`confidence`/`refactorability`) are the v2 (metavar-profile-aware) bands; `lcs_ratio`
/// stays the NiCad class fidelity. Every field round-trips through `clone_refinements` so a warm
/// cache hit returns the full payload without recomputing the alignment.
pub(crate) struct CachedRefinement {
    pub(crate) lcs_ratio: f64,
    pub(crate) confidence: Confidence,
    pub(crate) refactorability: f64,
    pub(crate) refine_mode: &'static str,
    /// The rendered anti-unification template (`clone_refinements.template`): fixed runs verbatim,
    /// variation runs as `⟨m0⟩`, gapped runs as `⟨m2?⟩`.
    pub(crate) template: String,
    /// `serde_json` array of [`super::antiunify::VariationPoint`]
    /// (`clone_refinements.variation_points_json`).
    pub(crate) variation_points_json: String,
    /// `serde_json` of the [`super::signature::ProposedSignature`]
    /// (`clone_refinements.proposed_signature_json`).
    pub(crate) proposed_signature_json: String,
    /// The REAL anti-unify coverage `fixed_spine_columns / total_spine_columns` ∈ [0,1] — `1.0`
    /// when all members are structurally identical. Distinct from `lcs_ratio` (the NiCad class
    /// fidelity); replaces the 4a proxy that stored `lcs_ratio` in this column.
    pub(crate) anti_unify_coverage: f64,
    /// `true` when the LCS fidelity for this refinement engaged EITHER cost-cap dimension — the
    /// member-count sample ([`LCS_MEMBER_SAMPLE`]) OR the per-pair length proxy
    /// ([`LCS_MAX_SEQ_TOKENS`]); see `class_lcs_ratio`, which ORs both into the returned bit.
    /// PERSISTED in `clone_refinements` (Fix 3, #215 Plan 4a round-2): a cache HIT reads the
    /// stored bit back, so the long-sequence sampling dimension survives a warm hit instead of
    /// degrading to `false`. The caller (`apply_refinement`) folds it into the class's
    /// `metrics_sampled` flag — and ALSO independently re-derives the member-count dimension
    /// there from `class.member_count > LCS_MEMBER_SAMPLE`, as a cache-agnostic guard that
    /// flags the member-count sample even for a row predating this persisted bit (stored
    /// default 0). The length-proxy dimension has no such independent re-derivation; it is
    /// carried only by this persisted bit.
    pub(crate) lcs_sampled: bool,
}

/// Content-addressed refinement key: `sha256(language ∥ refine_mode ∥ NORM_VERSION ∥
/// ALIGNMENT_VERSION ∥ sorted(struct_hashes…) ∥ sorted(source_discriminators…))`, each field
/// NUL-separated. Distinct from the read-side `class_key` (which is `sha256` over
/// `ref@path:start-end` material) by construction: this key sees only structural content + the
/// exact source bytes + the version pins, never a location-derived ref.
///
/// `struct_hashes` is the NORMALIZED token multiset (values stripped → `ID<n>`/`LIT_<KIND>`); it
/// pins the normalization version + structure. But the 4b cached payload (`template`,
/// `variation_points_json` with REAL per-member values, `proposed_signature_json`) is
/// SOURCE-SPECIFIC, so a structure-only key would let two classes with the same `struct_hash`
/// multiset but DIFFERENT real source (class A literals `10`/`2.5`, class B `20`/`3.5`) collide and
/// serve each other's payload — the cache-poisoning bug. `source_discriminators` closes that:
/// each entry is `"{file_sha256}:{start_byte}-{end_byte}"` for one member, pinning the exact source
/// bytes of that member's body (`file_sha256` is the file content hash; the span pins the body
/// range, so together they uniquely determine the raw source). It is fetched by a cheap scoped
/// SELECT (no re-parse), so the warm-probe-before-reparse optimization is preserved.
///
/// Both lists are sorted (duplicates kept) so the key is order-independent over the member set —
/// the same structural+source multiset always addresses the same refinement regardless of member
/// order. Result: same key ⟺ same exact source bodies (multiset) → cross-location reuse of TRUE
/// duplicates is preserved; structurally-identical-but-source-different classes get distinct keys.
pub(crate) fn refinement_key(
    language: &str,
    struct_hashes: &[String],
    source_discriminators: &[String],
) -> String {
    let mut sorted_hashes = struct_hashes.to_vec();
    sorted_hashes.sort_unstable();
    let mut sorted_discriminators = source_discriminators.to_vec();
    sorted_discriminators.sort_unstable();

    let mut material = String::new();
    material.push_str(language);
    material.push('\0');
    material.push_str(REFINE_MODE);
    material.push('\0');
    material.push_str(&NORM_VERSION.to_string());
    material.push('\0');
    material.push_str(&ALIGNMENT_VERSION.to_string());
    material.push('\0');
    for h in &sorted_hashes {
        material.push_str(h);
        material.push('\0');
    }
    // Separate the two lists with a sentinel so concatenation can't blur the boundary (a
    // struct_hash tail can never be confused for a discriminator head).
    material.push('\x01');
    material.push('\0');
    for d in &sorted_discriminators {
        material.push_str(d);
        material.push('\0');
    }
    crate::index::hex_sha256(material.as_bytes())
}

/// PURE READ: return the cached `clone_refinements` row for `refinement_key` (filtered to the
/// current `NORM_VERSION` / `ALIGNMENT_VERSION` so a stale-version row never serves), or `None` on
/// a cache miss. This is a SELECT only — NO compute, NO write — so it is safe to call on a
/// read-only connection. Callers that want compute-on-miss decide separately whether the connection
/// is writable before reaching for [`refine_compute_and_store`].
pub(crate) fn refine_lookup(
    conn: &Connection,
    refinement_key: &str,
) -> anyhow::Result<Option<CachedRefinement>> {
    // The full content payload (4b): the rendered template + the serialized variation_points /
    // proposed_signature + the REAL anti_unify_coverage round-trip alongside the scoring outputs,
    // so a warm hit returns everything without recomputing the alignment.
    #[allow(clippy::type_complexity)]
    let hit: Option<(f64, String, f64, i64, String, String, String, f64)> = conn
        .query_row(
            "SELECT lcs_ratio, confidence, refactorability, lcs_sampled,
                    template, variation_points_json, proposed_signature_json, anti_unify_coverage
             FROM clone_refinements
             WHERE class_key = ?1 AND norm_version = ?2 AND alignment_version = ?3",
            rusqlite::params![refinement_key, NORM_VERSION, ALIGNMENT_VERSION],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()?;
    Ok(hit.map(
        |(
            lcs_ratio,
            confidence,
            refactorability,
            lcs_sampled,
            template,
            variation_points_json,
            proposed_signature_json,
            anti_unify_coverage,
        )| CachedRefinement {
            lcs_ratio,
            confidence: Confidence::from_db_str(&confidence),
            refactorability,
            refine_mode: REFINE_MODE,
            template,
            variation_points_json,
            proposed_signature_json,
            anti_unify_coverage,
            // Cache hit: read the persisted sampling bit back (Fix 3) so the long-sequence
            // dimension survives a warm hit.
            lcs_sampled: lcs_sampled != 0,
        },
    ))
}

/// COMPUTE + WRITE: run the full anti-unification (medoid-anchored star LCS → template + variation
/// points → proposed signature), score it with the metavar-profile-aware v2 formulas, persist the
/// full `clone_refinements` row keyed by `refinement_key`, and return the computed refinement. This
/// is the expensive half (the star alignment + the INSERT) and REQUIRES a writable connection —
/// call it only after [`refine_lookup`] missed AND the caller confirmed the connection is
/// read-write (otherwise the INSERT errors with `SQLITE_READONLY`, which is the deliberate signal
/// the read path uses to retry read-write — see `refine_class_in_place`).
///
/// `medoid_symbol_id` is the class's bag-overlap medoid (Plan 4b §1.1) — threaded out by
/// `build_class` and passed straight through as the anti-unify spine anchor; `None` falls back to
/// the canonical-first `(struct_hash, path, start_byte)` member.
///
/// `similarity_min` is the Plan-2 pairwise floor for the class — it feeds the v2 confidence band
/// only, not the cache key (the key is purely structural). Plan 4b persists the REAL payload: the
/// rendered anti-unification `template`, the serialized `variation_points` + `proposed_signature`,
/// and the REAL `anti_unify_coverage` (`fixed_spine_columns / total_spine_columns`, no longer the
/// 4a `lcs_ratio` proxy).
pub(crate) fn refine_compute_and_store(
    conn: &Connection,
    refinement_key: &str,
    language: &str,
    members: &[RefineMember],
    similarity_min: f64,
    medoid_symbol_id: Option<i64>,
) -> anyhow::Result<CachedRefinement> {
    // `lcs_ratio` stays the NiCad class fidelity (min pairwise 2·LCS/(|a|+|b|)) + its sampling bit.
    let seqs: Vec<Vec<String>> = members.iter().map(|m| m.seq.clone()).collect();
    let (lcs_ratio, lcs_sampled) = class_lcs_ratio(&seqs);

    // ── Anti-unification (Plan 4b §1.1-§1.10): medoid-anchored star LCS → template + VPs
    // ──────────
    let anchor_idx = resolve_anchor_idx(members, medoid_symbol_id);
    let alignment = align_to_anchor(members, anchor_idx);
    // The anti-unify alignment has its own cost guards (P1 + the aggregate cell budget): a member
    // or anchor seq past `LCS_MAX_SEQ_TOKENS`, OR a member past the per-class
    // `ALIGN_AGGREGATE_CELLS_BUDGET`, is skipped/degraded rather than allocating/running the huge
    // DP. `alignment.sampled` covers the parent star-align; `template.sampled` (below) covers a
    // budget-degraded matched-statement re-descent. Fold BOTH into `lcs_sampled` so the persisted
    // sampling bit reflects the fidelity-metric cap AND the whole template lane — a
    // degraded/skipped template is never reported as exact.
    let template = anti_unify(members, &alignment);
    let lcs_sampled = lcs_sampled || alignment.sampled || template.sampled;

    // ── Proposed signature (Plan 4b §1.11) ───────────────────────────────────────────────────────
    // Pass the SAME anchor_idx anti_unify used: variation_points' occurrences index the anchor
    // (medoid) member, so type recovery MUST read that member, not members[0] (P2c).
    let signature = propose_signature(&template, members, anchor_idx);

    // ── v2 scoring (Plan 4b §1.12, DOWNGRADE-ONLY): fold the metavar profile in
    // ───────────────────
    let profile = metavar_profile(&template);
    let refactorability = refactorability_v2(lcs_ratio, &profile);
    let confidence = confidence_v2(lcs_ratio, similarity_min, &profile);

    // The REAL coverage from the anti-unified template — NOT the 4a lcs_ratio proxy.
    let anti_unify_coverage = template.anti_unify_coverage;

    // Serialize the machine contract over ORDERED Vecs (no HashMap JSON maps — determinism).
    let variation_points_json = serde_json::to_string(&template.variation_points)?;
    let proposed_signature_json = serde_json::to_string(&signature)?;

    // Persist the full row. INSERT OR REPLACE because `class_key` is the table PRIMARY KEY — a
    // re-run at a NEW version pin replaces the stale row in place rather than accumulating.
    conn.execute(
        "INSERT OR REPLACE INTO clone_refinements(
             class_key, language, refine_mode, template,
             variation_points_json, proposed_signature_json, confidence,
             anti_unify_coverage, lcs_ratio, refactorability,
             norm_version, alignment_version, created_at_ms, lcs_sampled
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        rusqlite::params![
            refinement_key,
            language,
            REFINE_MODE,
            template.text,
            variation_points_json,
            proposed_signature_json,
            confidence.as_db_str(),
            anti_unify_coverage, // REAL coverage (fixed/total), not the lcs_ratio proxy
            lcs_ratio,
            refactorability,
            NORM_VERSION,
            ALIGNMENT_VERSION,
            crate::index::now_ms(),
            lcs_sampled as i64, // Fix 3: persist the sampling bit so warm hits keep it.
        ],
    )?;

    Ok(CachedRefinement {
        lcs_ratio,
        confidence,
        refactorability,
        refine_mode: REFINE_MODE,
        template: template.text,
        variation_points_json,
        proposed_signature_json,
        anti_unify_coverage,
        lcs_sampled,
    })
}

/// Read-through refinement (writable-connection convenience): [`refine_lookup`] on hit, else
/// [`refine_compute_and_store`]. Requires a writable connection on a cache miss. The RO-aware read
/// path (`refine_class_in_place`) calls the two halves directly so it can probe the cache on a
/// read-only connection and surface `SQLITE_READONLY` BEFORE any expensive compute.
///
/// `medoid_symbol_id` threads the anti-unify spine anchor through to the compute half (Plan 4b).
pub(crate) fn refine_class(
    conn: &Connection,
    refinement_key: &str,
    language: &str,
    members: &[RefineMember],
    similarity_min: f64,
    medoid_symbol_id: Option<i64>,
) -> anyhow::Result<CachedRefinement> {
    if let Some(cached) = refine_lookup(conn, refinement_key)? {
        return Ok(cached);
    }
    refine_compute_and_store(
        conn,
        refinement_key,
        language,
        members,
        similarity_min,
        medoid_symbol_id,
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::index::clones::normalize::{NodeSpan, normalize_baseline_spanned};
    use crate::index::clones::refine::align::LCS_MAX_SEQ_TOKENS;
    use crate::index::clones::tokens;
    use crate::index::parser;
    use crate::language::Language;

    /// Build a `RefineMember` from a Rust snippet, mirroring `load_refine_members`: parse, descend
    /// to the first `function` symbol, span-normalize (so `node_spans.len() == seq.len()`),
    /// compute the faithfulness struct_hash. This is what feeds the 4b anti-unify path — empty
    /// `node_spans` would index out of bounds.
    fn member(symbol_id: i64, src: &str) -> RefineMember {
        let text: Arc<str> = Arc::from(src);
        let parsed = parser::parse_file(Path::new("t.rs"), Language::Rust, &text).expect("parse");
        let func = parsed.symbols.iter().find(|s| s.kind == "function").expect("a function symbol");
        let node =
            parsed.root().descendant_for_byte_range(func.start_byte, func.end_byte).expect("node");
        let (seq, node_spans) = normalize_baseline_spanned(node, &text, Language::Rust);
        let struct_hash = tokens::struct_hash(&seq);
        RefineMember { symbol_id, lang: Language::Rust, struct_hash, seq, node_spans, text }
    }

    /// Per-member source discriminator the way `load_source_discriminators` builds it in
    /// production: `"{file_sha256}:{start}-{end}"`, pinning the EXACT source bytes. Tests
    /// synthesize one member per file, so the discriminator hashes the member's whole source
    /// and spans `0..len` — distinct real source ⇒ distinct discriminator, byte-identical
    /// source ⇒ identical discriminator.
    fn discriminators_for(members: &[RefineMember]) -> Vec<String> {
        members
            .iter()
            .map(|m| {
                let src = m.text.as_ref();
                format!("{}:{}-{}", crate::index::hex_sha256(src.as_bytes()), 0, src.len())
            })
            .collect()
    }

    #[test]
    fn refinement_key_is_order_independent_over_struct_hashes() {
        let k1 = refinement_key("rust", &["aaa".into(), "bbb".into(), "ccc".into()], &[]);
        let k2 = refinement_key("rust", &["ccc".into(), "aaa".into(), "bbb".into()], &[]);
        assert_eq!(k1, k2, "the same struct_hash multiset must address the same refinement");

        // Source discriminators are ALSO order-independent (sorted before folding).
        let d1 = refinement_key("rust", &["aaa".into()], &["s1".into(), "s2".into()]);
        let d2 = refinement_key("rust", &["aaa".into()], &["s2".into(), "s1".into()]);
        assert_eq!(d1, d2, "source discriminators must be order-independent too");
    }

    #[test]
    fn refinement_key_distinguishes_language_and_multiset() {
        let base = refinement_key("rust", &["aaa".into(), "bbb".into()], &[]);
        // Different language → different key.
        assert_ne!(base, refinement_key("typescript", &["aaa".into(), "bbb".into()], &[]));
        // Different multiset → different key.
        assert_ne!(base, refinement_key("rust", &["aaa".into(), "ccc".into()], &[]));
        // Duplicate-sensitive: a doubled hash is a different multiset.
        assert_ne!(base, refinement_key("rust", &["aaa".into(), "aaa".into(), "bbb".into()], &[]));
        // Same struct_hash multiset, DIFFERENT source discriminators → different key (the
        // cache-poisoning fix: structure-only is no longer sufficient).
        assert_ne!(base, refinement_key("rust", &["aaa".into(), "bbb".into()], &["s1".into()]));
    }

    /// Synthesize a long `RefineMember` with VALID parallel `node_spans` (one leaf span per token):
    /// the loader guarantees `node_spans.len() == seq.len()` and the 4b anti-unify path indexes
    /// `node_spans`, so a long-member test fixture must build spans, not leave them empty. Each
    /// token `t{i}` is a single-char leaf at byte offset `i` in a synthetic backing buffer.
    fn long_member(symbol_id: i64, struct_hash: &str, token_count: usize) -> RefineMember {
        let seq: Vec<String> = (0..token_count).map(|i| format!("t{i}")).collect();
        // Synthetic byte layout: token i occupies bytes [i*2 .. i*2+1) (a placeholder leaf each).
        let text: String = (0..token_count).map(|_| "x ").collect();
        let node_spans: Vec<NodeSpan> = (0..token_count)
            .map(|i| NodeSpan {
                start_byte: i * 2,
                end_byte: i * 2 + 1,
                kind: "identifier",
                is_leaf: true,
            })
            .collect();
        RefineMember {
            symbol_id,
            lang: Language::Rust,
            struct_hash: struct_hash.to_string(),
            seq,
            node_spans,
            text: Arc::from(text.as_str()),
        }
    }

    /// Fix 2 (#215 Plan 4a round-2): the compute path stays cheap on long members. A 2-member class
    /// whose sequences exceed [`LCS_MAX_SEQ_TOKENS`] flows through `refine_compute_and_store` with
    /// the LCS ratio falling back to the clamped Dice proxy — it returns promptly, persists one
    /// row, and marks `lcs_sampled`.
    #[test]
    fn refine_compute_on_long_members_is_cheap_and_sampled() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        let members = vec![
            long_member(1, "h1", LCS_MAX_SEQ_TOKENS + 1),
            long_member(2, "h2", LCS_MAX_SEQ_TOKENS + 1),
        ];
        let key = refinement_key("rust", &["h1".into(), "h2".into()], &[]);

        let refinement =
            refine_compute_and_store(&conn, &key, "rust", &members, 1.0, None).unwrap();
        assert!(refinement.lcs_sampled, "long-member compute must set lcs_sampled (proxy path)");
        let rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "compute persists exactly one row");
    }

    /// Fix 3 (#215 Plan 4a round-2): the `lcs_sampled` bit is PERSISTED, so a class with ≤
    /// `LCS_MEMBER_SAMPLE` members but LONG sequences (sampled via the per-pair length proxy on the
    /// COLD compute) still reports `lcs_sampled = true` on a WARM cache hit. Before the fix the bit
    /// was compute-only and `refine_lookup` hardcoded `false`, so the long-sequence sampling
    /// dimension was lost on a warm hit. This test pins the round-trip end to end.
    #[test]
    fn lcs_sampled_survives_cache_hit_for_long_seq_small_class() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        // A 2-member class (well below LCS_MEMBER_SAMPLE) with sequences past LCS_MAX_SEQ_TOKENS:
        // the member-count cap does NOT engage, only the per-pair length proxy does — exactly the
        // dimension the OR(member_count > LCS_MEMBER_SAMPLE) in apply_refinement cannot catch.
        let members = vec![
            long_member(1, "h1", LCS_MAX_SEQ_TOKENS + 1),
            long_member(2, "h2", LCS_MAX_SEQ_TOKENS + 1),
        ];
        let key = refinement_key("rust", &["h1".into(), "h2".into()], &[]);

        // COLD compute: the length proxy engages → lcs_sampled = true, and it is PERSISTED.
        let cold = refine_compute_and_store(&conn, &key, "rust", &members, 1.0, None).unwrap();
        assert!(cold.lcs_sampled, "cold compute on long seqs must set lcs_sampled");

        // Confirm the bit landed in the row (1, not the DEFAULT 0).
        let persisted: i64 = conn
            .query_row(
                "SELECT lcs_sampled FROM clone_refinements WHERE class_key = ?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(persisted, 1, "lcs_sampled must be persisted as 1");

        // WARM hit: refine_lookup reads the bit back — it must NOT degrade to false.
        let warm = refine_lookup(&conn, &key).unwrap().expect("warm hit");
        assert!(warm.lcs_sampled, "warm cache hit must read lcs_sampled=true back from the row");
    }

    /// [`refine_lookup`] is a PURE READ: a miss returns `None` and writes NOTHING — no
    /// `clone_refinements` row appears. This is the property that makes the read path safe to run
    /// on a read-only MCP connection: the cache probe never touches the write lock, so the
    /// expensive `refine_compute_and_store` (and its INSERT) only runs after the caller has
    /// confirmed a writable connection.
    ///
    /// The read-only-connection RETRY mechanism itself (probe → `DELETE FROM clone_refinements
    /// WHERE 1=0` triggering `SQLITE_READONLY` → dispatcher retries read-write → compute runs
    /// once on the writable retry) is exercised by
    /// `refine_ro_connection_probe_fires_sqlite_readonly` (below), which opens a real
    /// `rusqlite` read-only connection and asserts that the DELETE probe yields a
    /// `SQLITE_READONLY` error that `is_readonly_violation` flags. The unit-level invariant
    /// here is the load-bearing half: the lookup must not write.
    #[test]
    fn refine_lookup_returns_none_on_miss_without_writing() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        let count_rows = |conn: &rusqlite::Connection| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(count_rows(&conn), 0, "table starts empty");

        let key = refinement_key("rust", &["h1".into(), "h2".into()], &[]);
        let hit = refine_lookup(&conn, &key).unwrap();

        assert!(hit.is_none(), "a miss must return None");
        assert_eq!(count_rows(&conn), 0, "refine_lookup must NOT write a row on a miss");
    }

    /// The writable-connection read-through [`refine_class`]: a miss computes + persists exactly
    /// one row; a second call over the same key is a cache HIT that serves the persisted row
    /// WITHOUT growing the count. (The RO-aware split path is `refine_class_in_place`; this
    /// covers the convenience wrapper directly on a writable in-memory connection.)
    #[test]
    fn refine_class_read_through_computes_once_then_serves_cache() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        let count_rows = |conn: &rusqlite::Connection| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0)).unwrap()
        };

        // Two structurally-identical members (alpha-renamed) ⇒ a clean, near-perfect class.
        let members = vec![
            member(1, "fn f() { let x = 10; sink(x); }"),
            member(2, "fn g() { let y = 20; sink(y); }"),
        ];
        let struct_hashes: Vec<String> = members.iter().map(|m| m.struct_hash.clone()).collect();
        let key = refinement_key("rust", &struct_hashes, &discriminators_for(&members));

        // Miss: compute + persist one row.
        let first = refine_class(&conn, &key, "rust", &members, 1.0, None).unwrap();
        assert_eq!(count_rows(&conn), 1, "the miss persists exactly one row");
        assert!(first.lcs_ratio > 0.99, "identical sequences ⇒ near-perfect lcs_ratio");

        // Hit: same key serves the cache, no new row.
        let second = refine_class(&conn, &key, "rust", &members, 1.0, None).unwrap();
        assert_eq!(count_rows(&conn), 1, "the hit must NOT grow the row count");
        assert_eq!(
            first.confidence, second.confidence,
            "the cache-served refinement matches the computed one"
        );
    }

    /// Fix 2 (#215 Plan 4a): the DELETE-probe in `refine_class_in_place` (the cold-path on a
    /// read-only connection) fires `SQLITE_READONLY`, which `is_readonly_violation` recognises
    /// so the MCP dispatcher can retry read-write. This unit test runs the probe against a REAL
    /// `rusqlite` read-only connection to prove the signal is produced — it is NOT produced by
    /// the writable `IndexDatabase::rebuild` connection used in `refine_cache_is_read_through`.
    #[test]
    fn refine_ro_connection_probe_fires_sqlite_readonly() {
        use rusqlite::OpenFlags;

        // Build a writable in-memory DB and apply the schema.
        let rw_path =
            std::env::temp_dir().join(format!("ragrat-refine-ro-probe-{}.db", std::process::id()));
        {
            let rw = rusqlite::Connection::open(&rw_path).unwrap();
            crate::index::schema::apply(&rw).unwrap();
        }

        // Open it read-only.
        let ro = rusqlite::Connection::open_with_flags(
            &rw_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();

        // The same DELETE probe used in `refine_class_in_place`.
        let probe_err = ro.execute("DELETE FROM clone_refinements WHERE 1=0", []).unwrap_err();

        // Wrap in anyhow so `is_readonly_violation` can walk the chain.
        let anyhow_err = anyhow::Error::from(probe_err);
        assert!(
            crate::storage::is_readonly_violation(&anyhow_err),
            "the DELETE probe on a RO connection must produce SQLITE_READONLY: {anyhow_err}"
        );

        let _ = std::fs::remove_file(&rw_path);
    }

    // ── Plan 4b Task 7: full content cache round-trip + version invalidation
    // ──────────────────────

    /// The full anti-unification payload round-trips through `clone_refinements`: a
    /// `refine_compute_and_store` followed by a `refine_lookup` returns the SAME `template`,
    /// `variation_points_json`, `proposed_signature_json`, and `anti_unify_coverage` (no recompute,
    /// no proxy). This is the property that makes a warm cache hit return the real 4b payload.
    #[test]
    fn cache_round_trips_template_variation_points_signature() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        // A class that differs only in a literal kind → one value_param variation point + a real
        // proposed signature (non-trivial template / VP / sig payload to round-trip).
        let members = vec![
            member(1, "fn f() { let x: i32 = 10; sink(x); }"),
            member(2, "fn g() { let y: f32 = 2.5; sink(y); }"),
        ];
        let struct_hashes: Vec<String> = members.iter().map(|m| m.struct_hash.clone()).collect();
        let key = refinement_key("rust", &struct_hashes, &discriminators_for(&members));

        let computed = refine_compute_and_store(&conn, &key, "rust", &members, 1.0, None).unwrap();
        // The payload is non-trivial: a real template + a non-empty VP array + a non-stub
        // signature.
        assert!(!computed.template.is_empty(), "computed template must be non-empty");
        assert_ne!(computed.variation_points_json, "[]", "expected a non-empty VP array");
        assert_ne!(computed.proposed_signature_json, "{}", "expected a real proposed signature");
        assert!(
            computed.variation_points_json.contains("value_param"),
            "the differing literal must surface as a value_param VP: {}",
            computed.variation_points_json
        );

        // Warm lookup returns byte-identical payload — no recompute, no proxy.
        let cached = refine_lookup(&conn, &key).unwrap().expect("warm hit");
        assert_eq!(cached.template, computed.template, "template must round-trip");
        assert_eq!(
            cached.variation_points_json, computed.variation_points_json,
            "variation_points_json must round-trip"
        );
        assert_eq!(
            cached.proposed_signature_json, computed.proposed_signature_json,
            "proposed_signature_json must round-trip"
        );
        assert!(
            (cached.anti_unify_coverage - computed.anti_unify_coverage).abs() < 1e-12,
            "anti_unify_coverage must round-trip ({} vs {})",
            cached.anti_unify_coverage,
            computed.anti_unify_coverage
        );
        // Coverage is the REAL fixed/total fraction (< 1.0 here — one differing column), NOT the
        // lcs_ratio proxy.
        assert!(
            cached.anti_unify_coverage > 0.0 && cached.anti_unify_coverage < 1.0,
            "one differing column → fractional REAL coverage, got {}",
            cached.anti_unify_coverage
        );
    }

    /// An `ALIGNMENT_VERSION` bump invalidates a stale-version row: a row written at
    /// `alignment_version = 1` (the 4a pin) MISSES the lookup at the current `ALIGNMENT_VERSION`
    /// (= 3), so it is never served; recomputing writes the row at the current version with the
    /// REAL 4b payload. (The bump lives in the lookup WHERE + the content key — no schema
    /// migration.)
    #[test]
    fn alignment_version_bump_invalidates_and_recomputes() {
        assert_eq!(ALIGNMENT_VERSION, 3, "this test pins the current alignment version");

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        let members = vec![
            member(1, "fn f() { let x = 10; sink(x); }"),
            member(2, "fn g() { let y = 2.5; sink(y); }"),
        ];
        let struct_hashes: Vec<String> = members.iter().map(|m| m.struct_hash.clone()).collect();
        let key = refinement_key("rust", &struct_hashes, &discriminators_for(&members));

        // Plant a stale 4a-style row at alignment_version = 1 (placeholder payload) directly.
        conn.execute(
            "INSERT INTO clone_refinements(
                 class_key, language, refine_mode, template,
                 variation_points_json, proposed_signature_json, confidence,
                 anti_unify_coverage, lcs_ratio, refactorability,
                 norm_version, alignment_version, created_at_ms, lcs_sampled
             ) VALUES (?1, 'rust', 'baseline', 'STALE 4A SKELETON',
                       '[]', '{}', 'low', 0.5, 0.5, 0.5, ?2, 1, 0, 0)",
            rusqlite::params![key, NORM_VERSION],
        )
        .unwrap();

        // The lookup at the CURRENT version (2) must MISS the v1 row.
        assert!(
            refine_lookup(&conn, &key).unwrap().is_none(),
            "an alignment_version=1 row must not serve at the current version"
        );

        // Recompute: writes the row at the current version with the REAL payload (INSERT OR REPLACE
        // over the same class_key PRIMARY KEY).
        let recomputed =
            refine_compute_and_store(&conn, &key, "rust", &members, 1.0, None).unwrap();
        assert_ne!(recomputed.template, "STALE 4A SKELETON", "must recompute the real template");
        assert_ne!(recomputed.variation_points_json, "[]", "recompute fills the real VP array");

        // Exactly one row (REPLACE collapsed the v1 row), now at the current version, and it
        // serves.
        let rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 1, "INSERT OR REPLACE keeps one row per class_key");
        let stored_version: i64 = conn
            .query_row(
                "SELECT alignment_version FROM clone_refinements WHERE class_key = ?1",
                [&key],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_version, ALIGNMENT_VERSION, "the recompute writes the current version");
        let warm = refine_lookup(&conn, &key).unwrap().expect("warm hit after recompute");
        assert_eq!(
            warm.template, recomputed.template,
            "the warm hit serves the recomputed payload"
        );
    }

    /// The content-addressed `refinement_key` (and therefore the cached template) collides ONLY for
    /// identical structural content: the SAME struct_hash multiset yields the same key + the same
    /// template; a DIFFERENT multiset yields a different key + (here) a different template.
    #[test]
    fn refinement_key_collides_only_for_identical_content() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        let class_a = vec![
            member(1, "fn f() { let x = 10; sink(x); }"),
            member(2, "fn g() { let y = 2.5; sink(y); }"),
        ];
        let hashes_a: Vec<String> = class_a.iter().map(|m| m.struct_hash.clone()).collect();
        let discs_a = discriminators_for(&class_a);
        let key_a = refinement_key("rust", &hashes_a, &discs_a);
        let computed_a =
            refine_compute_and_store(&conn, &key_a, "rust", &class_a, 1.0, None).unwrap();

        // Re-key the SAME content (same struct_hash multiset + same source bytes) → same key, same
        // template (cache hit).
        let key_a2 = refinement_key("rust", &hashes_a, &discs_a);
        assert_eq!(key_a, key_a2, "identical content must address the same refinement");
        let cached = refine_lookup(&conn, &key_a2).unwrap().expect("hit for identical content");
        assert_eq!(cached.template, computed_a.template, "identical content → identical template");

        // A DIFFERENT class (a different body shape → a different struct_hash multiset) → a
        // different key, and a structurally different template.
        let class_b = vec![
            member(3, "fn p(a: i32, b: i32) -> i32 { let r = a + b + a; r * 2 }"),
            member(4, "fn q(c: i32, d: i32) -> i32 { let s = c + d + c; s * 2 }"),
        ];
        let hashes_b: Vec<String> = class_b.iter().map(|m| m.struct_hash.clone()).collect();
        let key_b = refinement_key("rust", &hashes_b, &discriminators_for(&class_b));
        assert_ne!(key_a, key_b, "different content must address a different refinement");
        let computed_b =
            refine_compute_and_store(&conn, &key_b, "rust", &class_b, 1.0, None).unwrap();
        assert_ne!(
            computed_a.template, computed_b.template,
            "structurally different classes must not share a template"
        );
    }

    /// CACHE-POISONING REGRESSION (#215 Plan 4b P1): two classes with the SAME `struct_hash`
    /// multiset (the NORMALIZED token sequence — literal VALUES are erased to `LIT_<KIND>` buckets,
    /// locals to `ID<n>`) but DIFFERENT real source literals must get DIFFERENT `refinement_key`s
    /// AND DIFFERENT cached templates/per-member-values. The 4b cached payload is SOURCE-SPECIFIC,
    /// so a structure-only key would serve class A's template (with A's literals `10`/`2.5`) to
    /// class B (whose literals are `20`/`3.5`) — the poisoning this test guards against. The
    /// source-discriminator key material (`{file_sha256}:{start}-{end}`) closes the hole.
    #[test]
    fn refinement_cache_not_poisoned_by_structurally_identical_different_source() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        // Class A and class B differ only in the literal KIND per column (int vs float) — alpha-
        // renaming + literal-bucketing makes their struct_hash multiset IDENTICAL — but the real
        // literal values differ (A: 10/2.5, B: 20/3.5). The template's per_member_values are the
        // REAL source, so A's payload must never be served to B.
        let class_a = vec![
            member(1, "fn f() { let x = 10; sink(x); }"),
            member(2, "fn g() { let y = 2.5; sink(y); }"),
        ];
        let class_b = vec![
            member(101, "fn aa() { let pp = 20; sink(pp); }"),
            member(102, "fn bb() { let qq = 3.5; sink(qq); }"),
        ];

        // Precondition: the two classes DO share the struct_hash multiset (this is the property the
        // OLD structure-only key keyed on — the bug). The fix discriminates them by source bytes.
        let hashes_a: Vec<String> = class_a.iter().map(|m| m.struct_hash.clone()).collect();
        let hashes_b: Vec<String> = class_b.iter().map(|m| m.struct_hash.clone()).collect();
        let mut ha = hashes_a.clone();
        ha.sort();
        let mut hb = hashes_b.clone();
        hb.sort();
        assert_eq!(ha, hb, "the two classes must share the struct_hash multiset (the bug premise)");

        let key_a = refinement_key("rust", &hashes_a, &discriminators_for(&class_a));
        let key_b = refinement_key("rust", &hashes_b, &discriminators_for(&class_b));
        assert_ne!(
            key_a, key_b,
            "same struct_hash multiset but different source MUST get distinct keys (no poisoning)"
        );

        // Compute A, then B. Two distinct keys → two distinct rows, each with its OWN payload.
        let computed_a =
            refine_compute_and_store(&conn, &key_a, "rust", &class_a, 1.0, None).unwrap();
        let computed_b =
            refine_compute_and_store(&conn, &key_b, "rust", &class_b, 1.0, None).unwrap();
        let rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2, "two source-distinct classes persist two distinct rows");

        // B's WARM lookup serves B's OWN payload — NOT A's. The per_member_values carry the real
        // literals, so a poisoned cache would surface A's `10`/`2.5` for class B.
        let warm_b = refine_lookup(&conn, &key_b).unwrap().expect("class B has its own warm row");
        assert_eq!(warm_b.template, computed_b.template, "B must serve B's template, not A's");
        assert_ne!(
            warm_b.variation_points_json, computed_a.variation_points_json,
            "B's per_member_values must differ from A's (no cross-class poisoning)"
        );
        // Concretely: A's payload mentions `2.5`, B's mentions `3.5` — they are not
        // interchangeable.
        assert!(
            computed_a.variation_points_json.contains("2.5"),
            "class A payload carries its real literal 2.5: {}",
            computed_a.variation_points_json
        );
        assert!(
            warm_b.variation_points_json.contains("3.5")
                && !warm_b.variation_points_json.contains("2.5"),
            "class B payload carries ITS real literal 3.5 (not A's 2.5): {}",
            warm_b.variation_points_json
        );
    }

    /// Content-addressing of TRUE duplicates is preserved: two DISTINCT classes (different
    /// `symbol_id`s, different file rows) whose member bodies are BYTE-IDENTICAL source share the
    /// same `refinement_key` → the second class's lookup serves the first's persisted template
    /// without recomputing. (The complement of the poisoning test: same struct_hash multiset AND
    /// same source bytes ⟹ same key.)
    #[test]
    fn content_addressable_byte_identical_source_shares_template() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();

        // Class 1 and class 2 are BYTE-IDENTICAL member bodies (a true cross-location duplicate),
        // only the symbol_ids differ. Identical source bytes → identical discriminators → identical
        // key.
        let class1 = vec![
            member(1, "fn f() { let x = 10; sink(x); }"),
            member(2, "fn g() { let y = 2.5; sink(y); }"),
        ];
        let class2 = vec![
            member(101, "fn f() { let x = 10; sink(x); }"),
            member(102, "fn g() { let y = 2.5; sink(y); }"),
        ];
        let hashes1: Vec<String> = class1.iter().map(|m| m.struct_hash.clone()).collect();
        let hashes2: Vec<String> = class2.iter().map(|m| m.struct_hash.clone()).collect();
        let key1 = refinement_key("rust", &hashes1, &discriminators_for(&class1));
        let key2 = refinement_key("rust", &hashes2, &discriminators_for(&class2));
        assert_eq!(key1, key2, "byte-identical source → same content key (true dupe)");

        // Compute class 1 → persists one row.
        let computed1 = refine_compute_and_store(&conn, &key1, "rust", &class1, 1.0, None).unwrap();
        let rows_after_1: i64 =
            conn.query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0)).unwrap();
        assert_eq!(rows_after_1, 1, "class 1 persists one row");

        // Class 2 (distinct class, byte-identical content) LOOKS UP the same key → cache hit, same
        // template, no new row.
        let cached2 = refine_lookup(&conn, &key2).unwrap().expect("class 2 hits class 1's cache");
        assert_eq!(
            cached2.template, computed1.template,
            "byte-identical source must serve the same cached template"
        );
        let rows_after_2: i64 =
            conn.query_row("SELECT COUNT(*) FROM clone_refinements", [], |r| r.get(0)).unwrap();
        assert_eq!(rows_after_2, 1, "a content-addressed hit must not grow the row count");
    }
}
