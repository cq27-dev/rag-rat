//! Selector resolution + ineligibility classification for `clones_for_symbol`.
//!
//! [`resolve_selector_to_symbol_id`] turns a [`CloneSymbolSelector`] (id handle / qualified ref /
//! path+line) into an in-scope `symbols.id`; [`classify_ineligibility_reason`] names WHY a resolved
//! symbol carries no current-version fingerprint (generated / stale normalizer / non-function /
//! below-min-tokens) in priority order.

use rag_rat_clones::NORM_VERSION;
use rusqlite::{Connection, OptionalExtension};

use super::types::{CloneIneligibilityReason, CloneSymbolSelector};

/// Resolve a [`CloneSymbolSelector`] to an in-scope `symbols.id` rowid, or `None` if the selector
/// doesn't match any symbol in the active scope.
pub(crate) fn resolve_selector_to_symbol_id(
    conn: &Connection,
    selector: &CloneSymbolSelector,
) -> anyhow::Result<Option<i64>> {
    match selector {
        CloneSymbolSelector::Id(handle) => {
            let Some(logical_id) = rag_rat_base::serde_big_id::parse_sym_handle(handle) else {
                return Ok(None);
            };
            // A logical-symbol may have multiple member rows (cfg splits, overloads). We PREFER
            // a fingerprinted member: a cfg-split logical symbol whose lowest-rowid member is
            // below MIN_TOKENS or unfingerprinted but whose sibling IS fingerprinted (and in a
            // clone class) would otherwise report `symbol_fingerprinted=false` and miss the class.
            // `(sf.symbol_id IS NULL) ASC` sorts fingerprinted members first (NULL = unmatched →
            // treated as 1 in SQLite boolean context → sorts after matched rows); falls back to
            // lowest-rowid when no member is fingerprinted, so `symbol_resolved=true,
            // symbol_fingerprinted=false` is still correctly reported.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT lm.symbol_id
                     FROM logical_symbol_members lm
                     JOIN symbols ON symbols.id = lm.symbol_id
                     JOIN files ON files.id = symbols.file_id
                     LEFT JOIN symbol_fingerprints sf
                       ON sf.symbol_id = lm.symbol_id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?2
                     WHERE lm.logical_symbol_id = ?1
                     ORDER BY (sf.symbol_id IS NULL) ASC, lm.symbol_id ASC
                     LIMIT 1",
                    rusqlite::params![logical_id, NORM_VERSION],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
        CloneSymbolSelector::Ref(qualified_name) => {
            // Exact qualified-name match through the scoped `files` view.
            // Ambiguity rule: collect ALL current-version fingerprinted symbols matching this ref.
            // - 0 fingerprinted → fall back to lowest-rowid unfingerprinted match (preserved
            //   "resolved but not fingerprinted" path: symbol_resolved=true,
            //   symbol_fingerprinted=false), or None if no symbol at all.
            // - 1 fingerprinted → use it (unambiguous).
            // - >1 fingerprinted → REJECT with a clear error: the ref maps to multiple distinct
            //   logical symbols (overloads, cfg variants) — the caller must disambiguate with Id or
            //   PathLine. Silently picking one could return an unrelated overload's class.
            let mut fingerprinted_ids: Vec<i64> = conn
                .prepare(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                     JOIN symbol_fingerprints sf
                       ON sf.symbol_id = symbols.id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?2
                     WHERE ns.value = ?1
                     ORDER BY symbols.id ASC",
                )?
                .query_map(rusqlite::params![qualified_name.as_str(), NORM_VERSION], |row| {
                    row.get(0)
                })?
                .collect::<Result<_, _>>()?;
            // Deduplicate: the same symbols.id can appear multiple times if there are multiple
            // fingerprint rows (shouldn't happen for a normalizer_version-locked query, but be
            // safe).
            fingerprinted_ids.dedup();

            if fingerprinted_ids.len() > 1 {
                let n = fingerprinted_ids.len();
                anyhow::bail!(
                    "clones_for_symbol: ref '{}' matches {} fingerprinted symbols (overloads/cfg \
                     variants) — use id or path+line to disambiguate",
                    qualified_name,
                    n
                );
            }
            if let Some(&id) = fingerprinted_ids.first() {
                return Ok(Some(id));
            }
            // 0 fingerprinted matches: fall back to lowest-rowid unfingerprinted symbol for the
            // "resolved but not fingerprinted" path.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     JOIN name_strings ns ON ns.id = symbols.qualified_name_id
                     WHERE ns.value = ?1
                     ORDER BY symbols.id ASC
                     LIMIT 1",
                    rusqlite::params![qualified_name.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
        CloneSymbolSelector::PathLine { path, line } => {
            // Tightest-spanning symbol whose range contains `line`: smallest (end_line -
            // start_line) among symbols where start_line <= line <= end_line.
            // CONTRACT: span is the PRIMARY key — return the symbol AT the cursor and let the
            // eligibility flags report it as not-fingerprinted; do NOT silently jump to an
            // enclosing fingerprinted function. Fingerprint presence is a TIE-BREAKER only:
            // among symbols with equal span, prefer the fingerprinted variant (so a cfg-split
            // same-span pair doesn't pick the unfingerprinted one and miss the clone class).
            // rowid is the final stable tie-breaker.
            let id: Option<i64> = conn
                .query_row(
                    "SELECT symbols.id
                     FROM symbols
                     JOIN files ON files.id = symbols.file_id
                     LEFT JOIN symbol_fingerprints sf
                       ON sf.symbol_id = symbols.id
                       AND sf.normalizer_kind = 'baseline'
                       AND sf.normalizer_version = ?3
                     WHERE files.path = ?1
                       AND ?2 BETWEEN symbols.start_line AND symbols.end_line
                     ORDER BY (symbols.end_line - symbols.start_line) ASC, (sf.symbol_id IS NULL) \
                     ASC, symbols.id ASC
                     LIMIT 1",
                    rusqlite::params![path.as_str(), line, NORM_VERSION],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(id)
        },
    }
}

/// Classify WHY a *resolved* symbol is not clone-eligible (#274 item 3a) — only called once
/// `clones_for_symbol` has established `symbol_resolved = true` AND the symbol is absent from the
/// candidate bags (`symbol_fingerprinted = false`). Queries the DB for the three discriminating
/// facts — is the file generated, what is the symbol's `kind`, and does ANY baseline fingerprint
/// row exist (at any `normalizer_version`) — and resolves them in PRIORITY order so exactly one
/// reason is reported:
///
/// 1. [`Generated`](CloneIneligibilityReason::Generated) — `files.generated = 1`. Checked first
///    because generated symbols are never fingerprinted regardless of kind/size, AND the read
///    filter (`load_scoped_baseline_bags`) excludes them even if a stale row lingers from before a
///    target-reclassification.
/// 2. [`StaleNormalizerVersion`](CloneIneligibilityReason::StaleNormalizerVersion) — a baseline row
///    EXISTS but not at the current [`NORM_VERSION`]. Checked before the kind test because a
///    function-VALUED declarator (`const f = () => …`, #232 #5) keeps `kind = "const"` yet IS
///    fingerprinted, so an existing row is the authoritative "this symbol was eligible" signal —
///    the index is merely stale.
/// 3. [`NonFunctionKind`](CloneIneligibilityReason::NonFunctionKind) — `kind != "function"` and no
///    fingerprint row exists. (A large function-valued declarator is caught by rule 2; a tiny one
///    with a non-`function` kind is honestly reported here — both `NonFunctionKind` and
///    `BelowMinTokens` are true, and the literal `kind` fact is the one the DB can attest to
///    without the AST.)
/// 4. [`BelowMinTokens`](CloneIneligibilityReason::BelowMinTokens) — the residual: a `kind =
///    "function"` symbol in a non-generated file with no current-version row, i.e. its normalized
///    body fell below [`MIN_TOKENS`](crate::index::clones).
pub(crate) fn classify_ineligibility_reason(
    conn: &Connection,
    symbol_id: i64,
) -> anyhow::Result<CloneIneligibilityReason> {
    // The three discriminating facts in one read: the file's generated flag, the symbol kind, and
    // whether ANY baseline fingerprint row exists (any normalizer_version) — `EXISTS` so the kind
    // and generated columns stay single-row.
    let (generated, kind, has_any_baseline_fp): (bool, String, bool) = conn.query_row(
        "SELECT files.generated,
                symbols.kind,
                EXISTS(
                    SELECT 1 FROM symbol_fingerprints sf
                    WHERE sf.symbol_id = symbols.id AND sf.normalizer_kind = 'baseline'
                )
         FROM symbols
         JOIN files ON files.id = symbols.file_id
         WHERE symbols.id = ?1",
        rusqlite::params![symbol_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    Ok(if generated {
        CloneIneligibilityReason::Generated
    } else if has_any_baseline_fp {
        // A row exists but the current-version read missed it (the caller already established
        // `symbol_fingerprinted = false`) ⇒ the only stored row is at a stale normalizer_version.
        CloneIneligibilityReason::StaleNormalizerVersion
    } else if kind != "function" {
        CloneIneligibilityReason::NonFunctionKind
    } else {
        CloneIneligibilityReason::BelowMinTokens
    })
}
