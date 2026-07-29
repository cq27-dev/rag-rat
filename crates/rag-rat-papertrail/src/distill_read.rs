//! Read model for distilled decision records (#705).
//!
//! The distilled record store (#703) is written by `rag-rat-core`'s extraction + LLM passes and,
//! until now, read by nothing. This is the read side: [`distilled_record_for_thread`] loads a
//! record (main row + junctions) by thread identity and resolves the EFFECTIVE outcome status
//! ([`crate::distill_status::effective_status`]) — never the raw `outcome_status_model`, which the
//! mechanical floors can override.
//!
//! Coalescing: a merged PR that closed an issue does NOT get its own record — the issue's row owns
//! the pair (the extraction coalesces via a `coalesced` edge). So a lookup keyed by such a PR
//! follows the edge to the issue that holds the record, and every record carries the identities of
//! the threads coalesced into it so a caller can present an issue↔PR pair as one result.

use rusqlite::{Connection, OptionalExtension, params};

use crate::distill_status::{EffectiveStatusInputs, effective_status};
use crate::{DistillEdgeKind, EpistemicStatus, FixEdgeSource, OutcomeStatus, ThreadShape};

/// A thread's natural identity — the key of a `papertrail_distill` row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordKey {
    pub tracker: String,
    pub project: String,
    pub item_kind: String,
    pub item_key: String,
}

/// A thread coalesced into a record (the paired issue or PR), for issue↔PR dedup at the call site.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CoalescedThread {
    pub item_kind: String,
    pub item_key: String,
}

/// An alternative the thread explicitly considered and did not take.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RejectedAlternative {
    pub alternative: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A distilled decision record as the consumption surfaces see it: the model-owned narrative
/// fields, the mechanically resolved effective status, the rejected alternatives, the fixing
/// commits, the provenance facets, and the coalesced partner identities.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DistilledRecord {
    pub tracker: String,
    pub project: String,
    pub item_kind: String,
    pub item_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_issue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_cause_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_chosen: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected_alternatives: Vec<RejectedAlternative>,
    /// The EFFECTIVE outcome status (floors resolved over the model status) — surface this, never
    /// [`outcome_status_model`](Self::outcome_status_model) alone.
    pub outcome_status: OutcomeStatus,
    /// The raw model-emitted status, retained for internal provenance; may differ from
    /// [`outcome_status`](Self::outcome_status) when a mechanical floor overrode it. NOT
    /// serialized into the payload, so a consumer cannot read the pre-floor status instead of
    /// the resolved one.
    #[serde(skip_serializing)]
    pub outcome_status_model: Option<OutcomeStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epistemic_status_decision: Option<EpistemicStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epistemic_status_outcome: Option<EpistemicStatus>,
    pub fix_edge_source: FixEdgeSource,
    pub thread_shape: ThreadShape,
    pub outcome_claim_verified: bool,
    pub decision_provenance_verified: bool,
    pub anchors_qualified_count: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fixing_commits: Vec<String>,
    /// The issue/PR threads coalesced into this record — present so a caller can answer an
    /// issue↔PR pair as one result.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub coalesced: Vec<CoalescedThread>,
}

/// A distilled record surfaced on a drive-by (symbol / chunk) surface, LABELED so a consumer knows
/// it is model-distilled and NOT human-reviewed. The record itself already carries its thread
/// identity and fixing commits as provenance; this wrapper adds the `unreviewed` label the drive-by
/// spec requires (a symbol's related decisions are best-effort context, not verified facts).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DriveByRecord {
    /// Always `true`: the record is model-distilled, not human-reviewed.
    pub unreviewed: bool,
    #[serde(flatten)]
    pub record: DistilledRecord,
}

/// A selected distilled record anchored to one current file. Symbol anchors carry their current
/// display line; file anchors use `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathDistilledRecord {
    pub record: DistilledRecord,
    pub line: Option<i64>,
}

impl DriveByRecord {
    pub fn new(record: DistilledRecord) -> Self {
        Self { unreviewed: true, record }
    }
}

/// Load the canonical distilled record for a thread, following a `coalesced` edge when the thread
/// was coalesced into a partner that owns the row. `None` when no record exists for the thread or
/// its coalesce partner. `repo_id` is the active repo (mandatory: the store is one index over every
/// repo in a consolidated DB), passed by the caller so a per-hit loop resolves it once.
pub fn distilled_record_for_thread(
    conn: &Connection,
    repo_id: &str,
    key: &RecordKey,
) -> anyhow::Result<Option<DistilledRecord>> {
    if let Some(record) = load_record(conn, repo_id, key)? {
        return Ok(Some(record));
    }
    // No own row: the thread may be a merged PR coalesced into the issue that owns the record.
    if let Some(partner) = coalesced_partner_owning_a_record(conn, repo_id, key)? {
        return load_record(conn, repo_id, &partner);
    }
    Ok(None)
}

/// The distilled records worth surfacing on a logical symbol (#705 drive-by lane). The FACET GATE
/// is deliberately strict for a tight per-symbol cap: a record qualifies only when its thread has a
/// PROVIDER fix edge AND a SELECTED (model-chosen, V078) resolved symbol anchor bound to this
/// symbol. A bare mined-but-unselected anchor, or a text-tier / no fix edge, does not surface.
///
/// `logical_symbol_id` is the i64 logical-symbol handle (e.g. `SymbolHit.logical_symbol_id`); the
/// helper formats it to the `sym_<hex>` token the anchor store keys on
/// (`rag_rat_base::serde_big_id::format_sym_handle`) so a caller can never pass the wrong string
/// form. Records come back newest-distilled first, at most `limit` of them (the drive-by cap), each
/// loaded directly for the anchor's OWN record-owning thread (no coalesce redirect — the anchor
/// always sits on the thread that owns the record). Repo-scoped.
///
/// RELOCATION: the anchor stores the logical id as the opaque `sym_<hex>` TEXT handle rather than
/// the i64 every other reference column holds, which is exactly why an early remap pass skipped it
/// and a stale token could surface the previous occupant's record (#810). It is now rewritten with
/// every other durable reference when an id is remapped, and cleared to NULL with `resolved = 0`
/// when the remap has no winner — so a `selected` and `resolved` anchor names the symbol it was
/// mined for. The drive-by is still labeled unreviewed because the record's text is
/// model-distilled, not because the anchor might point somewhere else.
pub fn records_for_symbol(
    conn: &Connection,
    repo_id: &str,
    logical_symbol_id: i64,
    limit: usize,
) -> anyhow::Result<Vec<DistilledRecord>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    // The distill store is OPTIONAL enrichment: a pre-distill or partially-migrated index has the
    // symbol index but not the distill tables (V077). Serve nothing rather than error the whole
    // surface that attaches the drive-by (symbol_lookup etc.) on a missing table.
    if !rag_rat_db::schema::table_exists(conn, "papertrail_distill")? {
        return Ok(Vec::new());
    }
    let token = rag_rat_base::serde_big_id::format_sym_handle(logical_symbol_id);
    // The eligible record-owning threads: DISTINCT so one record anchored by several symbol rows
    // counts once; newest-distilled first with a deterministic tiebreak so the cap is stable.
    let mut stmt = conn.prepare(
        "SELECT anchor.tracker, anchor.project, anchor.item_kind, anchor.item_key
         FROM papertrail_distill_anchors AS anchor
         JOIN papertrail_distill AS record
           ON record.repo_id = anchor.repo_id AND record.tracker = anchor.tracker
          AND record.project = anchor.project AND record.item_kind = anchor.item_kind
          AND record.item_key = anchor.item_key
         WHERE anchor.repo_id = ?1 AND anchor.anchor_kind = 'symbol' AND anchor.selected = 1
           AND anchor.logical_symbol_id = ?2 AND record.fix_edge_source = ?3
         GROUP BY anchor.tracker, anchor.project, anchor.item_kind, anchor.item_key
         ORDER BY MAX(record.distilled_at_ms) DESC, anchor.tracker, anchor.project,
                  anchor.item_kind, anchor.item_key
         LIMIT ?4",
    )?;
    let keys = stmt.query_map(
        params![repo_id, token, FixEdgeSource::Provider.as_db_str(), i64::try_from(limit)?],
        |row| {
            Ok(RecordKey {
                tracker: row.get(0)?,
                project: row.get(1)?,
                item_kind: row.get(2)?,
                item_key: row.get(3)?,
            })
        },
    )?;
    let mut records = Vec::new();
    for key in keys {
        let key = key?;
        // Load the anchor thread's OWN record directly (never redirect): a redirect would be wrong
        // here (the thread owns the record), and it also closes a TOCTOU where a concurrent delete
        // between the SELECT and this load could otherwise return a coalesced partner's record.
        if let Some(record) = load_record(conn, repo_id, &key)? {
            records.push(record);
        }
    }
    Ok(records)
}

/// Distilled records selected for a current file, either through an exact file anchor or a symbol
/// anchor whose logical-symbol member currently belongs to that file. Repo-scoped and newest first.
///
/// A symbol anchor is admitted only when it is `resolved = 1` AND its token still joins a LIVE
/// `logical_symbol_members` row whose symbol sits in the requested file, so the returned `line` is
/// that symbol's position in the current index rather than a mining-time snapshot. That check plus
/// the remap rewrite described on [`records_for_symbol`] is what keeps a decision off a file it no
/// longer belongs to: an anchor whose symbol moved away simply stops matching here.
pub fn records_for_path(
    conn: &Connection,
    repo_id: &str,
    path: &str,
    limit: usize,
) -> anyhow::Result<Vec<PathDistilledRecord>> {
    if limit == 0 || !rag_rat_db::schema::table_exists(conn, "papertrail_distill")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "WITH requested AS MATERIALIZED (
             SELECT id FROM files WHERE path = ?2
         )
         SELECT anchor.tracker, anchor.project, anchor.item_kind, anchor.item_key,
                MIN(CASE WHEN anchor.anchor_kind = 'symbol' THEN symbol.start_line END) AS line
         FROM papertrail_distill_anchors anchor
         JOIN papertrail_distill record
           ON record.repo_id = anchor.repo_id AND record.tracker = anchor.tracker
          AND record.project = anchor.project AND record.item_kind = anchor.item_kind
          AND record.item_key = anchor.item_key
         LEFT JOIN logical_symbol_members member
           ON anchor.anchor_kind = 'symbol'
          AND anchor.logical_symbol_id = 'sym_' || format('%x', member.logical_symbol_id)
         LEFT JOIN symbols symbol
           ON symbol.id = member.symbol_id AND symbol.file_id IN (SELECT id FROM requested)
         WHERE anchor.repo_id = ?1 AND anchor.selected = 1
           AND EXISTS (SELECT 1 FROM requested)
           AND (
               (anchor.anchor_kind = 'file' AND anchor.file_path = ?2)
               OR (anchor.anchor_kind = 'symbol' AND anchor.resolved = 1 AND symbol.id IS NOT NULL)
           )
         GROUP BY anchor.tracker, anchor.project, anchor.item_kind, anchor.item_key
         ORDER BY MAX(record.distilled_at_ms) DESC, anchor.tracker, anchor.project,
                  anchor.item_kind, anchor.item_key
         LIMIT ?3",
    )?;
    let keys = stmt.query_map(params![repo_id, path, i64::try_from(limit)?], |row| {
        Ok((
            RecordKey {
                tracker: row.get(0)?,
                project: row.get(1)?,
                item_kind: row.get(2)?,
                item_key: row.get(3)?,
            },
            row.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut records = Vec::new();
    for key in keys {
        let (key, line) = key?;
        if let Some(record) = load_record(conn, repo_id, &key)? {
            records.push(PathDistilledRecord { record, line });
        }
    }
    Ok(records)
}

/// The threads a `coalesced` edge connects to `key`, in either direction (the edge is thread-keyed
/// and survives record regeneration).
fn coalesced_partners(
    conn: &Connection,
    repo_id: &str,
    key: &RecordKey,
) -> anyhow::Result<Vec<CoalescedThread>> {
    // ORDER BY the combined UNION so the partner list is DETERMINISTIC: when one PR closes several
    // issues (`Closes #5, Closes #7`), both are valid owners, so the redirect target and the
    // rendered `coalesced` order must not depend on SQLite's unspecified UNION order.
    let mut stmt = conn.prepare(
        "SELECT dst_item_kind, dst_item_key FROM papertrail_distill_edges
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND edge_kind = ?4
           AND src_item_kind = ?5 AND src_item_key = ?6
         UNION
         SELECT src_item_kind, src_item_key FROM papertrail_distill_edges
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND edge_kind = ?4
           AND dst_item_kind = ?5 AND dst_item_key = ?6
         ORDER BY 1, 2",
    )?;
    let rows = stmt.query_map(
        params![
            repo_id,
            key.tracker,
            key.project,
            DistillEdgeKind::Coalesced.as_db_str(),
            key.item_kind,
            key.item_key,
        ],
        |row| Ok(CoalescedThread { item_kind: row.get(0)?, item_key: row.get(1)? }),
    )?;
    let mut partners = Vec::new();
    for row in rows {
        partners.push(row?);
    }
    Ok(partners)
}

/// The first coalesce partner of `key` that actually owns a `papertrail_distill` row — the redirect
/// target for a thread (a coalesced-away merged PR) with no record of its own.
fn coalesced_partner_owning_a_record(
    conn: &Connection,
    repo_id: &str,
    key: &RecordKey,
) -> anyhow::Result<Option<RecordKey>> {
    for partner in coalesced_partners(conn, repo_id, key)? {
        let partner_key = RecordKey {
            tracker: key.tracker.clone(),
            project: key.project.clone(),
            item_kind: partner.item_kind,
            item_key: partner.item_key,
        };
        if record_exists(conn, repo_id, &partner_key)? {
            return Ok(Some(partner_key));
        }
    }
    Ok(None)
}

fn record_exists(conn: &Connection, repo_id: &str, key: &RecordKey) -> anyhow::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM papertrail_distill
             WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
               AND item_key = ?5
         )",
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Load the full record for a thread that owns a row (no coalesce redirect). `None` if absent.
fn load_record(
    conn: &Connection,
    repo_id: &str,
    key: &RecordKey,
) -> anyhow::Result<Option<DistilledRecord>> {
    let row = conn
        .query_row(
            "SELECT root_issue, root_cause, root_cause_class, decision_chosen, outcome_summary,
                    outcome_status_model, epistemic_status_decision, epistemic_status_outcome,
                    fix_edge_source, thread_shape, outcome_claim_verified,
                    decision_provenance_verified, anchors_qualified_count, revert_override,
                    closing_keyword_floor
             FROM papertrail_distill
             WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
               AND item_key = ?5",
            params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
            |row| {
                Ok(MainRow {
                    root_issue: row.get(0)?,
                    root_cause: row.get(1)?,
                    root_cause_class: row.get(2)?,
                    decision_chosen: row.get(3)?,
                    outcome_summary: row.get(4)?,
                    outcome_status_model: row.get(5)?,
                    epistemic_status_decision: row.get(6)?,
                    epistemic_status_outcome: row.get(7)?,
                    fix_edge_source: row.get(8)?,
                    thread_shape: row.get(9)?,
                    outcome_claim_verified: row.get(10)?,
                    decision_provenance_verified: row.get(11)?,
                    anchors_qualified_count: row.get(12)?,
                    revert_override: row.get(13)?,
                    closing_keyword_floor: row.get(14)?,
                })
            },
        )
        .optional()?;
    let Some(row) = row else { return Ok(None) };

    let fix_edge_source = FixEdgeSource::from_db_str(&row.fix_edge_source)?;
    let outcome_status_model =
        row.outcome_status_model.as_deref().map(OutcomeStatus::from_db_str).transpose()?;
    let outcome_status = effective_status(&EffectiveStatusInputs {
        revert_override: row.revert_override,
        closing_keyword: row.closing_keyword_floor.is_some(),
        fix_edge_source,
        model_status: outcome_status_model,
    });

    Ok(Some(DistilledRecord {
        tracker: key.tracker.clone(),
        project: key.project.clone(),
        item_kind: key.item_kind.clone(),
        item_key: key.item_key.clone(),
        root_issue: row.root_issue,
        root_cause: row.root_cause,
        root_cause_class: row.root_cause_class,
        decision_chosen: row.decision_chosen,
        rejected_alternatives: rejected_alternatives(conn, repo_id, key)?,
        outcome_status,
        outcome_status_model,
        outcome_summary: row.outcome_summary,
        epistemic_status_decision: row
            .epistemic_status_decision
            .as_deref()
            .map(EpistemicStatus::from_db_str)
            .transpose()?,
        epistemic_status_outcome: row
            .epistemic_status_outcome
            .as_deref()
            .map(EpistemicStatus::from_db_str)
            .transpose()?,
        fix_edge_source,
        thread_shape: ThreadShape::from_db_str(&row.thread_shape)?,
        outcome_claim_verified: row.outcome_claim_verified,
        decision_provenance_verified: row.decision_provenance_verified,
        anchors_qualified_count: row.anchors_qualified_count,
        fixing_commits: fixing_commits(conn, repo_id, key)?,
        coalesced: coalesced_partners(conn, repo_id, key)?,
    }))
}

/// The raw `papertrail_distill` columns the read model needs, before enum parsing.
struct MainRow {
    root_issue: Option<String>,
    root_cause: Option<String>,
    root_cause_class: Option<String>,
    decision_chosen: Option<String>,
    outcome_summary: Option<String>,
    outcome_status_model: Option<String>,
    epistemic_status_decision: Option<String>,
    epistemic_status_outcome: Option<String>,
    fix_edge_source: String,
    thread_shape: String,
    outcome_claim_verified: bool,
    decision_provenance_verified: bool,
    anchors_qualified_count: i64,
    revert_override: bool,
    closing_keyword_floor: Option<String>,
}

fn rejected_alternatives(
    conn: &Connection,
    repo_id: &str,
    key: &RecordKey,
) -> anyhow::Result<Vec<RejectedAlternative>> {
    let mut stmt = conn.prepare(
        "SELECT alternative, reason FROM papertrail_distill_alternatives
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5
         ORDER BY ordinal",
    )?;
    let rows = stmt.query_map(
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| Ok(RejectedAlternative { alternative: row.get(0)?, reason: row.get(1)? }),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn fixing_commits(
    conn: &Connection,
    repo_id: &str,
    key: &RecordKey,
) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT commit_sha FROM papertrail_distill_record_commits
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5
         ORDER BY commit_sha",
    )?;
    let rows = stmt.query_map(
        params![repo_id, key.tracker, key.project, key.item_kind, key.item_key],
        |row| row.get::<_, String>(0),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::{DriveByRecord, RecordKey, distilled_record_for_thread, records_for_symbol};
    use crate::{FixEdgeSource, OutcomeStatus, ThreadShape};

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply_distill_record_store(&conn).unwrap();
        // V078 adds the anchor `selected` flag that `records_for_symbol` gates on.
        rag_rat_db::schema::apply_distill_anchor_selection(&conn).unwrap();
        conn
    }

    fn seed_symbol_record(conn: &Connection, item_key: &str, fix_edge: &str, distilled_at_ms: i64) {
        conn.execute(
            "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  root_issue, fix_edge_source, thread_shape, anchors_qualified_count,
                  distilled_at_ms, repo_id)
             VALUES ('github','o/r','issue',?1,'sha256:h',3,?1,?2,'investigation',1,?3,'repoA')",
            params![item_key, fix_edge, distilled_at_ms],
        )
        .unwrap();
    }

    // Logical-symbol handles for the drive-by tests; the anchor store keys on their `sym_<hex>`
    // token form, which `records_for_symbol` derives from the i64 it is given.
    const SYM_A: i64 = 100;
    const SYM_B: i64 = 200;
    const SYM_C: i64 = 300;
    const SYM_UNANCHORED: i64 = 999;

    fn seed_symbol_anchor(conn: &Connection, item_key: &str, sym_id: i64, selected: bool) {
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, name,
                  resolved, candidate_ordinal, selected, repo_id)
             VALUES ('github','o/r','issue',?1,'symbol',?2,'run',1,0,?3,'repoA')",
            params![
                item_key,
                rag_rat_base::serde_big_id::format_sym_handle(sym_id),
                selected as i64
            ],
        )
        .unwrap();
    }

    #[test]
    fn records_for_symbol_gates_on_a_provider_edge_and_a_selected_anchor() {
        let conn = conn();
        // Qualifies: provider fix edge + a SELECTED symbol anchor on the target symbol.
        seed_symbol_record(&conn, "5", "provider", 10);
        seed_symbol_anchor(&conn, "5", SYM_A, true);
        // Excluded: a text-tier fix edge (not provider), even with a selected anchor.
        seed_symbol_record(&conn, "6", "text", 10);
        seed_symbol_anchor(&conn, "6", SYM_A, true);
        // Excluded: provider edge but the anchor is a mined candidate the model did NOT select.
        seed_symbol_record(&conn, "7", "provider", 10);
        seed_symbol_anchor(&conn, "7", SYM_A, false);
        // Excluded: qualifies but for a DIFFERENT symbol.
        seed_symbol_record(&conn, "8", "provider", 10);
        seed_symbol_anchor(&conn, "8", SYM_B, true);

        let records = records_for_symbol(&conn, "repoA", SYM_A, 10).unwrap();
        assert_eq!(
            records.iter().map(|r| r.item_key.as_str()).collect::<Vec<_>>(),
            vec!["5"],
            "only the provider-edge record with a selected anchor on this symbol surfaces",
        );
    }

    #[test]
    fn records_for_symbol_caps_at_the_limit_newest_distilled_first() {
        let conn = conn();
        for (key, ms) in [("5", 10), ("6", 30), ("7", 20)] {
            seed_symbol_record(&conn, key, "provider", ms);
            seed_symbol_anchor(&conn, key, SYM_C, true);
        }
        let records = records_for_symbol(&conn, "repoA", SYM_C, 2).unwrap();
        assert_eq!(
            records.iter().map(|r| r.item_key.as_str()).collect::<Vec<_>>(),
            vec!["6", "7"],
            "capped at 2, newest-distilled first",
        );
    }

    #[test]
    fn records_for_symbol_is_empty_for_an_unanchored_symbol() {
        let conn = conn();
        seed_symbol_record(&conn, "5", "provider", 10);
        seed_symbol_anchor(&conn, "5", SYM_A, true);
        assert!(records_for_symbol(&conn, "repoA", SYM_UNANCHORED, 10).unwrap().is_empty());
        assert!(records_for_symbol(&conn, "repoA", SYM_A, 0).unwrap().is_empty(), "limit 0");
    }

    #[test]
    fn drive_by_record_labels_the_serialized_record_unreviewed() {
        let conn = conn();
        // Seed a NON-null model status so this test can actually FAIL if the raw field ever
        // regresses from unconditional `skip_serializing` to a conditional skip: it must be absent
        // from the wire even when the record carries it.
        seed_record(&conn, &Seed {
            repo: "repoA",
            kind: "issue",
            key: "5",
            root_issue: None,
            model_status: Some("landed"),
            fix_edge_source: "provider",
            revert_override: false,
            closing_keyword: None,
        });
        let record =
            distilled_record_for_thread(&conn, "repoA", &key("issue", "5")).unwrap().unwrap();
        assert_eq!(
            record.outcome_status_model,
            Some(OutcomeStatus::Landed),
            "precondition: the record carries a raw model status",
        );
        let json = serde_json::to_value(DriveByRecord::new(record)).unwrap();
        assert_eq!(
            json["unreviewed"],
            serde_json::json!(true),
            "labeled model-distilled, unreviewed"
        );
        assert_eq!(json["item_key"], serde_json::json!("5"), "the record fields flatten alongside");
        assert!(
            json.get("outcome_status_model").is_none(),
            "the raw model status is UNCONDITIONALLY off the wire, even when present",
        );
    }

    #[test]
    fn records_for_symbol_is_a_no_op_when_the_distill_store_is_absent() {
        // A pre-distill / partially-migrated index has the symbol index but no distill tables; the
        // drive-by must return nothing rather than erroring on the missing table.
        let conn = Connection::open_in_memory().unwrap();
        assert!(records_for_symbol(&conn, "repoA", SYM_A, 10).unwrap().is_empty());
    }

    fn key(kind: &str, k: &str) -> RecordKey {
        RecordKey {
            tracker: "github".into(),
            project: "o/r".into(),
            item_kind: kind.into(),
            item_key: k.into(),
        }
    }

    /// A seed for one `papertrail_distill` row; nullable fields as `Option`, booleans as `bool`.
    struct Seed<'a> {
        repo: &'a str,
        kind: &'a str,
        key: &'a str,
        root_issue: Option<&'a str>,
        model_status: Option<&'a str>,
        fix_edge_source: &'a str,
        revert_override: bool,
        closing_keyword: Option<&'a str>,
    }

    fn seed_record(conn: &Connection, s: &Seed) {
        conn.execute(
            "INSERT INTO papertrail_distill
                 (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
                  root_issue, outcome_status_model, fix_edge_source, thread_shape, revert_override,
                  closing_keyword_floor, anchors_qualified_count, distilled_at_ms, repo_id)
             VALUES ('github','o/r',?1,?2,'sha256:h',3,?3,?4,?5,'investigation',?6,?7,2,1,?8)",
            params![
                s.kind,
                s.key,
                s.root_issue,
                s.model_status,
                s.fix_edge_source,
                s.revert_override as i64,
                s.closing_keyword,
                s.repo,
            ],
        )
        .unwrap();
    }

    fn seed_coalesced_edge(conn: &Connection, repo: &str, src: (&str, &str), dst: (&str, &str)) {
        conn.execute(
            "INSERT INTO papertrail_distill_edges
                 (tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                  edge_kind, created_at_ms, repo_id)
             VALUES ('github','o/r',?1,?2,?3,?4,'coalesced',1,?5)",
            params![src.0, src.1, dst.0, dst.1, repo],
        )
        .unwrap();
    }

    #[test]
    fn loads_the_record_fields_alternatives_and_commits_in_order() {
        let conn = conn();
        seed_record(&conn, &Seed {
            repo: "repoA",
            kind: "issue",
            key: "5",
            root_issue: Some("The widget crashes."),
            model_status: Some("superseded"),
            fix_edge_source: "provider",
            revert_override: false,
            closing_keyword: None,
        });
        // Alternatives out of ordinal order; the read model returns them by ordinal.
        conn.execute(
            "INSERT INTO papertrail_distill_alternatives
                 (tracker, project, item_kind, item_key, ordinal, alternative, reason, repo_id)
             VALUES ('github','o/r','issue','5',1,'Retry','Too slow','repoA'),
                    ('github','o/r','issue','5',0,'Cache',NULL,'repoA')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_record_commits
                 (tracker, project, item_kind, item_key, commit_sha, repo_id)
             VALUES ('github','o/r','issue','5','bbb','repoA'),
                    ('github','o/r','issue','5','aaa','repoA')",
            [],
        )
        .unwrap();

        let record =
            distilled_record_for_thread(&conn, "repoA", &key("issue", "5")).unwrap().unwrap();
        assert_eq!(record.item_key, "5");
        assert_eq!(record.root_issue.as_deref(), Some("The widget crashes."));
        assert_eq!(record.fix_edge_source, FixEdgeSource::Provider);
        assert_eq!(record.thread_shape, ThreadShape::Investigation);
        assert_eq!(record.anchors_qualified_count, 2);
        // A fix edge exists and no floor overrides → the effective status defers to the model.
        assert_eq!(record.outcome_status, OutcomeStatus::Superseded);
        assert_eq!(record.outcome_status_model, Some(OutcomeStatus::Superseded));
        assert_eq!(
            record.rejected_alternatives.iter().map(|a| a.alternative.as_str()).collect::<Vec<_>>(),
            vec!["Cache", "Retry"],
            "alternatives come back in ordinal order",
        );
        assert_eq!(record.rejected_alternatives[0].reason, None);
        assert_eq!(record.rejected_alternatives[1].reason.as_deref(), Some("Too slow"));
        assert_eq!(record.fixing_commits, vec!["aaa".to_string(), "bbb".to_string()]);
        assert!(record.coalesced.is_empty());
    }

    #[test]
    fn a_mechanical_floor_overrides_the_model_status() {
        // The read model must apply the effective-status resolver, never the raw model column: a
        // revert override beats a model claim of `landed`.
        let conn = conn();
        seed_record(&conn, &Seed {
            repo: "repoA",
            kind: "issue",
            key: "5",
            root_issue: None,
            model_status: Some("landed"),
            fix_edge_source: "provider",
            revert_override: true,
            closing_keyword: None,
        });
        let record =
            distilled_record_for_thread(&conn, "repoA", &key("issue", "5")).unwrap().unwrap();
        assert_eq!(record.outcome_status, OutcomeStatus::Reverted, "the floor wins");
        assert_eq!(
            record.outcome_status_model,
            Some(OutcomeStatus::Landed),
            "the raw claim is kept"
        );
    }

    #[test]
    fn a_coalesced_pr_hit_redirects_to_the_issue_record() {
        // A merged PR coalesced into an issue has no record of its own; a lookup by the PR follows
        // the `coalesced` edge to the issue that owns the record, which carries the PR as a
        // partner.
        let conn = conn();
        seed_record(&conn, &Seed {
            repo: "repoA",
            kind: "issue",
            key: "5",
            root_issue: Some("Owned by the issue."),
            model_status: Some("landed"),
            fix_edge_source: "provider",
            revert_override: false,
            closing_keyword: Some("closes"),
        });
        seed_coalesced_edge(&conn, "repoA", ("issue", "5"), ("change_request", "6"));

        let via_pr = distilled_record_for_thread(&conn, "repoA", &key("change_request", "6"))
            .unwrap()
            .unwrap();
        assert_eq!(via_pr.item_kind, "issue");
        assert_eq!(via_pr.item_key, "5", "the PR hit resolves to the issue's record");
        assert_eq!(via_pr.root_issue.as_deref(), Some("Owned by the issue."));
        assert_eq!(
            via_pr
                .coalesced
                .iter()
                .map(|c| (c.item_kind.as_str(), c.item_key.as_str()))
                .collect::<Vec<_>>(),
            vec![("change_request", "6")],
            "the record names the PR coalesced into it",
        );
        // A direct lookup by the issue key returns the same record.
        let via_issue =
            distilled_record_for_thread(&conn, "repoA", &key("issue", "5")).unwrap().unwrap();
        assert_eq!(via_issue.item_key, "5");
    }

    #[test]
    fn a_pr_closing_several_issues_redirects_deterministically() {
        // One merged PR closing issue #5 AND issue #7 (`Closes #5, Closes #7`) yields two coalesced
        // edges to the same PR; both issues own records. The PR lookup must resolve to the SAME
        // issue every time (lowest by (kind, key)), not an arbitrary one in SQLite's UNION order.
        let conn = conn();
        for issue in ["5", "7"] {
            seed_record(&conn, &Seed {
                repo: "repoA",
                kind: "issue",
                key: issue,
                root_issue: Some(issue),
                model_status: Some("landed"),
                fix_edge_source: "provider",
                revert_override: false,
                closing_keyword: Some("closes"),
            });
            seed_coalesced_edge(&conn, "repoA", ("issue", issue), ("change_request", "6"));
        }
        for _ in 0..5 {
            let record = distilled_record_for_thread(&conn, "repoA", &key("change_request", "6"))
                .unwrap()
                .unwrap();
            assert_eq!(
                record.item_key, "5",
                "the PR redirects to the ordinally-first owning issue"
            );
        }
    }

    #[test]
    fn no_record_and_no_coalesce_partner_returns_none() {
        let conn = conn();
        assert!(
            distilled_record_for_thread(&conn, "repoA", &key("issue", "99")).unwrap().is_none()
        );
    }

    #[test]
    fn reads_are_repo_scoped() {
        // The same natural key exists in two repos of a consolidated DB; the read must not cross.
        let conn = conn();
        for (repo, root) in [("repoA", "A's issue"), ("repoB", "B's issue")] {
            seed_record(&conn, &Seed {
                repo,
                kind: "issue",
                key: "5",
                root_issue: Some(root),
                model_status: Some("landed"),
                fix_edge_source: "provider",
                revert_override: false,
                closing_keyword: None,
            });
        }
        let a = distilled_record_for_thread(&conn, "repoA", &key("issue", "5")).unwrap().unwrap();
        assert_eq!(a.root_issue.as_deref(), Some("A's issue"));
        let b = distilled_record_for_thread(&conn, "repoB", &key("issue", "5")).unwrap().unwrap();
        assert_eq!(b.root_issue.as_deref(), Some("B's issue"));
        assert!(distilled_record_for_thread(&conn, "repoC", &key("issue", "5")).unwrap().is_none());
    }
}
