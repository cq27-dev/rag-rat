use rusqlite::{Connection, OptionalExtension, params};

use super::{
    CARRIED_META_KEYS, MEMORY_STREAM_ACCESS_MODE_META_KEY, MEMORY_STREAM_PIN_IMPORTED_META_KEY,
    MEMORY_STREAM_PIN_META_KEY, MEMORY_STREAM_SEAL_POLICY_META_KEY,
    MEMORY_SUBSCRIPTION_OWNER_META_KEY,
};
use crate::index::schema;

/// Copy `embedding_cache` — content-addressed by `input_hash` (which folds model + version + input
/// text), so `INSERT OR IGNORE` is a conflict-free union — the ONE copy the mirror invariant
/// exempts, because rows are CONTENT-ADDRESSED: `(input_hash, model_id)` determines the vector
/// bytes, an existing row is by definition identical (same content) and an extra unreferenced row
/// is harmless cache. A vector already present (same content)
/// is kept, a new one is added (the full 6-column table shape). This is the durable unit that
/// makes re-embedding the consolidated repo a no-op. It is a GLOBAL/shared table (no `repo_id`).
pub(super) fn copy_embedding_cache(source: &Connection, tx: &Connection) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "embedding_cache")? {
        return Ok(0);
    }
    let mut stmt = source.prepare(
        "SELECT input_hash, model_id, embedding_dim, vector_blob, computed_at_ms, last_used_at_ms
         FROM embedding_cache",
    )?;
    let mut rows = stmt.query([])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let changed = tx.execute(
            "INSERT OR IGNORE INTO embedding_cache(input_hash, model_id, embedding_dim, \
             vector_blob, computed_at_ms, last_used_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ],
        )?;
        count += changed as u64;
    }
    Ok(count)
}

/// Carry the repo's MODEL STATE as one coherent unit: the portable `repo_meta` keys
/// ([`CARRIED_META_KEYS`] — identity, freshness version, remote-endpoint config, and the
/// provisional-provenance flag) plus the active model's `ai_models` READINESS row. Splitting this
/// across the move breaks it as a set: the cache rows are useless without the model identity, the
/// identity routes nowhere without the remote config, an absent provisional flag hardens an
/// auto-pick into a config-immune choice, and an identity pointing at a `MissingModel` row makes
/// `active_embedder` refuse — semantic search/reconcile "not ready" despite the carried cache
/// making re-embedding a no-op. Each `repo_meta` carry MIRRORS the source per key (see
/// [`CARRIED_META_KEYS`]): value-gated upsert when present, delete when absent — counts reflect
/// rows actually changed, so a no-edit retry reports zero. The legacy DB is single-repo, so
/// values are read by KEY regardless of the source's own `repo_id`.
pub(super) fn copy_model_state(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
) -> anyhow::Result<u64> {
    if !schema::table_exists(source, "repo_meta")? {
        return Ok(0);
    }
    let mut count = 0u64;
    let mut active_model: Option<String> = None;
    for key in CARRIED_META_KEYS {
        if *key == MEMORY_STREAM_SEAL_POLICY_META_KEY || *key == MEMORY_STREAM_ACCESS_MODE_META_KEY
        {
            continue;
        }
        let value: Option<String> = source
            .query_row("SELECT value FROM repo_meta WHERE key = ?1 LIMIT 1", [key], |row| {
                row.get(0)
            })
            .optional()?
            .flatten();
        let changed = match value {
            Some(value) => {
                if *key == "active_embedding_model" {
                    active_model = Some(value.clone());
                }
                tx.execute(
                    "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)
                     ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value
                     WHERE repo_meta.value IS NOT excluded.value",
                    params![repo_id, key, value],
                )?
            },
            // Absent in the authoritative source: a window model switch may have REMOVED the key
            // (absence has meaning — batch 6); a surviving stale copy would tear the unit.
            None => tx
                .execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", params![
                    repo_id, key
                ])?,
        };
        count += changed as u64;
    }
    count += merge_stream_seal_policy(source, tx, repo_id)?;
    count += merge_stream_access_mode(source, tx, repo_id)?;
    count += merge_stream_pin(source, tx, repo_id)?;
    if let Some(model_id) = active_model {
        carry_active_model_readiness(source, tx, &model_id)?;
    }
    Ok(count)
}

/// `repo_meta[key]` in the legacy SOURCE index — a single-repo store, so the key alone selects it.
fn source_repo_meta(source: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(source
        .query_row("SELECT value FROM repo_meta WHERE key = ?1 LIMIT 1", [key], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

/// `repo_meta[key]` for `repo_id` in the consolidation target.
fn target_repo_meta(tx: &Connection, repo_id: &str, key: &str) -> anyhow::Result<Option<String>> {
    Ok(tx
        .query_row(
            "SELECT value FROM repo_meta WHERE repo_id = ?1 AND key = ?2",
            params![repo_id, key],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// A `repo_meta` setting whose only persisted token is `token` — absence means no explicit intent.
struct SingleTokenMeta {
    key: &'static str,
    token: &'static str,
    /// Names the setting in the unknown-token refusal.
    what: &'static str,
}

const SEAL_POLICY_META: SingleTokenMeta = SingleTokenMeta {
    key: MEMORY_STREAM_SEAL_POLICY_META_KEY,
    token: "sealed",
    what: "memory stream seal policy",
};

const ACCESS_MODE_META: SingleTokenMeta = SingleTokenMeta {
    key: MEMORY_STREAM_ACCESS_MODE_META_KEY,
    token: "public",
    what: "memory stream access mode",
};

struct MetaSides {
    source: Option<String>,
    target: Option<String>,
}

impl SingleTokenMeta {
    /// The `(source, target)` values, refusing when either side holds a token this binary does not
    /// understand — before any of consolidation's reconciliation authoring.
    fn read_both(
        &self,
        source: &Connection,
        tx: &Connection,
        repo_id: &str,
    ) -> anyhow::Result<MetaSides> {
        let source_value = source_repo_meta(source, self.key)?;
        let target_value = target_repo_meta(tx, repo_id, self.key)?;
        for (side, value) in
            [("legacy source", source_value.as_deref()), ("target", target_value.as_deref())]
        {
            if let Some(value) = value
                && value != self.token
            {
                anyhow::bail!(
                    "{side} repo `{repo_id}` has unknown {} `{value}`; refusing to consolidate",
                    self.what
                );
            }
        }
        Ok(MetaSides { source: source_value, target: target_value })
    }

    /// Carry a present source value onto an ABSENT target — the merge's only write. Returns the
    /// rows written.
    fn carry_onto_absent_target(
        &self,
        tx: &Connection,
        repo_id: &str,
        sides: &MetaSides,
    ) -> anyhow::Result<u64> {
        if sides.source.is_some() && sides.target.is_none() {
            Ok(tx.execute(
                "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)",
                params![repo_id, self.key, self.token],
            )? as u64)
        } else {
            Ok(0)
        }
    }
}

/// Carry the source's trust pin onto the target — see the `memory_stream_pin` entry in the
/// classification on [`CARRIED_META_KEYS`]. Returns the rows written.
fn merge_stream_pin(source: &Connection, tx: &Connection, repo_id: &str) -> anyhow::Result<u64> {
    let target_meta = |key: &str| target_repo_meta(tx, repo_id, key);
    // The EFFECTIVE pin: a store from before the pin existed, or one still subscribed, records its
    // trust decision only as the subscription owner.
    let Some(source_pin) = source_repo_meta(source, MEMORY_STREAM_PIN_META_KEY)?
        .or(source_repo_meta(source, MEMORY_SUBSCRIPTION_OWNER_META_KEY)?)
    else {
        return Ok(0);
    };
    let target_pin = target_meta(MEMORY_STREAM_PIN_META_KEY)?;
    let replaceable = match &target_pin {
        None => true,
        Some(pin) if *pin == source_pin => return Ok(0),
        // The pin an earlier, unfinished run wrote FROM THIS SAME legacy source, untouched since —
        // a subscribe in the target clears the marker, and a completed consolidation retires it.
        // Until the rename lands the legacy index is the live store, so a repin made there in the
        // crash-retry window must replace the copy the last run left. Another source's pin is not
        // a stale copy of this one: two clones of a repository share its repo id, and letting the
        // second overwrite the first would silently move the trust root.
        Some(pin) =>
            target_meta(MEMORY_STREAM_PIN_IMPORTED_META_KEY)?.as_deref()
                == Some(pin_import_marker(source, pin).as_str()),
    };
    if !replaceable {
        anyhow::bail!(
            "consolidation refused: the legacy index for `{repo_id}` trusts stream owner \
             {source_pin}, but the global store already pins {} from a decision made there, not \
             from an earlier consolidation. Carrying either would silently override the other \
             trust root. Confirm which owner this repository should trust, record it in the \
             legacy index with `rag-rat sync subscribe <owner>`, and retry",
            target_pin.unwrap_or_default(),
        );
    }
    let marker = pin_import_marker(source, &source_pin);
    let mut written = 0;
    for (key, value) in [
        (MEMORY_STREAM_PIN_META_KEY, source_pin.as_str()),
        (MEMORY_STREAM_PIN_IMPORTED_META_KEY, marker.as_str()),
    ] {
        written += tx.execute(
            "INSERT INTO repo_meta(repo_id, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value",
            params![repo_id, key, value],
        )?;
    }
    Ok(written as u64)
}

/// What `memory_stream_pin_imported` holds: the pin an import wrote AND the legacy source it came
/// from, so only a retry of that same source can claim it. A legacy index is always a file, so its
/// path identifies it for as long as the consolidation stays unfinished.
fn pin_import_marker(source: &Connection, pin: &str) -> String {
    serde_json::json!({ "source": source.path().unwrap_or_default(), "pin": pin }).to_string()
}

/// Retire the import marker once a consolidation has completed — see `merge_stream_pin`.
pub(super) fn retire_pin_import(conn: &Connection, repo_id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM repo_meta WHERE repo_id = ?1 AND key = ?2", params![
        repo_id,
        MEMORY_STREAM_PIN_IMPORTED_META_KEY
    ])
}

/// Merge the owner-stream seal policy as a one-way ratchet. The only persisted value this binary
/// understands is `sealed`; absence means no explicit intent. A sealed source must seal the target,
/// while a target already sealed remains sealed across retries even if the legacy source is absent.
/// Unknown values on either side fail closed before consolidation's reconciliation authoring.
fn merge_stream_seal_policy(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
) -> anyhow::Result<u64> {
    let sides = SEAL_POLICY_META.read_both(source, tx, repo_id)?;
    SEAL_POLICY_META.carry_onto_absent_target(tx, repo_id, &sides)
}

/// Merge the owner-stream ACCESS MODE. Unlike the seal ratchet there is NO safe winner: access mode
/// folds into the stream identity, so a public and a non-public index own DIFFERENT `/2` streams —
/// silently picking one would either strand content or (private→public) leak private memories onto
/// a public-labeled stream. So two EXPLICIT modes that disagree REFUSE. The only persisted token is
/// `public` (absence = private default); the consolidation target is fresh, so a lone `public`
/// source carries onto the absent target (making the consolidated index public), exactly as
/// intended for a published node switching embedding models.
fn merge_stream_access_mode(
    source: &Connection,
    tx: &Connection,
    repo_id: &str,
) -> anyhow::Result<u64> {
    let sides = ACCESS_MODE_META.read_both(source, tx, repo_id)?;

    // Two explicit-but-disagreeing modes have no safe winner. With only `public`/absent this can
    // only be source-`public` vs target-`public` (agree) today; the guard future-proofs a
    // `private` token.
    if let (Some(s), Some(t)) = (sides.source.as_deref(), sides.target.as_deref())
        && s != t
    {
        anyhow::bail!(
            "legacy source and target repo `{repo_id}` disagree on memory stream access mode \
             (`{s}` vs `{t}`); refusing to consolidate a public and a non-public index"
        );
    }

    ACCESS_MODE_META.carry_onto_absent_target(tx, repo_id, &sides)
}

/// Carry the active model's `ai_models` READINESS onto the target when the legacy DB holds it
/// Ready and the target does not. WHY carrying `Ready` is sound here: consolidation is
/// SAME-MACHINE by construction, and `Ready` asserts machine-level availability — fastembed
/// artifacts live in the machine-global HF cache (which `recover_cached_fastembed_model` re-probes
/// on scoped opens, so a stale carry self-corrects), remote runtimes reconstruct their transport
/// from the carried `active_embedding_remote_config` at use time, and the hash model needs no
/// artifacts at all. A misjudged carry surfaces as a use-time embed error and is repaired by
/// install/recovery — never data corruption. GUARD: a target row with `disabled = 1` is an
/// explicit machine-level opt-out shared by every repo in the global DB — never overridden.
fn carry_active_model_readiness(
    source: &Connection,
    tx: &Connection,
    model_id: &str,
) -> anyhow::Result<()> {
    if !schema::table_exists(source, "ai_models")? {
        return Ok(());
    }
    // Only a legacy row that is genuinely Ready (installed, not disabled) is worth carrying.
    let legacy: Option<(Option<i64>, String, Option<i64>)> = source
        .query_row(
            "SELECT embedding_dim, runtime, installed_at_ms FROM ai_models
             WHERE model_id = ?1 AND installed = 1 AND disabled = 0 AND status = 'Ready'",
            [model_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((embedding_dim, runtime, installed_at_ms)) = legacy else {
        return Ok(());
    };
    // Absent on the target → seed the full row Ready.
    let changed = tx.execute(
        "INSERT OR IGNORE INTO ai_models(model_id, capability, embedding_dim, runtime, installed, \
         disabled, status, installed_at_ms, last_error)
         VALUES (?1, 'embedding', ?2, ?3, 1, 0, 'Ready', ?4, NULL)",
        params![model_id, embedding_dim, runtime, installed_at_ms],
    )?;
    if changed > 0 {
        return Ok(());
    }
    // Present but not usable (e.g. the manifest seeded it `MissingModel`) → restore the legacy
    // readiness, UNLESS explicitly disabled on the target (machine-level opt-out wins).
    tx.execute(
        "UPDATE ai_models
         SET installed = 1, status = 'Ready', embedding_dim = ?2, runtime = ?3,
             installed_at_ms = ?4, last_error = NULL
         WHERE model_id = ?1 AND disabled = 0 AND NOT (installed = 1 AND status = 'Ready')",
        params![model_id, embedding_dim, runtime, installed_at_ms],
    )?;
    Ok(())
}

/// Re-derive the `repo_memory_fts` mirror for `repo_id` from the freshly-imported base tables —
/// the V042-rebuild shape (same space-joined tag derivation as `upsert_memory_fts`). Runs inside
/// the import transaction. Delete-then-insert scoped to the repo keeps a retry convergent (the
/// mirror has no PK, so re-inserting would otherwise accumulate duplicate rows) and re-derives any
/// pre-existing global-side memories of this repo to identical content.
pub(super) fn rebuild_memory_fts_for_repo(tx: &Connection, repo_id: &str) -> anyhow::Result<()> {
    tx.execute("DELETE FROM repo_memory_fts WHERE repo_id = ?1", [repo_id])?;
    tx.execute(
        "INSERT INTO repo_memory_fts(repo_id, memory_id, title, body, kind, tags)
         SELECT
             m.repo_id, m.id, m.title, m.body, m.kind,
             COALESCE(
                 (SELECT group_concat(t.tag, ' ')
                  FROM repo_memory_tags t WHERE t.memory_id = m.id),
                 ''
             )
         FROM repo_memories m
         WHERE m.repo_id = ?1",
        [repo_id],
    )?;
    Ok(())
}
