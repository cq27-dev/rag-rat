//! File-preparation pipeline: read/parse/chunk/symbol source files into prepared rows (off the DB
//! connection, in parallel) before insertion.

use rag_rat_base::hash::hex_sha256;
use rag_rat_base::paths::path_string;

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

/// Build the `(files, changes)` for an EXPLICIT candidate path list — the `index --paths` entry
/// point (#659). Unlike [`collect_changed_index_files`] + a raw [`GitChangedPaths`], the explicit
/// list can name CLEAN / already-committed files (a caller — a git hook, an agent edit hook — knows
/// what it touched, so this "also sees committed changes" the git-status walk misses). So it does
/// NOT put every supplied file in `changed`: it builds an `IndexFile` for every supplied indexable
/// path, but `changed` holds ONLY the DIRTY subset (cross-referenced against the actual git
/// status), because [`IndexDatabase::assign_file_scopes`] reads `changed` as "working-tree dirt" —
/// a clean file left OUT of `changed` is correctly scoped to the COMMITTED scope, not shadowed by
/// an overlay row. Each supplied path (absolute, or already `config.root`-relative) is:
///  1. normalized to a `config.root`-relative path — dropped if NOT under `config.root` (a
///     linked-worktree edit is routed to the overlay path by the caller, not fed here) or if it
///     escapes the tree once symlinks and `..` are resolved (see [`resolves_within_root`] — a
///     lexical check alone accepts both `<root>/src/../../outside.rs` and a path through an in-repo
///     symlink pointing outside);
///  2. dropped if ignored by the repo's `.gitignore` rules + floor, or not a configured target;
///  3. an existing target FILE becomes an `IndexFile` (content hash decides staleness downstream,
///     so an unchanged path is a no-op), added to `changed` ONLY if git status reports it dirty; a
///     path that no longer exists goes to `deleted` (tombstoned, keyed by path+scope — a no-op if
///     it was never indexed).
pub(crate) fn explicit_index_files_and_changes(
    db: &IndexDatabase,
    config: &Config,
    paths: &[PathBuf],
) -> anyhow::Result<(Vec<IndexFile>, GitChangedPaths, bool)> {
    let has_base_commit = !db.active_commit_sha.is_empty();
    let target_dirs = config.target_directories();
    let ignore = crate::index::ignore_rules::IgnoreMatcher::compile(&config.root, &target_dirs);
    // The actual working-tree-dirty set: a supplied path is overlay-scoped ONLY if it is genuinely
    // dirt here; a supplied CLEAN/committed path is left out of `changed` → committed-scoped.
    //
    // Whether a git-status FAILURE is fatal depends on whether a base commit exists. WITH a base
    // commit, `assign_file_scopes` uses this set to split dirty vs committed, so an empty set from
    // a swallowed error would misclassify a DIRTY supplied file as clean and write its
    // uncommitted bytes into the committed scope — corruption; PROPAGATE (`?`). WITHOUT a base
    // commit (a non-git or unborn checkout, where `git_changed_paths` errors on discovery),
    // `assign_file_scopes` scopes EVERY file as working-tree dirt regardless of this set, so
    // the classification is moot — tolerate the error.
    let dirty = if has_base_commit {
        crate::index::git_changed_paths(&config.root)?
    } else {
        crate::index::git_changed_paths(&config.root).unwrap_or_default()
    };
    let canonical_root = config.root.canonicalize().unwrap_or_else(|_| config.root.clone());
    let mut files = Vec::new();
    let mut changes = GitChangedPaths::default();
    // Whether a supplied path is a `Cargo.toml` (present, or a real deletion) — the package-map
    // refresh signal. A manifest is not itself a target file, so it never reaches `files`; the
    // signal must be carried out separately (crate names / path-deps must refresh even for a
    // CLEAN/committed manifest named on the command line — #659 review).
    let mut manifest_in_change_set = false;
    for path in paths {
        let raw_relative = if path.is_absolute() {
            match path.strip_prefix(&config.root) {
                Ok(relative) => relative.to_path_buf(),
                // A LEXICAL strip fails when the caller spells the path (or the checkout root)
                // through a DIFFERENT symlink than `config.root` (macOS `/tmp` vs `/private/tmp`,
                // a symlinked editor `$PWD`), even for a file genuinely inside the checkout. Retry
                // against the CANONICAL root before giving up, so the valid edit is not silently
                // dropped; a strip that still fails is a real outside / linked-worktree path the
                // caller routes elsewhere (#659 review).
                Err(_) => {
                    let Some(relative) =
                        canonicalize_nearest_ancestor(path).and_then(|canonical| {
                            canonical.strip_prefix(&canonical_root).ok().map(Path::to_path_buf)
                        })
                    else {
                        continue;
                    };
                    relative
                },
            }
        } else {
            path.clone()
        };
        // NORMALIZE the relative path (resolve `.` / `..` lexically) BEFORE it is used for target
        // matching, dirty-status lookup, and the persisted `files.path` key — the raw spelling
        // would otherwise let `src/../outside.rs` match the `src` target while resolving
        // elsewhere, or `src/../src/a.rs` miss its `dirty.changed` entry and write under a
        // duplicate key. `None` means the path escaped the root via `..`; skip it.
        let Some(relative) = lexically_normalized_within_root(&raw_relative) else {
            continue;
        };
        let full_path = config.root.join(&relative);
        // Containment: the path must stay under the root after resolving SYMLINKS (lexical
        // normalization can't see them) — a path through an in-repo symlink pointing outside
        // (`src/link/foo.rs`, `link` → /outside) resolves outside and would make `prepare_files`
        // read an external file. Like the symlink/ignore skips below, an out-of-root path must NOT
        // be INDEXED, but must NOT short-circuit the deletion branch either: a regular file
        // REPLACED by an outside-pointing symlink must still TOMBSTONE its stale row (a
        // discover pass would drop it). So fold containment into `is_present` rather than
        // `continue`ing; a never-indexed outside path stays a no-op (#659 review).
        let within_root = resolves_within_root(&full_path, &canonical_root);
        // Match the walker (`walker::walk_dir`), which SKIPS a symlink ENTRY and never descends
        // into it: a supplied path that crosses a symlink at ANY component — the leaf
        // (`src/link.rs`) OR an ancestor directory (`src/link/a.rs`, `src/link` → another
        // in-repo dir) — is likewise not indexable, or `index --paths` writes an index row
        // under a symlink spelling a full/discover pass never produces (a duplicate of the
        // real file's row). `symlink_metadata` per ancestor does NOT follow, so it catches
        // a symlinked component; a missing leaf simply isn't a symlink. `relative` is
        // lexically normalized (Normal components only), so walking it from the root
        // reconstructs the real ancestor chain (#659 review).
        let mut probe = config.root.clone();
        let crosses_symlink = relative.components().any(|component| {
            probe.push(component);
            probe.symlink_metadata().is_ok_and(|meta| meta.file_type().is_symlink())
        });
        // A path that ESCAPES the root, CROSSES a symlink, or is not a regular file is NOT
        // indexable; an existing indexed row for one (a regular file since replaced by a
        // symlink, in- or out-of-repo) still falls through to the deletion branch below and
        // is TOMBSTONED, while an unindexed such path stays a no-op. `within_root`
        // short-circuits before `is_file` (which follows symlinks), so an escaping
        // symlink's external target is never even stat'd.
        let is_present = within_root && !crosses_symlink && full_path.is_file();
        // Gate ignore on PRESENCE (dir-ness `false` is sound for the floor's component-name check
        // and for file globs — matches the watcher's classifier). An IGNORED path is never INDEXED
        // (the walker skips it), but a MISSING, previously-indexed path must still fall through to
        // the deletion branch below and be TOMBSTONED even if its path now matches an ignore rule:
        // the file is gone, so a discover pass would remove the stale row, and the ignore rule does
        // not resurrect it (`index --paths src/gen/a.rs` after deleting an indexed file under a
        // now-ignored dir). A PRESENT file that just became ignored is a SEMANTIC deletion Paths
        // defers to discovery — a `.gitignore` edit fires a discover pass — matching Changed's
        // fs-deletion-only posture (#659 review).
        if is_present && ignore.is_ignored(&full_path, false) {
            continue;
        }
        // A supplied `Cargo.toml` signals a package-map refresh whether it is present
        // (added/edited) or MISSING (deleted) — it is not a source target, so it flows no
        // further than this signal. NOT gated on a git-confirmed deletion: an UNTRACKED or
        // NON-GIT manifest that was scanned into `packages` and then deleted is reported by
        // neither git status nor a `files` row, yet the caller explicitly NAMED it, so its
        // stale package/import rows must still be re-scanned (#659 review). Harmless for a
        // typo'd nonexistent path — `refresh_packages` just re-scans and writes back the
        // same rows; this is the one-shot `index --paths` CLI path (never the
        // idle watcher), so an occasional redundant refresh is fine.
        if relative.file_name() == Some(std::ffi::OsStr::new("Cargo.toml")) {
            manifest_in_change_set = true;
        }
        if is_present {
            let Some((language, kind)) = target_for_path(config, &relative) else {
                continue; // wrong extension / outside a configured target directory
            };
            if dirty.changed.contains(&relative) {
                changes.changed.insert(relative.clone());
            }
            files.push(IndexFile {
                full_path,
                relative_path: relative,
                language,
                kind,
                commit_sha: String::new(),
                worktree_id: String::new(),
            });
        } else if dirty.deleted.contains(&relative) || db.path_has_indexed_row(&relative)? {
            // Tombstone a vanished path ONLY when it was really indexed (git-confirmed deletion, or
            // an existing indexed row). A never-indexed typo / out-of-target temp file must not get
            // a spurious `kind='deleted'` overlay row, which would shadow a real
            // committed file that later appears at that path (#659 review).
            changes.deleted.insert(relative);
        }
    }
    Ok((files, changes, manifest_in_change_set))
}

/// Lexically resolve `.` / `..` in a `config.root`-relative path, returning `None` if it escapes
/// the root (a leading/interior `..` that pops above the root) or is not relative. This is
/// symlink-blind (a symlink escape is caught separately by [`resolves_within_root`]); its job is to
/// produce the CANONICAL RELATIVE SPELLING used for target/dirty/persistence so `src/../src/a.rs`
/// collapses to `src/a.rs` and `src/../outside.rs` to `outside.rs` (then dropped by the target
/// filter).
fn lexically_normalized_within_root(relative: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => out.push(name),
            std::path::Component::CurDir => {},
            std::path::Component::ParentDir =>
                if !out.pop() {
                    return None; // escaped above the root
                },
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Whether `full_path` stays under `canonical_root` after resolving `..` and symlinks. Rejects both
/// `..`-escapes and in-repo-symlink escapes; the lexical prefix check alone would accept both.
/// Rejects a path where nothing on the chain resolves (containment unverifiable).
fn resolves_within_root(full_path: &Path, canonical_root: &Path) -> bool {
    canonicalize_nearest_ancestor(full_path)
        .is_some_and(|canonical| canonical.starts_with(canonical_root))
}

/// Canonicalize `path` by canonicalizing its nearest EXISTING ancestor (the file — or even its
/// parent dir — may be gone in a deletion, or not yet created) and re-appending the missing suffix,
/// so the result is SYMLINK-RESOLVED even for a path that doesn't exist. `None` when nothing on the
/// ancestor chain canonicalizes (effectively unreachable for a real absolute path — the filesystem
/// root always resolves). Shared by containment ([`resolves_within_root`]), the absolute-path
/// rebase in [`explicit_index_files_and_changes`] (a symlinked checkout-root spelling must compare
/// canonically or a valid in-repo edit is dropped), and the worktree routing in `watch::overlay`.
pub(crate) fn canonicalize_nearest_ancestor(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    loop {
        if let Ok(canonical) = ancestor.canonicalize() {
            let suffix = path.strip_prefix(ancestor).unwrap_or_else(|_| Path::new(""));
            return Some(canonical.join(suffix));
        }
        match ancestor.parent() {
            Some(parent) if parent != ancestor => ancestor = parent,
            _ => return None,
        }
    }
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
    Ok(prepare_index_content_from_text(
        &file.relative_path,
        file.language,
        file.kind,
        &text,
        modified_at_ms,
    ))
}

/// The parse-once core of [`prepare_index_content`], operating on text already in hand (no file
/// I/O). ONE tree-sitter parse feeds chunks + symbols + edges + fingerprints + the parser-failure
/// flag, instead of re-parsing the same file. Shared by the full-rebuild / changed-file prepare
/// phase (which reads the file first) AND the heal path (`file_index::index_file`, which already
/// holds the bytes) — so a lazily-healed file parses ONCE like a fully-indexed one instead of 5×
/// (#518), and the two entry points can't drift on the generated-file gate, the chunk-source
/// selection, or the parser-failure message.
pub(crate) fn prepare_index_content_from_text(
    relative_path: &Path,
    language: Language,
    kind: TargetKind,
    text: &str,
    modified_at_ms: i64,
) -> PreparedIndexContent {
    let sha256 = hex_sha256(text.as_bytes());

    // Only structural (non-generated, non-markdown) code within the parse-size cap gets a shared
    // tree; everything else takes the line-based paths.
    let structural_eligible = kind != TargetKind::Generated
        && language != Language::Markdown
        && text.len() <= chunker::MAX_STRUCTURAL_PARSE_BYTES;
    let parsed =
        structural_eligible.then(|| parser::parse_file(relative_path, language, text)).flatten();

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

    let chunks = if kind == TargetKind::Generated {
        chunker::generated_chunks_for_file(relative_path, text)
    } else if let Some(p) = &parsed {
        chunker::code_chunks_for_symbols(relative_path, text, &p.symbols)
    } else {
        // Markdown, oversized, or a hard parse failure: line-based chunking, no shared tree.
        chunker::chunks_for_file(relative_path, language, text)
    };
    // Precompute per-chunk hashes / anchor / embedding policy here, in parallel. The low-signal
    // gate classifies chunk spans against the shared tree (one parse per file, #516).
    let chunks = prepare_chunks(
        relative_path,
        language.as_str(),
        kind.as_str(),
        chunks,
        text,
        parsed.as_ref().map(|p| p.root()),
    );

    // Edge candidates walk the shared tree (no re-parse). from_symbol_id holds a local symbol
    // index, remapped to the real DB id at insert time. Empty when there's no structural parse.
    let edge_candidates = match &parsed {
        Some(p) => {
            let local = edges::IndexedSymbol::local_from_prepared(language, &symbols);
            edges::edge_candidates_from_root(relative_path, language, text, p.root(), &local)
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
    let symbol_fingerprints = if file_is_generated(kind, &path_string(relative_path)) {
        Vec::new()
    } else {
        parsed
            .as_ref()
            .map(|p| clones::fingerprint_symbols(p.root(), text, language, &symbols))
            .unwrap_or_default()
    };

    PreparedIndexContent {
        modified_at_ms,
        sha256,
        chunks,
        symbols,
        edge_candidates,
        symbol_fingerprints,
        parser_failure,
    }
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
