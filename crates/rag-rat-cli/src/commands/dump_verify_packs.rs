//! `rag-rat dump-verify-packs` (#695, `eval` feature) — regenerate the memory-compaction eval's
//! `verify-packs` corpus from committed inputs.
//!
//! Inserts the eval memories described by a spec JSON into the configured index, then dumps each
//! requested memory's `render_pack(evidence_pack(...))` keyed `<eval_id>|<root_label>`. Run against
//! a throwaway index built over a clean checkout (`--root-label /repo`) and over the doctored drift
//! tree (`--root-label /repo-drift`); the two outputs merge into `corpus/verify-packs.json`.
//! Because the packs are re-derived through the SHIPPED [`IndexDatabase::dream_render_pack`], the
//! corpus can never silently drift from the pack format again. See
//! `evals/memory-compaction/README.md`.

use std::collections::BTreeMap;

use rag_rat_base::config::Config;
use rag_rat_core::query::memory::{RepoMemoryBindTarget, RepoMemoryCreate};
use rag_rat_core::query::symbol::SymbolSelector;
use serde::Deserialize;

use crate::cli::DumpVerifyPacksArgs;
use crate::open_index;

/// The regeneration spec: the eval memories to insert, plus which of them to dump packs for.
#[derive(Deserialize)]
struct Spec {
    memories: Vec<SpecMemory>,
    dump: Vec<String>,
}

/// One eval memory. `binding` absent => an unanchored `Concept`/`Task` (the synthetic notes).
#[derive(Deserialize)]
struct SpecMemory {
    eval_id: String,
    kind: String,
    title: String,
    body: String,
    #[serde(default = "default_confidence")]
    confidence: String,
    #[serde(default)]
    binding: Option<Binding>,
}

/// A source anchor mirroring `memories-full.json`'s `binding_kind` — so the regenerated pack
/// windows the SAME bound-file excerpts the original corpus did (a `dir` anchor is what surfaces
/// the doctored file for the `/repo-drift` cases, so honoring the kind is load-bearing there).
#[derive(Deserialize)]
struct Binding {
    /// `path` | `dir` | `logical_symbol`.
    kind: String,
    /// A file path, a directory, or a `path::Name` symbol ref, per `kind`.
    value: String,
}

fn default_confidence() -> String {
    "high".to_string()
}

/// Build the create-time bind target for one eval memory, resolving a `logical_symbol` ref against
/// THIS (throwaway) index — its rowids are index-specific, so the ref must resolve here, not carry
/// a stale id from the corpus.
fn bind_target(
    db: &rag_rat_core::IndexDatabase,
    eval_id: &str,
    binding: &Option<Binding>,
) -> anyhow::Result<RepoMemoryBindTarget> {
    let Some(binding) = binding else {
        return Ok(RepoMemoryBindTarget::default());
    };
    Ok(match binding.kind.as_str() {
        "path" => RepoMemoryBindTarget { path: Some(binding.value.clone()), ..Default::default() },
        "dir" => RepoMemoryBindTarget { dir: Some(binding.value.clone()), ..Default::default() },
        "logical_symbol" => {
            let selector = SymbolSelector {
                logical_symbol_id: None,
                symbol_id: None,
                symbol_path: Some(binding.value.clone()),
                symbol: None,
                language: None,
                allow_ambiguous: false,
                limit: 1,
            };
            let hit = db
                .select_symbol_for_bind(&selector)?
                .map_err(|d| {
                    anyhow::anyhow!("`{eval_id}` symbol `{}` is ambiguous: {d:?}", binding.value)
                })?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "`{eval_id}` symbol `{}` did not resolve in this index",
                        binding.value
                    )
                })?;
            RepoMemoryBindTarget {
                symbol_id: Some(hit.symbol_id),
                logical_symbol_id: hit.logical_symbol_id,
                ..Default::default()
            }
        },
        other => anyhow::bail!("`{eval_id}` has unknown binding kind `{other}`"),
    })
}

pub(crate) fn dump_verify_packs(config: &Config, args: &DumpVerifyPacksArgs) -> anyhow::Result<()> {
    let spec: Spec = serde_json::from_str(&std::fs::read_to_string(&args.spec)?)?;
    // `memory_create` WRITES `repo_memories`, so serialize with the watcher/indexer via the
    // per-repo write flock like every other CLI writer. The index this runs against is a
    // THROWAWAY built over a checkout, so the inserts are discarded with it — no live index is
    // mutated.
    let lock_repo = rag_rat_base::locks::write_lock_repo_id(config);
    let _lock = rag_rat_base::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    let db = open_index(config)?;

    // Insert each eval memory; map its `eval_id` to the minted id (or, if identical content is
    // already present, the idempotent-duplicate id `memory_create` returns).
    let mut minted: BTreeMap<&str, String> = BTreeMap::new();
    for m in &spec.memories {
        let bind = bind_target(&db, &m.eval_id, &m.binding)?;
        let result = db
            .memory_create(RepoMemoryCreate {
                kind: m.kind.clone(),
                title: m.title.clone(),
                body: m.body.clone(),
                confidence: m.confidence.clone(),
                created_by: Some("eval".to_string()),
                source: Some("imported".to_string()),
                payload_json: None,
                tags: Vec::new(),
                bind,
            })
            .map_err(|e| anyhow::anyhow!("inserting eval memory `{}`: {e}", m.eval_id))?;
        minted.insert(&m.eval_id, result.memory.memory_id);
    }

    // Dump each requested memory's evidence pack, keyed `<eval_id>|<root_label>`.
    let mut packs: BTreeMap<String, String> = BTreeMap::new();
    for eval_id in &spec.dump {
        let memory_id = minted
            .get(eval_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("dump id `{eval_id}` is not one of spec.memories"))?;
        let pack = db.dream_render_pack(memory_id)?;
        packs.insert(format!("{eval_id}|{}", args.root_label), pack);
    }

    std::fs::write(&args.out, serde_json::to_string_pretty(&packs)?)?;
    eprintln!("wrote {} packs (root {}) to {}", packs.len(), args.root_label, args.out.display());
    Ok(())
}
