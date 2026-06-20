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

use super::align::{class_lcs_ratio, lcs_align};
use super::score::{Confidence, confidence_v1, refactorability_v1};
use crate::index::clones::refine::RefineMember;
use crate::index::clones::{ALIGNMENT_VERSION, NORM_VERSION};

/// The 4a refine mode. Baseline token space only; the SCIP-aware mode is Plan 3/4b.
const REFINE_MODE: &str = "baseline";

/// A computed (or cache-hit) refinement for one clone class. The 4a surface carries only the
/// scoring outputs; the template / variation-points / proposed-signature are persisted minimally
/// (4b fills them) and not returned.
pub(crate) struct CachedRefinement {
    pub(crate) lcs_ratio: f64,
    pub(crate) confidence: Confidence,
    pub(crate) refactorability: f64,
    pub(crate) refine_mode: &'static str,
    /// `true` when the LCS fidelity for THIS compute engaged a cost cap (member-count or per-pair
    /// length — see `class_lcs_ratio`). Computed on a cache MISS only; on a cache HIT it is
    /// conservatively `false` (the persisted row carries no sampling marker — `lcs_sampled` is an
    /// implementation artifact of the compute, not content, so it is deliberately NOT persisted in
    /// `clone_refinements`). The caller folds it into the class's `metrics_sampled` flag.
    pub(crate) lcs_sampled: bool,
}

/// Content-addressed refinement key: `sha256(language ∥ refine_mode ∥ NORM_VERSION ∥
/// ALIGNMENT_VERSION ∥ sorted(struct_hashes…))`, each field NUL-separated. Distinct from the
/// read-side `class_key` (which is `sha256` over `ref@path:start-end` material) by construction:
/// this key sees only structural content + version pins, never a location.
///
/// `struct_hashes` is sorted (duplicates kept) so the key is order-independent over the member set
/// — the same structural multiset always addresses the same refinement regardless of member order.
pub(crate) fn refinement_key(language: &str, struct_hashes: &[String]) -> String {
    let mut sorted = struct_hashes.to_vec();
    sorted.sort_unstable();

    let mut material = String::new();
    material.push_str(language);
    material.push('\0');
    material.push_str(REFINE_MODE);
    material.push('\0');
    material.push_str(&NORM_VERSION.to_string());
    material.push('\0');
    material.push_str(&ALIGNMENT_VERSION.to_string());
    material.push('\0');
    for h in &sorted {
        material.push_str(h);
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
    let hit: Option<(f64, String, f64)> = conn
        .query_row(
            "SELECT lcs_ratio, confidence, refactorability
             FROM clone_refinements
             WHERE class_key = ?1 AND norm_version = ?2 AND alignment_version = ?3",
            rusqlite::params![refinement_key, NORM_VERSION, ALIGNMENT_VERSION],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    Ok(hit.map(|(lcs_ratio, confidence, refactorability)| CachedRefinement {
        lcs_ratio,
        confidence: Confidence::from_db_str(&confidence),
        refactorability,
        refine_mode: REFINE_MODE,
        // Cache hit: `lcs_sampled` is not persisted (see the field doc) — conservatively `false`.
        lcs_sampled: false,
    }))
}

/// COMPUTE + WRITE: compute the 4a scores from the member token sequences, persist a minimal
/// `clone_refinements` row keyed by `refinement_key`, and return the computed refinement. This is
/// the expensive half (`class_lcs_ratio` + `lcs_skeleton` + the INSERT) and REQUIRES a writable
/// connection — call it only after [`refine_lookup`] missed AND the caller confirmed the connection
/// is read-write (otherwise the INSERT errors with `SQLITE_READONLY`, which is the deliberate
/// signal the read path uses to retry read-write — see `refine_class_in_place`).
///
/// `similarity_min` is the Plan-2 pairwise floor for the class — it feeds the confidence band only,
/// not the cache key (the key is purely structural). The persisted `template` is a crude LCS
/// skeleton (4b fills the full anti-unification template + variation points); `anti_unify_coverage`
/// is the `lcs_ratio` as a 4a proxy.
pub(crate) fn refine_compute_and_store(
    conn: &Connection,
    refinement_key: &str,
    language: &str,
    members: &[RefineMember],
    similarity_min: f64,
) -> anyhow::Result<CachedRefinement> {
    // Compute the 4a scores from the member token sequences.
    let seqs: Vec<Vec<String>> = members.iter().map(|m| m.seq.clone()).collect();
    let (lcs_ratio, lcs_sampled) = class_lcs_ratio(&seqs);
    let refactorability = refactorability_v1(lcs_ratio);
    let confidence = confidence_v1(lcs_ratio, similarity_min);

    let template = lcs_skeleton(&seqs);

    // Persist a minimal row. INSERT OR REPLACE because `class_key` is the table PRIMARY KEY — a
    // re-run at a NEW version pin replaces the stale row in place rather than accumulating.
    conn.execute(
        "INSERT OR REPLACE INTO clone_refinements(
             class_key, language, refine_mode, template,
             variation_points_json, proposed_signature_json, confidence,
             anti_unify_coverage, lcs_ratio, refactorability,
             norm_version, alignment_version, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            refinement_key,
            language,
            REFINE_MODE,
            template,
            "[]",
            "{}",
            confidence.as_db_str(),
            lcs_ratio, // anti_unify_coverage proxy
            lcs_ratio,
            refactorability,
            NORM_VERSION,
            ALIGNMENT_VERSION,
            crate::index::now_ms(),
        ],
    )?;

    Ok(CachedRefinement {
        lcs_ratio,
        confidence,
        refactorability,
        refine_mode: REFINE_MODE,
        lcs_sampled,
    })
}

/// Read-through refinement (writable-connection convenience): [`refine_lookup`] on hit, else
/// [`refine_compute_and_store`]. Requires a writable connection on a cache miss. The RO-aware read
/// path (`refine_class_in_place`) calls the two halves directly so it can probe the cache on a
/// read-only connection and surface `SQLITE_READONLY` BEFORE any expensive compute.
pub(crate) fn refine_class(
    conn: &Connection,
    refinement_key: &str,
    language: &str,
    members: &[RefineMember],
    similarity_min: f64,
) -> anyhow::Result<CachedRefinement> {
    if let Some(cached) = refine_lookup(conn, refinement_key)? {
        return Ok(cached);
    }
    refine_compute_and_store(conn, refinement_key, language, members, similarity_min)
}

/// Crude 4a LCS skeleton: the LCS-common token run of the first member pair (exact for a 2-member
/// class; a heuristic skeleton for larger classes), space-joined. 4b replaces this with the full
/// N-way anti-unification template; for 4a it is a legible, deterministic placeholder.
fn lcs_skeleton(seqs: &[Vec<String>]) -> String {
    match seqs {
        [] | [_] => seqs.first().map(|s| s.join(" ")).unwrap_or_default(),
        [a, b, ..] => {
            let aln = lcs_align(a, b);
            let matched: Vec<&str> = aln
                .ops
                .iter()
                .filter_map(|op| match op {
                    super::align::AlignOp::Match(i, _) => a.get(*i).map(String::as_str),
                    _ => None,
                })
                .collect();
            matched.join(" ")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refinement_key_is_order_independent_over_struct_hashes() {
        let k1 = refinement_key("rust", &["aaa".into(), "bbb".into(), "ccc".into()]);
        let k2 = refinement_key("rust", &["ccc".into(), "aaa".into(), "bbb".into()]);
        assert_eq!(k1, k2, "the same struct_hash multiset must address the same refinement");
    }

    #[test]
    fn refinement_key_distinguishes_language_and_multiset() {
        let base = refinement_key("rust", &["aaa".into(), "bbb".into()]);
        // Different language → different key.
        assert_ne!(base, refinement_key("typescript", &["aaa".into(), "bbb".into()]));
        // Different multiset → different key.
        assert_ne!(base, refinement_key("rust", &["aaa".into(), "ccc".into()]));
        // Duplicate-sensitive: a doubled hash is a different multiset.
        assert_ne!(base, refinement_key("rust", &["aaa".into(), "aaa".into(), "bbb".into()]));
    }

    #[test]
    fn lcs_skeleton_of_identical_pair_is_the_whole_seq() {
        let a: Vec<String> =
            ["fn", "f", "(", ")", "{", "}"].iter().map(|s| s.to_string()).collect();
        let seqs = vec![a.clone(), a.clone()];
        assert_eq!(lcs_skeleton(&seqs), a.join(" "));
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

        let key = refinement_key("rust", &["h1".into(), "h2".into()]);
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

        // Two identical ordered token sequences ⇒ a clean, near-perfect class.
        let seq: Vec<String> = ["fn", "f", "(", ")", "{", "g", "(", ")", ";", "}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mk = |id: i64| RefineMember {
            symbol_id: id,
            lang: crate::language::Language::Rust,
            path: format!("src/m{id}.rs"),
            start_byte: 0,
            end_byte: seq.len(),
            struct_hash: format!("h{id}"),
            seq: seq.clone(),
        };
        let members = vec![mk(1), mk(2)];
        let key = refinement_key("rust", &["h1".into(), "h2".into()]);

        // Miss: compute + persist one row.
        let first = refine_class(&conn, &key, "rust", &members, 1.0).unwrap();
        assert_eq!(count_rows(&conn), 1, "the miss persists exactly one row");
        assert!(first.lcs_ratio > 0.99, "identical sequences ⇒ near-perfect lcs_ratio");

        // Hit: same key serves the cache, no new row.
        let second = refine_class(&conn, &key, "rust", &members, 1.0).unwrap();
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
}
