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
use rag_rat_papertrail::{FixEdgeSource, ItemKind, OutcomeStatus};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::distill::candidates::{self, AnchorCaps};
use crate::distill::{units, validate};

/// Bumped whenever the extraction/prompt contract changes in a way that invalidates existing
/// records — part of the regeneration identity alongside `distill_input_hash`.
pub(crate) const PIPELINE_VERSION: i64 = 1;

/// Default byte budget for a thread's assembled units (head+tail retained, middle dropped).
const DEFAULT_MAX_THREAD_BYTES: usize = 26_000;

/// Knobs for one extraction pass.
pub(crate) struct ExtractOptions {
    pub pipeline_version: i64,
    pub max_thread_bytes: usize,
    pub anchor_caps: AnchorCaps,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            pipeline_version: PIPELINE_VERSION,
            max_thread_bytes: DEFAULT_MAX_THREAD_BYTES,
            anchor_caps: AnchorCaps::default(),
        }
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
    in_txn(conn, || {
        let items = load_items(conn, &repo_id)?;
        let edges = load_closing_edges(conn, &repo_id)?;

        // Index items by their FULL thread identity (tracker, project, kind_token, key): within one
        // repo, issue/PR numbers are only unique per (tracker, project) — a repo can mirror several
        // tracker bindings — so keying by kind/key alone would collide same-numbered items across
        // projects. `merged_prs` drops the kind (always change_request).
        let mut by_key: BTreeMap<(String, String, &'static str, String), &ItemRow> =
            BTreeMap::new();
        let mut merged_prs: BTreeMap<(String, String, String), &ItemRow> = BTreeMap::new();
        let mut closed_issues: Vec<&ItemRow> = Vec::new();
        for item in &items {
            by_key.insert(
                (
                    item.tracker.clone(),
                    item.project.clone(),
                    item.kind.as_db_str(),
                    item.key.clone(),
                ),
                item,
            );
            match item.kind {
                ItemKind::Issue if item.state_normalized == "closed" => closed_issues.push(item),
                ItemKind::ChangeRequest if item.state_normalized == "merged" => {
                    merged_prs.insert(
                        (item.tracker.clone(), item.project.clone(), item.key.clone()),
                        item,
                    );
                },
                _ => {},
            }
        }

        // Closing edges grouped by the (tracker, project, issue) they close.
        let mut edges_by_issue: BTreeMap<(String, String, String), Vec<&ClosingEdgeRow>> =
            BTreeMap::new();
        for edge in &edges {
            edges_by_issue
                .entry((edge.tracker.clone(), edge.project.clone(), edge.issue_key.clone()))
                .or_default()
                .push(edge);
        }

        // Plan the records: closed issues (coalescing their merged-PR closers within the same
        // project), then merged PRs that no issue coalesced away.
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
                match edge.closer_kind.as_str() {
                    "commit" => {
                        // A commit closer is an accepted fix edge; upgrade provenance and take the
                        // sha.
                        fix_shas.insert(edge.closer_key.clone());
                        source = stronger_source(source, edge.source.as_str());
                        text_closing |= edge.source == "text";
                    },
                    "change_request" => {
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
                            source = stronger_source(source, edge.source.as_str());
                            text_closing |= edge.source == "text";
                            if let Some(commit) =
                                edge.closer_commit.clone().or_else(|| pr.merge_commit_sha.clone())
                            {
                                fix_shas.insert(commit);
                            }
                        }
                    },
                    _ => {},
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

        // The full set of records that SHOULD exist after this pass.
        let planned: BTreeSet<RecordKey> = plans
            .iter()
            .map(|p| {
                (
                    p.tracker.clone(),
                    p.project.clone(),
                    p.kind.as_db_str().to_string(),
                    p.key.clone(),
                )
            })
            .collect();

        let mut report = ExtractReport { eligible: plans.len(), ..Default::default() };
        // Reconcile against the planned set: any persisted record no longer planned — a reopened
        // issue, an un-merged PR, or a PR now coalesced into an issue — loses its stale
        // record/junctions/queue so consumers never see an ineligible or duplicate record.
        for existing in load_record_keys(conn, &repo_id)? {
            if !planned.contains(&existing) {
                delete_record(conn, &repo_id, &existing)?;
            }
        }
        // The cheap sync enqueue queues eligible PRs BEFORE extraction runs, so a thread queued but
        // never recorded (a PR extraction now coalesces, or a thread that became ineligible first)
        // leaves a queue row `delete_record` never touched. Drop any queue key not in the plan so a
        // later drain never processes a duplicate coalesced PR or an ineligible thread.
        for queued in load_queue_keys(conn, &repo_id)? {
            if !planned.contains(&queued) {
                let (tracker, project, kind, key) = &queued;
                conn.execute(
                    "DELETE FROM papertrail_distill_queue
                     WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
                       AND item_key = ?5",
                    params![repo_id, tracker, project, kind, key],
                )?;
            }
        }
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
        Ok(report)
    })
}

/// The stronger of the current fix-edge source and a newly seen closing-edge `source` token:
/// provider outranks text outranks none. Unknown tokens are treated as text (a mined tier).
fn stronger_source(current: FixEdgeSource, token: &str) -> FixEdgeSource {
    let seen = if token == "provider" { FixEdgeSource::Provider } else { FixEdgeSource::Text };
    match (current, seen) {
        (FixEdgeSource::Provider, _) | (_, FixEdgeSource::Provider) => FixEdgeSource::Provider,
        _ => FixEdgeSource::Text,
    }
}

struct WriteOutcome {
    queued: usize,
    mechanical_status: OutcomeStatus,
}

/// Assemble and persist one record: units → hash, fixing commits, coalesced edges, anchor
/// candidates, status floors, the skeleton row, and the queue entry.
fn write_record(
    conn: &Connection,
    repo_id: &str,
    now: i64,
    opts: &ExtractOptions,
    plan: &RecordPlan,
    by_key: &BTreeMap<(String, String, &'static str, String), &ItemRow>,
    repo: Option<&gix::Repository>,
) -> anyhow::Result<WriteOutcome> {
    let item = by_key
        .get(&(plan.tracker.clone(), plan.project.clone(), plan.kind.as_db_str(), plan.key.clone()))
        .copied()
        .ok_or_else(|| {
            anyhow::anyhow!("record item {}#{} vanished mid-pass", plan.kind.as_db_str(), plan.key)
        })?;

    // --- Text assembly: record TITLE + body + comments, then each partner's title + body +
    // comments, in order. The title leads because for a thin/empty-body thread it IS the problem
    // statement, and it must ride the regeneration hash so a title-only edit re-distills. Alongside
    // the text we collect an ordered provenance stream (source id + author association): #704
    // snapshots per-unit provenance and gates the decision floor on maintainer authorship, so a
    // provenance-only change (a new comment, a re-associated author) must regenerate even when
    // the text is identical.
    let mut blobs: Vec<String> = vec![item.title.clone(), item.body.clone()];
    let mut provenance: Vec<(String, Option<String>)> = vec![(
        format!("{}/{}#item", plan.kind.as_db_str(), plan.key),
        item.author_association.clone(),
    )];
    // Body length spans the whole coalesced thread (issue + partner PRs), so a thin issue body with
    // a substantial partner PR is not misclassified `thin`.
    let mut body_len = item.body.len();
    let record_comments =
        load_comments(conn, repo_id, &plan.tracker, &plan.project, plan.kind, &plan.key)?;
    let mut total_comments = record_comments.len();
    let mut review_comments = record_comments.iter().filter(|c| c.is_review).count();
    for comment in &record_comments {
        blobs.push(comment.body.clone());
        provenance.push((comment.comment_id.clone(), comment.author_association.clone()));
    }
    for partner in &plan.partners {
        let partner_id = (
            plan.tracker.clone(),
            plan.project.clone(),
            ItemKind::ChangeRequest.as_db_str(),
            partner.clone(),
        );
        if let Some(pr) = by_key.get(&partner_id) {
            blobs.push(pr.title.clone());
            blobs.push(pr.body.clone());
            provenance.push((
                format!("{}/{partner}#item", ItemKind::ChangeRequest.as_db_str()),
                pr.author_association.clone(),
            ));
            body_len += pr.body.len();
        }
        let partner_comments = load_comments(
            conn,
            repo_id,
            &plan.tracker,
            &plan.project,
            ItemKind::ChangeRequest,
            partner,
        )?;
        total_comments += partner_comments.len();
        review_comments += partner_comments.iter().filter(|c| c.is_review).count();
        for comment in &partner_comments {
            blobs.push(comment.body.clone());
            provenance.push((comment.comment_id.clone(), comment.author_association.clone()));
        }
    }
    // Segment every blob into units, budget the concatenation head+tail, keep the retained texts.
    let unit_texts = budgeted_unit_texts(&blobs, opts.max_thread_bytes);

    let changed_paths = changed_paths_for(conn, repo_id, repo, &plan.fix_shas)?;
    // The closing-keyword floor comes from the canonical parser's text-tier closing edge
    // (plan.text_closing), not a re-scan of commit text — a marker string, since the specific
    // keyword isn't load-bearing.
    let closing_keyword: Option<&str> = plan.text_closing.then_some("closing");
    // Revert detection is causal, not timestamp-ordered, and NEVER keys on the fix commit itself
    // being a `Revert` (that is intentional revert work that LANDED — not this record being
    // reverted). The floor fires ONLY when a downstream landed commit reverts one of THIS record's
    // CURRENT fixing commits: git's revert body names the reverted commit ("This reverts commit
    // <sha>"), so a reopen→revert→re-fix leaves the stale revert pointing at the OLD (replaced) fix
    // sha — not in `fix_shas` — and correctly does not flip. A revert can name the record's OWN
    // thread OR a coalesced partner PR (GitHub's revert says "Reverts …#<pr>"), so gather from
    // both.
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
    let mut revert_override = false;
    for revert_sha in &revert_shas {
        if let Some(commit) = commit_message(conn, repo_id, revert_sha)? {
            let text = format!("{}\n{}", commit.subject, commit.body);
            if plan.fix_shas.iter().any(|fix| text.contains(fix.as_str())) {
                revert_override = true;
                break;
            }
        }
    }

    // --- Anchor candidates from the changed source files. "Qualified" counts resolved SYMBOL
    // anchors (bound to a `sym_<hex>` logical id) — the precise, high-value bindings — separately
    // from coarser file anchors (which are also resolved but tracked as their own rate).
    let anchors = candidates::mine_anchor_candidates(conn, &changed_paths, opts.anchor_caps)?;
    let anchors_qualified = anchors
        .iter()
        .filter(|a| a.resolved && matches!(a.kind, candidates::AnchorKind::Symbol))
        .count();

    let thread_shape = validate::classify_thread_shape(total_comments, review_comments, body_len);

    // The mechanical effective status (model absent) — a floors-only preview for the report.
    let mechanical_status = validate::effective_status(&validate::EffectiveStatusInputs {
        revert_override,
        closing_keyword: closing_keyword.is_some(),
        fix_edge_source: plan.fix_edge_source,
        model_status: None,
    });

    let input_hash = compute_input_hash(&HashInputs {
        pipeline_version: opts.pipeline_version,
        plan,
        unit_texts: &unit_texts,
        changed_paths: &changed_paths,
        provenance: &provenance,
        revert_override,
        closing_keyword,
        anchors: &anchors,
    });

    // Record state drives model-invalidation + enqueue: a New or Regenerated record is enqueued for
    // #704; an Unchanged one is NOT (re-enqueuing after a drain would re-pay the LLM cost). On
    // regeneration the model columns + model junctions (evidence, alternatives) are cleared so a
    // consumer never sees an old decision/outcome pinned to new input; an identical rerun preserves
    // the model's work.
    let state = record_state(conn, repo_id, plan, &input_hash, opts.pipeline_version)?;
    let regenerated = state == RecordState::Regenerated;

    // --- Persist: rebuild this thread's mechanical junctions, upsert the skeleton row (clearing
    // model columns on regeneration), rewrite junctions, queue.
    clear_mechanical_junctions(conn, repo_id, plan)?;
    if regenerated {
        clear_model_junctions(conn, repo_id, plan)?;
    }
    upsert_skeleton(conn, repo_id, now, opts, plan, regenerated, &SkeletonFacets {
        input_hash: &input_hash,
        fix_edge_source: plan.fix_edge_source,
        anchors_qualified,
        thread_shape: thread_shape.as_db_str(),
        revert_override,
        closing_keyword,
    })?;
    write_commits(conn, repo_id, plan, &plan.fix_shas)?;
    write_coalesced_edges(conn, repo_id, now, plan)?;
    write_anchors(conn, repo_id, plan, &anchors)?;
    let queued = if matches!(state, RecordState::New | RecordState::Regenerated) {
        enqueue_one(conn, repo_id, now, item, regenerated)?
    } else {
        0
    };
    Ok(WriteOutcome { queued, mechanical_status })
}

/// Segment each blob into block units, then apply the tail-aware budget across the concatenation,
/// returning the retained unit texts in source order.
fn budgeted_unit_texts(blobs: &[String], max_total: usize) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();
    for blob in blobs {
        for span in units::segment_blocks(blob) {
            texts.push(span.slice(blob).to_string());
        }
    }
    let spans: Vec<units::Span> = {
        // Re-express the retained texts as contiguous spans purely to reuse the budget planner.
        let mut offset = 0usize;
        texts
            .iter()
            .map(|t| {
                let s = units::Span { start: offset, end: offset + t.len() };
                offset += t.len();
                s
            })
            .collect()
    };
    let plan = units::tail_aware_budget(&spans, max_total);
    plan.kept.into_iter().map(|i| texts[i].clone()).collect()
}

/// Everything that folds into a record's regeneration identity.
struct HashInputs<'a> {
    pipeline_version: i64,
    plan: &'a RecordPlan,
    unit_texts: &'a [String],
    changed_paths: &'a [String],
    provenance: &'a [(String, Option<String>)],
    revert_override: bool,
    closing_keyword: Option<&'a str>,
    anchors: &'a [candidates::AnchorCandidate],
}

/// The regeneration identity: pipeline version, the record's kind/key, coalesce partners, retained
/// unit texts, per-source provenance, sorted changed-file selection, fix-edge source + SHAs, and
/// the computed mechanical status floors. Any change re-derives a new hash, which the natural-key
/// upsert treats as a fresh record — so a floor that flips later (a landed revert ref arrives, or a
/// fix commit's message becomes available and yields a closing keyword) re-queues the record for
/// #704.
fn compute_input_hash(inputs: &HashInputs<'_>) -> String {
    let HashInputs {
        pipeline_version,
        plan,
        unit_texts,
        changed_paths,
        provenance,
        revert_override,
        closing_keyword,
        anchors,
    } = inputs;
    let mut hasher = Sha256::new();
    hasher.update(pipeline_version.to_le_bytes());
    hasher
        .update([plan.kind.as_db_str().as_bytes(), b"\x1f", plan.key.as_bytes(), b"\x1e"].concat());
    for partner in &plan.partners {
        hasher.update(partner.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\x1e");
    for text in *unit_texts {
        hasher.update(text.as_bytes());
        hasher.update(b"\x1f");
    }
    // Ordered source identity + author association: a new/removed comment or a re-associated author
    // (which #704 snapshots and gates the decision floor on) regenerates even with identical text.
    hasher.update(b"\x1e");
    for (source_id, association) in *provenance {
        hasher.update(source_id.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(association.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\x1e");
    let mut sorted = changed_paths.to_vec();
    sorted.sort();
    for path in sorted {
        hasher.update(path.as_bytes());
        hasher.update(b"\x1f");
    }
    // Mechanical fix-edge inputs + computed status floors: a changed closing edge / merge SHA (same
    // files), a flipped provenance tier, or a floor that flips later must invalidate the model's
    // decision/outcome even when the thread text is identical.
    hasher.update(b"\x1e");
    hasher.update(plan.fix_edge_source.as_db_str().as_bytes());
    hasher.update([*revert_override as u8]);
    hasher.update(closing_keyword.unwrap_or("").as_bytes());
    hasher.update(b"\x1e");
    let mut shas = plan.fix_shas.clone();
    shas.sort();
    for sha in shas {
        hasher.update(sha.as_bytes());
        hasher.update(b"\x1f");
    }
    // Anchor candidate set: #704 selects anchors from this pool, so a reindex that resolves new
    // logical symbols (candidates absent at first extraction, present after) must re-queue the
    // record. Hash each candidate's identity; mining order is already deterministic.
    hasher.update(b"\x1e");
    for anchor in *anchors {
        hasher.update(anchor.kind.as_db_str().as_bytes());
        hasher.update(b"\x1f");
        hasher.update(anchor.logical_symbol_id.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\x1f");
        hasher.update(anchor.name.as_bytes());
        hasher.update(b"\x1f");
    }
    let hex: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
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
    author_association: Option<String>,
}

fn load_items(conn: &Connection, repo_id: &str) -> anyhow::Result<Vec<ItemRow>> {
    let mut stmt = conn.prepare(
        "SELECT item_kind, item_key, tracker, project, title, body, state_normalized,
                merge_commit_sha, author_association
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
            author_association: row.get(8)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

struct ClosingEdgeRow {
    tracker: String,
    project: String,
    issue_key: String,
    closer_kind: String,
    closer_key: String,
    closer_commit: Option<String>,
    source: String,
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
            closer_kind: row.get(3)?,
            closer_key: row.get(4)?,
            closer_commit: row.get(5)?,
            source: row.get(6)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

struct CommentRow {
    comment_id: String,
    body: String,
    is_review: bool,
    author_association: Option<String>,
}

fn load_comments(
    conn: &Connection,
    repo_id: &str,
    tracker: &str,
    project: &str,
    kind: ItemKind,
    key: &str,
) -> anyhow::Result<Vec<CommentRow>> {
    let mut stmt = conn.prepare(
        "SELECT comment_id, body, review_state, author_association FROM papertrail_comments
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5
         ORDER BY created_at, comment_id",
    )?;
    let rows =
        stmt.query_map(params![repo_id, tracker, project, kind.as_db_str(), key], |row| {
            Ok(CommentRow {
                comment_id: row.get(0)?,
                body: row.get(1)?,
                is_review: row.get::<_, Option<String>>(2)?.is_some(),
                author_association: row.get(3)?,
            })
        })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
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
    let parent_tree = match commit.parent_ids().next() {
        Some(parent) => repo
            .find_commit(parent.detach())
            .ok()
            .and_then(|p| p.tree().ok())
            .unwrap_or_else(|| repo.empty_tree()),
        None => repo.empty_tree(),
    };
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
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
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
    plan: &RecordPlan,
    new_hash: &str,
    new_version: i64,
) -> anyhow::Result<RecordState> {
    let existing: Option<(String, i64)> = conn
        .query_row(
            "SELECT distill_input_hash, pipeline_version FROM papertrail_distill
             WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4
               AND item_key = ?5",
            params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
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

/// A persisted record's full thread identity: (tracker, project, kind_token, key).
type RecordKey = (String, String, String, String);

/// Delete a thread's distill record, all its junctions (mechanical AND model), and its queue entry
/// — used to reconcile a record whose source thread is no longer eligible (reopened issue,
/// un-merged PR) or that a later sync discovered is actually coalesced into another thread, so
/// consumers never see a stale or duplicate record.
fn delete_record(conn: &Connection, repo_id: &str, record: &RecordKey) -> anyhow::Result<()> {
    let (tracker, project, kind, key) = record;
    for table in [
        "papertrail_distill",
        "papertrail_distill_record_commits",
        "papertrail_distill_anchors",
        "papertrail_distill_evidence",
        "papertrail_distill_alternatives",
        "papertrail_distill_queue",
    ] {
        conn.execute(
            &format!(
                "DELETE FROM {table} WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND \
                 item_kind = ?4 AND item_key = ?5"
            ),
            params![repo_id, tracker, project, kind, key],
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
        params![repo_id, tracker, project, kind, key],
    )?;
    Ok(())
}

/// Every distill record's full key currently persisted for `repo_id` — the reconciliation input.
fn load_record_keys(conn: &Connection, repo_id: &str) -> anyhow::Result<Vec<RecordKey>> {
    load_keys_from(conn, repo_id, "papertrail_distill")
}

/// Every queued thread's full key for `repo_id` — the queue-reconciliation input.
fn load_queue_keys(conn: &Connection, repo_id: &str) -> anyhow::Result<Vec<RecordKey>> {
    load_keys_from(conn, repo_id, "papertrail_distill_queue")
}

fn load_keys_from(conn: &Connection, repo_id: &str, table: &str) -> anyhow::Result<Vec<RecordKey>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT tracker, project, item_kind, item_key FROM {table} WHERE repo_id = ?1"
    ))?;
    let rows =
        stmt.query_map([repo_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
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
    regenerated: bool,
    facets: &SkeletonFacets<'_>,
) -> anyhow::Result<()> {
    // A fresh row inserts the model columns as NULL (honest nulls) for #704 to fill on this natural
    // key. On conflict the mechanical facets are always (re)written; the model columns are
    // PRESERVED for an identical rerun and NULLED when the input regenerated (`?14`), so a
    // stale decision/ outcome never rides new input.
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
                 CASE WHEN ?14 THEN 0 ELSE decision_provenance_verified END",
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
            regenerated,
        ],
    )?;
    Ok(())
}

/// Clear the MECHANICAL junctions this pass rebuilds deterministically (fixing commits, anchor
/// candidates, thread-keyed edges), scoped to the full thread identity so a same-numbered thread in
/// another project is untouched.
fn clear_mechanical_junctions(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
) -> anyhow::Result<()> {
    for table in ["papertrail_distill_record_commits", "papertrail_distill_anchors"] {
        conn.execute(
            &format!(
                "DELETE FROM {table} WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND \
                 item_kind = ?4 AND item_key = ?5"
            ),
            params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
        )?;
    }
    // Edges key their SOURCE thread as (src_item_kind, src_item_key). Clear ONLY the `coalesced`
    // edges this pass rebuilds — `supersedes` / `promoted` edges are reserved to survive record
    // regeneration (later model/human relationships), so a routine rerun must not wipe them.
    conn.execute(
        "DELETE FROM papertrail_distill_edges
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND src_item_kind = ?4
           AND src_item_key = ?5 AND edge_kind = 'coalesced'",
        params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
    )?;
    Ok(())
}

/// Clear the MODEL junctions (#704 output: evidence units and rejected alternatives) — only on
/// regeneration, so an identical rerun keeps the model's work but a changed input never leaves
/// stale evidence pinned to a fresh record.
fn clear_model_junctions(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
) -> anyhow::Result<()> {
    for table in ["papertrail_distill_evidence", "papertrail_distill_alternatives"] {
        conn.execute(
            &format!(
                "DELETE FROM {table} WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND \
                 item_kind = ?4 AND item_key = ?5"
            ),
            params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
        )?;
    }
    Ok(())
}

fn write_commits(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
    shas: &[String],
) -> anyhow::Result<()> {
    for sha in shas {
        conn.execute(
            "INSERT OR IGNORE INTO papertrail_distill_record_commits
                 (tracker, project, item_kind, item_key, commit_sha, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![plan.tracker, plan.project, plan.kind.as_db_str(), plan.key, sha, repo_id],
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
    for anchor in anchors {
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, logical_symbol_id, file_path,
                  name, resolved, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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

/// Run `body` inside an IMMEDIATE transaction when the connection is in autocommit; otherwise run
/// inline (the caller owns the transaction). Mirrors the store layer's fence.
fn in_txn<T>(conn: &Connection, body: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    if conn.is_autocommit() {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match body() {
            Ok(value) => {
                conn.execute_batch("COMMIT")?;
                Ok(value)
            },
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(err)
            },
        }
    } else {
        body()
    }
}

#[cfg(test)]
mod tests {
    use rag_rat_db::schema;
    use rusqlite::{Connection, params};

    use super::{ExtractOptions, enqueue_eligible, extract};

    /// A fully-migrated in-memory DB scoped to `repo`, via the same `temp.connection_context` write
    /// `install_scope_view` uses (the multi-repo-scope test convention).
    fn scoped_conn(repo: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn, &crate::index::migration_hooks()).unwrap();
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS connection_context(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO temp.connection_context(key, value) VALUES ('repo_id', ?1)",
            [repo],
        )
        .unwrap();
        conn
    }

    fn seed_item(
        conn: &Connection,
        repo: &str,
        kind: &str,
        key: &str,
        body: &str,
        state_normalized: &str,
        merge_commit_sha: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO papertrail_items
                 (tracker, project, item_kind, item_key, url, state, title, body, synced_at_ms,
                  repo_id, state_normalized, merge_commit_sha)
             VALUES ('github','o/r',?1,?2,'u','closed','t',?3,1,?4,?5,?6)",
            params![kind, key, body, repo, state_normalized, merge_commit_sha],
        )
        .unwrap();
    }

    fn seed_comment(conn: &Connection, repo: &str, kind: &str, key: &str, id: &str, review: bool) {
        conn.execute(
            "INSERT INTO papertrail_comments
                 (tracker, project, item_kind, item_key, comment_id, body, synced_at_ms, repo_id,
                  review_state, created_at)
             VALUES ('github','o/r',?1,?2,?3,'a comment',1,?4,?5,'2026-01-01')",
            params![kind, key, id, repo, if review { Some("approved") } else { None }],
        )
        .unwrap();
    }

    /// Project-parameterized item seed, for multi-binding isolation tests.
    fn seed_item_in(
        conn: &Connection,
        repo: &str,
        project: &str,
        kind: &str,
        key: &str,
        body: &str,
        state: &str,
    ) {
        conn.execute(
            "INSERT INTO papertrail_items
                 (tracker, project, item_kind, item_key, url, state, title, body, synced_at_ms,
                  repo_id, state_normalized, merge_commit_sha)
             VALUES ('github',?1,?2,?3,'u','closed','t',?4,1,?5,?6,NULL)",
            params![project, kind, key, body, repo, state],
        )
        .unwrap();
    }

    fn seed_closing_edge(
        conn: &Connection,
        repo: &str,
        issue: &str,
        closer_key: &str,
        commit: Option<&str>,
        source: &str,
    ) {
        conn.execute(
            "INSERT INTO papertrail_closing_edges
                 (tracker, project, issue_kind, issue_key, closer_kind, closer_key, closer_commit,
                  source, synced_at_ms, repo_id)
             VALUES ('github','o/r','issue',?1,'change_request',?2,?3,?4,1,?5)",
            params![issue, closer_key, commit, source, repo],
        )
        .unwrap();
    }

    fn seed_commit(conn: &Connection, repo: &str, sha: &str, subject: &str) {
        seed_commit_with_body(conn, repo, sha, subject, "");
    }

    fn seed_commit_with_body(conn: &Connection, repo: &str, sha: &str, subject: &str, body: &str) {
        conn.execute(
            "INSERT INTO git_commits
                 (hash, author_name, author_email, authored_at_s, committed_at_s, subject, body, \
             repo_id)
             VALUES (?1,'a','a@e',1,1,?2,?4,?3)",
            params![sha, subject, repo, body],
        )
        .unwrap();
    }

    fn seed_changed_file(conn: &Connection, repo: &str, sha: &str, path: &str) {
        conn.execute(
            "INSERT INTO git_file_changes (commit_hash, path, change_kind, repo_id)
             VALUES (?1, ?2, 'modified', ?3)",
            params![sha, path, repo],
        )
        .unwrap();
        seed_indexed_source(conn, repo, path);
    }

    /// The indexed `files` row + one logical/un-logical symbol at `path`, WITHOUT any
    /// `git_file_changes` row — so anchor mining only resolves it via a live gix diff (the
    /// merge-commit fallback path), not the `git_file_changes` lookup.
    fn seed_indexed_source(conn: &Connection, repo: &str, path: &str) {
        // A matching indexed file + one logical + one un-logical symbol, so anchor mining resolves.
        conn.execute(
            "INSERT INTO files (path, language, kind, sha256, modified_at_ms, indexed_at_ms, \
             repo_id, generation)
             VALUES (?1,'rust','source','s',1,1,?2,0)",
            params![path, repo],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO symbols (file_id, language, name, kind, start_byte, end_byte)
             VALUES (?1,'rust','render_widget','function',0,10)",
            [file_id],
        )
        .unwrap();
        let symbol_id = conn.last_insert_rowid();
        // The logical parent (FK target); id 999 is what the anchor's `sym_<hex>` encodes.
        conn.execute(
            "INSERT OR IGNORE INTO logical_symbols
                 (id, language, path, logical_name, kind, variant_count, group_reason)
             VALUES (999, 'rust', ?1, 'render_widget', 'function', 1, 'test')",
            [path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO logical_symbol_members (logical_symbol_id, symbol_id, start_line, \
             end_line)
             VALUES (999, ?1, 1, 2)",
            [symbol_id],
        )
        .unwrap();
    }

    fn distill_count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn full_flow_extracts_a_coalesced_record_with_floors_anchors_and_queue() {
        let conn = scoped_conn("repoA");
        // A closed issue #5 fixed by a merged PR #6, linked by a TEXT-tier closing edge (the
        // closer-minting parser matched a closing keyword); the PR has a review comment.
        seed_item(&conn, "repoA", "issue", "5", "The widget crashes on load.", "closed", None);
        seed_item(
            &conn,
            "repoA",
            "change_request",
            "6",
            "## Summary\nFixes it.",
            "merged",
            Some("deadbeef"),
        );
        seed_comment(&conn, "repoA", "issue", "5", "c1", false);
        seed_comment(&conn, "repoA", "change_request", "6", "c2", true);
        seed_closing_edge(&conn, "repoA", "5", "6", Some("deadbeef"), "text");
        seed_commit(&conn, "repoA", "deadbeef", "fix: widget crash (fixes #5)");
        seed_changed_file(&conn, "repoA", "deadbeef", "crates/core/src/widget.rs");

        let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(report.eligible, 1, "issue #5 is the record; PR #6 coalesces into it");
        assert_eq!(report.records_written, 1);
        assert_eq!(report.coalesced_pairs, 1);
        assert_eq!(report.fix_edge_text, 1);
        assert_eq!(report.mechanical_landed, 1, "the text closing edge floors it to landed");

        // The skeleton row: mechanical columns populated, model columns NULL.
        let (fix_src, kw, revert, shape, qualified, model_status): (
            String,
            Option<String>,
            i64,
            String,
            i64,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT fix_edge_source, closing_keyword_floor, revert_override, thread_shape,
                        anchors_qualified_count, outcome_status_model
                 FROM papertrail_distill WHERE item_kind='issue' AND item_key='5'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(fix_src, "text");
        assert_eq!(kw.as_deref(), Some("closing"), "text closing edge sets the keyword floor");
        assert_eq!(revert, 0);
        assert!(!shape.is_empty());
        assert_eq!(qualified, 1, "one resolved symbol anchor (render_widget)");
        assert_eq!(model_status, None, "model column is an honest null in Phase 1");

        // Mechanical junctions.
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_record_commits WHERE \
                 commit_sha='deadbeef'"
            ),
            1,
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_edges WHERE edge_kind='coalesced' AND \
                 src_item_key='5' AND dst_item_key='6'"
            ),
            1,
        );
        // File anchor + symbol anchor.
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE anchor_kind='file'"
            ),
            1
        );
        let sym: (String, i64) = conn
            .query_row(
                "SELECT logical_symbol_id, resolved FROM papertrail_distill_anchors
                 WHERE anchor_kind='symbol' AND name='render_widget'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(sym.0, "sym_3e7", "999 == 0x3e7"); // format_sym_handle(999)
        assert_eq!(sym.1, 1);
        // Queue holds the issue record (the coalesced PR is not its own record).
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"), 1);
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_queue WHERE item_kind='issue' AND \
                 item_key='5'"
            ),
            1,
        );
    }

    /// Build a throwaway git repo whose HEAD is a real MERGE commit, and return `(root, merge_sha,
    /// changed_path)`. The merge's first-parent diff touches `changed_path` (added on the merged
    /// branch), while a real merge carries NO per-file numstat — exactly the shape the history
    /// index stores no `git_file_changes` rows for.
    fn build_merge_repo() -> (std::path::PathBuf, String, String) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("ragrat-distill-merge-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        let git = |args: &[&str]| {
            let status =
                std::process::Command::new("git").current_dir(&root).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Rag Rat"]);
        git(&["config", "user.email", "rag@example.com"]);
        std::fs::write(root.join("README.md"), "base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("src/widget.rs"), "fn render_widget() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "feat: add widget"]);
        git(&["checkout", "-q", "-"]);
        // `--no-ff` forces a real merge commit (a fast-forward would carry no merge node).
        git(&["merge", "--no-ff", "-q", "-m", "Merge feature", "feature"]);
        let out = std::process::Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let merge_sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        (root, merge_sha, "src/widget.rs".to_string())
    }

    #[test]
    fn merge_commit_fix_mines_anchors_via_gix_first_parent_diff() {
        // A merge commit is the usual fixing SHA for a merged PR, but the history index records NO
        // `git_file_changes` rows for real merges — so anchor mining must fall back to a live gix
        // first-parent diff. This test seeds ONLY the indexed `files` row (no `git_file_changes`),
        // so it passes ONLY through the fallback: drop the fallback and the anchor count goes to 0.
        let (root, merge_sha, path) = build_merge_repo();
        let conn = scoped_conn("repoA");
        // A standalone merged PR, closed by its own merge commit (its own provider record).
        seed_item(
            &conn,
            "repoA",
            "change_request",
            "9",
            "A refactor PR.",
            "merged",
            Some(&merge_sha),
        );
        seed_commit(&conn, "repoA", &merge_sha, "Merge feature");
        seed_indexed_source(&conn, "repoA", &path);

        let report = extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();
        assert_eq!(report.records_written, 1);

        // The file anchor + the resolved symbol anchor both come from the gix-recovered path.
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE anchor_kind='file'"
            ),
            1,
            "the merge's first-parent diff surfaces the changed source file"
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE anchor_kind='symbol' AND \
                 name='render_widget' AND resolved=1"
            ),
            1,
            "and its symbol resolves"
        );

        // Control: with no repo handle the fallback cannot fire, so the same seed yields no anchors
        // — proving the anchors above came from the gix diff, not a stray
        // `git_file_changes` row.
        let conn2 = scoped_conn("repoB");
        seed_item(
            &conn2,
            "repoB",
            "change_request",
            "9",
            "A refactor PR.",
            "merged",
            Some(&merge_sha),
        );
        seed_commit(&conn2, "repoB", &merge_sha, "Merge feature");
        seed_indexed_source(&conn2, "repoB", &path);
        extract(&conn2, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(&conn2, "SELECT COUNT(*) FROM papertrail_distill_anchors"),
            0,
            "no repo handle → no fallback → no anchors from a merge with no git_file_changes rows"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_provider_only_closing_edge_does_not_set_the_keyword_floor() {
        // The closing-keyword floor is derived from a TEXT-tier closing edge (the parser matched a
        // closing keyword), NOT a re-scan of commit text — so a same-numbered cross-project or MR
        // URL in a commit can never force `landed`. A provider-attested closure carries no keyword,
        // so the floor stays NULL and the effective status defers to the model (unclear here).
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "A bug.", "closed", None);
        seed_item(&conn, "repoA", "change_request", "6", "The PR.", "merged", Some("m6"));
        seed_closing_edge(&conn, "repoA", "5", "6", Some("m6"), "provider");
        seed_commit(&conn, "repoA", "m6", "fix: crash. Fixes other/repo#5"); // cross-project ref

        let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
        let kw: Option<String> = conn
            .query_row(
                "SELECT closing_keyword_floor FROM papertrail_distill WHERE item_kind='issue' AND \
                 item_key='5'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kw, None, "a provider-only closure sets no keyword floor");
        assert_eq!(report.fix_edge_provider, 1);
        assert_eq!(report.mechanical_landed, 0, "no keyword floor → not force-landed");
    }

    #[test]
    fn standalone_merged_pr_is_its_own_provider_record() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A refactor PR.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "refactor: tidy up");
        let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(report.records_written, 1);
        assert_eq!(report.fix_edge_provider, 1, "a merged PR is its own provider closure");
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill WHERE item_kind='change_request' AND \
                 item_key='9'"
            ),
            1,
        );
    }

    #[test]
    fn closed_issue_with_no_closing_edge_has_no_fix_edge() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "7", "Closed as not planned.", "closed", None);
        let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(report.fix_edge_none, 1);
        let src: String = conn
            .query_row(
                "SELECT fix_edge_source FROM papertrail_distill WHERE item_key='7'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src, "none");
    }

    #[test]
    fn a_closing_edge_to_an_unmerged_pr_does_not_upgrade_fix_edge_provenance() {
        // Issue #5 has a provider closing edge to PR #6, but PR #6 is CLOSED (not merged) — its
        // merge_commit_sha is a trap. The edge must not be treated as a fix edge, so the
        // no-fix-edge floor still fires (fix_edge_source = none, and no coalesce).
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "A bug.", "closed", None);
        seed_item(
            &conn,
            "repoA",
            "change_request",
            "6",
            "An abandoned PR.",
            "closed",
            Some("beef"),
        );
        seed_closing_edge(&conn, "repoA", "5", "6", Some("beef"), "provider");

        let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(report.coalesced_pairs, 0, "an unmerged PR is not a coalesce partner");
        let src: String = conn
            .query_row(
                "SELECT fix_edge_source FROM papertrail_distill WHERE item_kind='issue' AND \
                 item_key='5'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src, "none", "an unmerged-PR closing edge is not a fix edge");
    }

    #[test]
    fn sibling_repo_items_never_leak_into_the_active_repos_records() {
        let conn = scoped_conn("repoA");
        // Active repo A has a closed issue #5; a SIBLING repo B has an identically-numbered closed
        // issue #5. Extraction is scoped to A and must not produce a record for B's item.
        seed_item(&conn, "repoA", "issue", "5", "A's bug.", "closed", None);
        seed_item(&conn, "repoB", "issue", "5", "B's bug.", "closed", None);
        seed_item(&conn, "repoB", "change_request", "6", "B's PR.", "merged", Some("beef"));

        let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(report.records_written, 1, "only repo A's single item");
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"), 1);
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill WHERE repo_id='repoB'"),
            0,
            "no sibling-repo record",
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_queue WHERE repo_id='repoB'"
            ),
            0,
        );
    }

    #[test]
    fn cheap_enqueue_covers_every_eligible_thread_and_is_idempotent_and_scoped() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "closed issue", "closed", None);
        seed_item(&conn, "repoA", "change_request", "6", "merged pr", "merged", Some("x"));
        seed_item(&conn, "repoA", "issue", "8", "still open", "open", None); // not eligible
        seed_item(&conn, "repoB", "issue", "5", "sibling", "closed", None); // wrong repo

        let first = enqueue_eligible(&conn).unwrap();
        assert_eq!(first, 2, "the closed issue + merged PR (not the open issue, not repo B)");
        // Idempotent: a second pass adds nothing.
        assert_eq!(enqueue_eligible(&conn).unwrap(), 0);
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"), 2);
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_queue WHERE repo_id='repoB'"
            ),
            0,
        );

        // Once a thread is distilled and its queue row is DRAINED, a later sync must NOT re-enqueue
        // it — otherwise every completed thread is re-processed forever.
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        conn.execute("DELETE FROM papertrail_distill_queue", []).unwrap();
        assert_eq!(enqueue_eligible(&conn).unwrap(), 0, "distilled threads are not re-enqueued");
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"), 0);
    }

    #[test]
    fn re_running_extraction_is_idempotent() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "refactor: x");
        seed_changed_file(&conn, "repoA", "cafe", "crates/core/src/a.rs");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        // No duplicate rows across the two passes (natural-key upsert + junction
        // clear-and-rebuild).
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"), 1);
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_record_commits"),
            1
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE anchor_kind='file'"
            ),
            1
        );
    }

    #[test]
    fn same_numbered_threads_in_different_projects_stay_isolated() {
        // One repo, two tracker-binding projects, each with a closed issue #5. Only o/r's issue is
        // closed by a merged PR #6. Records must not cross-contaminate on the shared number.
        let conn = scoped_conn("repoA");
        seed_item_in(&conn, "repoA", "o/r", "issue", "5", "Alpha bug body.", "closed");
        seed_item_in(&conn, "repoA", "o/r2", "issue", "5", "Beta bug body.", "closed");
        seed_item_in(&conn, "repoA", "o/r", "change_request", "6", "Alpha PR.", "merged");
        seed_closing_edge(&conn, "repoA", "5", "6", Some("deadbeef"), "provider");

        let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
        // Two issue records (one per project); o/r's PR #6 coalesced away.
        assert_eq!(report.records_written, 2);
        assert_eq!(report.coalesced_pairs, 1);

        // Each project's record has its OWN fix edge and a DISTINCT input hash (distinct bodies).
        let fix_a: String = conn
            .query_row(
                "SELECT fix_edge_source FROM papertrail_distill WHERE project='o/r' AND \
                 item_key='5'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let fix_b: String = conn
            .query_row(
                "SELECT fix_edge_source FROM papertrail_distill WHERE project='o/r2' AND \
                 item_key='5'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fix_a, "provider", "o/r#5 coalesces its merged PR closer");
        assert_eq!(fix_b, "none", "o/r2#5 has no closer of its own");
        let distinct_hashes: i64 = distill_count(
            &conn,
            "SELECT COUNT(DISTINCT distill_input_hash) FROM papertrail_distill WHERE item_key='5'",
        );
        assert_eq!(distinct_hashes, 2, "distinct bodies → distinct regeneration identities");

        // The coalesced edge belongs to o/r only — o/r2 has none.
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_edges WHERE project='o/r' AND \
                 src_item_key='5'"
            ),
            1,
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_edges WHERE project='o/r2'"
            ),
            0,
        );
    }

    #[test]
    fn a_pr_extracted_standalone_loses_its_record_once_it_becomes_coalesced() {
        let conn = scoped_conn("repoA");
        // Pass 1: PR #6 is merged with no known closing edge → a standalone record + queue entry.
        seed_item(&conn, "repoA", "change_request", "6", "The fix PR.", "merged", Some("dead"));
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill WHERE item_kind='change_request' AND \
                 item_key='6'"
            ),
            1,
            "PR #6 starts as a standalone record",
        );

        // Pass 2: a closed issue #5 and a closing edge #5 -> #6 arrive; #6 now coalesces into #5.
        seed_item(&conn, "repoA", "issue", "5", "The bug.", "closed", None);
        seed_closing_edge(&conn, "repoA", "5", "6", Some("dead"), "provider");
        extract(&conn, None, &ExtractOptions::default()).unwrap();

        // The stale standalone PR record + its queue entry are gone; only the coalesced issue
        // record remains, with the coalesce edge.
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill WHERE item_kind='change_request' AND \
                 item_key='6'"
            ),
            0,
            "the coalesced PR's standalone record is reconciled away",
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_queue WHERE item_kind='change_request' \
                 AND item_key='6'"
            ),
            0,
            "the coalesced PR's queue entry is removed",
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill WHERE item_kind='issue' AND item_key='5'"
            ),
            1,
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_edges WHERE edge_kind='coalesced' AND \
                 dst_item_key='6'"
            ),
            1,
        );
    }

    #[test]
    fn a_record_whose_thread_becomes_ineligible_is_reconciled_away() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "7", "A bug.", "closed", None);
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"), 1);

        // The issue is reopened → no longer eligible. The next extraction must drop its record.
        conn.execute("UPDATE papertrail_items SET state_normalized='open' WHERE item_key='7'", [])
            .unwrap();
        let report = extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(report.records_written, 0, "the reopened issue is not eligible");
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill"),
            0,
            "the ineligible record is reconciled away",
        );
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"), 0);
    }

    #[test]
    fn changing_the_fixing_commit_regenerates_even_with_identical_text() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "A bug.", "closed", None);
        // A commit closing edge (its SHA is a fix input that never appears in the thread text).
        let seed_commit_edge = |sha: &str| {
            conn.execute(
                "INSERT OR REPLACE INTO papertrail_closing_edges
                     (tracker, project, issue_kind, issue_key, closer_kind, closer_key,
                      closer_commit, source, synced_at_ms, repo_id)
                 VALUES ('github','o/r','issue','5','commit',?1,NULL,'provider',1,'repoA')",
                [sha],
            )
            .unwrap();
        };
        seed_commit_edge("c1");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        conn.execute("UPDATE papertrail_distill SET root_cause='x' WHERE item_key='5'", [])
            .unwrap();

        // A second fixing commit arrives (same thread text). The record must regenerate: model
        // output cleared, because the fix SHA set rides the regeneration hash.
        seed_commit_edge("c2");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let cause: Option<String> = conn
            .query_row("SELECT root_cause FROM papertrail_distill WHERE item_key='5'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cause, None, "a changed fixing commit invalidates stale model output");
    }

    #[test]
    fn a_pr_queued_before_extraction_is_dropped_from_the_queue_once_coalesced() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "The bug.", "closed", None);
        seed_item(&conn, "repoA", "change_request", "6", "The fix PR.", "merged", Some("dead"));
        seed_closing_edge(&conn, "repoA", "5", "6", Some("dead"), "provider");

        // The cheap sync enqueue runs first (no records yet) → queues BOTH #5 and #6.
        assert_eq!(enqueue_eligible(&conn).unwrap(), 2);
        // Extraction coalesces #6 into #5. #6's queue row (never a record) must be reconciled away.
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_queue WHERE item_kind='change_request' \
                 AND item_key='6'"
            ),
            0,
            "the coalesced PR's pre-extraction queue row is removed",
        );
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_queue WHERE item_kind='issue' AND \
                 item_key='5'"
            ),
            1,
            "the coalesced issue record stays queued",
        );
    }

    #[test]
    fn an_unchanged_rerun_after_a_drain_does_not_requeue() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "refactor: x");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        // Simulate the #704 drain removing the completed queue row.
        conn.execute("DELETE FROM papertrail_distill_queue", []).unwrap();
        // An identical re-extract must NOT re-enqueue the completed, unchanged record.
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"),
            0,
            "an unchanged record is not re-queued after a drain",
        );
    }

    #[test]
    fn only_a_landed_commit_revert_flips_status_not_a_text_claim() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "feat: x"); // the fix (merge commit)
        // A reverts ref, tagged by its source and (for commits) the reverting commit sha.
        let seed_reverts = |source_kind: &str, source_commit: Option<&str>| {
            conn.execute(
                "INSERT INTO papertrail_refs
                     (tracker, project, item_key, item_kind, ref_kind, source_kind, source_commit,
                      source_text, discovered_at_ms, repo_id)
                 VALUES ('github','o/r','9','change_request','reverts',?1,?2,'Reverts \
                 #9',1,'repoA')",
                rusqlite::params![source_kind, source_commit],
            )
            .unwrap();
        };
        // A text-tier `reverts` ref (an open PR body / comment claim) must NOT flip status.
        seed_reverts("item", None);
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"
            ),
            0,
            "a text-claim reverts ref does not flip status",
        );

        // A reverting commit that is NOT in git history (rebased away / stale annotation) also must
        // not flip — the ref is a dangling annotation.
        seed_reverts("commit", Some("nonexistent-sha"));
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"
            ),
            0,
            "a reverts commit no longer in git history does not flip status",
        );

        // A landed revert commit that reverts a DIFFERENT (old, replaced) commit — the
        // reopen→revert→re-fix case — must not flip the re-fixed record.
        seed_commit_with_body(
            &conn,
            "repoA",
            "stale-rev",
            "Revert old",
            "This reverts commit oldfix.",
        );
        seed_reverts("commit", Some("stale-rev"));
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"
            ),
            0,
            "a revert of a non-current fix commit does not flip status",
        );

        // A landed revert whose body reverts the CURRENT fix commit does.
        seed_commit_with_body(
            &conn,
            "repoA",
            "real-rev",
            "Revert the fix",
            "This reverts commit cafe.",
        );
        seed_reverts("commit", Some("real-rev"));
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"
            ),
            1,
            "a revert of the current fix commit flips status",
        );
    }

    #[test]
    fn a_fix_that_is_itself_a_revert_landed_it_is_not_marked_reverted() {
        // A merged PR whose own fixing commit is a `Revert` (intentional revert work) LANDED — the
        // record must not be marked reverted just because its subject starts with "Revert".
        let conn = scoped_conn("repoA");
        seed_item(
            &conn,
            "repoA",
            "change_request",
            "9",
            "Revert the bad change.",
            "merged",
            Some("cafe"),
        );
        seed_commit(&conn, "repoA", "cafe", "Revert \"feat: bad change\" (#8)");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT revert_override FROM papertrail_distill WHERE item_key='9'"
            ),
            0,
            "intentional revert work that landed is not itself reverted",
        );
    }

    #[test]
    fn a_revert_of_the_coalesced_partner_pr_flips_the_issue_record() {
        // Issue #5 coalesces merged PR #6; the revert commit references the PR (#6), not the issue.
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "A bug.", "closed", None);
        seed_item(&conn, "repoA", "change_request", "6", "The fix PR.", "merged", Some("m6"));
        seed_closing_edge(&conn, "repoA", "5", "6", Some("m6"), "provider");
        seed_commit(&conn, "repoA", "m6", "The fix"); // the coalesced fix (merge commit)
        // the revert names the fix commit it reverts
        seed_commit_with_body(&conn, "repoA", "rev6", "Revert the fix", "This reverts commit m6.");
        conn.execute(
            "INSERT INTO papertrail_refs
                 (tracker, project, item_key, item_kind, ref_kind, source_kind, source_commit,
                  source_text, discovered_at_ms, repo_id)
             VALUES ('github','o/r','6','change_request','reverts','commit','rev6','Reverts \
             #6',1,'repoA')",
            [],
        )
        .unwrap();

        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT revert_override FROM papertrail_distill WHERE item_kind='issue' AND \
                 item_key='5'"
            ),
            1,
            "a revert of the coalesced partner PR flips the issue record",
        );
    }

    #[test]
    fn a_rerun_preserves_non_coalesced_edges() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "feat: x");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        // A later model/human `supersedes` edge authored by this record (reserved to survive
        // regeneration).
        conn.execute(
            "INSERT INTO papertrail_distill_edges
                 (tracker, project, src_item_kind, src_item_key, dst_item_kind, dst_item_key,
                  edge_kind, created_at_ms, repo_id)
             VALUES \
             ('github','o/r','change_request','9','change_request','10','supersedes',1,'repoA')",
            [],
        )
        .unwrap();

        // Re-extract: the mechanical clear must NOT wipe the supersedes edge.
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_edges WHERE edge_kind='supersedes' AND \
                 src_item_key='9'"
            ),
            1,
            "a rerun preserves supersedes/promoted edges",
        );
    }

    #[test]
    fn regeneration_resets_the_queue_attempt_state() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "feat: x");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        // The previous input exhausted the drain's retries and left an error on the queue row.
        conn.execute(
            "UPDATE papertrail_distill_queue SET attempts=5, last_error='boom', raw_reply='junk'
             WHERE item_key='9'",
            [],
        )
        .unwrap();

        // The input changes (a new comment). The regenerated work must start with a fresh attempt.
        seed_comment(&conn, "repoA", "change_request", "9", "c-new", false);
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let (attempts, err, reply): (i64, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT attempts, last_error, raw_reply FROM papertrail_distill_queue WHERE \
                 item_key='9'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(attempts, 0, "regeneration resets the attempt count");
        assert_eq!(err, None);
        assert_eq!(reply, None);
    }

    #[test]
    fn regeneration_clears_stale_model_output_but_an_identical_rerun_preserves_it() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A PR body.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "refactor: x");
        extract(&conn, None, &ExtractOptions::default()).unwrap();

        // Simulate the #704 pass filling model columns + model junctions on this record.
        conn.execute(
            "UPDATE papertrail_distill SET root_cause='the cause', outcome_status_model='landed',
                 quotes_materialized=3 WHERE item_key='9'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_evidence
                 (tracker, project, item_kind, item_key, field, source_kind, source_id,
                  byte_start, byte_end, quote, repo_id)
             VALUES ('github','o/r','change_request','9','root_cause','item','9',0,3,'the','repoA')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_alternatives
                 (tracker, project, item_kind, item_key, ordinal, alternative, repo_id)
             VALUES ('github','o/r','change_request','9',0,'do nothing','repoA')",
            [],
        )
        .unwrap();

        // An IDENTICAL rerun preserves the model's work (same input hash + pipeline version).
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let cause: Option<String> = conn
            .query_row("SELECT root_cause FROM papertrail_distill WHERE item_key='9'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cause.as_deref(), Some("the cause"), "identical rerun keeps model output");
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_evidence"), 1);

        // Changing the input (a new comment shifts the assembled units → a new hash) INVALIDATES
        // the model columns and clears the model junctions.
        seed_comment(&conn, "repoA", "change_request", "9", "c-new", false);
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let cause_after: Option<String> = conn
            .query_row("SELECT root_cause FROM papertrail_distill WHERE item_key='9'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cause_after, None, "regeneration NULLs the stale model columns");
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_evidence"),
            0,
            "regeneration clears stale evidence",
        );
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_alternatives"),
            0,
            "regeneration clears stale alternatives",
        );
    }
}
