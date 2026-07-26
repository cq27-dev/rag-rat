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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceRole {
    Primary,
    Partner,
}

impl SourceRole {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Partner => "partner",
        }
    }

    #[cfg_attr(not(test), allow(dead_code, reason = "the drain will hydrate persisted snapshots"))]
    fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "primary" => Ok(Self::Primary),
            "partner" => Ok(Self::Partner),
            other => anyhow::bail!("unknown distill source role `{other}`"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Item,
    Comment,
}

impl SourceKind {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Comment => "comment",
        }
    }

    #[cfg_attr(not(test), allow(dead_code, reason = "the drain will hydrate persisted snapshots"))]
    fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "item" => Ok(Self::Item),
            "comment" => Ok(Self::Comment),
            other => anyhow::bail!("unknown distill source kind `{other}`"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourcePart {
    Title,
    Body,
    Comment,
}

impl SourcePart {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Body => "body",
            Self::Comment => "comment",
        }
    }

    #[cfg_attr(not(test), allow(dead_code, reason = "the drain will hydrate persisted snapshots"))]
    fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "title" => Ok(Self::Title),
            "body" => Ok(Self::Body),
            "comment" => Ok(Self::Comment),
            other => anyhow::bail!("unknown distill source part `{other}`"),
        }
    }
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
    by_key: &BTreeMap<(String, String, &'static str, String), &ItemRow>,
    repo: Option<&gix::Repository>,
) -> anyhow::Result<WriteOutcome> {
    let item = by_key
        .get(&(plan.tracker.clone(), plan.project.clone(), plan.kind.as_db_str(), plan.key.clone()))
        .copied()
        .ok_or_else(|| {
            anyhow::anyhow!("record item {}#{} vanished mid-pass", plan.kind.as_db_str(), plan.key)
        })?;

    // Snapshot exact source rows before deriving anything lossy. The snapshot, its hash, and the
    // skeleton are committed in this extraction transaction, so later mirror LWW edits cannot make
    // a model citation point at different bytes.
    let snapshot = build_thread_snapshot(conn, repo_id, plan, item, by_key)?;
    // Body length spans the whole coalesced thread (issue + partner PRs), so a thin issue body with
    // a substantial partner PR is not misclassified `thin`.
    let mut body_len = item.body.len();
    let record_comments =
        load_comments(conn, repo_id, &plan.tracker, &plan.project, plan.kind, &plan.key)?;
    let mut total_comments = record_comments.len();
    let mut review_comments = record_comments.iter().filter(|c| c.is_review).count();
    for partner in &plan.partners {
        let partner_id = (
            plan.tracker.clone(),
            plan.project.clone(),
            ItemKind::ChangeRequest.as_db_str(),
            partner.clone(),
        );
        if let Some(pr) = by_key.get(&partner_id) {
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
    }

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
    let state = record_state(conn, repo_id, plan, &input_hash, opts.pipeline_version)?;
    let regenerated = state == RecordState::Regenerated;
    let prompt_changed = state == RecordState::Unchanged
        && match stored_prompt_version(conn, repo_id, plan)? {
            Some(version) => version != prompts::PROMPT_VERSION,
            // A queued NULL-stamped row is pending or previously failed, not legacy completed
            // output. Preserve its attempt diagnostics; the drain always renders the current
            // prompt. A NULL-stamped row with no queue entry needs recovery/reprocessing.
            None => !queue_entry_exists(conn, repo_id, plan)?,
        };
    let invalidate_model = regenerated || prompt_changed;

    // --- Persist: rebuild this thread's mechanical junctions, upsert the skeleton row (clearing
    // model columns on regeneration), rewrite junctions, queue.
    let rebuild_anchor_candidates = state != RecordState::Unchanged;
    clear_mechanical_junctions(conn, repo_id, plan, rebuild_anchor_candidates)?;
    if invalidate_model {
        clear_model_junctions(conn, repo_id, plan)?;
    }
    upsert_skeleton(conn, repo_id, now, opts, plan, invalidate_model, &SkeletonFacets {
        input_hash: &input_hash,
        fix_edge_source: plan.fix_edge_source,
        anchors_qualified,
        thread_shape: thread_shape.as_db_str(),
        revert_override,
        closing_keyword,
    })?;
    if state != RecordState::Unchanged {
        replace_snapshot(conn, repo_id, plan, &snapshot)?;
        replace_xrefs(conn, repo_id, plan, &xrefs)?;
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
        && !fix_diff_rows_exist(conn, repo_id, plan)?;
    if state != RecordState::Unchanged || diff_heal {
        let fix_diffs = fix_diff_snapshots(repo, &plan.fix_shas, &anchors);
        replace_fix_diffs(conn, repo_id, plan, &fix_diffs)?;
    }
    write_commits(conn, repo_id, plan, &plan.fix_shas)?;
    write_coalesced_edges(conn, repo_id, now, plan)?;
    if rebuild_anchor_candidates {
        write_anchors(conn, repo_id, plan, &anchors)?;
    }
    let queued = if matches!(state, RecordState::New | RecordState::Regenerated) || prompt_changed {
        enqueue_one(conn, repo_id, now, item, invalidate_model)?
    } else {
        0
    };
    Ok(WriteOutcome { queued, mechanical_status })
}

fn stored_prompt_version(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
) -> anyhow::Result<Option<u32>> {
    let version = conn.query_row(
        "SELECT prompt_version FROM papertrail_distill
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5",
        params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    version.map(u32::try_from).transpose().map_err(Into::into)
}

fn queue_entry_exists(conn: &Connection, repo_id: &str, plan: &RecordPlan) -> anyhow::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM papertrail_distill_queue
             WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3
               AND item_kind = ?4 AND item_key = ?5
         )",
        params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Whether the thread already has snapshotted fix-diff rows (#800) — the self-heal probe for a
/// record first extracted without a usable repo handle.
fn fix_diff_rows_exist(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
) -> anyhow::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM papertrail_distill_fix_diffs
             WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3
               AND item_kind = ?4 AND item_key = ?5
         )",
        params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
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
    let hex: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
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
    by_key: &BTreeMap<(String, String, &'static str, String), &ItemRow>,
) -> anyhow::Result<ThreadSnapshot> {
    let mut snapshot = ThreadSnapshot { sources: Vec::new(), units: Vec::new() };
    append_item_snapshot(
        &mut snapshot,
        SourceRole::Primary,
        None,
        primary,
        load_comments(conn, repo_id, &plan.tracker, &plan.project, primary.kind, &primary.key)?,
    );
    for (partner_ordinal, partner_key) in plan.partners.iter().enumerate() {
        let partner_identity = (
            plan.tracker.clone(),
            plan.project.clone(),
            ItemKind::ChangeRequest.as_db_str(),
            partner_key.clone(),
        );
        let partner = by_key.get(&partner_identity).copied().ok_or_else(|| {
            anyhow::anyhow!("coalesced partner change_request#{partner_key} vanished mid-pass")
        })?;
        append_item_snapshot(
            &mut snapshot,
            SourceRole::Partner,
            Some(partner_ordinal),
            partner,
            load_comments(conn, repo_id, &plan.tracker, &plan.project, partner.kind, &partner.key)?,
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
    author: Option<String>,
    author_kind: Option<String>,
    author_association: Option<String>,
    created_at_ms: Option<i64>,
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
        "SELECT comment_id, body, review_state, author, author_kind, author_association,
                CASE WHEN created_at IS NULL THEN NULL ELSE
                    CAST(strftime('%s', created_at) AS INTEGER) * 1000 +
                    CAST(substr(strftime('%f', created_at), 4, 3) AS INTEGER)
                END
         FROM papertrail_comments
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5
         ORDER BY created_at, comment_id",
    )?;
    let rows =
        stmt.query_map(params![repo_id, tracker, project, kind.as_db_str(), key], |row| {
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
        "papertrail_distill_sources",
        "papertrail_distill_units",
        "papertrail_distill_fix_diffs",
        "papertrail_distill_xrefs",
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
    plan: &RecordPlan,
    clear_anchor_candidates: bool,
) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM papertrail_distill_record_commits
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5",
        params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
    )?;
    if clear_anchor_candidates {
        conn.execute(
            "DELETE FROM papertrail_distill_anchors
             WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3
               AND item_kind = ?4 AND item_key = ?5",
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

/// Clear model-owned junction state when extraction or prompt identity changes. An identical rerun
/// keeps the model's work; a requeued record exposes no stale evidence, alternatives, or
/// selections.
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
    conn.execute(
        "UPDATE papertrail_distill_anchors SET selected = 0
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5",
        params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
    )?;
    Ok(())
}

fn replace_snapshot(
    conn: &Connection,
    repo_id: &str,
    plan: &RecordPlan,
    snapshot: &ThreadSnapshot,
) -> anyhow::Result<()> {
    for table in ["papertrail_distill_units", "papertrail_distill_sources"] {
        conn.execute(
            &format!(
                "DELETE FROM {table} WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND \
                 item_kind = ?4 AND item_key = ?5"
            ),
            params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
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
                plan.tracker,
                plan.project,
                plan.kind.as_db_str(),
                plan.key,
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
                plan.tracker,
                plan.project,
                plan.kind.as_db_str(),
                plan.key,
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
    plan: &RecordPlan,
    diffs: &[FixDiffSnapshot],
) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM papertrail_distill_fix_diffs
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5",
        params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
    )?;
    for diff in diffs {
        conn.execute(
            "INSERT INTO papertrail_distill_fix_diffs
                 (tracker, project, item_kind, item_key, commit_sha, path, patch, repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                plan.tracker,
                plan.project,
                plan.kind.as_db_str(),
                plan.key,
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
    plan: &RecordPlan,
    xrefs: &[XrefSnapshot],
) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM papertrail_distill_xrefs
         WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3 AND item_kind = ?4 AND item_key = ?5",
        params![repo_id, plan.tracker, plan.project, plan.kind.as_db_str(), plan.key],
    )?;
    for xref in xrefs {
        conn.execute(
            "INSERT INTO papertrail_distill_xrefs
                 (tracker, project, item_kind, item_key, xref_ordinal, target_tracker,
                  target_project, target_item_kind, target_item_key, ref_kind, title, opening,
                  repo_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                plan.tracker,
                plan.project,
                plan.kind.as_db_str(),
                plan.key,
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

    use super::{ExtractOptions, SourceKind, SourcePart, SourceRole, enqueue_eligible, extract};

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

    fn input_hash(conn: &Connection, key: &str) -> String {
        conn.query_row(
            "SELECT distill_input_hash FROM papertrail_distill WHERE item_key = ?1",
            [key],
            |row| row.get(0),
        )
        .unwrap()
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
        let sym: (String, i64, i64, i64) = conn
            .query_row(
                "SELECT logical_symbol_id, resolved, candidate_ordinal, selected
                 FROM papertrail_distill_anchors
                  WHERE anchor_kind='symbol' AND name='render_widget'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(sym.0, "sym_3e7", "999 == 0x3e7"); // format_sym_handle(999)
        assert_eq!(sym.1, 1);
        assert_eq!(sym.2, 1, "file A0 is followed deterministically by symbol A1");
        assert_eq!(sym.3, 0, "extraction mines candidates; only the model selects them");
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
    fn build_merge_repo() -> (rag_rat_base::test_scratch::ScratchDir, String, String) {
        let root = rag_rat_base::test_scratch::ScratchDir::new("distill-merge");
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
    }

    #[test]
    fn fix_diff_is_snapshotted_only_for_symbol_candidate_files() {
        // The merge repo's first-parent diff adds `src/widget.rs`; the seeded index resolves its
        // symbol, so the patch is snapshotted. The same commit's `README.md` change (no symbol
        // candidate) must NOT appear — the cap is by symbol-candidate files, not by changed files.
        let (root, merge_sha, path) = build_merge_repo();
        let conn = scoped_conn("repoA");
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

        extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();

        let patches: Vec<(String, String)> = conn
            .prepare("SELECT path, patch FROM papertrail_distill_fix_diffs ORDER BY path")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(patches.len(), 1, "only the symbol-candidate file's patch is snapshotted");
        let (patch_path, patch) = &patches[0];
        assert_eq!(patch_path, &path);
        assert!(patch.contains("diff --git a/src/widget.rs b/src/widget.rs"), "{patch}");
        assert!(patch.contains("--- /dev/null"), "an added file diffs from /dev/null: {patch}");
        assert!(patch.contains("+++ b/src/widget.rs"), "{patch}");
        assert!(patch.contains("+fn render_widget() {}"), "the hunk content renders: {patch}");

        // Control: no repo handle → no diff rows (the drain never sees git state).
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
            distill_count(&conn2, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
            0,
            "no repo handle → best-effort empty diff snapshot"
        );
    }

    #[test]
    fn losing_the_repo_handle_preserves_diff_snapshots_and_the_identity() {
        // The diff is a pure function of already-hashed inputs, so git AVAILABILITY must not be
        // part of the record identity: extract with a repo, then again with `None` (the documented
        // bare/copied-index path) — the hash holds, the good snapshot rows survive, nothing
        // re-enqueues. The changed paths come from `git_file_changes` rows here (not the live-gix
        // merge fallback), so the anchors and changed-path selection are repo-independent and ONLY
        // the diff snapshot differs between the two passes.
        let (root, merge_sha, path) = build_merge_repo();
        let conn = scoped_conn("repoA");
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
        seed_changed_file(&conn, "repoA", &merge_sha, &path);

        extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();
        let hash_with_repo: String = conn
            .query_row("SELECT distill_input_hash FROM papertrail_distill", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
            1,
            "the repo-backed pass snapshots the diff"
        );

        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let hash_without_repo: String = conn
            .query_row("SELECT distill_input_hash FROM papertrail_distill", [], |row| row.get(0))
            .unwrap();
        assert_eq!(hash_with_repo, hash_without_repo, "repo availability is not identity");
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
            1,
            "a repo-less rerun preserves the snapshot"
        );

        // Self-heal: the mirror is reset to the repo-less state (record + no diff rows), then a
        // repo-backed pass fills the missing rows WITHOUT any identity change.
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
        seed_changed_file(&conn2, "repoB", &merge_sha, &path);
        extract(&conn2, None, &ExtractOptions::default()).unwrap();
        assert_eq!(distill_count(&conn2, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"), 0);
        extract(&conn2, Some(&root), &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(&conn2, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
            1,
            "a later repo-backed pass heals the missing diff rows"
        );
    }

    #[test]
    fn a_shallow_clone_missing_the_parent_yields_no_bogus_full_tree_diff() {
        // At a shallow boundary the parent id is recorded but the object is absent; that is NOT a
        // root commit. Diffing against the empty tree would snapshot every repo file as added.
        let (root, merge_sha, path) = build_merge_repo();
        // `git clone` into the guard's fresh, empty directory.
        let shallow = rag_rat_base::test_scratch::ScratchDir::new("distill-shallow");
        let status = std::process::Command::new("git")
            .args([
                "clone",
                "-q",
                "--depth",
                "1",
                &format!("file://{}", root.display()),
                shallow.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "shallow clone failed");

        let conn = scoped_conn("repoA");
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
        // `git_file_changes` rows supply the anchors, so the symbol-candidate filter is non-empty
        // even though the shallow repo cannot produce a first-parent diff.
        seed_changed_file(&conn, "repoA", &merge_sha, &path);

        extract(&conn, Some(&shallow), &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_fix_diffs"),
            0,
            "a missing parent skips the commit — never a full-tree 'everything added' patch"
        );
    }

    /// Build a throwaway repo whose HEAD fix commit modifies `src/widget.rs` and adds a >1MiB
    /// `src/big.rs`, returning `(root, fix_sha)`.
    fn build_big_blob_repo() -> (rag_rat_base::test_scratch::ScratchDir, String) {
        let root = rag_rat_base::test_scratch::ScratchDir::new("distill-big");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let git = |args: &[&str]| {
            let status =
                std::process::Command::new("git").current_dir(&root).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Rag Rat"]);
        git(&["config", "user.email", "rag@example.com"]);
        std::fs::write(root.join("src/widget.rs"), "fn render_widget() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        std::fs::write(root.join("src/widget.rs"), "fn render_widget() { todo!() }\n").unwrap();
        std::fs::write(root.join("src/big.rs"), "x".repeat(1_200_000)).unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "fix: widget and a giant file"]);
        let out = std::process::Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let fix_sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        (root, fix_sha)
    }

    #[test]
    fn a_blob_over_the_size_cap_is_skipped_before_rendering() {
        let (root, fix_sha) = build_big_blob_repo();
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "The fix.", "merged", Some(&fix_sha));
        seed_commit(&conn, "repoA", &fix_sha, "fix: widget and a giant file");
        seed_changed_file(&conn, "repoA", &fix_sha, "src/widget.rs");
        seed_changed_file(&conn, "repoA", &fix_sha, "src/big.rs");

        extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();
        let paths: Vec<String> = conn
            .prepare("SELECT path FROM papertrail_distill_fix_diffs ORDER BY path")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            paths,
            vec!["src/widget.rs".to_string()],
            "the small patch renders; the >1MiB blob is skipped before any full diff"
        );
    }

    /// Seed one outbound ref row the mirror sync would have mined from `source_text`'s item body.
    fn seed_outbound_ref(
        conn: &Connection,
        repo: &str,
        source_text: &str,
        target_key: &str,
        ref_kind: &str,
    ) {
        conn.execute(
            "INSERT INTO papertrail_refs
                 (tracker, project, item_key, item_kind, ref_kind, source_kind, source_text,
                  discovered_at_ms, repo_id)
             VALUES ('github','o/r',?1,'issue',?2,'item',?3,1,?4)",
            params![target_key, ref_kind, source_text, repo],
        )
        .unwrap();
    }

    #[test]
    fn outbound_refs_snapshot_the_referenced_title_and_opening() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "A bug. See #9.", "closed", None);
        seed_item(
            &conn,
            "repoA",
            "issue",
            "9",
            "Opening line.\n\nSecond paragraph.",
            "closed",
            None,
        );
        seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "9", "reference");
        // A comment-sourced ref to the same target dedupes; a ref to an unmirrored item drops.
        seed_comment(&conn, "repoA", "issue", "5", "c1", false);
        seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5:c1", "9", "reference");
        seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "404", "reference");
        // A giant single-paragraph body: the opening snapshot is capped to EXACTLY the prompt's
        // render width, so a multi-MB paragraph neither inflates the row nor hashes text the model
        // never sees.
        seed_item(&conn, "repoA", "issue", "10", &"x".repeat(5_000), "closed", None);
        seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "10", "reference");

        extract(&conn, None, &ExtractOptions::default()).unwrap();

        let rows: Vec<(String, String, String, String)> = conn
            .prepare(
                "SELECT target_item_key, ref_kind, title, opening FROM papertrail_distill_xrefs
                 WHERE item_kind='issue' AND item_key='5' ORDER BY xref_ordinal",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2, "comment-source dup and unmirrored target contribute nothing");
        assert_eq!(rows[0].0, "9");
        assert_eq!(rows[0].1, "reference");
        assert_eq!(rows[0].2, "t", "seeded title is frozen");
        assert_eq!(rows[0].3, "Opening line.", "the opening paragraph only, not the body");
        assert_eq!(rows[1].0, "10");
        assert_eq!(
            rows[1].3.chars().count(),
            crate::distill::prompts::XREF_TEXT_RENDER_CHARS + 1,
            "a giant paragraph's opening is capped to the render width plus the ellipsis",
        );
        assert!(rows[1].3.ends_with('…'), "the truncated opening keeps the ellipsis marker");

        // The referenced records are eligible too, but their own records have no outbound refs.
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_xrefs"), 2);
    }

    #[test]
    fn xref_snapshot_cap_matches_the_prompt_xref_budget() {
        // Rows beyond the snapshot cap are invisible to the prompt; a budget below the cap would
        // hash rows the model never sees (spurious regeneration on their edits). Keep the two
        // equal.
        assert_eq!(
            super::XREF_SNAPSHOT_CAP,
            crate::distill::prompts::PromptBudget::default().max_xrefs,
            "the snapshot cap and the render budget must move together"
        );
    }

    #[test]
    fn an_xref_edit_beyond_the_render_width_does_not_regenerate() {
        // The length-dimension partner of `xref_snapshot_cap_matches_the_prompt_xref_budget`: a
        // referenced item's title/opening are hashed at EXACTLY the width the prompt renders, so an
        // edit past that width is invisible to the model and must NOT regenerate the record
        // (re-paying the model with identical visible input). An edit WITHIN the width still does.
        let width = crate::distill::prompts::XREF_TEXT_RENDER_CHARS;
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "A bug. See #9.", "closed", None);
        seed_item(&conn, "repoA", "issue", "9", "Body.", "closed", None);
        seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "9", "reference");
        let head = "a".repeat(width);
        conn.execute("UPDATE papertrail_items SET title = ?1 WHERE item_key = '9'", [format!(
            "{head}X"
        )])
        .unwrap();

        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let baseline = input_hash(&conn, "5");
        let stored: String = conn
            .query_row("SELECT title FROM papertrail_distill_xrefs WHERE item_key='5'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            stored.chars().count(),
            width + 1,
            "the stored title is capped to the render width (plus the ellipsis), not the source",
        );

        // Edit only the TAIL, past the rendered width — the first `width` chars are unchanged.
        conn.execute("UPDATE papertrail_items SET title = ?1 WHERE item_key = '9'", [format!(
            "{head}YYYY"
        )])
        .unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            input_hash(&conn, "5"),
            baseline,
            "a tail-only edit past the render width leaves the identity unchanged",
        );

        // Edit WITHIN the rendered width — now the model-visible text changes, so regenerate.
        conn.execute("UPDATE papertrail_items SET title = ?1 WHERE item_key = '9'", [format!(
            "z{head}"
        )])
        .unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_ne!(
            input_hash(&conn, "5"),
            baseline,
            "an edit within the render width regenerates the record",
        );
    }

    #[test]
    fn a_bare_ref_resolves_the_target_kind_by_fallback() {
        // A kindless bare ref (`#N`, `papertrail_refs.item_kind` NULL) resolves down the fallback
        // ladder: the source item's OWN kind first (parser namespace inheritance), then the
        // deterministic kind order. A PR whose `#9` names an ISSUE resolves the issue even though
        // the syntax could not disambiguate; a ref that resolves as NEITHER kind drops entirely.
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "5", "A PR. See #9 and #404.", "merged", None);
        seed_item(&conn, "repoA", "issue", "9", "The referenced issue body.", "closed", None);
        let kindless = |target: &str| {
            conn.execute(
                "INSERT INTO papertrail_refs
                     (tracker, project, item_key, item_kind, ref_kind, source_kind, source_text,
                      discovered_at_ms, repo_id)
                 VALUES ('github','o/r',?1,NULL,'reference','item',
                         'github:o/r:change_request:5',1,'repoA')",
                [target],
            )
            .unwrap();
        };
        kindless("9"); // no change_request #9 exists, but issue #9 does → resolves via fallback
        kindless("404"); // resolves as neither kind → dropped

        extract(&conn, None, &ExtractOptions::default()).unwrap();

        let rows: Vec<(String, String)> = conn
            .prepare(
                "SELECT target_item_key, target_item_kind FROM papertrail_distill_xrefs
                 WHERE item_kind='change_request' AND item_key='5' ORDER BY xref_ordinal",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![("9".to_string(), "issue".to_string())],
            "the kindless ref resolves to the issue by fallback; the unresolvable ref drops",
        );
    }

    /// Build a throwaway repo whose HEAD fix commit DELETES `src/gone.rs` (added in the base
    /// commit), returning `(root, fix_sha, path)`.
    fn build_delete_repo() -> (rag_rat_base::test_scratch::ScratchDir, String, String) {
        let root = rag_rat_base::test_scratch::ScratchDir::new("distill-del");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let git = |args: &[&str]| {
            let status =
                std::process::Command::new("git").current_dir(&root).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Rag Rat"]);
        git(&["config", "user.email", "rag@example.com"]);
        std::fs::write(root.join("src/gone.rs"), "fn gone() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        std::fs::remove_file(root.join("src/gone.rs")).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "fix: remove gone"]);
        let out = std::process::Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let fix_sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        (root, fix_sha, "src/gone.rs".to_string())
    }

    #[test]
    fn a_deleted_symbol_file_snapshots_a_deletion_patch() {
        // The deletion arm of the file-patch header: a fixing commit that removes a
        // symbol-candidate file diffs the old path TO /dev/null, not a bogus addition.
        let (root, fix_sha, path) = build_delete_repo();
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "Remove it.", "merged", Some(&fix_sha));
        seed_commit(&conn, "repoA", &fix_sha, "fix: remove gone");
        seed_changed_file(&conn, "repoA", &fix_sha, &path);

        extract(&conn, Some(&root), &ExtractOptions::default()).unwrap();

        let patch: String = conn
            .query_row(
                "SELECT patch FROM papertrail_distill_fix_diffs WHERE path = ?1",
                [&path],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            patch.contains(&format!("--- a/{path}")),
            "a deletion diffs from the old path: {patch}"
        );
        assert!(patch.contains("+++ /dev/null"), "a deleted file diffs TO /dev/null: {patch}");
        assert!(patch.contains("-fn gone() {}"), "the removed line renders: {patch}");
    }

    #[test]
    fn an_xref_title_edit_regenerates_the_record() {
        // The referenced item's title is MUTABLE mirror state folded into the prompt, so an edit
        // must regenerate the record exactly like a primary-source edit.
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "A bug. See #9.", "closed", None);
        seed_item(&conn, "repoA", "issue", "9", "Opening line.", "closed", None);
        seed_outbound_ref(&conn, "repoA", "github:o/r:issue:5", "9", "reference");

        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let hash_before: String = conn
            .query_row(
                "SELECT distill_input_hash FROM papertrail_distill WHERE item_key='5'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE papertrail_items SET title='Renamed referenced item' WHERE item_key='9'",
            [],
        )
        .unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let hash_after: String = conn
            .query_row(
                "SELECT distill_input_hash FROM papertrail_distill WHERE item_key='5'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(hash_before, hash_after, "a referenced title edit changes the input identity");
        let title: String = conn
            .query_row("SELECT title FROM papertrail_distill_xrefs WHERE item_key='5'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "Renamed referenced item", "the snapshot re-froze the edited title");

        // An identical rerun is stable: no new queue rows for either record (the still-queued
        // NULL-stamped rows keep their place — draining, not extraction, retires them).
        let queue_before = distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        assert_eq!(
            distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_queue"),
            queue_before,
            "unchanged input does not re-enqueue"
        );
        let hash_stable: String = conn
            .query_row(
                "SELECT distill_input_hash FROM papertrail_distill WHERE item_key='5'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash_after, hash_stable, "an identical rerun recomputes the same identity");
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
        // Poison an identically-keyed sibling snapshot: A's replacement/clear paths must scope by
        // the complete record identity, not merely tracker/project/kind/key.
        conn.execute(
            "INSERT INTO papertrail_distill_sources
                 (tracker, project, item_kind, item_key, source_ordinal, role, partner_ordinal,
                  source_item_kind, source_item_key, source_kind, source_part, source_id,
                  exact_text, repo_id)
             VALUES ('github','o/r','issue','5',0,'primary',NULL,'issue','5','item','body','5',
                     'B poison','repoB')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO papertrail_distill_units
                 (tracker, project, item_kind, item_key, unit_ordinal, source_ordinal, byte_start,
                  byte_end, repo_id)
             VALUES ('github','o/r','issue','5',0,0,0,8,'repoB')",
            [],
        )
        .unwrap();

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
        let sibling_text: String = conn
            .query_row(
                "SELECT exact_text FROM papertrail_distill_sources WHERE repo_id='repoB'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sibling_text, "B poison", "the sibling snapshot survives A extraction");
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_units WHERE repo_id='repoB'"
            ),
            1,
            "the sibling unit span survives A extraction",
        );
    }

    #[test]
    fn snapshot_source_enums_round_trip_only_their_closed_tokens() {
        for role in [SourceRole::Primary, SourceRole::Partner] {
            assert_eq!(SourceRole::from_db_str(role.as_db_str()).unwrap(), role);
        }
        for kind in [SourceKind::Item, SourceKind::Comment] {
            assert_eq!(SourceKind::from_db_str(kind.as_db_str()).unwrap(), kind);
        }
        for part in [SourcePart::Title, SourcePart::Body, SourcePart::Comment] {
            assert_eq!(SourcePart::from_db_str(part.as_db_str()).unwrap(), part);
        }
        assert!(SourcePart::from_db_str("summary").is_err());
    }

    #[test]
    fn snapshots_distinguish_identical_title_and_body_and_unicode_units_quote_exact_bytes() {
        let conn = scoped_conn("repoA");
        let text = "Intro 🦀.\n\n尾 paragraph.";
        seed_item(&conn, "repoA", "issue", "5", text, "closed", None);
        conn.execute(
            "UPDATE papertrail_items SET title=?1, author='alice', author_kind='user',
                    author_association='member', created_at='2026-01-01T00:00:00.123Z'
             WHERE repo_id='repoA' AND item_key='5'",
            [text],
        )
        .unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();

        let sources: Vec<(i64, String, String, String)> = conn
            .prepare(
                "SELECT source_ordinal, source_part, source_id, exact_text
                 FROM papertrail_distill_sources WHERE repo_id='repoA' ORDER BY source_ordinal",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(&sources[0], &(0, "title".into(), "5".into(), text.into()));
        assert_eq!(&sources[1], &(1, "body".into(), "5".into(), text.into()));

        let units: Vec<(i64, i64, i64)> = conn
            .prepare(
                "SELECT source_ordinal, byte_start, byte_end FROM papertrail_distill_units
                 WHERE repo_id='repoA' ORDER BY unit_ordinal",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(units.len(), 4, "two markdown blocks for each distinct source part");
        for (source_ordinal, start, end) in units {
            let source_ordinal = source_ordinal as usize;
            let start = start as usize;
            let end = end as usize;
            let exact = &sources[source_ordinal].3;
            let quote = &exact[start..end];
            assert_eq!(quote.as_bytes(), &exact.as_bytes()[start..end]);
            assert!(std::str::from_utf8(quote.as_bytes()).is_ok(), "span is on UTF-8 boundaries");
        }
    }

    #[test]
    fn every_source_text_or_provenance_edit_changes_the_full_snapshot_hash() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "body one", "closed", None);
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let initial = input_hash(&conn, "5");

        conn.execute("UPDATE papertrail_items SET title='new title' WHERE item_key='5'", [])
            .unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let title_edit = input_hash(&conn, "5");
        assert_ne!(initial, title_edit, "title-only edits regenerate");

        conn.execute("UPDATE papertrail_items SET body='body two' WHERE item_key='5'", []).unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let body_edit = input_hash(&conn, "5");
        assert_ne!(title_edit, body_edit, "body-only edits regenerate");

        seed_comment(&conn, "repoA", "issue", "5", "c1", false);
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let comment_added = input_hash(&conn, "5");
        assert_ne!(body_edit, comment_added, "comments are full snapshot inputs");

        conn.execute(
            "UPDATE papertrail_comments SET body='edited comment' WHERE comment_id='c1'",
            [],
        )
        .unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let comment_edit = input_hash(&conn, "5");
        assert_ne!(comment_added, comment_edit, "comment text edits regenerate");

        conn.execute(
            "UPDATE papertrail_comments SET author_association='maintainer' WHERE comment_id='c1'",
            [],
        )
        .unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let provenance_edit = input_hash(&conn, "5");
        assert_ne!(comment_edit, provenance_edit, "provenance-only edits regenerate");
    }

    #[test]
    fn all_partners_are_snapshotted_in_deterministic_key_order() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "issue", "closed", None);
        seed_item(&conn, "repoA", "change_request", "8", "later", "merged", Some("m8"));
        seed_item(&conn, "repoA", "change_request", "6", "earlier", "merged", Some("m6"));
        seed_closing_edge(&conn, "repoA", "5", "8", Some("m8"), "provider");
        seed_closing_edge(&conn, "repoA", "5", "6", Some("m6"), "provider");
        extract(&conn, None, &ExtractOptions::default()).unwrap();

        let partners: Vec<(i64, String)> = conn
            .prepare(
                "SELECT partner_ordinal, source_item_key FROM papertrail_distill_sources
                 WHERE repo_id='repoA' AND role='partner' AND source_part='title'
                 ORDER BY source_ordinal",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(partners, vec![(0, "6".into()), (1, "8".into())]);
    }

    #[test]
    fn regeneration_replaces_snapshots_while_unchanged_reruns_leave_them_stable() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "issue", "5", "first\n\nsecond", "closed", None);
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let first_ids: Vec<i64> = conn
            .prepare("SELECT id FROM papertrail_distill_sources ORDER BY source_ordinal")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let first_hash = input_hash(&conn, "5");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let unchanged_ids: Vec<i64> = conn
            .prepare("SELECT id FROM papertrail_distill_sources ORDER BY source_ordinal")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(first_ids, unchanged_ids, "unchanged snapshots are left untouched");
        assert_eq!(first_hash, input_hash(&conn, "5"));

        conn.execute("UPDATE papertrail_items SET body='replacement' WHERE item_key='5'", [])
            .unwrap();
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        let body: String = conn
            .query_row(
                "SELECT exact_text FROM papertrail_distill_sources WHERE source_part='body'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(body, "replacement");
        assert_ne!(first_hash, input_hash(&conn, "5"));
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_units WHERE source_ordinal=1"
            ),
            1,
            "old body spans are replaced rather than appended",
        );
    }

    #[test]
    fn otherwise_identical_snapshots_have_repo_isolated_hashes() {
        let conn_a = scoped_conn("repoA");
        seed_item(&conn_a, "repoA", "issue", "5", "same", "closed", None);
        extract(&conn_a, None, &ExtractOptions::default()).unwrap();
        let conn_b = scoped_conn("repoB");
        seed_item(&conn_b, "repoB", "issue", "5", "same", "closed", None);
        extract(&conn_b, None, &ExtractOptions::default()).unwrap();
        assert_ne!(input_hash(&conn_a, "5"), input_hash(&conn_b, "5"));
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
        conn.execute(
            "UPDATE papertrail_distill SET prompt_version = ?1, model_input_hash = 'sha256:model'",
            [i64::from(crate::distill::prompts::PROMPT_VERSION)],
        )
        .unwrap();
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
    fn a_prompt_version_change_invalidates_model_output_and_requeues() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "refactor: x");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        conn.execute_batch(
            "UPDATE papertrail_distill SET
                 root_cause = 'old result', prompt_version = 0, model_input_hash = 'sha256:old';
             UPDATE papertrail_distill_anchors SET selected = 1;
             DELETE FROM papertrail_distill_queue;",
        )
        .unwrap();

        extract(&conn, None, &ExtractOptions::default()).unwrap();

        let row: (Option<String>, Option<i64>, Option<String>, i64) = conn
            .query_row(
                "SELECT root_cause, prompt_version, model_input_hash,
                        (SELECT COUNT(*) FROM papertrail_distill_queue)
                 FROM papertrail_distill WHERE repo_id = 'repoA' AND item_key = '9'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (None, None, None, 1));
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE selected=1"
            ),
            0,
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
    fn unchanged_pending_work_preserves_failure_attempt_state() {
        let conn = scoped_conn("repoA");
        seed_item(&conn, "repoA", "change_request", "9", "A PR.", "merged", Some("cafe"));
        seed_commit(&conn, "repoA", "cafe", "feat: x");
        extract(&conn, None, &ExtractOptions::default()).unwrap();
        conn.execute(
            "UPDATE papertrail_distill_queue
             SET attempts = 2, last_error = 'bad reply', raw_reply = 'raw', enqueued_at_ms = 7
             WHERE item_key = '9'",
            [],
        )
        .unwrap();

        extract(&conn, None, &ExtractOptions::default()).unwrap();

        let row: (i64, Option<String>, Option<String>, i64) = conn
            .query_row(
                "SELECT attempts, last_error, raw_reply, enqueued_at_ms
                 FROM papertrail_distill_queue WHERE item_key = '9'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (2, Some("bad reply".into()), Some("raw".into()), 7));
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
                 quotes_materialized=3, prompt_version=?1, model_input_hash='sha256:model'
             WHERE item_key='9'",
            [i64::from(crate::distill::prompts::PROMPT_VERSION)],
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
        conn.execute(
            "INSERT INTO papertrail_distill_anchors
                 (tracker, project, item_kind, item_key, anchor_kind, file_path, name, resolved,
                  candidate_ordinal, selected, repo_id)
             VALUES ('github','o/r','change_request','9','file','src/lib.rs','src/lib.rs',1,
                     0,1,'repoA')",
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
        let stamps: (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT prompt_version, model_input_hash FROM papertrail_distill WHERE \
                 item_key='9'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            stamps,
            (Some(i64::from(crate::distill::prompts::PROMPT_VERSION)), Some("sha256:model".into()))
        );
        assert_eq!(distill_count(&conn, "SELECT COUNT(*) FROM papertrail_distill_evidence"), 1);
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE selected = 1"
            ),
            1,
            "identical rerun keeps model-selected anchors",
        );

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
        let stamps_after: (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT prompt_version, model_input_hash FROM papertrail_distill WHERE \
                 item_key='9'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stamps_after, (None, None), "regeneration clears model-input stamps");
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
        assert_eq!(
            distill_count(
                &conn,
                "SELECT COUNT(*) FROM papertrail_distill_anchors WHERE selected = 1"
            ),
            0,
            "regeneration clears stale anchor selections",
        );
    }
}
