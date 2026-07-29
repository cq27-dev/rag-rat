//! `/api/file/clones` composition (#216): every cloned symbol in one file,
//! each mapped to its coherent clone class with partners and — when the class
//! was refined — the extract-helper payload.
//!
//! This is the BATCHED sibling of `clones_for_symbol`: one bag load, one
//! pair computation, one component walk + coherence split for the whole file
//! instead of per symbol. The class-formation and subject-subclass selection
//! rules mirror it exactly (largest containing group, ties by min cohesion).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rag_rat_clones::refine::split::coherence_split_cancellable;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::super::clones::precompute::{live_generation_row, precomputed_pairs_if_eligible};
use super::super::clones::{
    CandidateCloneClass, SymbolBag, THETA, bucket_edges_by_component_cancellable,
    build_class_cancellable, candidate_pairs_from_bags_cancellable,
    components_from_pairs_cancellable, load_scoped_baseline_bags,
    min_pairwise_cohesion_cancellable, overlap,
};
use crate::index::IndexDatabase;

/// One cloned region in the requested file (the #216 `clone_regions[]` contract shape).
#[derive(Debug, Serialize)]
pub struct LensCloneRegion {
    pub start_line: i64,
    pub end_line: i64,
    pub byte_offset: i64,
    pub class_id: u64,
    pub class_key: String,
    pub symbol: String,
    pub max_similarity: Option<f64>,
    pub partners: Vec<LensClonePartner>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refine: Option<LensCloneRefine>,
}

#[derive(Debug, Serialize)]
pub struct LensClonePartner {
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub similarity: f64,
    pub symbol: Option<String>,
}

/// The extract-helper preview payload from `clone_refinements`, attached only
/// when the class was actually refined.
#[derive(Clone, Debug, Serialize)]
pub struct LensCloneRefine {
    pub template: String,
    pub variation_points: serde_json::Value,
    pub proposed_signature: serde_json::Value,
    pub confidence: String,
    pub anti_unify_coverage: f64,
    pub lcs_ratio: f64,
    pub refactorability: f64,
}

#[derive(Debug, Serialize)]
pub struct LensCloneGraphMeta {
    pub generation: Option<i64>,
    pub eligible: bool,
    pub stale: bool,
    pub source_revision: Option<String>,
    pub current_revision: Option<String>,
    pub finished_at_ms: Option<i64>,
    pub hidden_low_value_classes: usize,
    pub min_tokens: i64,
    pub theta: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct LensFileClones {
    pub clone_regions: Vec<LensCloneRegion>,
    #[serde(serialize_with = "serialize_clone_graph_meta")]
    pub clone_graph: LensCloneGraphMeta,
}

const MAX_PARTNERS: usize = 25;

#[derive(Default)]
pub(super) struct CloneFileMetrics {
    pub(super) partners: HashSet<String>,
    pub(super) max_similarity: f64,
}

/// (ref, path, start_line, end_line) for one symbol id.
type MemberInfo = (Option<String>, String, i64, i64);

/// Every input that determines the resolved graph. `delta_files_applied` is
/// the persisted mutation epoch for in-place edge rewrites, including
/// revision-neutral self-heals; the scope fields prevent row-id reuse across
/// a worktree or files-generation switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LensCloneGraphCacheKey {
    clone_generation: Option<i64>,
    delta_files_applied: i64,
    content_revision: String,
    theta_bits: u64,
    repo_id: String,
    commit_sha: String,
    worktree_id: String,
    files_generation: i64,
    files_sequence: i64,
    generated_flags_version: String,
}

/// One cached clone-graph build: the staleness-filtered, θ-gated edge set
/// with components. Held on `IndexDatabase` so the interactive file path does
/// not pay a repository-wide scan per request.
#[derive(Clone)]
pub(crate) struct LensCloneGraphCacheEntry {
    pub(crate) key: LensCloneGraphCacheKey,
    pub(crate) data: Arc<LensCloneGraphData>,
}

/// Opaque cache shared by short-lived lens read connections. It retains a small bounded set of
/// immutable variants because treemap and file-clone reads commonly use different theta values.
#[derive(Debug, Default)]
pub struct LensCloneGraphCache(pub(crate) std::sync::Mutex<Vec<LensCloneGraphCacheEntry>>);

const LENS_CLONE_GRAPH_CACHE_ENTRIES: usize = 2;

// SymbolBag has no Debug impl; report shapes, not contents.
impl std::fmt::Debug for LensCloneGraphCacheEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LensCloneGraphCacheEntry")
            .field("key", &self.key)
            .field("components", &self.data.components.len())
            .field("bags", &self.data.bags.len())
            .finish()
    }
}

// SymbolBag has no Debug impl; the cache key alone carries the diagnostics.
pub(crate) struct LensCloneGraphData {
    bags: Vec<SymbolBag>,
    components: Vec<Vec<i64>>,
    edges_by_component: Vec<Vec<(i64, i64)>>,
    member_component: HashMap<i64, usize>,
}

impl IndexDatabase {
    pub fn set_lens_clone_graph_cache(&mut self, cache: Arc<LensCloneGraphCache>) {
        self.lens_clone_graph_cache = cache;
    }

    /// `/api/file/clones?path=&theta=&min_tokens=` composition.
    ///
    /// Classes whose medoid token length falls below `min_tokens` are same-shape
    /// trivia (enum matchers, accessors — the #960 actionability audit) and are
    /// hidden, reported via `hidden_low_value_classes`.
    pub fn lens_file_clones(
        &self,
        path: &str,
        theta: f64,
        min_tokens: i64,
    ) -> anyhow::Result<LensFileClones> {
        let cancelled = AtomicBool::new(false);
        self.lens_file_clones_with_cancel(path, theta, min_tokens, &cancelled)
    }

    pub fn lens_file_clones_with_cancel(
        &self,
        path: &str,
        theta: f64,
        min_tokens: i64,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<LensFileClones> {
        anyhow::ensure!(theta.is_finite(), "theta must be finite");
        ensure_not_cancelled(cancelled)?;
        let theta = theta.clamp(THETA, 1.0);
        let min_tokens = min_tokens.max(0);
        let conn = self.storage.connection();
        let (meta, delta_files_applied) = self.lens_clone_graph_meta(conn, theta, min_tokens)?;
        if !meta.eligible {
            return Ok(LensFileClones { clone_regions: vec![], clone_graph: meta });
        }

        // Symbols of the requested file, in the ACTIVE scope (the temp files view).
        let mut stmt = conn.prepare(
            "SELECT s.id, s.name, s.start_byte, s.start_line, s.end_line FROM symbols s JOIN \
             files f ON f.id = s.file_id WHERE f.path = ?1 ORDER BY s.start_line, s.id",
        )?;
        let file_symbols: Vec<(i64, String, i64, i64, i64)> = stmt
            .query_map([path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if file_symbols.is_empty() {
            return Ok(LensFileClones { clone_regions: vec![], clone_graph: meta });
        }

        let Some(data) =
            self.lens_clone_graph_data(conn, theta, &meta, delta_files_applied, cancelled)?
        else {
            // Mirror `clones_for_symbol`'s source choice exactly (persisted graph fast path; a
            // missing/ineligible graph is honestly unavailable here).
            return Ok(LensFileClones {
                clone_regions: vec![],
                clone_graph: LensCloneGraphMeta { eligible: false, ..meta },
            });
        };
        let LensCloneGraphData { bags, components, edges_by_component, member_component } = &*data;
        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
        // Partner similarity, and the seed ranking the coherence split below reads. No
        // structural-hash special case is needed to score an exact clone as exact: `struct_hash`
        // and `token_bag` are both derived from the ONE normalized token sequence
        // `fingerprint_symbol` produces, so symbols identical after normalization have equal bags
        // and equal `token_len` — `overlap / max_len` is already exactly 1. Pinned by
        // `lens_file_clones_serves_coherent_class_with_partners`, which asserts 1.0 rather than
        // merely ≥ θ for a renamed clone.
        let sim = |a: i64, b: i64| -> f64 {
            let (ba, bb) = (by_id[&a], by_id[&b]);
            let max_len = ba.token_len.max(bb.token_len);
            if max_len == 0 { 1.0 } else { overlap(ba, bb) as f64 / max_len as f64 }
        };

        // Best coherent sub-class per file symbol (mirrors clones_for_symbol's
        // subject selection); identical classes shared by several in-file
        // subjects share one `class_id` so siblings paint one hue. The split
        // itself runs ONCE per component, not per subject.
        let mut coherent_by_component: HashMap<usize, Vec<Vec<i64>>> = HashMap::new();
        let mut refinements_by_class: HashMap<String, Option<LensCloneRefine>> = HashMap::new();
        // Memoize the built class per coherent subclass: several in-file subjects share one
        // subclass, and `build_class` re-runs per-member hydration + fan-in queries the lens
        // never reads differently between subjects of the same class.
        let mut built_by_subclass: HashMap<Vec<i64>, Option<CandidateCloneClass>> = HashMap::new();
        // Partner hydration is per SUBCLASS, not per subject: a file that contributes several
        // symbols to one clone class asks for the same members every time, and each ask is a
        // chunked row fetch over the whole class. Left uncached that is O(members-in-file ×
        // class-size) database work for one request — enough to push the clone lane into its
        // timeout on a file with many sibling clones. Memoized beside `built_by_subclass`, which
        // already recognizes that subclasses are shared.
        let mut members_by_subclass: HashMap<Vec<i64>, HashMap<i64, MemberInfo>> = HashMap::new();
        let mut hidden_keys = std::collections::HashSet::new();
        let mut regions: Vec<LensCloneRegion> = Vec::new();
        let mut hidden = 0usize;
        for &(symbol_id, ref name, start_byte, start_line, end_line) in &file_symbols {
            ensure_not_cancelled(cancelled)?;
            let Some(&comp_idx) = member_component.get(&symbol_id) else { continue };
            if !by_id.contains_key(&symbol_id) {
                continue; // not fingerprinted (generated / below MIN_TOKENS / non-function)
            }
            let coherent = match coherent_by_component.entry(comp_idx) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let Some(split) = coherence_split_cancellable(
                        &components[comp_idx],
                        &edges_by_component[comp_idx],
                        sim,
                        || cancelled.load(Ordering::Relaxed),
                    ) else {
                        anyhow::bail!("lens request cancelled");
                    };
                    entry.insert(split)
                },
            };
            let mut subject_subclass: Option<&Vec<i64>> = None;
            for candidate in coherent.iter().filter(|class| class.contains(&symbol_id)) {
                ensure_not_cancelled(cancelled)?;
                let replace = match subject_subclass {
                    None => true,
                    Some(best) => {
                        let ordering = match best.len().cmp(&candidate.len()) {
                            std::cmp::Ordering::Equal => {
                                let Some(best_cohesion) =
                                    min_pairwise_cohesion_cancellable(best, &by_id, cancelled)
                                else {
                                    anyhow::bail!("lens request cancelled");
                                };
                                let Some(candidate_cohesion) =
                                    min_pairwise_cohesion_cancellable(candidate, &by_id, cancelled)
                                else {
                                    anyhow::bail!("lens request cancelled");
                                };
                                best_cohesion
                                    .partial_cmp(&candidate_cohesion)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            },
                            ordering => ordering,
                        };
                        ordering.then_with(|| candidate.cmp(best)).is_lt()
                    },
                };
                ensure_not_cancelled(cancelled)?;
                if replace {
                    subject_subclass = Some(candidate);
                }
            }
            let Some(subclass) = subject_subclass else { continue };
            ensure_not_cancelled(cancelled)?;
            let built = match built_by_subclass.entry(subclass.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let built = build_class_cancellable(
                        subclass,
                        &by_id,
                        conn,
                        Some(symbol_id),
                        cancelled,
                    )?;
                    entry.insert(built)
                },
            };
            let Some(built) = built.as_mut() else { continue };
            if built.body_token_len_medoid < min_tokens {
                // Count hidden CLASSES, not member visits: several in-file
                // subjects can share one low-token class. Filter BEFORE the
                // refine pass — trivia classes must not pay anti-unification.
                if hidden_keys.insert(built.class_key.clone()) {
                    hidden += 1;
                }
                continue;
            }
            let refine = match refinements_by_class.get(&built.class_key) {
                Some(cached) => cached.clone(),
                None => {
                    ensure_not_cancelled(cancelled)?;
                    self.refine_class_from_cache(conn, subclass, &by_id, built)?;
                    ensure_not_cancelled(cancelled)?;
                    let refine = built.refined.then(|| LensCloneRefine {
                        template: built.template.clone().unwrap_or_default(),
                        variation_points: built
                            .variation_points
                            .clone()
                            .unwrap_or(serde_json::Value::Null),
                        proposed_signature: built
                            .proposed_signature
                            .clone()
                            .unwrap_or(serde_json::Value::Null),
                        confidence: built.confidence.clone().unwrap_or_default(),
                        anti_unify_coverage: built.anti_unify_coverage.unwrap_or(0.0),
                        lcs_ratio: built.lcs_ratio.unwrap_or(0.0),
                        refactorability: built.refactorability.unwrap_or(0.0),
                    });
                    refinements_by_class.insert(built.class_key.clone(), refine.clone());
                    refine
                },
            };
            let class_id = stable_class_id(&built.class_key);
            let info = match members_by_subclass.entry(subclass.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) =>
                    entry.insert(member_info(conn, subclass, cancelled)?),
            };
            let mut partners = Vec::new();
            for (index, id) in subclass.iter().enumerate() {
                if index.is_multiple_of(32) {
                    ensure_not_cancelled(cancelled)?;
                }
                if *id == symbol_id {
                    continue;
                }
                let Some((qref, ppath, sl, el)) = info.get(id) else { continue };
                partners.push(LensClonePartner {
                    path: ppath.clone(),
                    start_line: *sl,
                    end_line: *el,
                    similarity: sim(symbol_id, *id),
                    symbol: qref.as_deref().map(member_name),
                });
                if partners.len() == MAX_PARTNERS * 2 {
                    sort_and_cap_partners(&mut partners);
                }
            }
            ensure_not_cancelled(cancelled)?;
            sort_and_cap_partners(&mut partners);
            let max_similarity = partners.iter().map(|p| p.similarity).reduce(f64::max);
            regions.push(LensCloneRegion {
                start_line,
                end_line,
                byte_offset: start_byte,
                class_id,
                class_key: built.class_key.clone(),
                symbol: name.clone(),
                max_similarity,
                partners,
                refine,
            });
        }
        ensure_not_cancelled(cancelled)?;
        regions.sort_by_key(|r| r.start_line);
        ensure_not_cancelled(cancelled)?;
        Ok(LensFileClones {
            clone_regions: regions,
            clone_graph: LensCloneGraphMeta { hidden_low_value_classes: hidden, ..meta },
        })
    }

    fn lens_clone_graph_data(
        &self,
        conn: &Connection,
        theta: f64,
        meta: &LensCloneGraphMeta,
        delta_files_applied: i64,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<Option<Arc<LensCloneGraphData>>> {
        ensure_not_cancelled(cancelled)?;
        // Cache check FIRST: only a key miss pays the repository-wide bag load + edge scan +
        // component build. AUTOINCREMENT covers byte-identical file replacement; the generated
        // flags version covers the sole in-place clone-membership rewrite.
        let key = LensCloneGraphCacheKey {
            clone_generation: meta.generation,
            delta_files_applied,
            content_revision: meta.current_revision.clone().unwrap_or_default(),
            theta_bits: theta.to_bits(),
            repo_id: self.active_repo_id.clone(),
            commit_sha: self.active_commit_sha.clone(),
            worktree_id: self.active_worktree_id.clone(),
            files_generation: self.active_generation,
            files_sequence: conn
                .query_row("SELECT seq FROM sqlite_sequence WHERE name = 'files'", [], |row| {
                    row.get(0)
                })
                .optional()?
                .unwrap_or(0),
            generated_flags_version: self
                .meta(crate::index::GENERATED_FLAGS_VERSION_KEY)?
                .unwrap_or_default(),
        };
        // Single-flight: the lock is held across the build below so a second caller reuses the
        // entry instead of rebuilding the repository-wide graph. Waiting here costs whatever
        // resource the caller already holds — for the Lens server that is a bounded database
        // worker — so callers that share this cache are expected to serialize their ENTRY
        // upstream, where a waiter holds nothing. This loop stays cancellation-aware for the
        // callers that do contend.
        let mut cache = loop {
            match self.lens_clone_graph_cache.0.try_lock() {
                Ok(cache) => break cache,
                Err(std::sync::TryLockError::WouldBlock) => {
                    ensure_not_cancelled(cancelled)?;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                },
                Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
            }
        };
        if let Some(entry) = cache.iter().find(|entry| entry.key == key) {
            return Ok(Some(Arc::clone(&entry.data)));
        }
        let Some(built) = self.build_lens_clone_graph(conn, theta, cancelled)? else {
            return Ok(None);
        };
        let data = Arc::new(built);
        if cache.len() == LENS_CLONE_GRAPH_CACHE_ENTRIES {
            cache.remove(0);
        }
        cache.push(LensCloneGraphCacheEntry { key, data: Arc::clone(&data) });
        Ok(Some(data))
    }

    /// The cacheable graph build: scoped bags, the staleness-filtered θ-gated pair set,
    /// components, and edge buckets. Linked overlays use the scope-correct live pair source;
    /// base scope returns `None` when its persisted graph is missing or ineligible.
    fn build_lens_clone_graph(
        &self,
        conn: &Connection,
        theta: f64,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<Option<LensCloneGraphData>> {
        let bags = load_scoped_baseline_bags(conn)?;
        ensure_not_cancelled(cancelled)?;
        let by_id: BTreeMap<i64, &SymbolBag> = bags.iter().map(|b| (b.symbol_id, b)).collect();
        let pairs = if self.active_scope_is_linked_overlay() {
            // Persisted edges describe the base checkout. Recompute from the overlay-scoped bags
            // once per cache key so branch-only and edited clone members are represented safely.
            let Some(pairs) = candidate_pairs_from_bags_cancellable(&bags, theta, cancelled) else {
                anyhow::bail!("lens request cancelled");
            };
            pairs
        } else {
            let Some(pairs) = precomputed_pairs_if_eligible(conn, &by_id, theta)? else {
                return Ok(None);
            };
            pairs
        };
        ensure_not_cancelled(cancelled)?;
        let Some(components) = components_from_pairs_cancellable(&pairs, cancelled) else {
            anyhow::bail!("lens request cancelled");
        };
        let Some(edges_by_component) =
            bucket_edges_by_component_cancellable(&pairs, &components, cancelled)
        else {
            anyhow::bail!("lens request cancelled");
        };
        let member_component = components
            .iter()
            .enumerate()
            .flat_map(|(i, c)| c.iter().map(move |m| (*m, i)))
            .collect();
        Ok(Some(LensCloneGraphData { bags, components, edges_by_component, member_component }))
    }

    pub(super) fn lens_overlay_clone_metrics(
        &self,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<HashMap<String, CloneFileMetrics>> {
        let conn = self.storage.connection();
        let (meta, delta_files_applied) = self.lens_clone_graph_meta(conn, THETA, 0)?;
        let Some(data) =
            self.lens_clone_graph_data(conn, THETA, &meta, delta_files_applied, cancelled)?
        else {
            return Ok(HashMap::new());
        };
        let by_id: BTreeMap<i64, &SymbolBag> =
            data.bags.iter().map(|bag| (bag.symbol_id, bag)).collect();
        ensure_not_cancelled(cancelled)?;
        let paths: HashMap<i64, String> = conn
            .prepare(
                "SELECT symbol.id, file.path FROM symbols symbol JOIN files file ON file.id = \
                 symbol.file_id",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        ensure_not_cancelled(cancelled)?;
        let mut metrics = HashMap::new();
        for edges in &data.edges_by_component {
            ensure_not_cancelled(cancelled)?;
            for (edge_index, &(a, b)) in edges.iter().enumerate() {
                if edge_index.is_multiple_of(1024) {
                    ensure_not_cancelled(cancelled)?;
                }
                let (Some(a_path), Some(b_path), Some(a_bag), Some(b_bag)) =
                    (paths.get(&a), paths.get(&b), by_id.get(&a), by_id.get(&b))
                else {
                    continue;
                };
                let max_len = a_bag.token_len.max(b_bag.token_len);
                let structurally_identical =
                    a_bag.language == b_bag.language && a_bag.struct_hash == b_bag.struct_hash;
                let similarity = if structurally_identical || max_len == 0 {
                    1.0
                } else {
                    overlap(a_bag, b_bag) as f64 / max_len as f64
                };
                record_clone_metric(&mut metrics, a_path, b_path, similarity);
                record_clone_metric(&mut metrics, b_path, a_path, similarity);
            }
        }
        Ok(metrics)
    }

    fn lens_clone_graph_meta(
        &self,
        conn: &Connection,
        theta: f64,
        min_tokens: i64,
    ) -> anyhow::Result<(LensCloneGraphMeta, i64)> {
        if self.active_scope_is_linked_overlay() {
            let current_revision = self.content_revision().ok();
            return Ok((
                LensCloneGraphMeta {
                    generation: None,
                    eligible: true,
                    stale: false,
                    source_revision: current_revision.clone(),
                    current_revision,
                    finished_at_ms: None,
                    hidden_low_value_classes: 0,
                    min_tokens,
                    theta,
                    unavailable_reason: None,
                },
                0,
            ));
        }
        let row = live_generation_row(conn)?;
        let current_revision = self.content_revision().ok();
        let Some(r) = row else {
            return Ok((
                LensCloneGraphMeta {
                    generation: None,
                    eligible: false,
                    stale: false,
                    source_revision: None,
                    current_revision,
                    finished_at_ms: None,
                    hidden_low_value_classes: 0,
                    min_tokens,
                    theta,
                    unavailable_reason: Some("missing_generation"),
                },
                0,
            ));
        };
        let finished_at_ms: Option<i64> = conn
            .query_row(
                "SELECT finished_at_ms FROM clone_graph_generations WHERE generation = ?1",
                params![r.generation],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        let compatible = r.normalizer_version == rag_rat_clones::NORM_VERSION;
        Ok((
            LensCloneGraphMeta {
                generation: Some(r.generation),
                eligible: compatible,
                stale: current_revision.as_deref() != Some(r.source_revision.as_str()),
                source_revision: Some(r.source_revision),
                current_revision,
                finished_at_ms,
                hidden_low_value_classes: 0,
                min_tokens,
                theta,
                // Preserve WHY a present generation cannot serve, so the editor can distinguish
                // "rebuild needed" from "no clones".
                unavailable_reason: (!compatible).then_some("normalizer_mismatch"),
            },
            r.delta_files_applied,
        ))
    }
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> anyhow::Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Acquire), "lens request cancelled");
    Ok(())
}

pub(super) fn record_clone_metric(
    metrics: &mut HashMap<String, CloneFileMetrics>,
    path: &str,
    partner: &str,
    similarity: f64,
) {
    let metric = metrics.entry(path.to_string()).or_default();
    if path != partner {
        metric.partners.insert(partner.to_string());
    }
    metric.max_similarity = metric.max_similarity.max(similarity);
}

fn serialize_clone_graph_meta<S>(
    meta: &LensCloneGraphMeta,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match (meta.generation, meta.eligible) {
        (Some(_), _) | (_, true) => meta.serialize(serializer),
        (None, false) => serializer.serialize_none(),
    }
}

/// The read-side class key is 16 hex SHA-256 characters. Its first 13 fit in
/// JavaScript's exact 53-bit integer range, yielding a stable cross-request UI
/// color/group id while `class_key` remains the collision-resistant identity.
fn stable_class_id(class_key: &str) -> u64 {
    let prefix_len = class_key.len().min(13);
    u64::from_str_radix(&class_key[..prefix_len], 16).unwrap_or_default()
}

fn member_name(qualified_ref: &str) -> String {
    qualified_ref.rsplit("::").next().unwrap_or(qualified_ref).to_string()
}

fn sort_and_cap_partners(partners: &mut Vec<LensClonePartner>) {
    partners.sort_by(|a, b| {
        b.similarity.partial_cmp(&a.similarity).unwrap_or(std::cmp::Ordering::Equal)
    });
    partners.truncate(MAX_PARTNERS);
}

/// `ref@path:start-end` display info per member id, one batched query
/// (partner ids come from the scoped bags, so main-table joins are in-scope).
fn member_info(
    conn: &Connection,
    subclass: &[i64],
    cancelled: &AtomicBool,
) -> anyhow::Result<HashMap<i64, MemberInfo>> {
    let mut out = HashMap::with_capacity(subclass.len());
    for member_chunk in subclass.chunks(super::super::clones::HYDRATION_CHUNK) {
        ensure_not_cancelled(cancelled)?;
        let marks = member_chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT s.id, ns.value, f.path, s.start_line, s.end_line FROM symbols s JOIN files f \
             ON f.id = s.file_id LEFT JOIN name_strings ns ON ns.id = s.qualified_name_id WHERE \
             s.id IN ({marks})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(member_chunk.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get(3)?, r.get(4)?),
            ))
        })?;
        for (index, row) in rows.enumerate() {
            if index.is_multiple_of(256) {
                ensure_not_cancelled(cancelled)?;
            }
            let (id, info) = row?;
            out.insert(id, info);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;

    use super::*;

    #[test]
    fn member_info_hydrates_more_than_sqlites_minimum_variable_limit() {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute(
            "INSERT INTO files(path, language, kind, sha256, modified_at_ms, indexed_at_ms,
                               commit_sha, worktree_id)
             VALUES ('src/lib.rs', 'rust', 'source', 'sha', 0, 0, 'c', '')",
            [],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        let mut ids = Vec::new();
        for ordinal in 0..1_001 {
            conn.execute(
                "INSERT INTO symbols(file_id, language, name, kind, start_byte, end_byte,
                                     start_line, end_line)
                 VALUES (?1, 'rust', ?2, 'function', ?3, ?3 + 1, ?3 + 1, ?3 + 1)",
                params![file_id, format!("f{ordinal}"), ordinal],
            )
            .unwrap();
            ids.push(conn.last_insert_rowid());
        }
        crate::index::install_scope_view(&conn, "c", "").unwrap();
        let inserted = ids.len();
        ids.extend(10_000..50_000);

        let info = member_info(&conn, &ids, &AtomicBool::new(false)).unwrap();
        assert_eq!(info.len(), inserted);
    }
}
