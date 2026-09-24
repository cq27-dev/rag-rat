//! Repo-memory vectors for the hybrid `memory_search` (#1443).
//!
//! A memory's vector lives in the content-addressed `embedding_cache`, keyed by
//! [`embedding_input_hash`] over the memory's embedding text — no per-memory table. An edited
//! memory hashes differently, so it reads as a cache miss until it is re-embedded; it never serves
//! its old vector.
//!
//! Only write paths fill the cache; search is a pure read (the MCP server searches on a read-only
//! connection). `memory_create` / `memory_update` embed the memory they just wrote, so it ranks by
//! meaning on the next search. The reconcile seam backfills everything else — memories that
//! arrived by sync or import, or whose inline embed failed — building the embedder only when
//! something is missing, so an idle watcher pass never loads a model.
//!
//! Both embed through [`active_embedder`], the query-path embedder, so memory and query vectors
//! share one model space; an ephemeral model embeds against its local query endpoint and never
//! provisions a remote box for memories. The text is capped at [`DEFAULT_MAX_EMBEDDING_CHARS`],
//! not the per-run reconcile cap, because every path must hash the same text.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use rag_rat_query::memory;

use super::*;

/// Memories embedded per reconcile pass. The first refresh on a large memory set spreads over a
/// few passes rather than stalling one; steady state embeds only the handful sync brought in.
const MEMORY_VECTORS_PER_PASS: usize = 256;
/// Memories per `embed_batch` call.
const MEMORY_EMBED_BATCH: usize = 32;
/// Consecutive single-memory embed failures that end a refresh. One refused input is skipped and
/// the rest continue; failures in a row mean the backend itself is down, and each further call
/// would wait out another request timeout.
const MAX_CONSECUTIVE_EMBED_FAILURES: usize = 2;
/// A live memory's cached vector gets its `last_used_at_ms` refreshed at most this often.
/// `prune_embedding_cache_unreferenced` protects only vectors a live chunk references, so this bump
/// is what keeps a live memory's vector past the GC grace. A repo nobody reconciles for longer than
/// the grace loses its memory vectors to a gc run from a sibling repo; its search degrades to BM25
/// until its next reconcile re-embeds them.
const LAST_USED_REFRESH_MS: i64 = 24 * 60 * 60 * 1000;

/// One live memory with its embedding input and the cache key that input hashes to.
struct KeyedMemory {
    memory_id: String,
    text: String,
    input_hash: String,
}

struct CachedVector {
    blob: Vec<u8>,
    last_used_at_ms: i64,
}

/// The active repo's live memories under one model, with whatever vectors the cache holds for them.
struct MemoryVectors {
    memories: Vec<KeyedMemory>,
    cached: HashMap<String, CachedVector>,
}

impl MemoryVectors {
    fn load(conn: &Connection, model_id: &str, dim: usize) -> anyhow::Result<Self> {
        let model_version = active_embedding_model_version(conn, model_id)?;
        let memories = memory::memory_embedding_sources(conn)?
            .into_iter()
            .map(|source| {
                let (text, _) = truncate_chars(&source.text, DEFAULT_MAX_EMBEDDING_CHARS);
                let input_hash = embedding_input_hash(model_id, &model_version, &text);
                KeyedMemory { memory_id: source.memory_id, text, input_hash }
            })
            .collect::<Vec<_>>();
        let hashes =
            serde_json::to_string(&memories.iter().map(|m| &m.input_hash).collect::<Vec<_>>())?;
        let cached = conn
            .prepare(
                "SELECT input_hash, vector_blob, last_used_at_ms FROM embedding_cache
                 WHERE input_hash IN (SELECT value FROM json_each(?1))
                   AND model_id = ?2 AND embedding_dim = ?3",
            )?
            .query_map(params![hashes, model_id, i64::try_from(dim)?], |row| {
                Ok((row.get::<_, String>(0)?, CachedVector {
                    blob: row.get(1)?,
                    last_used_at_ms: row.get(2)?,
                }))
            })?
            .collect::<Result<HashMap<_, _>, _>>()?;
        Ok(Self { memories, cached })
    }

    /// Distinct uncached `(input_hash, text)` inputs in memory-id order.
    fn missing(&self) -> Vec<(String, String)> {
        let mut seen = HashSet::new();
        self.memories
            .iter()
            .filter(|m| {
                !self.cached.contains_key(&m.input_hash) && seen.insert(m.input_hash.as_str())
            })
            .map(|m| (m.input_hash.clone(), m.text.clone()))
            .collect()
    }

    /// Bump `last_used_at_ms` on the cached vectors whose last bump is older than
    /// [`LAST_USED_REFRESH_MS`]. Takes the write lock only when there is something to bump.
    fn touch_stale(&self, conn: &Connection, now: i64) -> anyhow::Result<()> {
        let cutoff = now.saturating_sub(LAST_USED_REFRESH_MS);
        let stale = self
            .cached
            .iter()
            .filter(|(_, cached)| cached.last_used_at_ms < cutoff)
            .map(|(hash, _)| hash.as_str())
            .collect::<Vec<_>>();
        if stale.is_empty() {
            return Ok(());
        }
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE embedding_cache SET last_used_at_ms = ?2
             WHERE input_hash IN (SELECT value FROM json_each(?1))",
            params![serde_json::to_string(&stale)?, now],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Embed `inputs` and write their vectors, stopping at `deadline`. A batch the embedder rejects is
/// retried one input at a time, so one input the backend refuses (too long for a remote context
/// window, say) cannot block the rest; [`MAX_CONSECUTIVE_EMBED_FAILURES`] single failures in a row
/// end the run instead, so an unreachable endpoint costs a few request timeouts, not one per input.
/// Returns how many inputs were embedded.
fn embed_and_store(
    conn: &Connection,
    embedder: &dyn Embedder,
    inputs: &[(String, String)],
    deadline: Option<Instant>,
) -> anyhow::Result<usize> {
    let mut embedded = Vec::new();
    let mut consecutive_failures = 0;
    let past_deadline = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
    'batches: for batch in inputs.chunks(MEMORY_EMBED_BATCH) {
        if past_deadline() {
            break;
        }
        let texts = batch.iter().map(|(_, text)| text.clone()).collect::<Vec<_>>();
        match embed_checked(embedder, &texts) {
            Ok(vectors) => {
                consecutive_failures = 0;
                embedded.extend(batch.iter().map(|(hash, _)| hash.clone()).zip(vectors));
            },
            // A one-input batch already was the one-by-one attempt; retrying it would only wait
            // out a second request timeout against a hung backend.
            Err(err) if batch.len() == 1 =>
                if record_failure(&mut consecutive_failures, &err) {
                    break;
                },
            Err(_) =>
                for (hash, text) in batch {
                    if past_deadline() {
                        break 'batches;
                    }
                    match embed_checked(embedder, std::slice::from_ref(text)) {
                        Ok(mut vector) => {
                            consecutive_failures = 0;
                            embedded.push((hash.clone(), vector.remove(0)));
                        },
                        Err(err) =>
                            if record_failure(&mut consecutive_failures, &err) {
                                break 'batches;
                            },
                    }
                },
        }
    }
    if embedded.is_empty() {
        return Ok(0);
    }
    let now = now_ms();
    let dim = i64::try_from(embedder.dim())?;
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    for (hash, vector) in &embedded {
        tx.execute(
            "INSERT INTO embedding_cache(
                 input_hash, model_id, embedding_dim, vector_blob, computed_at_ms, last_used_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(input_hash) DO UPDATE SET last_used_at_ms = excluded.last_used_at_ms",
            params![hash, embedder.model_id(), dim, encode_vector(vector), now],
        )?;
    }
    tx.commit()?;
    Ok(embedded.len())
}

/// Log a failed embed attempt; `true` once the failures in a row say the backend itself is down.
fn record_failure(consecutive_failures: &mut usize, err: &anyhow::Error) -> bool {
    tracing::warn!(
        target: "rag_rat_core::index::ai::reconcile",
        error = %err,
        "memory embed failed; retrying on a later pass"
    );
    *consecutive_failures += 1;
    *consecutive_failures >= MAX_CONSECUTIVE_EMBED_FAILURES
}

fn embed_checked(embedder: &dyn Embedder, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
    let vectors = embedder.embed_batch(texts)?;
    anyhow::ensure!(
        vectors.len() == texts.len() && vectors.iter().all(|v| v.len() == embedder.dim()),
        "embedder {} returned {} vectors of the wrong shape for {} inputs",
        embedder.model_id(),
        vectors.len(),
        texts.len()
    );
    Ok(vectors)
}

/// The active model's id and dimension, or `None` when it is not a registered model.
fn active_model(conn: &Connection) -> anyhow::Result<Option<(String, usize)>> {
    ensure_model_manifest(conn)?;
    let model_id = active_embedding_model_id(conn)?;
    Ok(rag_rat_base::embedding_models::spec(&model_id).map(|spec| (model_id, spec.dim)))
}

/// The reconcile-seam backfill: embed the active repo's live memories that have no cached vector
/// under the active model, up to [`MEMORY_VECTORS_PER_PASS`] and until `deadline`, and keep the
/// cached ones past the cache GC. No embedder (none installed, or not ready) is a no-op. Returns
/// how many memories were embedded.
pub(crate) fn refresh_memory_vectors(
    conn: &Connection,
    deadline: Option<Instant>,
) -> anyhow::Result<usize> {
    let Some((model_id, dim)) = active_model(conn)? else {
        return Ok(0);
    };
    refresh_memory_vectors_with(conn, &model_id, dim, deadline, || active_embedder(conn, None))
}

/// [`refresh_memory_vectors`] with the embedder supplied lazily: `make_embedder` runs only when a
/// memory actually needs embedding, and a construction failure (model not ready) is a no-op.
pub(crate) fn refresh_memory_vectors_with(
    conn: &Connection,
    model_id: &str,
    dim: usize,
    deadline: Option<Instant>,
    make_embedder: impl FnOnce() -> anyhow::Result<Box<dyn Embedder>>,
) -> anyhow::Result<usize> {
    let vectors = MemoryVectors::load(conn, model_id, dim)?;
    let now = now_ms();
    vectors.touch_stale(conn, now)?;
    let mut missing = vectors.missing();
    if missing.is_empty() {
        return Ok(0);
    }
    // Start each pass at a different point, so a memory the backend keeps refusing cannot hold the
    // same place at the head of every pass.
    let start = usize::try_from(now.unsigned_abs()).unwrap_or(0) % missing.len();
    missing.rotate_left(start);
    missing.truncate(MEMORY_VECTORS_PER_PASS);
    // A chunk reconcile that spent the whole budget leaves none for memories: don't load a model
    // only to embed nothing.
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Ok(0);
    }
    let Ok(embedder) = make_embedder() else {
        return Ok(0);
    };
    embed_and_store(conn, &*embedder, &missing, deadline)
}

/// Embed one just-written memory, so it ranks by meaning on the next search instead of waiting for
/// a reconcile. Best effort: `false` when there is no embedder, the memory is not live, or its
/// vector is already cached. An embed failure leaves it to the reconcile backfill.
pub(crate) fn embed_written_memory(conn: &Connection, memory_id: &str) -> anyhow::Result<bool> {
    let Some((model_id, dim)) = active_model(conn)? else {
        return Ok(false);
    };
    let vectors = MemoryVectors::load(conn, &model_id, dim)?;
    let Some(memory) = vectors.memories.iter().find(|m| m.memory_id == memory_id) else {
        return Ok(false);
    };
    if vectors.cached.contains_key(&memory.input_hash) {
        return Ok(false);
    }
    let Ok(embedder) = active_embedder(conn, None) else {
        return Ok(false);
    };
    let input = [(memory.input_hash.clone(), memory.text.clone())];
    Ok(embed_and_store(conn, &*embedder, &input, None)? > 0)
}

/// Every live memory of the active repo with a positive similarity to `query`, best first (ties by
/// memory id). A pure read: a memory without a cached vector under the query's model is absent
/// from this arm and ranks by BM25 alone.
pub(crate) fn memory_vector_similarities(
    conn: &Connection,
    query: &QueryEmbedding,
) -> anyhow::Result<Vec<(String, f32)>> {
    let vectors = MemoryVectors::load(conn, &query.model_id, query.dim)?;
    let mut scored = Vec::new();
    for memory in &vectors.memories {
        let Some(vector) = vectors
            .cached
            .get(&memory.input_hash)
            .and_then(|cached| decode_vector(&cached.blob, query.dim))
        else {
            continue;
        };
        let similarity: f32 = query.vector.iter().zip(&vector).map(|(a, b)| a * b).sum();
        if similarity > 0.0 {
            scored.push((memory.memory_id.clone(), similarity));
        }
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(scored)
}
