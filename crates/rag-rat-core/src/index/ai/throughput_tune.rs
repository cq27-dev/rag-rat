//! In-Rust embedding throughput tuning.
//!
//! The concurrency sweep runs HERE (against the provisioned box, with the real [`Embedder`]), not
//! in the cookbook recipe — so the probe IS the reconcile request by construction: `batch_size`,
//! `max_batch_chars`, `num_ctx`, `request_timeout_s`, `max_embedding_chars` are all correct because
//! the exact embedder is used. The recipe just provisions + serves.
//!
//! BACKEND-AGNOSTIC. The sweep engine ([`run_sweep`]) knows nothing about ollama — it measures
//! texts/s for each candidate over the `Embedder` trait and picks the knee. A per-backend adapter
//! (today [`tune_ollama_concurrency`]) supplies the candidate list, a `build(candidate) ->
//! Embedder` closure, and a RUNTIME discriminator folded into the cache key so an ollama tune and a
//! future infinity/vLLM tune for the same model never collide.
//!
//! CAP MODEL: `[remote] concurrency` is the user's hard cap. The tuner returns the knee CLAMPED to
//! the cap; it never exceeds or overwrites it. The persisted active-config concurrency stays the
//! cap.

use std::time::Instant;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::RemoteEmbeddingConfig;
use crate::embedding_models::EmbeddingModelSpec;
use crate::index::ai::helpers::{meta, set_meta};
use crate::index::ai::providers::{Embedder, OpenAiEmbedder, ProvisionedEmbedderParams};
use crate::index::util::now_ms;

/// `index_meta` key holding the throughput-tune cache (a JSON map, so no schema migration).
const TUNE_CACHE_META_KEY: &str = "embedding_throughput_tune_v1";
/// Powers of two the concurrency sweep tries (plus the exact cap; filtered to `<= cap`).
const CONCURRENCY_CANDIDATES: [u32; 8] = [1, 2, 4, 8, 16, 32, 64, 128];
/// Byte budget on the synthetic probe texts a single candidate may allocate, so a huge
/// `max_batch_chars` (or per-text `max_embedding_chars`) can't balloon the sweep to GBs. A
/// candidate whose window bytes exceed this is SKIPPED (leaving the sweep incomplete → not cached);
/// since the window bytes are `candidate * min(batch_size * per_text_chars, max_batch_chars)`, 128
/// MB covers every sane config at full fan-out (e.g. the default 128 × 384 KB ≈ 49 MB).
const MAX_PROBE_WINDOW_BYTES: usize = 128 * 1024 * 1024;
/// Fan-out to reconcile with when the sweep RAN but found no stable candidate (every concurrency
/// was lossy/failed): the safest value, NOT the cap — reconcile's scoped-retry handles the rest,
/// and blasting a struggling box at the full cap would just write failed chunk groups.
const SWEEP_FALLBACK_CONCURRENCY: u32 = 1;
const DEFAULT_TUNE_BUDGET_MS: u64 = 60_000;
const MIN_TUNE_BUDGET_MS: u64 = 5_000;
const DEFAULT_TUNE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// One measured candidate.
struct SweepResult {
    candidate: u32,
    texts_per_second: f64,
    requests: u64,
    failures: u64,
    /// The failure breaker tripped — the candidate is unstable, drop it from the knee search.
    aborted: bool,
}

/// The result of a sweep, so the caller can distinguish "measured a knee" from "ran but nothing was
/// stable" from "didn't run" — each wants a DIFFERENT fallback (see [`tune_ollama_concurrency`]).
#[cfg_attr(test, derive(Debug))]
enum SweepOutcome {
    /// A knee (already clamped to the cap) and its measured texts/s. `complete` is false if the
    /// budget truncated the sweep before every candidate ran — the knee is still the best measured
    /// for THIS run, but the caller must NOT cache a partial sweep (it would pin an under-measured
    /// fan-out until the TTL).
    Knee { knee: u32, texts_per_second: f64, complete: bool },
    /// The sweep ran but every candidate was lossy/failed — reconcile conservatively, don't blast
    /// the box at the full cap.
    NoStableCandidate,
    /// The sweep didn't run (tune budget below the minimum) — use the user's cap unchanged.
    NotRun,
}

/// A cache entry (per runtime + request shape).
#[derive(Serialize, Deserialize)]
struct TuneCacheEntry {
    /// The chosen client knee (`<= cap`).
    knee: u32,
    /// The cap this was tuned under (so a RAISED cap forces a re-sweep).
    cap: u32,
    texts_per_second: f64,
    updated_at_ms: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct TuneCacheFile {
    entries: std::collections::HashMap<String, TuneCacheEntry>,
}

/// Tune the CLIENT concurrency (the ollama fan-out) for a provisioned box, returning the knee
/// clamped to the user's cap. `provider`/`gpu` come from the cookbook config;
/// `endpoint`/`auth_token` from the handshake. Never errors — any failure falls back to the cap
/// (tuning is best-effort).
#[allow(clippy::too_many_arguments)]
pub(crate) fn tune_ollama_concurrency(
    conn: &Connection,
    provider: &str,
    endpoint: &str,
    auth_token: Option<&str>,
    remote: &RemoteEmbeddingConfig,
    spec: &EmbeddingModelSpec,
    max_embedding_chars: usize,
    allow_sweep: bool,
) -> u32 {
    let cap = remote.bounded_concurrency();
    if tuning_disabled() {
        return cap;
    }
    // Normalize `batch_size` the SAME way `OpenAiEmbedder` does (clamp to >=1): a configured 0
    // serves one text per request live, so the probe (and cache key) must treat it as 1 —
    // otherwise every candidate window collapses to a single text and the sweep never exercises
    // fan-out.
    let batch_size = remote.batch_size.max(1);
    let key = tune_cache_key(TuneKey {
        runtime: "ollama",
        provider,
        gpu: remote.gpu.as_deref().unwrap_or("cpu"),
        model: remote.model.trim(),
        num_ctx: remote.num_ctx.unwrap_or(0),
        batch_size,
        max_batch_chars: remote.max_batch_chars,
        max_embedding_chars,
        request_timeout_s: remote.request_timeout_s,
    });

    // ALWAYS consult the cache first — even when a new sweep isn't warranted (a bounded
    // `--max-seconds` pass, or a run too small to fan out), a PRIOR tuned knee is still the right
    // fan-out, better than the raw cap.
    if let Some(knee) = fresh_cached_knee(conn, &key, cap) {
        return knee;
    }
    // Cache miss and a new sweep isn't warranted for this run (bounded / no fan-out /
    // single-flight): don't burn paid-box time sweeping a fan-out the loop can't use — fall
    // back to the cap.
    if !allow_sweep {
        return cap;
    }

    // Build an ollama embedder for a probe — identical to the reconcile embedder in every respect
    // (same `batch_size`, so the measured per-request cost matches the live requests and the knee
    // is cacheable) except: `concurrency` is the fan-out being measured, and
    // `request_timeout_s` is bounded by the tune budget so one blocking probe can't hold the
    // box for the full HTTP timeout.
    let build = |concurrency: u32, request_timeout_s: u64| -> OpenAiEmbedder {
        OpenAiEmbedder::from_provisioned(ProvisionedEmbedderParams {
            endpoint,
            auth_token,
            server_model: remote.model.trim(),
            selected_model_id: spec.model_id,
            dim: spec.dim,
            request_timeout_s,
            batch_size,
            concurrency,
            max_batch_chars: remote.max_batch_chars,
        })
    };

    let per_text_chars = probe_text_chars(max_embedding_chars);
    // Texts per probe request = how the live embedder splits (count cap AND char budget), so each
    // probe request matches a real `/api/embed` request's weight.
    let request_texts =
        effective_request_texts(batch_size, remote.max_batch_chars, max_embedding_chars);
    match run_sweep(
        sweep_candidates(cap),
        cap,
        per_text_chars,
        request_texts,
        remote.request_timeout_s,
        tune_budget_ms(),
        build,
    ) {
        SweepOutcome::Knee { knee, texts_per_second, complete } => {
            // Only cache a COMPLETE sweep — one where every candidate was measured. If the budget
            // truncated the loop, or high candidates were skipped for allocation, the knee is still
            // the best measured for THIS run but caching it would pin the repo to an under-measured
            // fan-out until the TTL — let the next provision re-tune.
            if complete {
                write_cached_knee(conn, &key, knee, cap, texts_per_second);
            }
            knee
        },
        // Sweep ran, nothing served stably: reconcile at the safest fan-out (NOT the cap) so we
        // don't just write failed chunk groups; reconcile's scoped-retry copes with the rest.
        SweepOutcome::NoStableCandidate => SWEEP_FALLBACK_CONCURRENCY.min(cap).max(1),
        // Tune budget too small to measure anything: use the user's cap unchanged.
        SweepOutcome::NotRun => cap,
    }
}

/// Whether a client-concurrency sweep is worth running for THIS reconcile — only when the live run
/// can actually fan out. Skip when:
/// - `max_seconds` is set — a bounded `--max-seconds`/maintenance pass is NOT widened
///   (`remote_reconcile_batch_size` returns the plain batch size), so no fan-out.
/// - `cap <= 1` — a single-flight config has no fan-out to optimize (`sweep_candidates(1) == [1]`).
/// - the work fits a single `/api/embed` request — `estimated_jobs <= per_request`, where
///   `per_request` accounts for BOTH the count cap (`batch_size`) AND the char budget
///   (`max_batch_chars`): a small `max_batch_chars` splits even a few chunks into many small
///   requests that DO fan out, so the count alone is the wrong threshold.
///
/// `estimated_jobs = None` (the count query errored) is treated as "maybe" — sweep if unbounded
/// rather than skip real tuning on a transient error.
pub(crate) fn sweep_is_worthwhile(
    max_seconds: Option<u64>,
    estimated_jobs: Option<u64>,
    cap: u32,
    batch_size: u32,
    max_batch_chars: usize,
    max_embedding_chars: usize,
) -> bool {
    if max_seconds.is_some() || cap <= 1 {
        return false;
    }
    let per_request = effective_request_texts(batch_size, max_batch_chars, max_embedding_chars);
    estimated_jobs.is_none_or(|jobs| jobs > u64::from(per_request))
}

/// Texts the live `OpenAiEmbedder` puts in ONE `/api/embed` request: bounded by BOTH the count cap
/// (`batch_size`, clamped to >=1 like the embedder) and the char budget (`max_batch_chars` / a
/// representative per-text size), whichever is smaller. A small `max_batch_chars` splits work into
/// many small requests, so the fan-out threshold must use the smaller of the two.
fn effective_request_texts(
    batch_size: u32,
    max_batch_chars: usize,
    max_embedding_chars: usize,
) -> u32 {
    let by_count = batch_size.max(1);
    let by_chars =
        (max_batch_chars / probe_text_chars(max_embedding_chars)).clamp(1, u32::MAX as usize);
    by_count.min(by_chars as u32)
}

/// GENERIC sweep engine (backend-agnostic): measure texts/s at each `candidate` (built via `build`)
/// against a representative workload, then pick the knee via [`select_knee`]. `build(concurrency,
/// request_timeout_s)` gets a per-request HTTP timeout bounded by the tune budget so no single
/// probe can hold the box past the budget.
fn run_sweep<E: Embedder>(
    candidates: Vec<u32>,
    cap: u32,
    per_text_chars: usize,
    request_texts: u32,
    request_timeout_s: u64,
    budget_ms: u64,
    build: impl Fn(u32, u64) -> E,
) -> SweepOutcome {
    if budget_ms < MIN_TUNE_BUDGET_MS {
        return SweepOutcome::NotRun;
    }
    let per_candidate_ms = (budget_ms / candidates.len().max(1) as u64).max(1_000);
    // Bound each blocking probe by its budget slice: a short RAG_RAT_TUNE_MS with a long configured
    // request_timeout_s must not let one probe hold the paid box for the full HTTP timeout.
    let probe_timeout = probe_timeout_s(request_timeout_s, per_candidate_ms);
    let deadline = Instant::now() + std::time::Duration::from_millis(budget_ms);
    let total_candidates = candidates.len();
    let mut attempted = 0usize;
    let mut results: Vec<SweepResult> = Vec::new();
    for candidate in candidates {
        if deadline.saturating_duration_since(Instant::now()).as_millis() < 1_000 {
            break;
        }
        // A window of `candidate * request_texts` texts (each ~`per_text_chars`) splits — by the
        // embedder's own count + char budget, which `request_texts` mirrors — into `candidate`
        // sub-requests: one parallel wave exercising concurrency `candidate` at the LIVE
        // per-request size. Candidates are ascending, so once the window BYTES exceed the
        // allocation cap every higher one does too: stop (leaving the sweep incomplete →
        // not cached), rather than shrink the request (which would measure a lighter cost
        // than reconcile actually sends).
        let window = (candidate as usize).saturating_mul(request_texts as usize).max(1);
        if window.saturating_mul(per_text_chars) > MAX_PROBE_WINDOW_BYTES {
            break;
        }
        attempted += 1;
        let embedder = build(candidate, probe_timeout);
        let texts = probe_texts(window, per_text_chars);
        let candidate_deadline =
            Instant::now() + std::time::Duration::from_millis(per_candidate_ms);
        let started = Instant::now();
        let mut embedded: u64 = 0;
        let mut requests: u64 = 0;
        let mut failures: u64 = 0;
        while Instant::now() < candidate_deadline {
            match embedder.embed_batch(&texts) {
                Ok(vectors) => {
                    embedded += vectors.len() as u64;
                    requests += 1;
                },
                Err(_) => {
                    failures += 1;
                    if failures > u64::from(candidate) * 2 {
                        break;
                    }
                },
            }
        }
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        results.push(SweepResult {
            candidate,
            texts_per_second: embedded as f64 / elapsed,
            requests,
            failures,
            aborted: failures > u64::from(candidate) * 2,
        });
    }

    match select_knee(&results, cap) {
        // `complete` gates caching: a budget-truncated sweep (didn't attempt every candidate) is
        // usable for this run but must not be persisted as the tuned knee for the full cap.
        Some((knee, texts_per_second)) =>
            SweepOutcome::Knee { knee, texts_per_second, complete: attempted == total_candidates },
        None => SweepOutcome::NoStableCandidate,
    }
}

/// Per-request HTTP timeout for a probe: the configured `request_timeout_s`, capped by this
/// candidate's budget slice so one blocking `embed_batch` can't outlast the tune budget. Always
/// ≥1s.
fn probe_timeout_s(request_timeout_s: u64, per_candidate_ms: u64) -> u64 {
    request_timeout_s.min(per_candidate_ms.div_ceil(1_000)).max(1)
}

/// Throughput scaled by the success ratio: a candidate that intermittently times out or resets is
/// penalized in proportion to its failures, so a lossy fan-out can't win the knee on its successful
/// requests alone (reconcile would treat those failures as failed chunk groups). Zero when it
/// landed no successful request.
fn penalized_tps(r: &SweepResult) -> f64 {
    let total = r.requests + r.failures;
    if r.requests == 0 || total == 0 {
        return 0.0;
    }
    r.texts_per_second * (r.requests as f64 / total as f64)
}

/// Pick the knee from measured candidates: rank by failure-penalized throughput, drop
/// breaker-tripped / no-success candidates, and return `(knee, raw texts/s)` — the
/// LOWEST-concurrency candidate within 8% of the penalized peak, clamped to `cap`. `None` when
/// nothing served stably.
fn select_knee(results: &[SweepResult], cap: u32) -> Option<(u32, f64)> {
    // `results` is concurrency-ascending (candidates are), so preserving order makes the FIRST hit
    // at/above the threshold the lowest-concurrency knee.
    let stable: Vec<(&SweepResult, f64)> = results
        .iter()
        .filter(|r| !r.aborted && r.requests > 0)
        .map(|r| (r, penalized_tps(r)))
        .filter(|(_, tps)| *tps > 0.0)
        .collect();
    let peak = stable.iter().map(|(_, tps)| *tps).fold(0.0_f64, f64::max);
    if peak <= 0.0 {
        return None;
    }
    let threshold = peak * 0.92;
    let knee = stable
        .iter()
        .find(|(_, tps)| *tps >= threshold)
        .or_else(|| stable.iter().max_by(|(_, a), (_, b)| a.total_cmp(b)))?;
    Some((knee.0.candidate.min(cap).max(1), knee.0.texts_per_second))
}

/// Powers of two `<= cap`, plus the EXACT cap (a non-power-of-two cap like 24/127 must be tested so
/// the tuner can use all the capacity the user allowed).
fn sweep_candidates(cap: u32) -> Vec<u32> {
    let cap = cap.clamp(1, 128);
    let mut values: Vec<u32> = CONCURRENCY_CANDIDATES.into_iter().filter(|c| *c <= cap).collect();
    if values.last() != Some(&cap) {
        values.push(cap);
    }
    if values.is_empty() {
        values.push(1);
    }
    values
}

/// Per-text size (chars) for synthetic probes: the configured live chunk cap
/// (`max_embedding_chars`, what `build_embedding_input` truncates each chunk to), NOT a smaller
/// fixed default — so the probe request's per-text compute/peak-memory matches the heaviest request
/// reconcile will actually send, and the cached knee can't be measured on artificially cheap
/// requests.
fn probe_text_chars(max_embedding_chars: usize) -> usize {
    max_embedding_chars.max(1)
}

/// `count` unique synthetic texts of ~`chars` each (unique prefix so a server input cache can't
/// make the probe artificially fast).
fn probe_texts(count: usize, chars: usize) -> Vec<String> {
    (0..count)
        .map(|i| {
            let prefix = format!("rag-rat throughput probe {i}: ");
            let mut text = prefix;
            while text.len() < chars {
                text.push_str("fn example() { let x = 1; return x; } ");
            }
            text.truncate(chars);
            text
        })
        .collect()
}

// ── cache (index_meta JSON map) ──────────────────────────────────────────────

#[derive(Clone, Copy)]
struct TuneKey<'a> {
    runtime: &'a str,
    provider: &'a str,
    gpu: &'a str,
    model: &'a str,
    num_ctx: u32,
    batch_size: u32,
    max_batch_chars: usize,
    max_embedding_chars: usize,
    request_timeout_s: u64,
}

fn tune_cache_key(k: TuneKey<'_>) -> String {
    let raw = format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
        k.runtime,
        k.provider,
        k.gpu,
        k.model,
        k.num_ctx,
        k.batch_size,
        k.max_batch_chars,
        k.max_embedding_chars,
        k.request_timeout_s,
    );
    let digest = Sha256::digest(raw.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn read_cache(conn: &Connection) -> TuneCacheFile {
    meta(conn, TUNE_CACHE_META_KEY)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// The cached knee IF a recent sweep produced it under a cap `>=` the current cap (a RAISED cap
/// re-sweeps to use the new capacity). Clamped down to the current cap so a LOWERED cap takes
/// effect.
fn fresh_cached_knee(conn: &Connection, key: &str, cap: u32) -> Option<u32> {
    let entry = read_cache(conn).entries.remove(key)?;
    fresh_knee_from_entry(&entry, cap, tune_ttl_ms(), now_ms())
}

/// The freshness decision for one cache entry, split out (pure) from the DB read so it's testable
/// without env/clock: fresh (within `ttl_ms`, not future-dated) AND tuned under a cap `>=` the
/// current `cap`; the knee is clamped down to the current cap so a LOWERED cap takes effect.
/// `ttl_ms == 0` (the always-re-sweep env override) never returns a cached knee.
fn fresh_knee_from_entry(
    entry: &TuneCacheEntry,
    cap: u32,
    ttl_ms: u64,
    now_ms: i64,
) -> Option<u32> {
    if ttl_ms == 0 {
        return None;
    }
    let age = now_ms.saturating_sub(entry.updated_at_ms);
    if age < 0 || age as u64 > ttl_ms {
        return None;
    }
    if cap > entry.cap {
        return None; // cap raised → re-sweep to use the new headroom
    }
    if entry.knee > cap {
        // Cap lowered BELOW the measured knee: that exact fan-out (`cap`) was never swept, so don't
        // fabricate it — re-sweep to measure a knee at or under the new cap.
        return None;
    }
    Some(entry.knee.max(1))
}

fn write_cached_knee(conn: &Connection, key: &str, knee: u32, cap: u32, texts_per_second: f64) {
    let mut cache = read_cache(conn);
    cache.entries.insert(key.to_string(), TuneCacheEntry {
        knee,
        cap,
        texts_per_second,
        updated_at_ms: now_ms(),
    });
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = set_meta(conn, TUNE_CACHE_META_KEY, &json);
    }
}

// ── env knobs ────────────────────────────────────────────────────────────────

fn tuning_disabled() -> bool {
    matches!(std::env::var("RAG_RAT_DISABLE_TUNING").ok().as_deref(), Some("1") | Some("true"))
}

fn tune_budget_ms() -> u64 {
    env_ms("RAG_RAT_TUNE_MS", DEFAULT_TUNE_BUDGET_MS)
}

fn tune_ttl_ms() -> u64 {
    // 0 is a valid override (always re-sweep), so parse it explicitly rather than via `env_ms`.
    match std::env::var("RAG_RAT_TUNE_TTL_MS") {
        Ok(raw) => raw.trim().parse::<u64>().unwrap_or(DEFAULT_TUNE_TTL_MS),
        Err(_) => DEFAULT_TUNE_TTL_MS,
    }
}

fn env_ms(var: &str, default: u64) -> u64 {
    match std::env::var(var) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(v) if v > 0 => v,
            _ => default,
        },
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_candidates_include_the_exact_cap() {
        assert_eq!(sweep_candidates(32), vec![1, 2, 4, 8, 16, 32]);
        assert_eq!(sweep_candidates(24), vec![1, 2, 4, 8, 16, 24]);
        assert_eq!(sweep_candidates(127), vec![1, 2, 4, 8, 16, 32, 64, 127]);
        assert_eq!(sweep_candidates(1), vec![1]);
    }

    #[test]
    fn tune_cache_key_folds_the_request_shape_and_runtime() {
        let base = TuneKey {
            runtime: "ollama",
            provider: "modal",
            gpu: "L4",
            model: "all-minilm",
            num_ctx: 0,
            batch_size: 256,
            max_batch_chars: 384_000,
            max_embedding_chars: 4_000,
            request_timeout_s: 60,
        };
        let k = tune_cache_key(base);
        // A different runtime (future infinity/vLLM) must NOT collide.
        let infinity = tune_cache_key(TuneKey { runtime: "infinity", ..base });
        assert_ne!(k, infinity);
        // A different request-shape knob re-tunes.
        let bigger_ctx = tune_cache_key(TuneKey { num_ctx: 8192, ..base });
        assert_ne!(k, bigger_ctx);
        let bigger_chunk = tune_cache_key(TuneKey { max_embedding_chars: 8_000, ..base });
        assert_ne!(k, bigger_chunk);
    }

    #[test]
    fn probe_texts_are_sized_and_unique() {
        let texts = probe_texts(4, 512);
        assert_eq!(texts.len(), 4);
        for t in &texts {
            assert_eq!(t.len(), 512);
        }
        assert_ne!(texts[0], texts[1]);
    }

    #[test]
    fn sweep_is_worthwhile_only_when_the_live_run_fans_out() {
        // cap, batch_size, max_batch_chars, max_embedding_chars — a big char budget so `batch_size`
        // is the binding per-request size (256).
        let big_chars = 10_000_000;
        // Unbounded run, single-flight cap, > one batch of work → sweep.
        assert!(sweep_is_worthwhile(None, Some(10_000), 32, 256, big_chars, 4_000));
        // Bounded (`--max-seconds`) never widens the window → skip regardless of work.
        assert!(!sweep_is_worthwhile(Some(30), Some(10_000), 32, 256, big_chars, 4_000));
        // cap == 1: single-flight, nothing to optimize → skip.
        assert!(!sweep_is_worthwhile(None, Some(10_000), 1, 256, big_chars, 4_000));
        // Work fits one request (<= per_request 256) → no fan-out → skip.
        assert!(!sweep_is_worthwhile(None, Some(256), 32, 256, big_chars, 4_000));
        assert!(!sweep_is_worthwhile(None, Some(10), 32, 256, big_chars, 4_000));
        // Just over one request → fans out → sweep.
        assert!(sweep_is_worthwhile(None, Some(257), 32, 256, big_chars, 4_000));
        // Unknown job count (query errored) but unbounded → sweep (don't skip on a transient
        // error).
        assert!(sweep_is_worthwhile(None, None, 32, 256, big_chars, 4_000));
        // CHAR-SPLIT: a tiny max_batch_chars makes per_request ~1, so even 10 chunks fan out over
        // ~10 one-text requests → sweep (the count-only threshold would have wrongly skipped).
        assert!(sweep_is_worthwhile(None, Some(10), 32, 256, 500, 4_000));
    }

    #[test]
    fn effective_request_texts_is_the_smaller_of_count_and_char_bounds() {
        // Big char budget → the count cap binds (batch_size).
        assert_eq!(effective_request_texts(256, 10_000_000, 4_000), 256);
        // Small char budget → it binds: 500 chars / 4000-char texts → 0 → clamped to 1.
        assert_eq!(effective_request_texts(256, 500, 4_000), 1);
        // 5 texts of 4000 chars fit a 20000-char budget < batch_size → char bound wins.
        assert_eq!(effective_request_texts(256, 20_000, 4_000), 5);
        // batch_size 0 is clamped to 1 like the embedder (never a 0 per-request size).
        assert_eq!(effective_request_texts(0, 10_000_000, 4_000), 1);
    }

    #[test]
    fn probe_timeout_is_capped_by_the_budget_slice() {
        // A short per-candidate slice caps a long configured request timeout.
        assert_eq!(probe_timeout_s(60, 1_000), 1);
        assert_eq!(probe_timeout_s(60, 4_500), 5); // 4.5s slice → ceil to 5s
        // A generous slice leaves the configured timeout untouched.
        assert_eq!(probe_timeout_s(30, 60_000), 30);
        // Never zero.
        assert_eq!(probe_timeout_s(0, 0), 1);
    }

    fn sr(candidate: u32, tps: f64, requests: u64, failures: u64, aborted: bool) -> SweepResult {
        SweepResult { candidate, texts_per_second: tps, requests, failures, aborted }
    }

    #[test]
    fn select_knee_picks_the_lowest_candidate_within_8pct_of_peak() {
        // c=2 (100) and c=4 (104, within 8% of peak 104) are both clean → knee is the LOWER, c=2.
        let results =
            vec![sr(1, 60.0, 10, 0, false), sr(2, 100.0, 10, 0, false), sr(4, 104.0, 10, 0, false)];
        assert_eq!(select_knee(&results, 32), Some((2, 100.0)));
        // The knee is clamped to the cap.
        assert_eq!(select_knee(&[sr(8, 100.0, 10, 0, false)], 4), Some((4, 100.0)));
    }

    #[test]
    fn select_knee_penalizes_lossy_candidates() {
        // c=16 has a higher RAW tps (180) but half its requests failed → penalized 90 < c=2's 100,
        // so the lossy high fan-out must NOT win the knee on successful requests alone.
        let results = vec![sr(2, 100.0, 10, 0, false), sr(16, 180.0, 5, 5, false)];
        assert_eq!(select_knee(&results, 32), Some((2, 100.0)));
    }

    #[test]
    fn select_knee_is_none_when_nothing_served_stably() {
        // Every candidate breaker-tripped / landed no successful request.
        let dead = vec![sr(1, 0.0, 0, 8, true), sr(2, 0.0, 0, 12, true)];
        assert_eq!(select_knee(&dead, 32), None);
    }

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::apply(&conn).unwrap();
        conn
    }

    #[test]
    fn cache_write_then_read_roundtrips_in_index_meta() {
        let conn = mem_conn();
        assert!(read_cache(&conn).entries.is_empty(), "fresh DB has no tune cache");
        write_cached_knee(&conn, "k1", 8, 32, 123.0);
        write_cached_knee(&conn, "k2", 2, 16, 50.0);
        let cache = read_cache(&conn);
        let e1 = cache.entries.get("k1").expect("k1 present");
        assert_eq!((e1.knee, e1.cap), (8, 32));
        let e2 = cache.entries.get("k2").expect("k2 present");
        assert_eq!((e2.knee, e2.cap), (2, 16));
        // A second write to an existing key overwrites in place (same JSON map).
        write_cached_knee(&conn, "k1", 4, 32, 99.0);
        assert_eq!(read_cache(&conn).entries.get("k1").unwrap().knee, 4);
        assert_eq!(read_cache(&conn).entries.len(), 2);
    }

    #[test]
    fn fresh_knee_from_entry_respects_ttl_and_cap() {
        let entry = |knee, cap, updated_at_ms| TuneCacheEntry {
            knee,
            cap,
            texts_per_second: 1.0,
            updated_at_ms,
        };
        let now = 1_000_000i64;
        // Fresh, cap still above the knee → reuse the measured knee.
        assert_eq!(fresh_knee_from_entry(&entry(8, 32, now), 32, 10_000, now), Some(8));
        assert_eq!(fresh_knee_from_entry(&entry(8, 32, now), 8, 10_000, now), Some(8));
        // Cap lowered BELOW the measured knee → re-sweep (don't fabricate an untested fan-out).
        assert_eq!(fresh_knee_from_entry(&entry(8, 32, now), 4, 10_000, now), None);
        // Raised cap re-sweeps (don't reuse a knee measured under a smaller ceiling).
        assert_eq!(fresh_knee_from_entry(&entry(8, 32, now), 64, 10_000, now), None);
        // Expired (age > ttl) → miss.
        assert_eq!(fresh_knee_from_entry(&entry(8, 32, now - 20_000), 32, 10_000, now), None);
        // Future-dated (clock skew) → miss.
        assert_eq!(fresh_knee_from_entry(&entry(8, 32, now + 5_000), 32, 10_000, now), None);
        // ttl == 0 (always-re-sweep override) → miss.
        assert_eq!(fresh_knee_from_entry(&entry(8, 32, now), 32, 0, now), None);
    }

    /// Deterministic fake: succeeds up to `fail_above` concurrency, errors above it (the ollama
    /// high-concurrency cliff). A tiny sleep keeps the timed sweep from pure-spinning a core.
    struct FakeEmbedder {
        concurrency: u32,
        fail_above: u32,
    }
    impl Embedder for FakeEmbedder {
        fn model_id(&self) -> &str {
            "fake"
        }
        fn dim(&self) -> usize {
            3
        }
        fn embed_batch(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            std::thread::sleep(std::time::Duration::from_millis(1));
            if self.concurrency > self.fail_above {
                anyhow::bail!("overloaded at concurrency {}", self.concurrency);
            }
            Ok(texts.iter().map(|_| vec![0.0; 3]).collect())
        }
    }

    #[test]
    fn run_sweep_does_not_run_below_min_budget() {
        // Budget under the minimum → the sweep is skipped entirely (caller uses the cap).
        let outcome =
            run_sweep(sweep_candidates(4), 4, 16, 4, 1, 1_000, |concurrency, _timeout| {
                FakeEmbedder { concurrency, fail_above: 8 }
            });
        assert!(matches!(outcome, SweepOutcome::NotRun));
    }

    #[test]
    fn run_sweep_selects_a_stable_low_knee_and_marks_it_complete() {
        // cap 4 → candidates [1, 2, 4]; the box fails above concurrency 2, so 4 aborts and the knee
        // is a stable low value. All candidates fit the budget → complete → cacheable.
        let outcome = run_sweep(
            sweep_candidates(4),
            4,
            16, // per_text_chars (tiny)
            4,  // batch_size (tiny window)
            1,  // request_timeout_s
            MIN_TUNE_BUDGET_MS,
            |concurrency, _timeout| FakeEmbedder { concurrency, fail_above: 2 },
        );
        match outcome {
            SweepOutcome::Knee { knee, complete, .. } => {
                assert!(knee <= 2, "knee {knee} should be a stable low value (box fails above 2)");
                assert!(complete, "every candidate was attempted within the budget");
            },
            other => panic!("expected a Knee, got {other:?}"),
        }
    }

    fn tune_remote(cap: u32) -> RemoteEmbeddingConfig {
        RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            cookbook: Some("@rag-rat/cookbook modal".to_string()),
            concurrency: cap,
            batch_size: 4,
            max_batch_chars: 384_000,
            request_timeout_s: 1,
            num_ctx: None,
            gpu: None,
            ..RemoteEmbeddingConfig::default()
        }
    }

    #[test]
    fn tune_ollama_concurrency_returns_a_fresh_cached_knee_without_probing() {
        let conn = mem_conn();
        let remote = tune_remote(32);
        let spec =
            crate::embedding_models::spec(crate::embedding_models::FASTEMBED_MODEL_ID).unwrap();
        // Pre-seed the cache with the exact key the tuner will compute (gpu None → "cpu", num_ctx
        // 0).
        let key = tune_cache_key(TuneKey {
            runtime: "ollama",
            provider: "modal",
            gpu: "cpu",
            model: "all-minilm",
            num_ctx: 0,
            batch_size: remote.batch_size,
            max_batch_chars: remote.max_batch_chars,
            max_embedding_chars: 4_000,
            request_timeout_s: remote.request_timeout_s,
        });
        write_cached_knee(&conn, &key, 6, 32, 100.0);
        // The endpoint is never contacted: the cache hits first, even with `allow_sweep = false`.
        let knee = tune_ollama_concurrency(
            &conn,
            "modal",
            "http://127.0.0.1:1",
            None,
            &remote,
            spec,
            4_000,
            false,
        );
        assert_eq!(knee, 6);
    }

    #[test]
    fn tune_ollama_concurrency_uses_the_cap_on_a_miss_when_sweep_disallowed() {
        let conn = mem_conn();
        let remote = tune_remote(8);
        let spec =
            crate::embedding_models::spec(crate::embedding_models::FASTEMBED_MODEL_ID).unwrap();
        // No cache entry + `allow_sweep = false` (a bounded / non-fan-out run) → the raw cap, and
        // the unreachable endpoint is never probed (no sweep runs).
        let knee = tune_ollama_concurrency(
            &conn,
            "modal",
            "http://127.0.0.1:1",
            None,
            &remote,
            spec,
            4_000,
            false,
        );
        assert_eq!(knee, 8);
        assert!(read_cache(&conn).entries.is_empty());
    }

    #[test]
    fn tune_ollama_concurrency_falls_back_conservatively_when_every_probe_fails() {
        let conn = mem_conn();
        // Small cap → few candidates; unreachable endpoint → every probe fails fast (connection
        // refused), so the sweep finds no stable candidate.
        let remote = tune_remote(2);
        let spec =
            crate::embedding_models::spec(crate::embedding_models::FASTEMBED_MODEL_ID).unwrap();
        let knee = tune_ollama_concurrency(
            &conn,
            "modal",
            "http://127.0.0.1:1",
            None,
            &remote,
            spec,
            4_000,
            true,
        );
        assert_eq!(knee, SWEEP_FALLBACK_CONCURRENCY);
        // A failed sweep is never cached (nothing to reuse next time).
        assert!(read_cache(&conn).entries.is_empty());
    }
}
