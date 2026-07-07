use std::path::{Component, Path, PathBuf};

use super::*;

pub(crate) fn validate_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
    fs_root: Option<&Path>,
) -> anyhow::Result<String> {
    match binding.binding_kind.as_str() {
        "logical_symbol" => validate_logical_symbol_binding(conn, binding),
        "symbol" => validate_symbol_binding(conn, binding),
        "chunk" => validate_chunk_binding(conn, binding),
        "edge" => validate_edge_binding(conn, binding),
        "call_path" => validate_call_path_binding(conn, binding),
        "scip_moniker" => validate_moniker_binding(conn, binding),
        "path" => validate_path_binding(conn, binding, fs_root),
        "dir" => validate_dir_binding(conn, binding, fs_root),
        "commit" | "github" => Ok("unverified".to_string()),
        _ => Ok("unverified".to_string()),
    }
}
/// Validate a `dir` binding: current while at least one indexed file lives at or under the
/// directory, gone otherwise. Dir bindings are descriptive anchors with no `source_text_hash`
/// — they never go stale, only current or gone.
///
/// A dir holding ONLY non-indexed file types (shell scripts, `.yml` workflows, Containerfiles)
/// has no `files` rows by construction, so [`dir_has_files`] sees it as empty even though the
/// directory is alive in the repo. Before declaring `gone`, fall back to a filesystem existence
/// check against `fs_root` (the active checkout root; #98) so an area anchor to such a directory
/// stays current.
pub(crate) fn validate_dir_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
    fs_root: Option<&Path>,
) -> anyhow::Result<String> {
    let dir = binding.path.clone().unwrap_or_else(|| binding.binding_id.clone());
    if dir_has_files(conn, &dir)? {
        return Ok("current".to_string());
    }
    Ok(if dir_exists_on_disk(fs_root, &dir) { "current" } else { "gone" }.to_string())
}
pub(crate) fn validate_logical_symbol_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(id) = binding.logical_symbol_id
        && crate::query::symbol::lookup_logical_by_id(conn, id)?.is_some()
    {
        // The logical symbol is live. Its id is content-derived and STABLE across reindex, but
        // chunk ids are reassigned on every re-chunk — so the stored `chunk_id` is stale whenever
        // the symbol shifted lines (an edit ELSEWHERE in the same file leaves the symbol's
        // name/qualified_name/kind/signature, hence its stable id, unchanged while its chunk
        // moves). Re-derive the chunk from the live logical symbol before
        // content-validating; trusting the churned `chunk_id` made `validate_bound_chunk`
        // report `gone` for an unchanged symbol (#154 — the gone-on-every-reindex symptom
        // was really gone-on-any-line-shift). Falls through to the stored-chunk check only
        // when the logical symbol resolves to no chunk.
        if let Some(chunk) = chunk_for_logical_symbol(conn, id)? {
            binding.symbol_id = chunk.symbol_id;
            binding.chunk_id = Some(chunk.chunk_id);
            binding.path = Some(chunk.path);
            binding.start_line = Some(chunk.start_line);
            binding.end_line = Some(chunk.end_line);
            return Ok(match source_hash_for_memory(conn, &binding.memory_id)? {
                Some(expected) if expected != chunk.text_hash => "stale".to_string(),
                _ => "current".to_string(),
            });
        }
        return validate_bound_chunk(conn, binding);
    }
    // Scope the qualified-name relocation to the ACTIVE repo. `logical_symbols` is direct-scoped by
    // `repo_id` (V040) and its ids are repo-distinct, so a consolidated DB can hold the SAME
    // qualified name under a sibling repo. Without the predicate, validating repo A's memory (whose
    // remembered symbol was deleted/renamed) could rebind it to repo B's logical id/path and report
    // `relocated` instead of `gone`/`stale`. The `files`-view queries elsewhere in this module are
    // repo-scoped for free through the scope view; `logical_symbols` is a direct table, so it needs
    // the explicit filter.
    let active_repo_id = crate::index::schema::active_repo_id(conn)?;
    let relocated = conn
        .query_row(
            "
            SELECT id, path
            FROM logical_symbols
            WHERE qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
              AND repo_id = ?2
            LIMIT 1
            ",
            params![&binding.binding_id, active_repo_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((id, path)) = relocated {
        binding.logical_symbol_id = Some(id);
        binding.path = Some(path);
        if let Some(chunk) = chunk_for_logical_symbol(conn, id)? {
            binding.symbol_id = chunk.symbol_id;
            binding.chunk_id = Some(chunk.chunk_id);
            binding.start_line = Some(chunk.start_line);
            binding.end_line = Some(chunk.end_line);
        }
        return Ok("relocated".to_string());
    }
    // Cross-file move: bare name + content hash fallback (same path as symbol binding).
    if let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? {
        let short = short_symbol_name(&binding.binding_id, binding.path.as_deref()).to_string();
        if let Some(m) = relocate_symbol_by_name(conn, &short, &hash)? {
            // binding_id becomes the relocated member symbol's qualified_name, not a
            // logical_symbols.qualified_name. The stable logical_symbol_id arm re-matches on the
            // next reindex; if that ever goes stale this bare-name fallback recovers it — the
            // logical-qualified-name arm above intentionally won't re-match this binding again.
            binding.binding_id = m.binding_id;
            binding.logical_symbol_id = m.logical_symbol_id;
            binding.symbol_id = Some(m.symbol_id);
            binding.path = Some(m.path);
            binding.chunk_id = m.chunk_id;
            binding.start_line = m.start_line;
            binding.end_line = m.end_line;
            binding.symbol_kind = m.symbol_kind;
            binding.signature_hash = m.signature_hash;
            return Ok("relocated".to_string());
        }
    }
    relocate_via_moniker_or_gone(conn, binding)
}
pub(crate) fn validate_symbol_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(id) = binding.symbol_id
        && crate::query::symbol::lookup_by_id(conn, id)?.is_some()
    {
        return validate_bound_chunk(conn, binding);
    }
    let relocated = conn
        .query_row(
            "
            SELECT symbols.id, files.path
            FROM symbols
            JOIN files ON files.id = symbols.file_id
            WHERE symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
            LIMIT 1
            ",
            [&binding.binding_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((id, path)) = relocated {
        binding.symbol_id = Some(id);
        binding.logical_symbol_id = logical_symbol_id_for_symbol(conn, id)?;
        binding.path = Some(path);
        if let Some(chunk) = chunk_for_symbol(conn, id, &binding.binding_id)? {
            binding.chunk_id = Some(chunk.chunk_id);
            binding.start_line = Some(chunk.start_line);
            binding.end_line = Some(chunk.end_line);
        }
        let (kind, sig) = symbol_signal(conn, id)?;
        binding.symbol_kind = kind;
        binding.signature_hash = sig;
        return Ok("relocated".to_string());
    }
    // Cross-file move: qualified_name changed with the path. Match by bare name + content hash.
    if let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? {
        let short = short_symbol_name(&binding.binding_id, binding.path.as_deref()).to_string();
        if let Some(m) = relocate_symbol_by_name(conn, &short, &hash)? {
            binding.binding_id = m.binding_id;
            binding.symbol_id = Some(m.symbol_id);
            binding.logical_symbol_id = m.logical_symbol_id;
            binding.path = Some(m.path);
            binding.chunk_id = m.chunk_id;
            binding.start_line = m.start_line;
            binding.end_line = m.end_line;
            binding.symbol_kind = m.symbol_kind;
            binding.signature_hash = m.signature_hash;
            return Ok("relocated".to_string());
        }
    }
    relocate_via_moniker_or_gone(conn, binding)
}

/// Last-resort relocation for a symbol/logical_symbol binding whose qualified-name and
/// name+content-hash anchors are exhausted: re-resolve the memory's recorded SCIP moniker against
/// current oracle data (#70). A unique live match — semantic identity, robust to content edits the
/// hash fallback can't survive — relocates with `relocation_reason = "moniker-match"`; otherwise
/// the binding is gone.
fn relocate_via_moniker_or_gone(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(m) = relocate_binding_by_moniker(conn, binding)? {
        binding.binding_id = m.binding_id;
        binding.symbol_id = Some(m.symbol_id);
        binding.logical_symbol_id = m.logical_symbol_id;
        binding.path = Some(m.path);
        binding.chunk_id = m.chunk_id;
        binding.start_line = m.start_line;
        binding.end_line = m.end_line;
        binding.symbol_kind = m.symbol_kind;
        binding.signature_hash = m.signature_hash;
        binding.relocation_reason = Some(MONIKER_MATCH_REASON.to_string());
        return Ok("relocated".to_string());
    }
    Ok("gone".to_string())
}
pub(crate) fn validate_chunk_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let status = validate_bound_chunk(conn, binding)?;
    if status != "gone" {
        return Ok(status);
    }
    let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? else {
        return Ok("gone".to_string());
    };
    let Some(chunk) = relocate_chunk_by_hash(conn, &hash)? else {
        return Ok("gone".to_string());
    };
    binding.binding_id = chunk.chunk_id.to_string();
    binding.chunk_id = Some(chunk.chunk_id);
    binding.path = Some(chunk.path);
    binding.start_line = Some(chunk.start_line);
    binding.end_line = Some(chunk.end_line);
    Ok("relocated".to_string())
}
pub(crate) fn validate_edge_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    if let Some(edge_id) = binding.edge_id
        && let Some(edge) = edge_by_id(conn, edge_id)?
    {
        binding.path = Some(edge.path);
        binding.start_line = Some(edge.start_line);
        binding.end_line = Some(edge.end_line);
        binding.symbol_id = None;
        binding.logical_symbol_id = None;
        return validate_bound_edge_source_hash(conn, binding, &edge.source_hash);
    }
    let Some(edge) = edge_by_fingerprint(conn, &binding.binding_id)? else {
        return Ok("gone".to_string());
    };
    binding.edge_id = Some(edge.edge_id);
    binding.path = Some(edge.path);
    binding.start_line = Some(edge.start_line);
    binding.end_line = Some(edge.end_line);
    binding.symbol_id = None;
    binding.logical_symbol_id = None;
    Ok("relocated".to_string())
}
pub(crate) fn validate_call_path_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    // Re-check each stored edge behind the server-derived hash (#38). Exact-fingerprint match →
    // the edge is unchanged; loose name/kind/target match → it moved lines (relocated); neither →
    // that edge is gone.
    let mut stmt = conn.prepare(
        "
        SELECT edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name
        FROM repo_memory_call_path_edges
        WHERE memory_id = ?1 AND edge_sequence_hash = ?2
        ORDER BY ordinal
        ",
    )?;
    let edges = stmt
        .query_map(params![binding.memory_id, binding.binding_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if edges.is_empty() {
        // Legacy client-supplied hash with no stored edges — honest-but-weak: current only as a
        // row, never verifiable against the graph.
        let exists = conn.query_row(
            "SELECT COUNT(*) FROM repo_memory_call_paths
             WHERE memory_id = ?1 AND edge_sequence_hash = ?2",
            params![binding.memory_id, binding.binding_id],
            |row| row.get::<_, i64>(0),
        )?;
        return Ok(if exists > 0 { "unverified" } else { "gone" }.to_string());
    }

    let total = edges.len();
    let mut relocated = 0usize;
    let mut gone = 0usize;
    for (fingerprint, from_name, to_name, edge_kind, target) in &edges {
        if edge_by_fingerprint(conn, fingerprint)?.is_some() {
            continue;
        }
        if call_path_edge_relocatable(
            conn,
            from_name.as_deref(),
            to_name.as_deref(),
            edge_kind,
            target.as_deref(),
        )? {
            relocated += 1;
        } else {
            gone += 1;
        }
    }

    Ok(if gone == total {
        "gone"
    } else if gone > 0 {
        "stale"
    } else if relocated > 0 {
        "relocated"
    } else {
        "current"
    }
    .to_string())
}

/// Is there still an edge matching this one's loose identity (names/kind/target), ignoring line
/// numbers? Used to call a call-path edge `relocated` (moved) rather than `gone` (#38). The
/// `JOIN files` is load-bearing (A6): `edges`/`edges_data` are NOT generation-scoped, so without it
/// a superseded generation's edge rows (dead until gc) would keep matching — a genuinely-deleted
/// call site would be reported `relocated` forever instead of `gone`. The join drops
/// dead-generation edges (their `source_file_id` file row is absent from the live scope view) and
/// scopes to the active repo for free, matching the sibling helpers `edge_by_fingerprint` /
/// `call_path_edge_by_id`.
fn call_path_edge_relocatable(
    conn: &Connection,
    from_name: Option<&str>,
    to_name: Option<&str>,
    edge_kind: &str,
    target_qualified_name: Option<&str>,
) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "
        SELECT COUNT(*)
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edge_kind = ?3
          AND COALESCE(from_name, '') = COALESCE(?1, '')
          AND COALESCE(to_name, '') = COALESCE(?2, '')
          AND COALESCE(target_qualified_name, '') = COALESCE(?4, '')
        ",
        params![from_name, to_name, edge_kind, target_qualified_name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}
pub(crate) fn validate_bound_edge_source_hash(
    conn: &Connection,
    binding: &RepoMemoryBinding,
    current_source_hash: &str,
) -> anyhow::Result<String> {
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != current_source_hash => Ok("stale".to_string()),
        _ => Ok("current".to_string()),
    }
}
pub(crate) fn validate_bound_chunk(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<String> {
    let Some(chunk_id) = binding.chunk_id else {
        return Ok("unverified".to_string());
    };
    let Some(chunk) = chunk_by_id(conn, chunk_id)? else {
        return Ok("gone".to_string());
    };
    binding.path = Some(chunk.path);
    binding.start_line = Some(chunk.start_line);
    binding.end_line = Some(chunk.end_line);
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != chunk.text_hash => Ok("stale".to_string()),
        _ => Ok("current".to_string()),
    }
}
pub(crate) fn validate_path_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
    fs_root: Option<&Path>,
) -> anyhow::Result<String> {
    let Some(path) = binding.path.as_deref() else {
        return Ok("unverified".to_string());
    };
    let current_hash = conn
        .query_row(
            "SELECT sha256 FROM files WHERE path = ?1 ORDER BY id DESC LIMIT 1",
            [path],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(current_hash) = current_hash else {
        // No `files` row — but `files` holds only files in the configured indexed language set, so
        // a binding to a path OUTSIDE that set (a Containerfile, shell script, `.yml` workflow,
        // `.toml` config) has no row by construction and is indistinguishable from a deleted file
        // by the index alone. Fall back to a filesystem existence check against `fs_root` (the
        // active checkout root) before declaring `gone` — acting on a false `gone` would delete
        // valid guidance (#98). A BARE path is an area anchor → `current` while the file is
        // present; a SPANNED `path:start-end` binding has no chunk to hash → `unverified`
        // (alive but un-content-verifiable), never `gone`. The target must be a FILE: a
        // path binding names a file, so a directory now occupying that name leaves the file
        // genuinely `gone`.
        let status = match path_is_file_on_disk(fs_root, path) {
            true if binding.start_line.is_none() && binding.end_line.is_none() => "current",
            true => "unverified",
            false => "gone",
        };
        return Ok(status.to_string());
    };
    // A BARE path binding (no line span) is an AREA anchor, like a `dir` binding: the claim is
    // "this note is about this file", not "this file's bytes are X" — so it is current while the
    // file is indexed, never content-stale. Hashing the whole file made every commit stale every
    // area-level note bound to a touched file, permanently (nothing refreshes the hash), which
    // buried the real staleness signals under noise. Only a SPANNED `path:start-end` binding
    // claims specific content and keeps the content-hash check.
    if binding.start_line.is_none() && binding.end_line.is_none() {
        return Ok("current".to_string());
    }
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != current_hash => Ok("stale".to_string()),
        _ => Ok("current".to_string()),
    }
}
/// The persisted `source_root` (the on-disk repo root recorded in `repo_meta` at
/// open/rebuild/incremental). `None` on a raw connection that never recorded it (some test
/// fixtures). This is a SINGLE shared value — under a shared DB across git worktrees it reflects
/// whichever worktree last indexed, which is why [`validate_memories`] prefers the caller-supplied
/// active checkout root and only falls back to this (#98 review).
fn persisted_source_root(conn: &Connection) -> Option<PathBuf> {
    // `source_root` moved to `repo_meta` (V039); resolve the active repo (the lone one in phase A).
    let repo_id = crate::index::schema::active_repo_id(conn).ok()?;
    crate::index::repo_meta(conn, &repo_id, "source_root").ok().flatten().map(PathBuf::from)
}

/// The filesystem root the off-index existence checks resolve against: the caller-supplied ACTIVE
/// checkout root (`storage.source_root`, correct under a multi-worktree shared DB) when known, else
/// the single persisted `repo_meta.source_root` (#98 review).
pub(crate) fn effective_fs_root(conn: &Connection, active_root: Option<&Path>) -> Option<PathBuf> {
    active_root.map(Path::to_path_buf).or_else(|| persisted_source_root(conn))
}

/// Whether a binding's stored `path`/`dir` honors the repo-root-relative contract: not absolute and
/// free of any `..` / root-prefix component that could escape `source_root` (#98 review). A binding
/// violating it is treated as not-on-disk, so a stray absolute/`..` path can't keep an out-of-repo
/// file's anchor alive. A leading `./` (`CurDir`) and an empty string (the repo root) are fine.
fn is_repo_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Whether `path` (repo-root-relative) resolves to an existing FILE under `root` — the off-index
/// existence check for non-indexed file types (#98). `false` when `root` is unknown (so a
/// connection without a source_root falls back to the pre-#98 `gone` behavior) or the path is not
/// repo-relative.
fn path_is_file_on_disk(root: Option<&Path>, path: &str) -> bool {
    root.is_some_and(|root| is_repo_relative(path) && root.join(path).is_file())
}

/// Whether `dir` (repo-root-relative, `""` = repo root) resolves to an existing directory under
/// `root` — the off-index dir existence check (#98).
fn dir_exists_on_disk(root: Option<&Path>, dir: &str) -> bool {
    root.is_some_and(|root| is_repo_relative(dir) && root.join(dir).is_dir())
}

pub(crate) fn source_hash_for_memory(
    conn: &Connection,
    memory_id: &str,
) -> anyhow::Result<Option<String>> {
    conn.query_row("SELECT source_text_hash FROM repo_memories WHERE id = ?1", [memory_id], |row| {
        row.get::<_, Option<String>>(0)
    })
    .optional()
    .map(|value| value.flatten())
    .map_err(Into::into)
}
pub(crate) fn validate_kind(kind: &str) -> anyhow::Result<()> {
    match kind {
        "Invariant"
        | "Decision"
        | "RejectedAlternative"
        | "Risk"
        | "BugPattern"
        | "TestExpectation"
        | "PerformanceNote"
        | "SecurityNote"
        | "FFIBoundary"
        | "PlatformQuirk"
        | "FollowUp"
        | "OpenQuestion"
        | "Obsolete"
        // Polymorphic graph-node kinds (#465): legitimately unanchored (a Concept / standalone
        // Task lives as a graph node with no code binding — see resolve_binding / #463).
        | "Task"
        | "Concept" => Ok(()),
        _ => anyhow::bail!("invalid memory kind `{kind}`"),
    }
}
/// A polymorphic node's payload (#465) must be a JSON OBJECT — so it round-trips, and so it can be
/// folded into `content_hash` in phase B. An array/scalar/malformed value is rejected; `None` (no
/// payload) is fine. Payload-closure (a payload carries no node/edge references) is enforced once
/// the edge model (#464) defines what a reference IS — a payload cannot "reference a node" before
/// nodes are referenceable, so there is nothing to reject here yet.
pub(crate) fn validate_payload(payload_json: Option<&str>) -> anyhow::Result<()> {
    let Some(payload) = payload_json else {
        return Ok(());
    };
    let value: serde_json::Value = serde_json::from_str(payload)
        .map_err(|e| anyhow::anyhow!("payload_json is not valid JSON: {e}"))?;
    if !value.is_object() {
        anyhow::bail!("payload_json must be a JSON object");
    }
    Ok(())
}

pub(crate) fn validate_confidence(confidence: &str) -> anyhow::Result<()> {
    match confidence {
        "high" | "medium" | "low" => Ok(()),
        _ => anyhow::bail!("invalid memory confidence `{confidence}`"),
    }
}
pub(crate) fn validate_status(status: &str) -> anyhow::Result<()> {
    match status {
        "active" | "stale" | "obsolete" | "rejected" => Ok(()),
        _ => anyhow::bail!("invalid memory status `{status}`"),
    }
}
pub(crate) fn validate_source(source: &str) -> anyhow::Result<()> {
    match source {
        "agent" | "human" | "imported" | "generated" => Ok(()),
        _ => anyhow::bail!("invalid memory source `{source}`"),
    }
}
pub(crate) fn validate_len(field: &str, value: &str, max: usize) -> anyhow::Result<()> {
    let len = value.trim().chars().count();
    if len == 0 {
        anyhow::bail!("memory {field} must not be empty");
    }
    if len > max {
        anyhow::bail!("memory {field} exceeds {max} characters");
    }
    Ok(())
}
/// Derive a memory id. On the post-A5 schema (`scope` is `Some`) the owning repo is FOLDED into the
/// hash suffix: two repos creating IDENTICAL content in the same millisecond would otherwise derive
/// the same `mem_<ms>_<hash-prefix>` id — the repo-scoped dedupe (correctly) sees no duplicate, and
/// the insert explodes on the global PK. Phase B and beyond: memory ids must remain globally unique
/// and coordination-free (they replicate across devices/DBs with no allocator) — folding the repo
/// INTO the hash strengthens that property, never weakens it. Pre-A5 (`None`) keeps the original
/// repo-blind suffix so a single-repo DB's ids are unchanged.
pub(crate) fn memory_id(now: i64, input_hash: &str, scope: &Option<String>) -> String {
    let suffix = match scope {
        Some(repo_id) => hex_sha256(format!("{repo_id}\u{1f}{input_hash}").as_bytes())
            .chars()
            .take(12)
            .collect::<String>(),
        None => input_hash.chars().take(12).collect::<String>(),
    };
    format!("mem_{now:x}_{suffix}")
}
pub(crate) fn memory_input_hash(
    kind: &str,
    title: &str,
    body: &str,
    tags: &[String],
    payload_json: Option<&str>,
) -> String {
    let mut normalized_tags = tags.iter().map(|tag| tag.trim()).collect::<Vec<_>>();
    normalized_tags.sort_unstable();
    // The payload is folded RAW (not canonicalized): this is the create-time dedup / id seed, which
    // wants EXACT-input identity so two nodes with identical text but different payloads get
    // different ids and neither collapses onto the other (#465). This is NOT the dream content
    // identity — `dream::note_content_hash` is separate, and its CANONICAL payload fold is deferred
    // to phase B (#404).
    hex_sha256(
        format!(
            "{kind}\n{}\n{}\n{}\n{}",
            title.trim(),
            body.trim(),
            normalized_tags.join(","),
            payload_json.unwrap_or("")
        )
        .as_bytes(),
    )
}
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
pub(crate) fn fts_query(query: &str) -> String {
    let terms = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    terms.join(" OR ")
}
