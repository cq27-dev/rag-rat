//! Deterministic distill extraction (#703): the model-free pass that turns the papertrail mirror
//! into skeleton distilled records ready for the LLM pass (#704).
//!
//! Eligibility is v1-narrow: CLOSED issues and MERGED change requests, filtered on
//! `state_normalized` (never raw `state` — a merged GitLab MR carries `state='merged'`). An issue
//! closed by a merged PR COALESCES: one record keyed to the ISSUE thread, the PR reachable through
//! a `coalesced` edge, both threads' text folded into the input. Everything a model does not decide
//! is computed here and stored raw: the fix edge's provenance, the mechanical fixing commits, the
//! anchor candidates, the status floors. The LLM columns are left NULL — honest nulls the #704 pass
//! fills by upsert on the same natural key.

use std::collections::{BTreeMap, BTreeSet};

use rag_rat_base::time::now_ms;
use rag_rat_db::schema::active_repo_id;
use rag_rat_papertrail::{CloserKind, ClosingEdgeSource, FixEdgeSource, ItemKind, OutcomeStatus};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::distill::candidates::{self, AnchorCaps};
use crate::distill::thread::{self, SourceKind, SourcePart, SourceRole, ThreadKey};
use crate::distill::{prompts, units, validate};

/// Bumped whenever the extraction/prompt contract changes in a way that invalidates existing
/// records — part of the regeneration identity alongside `distill_input_hash`. 2 → 3 (#800):
/// cross-referenced-item snapshots joined the extraction identity, and the fix-diff renderer was
/// added (its rows are regenerable and deliberately NOT hashed, so a renderer change also rides
/// this bump).
pub(crate) const PIPELINE_VERSION: i64 = 3;

/// Byte cap for ONE snapshotted per-file patch (#800). The diff is a deterministic function of the
/// (immutable) fixing commit, so truncation cannot hide a mutable edit; the cap bounds a single
/// generated/vendored file's patch so one sprawling fix cannot inflate the snapshot table.
const FIX_DIFF_FILE_CAP: usize = 8_000;

/// Blob-size pre-filter for the fix-diff renderer (#800): a file whose old OR new side exceeds
/// this is skipped entirely. The output cap truncates after a full render, which would diff a
/// hostile 50MB minified file in memory inside the extraction write transaction.
const FIX_DIFF_BLOB_CAP: u64 = 1_000_000;

/// Max cross-referenced items snapshotted per record (#800). Matches the prompt's `max_xrefs`
/// budget: refs beyond the cap are invisible to both the snapshot and the prompt.
const XREF_SNAPSHOT_CAP: usize = 20;

/// Knobs for one extraction pass.
pub(crate) struct ExtractOptions {
    pub pipeline_version: i64,
    pub anchor_caps: AnchorCaps,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self { pipeline_version: PIPELINE_VERSION, anchor_caps: AnchorCaps::default() }
    }
}

/// What one extraction pass did — enough to log an honest before/after without a second query.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExtractReport {
    pub eligible: usize,
    pub records_written: usize,
    pub coalesced_pairs: usize,
    pub queued: usize,
    pub fix_edge_provider: usize,
    pub fix_edge_text: usize,
    pub fix_edge_none: usize,
    /// The MECHANICAL effective status (floors only, model absent): how many records the
    /// deterministic floors already resolve to `landed` (closing keyword) or `reverted` before the
    /// model ever runs; the rest stay `unclear` pending #704.
    pub mechanical_landed: usize,
    pub mechanical_reverted: usize,
    pub mechanical_unclear: usize,
}

/// Cheap enqueue that RIDES the mirror sync: insert every currently-eligible thread key into the
/// distill queue, skipping keys already queued. No unit segmentation, no anchor mining, no LLM — a
/// single INSERT…SELECT so it fits inside the sync's short serialized writes. The DRAIN (#704) is
/// where the expensive work happens; it never runs here. Returns the number of newly queued
/// threads.
pub(crate) fn enqueue_eligible(conn: &Connection) -> anyhow::Result<usize> {
    let repo_id = active_repo_id(conn)?;
    let now = now_ms();
    // Eligible = closed issues + merged change requests, on the normalized state — but only threads
    // that have NOT already been distilled (no current record) and are NOT a PR already coalesced
    // into an issue record. Without these guards, every drain (which removes the queue row) would
    // be undone by the next sync re-inserting the completed thread, re-paying the LLM cost
    // forever. A thread whose INPUT changed is re-detected by the heavy extraction pass (hash
    // recompute), not the cheap enqueue. `DO NOTHING` keeps an already-queued thread's
    // attempts/errors intact.
    let queued = conn.execute(
        "INSERT INTO papertrail_distill_queue
             (tracker, project, item_kind, item_key, enqueued_at_ms, repo_id)
         SELECT i.tracker, i.project, i.item_kind, i.item_key, ?2, i.repo_id
         FROM papertrail_items i
         WHERE i.repo_id = ?1
           AND ( (i.item_kind = 'issue' AND i.state_normalized = 'closed')
              OR (i.item_kind = 'change_request' AND i.state_normalized = 'merged') )
           AND NOT EXISTS (
               SELECT 1 FROM papertrail_distill d
               WHERE d.repo_id = i.repo_id AND d.tracker = i.tracker AND d.project = i.project
                 AND d.item_kind = i.item_kind AND d.item_key = i.item_key)
           AND NOT EXISTS (
               SELECT 1 FROM papertrail_distill_edges e
               WHERE e.repo_id = i.repo_id AND e.tracker = i.tracker AND e.project = i.project
                 AND e.dst_item_kind = i.item_kind AND e.dst_item_key = i.item_key
                 AND e.edge_kind = 'coalesced')
         ON CONFLICT(repo_id, tracker, project, item_kind, item_key) DO NOTHING",
        params![repo_id, now],
    )?;
    Ok(queued)
}

/// Run the full deterministic extraction over the active repo's mirror. Writes a skeleton
/// `papertrail_distill` row (mechanical columns populated, model columns NULL), its fixing commits,
/// coalesced edges, and anchor candidates for every eligible thread, and enqueues each. Idempotent:
/// a record's thread-keyed junction rows are cleared and rebuilt, and the record row upserts on its
/// natural key.
pub(crate) fn extract(
    conn: &Connection,
    root: Option<&std::path::Path>,
    opts: &ExtractOptions,
) -> anyhow::Result<ExtractReport> {
    let repo_id = active_repo_id(conn)?;
    let now = now_ms();
    // Open the repo ONCE for the whole pass so anchor mining can fall back to a live gix
    // first-parent diff for merge-commit fixes (which carry no `git_file_changes` rows). Absent /
    // undiscoverable root → no fallback (indexed rows only).
    let repo = root.and_then(|r| rag_rat_base::repo_discover::discover_repo(r).ok());
    // The ENTIRE pass — mirror reads, planning, reconciliation, and writes — runs inside ONE
    // IMMEDIATE transaction. Papertrail sync uses a separate flight lock and mutates these mirror
    // tables concurrently; reading them outside the write txn would let a sync land between the
    // reads and the reconciliation writes, planning off a stale/mixed snapshot (records/queue for
    // threads that just reopened or coalesced). BEGIN IMMEDIATE takes the write lock up front, so
    // the reads and writes see one consistent snapshot.
    super::in_txn(conn, TransactionBehavior::Immediate, || {
        let items = load_items(conn, &repo_id)?;
        let edges = load_closing_edges(conn, &repo_id)?;

        // Index items by their FULL thread identity: within one repo, issue/PR numbers are only
        // unique per (tracker, project) — a repo can mirror several tracker bindings — so keying by
        // kind/key alone would collide same-numbered items across projects.
        let by_key: BTreeMap<ThreadKey, &ItemRow> =
            items.iter().map(|item| (ThreadKey::from(item), item)).collect();
        let plans = plan_records(&items, &edges);
        let records_deleted = clear_stale_threads(conn, &repo_id, &items, &plans)?;

        let mut report = ExtractReport { eligible: plans.len(), ..Default::default() };
        for plan in &plans {
            let written = write_record(conn, &repo_id, now, opts, plan, &by_key, repo.as_ref())?;
            report.records_written += 1;
            report.coalesced_pairs += plan.partners.len();
            report.queued += written.queued;
            match plan.fix_edge_source {
                FixEdgeSource::Provider => report.fix_edge_provider += 1,
                FixEdgeSource::Text => report.fix_edge_text += 1,
                FixEdgeSource::None => report.fix_edge_none += 1,
            }
            match written.mechanical_status {
                OutcomeStatus::Landed => report.mechanical_landed += 1,
                OutcomeStatus::Reverted => report.mechanical_reverted += 1,
                _ => report.mechanical_unclear += 1,
            }
        }
        // A pass that wrote or deleted any record changed papertrail-distill state, so advance the
        // papertrail Lens lane (V108 dropped the row triggers). Once per pass, inside this txn; a
        // no-op pass (no plans, no reconcile deletes) leaves the lane untouched.
        if report.records_written > 0 || records_deleted > 0 {
            super::bump_papertrail_lens_lanes(conn, &repo_id)?;
        }
        Ok(report)
    })
}

/// Plan the records this pass should hold: closed issues (coalescing their merged-PR closers within
/// the same project), then merged PRs that no issue coalesced away.
fn plan_records(items: &[ItemRow], edges: &[ClosingEdgeRow]) -> Vec<RecordPlan> {
    // Merged PRs are keyed without the kind (always change_request).
    let mut merged_prs: BTreeMap<(String, String, String), &ItemRow> = BTreeMap::new();
    let mut closed_issues: Vec<&ItemRow> = Vec::new();
    for item in items {
        match item.kind {
            ItemKind::Issue if item.state_normalized == "closed" => closed_issues.push(item),
            ItemKind::ChangeRequest if item.state_normalized == "merged" => {
                merged_prs
                    .insert((item.tracker.clone(), item.project.clone(), item.key.clone()), item);
            },
            _ => {},
        }
    }

    // Closing edges grouped by the (tracker, project, issue) they close.
    let mut edges_by_issue: BTreeMap<(String, String, String), Vec<&ClosingEdgeRow>> =
        BTreeMap::new();
    for edge in edges {
        edges_by_issue
            .entry((edge.tracker.clone(), edge.project.clone(), edge.issue_key.clone()))
            .or_default()
            .push(edge);
    }

    let mut coalesced_away: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut plans: Vec<RecordPlan> = Vec::new();
    for issue in &closed_issues {
        let issue_edges = edges_by_issue
            .get(&(issue.tracker.clone(), issue.project.clone(), issue.key.clone()))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut fix_shas: BTreeSet<String> = BTreeSet::new();
        let mut partners: BTreeSet<String> = BTreeSet::new();
        let mut source = FixEdgeSource::None;
        // The closing-keyword floor is derived from the canonical parser's output, NOT a
        // re-scan: a TEXT-tier closing edge on this ISSUE means the closer-minting tier matched
        // a closing keyword (provider-aware — GitLab gerunds included — project-scoped, and
        // issue-vs-PR kind-correct, none of which a hand-rolled text scan gets right).
        let mut text_closing = false;
        for edge in issue_edges {
            match edge.closer_kind {
                Some(CloserKind::Commit) => {
                    // A commit closer is an accepted fix edge; upgrade provenance and take the
                    // sha.
                    fix_shas.insert(edge.closer_key.clone());
                    source = stronger_source(source, edge.source);
                    text_closing |= edge.source == Some(ClosingEdgeSource::Text);
                },
                Some(CloserKind::ChangeRequest) => {
                    // Only a MERGED PR in the SAME project is a real coalesce partner +
                    // fix-commit source; a closed-unmerged PR's
                    // merge_commit_sha is a trap (GitHub's ephemeral
                    // test merge), so it never contributes a commit — and its edge must NOT
                    // upgrade provenance, or the no-fix-edge floor
                    // would wrongly not fire.
                    let partner_id =
                        (issue.tracker.clone(), issue.project.clone(), edge.closer_key.clone());
                    if let Some(pr) = merged_prs.get(&partner_id) {
                        partners.insert(edge.closer_key.clone());
                        coalesced_away.insert(partner_id);
                        source = stronger_source(source, edge.source);
                        text_closing |= edge.source == Some(ClosingEdgeSource::Text);
                        if let Some(commit) =
                            edge.closer_commit.clone().or_else(|| pr.merge_commit_sha.clone())
                        {
                            fix_shas.insert(commit);
                        }
                    }
                },
                None => {},
            }
        }
        plans.push(RecordPlan {
            tracker: issue.tracker.clone(),
            project: issue.project.clone(),
            kind: ItemKind::Issue,
            key: issue.key.clone(),
            partners: partners.into_iter().collect(),
            fix_shas: fix_shas.into_iter().collect(),
            fix_edge_source: source,
            text_closing,
        });
    }
    for pr in merged_prs.values() {
        if coalesced_away.contains(&(pr.tracker.clone(), pr.project.clone(), pr.key.clone())) {
            continue;
        }
        // A standalone merged PR is its own provider-attested closure; its merge commit is the
        // fix.
        let fix_shas = pr.merge_commit_sha.clone().into_iter().collect();
        plans.push(RecordPlan {
            tracker: pr.tracker.clone(),
            project: pr.project.clone(),
            kind: ItemKind::ChangeRequest,
            key: pr.key.clone(),
            partners: Vec::new(),
            fix_shas,
            fix_edge_source: FixEdgeSource::Provider,
            // The closing-keyword floor is an issue concept; a standalone merged PR's
            // landed-ness is carried by fix_edge_source, not a closing keyword.
            text_closing: false,
        });
    }
    plans
}

/// Reconcile persisted records and queue rows against this pass's `plans`, deleting what is stale.
/// Returns how many records were deleted.
fn clear_stale_threads(
    conn: &Connection,
    repo_id: &str,
    items: &[ItemRow],
    plans: &[RecordPlan],
) -> anyhow::Result<usize> {
    // The full set of records that SHOULD exist after this pass.
    let planned: BTreeSet<ThreadKey> = plans.iter().map(ThreadKey::from).collect();

    // The threads this device actually mirrors. A record whose thread is ABSENT here is "no
    // opinion, never delete" — not "delete": once `distill/1` replicates these records (#1135),
    // `delete_record` authors a producer `Remove`, so a thin/empty-mirror device (fresh
    // enrollment, or a roster peer without tracker credentials — exactly whom the roster-only
    // scope serves) would otherwise reconcile its empty plan into Removes that wipe the fleet's
    // distilled records. Only a thread STILL mirrored but no longer eligible is stale here.
    let mirrored: BTreeSet<ThreadKey> = items.iter().map(ThreadKey::from).collect();

    // Reconcile against the planned set: a persisted record whose thread is still mirrored but
    // no longer planned — a reopened issue, an un-merged PR, or a PR now coalesced into
    // an issue — loses its stale record/junctions/queue so consumers never see an
    // ineligible or duplicate record. A record whose thread the mirror no longer
    // carries is left untouched (see above).
    let mut records_deleted = 0usize;
    for existing in load_record_keys(conn, repo_id)? {
        if mirrored.contains(&existing) && !planned.contains(&existing) {
            delete_record(conn, repo_id, &existing)?;
            records_deleted += 1;
        }
    }
    // The cheap sync enqueue queues eligible PRs BEFORE extraction runs, so a thread queued but
    // never recorded (a PR extraction now coalesces, or a thread that became ineligible first)
    // leaves a queue row `delete_record` never touched. Drop any queue key not in the plan so a
    // later drain never processes a duplicate coalesced PR or an ineligible thread.
    for queued in load_queue_keys(conn, repo_id)? {
        if !planned.contains(&queued) {
            conn.execute(
                &format!("DELETE FROM papertrail_distill_queue WHERE {}", thread::THREAD_KEY_WHERE),
                queued.params(repo_id),
            )?;
        }
    }
    Ok(records_deleted)
}

/// The stronger of the current fix-edge source and a newly seen closing-edge source: provider
/// outranks text outranks none. An unknown source token (`None`) is treated as text (a mined tier).
fn stronger_source(current: FixEdgeSource, seen: Option<ClosingEdgeSource>) -> FixEdgeSource {
    match (current, seen) {
        (FixEdgeSource::Provider, _) | (_, Some(ClosingEdgeSource::Provider)) =>
            FixEdgeSource::Provider,
        _ => FixEdgeSource::Text,
    }
}

struct WriteOutcome {
    queued: usize,
    mechanical_status: OutcomeStatus,
}

#[derive(Debug)]
struct ThreadSnapshot {
    sources: Vec<SnapshotSource>,
    units: Vec<SnapshotUnit>,
}

#[derive(Debug)]
struct SnapshotSource {
    ordinal: usize,
    role: SourceRole,
    partner_ordinal: Option<usize>,
    item_kind: ItemKind,
    item_key: String,
    kind: SourceKind,
    part: SourcePart,
    /// Item sources use their item key; comment sources use the provider-qualified comment id.
    /// Together with `(source_item_kind, source_item_key, source_kind, source_part)` this is an
    /// unambiguous identity even when title and body text are byte-identical.
    id: String,
    exact_text: String,
    author: Option<String>,
    author_kind: Option<String>,
    author_association: Option<String>,
    created_at_ms: Option<i64>,
}

#[derive(Debug)]
struct SnapshotUnit {
    ordinal: usize,
    source_ordinal: usize,
    span: units::Span,
}

/// Assemble and persist one record: units → hash, fixing commits, coalesced edges, anchor
/// candidates, status floors, the skeleton row, and the queue entry.
fn write_record(
    conn: &Connection,
    repo_id: &str,
    now: i64,
    opts: &ExtractOptions,
    plan: &RecordPlan,
    by_key: &BTreeMap<ThreadKey, &ItemRow>,
    repo: Option<&gix::Repository>,
) -> anyhow::Result<WriteOutcome> {
    let thread_key = ThreadKey::from(plan);
    let item = by_key.get(&thread_key).copied().ok_or_else(|| {
        anyhow::anyhow!("record item {}#{} vanished mid-pass", plan.kind.as_db_str(), plan.key)
    })?;

    // Snapshot exact source rows before deriving anything lossy. The snapshot, its hash, and the
    // skeleton are committed in this extraction transaction, so later mirror LWW edits cannot make
    // a model citation point at different bytes.
    let snapshot = build_thread_snapshot(conn, repo_id, plan, item, by_key)?;
    // Body length spans the whole coalesced thread (issue + partner PRs), so a thin issue body with
    // a substantial partner PR is not misclassified `thin`.
    let mut body_len = item.body.len();
    let record_comments = load_comments(conn, repo_id, &thread_key)?;
    let mut total_comments = record_comments.len();
    let mut review_comments = record_comments.iter().filter(|c| c.is_review).count();
    for partner in &plan.partners {
        let partner_key = plan.partner_key(partner);
        if let Some(pr) = by_key.get(&partner_key) {
            body_len += pr.body.len();
        }
        let partner_comments = load_comments(conn, repo_id, &partner_key)?;
        total_comments += partner_comments.len();
        review_comments += partner_comments.iter().filter(|c| c.is_review).count();
    }

    let changed_paths = changed_paths_for(conn, repo_id, repo, &plan.fix_shas)?;
    // The closing-keyword floor comes from the canonical parser's text-tier closing edge
    // (plan.text_closing), not a re-scan of commit text — a marker string, since the specific
    // keyword isn't load-bearing.
    let closing_keyword: Option<&str> = plan.text_closing.then_some("closing");
    let revert_override = detect_revert_override(conn, repo_id, plan)?;

    // --- Anchor candidates from the changed source files. "Qualified" counts resolved SYMBOL
    // anchors (bound to a `sym_<hex>` logical id) — the precise, high-value bindings — separately
    // from coarser file anchors (which are also resolved but tracked as their own rate).
    let anchors = candidates::mine_anchor_candidates(conn, &changed_paths, opts.anchor_caps)?;
    let anchors_qualified = anchors
        .iter()
        .filter(|a| a.resolved && matches!(a.kind, candidates::AnchorKind::Symbol))
        .count();

    // Enriched context (#800), snapshotted in this same transaction so the drain never reads
    // mutable git/mirror state: the titles + opening paragraphs of items this thread's outbound
    // refs name are MUTABLE mirror rows and must be frozen here. The fix diff is different: it is
    // a pure function of the (already-hashed) fix SHAs and anchor candidates, so it is NOT part of
    // the input identity — folding rendered patch bytes in would tie the identity to git object
    // AVAILABILITY (a bare-index or shallow run would flip every hash, destroy good snapshots, and
    // re-pay the model) and force a full re-render of every record's diffs on every pass inside
    // this write transaction. It is rendered lazily below, only when the record's identity changed
    // or its rows are missing.
    let xrefs = xref_snapshots(conn, repo_id, plan, &snapshot)?;

    let thread_shape = validate::classify_thread_shape(total_comments, review_comments, body_len);

    // The mechanical effective status (model absent) — a floors-only preview for the report. The
    // resolver lives in the read layer (`rag_rat_papertrail`, #705); extraction reuses it here.
    let mechanical_status =
        rag_rat_papertrail::effective_status(&rag_rat_papertrail::EffectiveStatusInputs {
            revert_override,
            closing_keyword: closing_keyword.is_some(),
            fix_edge_source: plan.fix_edge_source,
            model_status: None,
        });

    let input_hash = compute_input_hash(&HashInputs {
        pipeline_version: opts.pipeline_version,
        repo_id,
        plan,
        snapshot: &snapshot,
        changed_paths: &changed_paths,
        thread_shape: thread_shape.as_db_str(),
        revert_override,
        closing_keyword,
        anchors: &anchors,
        xrefs: &xrefs,
    });

    // Record state drives model invalidation + enqueue. New/regenerated records need inference; an
    // unchanged record keeps its result unless the prompt contract changed. Input or prompt changes
    // clear every model-owned field before requeueing so stale findings never remain visible.
    let state = record_state(conn, repo_id, &thread_key, &input_hash, opts.pipeline_version)?;
    let prompt_changed = state == RecordState::Unchanged
        && match stored_prompt_version(conn, repo_id, &thread_key)? {
            Some(version) => version != prompts::PROMPT_VERSION,
            // A queued NULL-stamped row is pending or previously failed, not legacy completed
            // output. Preserve its attempt diagnostics; the drain always renders the current
            // prompt. A NULL-stamped row with no queue entry needs recovery/reprocessing.
            None => !queue_entry_exists(conn, repo_id, &thread_key)?,
        };

    let queued = persist_record(conn, repo_id, now, opts, repo, RecordWrite {
        plan,
        thread_key: &thread_key,
        item,
        state,
        prompt_changed,
        facets: SkeletonFacets {
            input_hash: &input_hash,
            fix_edge_source: plan.fix_edge_source,
            anchors_qualified,
            thread_shape: thread_shape.as_db_str(),
            revert_override,
            closing_keyword,
        },
        snapshot: &snapshot,
        xrefs: &xrefs,
        anchors: &anchors,
    })?;
    Ok(WriteOutcome { queued, mechanical_status })
}

/// Whether a landed commit reverts one of `plan`'s CURRENT fixing commits.
///
/// Revert detection is causal, not timestamp-ordered, and NEVER keys on the fix commit itself
/// being a `Revert` (that is intentional revert work that LANDED — not this record being
/// reverted). The floor fires ONLY when a downstream landed commit reverts one of THIS record's
/// CURRENT fixing commits: git's revert body names the reverted commit ("This reverts commit
/// <sha>"), so a reopen→revert→re-fix leaves the stale revert pointing at the OLD (replaced) fix
/// sha — not in `fix_shas` — and correctly does not flip. A revert can name the record's OWN
/// thread OR a coalesced partner PR (GitHub's revert says "Reverts …#<pr>"), so gather from
/// both.
fn detect_revert_override(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
) -> anyhow::Result<bool> {
    let mut revert_shas =
        revert_commit_shas(conn, repo_id, &plan.tracker, &plan.project, plan.kind, &plan.key)?;
    for partner in &plan.partners {
        revert_shas.extend(revert_commit_shas(
            conn,
            repo_id,
            &plan.tracker,
            &plan.project,
            ItemKind::ChangeRequest,
            partner,
        )?);
    }
    for revert_sha in &revert_shas {
        if let Some(commit) = commit_message(conn, repo_id, revert_sha)? {
            let text = format!("{}\n{}", commit.subject, commit.body);
            if plan.fix_shas.iter().any(|fix| text.contains(fix.as_str())) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Everything [`persist_record`] writes for one thread, derived by [`write_record`].
struct RecordWrite<'a> {
    plan: &'a RecordPlan,
    thread_key: &'a ThreadKey,
    item: &'a ItemRow,
    state: RecordState,
    prompt_changed: bool,
    facets: SkeletonFacets<'a>,
    snapshot: &'a ThreadSnapshot,
    xrefs: &'a [XrefSnapshot],
    anchors: &'a [candidates::AnchorCandidate],
}

/// Persist one record: rebuild this thread's mechanical junctions, upsert the skeleton row
/// (clearing model columns on regeneration), rewrite junctions, queue. Returns the queue rows
/// written.
fn persist_record(
    conn: &Connection,
    repo_id: &str,
    now: i64,
    opts: &ExtractOptions,
    repo: Option<&gix::Repository>,
    write: RecordWrite<'_>,
) -> anyhow::Result<usize> {
    let RecordWrite {
        plan,
        thread_key,
        item,
        state,
        prompt_changed,
        facets,
        snapshot,
        xrefs,
        anchors,
    } = write;
    let invalidate_model = state == RecordState::Regenerated || prompt_changed;
    let rebuild_anchor_candidates = state != RecordState::Unchanged;
    clear_mechanical_junctions(conn, repo_id, thread_key, rebuild_anchor_candidates)?;
    // Extraction or prompt identity changed: an identical rerun keeps the model's work, but a
    // requeued record must expose no stale evidence, alternatives, or anchor selections.
    if invalidate_model {
        thread::clear_model_junctions(conn, repo_id, thread_key)?;
        thread::deselect_anchors(conn, repo_id, thread_key)?;
    }
    upsert_skeleton(conn, repo_id, now, opts, plan, invalidate_model, &facets)?;
    if state != RecordState::Unchanged {
        replace_snapshot(conn, repo_id, thread_key, snapshot)?;
        replace_xrefs(conn, repo_id, thread_key, xrefs)?;
    }
    // The fix-diff snapshot is rebuilt when the identity changed, and SELF-HEALED when an earlier
    // pass ran without a usable repo handle (bare/copied index, shallow clone): the rows are a
    // pure function of hashed inputs, so filling them late cannot invalidate anything. A record
    // whose rendering legitimately yields zero rows (all binary/missing) re-attempts each pass —
    // the cheap edge of this posture.
    let diff_heal = state == RecordState::Unchanged
        && repo.is_some()
        && anchors
            .iter()
            .any(|a| matches!(a.kind, candidates::AnchorKind::Symbol) && a.file_path.is_some())
        && !fix_diff_rows_exist(conn, repo_id, thread_key)?;
    if state != RecordState::Unchanged || diff_heal {
        let fix_diffs = fix_diff_snapshots(repo, &plan.fix_shas, anchors);
        replace_fix_diffs(conn, repo_id, thread_key, &fix_diffs)?;
    }
    write_commits(conn, repo_id, now, plan, &plan.fix_shas)?;
    write_coalesced_edges(conn, repo_id, now, plan)?;
    if rebuild_anchor_candidates {
        write_anchors(conn, repo_id, plan, anchors)?;
    }
    if matches!(state, RecordState::New | RecordState::Regenerated) || prompt_changed {
        enqueue_one(conn, repo_id, now, item, invalidate_model)
    } else {
        Ok(0)
    }
}

fn stored_prompt_version(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
) -> anyhow::Result<Option<u32>> {
    let version = conn.query_row(
        &format!(
            "SELECT prompt_version FROM papertrail_distill WHERE {}",
            thread::THREAD_KEY_WHERE
        ),
        thread_key.params(repo_id),
        |row| row.get::<_, Option<i64>>(0),
    )?;
    version.map(u32::try_from).transpose().map_err(Into::into)
}

fn queue_entry_exists(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
) -> anyhow::Result<bool> {
    conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM papertrail_distill_queue WHERE {})",
            thread::THREAD_KEY_WHERE
        ),
        thread_key.params(repo_id),
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Whether the thread already has snapshotted fix-diff rows (#800) — the self-heal probe for a
/// record first extracted without a usable repo handle.
fn fix_diff_rows_exist(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
) -> anyhow::Result<bool> {
    conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM papertrail_distill_fix_diffs WHERE {})",
            thread::THREAD_KEY_WHERE
        ),
        thread_key.params(repo_id),
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Everything that folds into a record's regeneration identity.
struct HashInputs<'a> {
    pipeline_version: i64,
    repo_id: &'a str,
    plan: &'a RecordPlan,
    snapshot: &'a ThreadSnapshot,
    changed_paths: &'a [String],
    thread_shape: &'a str,
    revert_override: bool,
    closing_keyword: Option<&'a str>,
    anchors: &'a [candidates::AnchorCandidate],
    xrefs: &'a [XrefSnapshot],
}

/// The regeneration identity: pipeline version, full record identity, the complete exact source
/// snapshot and every unit span, sorted changed-file selection, fix-edge source + SHAs, and the
/// computed mechanical status floors. Prompt budgeting is deliberately absent: truncation cannot
/// hide a mutable source edit from regeneration.
fn compute_input_hash(inputs: &HashInputs<'_>) -> String {
    let HashInputs {
        pipeline_version,
        repo_id,
        plan,
        snapshot,
        changed_paths,
        thread_shape,
        revert_override,
        closing_keyword,
        anchors,
        xrefs,
    } = inputs;
    let mut hasher = Sha256::new();
    hash_str(&mut hasher, "rag-rat-distill-input-v3");
    hasher.update(pipeline_version.to_le_bytes());
    hash_str(&mut hasher, repo_id);
    hash_str(&mut hasher, &plan.tracker);
    hash_str(&mut hasher, &plan.project);
    hash_str(&mut hasher, plan.kind.as_db_str());
    hash_str(&mut hasher, &plan.key);
    hash_str(&mut hasher, "sources");
    hasher.update((snapshot.sources.len() as u64).to_le_bytes());
    for source in &snapshot.sources {
        hasher.update((source.ordinal as u64).to_le_bytes());
        hash_str(&mut hasher, source.role.as_db_str());
        hash_optional_u64(&mut hasher, source.partner_ordinal.map(|value| value as u64));
        hash_str(&mut hasher, source.item_kind.as_db_str());
        hash_str(&mut hasher, &source.item_key);
        hash_str(&mut hasher, source.kind.as_db_str());
        hash_str(&mut hasher, source.part.as_db_str());
        hash_str(&mut hasher, &source.id);
        hash_str(&mut hasher, &source.exact_text);
        hash_optional_str(&mut hasher, source.author.as_deref());
        hash_optional_str(&mut hasher, source.author_kind.as_deref());
        hash_optional_str(&mut hasher, source.author_association.as_deref());
        hash_optional_i64(&mut hasher, source.created_at_ms);
    }
    hash_str(&mut hasher, "units");
    hasher.update((snapshot.units.len() as u64).to_le_bytes());
    for unit in &snapshot.units {
        hasher.update((unit.ordinal as u64).to_le_bytes());
        hasher.update((unit.source_ordinal as u64).to_le_bytes());
        hasher.update((unit.span.start as u64).to_le_bytes());
        hasher.update((unit.span.end as u64).to_le_bytes());
    }
    hash_str(&mut hasher, "changed_paths");
    let mut sorted = changed_paths.to_vec();
    sorted.sort();
    hasher.update((sorted.len() as u64).to_le_bytes());
    for path in sorted {
        hash_str(&mut hasher, &path);
    }
    // Mechanical fix-edge inputs + computed status floors: a changed closing edge / merge SHA (same
    // files), a flipped provenance tier, or a floor that flips later must invalidate the model's
    // decision/outcome even when the thread text is identical.
    hash_str(&mut hasher, "status_inputs");
    hash_str(&mut hasher, plan.fix_edge_source.as_db_str());
    hash_str(&mut hasher, thread_shape);
    hasher.update([*revert_override as u8]);
    hash_optional_str(&mut hasher, *closing_keyword);
    hash_str(&mut hasher, "fix_commits");
    let mut shas = plan.fix_shas.clone();
    shas.sort();
    hasher.update((shas.len() as u64).to_le_bytes());
    for sha in shas {
        hash_str(&mut hasher, &sha);
    }
    // Anchor candidate set: #704 selects anchors from this pool, so a reindex that resolves new
    // logical symbols (candidates absent at first extraction, present after) must re-queue the
    // record. Hash each candidate's identity; mining order is already deterministic.
    hash_str(&mut hasher, "anchors");
    hasher.update((anchors.len() as u64).to_le_bytes());
    for anchor in *anchors {
        hash_str(&mut hasher, anchor.kind.as_db_str());
        hash_optional_str(&mut hasher, anchor.logical_symbol_id.as_deref());
        hash_optional_str(&mut hasher, anchor.file_path.as_deref());
        hash_str(&mut hasher, &anchor.name);
        hasher.update([anchor.resolved as u8]);
    }
    // Enriched-context snapshots (#800). The xref rows carry MUTABLE mirror text (a referenced
    // item's edited title/opening) that must regenerate the record exactly like a primary-source
    // edit. The fix diff is deliberately ABSENT: it is a pure function of the hashed fix SHAs and
    // anchor candidates, so hashing rendered patches would only couple the identity to git object
    // availability; renderer changes ride PIPELINE_VERSION instead.
    hash_str(&mut hasher, "xrefs");
    hasher.update((xrefs.len() as u64).to_le_bytes());
    for xref in *xrefs {
        hasher.update((xref.ordinal as u64).to_le_bytes());
        hash_str(&mut hasher, &xref.target_tracker);
        hash_str(&mut hasher, &xref.target_project);
        hash_optional_str(&mut hasher, xref.target_item_kind.as_deref());
        hash_str(&mut hasher, &xref.target_item_key);
        hash_str(&mut hasher, &xref.ref_kind);
        hash_str(&mut hasher, &xref.title);
        hash_str(&mut hasher, &xref.opening);
    }
    let hex: String = rag_rat_base::hash::hex_lower(&hasher.finalize());
    format!("sha256:{hex}")
}

fn hash_str(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn hash_optional_str(hasher: &mut Sha256, value: Option<&str>) {
    hasher.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        hash_str(hasher, value);
    }
}

fn hash_optional_i64(hasher: &mut Sha256, value: Option<i64>) {
    hasher.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        hasher.update(value.to_le_bytes());
    }
}

fn hash_optional_u64(hasher: &mut Sha256, value: Option<u64>) {
    hasher.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        hasher.update(value.to_le_bytes());
    }
}

fn build_thread_snapshot(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
    primary: &ItemRow,
    by_key: &BTreeMap<ThreadKey, &ItemRow>,
) -> anyhow::Result<ThreadSnapshot> {
    let mut snapshot = ThreadSnapshot { sources: Vec::new(), units: Vec::new() };
    append_item_snapshot(
        &mut snapshot,
        SourceRole::Primary,
        None,
        primary,
        load_comments(conn, repo_id, &ThreadKey::from(primary))?,
    );
    for (partner_ordinal, partner_key) in plan.partners.iter().enumerate() {
        let partner = by_key.get(&plan.partner_key(partner_key)).copied().ok_or_else(|| {
            anyhow::anyhow!("coalesced partner change_request#{partner_key} vanished mid-pass")
        })?;
        append_item_snapshot(
            &mut snapshot,
            SourceRole::Partner,
            Some(partner_ordinal),
            partner,
            load_comments(conn, repo_id, &ThreadKey::from(partner))?,
        );
    }
    Ok(snapshot)
}

fn append_item_snapshot(
    snapshot: &mut ThreadSnapshot,
    role: SourceRole,
    partner_ordinal: Option<usize>,
    item: &ItemRow,
    comments: Vec<CommentRow>,
) {
    for (part, exact_text) in
        [(SourcePart::Title, item.title.clone()), (SourcePart::Body, item.body.clone())]
    {
        append_snapshot_source(snapshot, SnapshotSource {
            ordinal: snapshot.sources.len(),
            role,
            partner_ordinal,
            item_kind: item.kind,
            item_key: item.key.clone(),
            kind: SourceKind::Item,
            part,
            id: item.key.clone(),
            exact_text,
            author: item.author.clone(),
            author_kind: item.author_kind.clone(),
            author_association: item.author_association.clone(),
            created_at_ms: item.created_at_ms,
        });
    }
    for comment in comments {
        append_snapshot_source(snapshot, SnapshotSource {
            ordinal: snapshot.sources.len(),
            role,
            partner_ordinal,
            item_kind: item.kind,
            item_key: item.key.clone(),
            kind: SourceKind::Comment,
            part: SourcePart::Comment,
            id: comment.comment_id,
            exact_text: comment.body,
            author: comment.author,
            author_kind: comment.author_kind,
            author_association: comment.author_association,
            created_at_ms: comment.created_at_ms,
        });
    }
}

fn append_snapshot_source(snapshot: &mut ThreadSnapshot, source: SnapshotSource) {
    let source_ordinal = source.ordinal;
    for span in units::segment_blocks(&source.exact_text) {
        snapshot.units.push(SnapshotUnit { ordinal: snapshot.units.len(), source_ordinal, span });
    }
    snapshot.sources.push(source);
}

// ── Loaders ────────────────────────────────────────────────────────────────────────────────────

struct ItemRow {
    kind: ItemKind,
    key: String,
    tracker: String,
    project: String,
    title: String,
    body: String,
    state_normalized: String,
    merge_commit_sha: Option<String>,
    author: Option<String>,
    author_kind: Option<String>,
    author_association: Option<String>,
    created_at_ms: Option<i64>,
}

fn load_items(conn: &Connection, repo_id: &str) -> anyhow::Result<Vec<ItemRow>> {
    let mut stmt = conn.prepare(
        "SELECT item_kind, item_key, tracker, project, title, body, state_normalized,
                merge_commit_sha, author, author_kind, author_association,
                CASE WHEN created_at IS NULL THEN NULL ELSE
                    CAST(strftime('%s', created_at) AS INTEGER) * 1000 +
                    CAST(substr(strftime('%f', created_at), 4, 3) AS INTEGER)
                END
         FROM papertrail_items WHERE repo_id = ?1",
    )?;
    let rows = stmt.query_map([repo_id], |row| {
        Ok(ItemRow {
            kind: ItemKind::from_db_str(&row.get::<_, String>(0)?)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?,
            key: row.get(1)?,
            tracker: row.get(2)?,
            project: row.get(3)?,
            title: row.get(4)?,
            body: row.get(5)?,
            state_normalized: row.get(6)?,
            merge_commit_sha: row.get(7)?,
            author: row.get(8)?,
            author_kind: row.get(9)?,
            author_association: row.get(10)?,
            created_at_ms: row.get(11)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A closing edge with its closed-set tokens parsed. A token outside the set reads as `None` rather
/// than an error: an unknown closer kind contributes nothing and an unknown source ranks as text.
struct ClosingEdgeRow {
    tracker: String,
    project: String,
    issue_key: String,
    closer_kind: Option<CloserKind>,
    closer_key: String,
    closer_commit: Option<String>,
    source: Option<ClosingEdgeSource>,
}

fn load_closing_edges(conn: &Connection, repo_id: &str) -> anyhow::Result<Vec<ClosingEdgeRow>> {
    let mut stmt = conn.prepare(
        "SELECT tracker, project, issue_key, closer_kind, closer_key, closer_commit, source
         FROM papertrail_closing_edges WHERE repo_id = ?1 AND issue_kind = 'issue'",
    )?;
    let rows = stmt.query_map([repo_id], |row| {
        Ok(ClosingEdgeRow {
            tracker: row.get(0)?,
            project: row.get(1)?,
            issue_key: row.get(2)?,
            closer_kind: CloserKind::from_db_str(&row.get::<_, String>(3)?).ok(),
            closer_key: row.get(4)?,
            closer_commit: row.get(5)?,
            source: ClosingEdgeSource::from_db_str(&row.get::<_, String>(6)?).ok(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

struct CommentRow {
    comment_id: String,
    body: String,
    is_review: bool,
    author: Option<String>,
    author_kind: Option<String>,
    author_association: Option<String>,
    created_at_ms: Option<i64>,
}

fn load_comments(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
) -> anyhow::Result<Vec<CommentRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT comment_id, body, review_state, author, author_kind, author_association,
                CASE WHEN created_at IS NULL THEN NULL ELSE
                    CAST(strftime('%s', created_at) AS INTEGER) * 1000 +
                    CAST(substr(strftime('%f', created_at), 4, 3) AS INTEGER)
                END
         FROM papertrail_comments
         WHERE {}
         ORDER BY created_at, comment_id",
        thread::THREAD_KEY_WHERE
    ))?;
    let rows = stmt.query_map(thread_key.params(repo_id), |row| {
        Ok(CommentRow {
            comment_id: row.get(0)?,
            body: row.get(1)?,
            is_review: row.get::<_, Option<String>>(2)?.is_some(),
            author: row.get(3)?,
            author_kind: row.get(4)?,
            author_association: row.get(5)?,
            created_at_ms: row.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn changed_paths_for(
    conn: &Connection,
    repo_id: &str,
    repo: Option<&gix::Repository>,
    shas: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT path FROM git_file_changes WHERE commit_hash = ?1 AND repo_id = ?2",
    )?;
    for sha in shas {
        let mut found = false;
        let rows = stmt.query_map(params![sha, repo_id], |row| row.get::<_, String>(0))?;
        for row in rows {
            paths.insert(row?);
            found = true;
        }
        // A real MERGE commit carries NO `git_file_changes` rows (the history index records the
        // first-parent diff only for scope, not per file — git numstat has no merge diff). But
        // `merge_commit_sha` is the usual fixing SHA for merged PRs, so fall back to a live gix
        // first-parent diff to recover its changed source files for anchor mining.
        if !found && let Some(repo) = repo {
            paths.extend(merge_first_parent_paths(repo, sha).unwrap_or_default());
        }
    }
    Ok(paths.into_iter().collect())
}

/// The paths a commit changed vs its FIRST parent, via a live gix tree diff — the recovery path for
/// merge commits (which the history index leaves out of `git_file_changes`). Best-effort: any gix
/// error (unparseable sha, missing object on a shallow clone) yields no paths rather than failing
/// the pass. Paths are worktree-root-relative, matching `files.path` for a full-repo index (a
/// subtree index simply won't match them in the `files` lookup — same as before, never wrong).
fn merge_first_parent_paths(repo: &gix::Repository, sha: &str) -> anyhow::Result<Vec<String>> {
    use gix::object::tree::diff::Action;
    let Ok(id) = gix::ObjectId::from_hex(sha.as_bytes()) else {
        return Ok(Vec::new());
    };
    let Ok(commit) = repo.find_commit(id) else {
        return Ok(Vec::new());
    };
    let new_tree = commit.tree()?;
    // A parent id with a MISSING object is a shallow boundary, not a root commit — diffing against
    // the empty tree would report every file in the repo as added. Skip the commit instead.
    let parent_tree = match commit.parent_ids().next() {
        Some(parent) => repo.find_commit(parent.detach()).ok().and_then(|p| p.tree().ok()),
        None => Some(repo.empty_tree()),
    };
    let Some(parent_tree) = parent_tree else { return Ok(Vec::new()) };
    let mut paths = Vec::new();
    parent_tree
        .changes()?
        .options(|opts| {
            opts.track_path();
        })
        .for_each_to_obtain_tree(&new_tree, |change| {
            if !change.entry_mode().is_tree() {
                paths.push(change.location().to_string());
            }
            Ok::<_, std::convert::Infallible>(Action::Continue(()))
        })?;
    Ok(paths)
}

struct CommitMeta {
    subject: String,
    body: String,
}

/// One snapshotted per-file patch of a fixing commit (#800): git-style headers plus the unified
/// hunks, capped at [`FIX_DIFF_FILE_CAP`]. Rendered at extraction time and persisted; the drain
/// never opens the repo.
struct FixDiffSnapshot {
    commit_sha: String,
    path: String,
    patch: String,
}

/// One snapshotted cross-referenced item (#800): the outbound ref's target identity (kind as
/// RESOLVED against the mirror) plus the frozen title and opening paragraph the prompt shows.
struct XrefSnapshot {
    ordinal: usize,
    target_tracker: String,
    target_project: String,
    target_item_kind: Option<String>,
    target_item_key: String,
    ref_kind: String,
    title: String,
    opening: String,
}

/// The fix diff, "capped by files with symbol candidates" (#800): per fixing commit, the unified
/// patch of every changed file that yielded a SYMBOL anchor candidate (mining already excluded
/// test/generated churn and unindexed paths). Git content is immutable, so this is a determinism
/// snapshot (drain stays DB-only), not a mutability one — best-effort like the merge path
/// recovery: an unresolvable sha, shallow clone, binary file, or driver-skipped diff contributes
/// nothing rather than failing the pass.
fn fix_diff_snapshots(
    repo: Option<&gix::Repository>,
    fix_shas: &[String],
    anchors: &[candidates::AnchorCandidate],
) -> Vec<FixDiffSnapshot> {
    let Some(repo) = repo else { return Vec::new() };
    let symbol_paths: BTreeSet<&str> = anchors
        .iter()
        .filter(|anchor| matches!(anchor.kind, candidates::AnchorKind::Symbol))
        .filter_map(|anchor| anchor.file_path.as_deref())
        .collect();
    if symbol_paths.is_empty() {
        return Vec::new();
    }
    let Ok(mut diff_cache) = repo.diff_resource_cache_for_tree_diff() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sha in fix_shas {
        diff_cache.clear_resource_cache();
        let _ = collect_commit_diffs(repo, sha, &symbol_paths, &mut diff_cache, &mut out);
    }
    out
}

/// Append one fixing commit's per-file patches (vs its FIRST parent) for the symbol-candidate
/// paths. Best-effort at the call site: any gix error (unparseable sha, missing object on a
/// shallow clone) yields no rows for this commit rather than failing the pass.
fn collect_commit_diffs(
    repo: &gix::Repository,
    sha: &str,
    symbol_paths: &BTreeSet<&str>,
    diff_cache: &mut gix::diff::blob::Platform,
    out: &mut Vec<FixDiffSnapshot>,
) -> anyhow::Result<()> {
    use gix::object::tree::diff::Action;

    let id = gix::ObjectId::from_hex(sha.as_bytes())?;
    let commit = repo.find_commit(id)?;
    let new_tree = commit.tree()?;
    // A parent id with a MISSING object is a shallow boundary, not a root commit — diffing against
    // the empty tree would render a bogus full-tree addition. Skip the commit instead.
    let parent_tree = match commit.parent_ids().next() {
        Some(parent) => repo.find_commit(parent.detach()).ok().and_then(|p| p.tree().ok()),
        None => Some(repo.empty_tree()),
    };
    let Some(parent_tree) = parent_tree else { return Ok(()) };
    parent_tree
        .changes()?
        .options(|opts| {
            opts.track_path();
        })
        .for_each_to_obtain_tree(&new_tree, |change| {
            // Blobs only: a gitlink's submodule SHA is not diffable text, and a symlink's target
            // path is not file content.
            if change.entry_mode().is_blob() {
                let path = change.location().to_string();
                if symbol_paths.contains(path.as_str())
                    && let Some(patch) = render_file_patch(repo, &change, &path, diff_cache)
                {
                    out.push(FixDiffSnapshot { commit_sha: sha.to_string(), path, patch });
                }
            }
            Ok::<_, std::convert::Infallible>(Action::Continue(()))
        })?;
    Ok(())
}

/// Render one changed file's unified patch with git-style headers. `None` for binary/driver-
/// skipped diffs, empty patches (a mode-only change), either side over [`FIX_DIFF_BLOB_CAP`]
/// (the 8k output cap truncates AFTER a full render — a 50MB minified file would otherwise be
/// diffed in memory inside the extraction write transaction), and paths carrying control chars
/// (a newline-bearing filename would split the header lines the drain concatenates). Hunk text
/// is lossy-decoded — deterministic, and the prompt treats it as untrusted display text only.
fn render_file_patch(
    repo: &gix::Repository,
    change: &gix::object::tree::diff::Change<'_, '_, '_>,
    path: &str,
    diff_cache: &mut gix::diff::blob::Platform,
) -> Option<String> {
    use gix::diff::blob::platform::prepare_diff::Operation;
    use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
    use gix::objs::FindHeader;

    if path.chars().any(char::is_control) {
        return None;
    }
    let platform = change.diff(diff_cache).ok()?;
    for resource in
        platform.resource_cache.resources().into_iter().flat_map(|(old, new)| [old, new])
    {
        if resource.id.is_null() {
            continue;
        }
        if let Ok(Some(header)) = repo.try_header(resource.id)
            && header.size > FIX_DIFF_BLOB_CAP
        {
            return None;
        }
    }
    platform.resource_cache.options.skip_internal_diff_if_external_is_configured = false;
    let prep = platform.resource_cache.prepare_diff().ok()?;
    let Operation::InternalDiff { algorithm } = prep.operation else { return None };
    let input = prep.interned_input();
    let diff = gix::diff::blob::diff_with_slider_heuristics(algorithm, &input);
    let hunks = gix::diff::blob::UnifiedDiff::new(
        &diff,
        &input,
        ConsumeBinaryHunk::new(Vec::new(), "\n"),
        ContextSize::symmetrical(3),
    )
    .consume()
    .ok()?;
    let hunks = String::from_utf8_lossy(&hunks);
    if hunks.trim().is_empty() {
        return None;
    }
    let header = match change {
        gix::object::tree::diff::Change::Addition { .. } =>
            format!("diff --git a/{path} b/{path}\n--- /dev/null\n+++ b/{path}\n"),
        gix::object::tree::diff::Change::Deletion { .. } =>
            format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ /dev/null\n"),
        _ => format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n"),
    };
    let patch = format!("{header}{hunks}");
    // The cap covers the WHOLE row, headers included; truncation is deterministic and the drain
    // renders at a tighter budget anyway.
    let mut end = FIX_DIFF_FILE_CAP.min(patch.len());
    while end > 0 && !patch.is_char_boundary(end) {
        end -= 1;
    }
    Some(patch[..end].to_string())
}

/// The thread's cross-referenced items (#800): outbound `papertrail_refs` rows keyed by the
/// SNAPSHOT's own source identities (so the ref set a record sees is exactly the set derivable
/// from its frozen sources), each target resolved against the mirror in this transaction and
/// frozen as title + opening paragraph. Unmirrored targets (foreign projects, never-synced
/// items) contribute nothing — there is no immutable text to freeze. Kind preference when the
/// ref syntax left the kind ambiguous (bare `#N`): the source item's own kind first, matching
/// the parser's namespace inheritance, then a deterministic kind order.
fn xref_snapshots(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
    snapshot: &ThreadSnapshot,
) -> anyhow::Result<Vec<XrefSnapshot>> {
    let mut ref_stmt = conn.prepare(
        "SELECT tracker, project, item_kind, item_key, ref_kind FROM papertrail_refs
         WHERE repo_id = ?1 AND source_kind IN ('item', 'comment') AND source_text = ?2
         ORDER BY id",
    )?;
    let mut seen: BTreeSet<(String, String, String, String)> = BTreeSet::new();
    let mut out = Vec::new();
    'sources: for source in &snapshot.sources {
        let identity = match source.kind {
            SourceKind::Item => format!(
                "{}:{}:{}:{}",
                plan.tracker,
                plan.project,
                source.item_kind.as_db_str(),
                source.item_key
            ),
            SourceKind::Comment => format!(
                "{}:{}:{}:{}:{}",
                plan.tracker,
                plan.project,
                source.item_kind.as_db_str(),
                source.item_key,
                source.id
            ),
        };
        let rows = ref_stmt.query_map(params![repo_id, identity], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (tracker, project, parsed_kind, key, ref_kind) = row?;
            let Some((kind, title, body)) = resolve_xref_target(
                conn,
                repo_id,
                &tracker,
                &project,
                parsed_kind.as_deref(),
                source.item_kind.as_db_str(),
                &key,
            )?
            else {
                continue;
            };
            if !seen.insert((tracker.clone(), project.clone(), kind.clone(), key.clone())) {
                continue;
            }
            // Cap the STORED (and therefore hashed) title/opening to exactly what the prompt
            // renders (`XREF_TEXT_RENDER_CHARS`). Storing more would hash text the model never
            // sees, so a referenced item's edit beyond the rendered width would regenerate the
            // record and re-pay the model with identical visible input. `truncate_chars` is the
            // same idempotent helper the render applies, so stored == rendered (pre-neutralize).
            let opening = units::segment_blocks(&body)
                .first()
                .map(|span| {
                    prompts::truncate_chars(
                        body[span.start..span.end].trim(),
                        prompts::XREF_TEXT_RENDER_CHARS,
                    )
                })
                .unwrap_or_default();
            out.push(XrefSnapshot {
                ordinal: out.len(),
                target_tracker: tracker,
                target_project: project,
                target_item_kind: Some(kind),
                target_item_key: key,
                ref_kind,
                title: prompts::truncate_chars(title.trim(), prompts::XREF_TEXT_RENDER_CHARS),
                opening,
            });
            if out.len() >= XREF_SNAPSHOT_CAP {
                break 'sources;
            }
        }
    }
    Ok(out)
}

/// Resolve an outbound ref's target to a mirrored item: exact kind when the syntax named one,
/// else the source item's kind (parser namespace inheritance), else the deterministically first
/// kind. Returns `(item_kind, title, body)`.
fn resolve_xref_target(
    conn: &Connection,
    repo_id: &str,
    tracker: &str,
    project: &str,
    parsed_kind: Option<&str>,
    source_kind: &str,
    key: &str,
) -> anyhow::Result<Option<(String, String, String)>> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(kind) = parsed_kind {
        candidates.push(kind);
    } else {
        candidates.push(source_kind);
        for kind in [ItemKind::ChangeRequest.as_db_str(), ItemKind::Issue.as_db_str()] {
            if kind != source_kind {
                candidates.push(kind);
            }
        }
    }
    for kind in candidates {
        let resolved = conn
            .query_row(
                "SELECT item_kind, title, body FROM papertrail_items
                 WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_key = ?4
                   AND item_kind = ?5",
                params![repo_id, tracker, project, key, kind],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        if resolved.is_some() {
            return Ok(resolved);
        }
    }
    Ok(None)
}

fn commit_message(
    conn: &Connection,
    repo_id: &str,
    sha: &str,
) -> anyhow::Result<Option<CommitMeta>> {
    Ok(conn
        .query_row(
            "SELECT subject, body FROM git_commits WHERE hash = ?1 AND repo_id = ?2",
            params![sha, repo_id],
            |row| Ok(CommitMeta { subject: row.get(0)?, body: row.get(1)? }),
        )
        .optional()?)
}

/// The LANDED reverting-commit SHAs (`source_kind='commit'` reverts refs whose commit is still in
/// git history) that name this thread. Kind-matched (a typed `/pull/5` revert must not flag issue
/// #5; an UNKNOWN-kind ref is ambiguous and applies to either same-numbered item). A text claim in
/// an item/comment body or an open PR is NOT here — only a landed commit. Callers confirm the
/// commit actually reverts a CURRENT fix before flipping status (ordering is by ancestry-of-fix,
/// not by unreliable commit timestamps).
fn revert_commit_shas(
    conn: &Connection,
    repo_id: &str,
    tracker: &str,
    project: &str,
    kind: ItemKind,
    key: &str,
) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT r.source_commit FROM papertrail_refs r
         WHERE r.repo_id = ?1 AND r.tracker = ?2 AND r.project = ?3 AND r.item_key = ?4
           AND (r.item_kind = ?5 OR r.item_kind IS NULL)
           AND r.ref_kind = 'reverts' AND r.source_kind = 'commit'
           AND r.source_commit IS NOT NULL
           AND EXISTS (
               SELECT 1 FROM git_commits gc WHERE gc.repo_id = ?1 AND gc.hash = r.source_commit)",
    )?;
    let rows = stmt
        .query_map(params![repo_id, tracker, project, key, kind.as_db_str()], |row| {
            row.get::<_, String>(0)
        })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The state of a thread's existing distill record relative to a freshly computed identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordState {
    /// No record yet — a first distillation. Enqueue it.
    New,
    /// A record exists with the SAME (hash, pipeline version) — nothing changed. Do NOT re-enqueue
    /// (that would undo the drain and re-pay the LLM cost) and preserve the model's output.
    Unchanged,
    /// A record exists with a DIFFERENT identity — the input regenerated. Enqueue it and invalidate
    /// the stale model output.
    Regenerated,
}

fn record_state(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
    new_hash: &str,
    new_version: i64,
) -> anyhow::Result<RecordState> {
    let existing: Option<(String, i64)> = conn
        .query_row(
            &format!(
                "SELECT distill_input_hash, pipeline_version FROM papertrail_distill WHERE {}",
                thread::THREAD_KEY_WHERE
            ),
            thread_key.params(repo_id),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(match existing {
        None => RecordState::New,
        Some((hash, version)) if hash != new_hash || version != new_version =>
            RecordState::Regenerated,
        Some(_) => RecordState::Unchanged,
    })
}

/// Delete a thread's distill record, all its junctions (mechanical AND model), and its queue entry
/// — used to reconcile a record whose source thread is no longer eligible (reopened issue,
/// un-merged PR) or that a later sync discovered is actually coalesced into another thread, so
/// consumers never see a stale or duplicate record.
fn delete_record(conn: &Connection, repo_id: &str, record: &ThreadKey) -> anyhow::Result<()> {
    for table in [
        "papertrail_distill",
        "papertrail_distill_record_commits",
        "papertrail_distill_anchors",
        "papertrail_distill_evidence",
        "papertrail_distill_alternatives",
        "papertrail_distill_queue",
        "papertrail_distill_sources",
        "papertrail_distill_units",
        "papertrail_distill_fix_diffs",
        "papertrail_distill_xrefs",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE {}", thread::THREAD_KEY_WHERE),
            record.params(repo_id),
        )?;
    }
    // Delete every edge that TOUCHES this record — as SOURCE or DESTINATION — so no dangling
    // relationship survives to a record that no longer exists (a `supersedes`/`promoted` edge from
    // another record pointing here would otherwise linger). A `coalesced` edge whose destination
    // this was is safely rebuilt by its surviving source issue record on the next pass.
    conn.execute(
        "DELETE FROM papertrail_distill_edges
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3
           AND ( (src_item_kind = ?4 AND src_item_key = ?5)
              OR (dst_item_kind = ?4 AND dst_item_key = ?5) )",
        record.params(repo_id),
    )?;
    Ok(())
}

/// Every distill record's full key currently persisted for `repo_id` — the reconciliation input.
fn load_record_keys(conn: &Connection, repo_id: &str) -> anyhow::Result<Vec<ThreadKey>> {
    load_keys_from(conn, repo_id, "papertrail_distill")
}

/// Every queued thread's full key for `repo_id` — the queue-reconciliation input.
fn load_queue_keys(conn: &Connection, repo_id: &str) -> anyhow::Result<Vec<ThreadKey>> {
    load_keys_from(conn, repo_id, "papertrail_distill_queue")
}

fn load_keys_from(conn: &Connection, repo_id: &str, table: &str) -> anyhow::Result<Vec<ThreadKey>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT tracker, project, item_kind, item_key FROM {table} WHERE repo_id = ?1"
    ))?;
    let rows = stmt.query_map([repo_id], |row| {
        Ok(ThreadKey {
            tracker: row.get(0)?,
            project: row.get(1)?,
            item_kind: row.get(2)?,
            item_key: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ── Writers ────────────────────────────────────────────────────────────────────────────────────

struct SkeletonFacets<'a> {
    input_hash: &'a str,
    fix_edge_source: FixEdgeSource,
    anchors_qualified: usize,
    thread_shape: &'a str,
    revert_override: bool,
    closing_keyword: Option<&'a str>,
}

fn upsert_skeleton(
    conn: &Connection,
    repo_id: &str,
    now: i64,
    opts: &ExtractOptions,
    plan: &RecordPlan,
    invalidate_model: bool,
    facets: &SkeletonFacets<'_>,
) -> anyhow::Result<()> {
    // A fresh row inserts the model columns as NULL (honest nulls) for #704 to fill on this natural
    // key. On conflict the mechanical facets are always (re)written; the model columns are
    // PRESERVED for an identical rerun and NULLED when input or prompt identity changed (`?14`), so
    // a stale decision/outcome never rides current model inputs.
    conn.execute(
        "INSERT INTO papertrail_distill
             (tracker, project, item_kind, item_key, distill_input_hash, pipeline_version,
              fix_edge_source, anchors_qualified_count, thread_shape, revert_override,
              closing_keyword_floor, distilled_at_ms, repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(repo_id, tracker, project, item_kind, item_key) DO UPDATE SET
             distill_input_hash = excluded.distill_input_hash,
             pipeline_version = excluded.pipeline_version,
             fix_edge_source = excluded.fix_edge_source,
             anchors_qualified_count = excluded.anchors_qualified_count,
             thread_shape = excluded.thread_shape,
             revert_override = excluded.revert_override,
             closing_keyword_floor = excluded.closing_keyword_floor,
             distilled_at_ms = excluded.distilled_at_ms,
             root_issue = CASE WHEN ?14 THEN NULL ELSE root_issue END,
             root_cause = CASE WHEN ?14 THEN NULL ELSE root_cause END,
             root_cause_class = CASE WHEN ?14 THEN NULL ELSE root_cause_class END,
             decision_chosen = CASE WHEN ?14 THEN NULL ELSE decision_chosen END,
             outcome_summary = CASE WHEN ?14 THEN NULL ELSE outcome_summary END,
             outcome_status_model = CASE WHEN ?14 THEN NULL ELSE outcome_status_model END,
             epistemic_status_decision =
                 CASE WHEN ?14 THEN NULL ELSE epistemic_status_decision END,
             epistemic_status_outcome = CASE WHEN ?14 THEN NULL ELSE epistemic_status_outcome END,
             quotes_materialized = CASE WHEN ?14 THEN 0 ELSE quotes_materialized END,
             outcome_claim_verified = CASE WHEN ?14 THEN 0 ELSE outcome_claim_verified END,
              decision_provenance_verified =
                  CASE WHEN ?14 THEN 0 ELSE decision_provenance_verified END,
              prompt_version = CASE WHEN ?14 THEN NULL ELSE prompt_version END,
              model_input_hash = CASE WHEN ?14 THEN NULL ELSE model_input_hash END",
        params![
            plan.tracker,
            plan.project,
            plan.kind.as_db_str(),
            plan.key,
            facets.input_hash,
            opts.pipeline_version,
            facets.fix_edge_source.as_db_str(),
            facets.anchors_qualified as i64,
            facets.thread_shape,
            facets.revert_override as i64,
            facets.closing_keyword,
            now,
            repo_id,
            invalidate_model,
        ],
    )?;
    Ok(())
}

/// Clear the MECHANICAL junctions this pass rebuilds deterministically, scoped to the full thread
/// identity. Anchor candidates are rebuilt only for new/regenerated inputs: an unchanged rerun must
/// preserve the model's `selected` flags because it is not requeued.
fn clear_mechanical_junctions(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
    clear_anchor_candidates: bool,
) -> anyhow::Result<()> {
    conn.execute(
        &format!(
            "DELETE FROM papertrail_distill_record_commits WHERE {}",
            thread::THREAD_KEY_WHERE
        ),
        thread_key.params(repo_id),
    )?;
    if clear_anchor_candidates {
        conn.execute(
            &format!("DELETE FROM papertrail_distill_anchors WHERE {}", thread::THREAD_KEY_WHERE),
            thread_key.params(repo_id),
        )?;
    }
    // Edges key their SOURCE thread as (src_item_kind, src_item_key). Clear ONLY the `coalesced`
    // edges this pass rebuilds — `supersedes` / `promoted` edges are reserved to survive record
    // regeneration (later model/human relationships), so a routine rerun must not wipe them.
    conn.execute(
        "DELETE FROM papertrail_distill_edges
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND src_item_kind = ?4
           AND src_item_key = ?5 AND edge_kind = 'coalesced'",
        thread_key.params(repo_id),
    )?;
    Ok(())
}

fn replace_snapshot(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
    snapshot: &ThreadSnapshot,
) -> anyhow::Result<()> {
    for table in ["papertrail_distill_units", "papertrail_distill_sources"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE {}", thread::THREAD_KEY_WHERE),
            thread_key.params(repo_id),
        )?;
    }
    for source in &snapshot.sources {
        conn.execute(
            "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, author, author_kind, author_association, created_at_ms, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18)",
            params![
                thread_key.tracker,
                thread_key.project,
                thread_key.item_kind,
                thread_key.item_key,
                source.ordinal as i64,
                source.role.as_db_str(),
                source.partner_ordinal.map(|value| value as i64),
                source.item_kind.as_db_str(),
                source.item_key,
                source.kind.as_db_str(),
                source.part.as_db_str(),
                source.id,
                source.exact_text,
                source.author,
                source.author_kind,
                source.author_association,
                source.created_at_ms,
                repo_id,
            ],
        )?;
    }
    for unit in &snapshot.units {
        conn.execute(
            "INSERT INTO papertrail_distill_units
                 (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal, byte_start,
                  byte_end, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                thread_key.tracker,
                thread_key.project,
                thread_key.item_kind,
                thread_key.item_key,
                unit.ordinal as i64,
                unit.source_ordinal as i64,
                unit.span.start as i64,
                unit.span.end as i64,
                repo_id,
            ],
        )?;
    }
    Ok(())
}

/// Replace a thread's snapshotted fix-diff rows (#800). Same gating as [`replace_snapshot`]:
/// rewritten only when the extraction identity changed.
fn replace_fix_diffs(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
    diffs: &[FixDiffSnapshot],
) -> anyhow::Result<()> {
    conn.execute(
        &format!("DELETE FROM papertrail_distill_fix_diffs WHERE {}", thread::THREAD_KEY_WHERE),
        thread_key.params(repo_id),
    )?;
    for diff in diffs {
        conn.execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                thread_key.tracker,
                thread_key.project,
                thread_key.item_kind,
                thread_key.item_key,
                diff.commit_sha,
                diff.path,
                diff.patch,
                repo_id,
            ],
        )?;
    }
    Ok(())
}

/// Replace a thread's snapshotted cross-reference rows (#800). Same gating as
/// [`replace_snapshot`].
fn replace_xrefs(
    conn: &Connection,
    repo_id: &str,
    thread_key: &ThreadKey,
    xrefs: &[XrefSnapshot],
) -> anyhow::Result<()> {
    conn.execute(
        &format!("DELETE FROM papertrail_distill_xrefs WHERE {}", thread::THREAD_KEY_WHERE),
        thread_key.params(repo_id),
    )?;
    for xref in xrefs {
        conn.execute(
            "INSERT INTO papertrail_distill_xrefs
                 (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                  target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                  repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                thread_key.tracker,
                thread_key.project,
                thread_key.item_kind,
                thread_key.item_key,
                xref.ordinal as i64,
                xref.target_tracker,
                xref.target_project,
                xref.target_item_kind,
                xref.target_item_key,
                xref.ref_kind,
                xref.title,
                xref.opening,
                repo_id,
            ],
        )?;
    }
    Ok(())
}

fn write_commits(
    conn: &Connection,
    repo_id: &str,
    now: i64,
    plan: &RecordPlan,
    shas: &[String],
) -> anyhow::Result<()> {
    for sha in shas {
        conn.execute(
            "INSERT OR IGNORE INTO papertrail_distill_record_commits
                 (tracker, project, item_kind, item_key, commit_sha, created_at_ms, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![plan.tracker, plan.project, plan.kind.as_db_str(), plan.key, sha, now, repo_id],
        )?;
    }
    Ok(())
}

fn write_coalesced_edges(
    conn: &Connection,
    repo_id: &str,
    now: i64,
    plan: &RecordPlan,
) -> anyhow::Result<()> {
    for partner in &plan.partners {
        conn.execute(
            "INSERT OR IGNORE INTO papertrail_distill_edges
                 (tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                  edge_kind, created_at_ms, repo_id)
             VALUES (?1, ?2, ?3, ?4, 'change_request', ?5, 'coalesced', ?6, ?7)",
            params![
                plan.tracker,
                plan.project,
                plan.kind.as_db_str(),
                plan.key,
                partner,
                now,
                repo_id,
            ],
        )?;
    }
    Ok(())
}

fn write_anchors(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
    anchors: &[candidates::AnchorCandidate],
) -> anyhow::Result<()> {
    for (candidate_ordinal, anchor) in anchors.iter().enumerate() {
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, file_path,
                   name, resolved, candidate_ordinal, selected, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11)",
            params![
                plan.tracker,
                plan.project,
                plan.kind.as_db_str(),
                plan.key,
                anchor.kind.as_db_str(),
                anchor.logical_symbol_id,
                anchor.file_path,
                anchor.name,
                anchor.resolved as i64,
                candidate_ordinal as i64,
                repo_id,
            ],
        )?;
    }
    Ok(())
}

fn enqueue_one(
    conn: &Connection,
    repo_id: &str,
    now: i64,
    item: &ItemRow,
    regenerated: bool,
) -> anyhow::Result<usize> {
    // On REGENERATION, RESET a surviving queue row's attempt state — the new input must not inherit
    // the old input's exhausted attempts / stale error / stale raw reply, or the drain might skip
    // it (retry budget spent) or report diagnostics against the wrong input. New/unchanged work
    // keeps its state (`DO NOTHING`).
    let sql = if regenerated {
        "INSERT INTO papertrail_distill_queue
             (tracker, project, item_kind, item_key, enqueued_at_ms, repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(repo_id, tracker, project, item_kind, item_key) DO UPDATE SET
             enqueued_at_ms = excluded.enqueued_at_ms, attempts = 0, last_error = NULL,
             raw_reply = NULL"
    } else {
        "INSERT INTO papertrail_distill_queue
             (tracker, project, item_kind, item_key, enqueued_at_ms, repo_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(repo_id, tracker, project, item_kind, item_key) DO NOTHING"
    };
    Ok(conn.execute(sql, params![
        item.tracker,
        item.project,
        item.kind.as_db_str(),
        item.key,
        now,
        repo_id
    ])?)
}

// ── Small helpers ──────────────────────────────────────────────────────────────────────────────

/// A planned record: the thread identity (tracker/project/kind/key), its coalesce partners, and its
/// mechanical fix commits + fix-edge source. Tracker/project are carried so the writers don't each
/// re-look-up the row.
struct RecordPlan {
    tracker: String,
    project: String,
    kind: ItemKind,
    key: String,
    partners: Vec<String>,
    fix_shas: Vec<String>,
    fix_edge_source: FixEdgeSource,
    /// A TEXT-tier closing edge links this issue — the canonical parser matched a closing keyword.
    /// Drives the closing-keyword status floor (issue records only).
    text_closing: bool,
}

impl RecordPlan {
    /// The thread key of the coalesced partner change request `partner` in this record's project.
    fn partner_key(&self, partner: &str) -> ThreadKey {
        ThreadKey {
            tracker: self.tracker.clone(),
            project: self.project.clone(),
            item_kind: ItemKind::ChangeRequest.as_db_str().to_owned(),
            item_key: partner.to_owned(),
        }
    }
}

impl From<&RecordPlan> for ThreadKey {
    fn from(plan: &RecordPlan) -> Self {
        Self {
            tracker: plan.tracker.clone(),
            project: plan.project.clone(),
            item_kind: plan.kind.as_db_str().to_owned(),
            item_key: plan.key.clone(),
        }
    }
}

impl From<&ItemRow> for ThreadKey {
    fn from(item: &ItemRow) -> Self {
        Self {
            tracker: item.tracker.clone(),
            project: item.project.clone(),
            item_kind: item.kind.as_db_str().to_owned(),
            item_key: item.key.clone(),
        }
    }
}

#[cfg(test)]
#[path = "extract_tests.rs"]
mod tests;
