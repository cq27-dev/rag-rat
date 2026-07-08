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
    // #491: one qualified name can hold several live twins (a struct and its impl block;
    // overloads with distinct signatures), so a bare `LIMIT 1` is a plan-order coin flip that
    // can land a struct-bound memory on the impl row. The binding stores the V014 relocation
    // discriminators (`symbol_kind`, `signature_hash`) — fetch every twin (with a member
    // signature: all members of a group share it, since the signature is part of the logical
    // key) and prefer the one that agrees, tiebreaking deterministically by id.
    let candidates: Vec<RelocationTwin> = {
        let mut stmt = conn.prepare(
            "
            SELECT ls.id, ls.path, ls.kind,
                   (SELECT s.signature FROM logical_symbol_members m
                      JOIN symbols s ON s.id = m.symbol_id
                     WHERE m.logical_symbol_id = ls.id LIMIT 1)
            FROM logical_symbols ls
            WHERE ls.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
              AND ls.repo_id = ?2
            ORDER BY ls.id
            ",
        )?;
        let rows = stmt.query_map(params![&binding.binding_id, active_repo_id], |row| {
            Ok(RelocationTwin {
                id: row.get(0)?,
                path: row.get(1)?,
                kind: row.get(2)?,
                signature: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let relocated = pick_relocation_twin(
        candidates,
        binding.symbol_kind.as_deref(),
        binding.signature_hash.as_deref(),
    );
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
/// A live logical-symbol row sharing the dead binding's qualified name — a relocation candidate.
struct RelocationTwin {
    id: i64,
    path: String,
    kind: String,
    signature: Option<String>,
}

/// Choose which same-qualified-name twin a gone binding relocates onto (#491): the stored
/// discriminators win — kind agreement outranks signature-hash agreement (a rename usually keeps
/// the kind but changes the signature text) — and equal evidence falls back to the lowest id, so
/// the pick is deterministic instead of plan-order. `None` evidence on the binding degrades
/// gracefully: every candidate scores equally and the id tiebreak decides, matching the old
/// behavior for bindings that predate the V014 discriminators.
fn pick_relocation_twin(
    candidates: Vec<RelocationTwin>,
    bound_kind: Option<&str>,
    bound_signature_hash: Option<&str>,
) -> Option<(i64, String)> {
    candidates
        .into_iter()
        .map(|twin| {
            let kind_agrees = bound_kind == Some(twin.kind.as_str());
            let signature_agrees = match (bound_signature_hash, &twin.signature) {
                (Some(bound), Some(sig)) => bound == hex_sha256(sig.trim().as_bytes()),
                _ => false,
            };
            // Candidates arrive id-ascending; max_by_key keeps the LAST maximum, so compare on
            // (score, negated id) to keep the lowest-id winner among evidence ties.
            let score = (u8::from(kind_agrees) << 1) | u8::from(signature_agrees);
            (score, -twin.id, twin)
        })
        .max_by_key(|(score, neg_id, _)| (*score, *neg_id))
        .map(|(_, _, twin)| (twin.id, twin.path))
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
/// The polymorphic graph-node kinds — `Task` and `Concept` (#463/#465). They ALONE may be created
/// UNANCHORED (no code binding) AND may carry a structured `payload_json`; every other kind is a
/// plain note (anchors to code, no payload). The SINGLE source of truth for the unanchored-create
/// gate (`create`/`update_memory`), the payload-kind gate (`validate_payload`), and the dream
/// verifier's `memory_unverifiable` exemption — they must never drift, or a create the gate allows
/// becomes self-inflicted dream noise, or an off-contract payload/anchor slips through.
pub(crate) fn is_polymorphic_node_kind(kind: &str) -> bool {
    matches!(kind, "Task" | "Concept")
}

/// Validate a memory's `payload_json` for its `kind`. Only the polymorphic graph-node kinds
/// (`is_polymorphic_node_kind`) may carry a payload, and it must be a JSON OBJECT (so it
/// round-trips and can be folded into the identity hash). A payload on a plain-note kind, or a
/// non-object payload, is rejected; `None` (no payload) is always fine.
///
/// Payload-closure — that a relationship between nodes lives in a typed EDGE (#464
/// `repo_node_edges`) rather than embedded in an opaque payload — is a documented CONVENTION,
/// deliberately NOT a hard validator: reliably detecting a "node reference" inside arbitrary JSON
/// isn't feasible without false positives (a reserved-word scan would reject a legitimate
/// `{"tracks": [...]}` domain field, since `tracks` is also an edge relation). It is steered by the
/// edge API + tool docs, not rejected here.
pub(crate) fn validate_payload(kind: &str, payload_json: Option<&str>) -> anyhow::Result<()> {
    let Some(payload) = payload_json else {
        return Ok(());
    };
    if !is_polymorphic_node_kind(kind) {
        anyhow::bail!(
            "a `{kind}` memory carries no payload (only Task/Concept may have a payload_json)"
        );
    }
    // Strict parse: reject a LITERAL duplicate object key (serde_json's default silently keeps the
    // last, but parsers disagree on which wins → a cross-device hash divergence). Complete for a
    // caller that passes the RAW payload string (CLI, direct core); the MCP JSON-RPC transport
    // parses tool args into a `Value` upstream, collapsing dups deterministically before this runs
    // (harmless among serde_json writers today) — hardening that boundary is #488.
    let value = crate::canonical::parse_rejecting_duplicate_keys(payload)
        .map_err(|e| anyhow::anyhow!("payload_json invalid: {e}"))?;
    if !value.is_object() {
        anyhow::bail!("payload_json must be a JSON object");
    }
    // Reject on WRITE anything the canonical encoder can't hash (two keys that NFC-normalize to the
    // same key → an ambiguous dup-key map), so a STORED payload always encodes cleanly and
    // `content_hash` can treat the canonical encoding as effectively infallible.
    if let Some(err) = crate::canonical::payload_encoding_error(&value) {
        anyhow::bail!("payload_json is not canonically encodable: {err}");
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
/// The canonical, content-addressed identity of a memory's CONTENT (phase B §5.5) — hex SHA-256
/// over a canonical CBOR array `[domain, payload_schema_version, nfc(trim title), nfc(trim body),
/// payload]`. `kind` and `tags` are EXCLUDED (a pure reclassification / re-tag must not churn the
/// content-addressed derived overlays); the payload IS folded, and its self-described
/// `schema_version` is folded separately, so a payload-schema migration is a deliberate identity
/// change. Distinct from `memory_input_hash` (the raw create-time dedup/id seed) and NEVER raw
/// concatenation. A `None` payload folds as CBOR null with `schema_version = 0`.
// Frozen §5.5 primitive, golden-vector-pinned; first production consumer is the op-log increment
// (#404).
#[allow(dead_code)]
pub(crate) fn content_hash(title: &str, body: &str, payload_json: Option<&str>) -> String {
    use crate::canonical::nfc;
    let (schema_version, payload_element) = payload_cbor_element(payload_json);
    let mut buf = Vec::new();
    {
        let mut enc = minicbor::Encoder::new(&mut buf);
        // §5.5: array([domain, schema_version, trimmed_title_nfc, trimmed_body_nfc, payload]).
        // These fixed ops write to a `Vec`, so they are infallible.
        enc.array(5).expect("cbor to a Vec is infallible");
        enc.str("rag-rat/content-hash/1").expect("cbor to a Vec is infallible");
        enc.u64(schema_version).expect("cbor to a Vec is infallible");
        enc.str(&nfc(title.trim())).expect("cbor to a Vec is infallible");
        enc.str(&nfc(body.trim())).expect("cbor to a Vec is infallible");
    }
    // Append the pre-encoded payload as the 5th array element (one CBOR item either way).
    buf.extend_from_slice(&payload_element);
    hex_sha256(&buf)
}

/// The `(schema_version, pre-encoded payload CBOR)` for `content_hash`. A payload folds as
/// CANONICAL CBOR only when it parses with NO duplicate key (`serde_json` silently keeps the last
/// of a LITERAL dup — parser-dependent — so we parse strictly) AND encodes with no NFC-duplicate
/// key; otherwise — invalid JSON, a literal-dup, or an NFC-dup, all of which `validate_payload`
/// rejects on write, so only a legacy / out-of-band payload reaches here — its RAW bytes fold as a
/// CBOR byte string (a distinct major type, so it can't collide with a structured payload). This
/// keeps `content_hash` TOTAL, DETERMINISTIC, and PARSER-INDEPENDENT: it runs on every memory in
/// the dream pass and must never panic, and both duplicate-key kinds must hash identically across
/// devices.
fn payload_cbor_element(payload_json: Option<&str>) -> (u64, Vec<u8>) {
    use crate::canonical::{encode_canonical_json, parse_rejecting_duplicate_keys};
    let Some(raw) = payload_json else {
        let mut buf = Vec::new();
        minicbor::Encoder::new(&mut buf).null().expect("cbor to a Vec is infallible");
        return (0, buf);
    };
    // A payload folds structurally ONLY when it is a JSON OBJECT (what `validate_payload` accepts)
    // AND encodes canonically. A non-object (`null`, a scalar, an array) is rejected on write just
    // like a dup-key / invalid one, so — crucially — it must NOT fold as structured CBOR: a text
    // payload of `"null"` would encode to the SAME CBOR-null element as `None`, colliding a
    // no-payload memory with a `null`-payload one and never invalidating overlays.
    if let Ok(value) = parse_rejecting_duplicate_keys(raw)
        && value.is_object()
    {
        let mut buf = Vec::new();
        if encode_canonical_json(&value, &mut minicbor::Encoder::new(&mut buf)).is_ok() {
            let version =
                value.get("schema_version").and_then(serde_json::Value::as_u64).unwrap_or(0);
            return (version, buf);
        }
    }
    // Legacy / out-of-band non-canonical payload (non-object, literal-dup, NFC-dup, or invalid
    // JSON).
    let mut buf = Vec::new();
    minicbor::Encoder::new(&mut buf).bytes(raw.as_bytes()).expect("cbor to a Vec is infallible");
    (0, buf)
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

#[cfg(test)]
mod content_hash_tests {
    use super::content_hash;

    #[test]
    fn folds_payload_and_schema_version() {
        let base = content_hash("t", "b", None);
        assert_ne!(base, content_hash("t", "b", Some(r#"{"schema_version":1,"x":1}"#)));
        // A payload VALUE change changes the hash.
        assert_ne!(
            content_hash("t", "b", Some(r#"{"schema_version":1,"x":1}"#)),
            content_hash("t", "b", Some(r#"{"schema_version":1,"x":2}"#)),
        );
        // A schema_version bump is a DELIBERATE identity change (§5.5), even with identical fields.
        assert_ne!(
            content_hash("t", "b", Some(r#"{"schema_version":1,"x":1}"#)),
            content_hash("t", "b", Some(r#"{"schema_version":2,"x":1}"#)),
        );
    }

    #[test]
    fn payload_is_canonicalized_key_order_and_whitespace_dont_matter() {
        assert_eq!(
            content_hash("t", "b", Some(r#"{"a":1,"b":2}"#)),
            content_hash("t", "b", Some(r#"{ "b": 2, "a": 1 }"#)),
        );
    }

    #[test]
    fn title_body_are_trimmed_and_nfc_normalized() {
        assert_eq!(content_hash("t", "b", None), content_hash("  t  ", "\nb\t", None));
        // é precomposed (U+00E9) vs e + combining acute (U+0065 U+0301).
        assert_eq!(content_hash("caf\u{00e9}", "b", None), content_hash("cafe\u{0301}", "b", None),);
    }

    #[test]
    fn no_payload_differs_from_empty_object() {
        // `None` (CBOR null) is a distinct identity from an empty-object payload `{}`.
        assert_ne!(content_hash("t", "b", None), content_hash("t", "b", Some("{}")));
    }

    #[test]
    fn non_object_legacy_payload_does_not_collide_with_none() {
        // A non-object text payload (`"null"`, a scalar) folds as raw BYTES, not structured CBOR —
        // else `"null"` would encode to the same CBOR-null element as `None` and collide.
        assert_ne!(content_hash("t", "b", Some("null")), content_hash("t", "b", None));
        assert_ne!(content_hash("t", "b", Some("42")), content_hash("t", "b", None));
    }

    #[test]
    fn validate_rejects_non_integer_number_payloads() {
        // A float can't be a reliable content-hash input (binary64 collapse) — rejected on write.
        let err = super::validate_payload("Task", Some(r#"{"score":0.85}"#)).unwrap_err();
        assert!(err.to_string().contains("canonically encodable"), "{err}");
    }

    #[test]
    fn validate_rejects_nfc_duplicate_payload_keys() {
        // A payload with "café" precomposed AND decomposed collapses to one NFC key on write —
        // rejected so `content_hash` never sees an ambiguous dup-key map.
        let dup = "{\"caf\u{00e9}\":1,\"cafe\u{0301}\":2}";
        let err = super::validate_payload("Task", Some(dup)).unwrap_err();
        assert!(err.to_string().contains("canonically encodable"), "{err}");
    }

    #[test]
    fn validate_rejects_literal_duplicate_payload_keys() {
        // serde_json would silently keep the last; reject so the cross-device hash is well-defined.
        let err = super::validate_payload("Task", Some(r#"{"a":1,"a":2}"#)).unwrap_err();
        assert!(err.to_string().contains("duplicate object key"), "{err}");
        // Nested duplicates are caught at any depth.
        let nested = super::validate_payload("Task", Some(r#"{"x":{"a":1,"a":2}}"#)).unwrap_err();
        assert!(nested.to_string().contains("duplicate object key"), "{nested}");
    }

    #[test]
    fn legacy_dup_key_payloads_hash_as_raw_bytes() {
        // A dup-key payload can't be created (validate rejects it), but a legacy / out-of-band one
        // must NOT crash the dream pass. BOTH dup kinds — NFC-normalized and LITERAL — fold the raw
        // bytes: total, deterministic, and PARSER-INDEPENDENT.
        let nfc_dup = "{\"caf\u{00e9}\":1,\"cafe\u{0301}\":2}";
        let literal_dup = r#"{"a":1,"a":2}"#;
        for dup in [nfc_dup, literal_dup] {
            let h = content_hash("t", "b", Some(dup));
            assert_eq!(h, content_hash("t", "b", Some(dup)), "deterministic fallback");
            assert_eq!(h.len(), 64, "still a sha-256 hex digest");
        }
        // A literal-dup must NOT collapse to serde's silent last-wins interpretation.
        assert_ne!(
            content_hash("t", "b", Some(literal_dup)),
            content_hash("t", "b", Some(r#"{"a":2}"#)),
            "raw-bytes fold is distinct from serde last-wins",
        );
    }

    #[test]
    fn golden_vector_pins_the_canonical_rule() {
        // A stored dream freshness hash is content-addressed on this exact encoding — pin it so any
        // change to the §5.5 canonical rule (which would silently re-derive every dream overlay) is
        // caught and the domain tag `rag-rat/content-hash/1` bumped deliberately.
        assert_eq!(
            content_hash("title", "body", Some(r#"{"schema_version":1,"status":"todo"}"#)),
            "5a07a01d8bc81c1dc9a80a2ea8707fc9d3f3bcfac5a6c31761deb3d2be2107b2"
        );
    }
}
