//! Clone-check of arbitrary, not-yet-indexed text (#287): fingerprint the functions in one or more
//! in-memory files and find their EXACT (`struct_hash`) and NEAR (≥θ overlap) clones among the
//! INDEXED symbols — the engine behind a write-time "you just wrote a clone" check for coding
//! agents.
//!
//! BATCH-first: [`IndexDatabase::clones_of_texts`] loads + structures the index ONCE and checks N
//! files against it (a MultiEdit, or a batch that touches several files, pays the load once).
//! [`IndexDatabase::clones_of_text`] is the single-file convenience.
//!
//! Read-only and best-effort: it never writes and returns an empty list (a no-op) when there's
//! nothing to compare against (empty/absent fingerprints) or a file doesn't parse — so a Write/Edit
//! hook can run it unconditionally without ever blocking the agent. It reuses the parent `clones`
//! module's EXACT candidate-gen primitives (`sub_block_tokens`, `overlap`, `verified_clone`) so a
//! near match is the same relation `find_clones` reports.
//!
//! Self-matches are filtered: a function is never reported as a clone of code in its OWN file
//! (`in_file`), so editing a function in place doesn't flag it against its own indexed copy.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use super::{
    DF_FALLBACK, HYDRATION_CHUNK, SymbolBag, THETA, TokenPosting, load_scoped_baseline_bags,
    overlap, sub_block_tokens, verified_clone,
};
use crate::index::clones::{SymbolFingerprint, fingerprint_symbols};
use crate::index::{IndexDatabase, parser, symbols};
use crate::language::Language;

/// One file to clone-check (owns its data so a batch can be assembled from disparate sources).
pub struct CloneCheckInput {
    pub text: String,
    pub language: Language,
    /// Path of the file the text belongs to, RELATIVE to the index root — used both for parsing
    /// and to exclude self-matches (so it must match the indexed `path::name` ref form).
    pub path: PathBuf,
}

/// One just-written function that duplicates existing indexed code.
#[derive(Debug, Clone, Serialize)]
pub struct TextCloneMatch {
    /// The input file (relative path) the new function is in — set on every match so a batch
    /// result is self-describing.
    pub in_file: String,
    /// The new function's name, as parsed.
    pub name: String,
    /// 1-based line of the new function within its file's text.
    pub start_line: usize,
    /// `"exact"` (identical structure — same `struct_hash`) or `"near"` (≥θ overlap).
    pub kind: &'static str,
    /// `overlap / max_len` similarity in `[θ, 1.0]`; `1.0` for an exact match.
    pub similarity: f64,
    /// Existing indexed refs (`path::name`) this function clones — sorted, deduplicated, self-file
    /// excluded.
    pub clone_of: Vec<String>,
}

impl IndexDatabase {
    /// Find EXACT + NEAR clones of every fingerprintable function in `text` among the indexed
    /// symbols (single-file convenience over [`Self::clones_of_texts`]).
    pub fn clones_of_text(
        &self,
        text: &str,
        language: Language,
        path: &Path,
    ) -> anyhow::Result<Vec<TextCloneMatch>> {
        let conn = self.storage.connection();
        let Some(index) = CheckIndex::load(conn)? else {
            return Ok(Vec::new());
        };
        index.check(conn, text, language, path)
    }

    /// Cheap count of fingerprinted baseline functions — a write-time hook reads it to BOUND its
    /// cost: `clones_of_text(s)` loads every bag + builds an in-RAM inverted index (O(functions))
    /// until the persisted-postings follow-up lands, so a hook skips (no-ops) above a threshold
    /// rather than risk a perceptible delay on a very large repo.
    pub fn clone_check_function_count(&self) -> anyhow::Result<u64> {
        let count: i64 = self.storage.connection().query_row(
            "SELECT COUNT(*) FROM symbol_fingerprints WHERE normalizer_kind = 'baseline'",
            [],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    /// BATCH clone-check: load + structure the index ONCE, then check every input file against it.
    /// An empty index (nothing to compare against) yields an empty result — a best-effort no-op.
    pub fn clones_of_texts(
        &self,
        inputs: &[CloneCheckInput],
    ) -> anyhow::Result<Vec<TextCloneMatch>> {
        let conn = self.storage.connection();
        let Some(index) = CheckIndex::load(conn)? else {
            return Ok(Vec::new());
        };
        let mut all = Vec::new();
        for input in inputs {
            all.extend(index.check(conn, &input.text, input.language, &input.path)?);
        }
        Ok(all)
    }
}

/// The indexed baseline bags + the lookup structures a clone-check needs, built ONCE so a batch
/// reuses them. Holds the bags by value + an id→index map (rather than a borrowing `by_id`) so it
/// is not a self-referential struct.
struct CheckIndex {
    indexed: Vec<SymbolBag>,
    id_to_idx: HashMap<i64, usize>,
    /// `(struct_hash, language) -> ids` for the exact fast path.
    by_struct: HashMap<(String, String), Vec<i64>>,
    /// sub-block `token -> ids` inverted index for the near path.
    inverted: HashMap<i64, Vec<i64>>,
    df_by_token: HashMap<i64, i64>,
}

impl CheckIndex {
    fn load(conn: &Connection) -> anyhow::Result<Option<Self>> {
        let indexed = load_scoped_baseline_bags(conn)?;
        if indexed.is_empty() {
            return Ok(None);
        }
        let df_by_token = load_clone_token_df(conn)?;
        let mut id_to_idx = HashMap::with_capacity(indexed.len());
        let mut by_struct: HashMap<(String, String), Vec<i64>> = HashMap::new();
        let mut inverted: HashMap<i64, Vec<i64>> = HashMap::new();
        for (idx, bag) in indexed.iter().enumerate() {
            id_to_idx.insert(bag.symbol_id, idx);
            by_struct
                .entry((bag.struct_hash.clone(), bag.language.clone()))
                .or_default()
                .push(bag.symbol_id);
            for token in sub_block_tokens(bag, THETA) {
                inverted.entry(token).or_default().push(bag.symbol_id);
            }
        }
        Ok(Some(Self { indexed, id_to_idx, by_struct, inverted, df_by_token }))
    }

    fn bag(&self, id: i64) -> &SymbolBag {
        &self.indexed[self.id_to_idx[&id]]
    }

    /// Clone-check one file's `text` against the loaded index, resolving refs and excluding
    /// self-file matches.
    fn check(
        &self,
        conn: &Connection,
        text: &str,
        language: Language,
        path: &Path,
    ) -> anyhow::Result<Vec<TextCloneMatch>> {
        let syms = symbols::symbols_for_file(path, language, text);
        let Some(parsed) = parser::parse_file(path, language, text) else {
            return Ok(Vec::new());
        };
        let new_fps = fingerprint_symbols(parsed.root(), text, language, &syms);
        if new_fps.is_empty() {
            return Ok(Vec::new());
        }

        let lang = language.as_str();
        let in_file = path.to_string_lossy().into_owned();
        let self_prefix = format!("{in_file}::"); // refs starting with this are the file's own code

        let mut id_set: BTreeSet<i64> = BTreeSet::new();
        let mut pending: Vec<(String, usize, &'static str, f64, Vec<i64>)> = Vec::new();

        for (local, fp) in &new_fps {
            let symbol = &syms[*local];
            let new_bag = bag_from_fingerprint(fp, lang, &self.df_by_token);

            // EXACT: identical structure (same struct_hash + language) → similarity 1.0.
            let exact: Vec<i64> = self
                .by_struct
                .get(&(fp.struct_hash.clone(), lang.to_string()))
                .cloned()
                .unwrap_or_default();
            let exact_set: BTreeSet<i64> = exact.iter().copied().collect();
            if !exact.is_empty() {
                id_set.extend(&exact);
                pending.push((symbol.name.clone(), symbol.start_line, "exact", 1.0, exact));
            }

            // NEAR: candidates share a sub-block token; kept by the exact overlap verify. Exclude
            // ids already reported as exact for this function.
            let mut cand: BTreeSet<i64> = BTreeSet::new();
            for token in sub_block_tokens(&new_bag, THETA) {
                if let Some(ids) = self.inverted.get(&token) {
                    cand.extend(ids);
                }
            }
            let mut near_ids: Vec<i64> = Vec::new();
            let mut best_sim = 0.0_f64;
            for id in cand {
                if exact_set.contains(&id) {
                    continue;
                }
                let other = self.bag(id);
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
                let mut clone_of: Vec<String> = ids
                    .iter()
                    .filter_map(|id| refs.get(id).cloned())
                    // Self-file exclusion: a function isn't a clone of code in its own file (so an
                    // in-place edit doesn't flag against its own indexed copy).
                    .filter(|r| !r.starts_with(&self_prefix))
                    .collect();
                clone_of.sort();
                clone_of.dedup();
                TextCloneMatch {
                    in_file: in_file.clone(),
                    name,
                    start_line,
                    kind,
                    similarity,
                    clone_of,
                }
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
    use super::CloneCheckInput;

    fn fixture_db(tag: &str) -> crate::IndexDatabase {
        let root = std::env::temp_dir().join(format!(
            "rag-rat-of-text-{tag}-{}-{}",
            std::process::id(),
            crate::index::util::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
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
    fn finds_exact_clone_of_new_text() {
        let db = fixture_db("hit");
        let exact_text = "pub fn fetch_account(store: Db) -> i32 { let a = store.get(20); \
                          validate(a); a + 1 }\n";
        let hits = db
            .clones_of_text(
                exact_text,
                crate::language::Language::Rust,
                std::path::Path::new("new.rs"),
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "exact");
        assert_eq!(hits[0].in_file, "new.rs");
        assert!(hits[0].clone_of.iter().any(|r| r.contains("load_user")), "{:?}", hits[0].clone_of);
    }

    #[test]
    fn batch_checks_multiple_files_and_tags_in_file() {
        let db = fixture_db("batch");
        let inputs = vec![
            CloneCheckInput {
                text: "pub fn a_clone(s: Db) -> i32 { let x = s.get(1); validate(x); x + 1 }\n"
                    .to_string(),
                language: crate::language::Language::Rust,
                path: std::path::PathBuf::from("a.rs"),
            },
            CloneCheckInput {
                text: "pub fn unrelated() -> i32 { 42 }\n".to_string(),
                language: crate::language::Language::Rust,
                path: std::path::PathBuf::from("b.rs"),
            },
        ];
        let hits = db.clones_of_texts(&inputs).unwrap();
        // a.rs's clone is flagged; b.rs's trivial fn is below MIN_TOKENS / not a clone.
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].in_file, "a.rs");
        assert!(hits[0].clone_of.iter().any(|r| r.contains("load_user")));
    }

    #[test]
    fn excludes_self_file_so_in_place_edits_dont_self_flag() {
        let db = fixture_db("self");
        // Re-write load_user IN its own file: it must NOT be reported as a clone of its indexed
        // self.
        let edited = "pub fn load_user(db: Db) -> i32 { let u = db.get(10); validate(u); u + 1 }\n";
        let hits = db
            .clones_of_text(
                edited,
                crate::language::Language::Rust,
                std::path::Path::new("src/lib.rs"),
            )
            .unwrap();
        assert!(hits.is_empty(), "self-file match must be excluded, got {hits:?}");
    }

    #[test]
    fn no_op_on_unparseable_text() {
        let db = fixture_db("noop");
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
