//! Repo-memory commands, split out of the `commands` god-module: `memory` (doctor / rebind / list
//! / show) with its `*_bind_target` selector helpers, and `dream` (the deterministic
//! memory-maintenance worklist pass).
use rag_rat_core::{Config, OutputFormat};

use crate::cli::{DreamArgs, MemoryArgs, MemoryCommand};
use crate::commands::output_format;
use crate::open_index;
use crate::render::print_output;

/// Dream-mode worklist (#122): run the deterministic memory-maintenance pass (coverage gaps +
/// stale references), sync it into `dream_findings`, and render the open worklist. Writes ONLY to
/// `dream_findings` / the derived `memory_reality` + `memory_summaries` siblings — never mutates a
/// `repo_memories` row.
///
/// `--verify` turns on the dream v2 verification pass: the deterministic `memory_unverifiable`
/// findings always, and — when `[llm.dream] enabled = true` — the out-of-process model verdict
/// pass (writing `memory_reality` verdicts + `memory_divergence` findings). `--compact`
/// (independent of `--verify`) turns on the compaction pass: when the model is enabled it rewrites
/// un-summarized memories into `memory_summaries`. With neither flag the run is byte-identical to
/// the v1 deterministic worklist.
pub(crate) fn dream(config: &Config, args: &DreamArgs) -> anyhow::Result<()> {
    // `dream` WRITES dream_findings — serialize with the watcher/index like every other write
    // command (index/maintenance/oracle); WriteLock is reentrant so the open-time migrate is safe.
    //
    // ACCEPTED v1 POSTURE: with the model enabled, the verdict/compact passes run their whole
    // budget (minutes of out-of-process network I/O) while HOLDING this lock, so the watcher and
    // indexer block for the run's duration. That is acceptable because `dream --verify/--compact`
    // is an EXPLICIT, human-invoked batch command run at most a few times a day — not a hot path.
    // A finer-grained lock (release during model I/O) is deferred until that cadence proves wrong.
    let lock_repo = rag_rat_core::locks::write_lock_repo_id(config);
    let _lock = rag_rat_core::locks::WriteLock::acquire_blocking(&config.database, &lock_repo)?;
    let db = open_index(config)?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // REVIEW mode: `rag-rat dream <FINDING_ID> --accept|--dismiss|--reset` applies a human verdict
    // to ONE finding and prints it — it does NOT run the worklist/model passes. The verdict is
    // preserved across future runs (the sync refresh keeps accepted/dismissed).
    if let Some(finding) = &args.finding {
        let verdict = match (args.accept, args.dismiss, args.reset) {
            (true, false, false) => rag_rat_core::dream::ReviewVerdict::Accept,
            (false, true, false) => rag_rat_core::dream::ReviewVerdict::Dismiss,
            (false, false, true) => rag_rat_core::dream::ReviewVerdict::Reset,
            _ => anyhow::bail!(
                "reviewing `{finding}` needs exactly one of --accept / --dismiss / --reset"
            ),
        };
        if args.verify || args.compact {
            anyhow::bail!(
                "--verify / --compact run the worklist; they can't combine with reviewing a \
                 finding"
            );
        }
        let reviewed = db.review_dream_finding(finding, verdict, now_ms)?;
        return print_output(&reviewed);
    }
    // A verdict flag with no finding id is a mistake (clap keeps the three verdicts mutually
    // exclusive; this catches "flag but no id").
    if args.accept || args.dismiss || args.reset {
        anyhow::bail!("--accept / --dismiss / --reset need a <FINDING_ID> to review");
    }

    let opts = rag_rat_core::dream::DreamOptions {
        now_ms,
        limit: args.limit.unwrap_or(20) as usize,
        verify: args.verify,
        // `--all` also lists the human-reviewed (accepted/dismissed) findings, so you can see and
        // `--reset` them; the default worklist is the open (needs-attention) set.
        include_reviewed: args.all,
    };
    // The model passes run only when the operator asked for the matching flag AND enabled the model
    // in config — the generative-model dependency stays strictly opt-in (#122). One client is built
    // (out-of-process) and borrowed by whichever passes are active; with the model disabled, both
    // are `None` and the run stays 100% deterministic.
    let model_enabled = config.llm.dream.enabled;
    // A model-pass flag with the model disabled would otherwise run silently as a
    // deterministic-only pass; name the config key so the operator knows why no
    // verdicts/summaries were written.
    if (args.verify || args.compact) && !model_enabled {
        eprintln!(
            "note: --verify/--compact was requested but [llm.dream] enabled = false — running the \
             deterministic passes only; set [llm.dream] enabled = true to run the model \
             verdict/compaction pass."
        );
    }
    let budget = args.max_memories.unwrap_or(20) as usize;
    let remote = &config.llm.dream.remote;
    // Build the model client when a model pass is asked-for AND enabled. For an EPHEMERAL
    // `[llm.dream.remote]` (cookbook set), provision a GPU box first — but a ZERO-WORK GUARD skips
    // that entirely when the churn-skip queues are already drained, so an idle `dream --verify`
    // never cold-starts a paid box. `_provisioned` holds the box for the whole run; its `Drop`
    // tears it down after the passes finish. Connect mode (or the local-Ollama default) builds
    // directly.
    let mut _provisioned = None;
    let model = if model_enabled && (args.verify || args.compact) {
        if remote.is_ephemeral() {
            if db.dream_model_work_pending(
                opts,
                budget,
                args.verify,
                args.compact,
                remote.model.trim(),
            )? {
                let (m, provisioned) = rag_rat_core::dream::provision_verdict_model(remote)?;
                _provisioned = Some(provisioned);
                Some(m)
            } else {
                eprintln!(
                    "note: no memories pending verification/compaction — skipping the ephemeral \
                     `[llm.dream.remote]` GPU box for this run."
                );
                None
            }
        } else {
            Some(rag_rat_core::dream::HttpVerdictModel::from_config(remote)?)
        }
    } else {
        None
    };
    let verdict_pass = (args.verify && model.is_some())
        .then(|| rag_rat_core::dream::VerdictPass { model: model.as_ref().unwrap(), budget });
    let compact_pass = (args.compact && model.is_some())
        .then(|| rag_rat_core::dream::CompactPass { model: model.as_ref().unwrap(), budget });
    let report = db.dream_run_with_passes(opts, verdict_pass, compact_pass)?;
    print_output(&report)
}

// Each `memory rebind` target sets one anchor field and defaults the rest, so the call sites
// below state only what differs.
fn symbol_bind_target(
    hit: &rag_rat_core::query::symbol::SymbolHit,
) -> rag_rat_core::query::memory::RepoMemoryBindTarget {
    rag_rat_core::query::memory::RepoMemoryBindTarget {
        symbol_id: Some(hit.symbol_id),
        logical_symbol_id: hit.logical_symbol_id,
        ..Default::default()
    }
}

fn path_bind_target(path: String) -> rag_rat_core::query::memory::RepoMemoryBindTarget {
    rag_rat_core::query::memory::RepoMemoryBindTarget { path: Some(path), ..Default::default() }
}

fn dir_bind_target(dir: String) -> rag_rat_core::query::memory::RepoMemoryBindTarget {
    rag_rat_core::query::memory::RepoMemoryBindTarget { dir: Some(dir), ..Default::default() }
}

fn chunk_bind_target(chunk_id: i64) -> rag_rat_core::query::memory::RepoMemoryBindTarget {
    rag_rat_core::query::memory::RepoMemoryBindTarget {
        chunk_id: Some(chunk_id),
        ..Default::default()
    }
}

pub(crate) fn memory(config: &Config, args: &MemoryArgs) -> anyhow::Result<()> {
    match &args.command {
        MemoryCommand::Doctor => {
            let db = open_index(config)?;
            let entries = db.memory_doctor()?;
            // Human-readable rebind suggestions by default; the global `--json` emits the
            // structured doctor entries instead.
            if output_format() == OutputFormat::Json {
                print_output(&entries)?;
                let any_gone = entries.iter().any(|e| e.anchor_status == "gone");
                if any_gone {
                    anyhow::bail!("one or more memories have gone anchors");
                }
                return Ok(());
            }
            if entries.is_empty() {
                eprintln!("All active memory anchors are current.");
                return Ok(());
            }
            let mut any_gone = false;
            for entry in &entries {
                eprintln!("[{}] {} ({})", entry.anchor_status, entry.title, entry.memory_id);
                eprintln!("  binding: {} {}", entry.binding_kind, entry.binding_id);
                if entry.candidates.is_empty() {
                    if entry.anchor_status == "gone" {
                        eprintln!(
                            "  -> code appears deleted; rag-rat memory mark-obsolete {}",
                            entry.memory_id
                        );
                    }
                } else {
                    for candidate in &entry.candidates {
                        // Suggest --symbol-path (exact qualified-name match) rather than --symbol
                        // (substring): a fully-qualified candidate fed to --symbol would also hit
                        // longer siblings. Exact match plus cfg-group collapse makes this runnable.
                        eprintln!(
                            "  rag-rat memory rebind {} --symbol-path {}",
                            entry.memory_id, candidate
                        );
                    }
                }
                if entry.anchor_status == "gone" {
                    any_gone = true;
                }
            }
            if any_gone {
                anyhow::bail!("one or more memories have gone anchors");
            }
            Ok(())
        },
        MemoryCommand::Rebind { memory_id, symbol, symbol_path, symbol_id, path, chunk, dir } => {
            let db = open_index(config)?;
            let bind = if symbol.is_some() || symbol_path.is_some() || symbol_id.is_some() {
                let selector = rag_rat_core::query::symbol::SymbolSelector {
                    logical_symbol_id: None,
                    symbol_id: *symbol_id,
                    symbol_path: symbol_path.clone(),
                    symbol: symbol.clone(),
                    language: None,
                    allow_ambiguous: false,
                    limit: 10,
                };
                let label = symbol
                    .as_deref()
                    .or(symbol_path.as_deref())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("#{}", symbol_id.unwrap_or_default()));
                match db.select_symbol_for_bind(&selector)? {
                    Ok(Some(hit)) => symbol_bind_target(&hit),
                    Ok(None) => anyhow::bail!("symbol `{label}` not found"),
                    Err(disambiguation) => anyhow::bail!(
                        "symbol `{label}` is ambiguous — disambiguate with one of:\n{}",
                        disambiguation
                            .candidates
                            .iter()
                            .map(|c| format!(
                                "  --symbol-id {}   ({} in {})",
                                c.symbol_id, c.qualified_name, c.path
                            ))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                }
            } else if let Some(path) = path {
                path_bind_target(path.clone())
            } else if let Some(chunk_id) = chunk {
                chunk_bind_target(*chunk_id)
            } else if let Some(dir) = dir {
                dir_bind_target(dir.clone())
            } else {
                anyhow::bail!(
                    "memory rebind needs one of --symbol <name>, --symbol-path <path::name>, \
                     --symbol-id <id>, --path <path>, --chunk <id>, or --dir <dir>"
                );
            };
            print_output(&db.memory_rebind(memory_id, bind)?)
        },
        MemoryCommand::List { kind } => {
            let db = open_index(config)?;
            let summaries = db.memory_list(kind.as_deref())?;
            // The global `--json` emits the structured list (a caller parsing stdout gets JSON, not
            // the human lines below).
            if output_format() == OutputFormat::Json {
                return print_output(&summaries);
            }
            if summaries.is_empty() {
                eprintln!("No memories found.");
                return Ok(());
            }
            for s in &summaries {
                println!(
                    "{}  [{}/{}]  {}  ({}:{})",
                    s.memory_id, s.kind, s.status, s.title, s.binding_kind, s.binding_id
                );
            }
            Ok(())
        },
        MemoryCommand::Show { memory_id } => {
            let db = open_index(config)?;
            let Some(memory) = db.memory_get(memory_id)? else {
                anyhow::bail!("memory `{memory_id}` not found");
            };
            // The global `--json` emits the structured memory instead of the human view below.
            if output_format() == OutputFormat::Json {
                return print_output(&memory);
            }
            println!("Title:      {}", memory.title);
            println!("Kind:       {} / {} / {}", memory.kind, memory.status, memory.confidence);
            println!();
            println!("{}", memory.body);
            if !memory.bindings.is_empty() {
                println!();
                println!("Bindings:");
                for b in &memory.bindings {
                    println!("  {} {} [{}]", b.binding_kind, b.binding_id, b.anchor_status);
                }
            }
            Ok(())
        },
    }
}
