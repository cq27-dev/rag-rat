use std::path::{Component, Path, PathBuf};

use rag_rat_db::schema::TOMBSTONE_FILE_KIND;

use super::*;

pub(crate) fn validate_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
    fs_root: Option<&Path>,
) -> anyhow::Result<AnchorStatus> {
    // A kind outside this build's set — a binding a peer on a newer build replicated here — has no
    // validator, so it is reported, never guessed at.
    let Ok(kind) = BindingKind::from_db_str(&binding.binding_kind) else {
        return Ok(AnchorStatus::Unverified);
    };
    match kind {
        BindingKind::LogicalSymbol => validate_logical_symbol_binding(conn, binding),
        BindingKind::Symbol => validate_symbol_binding(conn, binding),
        BindingKind::Chunk => validate_chunk_binding(conn, binding),
        BindingKind::Edge => validate_edge_binding(conn, binding),
        BindingKind::CallPath => validate_call_path_binding(conn, binding),
        BindingKind::ScipMoniker => validate_moniker_binding(conn, binding),
        BindingKind::Path => validate_path_binding(conn, binding, fs_root),
        BindingKind::Dir => validate_dir_binding(conn, binding, fs_root),
        BindingKind::Commit | BindingKind::Tracker => Ok(AnchorStatus::Unverified),
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
) -> anyhow::Result<AnchorStatus> {
    let dir = binding.path.clone().unwrap_or_else(|| binding.current_binding_id().to_string());
    if dir_has_files(conn, &dir)? {
        return Ok(AnchorStatus::Current);
    }
    Ok(if dir_exists_on_disk(fs_root, &dir) { AnchorStatus::Current } else { AnchorStatus::Gone })
}
/// Validate a binding against the live logical symbol `id` it resolves to.
fn validate_live_logical_symbol(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
    id: i64,
    qualified_name: String,
) -> anyhow::Result<AnchorStatus> {
    // The handle proves WHICH group this is; record the name it carries as this store's
    // resolution, so a row whose resolution was reset — or whose authored name a stranger now
    // reuses — answers to the name of the target it actually points at (#1297).
    binding.set_resolved_binding_id(qualified_name);
    if let Some(chunk) = chunk_for_logical_symbol(conn, id)? {
        binding.symbol_id = chunk.symbol_id;
        binding.chunk_id = Some(chunk.chunk_id);
        binding.path = Some(chunk.path);
        binding.start_line = Some(chunk.start_line);
        binding.end_line = Some(chunk.end_line);
        return Ok(match source_hash_for_memory(conn, &binding.memory_id)? {
            Some(expected) if expected != chunk.text_hash => AnchorStatus::Stale,
            _ => AnchorStatus::Current,
        });
    }
    validate_bound_chunk(conn, binding)
}

pub(crate) fn validate_logical_symbol_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<AnchorStatus> {
    // A live handle is trusted — unless its writer marked the row retargeted and the handle's
    // target contradicts the recorded kind or signature (see `RelocationReason::Retargeted`); then
    // it is held back and the twins below are searched with the author's evidence first.
    let retargeted = is_retargeted(binding);
    let published_scope = if retargeted { published_scope_for(conn, binding)? } else { None };
    let mut held_back = None;
    if let Some(id) = binding.logical_symbol_id
        && let Some(hit) = crate::symbol::lookup_logical_by_id(conn, id)?
    {
        let trusted = !retargeted
            || target_agrees(
                binding,
                &hit.kind,
                logical_symbol_signature(conn, id)?.as_deref(),
                logical_symbol_scope(conn, id)?.as_deref(),
                published_scope.as_deref(),
            );
        if trusted {
            answer_retarget_mark(binding);
            return validate_live_logical_symbol(conn, binding, id, hit.qualified_name);
        }
        held_back = Some(id);
    }
    // Scope the qualified-name relocation to the ACTIVE repo. `logical_symbols` is direct-scoped by
    // `repo_id` (V040) and its ids are repo-distinct, so a consolidated DB can hold the SAME
    // qualified name under a sibling repo. Without the predicate, validating repo A's memory (whose
    // remembered symbol was deleted/renamed) could rebind it to repo B's logical id/path and report
    // `relocated` instead of `gone`/`stale`.
    let active_repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    // Land on the twin among those answering to `name` that the binding's discriminators pick
    // (#491: one qualified name can hold several live twins — a struct and its impl block,
    // overloads with distinct signatures — so a bare `LIMIT 1` is a plan-order coin flip).
    let land =
        |binding: &mut RepoMemoryBinding, name: &str| -> anyhow::Result<Option<AnchorStatus>> {
            let candidates = logical_twins_named(conn, name, &active_repo_id)?;
            let picked =
                pick_relocation_twin(candidates, binding, retargeted, published_scope.as_deref());
            let Some(RelocationTwin { id, path, kind, signature, scope, .. }) = picked else {
                return Ok(None);
            };
            // Back at the row the retarget check held back: nothing that matches the author's
            // evidence answers to the name. Validate it live rather than relocate onto
            // itself; the mark stays for a checkout that holds the author's target.
            if Some(id) == held_back {
                return validate_live_logical_symbol(conn, binding, id, name.to_string()).map(Some);
            }
            if target_agrees(
                binding,
                &kind,
                signature.as_deref(),
                scope.as_deref(),
                published_scope.as_deref(),
            ) {
                answer_retarget_mark(binding);
            }
            binding.set_resolved_binding_id(name.to_string());
            binding.logical_symbol_id = Some(id);
            binding.path = Some(path);
            if let Some(chunk) = chunk_for_logical_symbol(conn, id)? {
                binding.symbol_id = chunk.symbol_id;
                binding.chunk_id = Some(chunk.chunk_id);
                binding.start_line = Some(chunk.start_line);
                binding.end_line = Some(chunk.end_line);
            }
            Ok(Some(AnchorStatus::Relocated))
        };
    // The name this store last found the target under.
    let current = binding.current_binding_id().to_string();
    if let Some(status) = land(binding, &current)? {
        return Ok(status);
    }
    // Cross-file move: bare name + content hash fallback (same path as symbol binding).
    if let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? {
        let short = binding_leaf_name(&current, binding.path.as_deref()).to_string();
        if let Some(m) = relocate_symbol_by_name(conn, &short, &hash)? {
            // The resolution becomes the relocated member symbol's qualified_name, not a
            // logical_symbols.qualified_name. The stable logical_symbol_id arm re-matches on the
            // next reindex; if that ever goes stale this bare-name fallback recovers it — the
            // logical-qualified-name arm above intentionally won't re-match this binding again.
            binding.set_resolved_binding_id(m.binding_id);
            binding.logical_symbol_id = m.logical_symbol_id;
            binding.symbol_id = Some(m.symbol_id);
            binding.path = Some(m.path);
            binding.chunk_id = m.chunk_id;
            binding.start_line = m.start_line;
            binding.end_line = m.end_line;
            binding.symbol_kind = m.symbol_kind;
            binding.signature_hash = m.signature_hash;
            answer_retarget_mark(binding);
            return Ok(AnchorStatus::Relocated);
        }
    }
    if relocate_by_moniker(conn, binding)? {
        return Ok(AnchorStatus::Relocated);
    }
    // The authored name is identity, not evidence: once this store has resolved the row elsewhere
    // it is never searched again, since a stranger can reuse it while the target sits somewhere
    // the hash and the moniker could not place (#1297).
    Ok(AnchorStatus::Gone)
}

/// The live logical symbols of the active repo answering to `name`, each with its group's member
/// signature and scope (all members of a group share them, since both are part of the logical key).
fn logical_twins_named(
    conn: &Connection,
    name: &str,
    active_repo_id: &str,
) -> anyhow::Result<Vec<RelocationTwin>> {
    let mut stmt = conn.prepare(
        "
        SELECT ls.id, ls.path, ls.kind,
               (SELECT s.signature FROM logical_symbol_members m
                  JOIN symbols s ON s.id = m.symbol_id
                 WHERE m.logical_symbol_id = ls.id LIMIT 1),
               ls.id,
               (SELECT s.scope_path FROM logical_symbol_members m
                  JOIN symbols s ON s.id = m.symbol_id
                 WHERE m.logical_symbol_id = ls.id LIMIT 1)
        FROM logical_symbols ls
        WHERE ls.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
          AND ls.repo_id = ?2
        ORDER BY ls.id
        ",
    )?;
    let rows = stmt.query_map(params![name, active_repo_id], |row| {
        Ok(RelocationTwin {
            id: row.get(0)?,
            path: row.get(1)?,
            kind: row.get(2)?,
            signature: row.get(3)?,
            logical_symbol_id: row.get(4)?,
            scope: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Whether the binding carries [`RelocationReason::Retargeted`].
fn is_retargeted(binding: &RepoMemoryBinding) -> bool {
    binding.relocation_reason.as_deref() == Some(RelocationReason::Retargeted.as_db_str())
}

/// What a published anchor set said a symbol anchor's target is: the kind, the signature hash and
/// the scope hash its author recorded. The drain records these per anchor identity as the memory's
/// `anchors_applied_targets` — the baseline a later set is compared against to tell a retarget from
/// a republish of the same target — and the validator reads the scope back on a
/// [`RelocationReason::Retargeted`] row, where it is the author's evidence of which same-named twin
/// the row moved to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppliedTarget {
    pub symbol_kind: Option<String>,
    pub signature_hash: Option<String>,
    /// `hex_sha256` of the target's scope path as the AUTHOR's index derives it, from the
    /// `node_anchor_scopes` register; `None` when the author published none for this anchor.
    pub scope_hash: Option<String>,
}

/// The applied targets of one memory, keyed by anchor identity `(binding_kind, binding_id)`.
pub type AppliedTargets = std::collections::BTreeMap<(String, String), AppliedTarget>;

/// One serialized applied target: `(kind, id, symbol_kind, signature_hash, scope_hash)`.
type AppliedTargetRow<'a> = (&'a str, &'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>);

/// Serialize [`AppliedTargets`] for `repo_memories.anchors_applied_targets`: one
/// [`AppliedTargetRow`] per anchor.
pub fn encode_applied_targets(targets: &AppliedTargets) -> anyhow::Result<String> {
    let rows: Vec<AppliedTargetRow<'_>> = targets
        .iter()
        .map(|((kind, id), target)| {
            (
                kind.as_str(),
                id.as_str(),
                target.symbol_kind.as_deref(),
                target.signature_hash.as_deref(),
                target.scope_hash.as_deref(),
            )
        })
        .collect();
    Ok(serde_json::to_string(&rows)?)
}

/// Parse [`encode_applied_targets`]'s output, or the four-tuple rows written before the scope
/// existed; `None` when nothing was recorded or it does not parse, which leaves the retarget
/// decision to the row's own values.
pub fn decode_applied_targets(json: Option<&str>) -> Option<AppliedTargets> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Row {
        WithScope(String, String, Option<String>, Option<String>, Option<String>),
        Legacy(String, String, Option<String>, Option<String>),
    }
    let rows: Vec<Row> = serde_json::from_str(json?).ok()?;
    Some(
        rows.into_iter()
            .map(|row| match row {
                Row::WithScope(kind, id, symbol_kind, signature_hash, scope_hash) =>
                    ((kind, id), AppliedTarget { symbol_kind, signature_hash, scope_hash }),
                Row::Legacy(kind, id, symbol_kind, signature_hash) =>
                    ((kind, id), AppliedTarget { symbol_kind, signature_hash, scope_hash: None }),
            })
            .collect(),
    )
}

/// The scope the author published for a [`RelocationReason::Retargeted`] row's target, from the
/// memory's applied targets; `None` when none was recorded for this anchor.
fn published_scope_for(
    conn: &Connection,
    binding: &RepoMemoryBinding,
) -> anyhow::Result<Option<String>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT anchors_applied_targets FROM repo_memories WHERE id = ?1",
            [&binding.memory_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(decode_applied_targets(json.as_deref())
        .and_then(|targets| {
            targets.get(&(binding.binding_kind.clone(), binding.binding_id.clone())).cloned()
        })
        .and_then(|target| target.scope_hash))
}

/// Whether a live target's scope path agrees with the scope the author published: yes when either
/// side is unknown (the author published none, or the row predates the scope column), else by
/// hash.
fn scope_agrees(live_scope: Option<&str>, published_scope: Option<&str>) -> bool {
    match (live_scope.filter(|scope| !scope.is_empty()), published_scope) {
        (Some(live), Some(published)) => hex_sha256(live.as_bytes()) == published,
        _ => true,
    }
}

/// A symbol row's scope path; `None` for a row that predates the column, as for a missing row.
fn symbol_scope(conn: &Connection, id: i64) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT scope_path FROM symbols WHERE id = ?1", [id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

/// The scope path a logical symbol's members share (it is part of the logical key).
fn logical_symbol_scope(conn: &Connection, id: i64) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT s.scope_path FROM logical_symbol_members m
               JOIN symbols s ON s.id = m.symbol_id
              WHERE m.logical_symbol_id = ?1 LIMIT 1",
            [id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Clear [`RelocationReason::Retargeted`]: validation found a target agreeing with the author's
/// evidence.
fn answer_retarget_mark(binding: &mut RepoMemoryBinding) {
    if is_retargeted(binding) {
        binding.relocation_reason = None;
    }
}

/// Whether a symbol of `kind`, `signature` and `scope` agrees with everything the binding records
/// of its target — the evidence a retargeted row must be answered with. `published_scope` is the
/// author's, read from the memory's applied targets; the row itself records no scope.
fn target_agrees(
    binding: &RepoMemoryBinding,
    kind: &str,
    signature: Option<&str>,
    scope: Option<&str>,
    published_scope: Option<&str>,
) -> bool {
    binding.symbol_kind.as_deref().is_none_or(|bound| bound == kind)
        && binding_signature_agrees(binding, signature).unwrap_or(true)
        && scope_agrees(scope, published_scope)
}

/// Whether `signature` hashes to the binding's recorded signature hash; `None` when either side has
/// none to compare.
fn binding_signature_agrees(binding: &RepoMemoryBinding, signature: Option<&str>) -> Option<bool> {
    Some(binding.signature_hash.as_deref()? == hex_sha256(signature?.trim().as_bytes()))
}

/// The signature a logical symbol's members share (it is part of the logical key).
fn logical_symbol_signature(conn: &Connection, id: i64) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT s.signature FROM logical_symbol_members m
               JOIN symbols s ON s.id = m.symbol_id
              WHERE m.logical_symbol_id = ?1 LIMIT 1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

/// A live logical-symbol row sharing the dead binding's qualified name — a relocation candidate.
struct RelocationTwin {
    id: i64,
    path: String,
    kind: String,
    signature: Option<String>,
    /// The logical group this twin belongs to — for a logical-symbol candidate, itself.
    logical_symbol_id: Option<i64>,
    /// Its scope path — for a logical-symbol candidate, the one its members share.
    scope: Option<String>,
}

/// Choose which same-qualified-name twin a gone binding relocates onto (#491): the stored
/// discriminators win, and equal evidence falls back to the lowest id, so the pick is deterministic
/// instead of plan-order.
///
/// The logical handle outranks both shape axes because it is derived from the whole logical key,
/// `scope_path` included, which makes it the only stored evidence that separates two impls of
/// different traits for one type: an impl symbol is named for its self type, so `impl Alpha for W`
/// and `impl Beta for W` agree on the qualified name, agree on `impl`, and — with the trait on a
/// later line, as a formatter produces for long bounds — agree on the captured signature too. Kind
/// then outranks signature, since a rename usually keeps the kind but changes the signature text.
/// `None` evidence on the binding degrades gracefully: every candidate scores equally on the axes
/// it can't speak to, and the id tiebreak decides, matching the behavior for bindings that predate
/// the V014 discriminators.
///
/// On a row its writer marked [`RelocationReason::Retargeted`] the recorded kind and signature
/// outrank the handle instead: the writer set them, while the handle can be the one it left (a
/// rebind from a struct to its impl keeps the struct's handle beside the impl's kind). The author's
/// published scope (`published_scope`) ranks next, below both: it separates twins that agree on
/// kind and signature — two impls of different traits for one type — and never overrides the
/// evidence those already give. The handle still decides where all three tie — including evidence
/// no candidate matches, which names nothing here — so a held-back handle keeps its row over an
/// equal twin.
fn pick_relocation_twin(
    candidates: Vec<RelocationTwin>,
    binding: &RepoMemoryBinding,
    retargeted: bool,
    published_scope: Option<&str>,
) -> Option<RelocationTwin> {
    candidates
        .into_iter()
        .map(|twin| {
            let kind_agrees = binding.symbol_kind.as_deref() == Some(twin.kind.as_str());
            let group_agrees = matches!(
                (binding.logical_symbol_id, twin.logical_symbol_id),
                (Some(bound), Some(group)) if bound == group
            );
            let signature_agrees =
                binding_signature_agrees(binding, twin.signature.as_deref()).unwrap_or(false);
            let scope_matches = published_scope.is_some_and(|published| {
                twin.scope.as_deref().is_some_and(|scope| hex_sha256(scope.as_bytes()) == published)
            });
            // Candidates arrive id-ascending; max_by_key keeps the LAST maximum, so compare on
            // (score, negated id) to keep the lowest-id winner among evidence ties.
            let [kind, signature, group, scope] =
                [kind_agrees, signature_agrees, group_agrees, scope_matches].map(u8::from);
            let score = if retargeted {
                (kind << 3) | (signature << 2) | (scope << 1) | group
            } else {
                (group << 2) | (kind << 1) | signature
            };
            (score, -twin.id, twin)
        })
        .max_by_key(|(score, neg_id, _)| (*score, *neg_id))
        .map(|(_, _, twin)| twin)
}

/// Validate a binding against the live symbol row `id` it resolves to, named `qualified_name`.
fn validate_live_symbol(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
    id: i64,
    qualified_name: String,
) -> anyhow::Result<AnchorStatus> {
    // The row id proves WHICH symbol this is; the name is only what every later relocation
    // searches by. A rename in place — an index upgrade re-deriving an impl's identity moves the
    // row's name from the trait to the type — leaves the two disagreeing, and nothing notices
    // until the next reindex churns the row id. Relocation then looks up a name that no longer
    // belongs to this symbol and attaches the memory to whatever else answers to it, or calls it
    // gone. Record the live name as this store's resolution while the id still vouches for it;
    // the authored name stays what the author bound (#1297).
    binding.set_resolved_binding_id(qualified_name);
    // The row can also be REGROUPED without moving: a key-version rebuild mints a new logical id
    // for the same impl. The raw id still proves the binding's identity, so re-read the handle
    // here — leaving the vanished one in place reports `current` forever while every
    // logical-id-keyed surface stays disconnected from the symbol.
    binding.logical_symbol_id = logical_symbol_id_for_symbol(conn, id)?;
    validate_bound_chunk(conn, binding)
}

pub(crate) fn validate_symbol_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<AnchorStatus> {
    let retargeted = is_retargeted(binding);
    let published_scope = if retargeted { published_scope_for(conn, binding)? } else { None };
    let mut held_back = None;
    if let Some(id) = binding.symbol_id
        && let Some(hit) = crate::symbol::lookup_by_id(conn, id)?
    {
        let trusted = !retargeted
            || target_agrees(
                binding,
                &hit.kind,
                hit.signature.as_deref(),
                symbol_scope(conn, id)?.as_deref(),
                published_scope.as_deref(),
            );
        if trusted {
            answer_retarget_mark(binding);
            return validate_live_symbol(conn, binding, id, hit.qualified_name);
        }
        held_back = Some(id);
    }
    // One qualified name can hold several live rows: `{path}::{name}` is shared by a `struct
    // Worker` and its `impl Worker` block, since an impl symbol is named for its self type. A
    // `LIMIT 1` here is a plan-order coin flip between them, so the binding's own discriminators
    // pick — the same rule, and the same helper, the logical-symbol path uses. Each candidate
    // carries its logical group, keyed on its own row id so the correlated read adds no scope
    // surface, because the group is what separates two impls of different traits for one type.
    let land = |binding: &mut RepoMemoryBinding,
                name: &str|
     -> anyhow::Result<Option<AnchorStatus>> {
        let candidates = symbol_twins_named(conn, name)?;
        let picked =
            pick_relocation_twin(candidates, binding, retargeted, published_scope.as_deref());
        let Some(RelocationTwin { id, path, kind, signature, scope, .. }) = picked else {
            return Ok(None);
        };
        // Back at the row the retarget check held back: nothing that matches the author's evidence
        // answers to the name here. Validate it live rather than relocate onto itself; the mark
        // stays for a checkout that holds the author's target.
        if Some(id) == held_back {
            return validate_live_symbol(conn, binding, id, name.to_string()).map(Some);
        }
        if target_agrees(
            binding,
            &kind,
            signature.as_deref(),
            scope.as_deref(),
            published_scope.as_deref(),
        ) {
            answer_retarget_mark(binding);
        }
        binding.set_resolved_binding_id(name.to_string());
        binding.symbol_id = Some(id);
        binding.logical_symbol_id = logical_symbol_id_for_symbol(conn, id)?;
        binding.path = Some(path);
        if let Some(chunk) = chunk_for_symbol(conn, id, name)? {
            binding.chunk_id = Some(chunk.chunk_id);
            binding.start_line = Some(chunk.start_line);
            binding.end_line = Some(chunk.end_line);
        }
        // The relocated target's kind and signature become the recorded ones, so the next pick —
        // after the handle dies with a later edit — credits the target, not a same-named sibling
        // that keeps the old kind or signature. An unanswered retarget keeps the author's: they are
        // what a checkout holding the author's target must answer.
        if !is_retargeted(binding) {
            let (kind, sig) = symbol_signal(conn, id)?;
            binding.symbol_kind = kind;
            binding.signature_hash = sig;
        }
        Ok(Some(AnchorStatus::Relocated))
    };
    // The name this store last found the target under.
    let current = binding.current_binding_id().to_string();
    if let Some(status) = land(binding, &current)? {
        return Ok(status);
    }
    // Cross-file move: qualified_name changed with the path. Match by bare name + content hash.
    if let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? {
        let short = binding_leaf_name(&current, binding.path.as_deref()).to_string();
        if let Some(m) = relocate_symbol_by_name(conn, &short, &hash)? {
            binding.set_resolved_binding_id(m.binding_id);
            binding.symbol_id = Some(m.symbol_id);
            binding.logical_symbol_id = m.logical_symbol_id;
            binding.path = Some(m.path);
            binding.chunk_id = m.chunk_id;
            binding.start_line = m.start_line;
            binding.end_line = m.end_line;
            binding.symbol_kind = m.symbol_kind;
            binding.signature_hash = m.signature_hash;
            answer_retarget_mark(binding);
            return Ok(AnchorStatus::Relocated);
        }
    }
    if relocate_by_moniker(conn, binding)? {
        return Ok(AnchorStatus::Relocated);
    }
    // The authored name is identity, not evidence: once this store has resolved the row elsewhere
    // it is never searched again, since a stranger can reuse it while the target sits somewhere
    // the hash and the moniker could not place (#1297).
    Ok(AnchorStatus::Gone)
}

/// The live symbol rows answering to `name`, each with its logical group, kind, signature and
/// scope — the discriminators the pick weighs.
fn symbol_twins_named(conn: &Connection, name: &str) -> anyhow::Result<Vec<RelocationTwin>> {
    let mut stmt = conn.prepare(
        "
        SELECT symbols.id, files.path, symbols.kind, symbols.signature,
               (SELECT m.logical_symbol_id FROM logical_symbol_members m
                 WHERE m.symbol_id = symbols.id LIMIT 1),
               symbols.scope_path
        FROM symbols
        JOIN files ON files.id = symbols.file_id
        WHERE symbols.qualified_name_id = (SELECT id FROM name_strings WHERE value = ?1)
        ORDER BY symbols.id
        ",
    )?;
    let rows = stmt.query_map([name], |row| {
        Ok(RelocationTwin {
            id: row.get(0)?,
            path: row.get(1)?,
            kind: row.get(2)?,
            signature: row.get(3)?,
            logical_symbol_id: row.get(4)?,
            scope: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Relocation for a symbol/logical_symbol binding whose qualified-name and name+content-hash
/// anchors are exhausted: re-resolve the memory's recorded SCIP moniker against current oracle
/// data (#70). A unique live match — semantic identity, robust to content edits the hash fallback
/// can't survive — relocates with `relocation_reason = "moniker-match"`. Returns whether it did;
/// the authored-name fallback and `gone` come after it, since a reused name is weaker evidence
/// than a retained semantic identity.
fn relocate_by_moniker(conn: &Connection, binding: &mut RepoMemoryBinding) -> anyhow::Result<bool> {
    if let Some(m) = relocate_binding_by_moniker(conn, binding)? {
        binding.set_resolved_binding_id(m.binding_id);
        binding.symbol_id = Some(m.symbol_id);
        binding.logical_symbol_id = m.logical_symbol_id;
        binding.path = Some(m.path);
        binding.chunk_id = m.chunk_id;
        binding.start_line = m.start_line;
        binding.end_line = m.end_line;
        binding.symbol_kind = m.symbol_kind;
        binding.signature_hash = m.signature_hash;
        binding.relocation_reason = Some(RelocationReason::MonikerMatch.as_db_str().to_string());
        return Ok(true);
    }
    Ok(false)
}
pub(crate) fn validate_chunk_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<AnchorStatus> {
    let status = validate_bound_chunk(conn, binding)?;
    if status != AnchorStatus::Gone {
        return Ok(status);
    }
    let Some(hash) = source_hash_for_memory(conn, &binding.memory_id)? else {
        return Ok(AnchorStatus::Gone);
    };
    let Some(chunk) = relocate_chunk_by_hash(conn, &hash)? else {
        return Ok(AnchorStatus::Gone);
    };
    binding.set_resolved_binding_id(chunk.chunk_id.to_string());
    binding.chunk_id = Some(chunk.chunk_id);
    binding.path = Some(chunk.path);
    binding.start_line = Some(chunk.start_line);
    binding.end_line = Some(chunk.end_line);
    Ok(AnchorStatus::Relocated)
}
pub(crate) fn validate_edge_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<AnchorStatus> {
    // The row id alone is not identity. A graph-version rebuild DELETEs and re-INSERTs every
    // edge, and SQLite reuses freed rowids, so a stored `edge_id` can come back pointing at a
    // DIFFERENT call site — and this fast path would then report the binding `current` on the
    // strength of a file hash that has nothing to do with it. Take the shortcut only when the row
    // still hashes to the identity the binding was made against.
    //
    // The CURRENT fingerprint only. A pre-upgrade digest matching proves that the eight older
    // fields agree, not that the call still means what it did: the receiver-type hint is not among
    // them, so a call whose inferred receiver changed which method it reaches carries the same
    // legacy digest. That binding has to fall through to relocation, which says `relocated` — the
    // honest answer — instead of `current`.
    if let Some(edge_id) = binding.edge_id
        && let Some(edge) = edge_by_id(conn, edge_id)?
        && edge.fingerprint == binding.current_binding_id()
    {
        binding.path = Some(edge.path);
        binding.start_line = Some(edge.start_line);
        binding.end_line = Some(edge.end_line);
        binding.symbol_id = None;
        binding.logical_symbol_id = None;
        return validate_bound_edge_source_hash(conn, binding, &edge.source_hash);
    }
    // The fingerprint this store last resolved to — and only that one. The authored fingerprint is
    // not retried once a resolution exists: a pre-versioned digest omits the identity fields, so
    // after a callee or receiver change it would match the replacement callee and reattach the
    // memory there, where the versioned identity it converged to says the edge is gone (#1297).
    let Some(edge) = edge_by_fingerprint(conn, binding.current_binding_id())? else {
        // A row id may survive an in-place re-resolution or be reused after rebuild. Once the
        // stable fingerprint no longer exists, retaining that id would surface this memory on the
        // replacement edge through edge-id lookups.
        binding.edge_id = None;
        return Ok(AnchorStatus::Gone);
    };
    // Compatibility matches return the live current fingerprint. Converge the resolution so the
    // next validation takes the current fast path instead of reporting `relocated` forever.
    binding.set_resolved_binding_id(edge.fingerprint.clone());
    binding.edge_id = Some(edge.edge_id);
    binding.path = Some(edge.path);
    binding.start_line = Some(edge.start_line);
    binding.end_line = Some(edge.end_line);
    binding.symbol_id = None;
    binding.logical_symbol_id = None;
    Ok(AnchorStatus::Relocated)
}
pub(crate) fn validate_call_path_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<AnchorStatus> {
    // The row's resolution as the table holds it NOW, not as this pass hydrated it: converging a
    // sibling binding of the same memory re-points every binding resolving to the old hash and
    // moves the local rows with them, and a binding hydrated before that would otherwise look the
    // rows up under the old hash and stamp that hash back over the re-point.
    let current: Option<String> = conn
        .query_row(
            &format!(
                "SELECT {BINDING_CURRENT_BINDING_ID} FROM repo_memory_bindings
                 WHERE memory_id = ?1 AND binding_kind = 'call_path' AND binding_id = ?2
                   AND repo_id = (SELECT repo_id FROM repo_memories WHERE id = ?1)"
            ),
            params![binding.memory_id, binding.binding_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(current) = current {
        binding.set_resolved_binding_id(current);
    }
    // Re-check each stored edge behind the server-derived hash (#38). Exact-fingerprint match →
    // the edge is unchanged; loose name/kind/target match → it moved lines (relocated); neither →
    // that edge is gone.
    let mut stmt = conn.prepare(
        "
        SELECT ordinal, edge_fingerprint, from_name, to_name, edge_kind, target_qualified_name,
               callee_logical_symbol_id, callee_identity_known
        FROM repo_memory_call_path_edges
        WHERE memory_id = ?1 AND edge_sequence_hash = ?2
        ORDER BY ordinal
        ",
    )?;
    let edges = stmt
        .query_map(params![binding.memory_id, binding.current_binding_id()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, i64>(7)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if edges.is_empty() {
        // Legacy client-supplied hash with no stored edges — honest-but-weak: current only as a
        // row, never verifiable against the graph.
        let exists = conn.query_row(
            "SELECT COUNT(*) FROM repo_memory_call_paths
             WHERE memory_id = ?1 AND edge_sequence_hash = ?2",
            params![binding.memory_id, binding.current_binding_id()],
            |row| row.get::<_, i64>(0),
        )?;
        return Ok(if exists > 0 { AnchorStatus::Unverified } else { AnchorStatus::Gone });
    }

    let total = edges.len();
    let mut relocated = 0usize;
    let mut gone = 0usize;
    // The live v2 identity of every edge that matched one, in `ordinal` order. Complete ⇔ the
    // whole path still resolves edge-for-edge, which is the only state the upgrade below may act
    // on: an edge that matched only its loose identity has no live fingerprint to converge to.
    let mut live_fingerprints = Vec::with_capacity(total);
    let mut matched_legacy = false;
    for (
        ordinal,
        fingerprint,
        from_name,
        to_name,
        edge_kind,
        target,
        callee_logical_symbol_id,
        callee_identity_known,
    ) in &edges
    {
        if let Some(edge) = edge_by_fingerprint(conn, fingerprint)? {
            if edge.matched_legacy_fingerprint {
                // The v1 identity has no receiver type. It proves the call site survived, but not
                // that receiver-aware resolution still targets the same owner, so this validation
                // reports relocated — once. The convergence below then rewrites the stored
                // identity so later validations compare the full receiver-aware fingerprint.
                relocated += 1;
                matched_legacy = true;
            }
            live_fingerprints.push((*ordinal, edge.fingerprint, edge.callee_logical_symbol_id));
            continue;
        }
        let loose_identity = EdgeLooseIdentity {
            from_name: from_name.clone(),
            to_name: to_name.clone().unwrap_or_default(),
            edge_kind: edge_kind.clone(),
            target_qualified_name: target.clone(),
            callee_logical_symbol_id: *callee_logical_symbol_id,
        };
        if call_path_edge_relocatable(conn, &loose_identity, *callee_identity_known)? {
            relocated += 1;
        } else {
            gone += 1;
        }
    }

    if matched_legacy && live_fingerprints.len() == total {
        converge_call_path_identity(conn, binding, &live_fingerprints)?;
    }

    Ok(if gone == total {
        AnchorStatus::Gone
    } else if gone > 0 {
        AnchorStatus::Stale
    } else if relocated > 0 {
        AnchorStatus::Relocated
    } else {
        AnchorStatus::Current
    })
}

/// Migrate one pre-versioned call-path binding onto the current edge identity, in full.
///
/// A binding is keyed by `edge_sequence_hash` — the hash OF its ordered edge fingerprints — so
/// rewriting the member fingerprints without rewriting the key would leave a row that no longer
/// re-derives its own id, and `call_path_memories_for_crossed` (which looks memories up by the
/// hash it computes from LIVE fingerprints) would keep missing it: the memory would validate
/// `current` yet never surface on the traversal it was recorded for. So both move together, and
/// the binding's RESOLUTION is re-pointed as well — `stamp_validated_binding` writes it back, the
/// same mechanism a relocated symbol binding uses. The authored hash is the author's and stays
/// (#1297).
///
/// Runs only when every edge of the path matched a live edge, so the recomputed hash describes
/// the same call path the binding already named.
fn converge_call_path_identity(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
    live_fingerprints: &[(i64, String, Option<i64>)],
) -> anyhow::Result<()> {
    let converged =
        compute_edge_sequence_hash(live_fingerprints.iter().map(|(_, value, _)| value.as_str()));
    let current = binding.current_binding_id().to_string();
    if converged == current {
        return Ok(());
    }
    // The local rows under `current` are shared by every binding of the memory resolving to it —
    // this one and any sibling authored under another hash — so all of them move together, ahead
    // of the rows (`stamp_validated_binding` writes this one again, identically).
    conn.execute(
        &format!(
            "UPDATE repo_memory_bindings
                SET resolved_binding_id = ?1, {BINDING_RESOLUTION_CARRY_SQL}
              WHERE memory_id = ?2 AND binding_kind = 'call_path'
                AND {BINDING_CURRENT_BINDING_ID} = ?3
                AND repo_id = (SELECT repo_id FROM repo_memories WHERE id = ?2)"
        ),
        params![converged, binding.memory_id, current],
    )?;
    // Both local tables are keyed `(memory_id, edge_sequence_hash)`. When the memory already
    // carries the converged hash — another authored binding of the same path re-derived here
    // first — the rows under `current` go: the rows under the converged hash describe the same
    // path, and two sets would be two competing derivations of one thing. That holds only when
    // the destination carries an EDGE sequence: a client-supplied hash creates a parent without
    // edges, and dropping this row's verified edges for that would leave both bindings
    // unverifiable, so those edges move under the converged hash instead and only the parent
    // yields.
    let (taken_parent, taken_edges): (i64, i64) = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM repo_memory_call_paths
                  WHERE memory_id = ?1 AND edge_sequence_hash = ?2),
                (SELECT COUNT(*) FROM repo_memory_call_path_edges
                  WHERE memory_id = ?1 AND edge_sequence_hash = ?2)",
        params![binding.memory_id, converged],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if taken_parent > 0 && taken_edges > 0 {
        for table in ["repo_memory_call_path_edges", "repo_memory_call_paths"] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE memory_id = ?1 AND edge_sequence_hash = ?2"),
                params![binding.memory_id, current],
            )?;
        }
        binding.set_resolved_binding_id(converged);
        return Ok(());
    }
    for (ordinal, fingerprint, callee_logical_symbol_id) in live_fingerprints {
        conn.execute(
            "UPDATE repo_memory_call_path_edges
             SET edge_fingerprint = ?1, edge_sequence_hash = ?2,
                 callee_logical_symbol_id = ?3, callee_identity_known = 1
             WHERE memory_id = ?4 AND edge_sequence_hash = ?5 AND ordinal = ?6",
            params![
                fingerprint,
                converged,
                callee_logical_symbol_id,
                binding.memory_id,
                current,
                ordinal
            ],
        )?;
    }
    if taken_parent > 0 {
        conn.execute(
            "DELETE FROM repo_memory_call_paths WHERE memory_id = ?1 AND edge_sequence_hash = ?2",
            params![binding.memory_id, current],
        )?;
    } else {
        conn.execute(
            "UPDATE repo_memory_call_paths
             SET edge_sequence_hash = ?1
             WHERE memory_id = ?2 AND edge_sequence_hash = ?3",
            params![converged, binding.memory_id, current],
        )?;
    }
    binding.set_resolved_binding_id(converged);
    Ok(())
}

/// Is there still an edge matching this one's loose identity (names/kind/target), ignoring line
/// numbers? Used to call a call-path edge `relocated` (moved) rather than `gone` (#38). The
/// `JOIN files` is load-bearing (A6): `edges`/`edges_data` are NOT generation-scoped, so without it
/// a superseded generation's edge rows (dead until gc) would keep matching — a genuinely-deleted
/// call site would be reported `relocated` forever instead of `gone`. The join drops
/// dead-generation edges (their `source_file_id` file row is absent from the live scope view) and
/// scopes to the active repo for free, matching the sibling helpers `edge_by_fingerprint` /
/// `call_path_edge_by_id`.
pub(crate) fn call_path_edge_relocatable(
    conn: &Connection,
    identity: &EdgeLooseIdentity,
    callee_identity_known: bool,
) -> anyhow::Result<bool> {
    if !callee_identity_known {
        return Ok(false);
    }
    let count: i64 = conn.query_row(
        "
        SELECT COUNT(*)
        FROM edges
        JOIN files ON files.id = edges.source_file_id
        WHERE edge_kind = ?3
          AND COALESCE(from_name, '') = COALESCE(?1, '')
          AND COALESCE(to_name, '') = COALESCE(?2, '')
          AND COALESCE(target_qualified_name, '') = COALESCE(?4, '')
          AND ((?5 IS NULL AND to_symbol_id IS NULL) OR EXISTS(
              SELECT 1 FROM logical_symbol_members members
              WHERE members.symbol_id = edges.to_symbol_id AND members.logical_symbol_id = ?5
          ))
        ",
        params![
            identity.from_name,
            identity.to_name,
            identity.edge_kind,
            identity.target_qualified_name,
            identity.callee_logical_symbol_id
        ],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}
pub(crate) fn validate_bound_edge_source_hash(
    conn: &Connection,
    binding: &RepoMemoryBinding,
    current_source_hash: &str,
) -> anyhow::Result<AnchorStatus> {
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != current_source_hash => Ok(AnchorStatus::Stale),
        _ => Ok(AnchorStatus::Current),
    }
}
pub(crate) fn validate_bound_chunk(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
) -> anyhow::Result<AnchorStatus> {
    let Some(chunk_id) = binding.chunk_id else {
        return Ok(AnchorStatus::Unverified);
    };
    let Some(chunk) = chunk_by_id(conn, chunk_id)? else {
        return Ok(AnchorStatus::Gone);
    };
    // A chunk binding's name IS its chunk id: record the live one as this store's resolution, so
    // a row whose resolution was reset answers to the chunk it points at, not to the authored id
    // (#1297). A symbol binding validated through its chunk keeps its qualified name.
    if binding.binding_kind == BindingKind::Chunk.as_db_str() {
        binding.set_resolved_binding_id(chunk_id.to_string());
    }
    binding.path = Some(chunk.path);
    binding.start_line = Some(chunk.start_line);
    binding.end_line = Some(chunk.end_line);
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != chunk.text_hash => Ok(AnchorStatus::Stale),
        _ => Ok(AnchorStatus::Current),
    }
}
pub(crate) fn validate_path_binding(
    conn: &Connection,
    binding: &mut RepoMemoryBinding,
    fs_root: Option<&Path>,
) -> anyhow::Result<AnchorStatus> {
    let Some(path) = binding.path.as_deref() else {
        return Ok(AnchorStatus::Unverified);
    };
    // `kind != 'deleted'` (#492): a deleted-at-HEAD file leaves a marker row (kind='deleted',
    // sha256='') that would otherwise be the newest row for the path and shadow the absence —
    // the binding stayed `current` forever behind it.
    let current_hash = conn
        .query_row(
            &format!(
                "SELECT sha256 FROM files WHERE path = ?1 AND kind != '{TOMBSTONE_FILE_KIND}'
             ORDER BY id DESC LIMIT 1"
            ),
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
        let status = if path_is_file_on_disk(fs_root, path) {
            if binding.start_line.is_none() && binding.end_line.is_none() {
                AnchorStatus::Current
            } else {
                AnchorStatus::Unverified
            }
        } else if path_is_live_in_another_scope(conn, path)? {
            // Neither indexed here nor on disk — but ALIVE in another indexed scope (a
            // linked-worktree overlay: an in-flight branch, #492): the anchor is `pending`,
            // not gone. Verified live: forward anchors to branch-only files ping-ponged
            // current/gone between checkout contexts, and doctor advised mark-obsolete for
            // valid in-flight work. Only when NO scope holds the path is it genuinely gone.
            AnchorStatus::Pending
        } else {
            AnchorStatus::Gone
        };
        return Ok(status);
    };
    // A BARE path binding (no line span) is an AREA anchor, like a `dir` binding: the claim is
    // "this note is about this file", not "this file's bytes are X" — so it is current while the
    // file is indexed, never content-stale. Hashing the whole file made every commit stale every
    // area-level note bound to a touched file, permanently (nothing refreshes the hash), which
    // buried the real staleness signals under noise. Only a SPANNED `path:start-end` binding
    // claims specific content and keeps the content-hash check.
    if binding.start_line.is_none() && binding.end_line.is_none() {
        return Ok(AnchorStatus::Current);
    }
    match source_hash_for_memory(conn, &binding.memory_id)? {
        Some(expected) if expected != current_hash => Ok(AnchorStatus::Stale),
        _ => Ok(AnchorStatus::Current),
    }
}
/// The persisted `source_root` (the on-disk repo root recorded in `repo_meta` at
/// open/rebuild/incremental). `None` on a raw connection that never recorded it (some test
/// fixtures). This is a SINGLE shared value — under a shared DB across git worktrees it reflects
/// whichever worktree last indexed, which is why [`validate_memories`] prefers the caller-supplied
/// active checkout root and only falls back to this (#98 review).
fn persisted_source_root(conn: &Connection) -> Option<PathBuf> {
    // `source_root` moved to `repo_meta` (V039); resolve the active repo (the lone one in phase A).
    let repo_id = rag_rat_db::schema::active_repo_id(conn).ok()?;
    rag_rat_db::meta::repo_meta(conn, &repo_id, "source_root").ok().flatten().map(PathBuf::from)
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

/// Whether ANOTHER CHECKOUT's overlay holds a live row for `path` — the `pending` probe (#492).
/// The scoped `files` view already answered "not in THIS context"; a linked-worktree overlay row
/// (an in-flight branch's checkout) means the anchor's target exists and will re-anchor when the
/// branch lands, so it must not be treated (or remediated) as dead. Three predicates keep the
/// probe honest (each closes a false-pending source found in review):
/// - repo-scoped: a sibling repo's identical path cannot bless a dead anchor on a consolidated DB;
/// - LIVE-generation only: a superseded full-rebuild generation's not-yet-GC'd rows are not
///   alive-elsewhere evidence;
/// - OTHER-worktree only (`worktree_id != ''` and != the active context's): retained old-commit
///   base rows (worktree_id = '') and the active checkout's own dirty-scope rows are THIS context,
///   not another one — a path deleted at HEAD must stay `gone` through the pre-gc window.
fn path_is_live_in_another_scope(conn: &Connection, path: &str) -> anyhow::Result<bool> {
    let active_repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let live_generation = rag_rat_db::schema::live_files_generation(conn, &active_repo_id)?;
    let active_worktree = context_worktree_id(conn);
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM main.files
                       WHERE path = ?1 AND kind != '{TOMBSTONE_FILE_KIND}' AND repo_id = ?2
                         AND generation = ?3
                         AND worktree_id != '' AND worktree_id != ?4)"
        ),
        params![path, active_repo_id, live_generation, active_worktree],
        |r| r.get::<_, i64>(0),
    )? != 0)
}

/// The active context's worktree id, `''` when no scope view is installed on this connection.
fn context_worktree_id(conn: &Connection) -> String {
    rag_rat_db::schema::connection_context_value(
        conn,
        rag_rat_db::schema::CONNECTION_CONTEXT_WORKTREE_KEY,
    )
    .unwrap_or_default()
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
pub fn validate_kind(kind: &str) -> anyhow::Result<()> {
    MemoryKind::from_db_str(kind).map(|_| ())
}
/// [`MemoryKind::is_polymorphic_node`] for a kind token as stored or submitted; a token outside the
/// closed set is not a node kind.
pub fn is_polymorphic_node_kind(kind: &str) -> bool {
    MemoryKind::from_db_str(kind).is_ok_and(MemoryKind::is_polymorphic_node)
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
pub fn validate_payload(kind: &str, payload_json: Option<&str>) -> anyhow::Result<()> {
    let Some(payload) = payload_json else {
        return Ok(());
    };
    if !is_polymorphic_node_kind(kind) {
        anyhow::bail!(
            "a `{kind}` memory carries no payload (only Task/Concept may have a payload_json)"
        );
    }
    // Byte cap at the write boundary (#680): the payload is the only otherwise-uncapped envelope
    // input, so an oversized one is how a memory whose signed `/3` envelope exceeds the op-log's
    // §18a cap gets created — an un-authorable row that would have to be quarantined. Reject it
    // here so the normal API can never mint one. Checked before parsing, so a huge blob is
    // rejected cheaply.
    if payload.len() > MAX_MEMORY_PAYLOAD_LEN {
        anyhow::bail!(
            "payload_json is {} bytes, over the {MAX_MEMORY_PAYLOAD_LEN}-byte cap",
            payload.len()
        );
    }
    // Strict parse: reject a LITERAL duplicate object key (serde_json's default silently keeps the
    // last, but parsers disagree on which wins → a cross-device hash divergence). Complete for a
    // caller that passes the RAW payload string (CLI, direct core); the MCP JSON-RPC transport
    // parses tool args into a `Value` upstream, collapsing dups deterministically before this runs
    // (harmless among serde_json writers today) — hardening that boundary is #488.
    let value = rag_rat_base::canonical::parse_rejecting_duplicate_keys(payload)
        .map_err(|e| anyhow::anyhow!("payload_json invalid: {e}"))?;
    if !value.is_object() {
        anyhow::bail!("payload_json must be a JSON object");
    }
    // Reject on WRITE anything the canonical encoder can't hash (two keys that NFC-normalize to the
    // same key → an ambiguous dup-key map), so a STORED payload always encodes cleanly and
    // `content_hash` can treat the canonical encoding as effectively infallible.
    if let Some(err) = rag_rat_base::canonical::payload_encoding_error(&value) {
        anyhow::bail!("payload_json is not canonically encodable: {err}");
    }
    Ok(())
}

pub fn validate_confidence(confidence: &str) -> anyhow::Result<()> {
    MemoryConfidence::from_db_str(confidence).map(|_| ())
}
pub fn validate_status(status: &str) -> anyhow::Result<()> {
    MemoryStatus::from_db_str(status).map(|_| ())
}
pub fn validate_source(source: &str) -> anyhow::Result<()> {
    MemorySource::from_db_str(source).map(|_| ())
}
pub fn validate_len(field: &str, value: &str, max: usize) -> anyhow::Result<()> {
    let len = value.trim().chars().count();
    if len == 0 {
        anyhow::bail!("memory {field} must not be empty");
    }
    if len > max {
        anyhow::bail!("memory {field} exceeds {max} characters");
    }
    Ok(())
}
/// Reject an oversized free-form edge string at the write boundary (#680) — the edge twin of
/// [`validate_payload`]'s byte cap. A `target_anchor` / `target_repo_id` is normally a short
/// identifier, but an explicit cross-repo (or github) target stores the caller's raw string
/// verbatim and it is signed verbatim into the `EdgeAdd` op; an oversized one is how an
/// un-authorable edge (a signed `/3` envelope over the §18a cap) would otherwise be minted. `field`
/// names the offending input for the error. Bytes, not chars, because the envelope budget is a byte
/// budget.
pub fn validate_edge_len(field: &str, value: &str) -> anyhow::Result<()> {
    if value.len() > MAX_EDGE_ANCHOR_LEN {
        anyhow::bail!(
            "edge {field} is {} bytes, over the {MAX_EDGE_ANCHOR_LEN}-byte cap",
            value.len()
        );
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
pub fn memory_id(now: i64, input_hash: &str, scope: &Option<String>) -> String {
    let suffix = match scope {
        Some(repo_id) => hex_sha256(format!("{repo_id}\u{1f}{input_hash}").as_bytes())
            .chars()
            .take(12)
            .collect::<String>(),
        None => input_hash.chars().take(12).collect::<String>(),
    };
    format!("mem_{now:x}_{suffix}")
}
pub fn memory_input_hash(
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
    use rag_rat_base::canonical::nfc;
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
    use rag_rat_base::canonical::{encode_canonical_json, parse_rejecting_duplicate_keys};
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

pub(crate) fn fts_query(query: &str) -> String {
    let terms = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    terms.join(" OR ")
}

#[cfg(test)]
#[path = "validate/content_hash_tests.rs"]
mod content_hash_tests;

#[cfg(test)]
#[path = "validate/call_path_receiver_type_hint_tests.rs"]
mod call_path_receiver_type_hint_tests;
