use super::super::*;

/// One chunk's fields needed to re-derive its embedding policy in
/// [`embedding_policy_skip_summary`]. `id` + `current_policy` let the self-heal write back ONLY the
/// chunks whose recomputed policy actually differs from what is persisted.
pub(crate) struct ChunkForPolicy {
    id: i64,
    current_policy: String,
    chunk_kind: String,
    symbol_path: Option<String>,
    start_byte: usize,
    end_byte: usize,
    text: String,
}

/// Reconstruct a file's text from its `start_byte`-ordered chunks. A chunk's stored text is
/// `file[start_byte..end_byte]` for an LF file, so appending each chunk's tail past the running
/// length rebuilds the file (overlaps are consistent; `.get()` keeps char boundaries). The chunker
/// omits WHITESPACE-ONLY gaps (blank lines between symbols) and uses `\n` line endings, so a gap is
/// padded with newlines — a common case that would otherwise defeat the shared parse. `None` only
/// on a non-char-boundary slice. The caller trusts the result ONLY when it hashes to
/// `files.sha256`, so any wrong guess (CRLF / spaces-in-a-gap / older-chunker rows) fails the hash
/// and the caller falls back to per-chunk text.
pub(crate) fn reconstruct_file_text(chunks: &[ChunkForPolicy]) -> Option<String> {
    let mut buf = String::new();
    for chunk in chunks {
        for _ in buf.len()..chunk.start_byte {
            buf.push('\n'); // whitespace-only gap the chunker didn't emit — guess `\n`, sha validates
        }
        if chunk.end_byte > buf.len() {
            buf.push_str(chunk.text.get(buf.len() - chunk.start_byte..)?);
        }
    }
    Some(buf)
}

/// Re-derive one chunk's embedding policy, using `low_signal` for the low-signal gate (span-based
/// off a shared tree, or the chunk's own text). Callers decide what to do with the decision (tally
/// it, or write it back).
fn classify_chunk(
    path: &str,
    language: &str,
    file_kind: &str,
    chunk: &ChunkForPolicy,
    low_signal: LowSignalCheck<'_>,
    max_embedding_chars: usize,
) -> EmbeddingPolicyDecision {
    embedding_policy_for_chunk(
        std::path::Path::new(path),
        language,
        file_kind,
        &chunk.chunk_kind,
        chunk.symbol_path.as_deref(),
        &chunk.text,
        max_embedding_chars,
        low_signal,
    )
}

/// Classify one structural file's collected chunks. Reconstructs the file text and parses it ONCE
/// for span-based low-signal (#516 index-time semantics) — but only when the reconstruction hashes
/// to `files.sha256`; otherwise each chunk falls back to classification from its own text.
fn classify_collected_file(
    path: &str,
    language: &str,
    file_kind: &str,
    sha256: &str,
    chunks: &[ChunkForPolicy],
    max_embedding_chars: usize,
    emit: &mut impl FnMut(&ChunkForPolicy, EmbeddingPolicyDecision) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // Only reconstruct + parse if at least one chunk actually REACHES the low-signal gate — a file
    // whose every chunk is eliminated by a cheaper, parse-free gate (path-generated, test-fixture,
    // too-small, unsupported language) needs no tree-sitter work at all, exactly like the old
    // per-chunk path which parsed lazily at the low-signal gate.
    let needs_low_signal = chunks.iter().any(|chunk| {
        cheap_skip_policy(
            std::path::Path::new(path),
            language,
            file_kind,
            &chunk.chunk_kind,
            chunk.symbol_path.as_deref(),
            chunk.text.trim(),
            max_embedding_chars,
        )
        .is_none()
    });
    let parsed = needs_low_signal
        .then(|| {
            reconstruct_file_text(chunks)
                .filter(|buf| rag_rat_base::hash::hex_sha256(buf.as_bytes()) == sha256)
                .and_then(|buf| {
                    let lang = language.parse::<rag_rat_base::language::Language>().ok()?;
                    crate::index::parser::parse_file(std::path::Path::new(path), lang, &buf)
                        .map(|pf| (lang, pf))
                })
        })
        .flatten();
    for chunk in chunks {
        let low_signal = match &parsed {
            Some((lang, pf)) => LowSignalCheck::FromSpan {
                language: *lang,
                root: pf.root(),
                start_byte: chunk.start_byte,
                end_byte: chunk.end_byte,
            },
            None => LowSignalCheck::FromText,
        };
        emit(
            chunk,
            classify_chunk(path, language, file_kind, chunk, low_signal, max_embedding_chars),
        )?;
    }
    Ok(())
}

/// Per-skip-reason counts of the chunks the embedding policy would skip. DIAGNOSTIC-ONLY (reported
/// in the reconcile report and `reconcile --plan`; nothing gates real work on it).
///
/// FAST PATH (#530): when a full rebuild has certified `chunks.embedding_policy` current for this
/// repo (the `repo_meta` version stamp matches [`EMBEDDING_POLICY_VERSION`]) AND the caller wants
/// the cap the column was stamped at, the counts come straight from the column via `GROUP BY` — no
/// tree-sitter parse, no chunk-text decompress. A stale/absent stamp, or a different cap, falls
/// through to the slow recompute (correct, but O(files) parses). The column is the index-time
/// truth; the recompute approximates it, so they can differ by a few chunks that slice a long
/// comment/string (FromSpan vs FromText) — acceptable for a diagnostic, and precisely why the
/// version stamp gates the read.
pub(crate) fn embedding_policy_skip_summary(
    conn: &Connection,
    max_embedding_chars: usize,
) -> anyhow::Result<BTreeMap<String, u64>> {
    if let Some(fast) = policy_skip_summary_from_column(conn, max_embedding_chars)? {
        return Ok(fast);
    }
    recompute_policy_skip_summary(conn, max_embedding_chars)
}

/// Read the per-policy counts straight from the persisted `chunks.embedding_policy` column, but
/// ONLY when a full rebuild has stamped it current for this repo (`EMBEDDING_POLICY_VERSION`) at
/// the requested cap. `None` — stamp absent/stale, or a different cap — tells the caller to
/// recompute.
fn policy_skip_summary_from_column(
    conn: &Connection,
    max_embedding_chars: usize,
) -> anyhow::Result<Option<BTreeMap<String, u64>>> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let version = rag_rat_db::meta::repo_meta(conn, &repo_id, EMBEDDING_POLICY_VERSION_KEY)?;
    let cap = rag_rat_db::meta::repo_meta(conn, &repo_id, EMBEDDING_POLICY_CAP_KEY)?;
    // Trust the column ONLY when the CURRENT classifier stamped it (version) AND at the cap the
    // caller wants: a different cap re-buckets SkipTooLarge/truncation, which the
    // default-stamped column can't reflect. Both gates fail SAFE — a miss just recomputes.
    if version.as_deref() != Some(EMBEDDING_POLICY_VERSION)
        || cap.as_deref() != Some(max_embedding_chars.to_string().as_str())
    {
        return Ok(None);
    }
    // BYTE-IDENTICAL FROM/JOIN to the recompute (scope view + chunk_text presence) so the counted
    // row SET is the same; only the classification source differs. `Embed` is the sole eligible
    // policy, so excluding it yields exactly the ineligible-skip counts the recompute tallies.
    let mut stmt = conn.prepare(
        "
        SELECT chunks.embedding_policy, COUNT(*)
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        WHERE chunks.embedding_policy != 'Embed'
        GROUP BY chunks.embedding_policy
        ",
    )?;
    let mut out = BTreeMap::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let policy: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        out.insert(policy, u64::try_from(count).unwrap_or(0));
    }
    Ok(Some(out))
}

/// The slow path: re-derive each chunk's policy from source and count the ineligible ones by
/// category. Used when the fast path can't certify the column (stale/absent stamp, or a non-default
/// cap). It deliberately does NOT read `chunks.embedding_policy` (stamped at
/// `DEFAULT_MAX_EMBEDDING_CHARS`, and for pre-migration chunks defaulting to 'Embed' with no
/// backfill) — that uncertified column is what the fast path gates on; here we recompute the ground
/// truth.
fn recompute_policy_skip_summary(
    conn: &Connection,
    max_embedding_chars: usize,
) -> anyhow::Result<BTreeMap<String, u64>> {
    let mut skipped_by_policy = BTreeMap::new();
    for_each_recomputed_chunk_policy(conn, max_embedding_chars, |_chunk, decision| {
        if !decision.eligible {
            *skipped_by_policy.entry(decision.policy).or_default() += 1;
        }
        Ok(())
    })?;
    Ok(skipped_by_policy)
}

/// Re-derive every chunk's embedding policy from source and hand each `(chunk_id, decision)` to
/// `emit`. This is the shared engine behind the skip-summary recompute (which tallies) and the
/// reconcile self-heal (which writes the decision back to `chunks.embedding_policy`).
///
/// Low-signal is classified from the file's SHARED parse (`FromSpan`, #516), not by re-parsing each
/// chunk's text (`FromText`): it groups chunks by file, reconstructs the file text from the chunks'
/// verbatim substrings, and — only when that reconstruction hashes to the stored `files.sha256` —
/// parses it ONCE and classifies each chunk's span. That is O(files) parses instead of O(chunks)
/// (chunks overlap, so per-chunk text re-parses overlapped regions). A file that is generated/
/// markdown, oversized (any chunk past the parse cap), or whose text does not hash-match
/// (CRLF/normalized/older-chunker rows) falls back to per-chunk `FromText`.
///
/// NOTE: this mirrors prep's #516 low-signal classification, so the self-heal writeback re-derives
/// exactly what a reindex would stamp; it differs from the embed path's `FromText` only for chunks
/// that slice into a long comment/string, where `FromSpan` treats the sliced leaf as plumbing.
///
/// Memory is bounded to one structural file's chunks; a file with any oversized chunk flips to
/// streaming per-chunk classification immediately (#379).
fn for_each_recomputed_chunk_policy(
    conn: &Connection,
    max_embedding_chars: usize,
    mut emit: impl FnMut(&ChunkForPolicy, EmbeddingPolicyDecision) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let dicts = rag_rat_query::chunk_text_dicts(conn)?;
    let mut decoder = rag_rat_db::text_compression::ChunkTextDecoder::new(&dicts);
    let mut stmt = conn.prepare(
        "
        SELECT files.id, files.path, files.language, files.kind, files.sha256, chunks.id,
               chunks.chunk_kind, chunks.symbol_path, chunks.start_byte, chunks.end_byte,
               chunk_text.blob, chunk_text.raw_len, chunk_text.dict_version, \
         chunks.embedding_policy
        FROM chunks
        JOIN files ON files.id = chunks.file_id
        JOIN chunk_text ON chunk_text.chunk_id = chunks.id
        ORDER BY files.id, chunks.start_byte
        ",
    )?;
    let mut rows = stmt.query([])?;

    let mut file_id: i64 = -1;
    let mut path = String::new();
    let mut language = String::new();
    let mut file_kind = String::new();
    let mut sha256 = String::new();
    let mut collected: Vec<ChunkForPolicy> = Vec::new();
    let mut streaming = false; // this file already flipped to per-chunk FromText

    while let Some(row) = rows.next()? {
        let row_file_id: i64 = row.get(0)?;
        if row_file_id != file_id {
            if file_id != -1 && !streaming {
                classify_collected_file(
                    &path,
                    &language,
                    &file_kind,
                    &sha256,
                    &collected,
                    max_embedding_chars,
                    &mut emit,
                )?;
            }
            file_id = row_file_id;
            path = row.get(1)?;
            language = row.get(2)?;
            file_kind = row.get(3)?;
            sha256 = row.get(4)?;
            collected.clear();
            streaming = false;
        }
        let chunk = ChunkForPolicy {
            id: row.get(5)?,
            chunk_kind: row.get(6)?,
            symbol_path: row.get(7)?,
            start_byte: row.get::<_, i64>(8)? as usize,
            end_byte: row.get::<_, i64>(9)? as usize,
            text: rag_rat_db::text_compression::ChunkTextRow {
                blob: row.get(10)?,
                raw_len: row.get(11)?,
                dict_version: row.get(12)?,
            }
            .resolve(&mut decoder)?,
            current_policy: row.get(13)?,
        };
        // A structural file (has a grammar, not generated) within the parse cap classifies from its
        // shared tree; anything else streams per-chunk text, bounding memory for huge files.
        // Markdown is the only indexed language without a tree-sitter grammar (mirrors prep's
        // gate).
        let structural = file_kind != "generated"
            && language
                .parse::<rag_rat_base::language::Language>()
                .is_ok_and(|l| l != rag_rat_base::language::Language::Markdown);
        if streaming {
            emit(
                &chunk,
                classify_chunk(
                    &path,
                    &language,
                    &file_kind,
                    &chunk,
                    LowSignalCheck::FromText,
                    max_embedding_chars,
                ),
            )?;
        } else if !structural || chunk.end_byte > crate::index::chunker::MAX_STRUCTURAL_PARSE_BYTES
        {
            for collected_chunk in collected.drain(..) {
                emit(
                    &collected_chunk,
                    classify_chunk(
                        &path,
                        &language,
                        &file_kind,
                        &collected_chunk,
                        LowSignalCheck::FromText,
                        max_embedding_chars,
                    ),
                )?;
            }
            emit(
                &chunk,
                classify_chunk(
                    &path,
                    &language,
                    &file_kind,
                    &chunk,
                    LowSignalCheck::FromText,
                    max_embedding_chars,
                ),
            )?;
            streaming = true;
        } else {
            collected.push(chunk);
        }
    }
    if file_id != -1 && !streaming {
        classify_collected_file(
            &path,
            &language,
            &file_kind,
            &sha256,
            &collected,
            max_embedding_chars,
            &mut emit,
        )?;
    }
    Ok(())
}

/// Reconcile-only self-heal (#530). When the persisted `chunks.embedding_policy` column is NOT
/// certified current for this repo (a stale/absent version stamp — e.g. after a rag-rat upgrade
/// that changed the classifier or bumped a tree-sitter grammar), re-derive every chunk's policy at
/// the DEFAULT cap (what prep stamps) and write it back, then stamp the version current. One slow
/// reconcile pays for it; every later reconcile/plan then takes the fast GROUP BY path — so a
/// version bump does not leave a long-lived, never-fully-rebuilt index paying the O(files) parse
/// forever.
///
/// Runs ONLY on the reconcile write path (holds the write flock); `status`/plan stays read-only and
/// simply recomputes in the rare stale window. For the small fraction of files whose reconstruction
/// does not hash-match (CRLF/normalized), the writeback persists the FromText decision; the next
/// incremental reindex of those files overwrites it with prep's FromSpan value. Diagnostic-only, so
/// that transient difference is acceptable.
/// Whether the connection's active scope covers the repo's ENTIRE live file set — i.e. there is no
/// other live scope this connection can't see. "Outside the active scope" is `commit_sha != active
/// AND worktree_id != active` at the live generation, the exact predicate
/// `carry_forward_live_overlays` uses (a row is in scope when its commit matches the active HEAD OR
/// its worktree matches the active overlay). Correctly ignores the base checkout's own committed +
/// dirty split (both share one of the active keys) and only trips on a second linked-worktree
/// overlay / other-commit leftover.
fn active_scope_covers_all_live_rows(conn: &Connection, repo_id: &str) -> anyhow::Result<bool> {
    use rag_rat_db::schema::{active_generation, connection_context_value};
    let generation = active_generation(conn)?;
    let commit_sha = connection_context_value(conn, "commit_sha").unwrap_or_default();
    let worktree_id = connection_context_value(conn, "worktree_id").unwrap_or_default();
    // A whole-generation BARE open (`write_repo_generation_view`, e.g. the MCP read path) serves
    // EVERY live row for the repo with NO commit/worktree filter, writing both context keys empty.
    // Every scoped open — even a non-git base — carries a non-empty `worktree_id` (the root path
    // via `worktree_id_of`), so `("", "")` uniquely means "the active `files` view already
    // covers the whole live set", i.e. the heal reparses everything. Full coverage.
    if commit_sha.is_empty() && worktree_id.is_empty() {
        return Ok(true);
    }
    let has_other_scope: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM main.files
             WHERE repo_id = ?1 AND generation = ?2 AND commit_sha != ?3 AND worktree_id != ?4
         )",
        params![repo_id, generation, commit_sha, worktree_id],
        |row| row.get(0),
    )?;
    Ok(!has_other_scope)
}

/// Best-effort self-heal wrapper for the reconcile paths. Runs [`ensure_embedding_policy_current`]
/// ONLY when this reconcile is at the DEFAULT cap: the heal reclassifies + stamps the column at
/// DEFAULT, which only the DEFAULT-cap fast path can then read. A custom-cap reconcile would still
/// recompute the summary at its own cap, so healing at DEFAULT would just DOUBLE the parse pass —
/// skip it there. A heal failure is swallowed (the slow recompute is still correct), so it never
/// aborts the reconcile.
pub(crate) fn maybe_heal_embedding_policy(conn: &Connection, max_embedding_chars: usize) {
    if max_embedding_chars != DEFAULT_MAX_EMBEDDING_CHARS {
        return;
    }
    if let Err(err) = ensure_embedding_policy_current(conn) {
        tracing::debug!(?err, "embedding-policy column self-heal skipped");
    }
}

pub(crate) fn ensure_embedding_policy_current(conn: &Connection) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let version = rag_rat_db::meta::repo_meta(conn, &repo_id, EMBEDDING_POLICY_VERSION_KEY)?;
    let cap = rag_rat_db::meta::repo_meta(conn, &repo_id, EMBEDDING_POLICY_CAP_KEY)?;
    if version.as_deref() == Some(EMBEDDING_POLICY_VERSION)
        && cap.as_deref() == Some(DEFAULT_MAX_EMBEDDING_CHARS.to_string().as_str())
    {
        return Ok(()); // already certified current at the default cap — nothing to heal.
    }
    // The scan, writeback, and STAMP must be ONE serialized unit. The CLI reconcile path holds no
    // per-repo `WriteLock`, so an old watcher mid-upgrade could commit an old-classifier chunk
    // BETWEEN an unlocked scan and the stamp — the stamp would then certify a mixed-version column
    // and the fast summary would trust it. `BEGIN IMMEDIATE` takes the SQLite write lock up front,
    // so no other writer interleaves until we COMMIT. Held only for a one-per-version-bump heal
    // (same posture as a full rebuild). The temp table is created OUTSIDE the txn so a ROLLBACK
    // leaves it to be reused/dropped, not half-created.
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS embedding_policy_heal(id INTEGER PRIMARY KEY, policy \
         TEXT NOT NULL);",
    )?;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    // NEVER return with an open transaction: the caller (`maybe_heal_embedding_policy`) swallows
    // our error, and a dangling txn would then break the reconcile's later `BEGIN IMMEDIATE`
    // embed batches. So on ANY failure — including a COMMIT that itself fails — roll back
    // before returning.
    let outcome = heal_embedding_policy_locked(conn, &repo_id)
        .and_then(|()| conn.execute_batch("COMMIT;").map_err(Into::into));
    if let Err(err) = outcome {
        let _ = conn.execute_batch("ROLLBACK;");
        let _ = conn.execute_batch("DROP TABLE IF EXISTS temp.embedding_policy_heal;");
        return Err(err);
    }
    conn.execute_batch("DROP TABLE IF EXISTS temp.embedding_policy_heal;")?;
    Ok(())
}

/// The scan + writeback + stamp, run INSIDE the caller's `BEGIN IMMEDIATE` (which holds the write
/// lock). Re-checks coverage HERE, inside the lock: if another live scope exists — a second
/// linked-worktree overlay or an other-commit leftover — it would go un-healed while the repo-wide
/// stamp certified it, so we stamp NOTHING (committing an empty heal; every scope keeps
/// recomputing). Streams only the CHANGED chunks into the temp table (bounded RAM regardless of how
/// many reclassified; the recompute itself streams one file at a time, #379), then corrects
/// `main.chunks` with one `UPDATE ... FROM` after the read cursor closes.
fn heal_embedding_policy_locked(conn: &Connection, repo_id: &str) -> anyhow::Result<()> {
    conn.execute_batch("DELETE FROM temp.embedding_policy_heal;")?;
    if !active_scope_covers_all_live_rows(conn, repo_id)? {
        return Ok(());
    }
    {
        let mut stage = conn.prepare(
            "INSERT OR REPLACE INTO temp.embedding_policy_heal(id, policy) VALUES (?1, ?2)",
        )?;
        for_each_recomputed_chunk_policy(conn, DEFAULT_MAX_EMBEDDING_CHARS, |chunk, decision| {
            if decision.policy != chunk.current_policy {
                stage.execute(params![chunk.id, decision.policy])?;
            }
            Ok(())
        })?;
    }
    conn.execute(
        "UPDATE main.chunks
         SET embedding_policy = (SELECT policy FROM temp.embedding_policy_heal WHERE id = \
         main.chunks.id)
         WHERE id IN (SELECT id FROM temp.embedding_policy_heal)",
        [],
    )?;
    rag_rat_db::meta::set_repo_meta(
        conn,
        repo_id,
        EMBEDDING_POLICY_VERSION_KEY,
        EMBEDDING_POLICY_VERSION,
    )?;
    rag_rat_db::meta::set_repo_meta(
        conn,
        repo_id,
        EMBEDDING_POLICY_CAP_KEY,
        &DEFAULT_MAX_EMBEDDING_CHARS.to_string(),
    )?;
    Ok(())
}

#[cfg(test)]
mod reconstruct_file_text_tests {
    use super::{ChunkForPolicy, reconstruct_file_text};

    fn chunk(start_byte: usize, end_byte: usize, text: &str) -> ChunkForPolicy {
        ChunkForPolicy {
            id: 0,
            current_policy: "Embed".to_string(),
            chunk_kind: "code".to_string(),
            symbol_path: None,
            start_byte,
            end_byte,
            text: text.to_string(),
        }
    }

    #[test]
    fn reconstruct_file_text_rebuilds_from_overlapping_chunks() {
        // Chunks store `file[start..end]`; an overlapping inner chunk's already-buffered prefix is
        // skipped and only its tail appended. file = "abcdefgh".
        let chunks = [chunk(0, 5, "abcde"), chunk(3, 8, "defgh")];
        assert_eq!(reconstruct_file_text(&chunks).as_deref(), Some("abcdefgh"));
    }

    #[test]
    fn reconstruct_file_text_handles_abutting_and_multibyte() {
        // Abutting chunks tile cleanly; a UTF-8 boundary in the middle is respected by `.get()`.
        let chunks = [chunk(0, 3, "abc"), chunk(3, 6, "def")];
        assert_eq!(reconstruct_file_text(&chunks).as_deref(), Some("abcdef"));
        // "café" is 5 bytes (é = 2); split [0,3)="caf" + [3,5)="é".
        let mb = [chunk(0, 3, "caf"), chunk(3, 5, "é")];
        assert_eq!(reconstruct_file_text(&mb).as_deref(), Some("café"));
    }

    #[test]
    fn reconstruct_file_text_pads_whitespace_gaps_with_newlines() {
        // Bytes 3..5 are an unchunked whitespace-only gap (blank lines) → padded with '\n'. The
        // caller's sha check validates the guess; a wrong guess (spaces/CRLF) just fails the hash.
        let chunks = [chunk(0, 3, "abc"), chunk(5, 8, "fgh")];
        assert_eq!(reconstruct_file_text(&chunks).as_deref(), Some("abc\n\nfgh"));
    }
}
