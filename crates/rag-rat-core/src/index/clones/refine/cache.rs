//! Content-addressed refinement cache (#215 Plan 4a Task 4).
//!
//! [`refinement_key`] derives a STABLE key from the inputs that determine a refinement (language,
//! refine mode, the version pins, and the member `struct_hash` multiset) — NOT from the read-side
//! `class_key` (which is location-derived: `ref@path:start-end`). Two clone classes with the same
//! structural content therefore share a refinement even when they live at different locations, and
//! the key survives a reindex that reassigns rowids. [`refine_class`] is a read-through cache over
//! the `clone_refinements` table keyed by this content address: compute on miss, persist, return.

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
pub(crate) struct Refinement {
    pub(crate) lcs_ratio: f64,
    pub(crate) confidence: Confidence,
    pub(crate) refactorability: f64,
    pub(crate) refine_mode: &'static str,
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

/// Read-through refinement: return the cached `clone_refinements` row for `refinement_key`
/// (filtered to the current `NORM_VERSION` / `ALIGNMENT_VERSION` so a stale-version row never
/// serves), else compute the 4a scores from the member token sequences, persist a minimal row, and
/// return.
///
/// `similarity_min` is the Plan-2 pairwise floor for the class — it feeds the confidence band only,
/// not the cache key (the key is purely structural). The persisted `template` is a crude LCS
/// skeleton (4b fills the full anti-unification template + variation points); `anti_unify_coverage`
/// is the `lcs_ratio` as a 4a proxy.
pub(crate) fn refine_class(
    conn: &Connection,
    refinement_key: &str,
    language: &str,
    members: &[RefineMember],
    similarity_min: f64,
) -> anyhow::Result<Refinement> {
    // Cache HIT: a row keyed by this content address at the current version pins.
    let hit: Option<(f64, String, f64)> = conn
        .query_row(
            "SELECT lcs_ratio, confidence, refactorability
             FROM clone_refinements
             WHERE class_key = ?1 AND norm_version = ?2 AND alignment_version = ?3",
            rusqlite::params![refinement_key, NORM_VERSION, ALIGNMENT_VERSION],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((lcs_ratio, confidence, refactorability)) = hit {
        return Ok(Refinement {
            lcs_ratio,
            confidence: Confidence::from_db_str(&confidence),
            refactorability,
            refine_mode: REFINE_MODE,
        });
    }

    // Cache MISS: compute the 4a scores from the member token sequences.
    let seqs: Vec<Vec<String>> = members.iter().map(|m| m.seq.clone()).collect();
    let lcs_ratio = class_lcs_ratio(&seqs);
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

    Ok(Refinement { lcs_ratio, confidence, refactorability, refine_mode: REFINE_MODE })
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
}
