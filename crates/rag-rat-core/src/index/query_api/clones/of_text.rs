//! Clone-check of arbitrary, not-yet-indexed text (#287): fingerprint the functions in `text`
//! in-memory and find their EXACT (`struct_hash`) and NEAR (≥θ overlap) clones among the INDEXED
//! symbols — the engine behind a write-time "you just wrote a clone" check for coding agents.
//!
//! Read-only and best-effort: it never writes and returns an empty list (a no-op) when there's
//! nothing to compare against (empty/absent fingerprints) or the text doesn't parse — so a caller
//! (e.g. a Write/Edit hook) can run it unconditionally without ever blocking the agent. It reuses
//! the parent `clones` module's EXACT candidate-gen primitives (`sub_block_tokens`, `overlap`,
//! `verified_clone`) so a near match is the same relation `find_clones` would report.
//!
//! Self-matches (a function vs its own already-indexed copy, when editing in place) are NOT
//! filtered here — the engine is pure; the caller, which knows the file being written, excludes its
//! own path.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

use super::{
    DF_FALLBACK, HYDRATION_CHUNK, SymbolBag, THETA, TokenPosting, load_scoped_baseline_bags,
    overlap, sub_block_tokens, verified_clone,
};
use crate::index::clones::{SymbolFingerprint, fingerprint_symbols};
use crate::index::{IndexDatabase, parser, symbols};
use crate::language::Language;

/// One just-written function that duplicates existing indexed code.
#[derive(Debug, Clone, Serialize)]
pub struct TextCloneMatch {
    /// The new function's name, as parsed from the text.
    pub name: String,
    /// 1-based line of the new function within the supplied text.
    pub start_line: usize,
    /// `"exact"` (identical structure — same `struct_hash`) or `"near"` (≥θ overlap).
    pub kind: &'static str,
    /// `overlap / max_len` similarity in `[θ, 1.0]`; `1.0` for an exact match.
    pub similarity: f64,
    /// Existing indexed refs (`path::name`) this function clones — sorted, deduplicated.
    pub clone_of: Vec<String>,
}

impl IndexDatabase {
    /// Find EXACT + NEAR clones of every fingerprintable function in `text` among the indexed
    /// symbols. `path`/`language` drive PARSING only — `text` need not be saved or indexed. An
    /// empty result means no clones OR nothing to compare against (a best-effort no-op, never
    /// an error for an unparseable/empty input).
    pub fn clones_of_text(
        &self,
        text: &str,
        language: Language,
        path: &Path,
    ) -> anyhow::Result<Vec<TextCloneMatch>> {
        let conn = self.storage.connection();

        // Fingerprint the new functions in-memory — the exact pipeline indexing uses.
        let syms = symbols::symbols_for_file(path, language, text);
        let Some(parsed) = parser::parse_file(path, language, text) else {
            return Ok(Vec::new());
        };
        let new_fps = fingerprint_symbols(parsed.root(), text, language, &syms);
        if new_fps.is_empty() {
            return Ok(Vec::new());
        }

        // Compare against the indexed baseline bags. Empty/absent index → nothing to clone → no-op.
        let indexed = load_scoped_baseline_bags(conn)?;
        if indexed.is_empty() {
            return Ok(Vec::new());
        }
        let by_id: BTreeMap<i64, &SymbolBag> = indexed.iter().map(|b| (b.symbol_id, b)).collect();
        let df_by_token = load_clone_token_df(conn)?;

        // `(struct_hash, language) -> ids` (exact) and the sub-block `token -> ids` inverted index
        // (near) — built once over the indexed bags.
        let mut by_struct: HashMap<(&str, &str), Vec<i64>> = HashMap::new();
        let mut inverted: HashMap<i64, Vec<i64>> = HashMap::new();
        for bag in &indexed {
            by_struct
                .entry((bag.struct_hash.as_str(), bag.language.as_str()))
                .or_default()
                .push(bag.symbol_id);
            for token in sub_block_tokens(bag, THETA) {
                inverted.entry(token).or_default().push(bag.symbol_id);
            }
        }

        let lang = language.as_str();
        let mut id_set: BTreeSet<i64> = BTreeSet::new();
        // (name, start_line, kind, similarity, existing_ids)
        let mut pending: Vec<(String, usize, &'static str, f64, Vec<i64>)> = Vec::new();

        for (local, fp) in &new_fps {
            let symbol = &syms[*local];
            let new_bag = bag_from_fingerprint(fp, lang, &df_by_token);

            // EXACT: identical structure (same struct_hash + language) → similarity 1.0.
            let exact: Vec<i64> =
                by_struct.get(&(fp.struct_hash.as_str(), lang)).cloned().unwrap_or_default();
            let exact_set: BTreeSet<i64> = exact.iter().copied().collect();
            if !exact.is_empty() {
                id_set.extend(&exact);
                pending.push((symbol.name.clone(), symbol.start_line, "exact", 1.0, exact));
            }

            // NEAR: candidates share a sub-block token; kept by the exact overlap verify. Exclude
            // ids already reported as exact for this function.
            let mut cand: BTreeSet<i64> = BTreeSet::new();
            for token in sub_block_tokens(&new_bag, THETA) {
                if let Some(ids) = inverted.get(&token) {
                    cand.extend(ids);
                }
            }
            let mut near_ids: Vec<i64> = Vec::new();
            let mut best_sim = 0.0_f64;
            for id in cand {
                if exact_set.contains(&id) {
                    continue;
                }
                let other = by_id[&id];
                if other.language != lang || !verified_clone(&new_bag, other, THETA) {
                    continue;
                }
                let max_len = new_bag.token_len.max(other.token_len);
                let sim = if max_len == 0 {
                    1.0
                } else {
                    overlap(&new_bag, other) as f64 / max_len as f64
                };
                best_sim = best_sim.max(sim);
                near_ids.push(id);
            }
            if !near_ids.is_empty() {
                id_set.extend(&near_ids);
                pending.push((symbol.name.clone(), symbol.start_line, "near", best_sim, near_ids));
            }
        }

        let refs = resolve_refs(conn, &id_set)?;
        let mut matches: Vec<TextCloneMatch> = pending
            .into_iter()
            .map(|(name, start_line, kind, similarity, ids)| {
                let mut clone_of: Vec<String> =
                    ids.iter().filter_map(|id| refs.get(id).cloned()).collect();
                clone_of.sort();
                clone_of.dedup();
                TextCloneMatch { name, start_line, kind, similarity, clone_of }
            })
            .filter(|m| !m.clone_of.is_empty())
            .collect();
        matches.sort_by(|a, b| {
            a.start_line
                .cmp(&b.start_line)
                .then_with(|| a.name.cmp(&b.name))
                .then(a.kind.cmp(b.kind))
        });
        Ok(matches)
    }
}

/// `token_hash -> df` over the baseline normalizer — the selectivity source for a NEW bag's
/// sub-block ordering. A token absent from the index is maximally selective (`DF_FALLBACK`),
/// matching `load_scoped_baseline_bags`.
fn load_clone_token_df(conn: &Connection) -> anyhow::Result<HashMap<i64, i64>> {
    let mut stmt = conn
        .prepare("SELECT token_hash, df FROM clone_token_df WHERE normalizer_kind = 'baseline'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
    let mut map = HashMap::new();
    for row in rows {
        let (token_hash, df) = row?;
        map.insert(token_hash, df);
    }
    Ok(map)
}

/// A candidate-gen [`SymbolBag`] for a fresh, not-yet-indexed fingerprint (`symbol_id = -1`).
/// Tokens carry the indexed df (or `DF_FALLBACK`), matching `load_scoped_baseline_bags` so
/// `sub_block_tokens` orders identically.
fn bag_from_fingerprint(
    fp: &SymbolFingerprint,
    language: &str,
    df_by_token: &HashMap<i64, i64>,
) -> SymbolBag {
    let tokens = fp
        .token_bag
        .iter()
        .map(|&(token_hash, freq)| TokenPosting {
            token_hash,
            freq,
            coalesced_df: df_by_token.get(&token_hash).copied().unwrap_or(DF_FALLBACK),
        })
        .collect();
    SymbolBag {
        symbol_id: -1,
        language: language.to_string(),
        struct_hash: fp.struct_hash.clone(),
        token_len: fp.token_len,
        tokens,
    }
}

/// Resolve indexed symbol ids → `path::name` refs (the same `name_strings` source as
/// `CloneMember.ref`), chunked to respect SQLite's bound-parameter limit.
fn resolve_refs(conn: &Connection, ids: &BTreeSet<i64>) -> anyhow::Result<HashMap<i64, String>> {
    let ids: Vec<i64> = ids.iter().copied().collect();
    let mut refs = HashMap::new();
    for chunk in ids.chunks(HYDRATION_CHUNK) {
        let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT symbols.id, ns.value FROM symbols
             JOIN name_strings ns ON ns.id = symbols.qualified_name_id
             WHERE symbols.id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, value) = row?;
            refs.insert(id, value);
        }
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    fn fixture_db(tag: &str) -> crate::IndexDatabase {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-of-text-{tag}-{}-{}",
            std::process::id(),
            crate::index::util::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        // An indexed function the new text will clone.
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n",
        )
        .unwrap();
        let config = crate::Config {
            root: root.clone(),
            database: root.join(".rag-rat/index.sqlite"),
            targets: vec![crate::config::ResolvedTarget {
                name: "rust".to_string(),
                language: crate::language::Language::Rust,
                directories: vec![std::path::PathBuf::from("src")],
                include: vec!["src/".to_string()],
                exclude: Vec::new(),
                kind: crate::config::TargetKind::Source,
            }],
            local_ai: Default::default(),
            watch: Default::default(),
            version_check: Default::default(),
            oracle: Default::default(),
        };
        crate::IndexDatabase::rebuild(&config).unwrap()
    }

    #[test]
    fn finds_exact_and_near_clones_of_new_text() {
        let db = fixture_db("hit");

        // EXACT: byte-identical structure (renamed identifiers normalize away) to the indexed fn.
        let exact_text = "pub fn fetch_account(store: Db) -> i32 { let a = store.get(20); \
                          validate(a); a + 1 }\n";
        let exact = db
            .clones_of_text(
                exact_text,
                crate::language::Language::Rust,
                std::path::Path::new("new.rs"),
            )
            .unwrap();
        assert_eq!(exact.len(), 1, "one new function checked");
        assert_eq!(exact[0].kind, "exact");
        assert_eq!(exact[0].similarity, 1.0);
        assert!(
            exact[0].clone_of.iter().any(|r| r.contains("load_user")),
            "exact clone of load_user, got {:?}",
            exact[0].clone_of
        );
    }

    #[test]
    fn no_op_on_empty_index_and_unparseable_text() {
        let db = fixture_db("noop");
        // Garbage / non-function text → no fingerprints → empty, never an error.
        let none = db
            .clones_of_text(
                "not real code !!!",
                crate::language::Language::Rust,
                std::path::Path::new("x.rs"),
            )
            .unwrap();
        assert!(none.is_empty());
    }
}
