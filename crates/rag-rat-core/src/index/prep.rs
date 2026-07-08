//! File-preparation pipeline: read/parse/chunk/symbol source files into prepared rows (off the DB
//! connection, in parallel) before insertion.

use super::*;

#[derive(Debug)]
pub(crate) struct PreparedIndexFile {
    pub(crate) file: IndexFile,
    pub(crate) prepared: anyhow::Result<PreparedIndexContent>,
}

/// A chunk with all of its per-chunk CPU work (text hash, relocation anchor, embedding policy)
/// precomputed in the parallel prepare phase. The serial insert stage then only steps the INSERT —
/// no hashing on the single-threaded path.
#[derive(Debug)]
pub(crate) struct PreparedChunk {
    pub(crate) chunk: Chunk,
    pub(crate) text_hash: String,
    pub(crate) anchor: anchors::ChunkAnchor,
    pub(crate) embedding: ai::EmbeddingPolicyDecision,
}

/// Compute the per-chunk hashes / anchor / embedding policy for a file's chunks. Splits the file's
/// lines once (anchors only read a few boundary/context lines). Shared by the parallel prepare
/// phase and the inline incremental path, so both keep this CPU work off the serial insert stage.
///
/// `parsed_root` is the file's shared tree-sitter parse when the caller holds one: the low-signal
/// embedding gate then classifies each chunk's byte span against it instead of re-parsing every
/// chunk's text (#516 — the re-parse cost a worker-thread spawn per chunk). `None` (generated /
/// oversized / markdown / parse-failure files, and the heal path) keeps the text-based fallback.
pub(crate) fn prepare_chunks(
    path: &Path,
    language: &str,
    file_kind: &str,
    chunks: Vec<Chunk>,
    full_text: &str,
    parsed_root: Option<tree_sitter::Node<'_>>,
) -> Vec<PreparedChunk> {
    let full_lines = full_text.lines().collect::<Vec<_>>();
    let language_kind = language.parse::<Language>().ok();
    chunks
        .into_iter()
        .map(|chunk| {
            let text_hash = hex_sha256(chunk.text.as_bytes());
            let anchor = anchors::anchor_for_lines(
                &chunk.text,
                chunk.start_line,
                chunk.end_line,
                &full_lines,
            );
            // Build the check lazily — the policy's cheaper gates (SkipTooSmall etc.) must keep
            // short-circuiting before any tree walk or re-parse happens.
            let low_signal = match parsed_root.zip(language_kind) {
                Some((root, language)) => ai::LowSignalCheck::FromSpan {
                    language,
                    root,
                    start_byte: chunk.start_byte,
                    end_byte: chunk.end_byte,
                },
                None => ai::LowSignalCheck::FromText,
            };
            let embedding = ai::embedding_policy_for_chunk(
                path,
                language,
                file_kind,
                chunk.kind,
                chunk.symbol_path.as_deref(),
                &chunk.text,
                ai::DEFAULT_MAX_EMBEDDING_CHARS,
                low_signal,
            );
            PreparedChunk { chunk, text_hash, anchor, embedding }
        })
        .collect()
}

#[derive(Debug)]
pub(crate) struct PreparedIndexContent {
    pub(crate) modified_at_ms: i64,
    pub(crate) sha256: String,
    pub(crate) chunks: Vec<PreparedChunk>,
    pub(crate) symbols: Vec<Symbol>,
    // Graph edge candidates computed here in the parallel prepare phase (their `from_symbol_id`s
    // are local symbol indices, remapped to DB ids in insert_prepared_file). Empty for
    // generated / oversized / markdown files. Moving this off the serial insert loop kills the
    // duplicate parse.
    pub(crate) edge_candidates: Vec<edges::EdgeCandidate>,
    // Baseline clone fingerprints (#215), computed here from the SAME shared parse instead of the
    // serial insert stage re-reading + re-parsing the file. Keyed by LOCAL index into `symbols`,
    // remapped to the real DB id in insert_prepared_file. Empty for generated / oversized /
    // markdown files (no structural parse).
    pub(crate) symbol_fingerprints: Vec<(usize, clones::SymbolFingerprint)>,
    pub(crate) parser_failure: Option<String>,
}

pub(crate) fn collect_index_files(config: &Config) -> anyhow::Result<Vec<IndexFile>> {
    let mut targets = config.targets.iter().collect::<Vec<_>>();
    targets.sort_by_key(|target| target.index_precedence());
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();

    // Compile the repo's `.gitignore` rules (ancestor chain + nested-along-target-trees) once,
    // shared across targets, so the walker skips the same paths the watcher's `event_is_relevant`
    // will (issue #62). Nested-gitignore discovery is scoped to the configured target directories
    // so a large unindexed sibling is never walked (round-3 finding).
    let ignore = ignore_rules::IgnoreMatcher::compile(&config.root, &config.target_directories());
    for target in targets {
        for file in walker::walk_target(&config.root, target, &ignore)? {
            let relative_path = file.strip_prefix(&config.root)?.to_path_buf();
            if !seen.insert(relative_path.clone()) {
                continue;
            }
            files.push(IndexFile {
                full_path: file,
                relative_path,
                language: target.language,
                kind: target.kind,
                commit_sha: String::new(),
                worktree_id: String::new(),
            });
        }
    }

    Ok(files)
}

pub(crate) fn collect_changed_index_files(
    config: &Config,
    changes: &GitChangedPaths,
) -> anyhow::Result<Vec<IndexFile>> {
    let mut files = Vec::new();
    for relative_path in &changes.changed {
        let full_path = config.root.join(relative_path);
        if !full_path.is_file() {
            continue;
        }
        let Some((language, kind)) = target_for_path(config, relative_path) else {
            continue;
        };
        files.push(IndexFile {
            full_path,
            relative_path: relative_path.clone(),
            language,
            kind,
            commit_sha: String::new(),
            worktree_id: String::new(),
        });
    }
    Ok(files)
}

pub(crate) fn spawn_git_history_prepare(
    root: &Path,
) -> JoinHandle<anyhow::Result<git_history::PreparedGitHistory>> {
    let root = root.to_path_buf();
    thread::spawn(move || git_history::prepare(&root))
}

pub(crate) fn spawn_git_history_prepare_with_plan(
    root: &Path,
    plan: git_history::GitHistoryPreparePlan,
) -> JoinHandle<anyhow::Result<git_history::PreparedGitHistory>> {
    let root = root.to_path_buf();
    thread::spawn(move || git_history::prepare_with_plan(&root, plan))
}

pub(crate) fn join_git_history_prepare(
    handle: JoinHandle<anyhow::Result<git_history::PreparedGitHistory>>,
) -> anyhow::Result<git_history::PreparedGitHistory> {
    handle.join().map_err(|_| anyhow::anyhow!("git history preparation panicked"))?
}

pub(crate) fn prepare_index_file(file: &IndexFile) -> PreparedIndexFile {
    PreparedIndexFile { file: file.clone(), prepared: prepare_index_content(file) }
}

/// Files per full-rebuild wave (prepare → insert → drop). Bounds peak memory; tunable via
/// `RAG_RAT_INDEX_WAVE` for memory/throughput trade-offs on a given machine.
pub(crate) fn index_wave_size() -> usize {
    std::env::var("RAG_RAT_INDEX_WAVE")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&size| size > 0)
        .unwrap_or(2_000)
}

/// Prepare a slice of files in parallel. `base` / `grand_total` let a caller that processes files
/// in waves report progress against the overall total rather than the wave size (a non-waved caller
/// passes `0` and `files.len()`).
pub(crate) fn prepare_files_with_progress<F>(
    files: &[IndexFile],
    progress: &mut F,
    base: usize,
    grand_total: usize,
) -> anyhow::Result<Vec<PreparedIndexFile>>
where
    F: FnMut(IndexProgress),
{
    #[derive(Debug)]
    struct PreparedProgress {
        current: usize,
        total: usize,
        path: PathBuf,
        language: Language,
        kind: TargetKind,
    }

    let prepared = thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let completed = AtomicUsize::new(0);
        let handle = scope.spawn(move || {
            files
                .par_iter()
                .map(|file| {
                    let prepared = prepare_index_file(file);
                    let current = base + completed.fetch_add(1, Ordering::Relaxed) + 1;
                    if should_report_file_progress(current, grand_total) {
                        let _ = tx.send(PreparedProgress {
                            current,
                            total: grand_total,
                            path: file.relative_path.clone(),
                            language: file.language,
                            kind: file.kind,
                        });
                    }
                    prepared
                })
                .collect::<Vec<_>>()
        });

        for event in rx {
            progress(IndexProgress::PreparingFile {
                current: event.current,
                total: event.total,
                path: event.path,
                language: event.language,
                kind: event.kind,
            });
        }

        handle.join().map_err(|_| anyhow::anyhow!("parallel file preparation panicked"))
    })?;
    Ok(prepared)
}

pub(crate) fn should_report_file_progress(current: usize, total: usize) -> bool {
    if total == 0 {
        return false;
    }
    current == 1
        || current == total
        || current.saturating_mul(10) / total
            != current.saturating_sub(1).saturating_mul(10) / total
}

pub(crate) fn prepare_index_content(file: &IndexFile) -> anyhow::Result<PreparedIndexContent> {
    let text = fs::read_to_string(&file.full_path)?;
    let modified_at_ms = file_metadata_ms(&file.full_path)?;
    let sha256 = hex_sha256(text.as_bytes());

    // ONE tree-sitter parse per file, shared by chunks + symbols + edges + the parser-failure flag,
    // instead of re-parsing the same file four times. Only for structural (non-generated,
    // non-markdown) code within the parse-size cap; everything else takes the line-based paths.
    let structural_eligible = file.kind != TargetKind::Generated
        && file.language != Language::Markdown
        && text.len() <= chunker::MAX_STRUCTURAL_PARSE_BYTES;
    let parsed = structural_eligible
        .then(|| parser::parse_file(&file.relative_path, file.language, &text))
        .flatten();

    let symbols = parsed.as_ref().map(|p| symbols::from_parsed(&p.symbols)).unwrap_or_default();

    // Preserve the historical failure signal: a clean parse → None, error nodes → the message, and
    // a hard parse failure on otherwise-eligible code → an error (matches the old parse_error
    // Err arm).
    let parser_failure = if structural_eligible {
        match &parsed {
            Some(p) => p.parser_failure(),
            None => Some("tree-sitter parse failed".to_string()),
        }
    } else {
        None
    };

    let chunks = if file.kind == TargetKind::Generated {
        chunker::generated_chunks_for_file(&file.relative_path, &text)
    } else if let Some(p) = &parsed {
        chunker::code_chunks_for_symbols(&file.relative_path, &text, &p.symbols)
    } else {
        // Markdown, oversized, or a hard parse failure: line-based chunking, no shared tree.
        chunker::chunks_for_file(&file.relative_path, file.language, &text)
    };
    // Precompute per-chunk hashes / anchor / embedding policy here, in parallel. The low-signal
    // gate classifies chunk spans against the shared tree (one parse per file, #516).
    let chunks = prepare_chunks(
        &file.relative_path,
        file.language.as_str(),
        file.kind.as_str(),
        chunks,
        &text,
        parsed.as_ref().map(|p| p.root()),
    );

    // Edge candidates walk the shared tree (no re-parse). from_symbol_id holds a local symbol
    // index, remapped to the real DB id at insert time. Empty when there's no structural parse.
    let edge_candidates = match &parsed {
        Some(p) => {
            let local = edges::IndexedSymbol::local_from_prepared(file.language, &symbols);
            edges::edge_candidates_from_root(
                &file.relative_path,
                file.language,
                &text,
                p.root(),
                &local,
            )
        },
        None => Vec::new(),
    };

    // Baseline clone fingerprints walk the SAME shared tree (no re-parse, no DB). Keyed by local
    // symbol index; insert_prepared_file maps each to its DB id and writes. This is what lets the
    // full-rebuild insert stage skip the second read + second parse the fingerprints used to need.
    //
    // (#232 #6) Skip generated files at index time — write-side storage hygiene only (ZERO recall /
    // precision effect: the candidate read already filters `files.generated = 0`). `kind=Generated`
    // files are already symbol-empty (no parse), so this gate is what catches PATH-heuristic
    // codegen under a SOURCE target (`src/generated/*.rs`, `*.d.ts`) — those DO get symbols and
    // so used to get fingerprints. The arg is byte-identical to the `files.generated` INSERT
    // (file_index.rs), so the index skip and the read filter agree.
    let symbol_fingerprints = if file_is_generated(file.kind, &path_string(&file.relative_path)) {
        Vec::new()
    } else {
        parsed
            .as_ref()
            .map(|p| clones::fingerprint_symbols(p.root(), &text, file.language, &symbols))
            .unwrap_or_default()
    };

    Ok(PreparedIndexContent {
        modified_at_ms,
        sha256,
        chunks,
        symbols,
        edge_candidates,
        symbol_fingerprints,
        parser_failure,
    })
}

#[cfg(test)]
mod low_signal_wiring_tests {
    use super::*;

    /// The span-based low-signal gate (#516) must reach the same per-chunk policy decisions as the
    /// text-based fallback it replaces on the shared-parse path — same chunks, tree present vs
    /// absent. The fixture is sized so both outcomes occur (every block clears the 80-char
    /// `SkipTooSmall` gate): a use/doc-comment block that must be SkipLowSignal and function
    /// bodies that must Embed. If the wiring ever regresses to `None`, this still passes — the
    /// companion assertions that BOTH policies occur keep the parity check non-vacuous.
    #[test]
    fn span_and_text_low_signal_paths_agree_on_prepared_chunk_policies() {
        let src = "//! Module documentation explaining what this file is for.\n\nuse \
                   std::collections::BTreeMap;\nuse std::collections::HashSet;\nuse \
                   std::path::PathBuf;\n\nfn real_work(input: usize) -> usize {\n    let doubled \
                   = input * 2;\n    let shifted = doubled + 7;\n    shifted * shifted\n}\n\nfn \
                   more_work(count: usize) -> usize {\n    let mut total = 0;\n    for step in \
                   0..count {\n        total += step;\n    }\n    total\n}\n";
        let path = Path::new("src/lib.rs");
        let parsed = parser::parse_file(path, Language::Rust, src).expect("fixture parses");
        let chunks = chunker::code_chunks_for_symbols(path, src, &parsed.symbols);

        let decisions = |prepared: &[PreparedChunk]| {
            prepared
                .iter()
                .map(|pc| {
                    (
                        pc.chunk.symbol_path.clone(),
                        pc.embedding.policy.clone(),
                        pc.embedding.eligible,
                    )
                })
                .collect::<Vec<_>>()
        };
        let with_tree =
            prepare_chunks(path, "rust", "source", chunks.clone(), src, Some(parsed.root()));
        let text_only = prepare_chunks(path, "rust", "source", chunks, src, None);

        assert_eq!(decisions(&with_tree), decisions(&text_only), "span vs text policy parity");
        assert!(
            with_tree.iter().any(|pc| pc.embedding.policy == "SkipLowSignal"),
            "fixture must exercise the low-signal outcome: {:?}",
            decisions(&with_tree),
        );
        assert!(
            with_tree.iter().any(|pc| pc.embedding.policy == "Embed"),
            "fixture must exercise the embed outcome: {:?}",
            decisions(&with_tree),
        );
    }
}
