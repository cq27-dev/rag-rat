//! Issue-tracker papertrail for the rag-rat workspace: the provider-neutral whole-project
//! mirror (items, comments, refs, closing metadata), the GitHub/GitLab clients on a shared
//! rate-governed transport, ref-grammar parsing, evidence queries, and automatic sync
//! scheduling. Sits on rag-rat-db + rag-rat-base; the engine crate supplies scheduling
//! callers and query-surface integration.

mod api;
mod distill;
mod distill_read;
mod distill_status;
mod evidence;
mod github;
mod gitlab;
mod http;
mod mirror;
mod parse;
mod schedule;
mod store;
mod sync;
mod trackers;
pub mod transport;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

pub use api::{sync_from_refs, sync_from_refs_with_progress, *};
pub use distill::{
    AnchorKind, DistillEdgeKind, EpistemicStatus, FixEdgeSource, OutcomeStatus, ThreadShape,
};
pub use distill_read::{
    CoalescedThread, DistilledRecord, DriveByRecord, PathDistilledRecord, RecordKey,
    RejectedAlternative, distilled_record_for_thread, records_for_path, records_for_symbol,
};
pub use distill_status::{EffectiveStatusInputs, effective_status, no_fix_edge};
pub use evidence::{refs, *};
pub(crate) use github::*;
pub(crate) use gitlab::*;
pub use http::*;
pub use mirror::MirrorBindingReport;
pub(crate) use mirror::{
    MirrorContinuation, days_in_month, load_mirror_continuation, max_timestamp, mirror_binding,
    parse_date,
};
pub use parse::{TrackerParsedRef, parse_tracker_refs, *};
pub use rag_rat_base::config::Tracker;
use rag_rat_base::time::now_ms;
use rusqlite::{Connection, OptionalExtension, params};
pub use schedule::{AutosyncRequest, *};
use serde::{Deserialize, Serialize};
pub use store::{rebuild_fts, *};
pub use sync::*;
pub(crate) use trackers::resolve_trackers;
pub use trackers::{
    ResolvedTracker, auto_detect_tracker, detect_tracker_for_remote,
    detect_tracker_from_remote_url, normalized_tags,
};

/// Resolved tracker context, injected into the sync/query paths instead of being resolved inside
/// the library. Resolution runs ONLY at the real-usage boundary (`IndexDatabase::open_config`)
/// and never in tests, which pass an explicit context (#60): it reads local git state (the
/// remote URL). Authentication is carried per binding; there is no process-global `gh`
/// capability bit.
#[derive(Debug, Clone, Default)]
pub struct PapertrailContext {
    /// Resolved tracker bindings, in config order (or the single auto-detected code host).
    /// Production discovery, rationale lookup, and manual mirror sync consume every binding.
    pub trackers: Vec<ResolvedTracker>,
    pub transport_options: transport::TransportOptions,
    pub schedule: rag_rat_base::config::PapertrailConfig,
}

impl PapertrailContext {
    /// Resolve from the repo config: `[[tracker]]` bindings when present, otherwise a binding
    /// auto-detected from the git `origin` remote. Call ONLY at the real-usage boundary
    /// (open_config), never inside the library internals or tests.
    pub fn resolve(config: &rag_rat_base::config::Config) -> Self {
        let mut trackers = resolve_trackers(&config.trackers, &config.root);
        // GitHub's provider-native fallback is environment auth. Materialize the ENV-VAR NAME
        // into the binding (never its value) so status and transport use the same per-binding
        // capability instead of a process-global `gh_available` bit.
        apply_implicit_github_auth(&mut trackers, |name| std::env::var(name).ok());
        Self {
            trackers,
            transport_options: transport::TransportOptions::from(&config.papertrail),
            schedule: config.papertrail.clone(),
        }
    }

    /// An explicit GitHub-repo context that never touches git or `gh` — for tests and non-gh
    /// callers.
    pub fn new(default_repo: Option<&str>) -> Self {
        let trackers = default_repo
            .map(|project| ResolvedTracker {
                provider: Tracker::Github,
                project: project.to_string(),
                base_url: None,
                auth: None,
                authentication: TrackerAuthentication::AuthMissing,
                tags: Vec::new(),
            })
            .into_iter()
            .collect();
        Self {
            trackers,
            transport_options: transport::TransportOptions::default(),
            schedule: rag_rat_base::config::PapertrailConfig::default(),
        }
    }

    /// `owner/repo` qualifying a manually requested legacy GitHub reference. Production
    /// discovery and rationale parsing use all bindings through [`parse_tracker_refs`].
    fn default_repo(&self) -> Option<&str> {
        self.trackers
            .iter()
            .find(|tracker| tracker.provider == Tracker::Github)
            .map(|tracker| tracker.project.as_str())
    }
}

fn apply_implicit_github_auth(
    trackers: &mut [ResolvedTracker],
    env: impl Fn(&str) -> Option<String>,
) {
    for tracker in trackers {
        if tracker.provider == Tracker::Github
            && tracker.base_url.is_none()
            && tracker.auth.is_none()
            && let Some(name) = ["GH_TOKEN", "GITHUB_TOKEN"]
                .into_iter()
                .find(|name| env(name).is_some_and(|value| !value.trim().is_empty()))
        {
            tracker.auth = Some(rag_rat_base::config::TrackerAuth::Env(name.to_string()));
            tracker.authentication = TrackerAuthentication::AuthConfigured;
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailStatus {
    pub refs: u64,
    pub issues: u64,
    pub change_requests: u64,
    pub comments: u64,
    pub last_sync_ms: Option<i64>,
    pub capabilities: Vec<TrackerCapability>,
    pub bindings: Vec<PapertrailBindingStatus>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PapertrailBindingStatus {
    pub tracker: Tracker,
    pub project: String,
    pub last_attempt_ms: Option<i64>,
    pub last_successful_probe_ms: Option<i64>,
    pub last_successful_mirror_ms: Option<i64>,
    pub last_full_walk_ms: Option<i64>,
    pub retry_not_before_ms: Option<i64>,
    pub full_walk_in_progress: bool,
    pub error_class: Option<PapertrailErrorClass>,
    pub error_detail: Option<String>,
    /// The last HARD attested-closers walk failure, if any — the provider-closer ENRICHMENT lane
    /// (#702 stage 2). Independent of `error_class`/`failed`: the item mirror can be healthy while
    /// this lane fails every tick with a stalled watermark. `None` once a clean attested walk
    /// runs.
    pub attested_error: Option<String>,
    pub overdue: bool,
    pub failed: bool,
}

/// Per-binding auth and live-sync capability. Environment auth reflects token presence; command
/// auth reflects a configured source without running shell code on read paths. The transport
/// resolves either source when a network client is built.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TrackerCapability {
    pub tracker: Tracker,
    pub project: String,
    pub authentication: TrackerAuthentication,
    pub synchronization: TrackerSynchronization,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackerAuthentication {
    AuthConfigured,
    AuthMissing,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackerSynchronization {
    Native,
    ProviderClientPending,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailSyncReport {
    pub offline: bool,
    pub discovered_refs: usize,
    pub skipped_refs: usize,
    pub failed_refs: usize,
    pub synced_items: usize,
    /// One resumable outcome per dispatched binding, including rate-governed pauses.
    pub bindings: Vec<MirrorBindingReport>,
    pub errors: Vec<PapertrailSyncError>,
    pub status: PapertrailStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailSyncError {
    pub tracker: Tracker,
    pub project: String,
    pub item_key: String,
    pub status: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct PapertrailSyncProgress {
    pub current: usize,
    pub total: usize,
    pub project: String,
    pub item_key: String,
    pub action: PapertrailSyncAction,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PapertrailSyncAction {
    Syncing,
    Skipped,
    Synced,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailRef {
    pub tracker: Tracker,
    pub project: String,
    pub item_key: String,
    pub item_kind: Option<ItemKind>,
    pub ref_kind: String,
    pub source_kind: String,
    pub source_path: Option<String>,
    pub source_commit: Option<String>,
    pub source_text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PapertrailEvidence {
    pub tracker: String,
    pub project: String,
    /// Kind of the ITEM this row belongs to: `issue` | `change_request`.
    pub item_kind: String,
    pub item_key: String,
    /// Which mirror row matched: the item's own text (`item`) or one of its comments (`comment`).
    pub doc_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_id: Option<String>,
    pub url: String,
    pub title: String,
    pub snippet: String,
    /// Coarse keyword label (`classify_text`) kept for the internal eval harness only. It is NOT a
    /// read surface: the distilled decision record (`record`, #705) supersedes it, so it is never
    /// serialized to a retrieval payload or drive-by. Retained (not deleted) because the eval
    /// harness still scores against it; the `papertrail_fts.classification` column keeps
    /// populating it. Do not resurface it — read `record` instead.
    #[serde(skip_serializing)]
    pub classification: String,
    pub evidence_kind: &'static str,
    pub score: f64,
    /// The distilled decision record for this thread (#705), when one exists — the model's
    /// root-cause/decision/outcome over the thread, replacing a bare item/comment match with the
    /// resolved rationale. `None` until the thread is distilled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<DistilledRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Papertrail {
    pub current_source: Option<CurrentSourceEvidence>,
    pub evidence: Vec<PapertrailEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback_evidence: Vec<PapertrailEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentSourceEvidence {
    pub chunk_id: Option<i64>,
    pub path: String,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub symbol: Option<String>,
}

/// Whether a provider's item keys are unique across kinds. GitHub's shared issue/PR numbering
/// is the exception (a key names at most one item, and its repo comment feed cannot always name
/// the kind); namespaced providers (GitLab iids, Bitbucket, Jira) always name the kind on their
/// comments, so a comment there must NEVER attach across namespaces.
pub(crate) fn item_numbering_is_shared(tracker: Tracker) -> bool {
    matches!(tracker, Tracker::Github)
}

/// Provider-neutral item kind. GitHub's shared issue/PR numbering is the exception, not the
/// rule — GitLab issues and merge requests live in separate iid namespaces and Jira has no
/// change requests at all — so the kind is part of an item's identity, never inferred.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Issue,
    ChangeRequest,
}

impl ItemKind {
    /// The exact persisted token (`papertrail_items.item_kind` and friends).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown item kind token `{value}`"))
    }
}

/// Provider-NEUTRAL outcome of a closed item (`papertrail_items.resolution`). GitHub's
/// `state_reason` maps in (`completed` / `not_planned` / `duplicate`); Jira's resolution field
/// maps in richer (its `Duplicate` / `Won't Do` map to the same tokens); GitLab has no
/// first-class resolution and stays `unknown`. `superseded` has no GitHub source today — it is
/// reserved for providers that attest it and for the distill layer's supersession chains.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ItemResolution {
    Completed,
    NotPlanned,
    Duplicate,
    Superseded,
    Unknown,
}

impl ItemResolution {
    /// The exact persisted token (`papertrail_items.resolution`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown item resolution token `{value}`"))
    }
}

/// Cross-provider lifecycle state (`papertrail_items.state_normalized`). THE TRAP this exists
/// for: GitLab merged MRs carry raw `state = 'merged'` while GitHub merged PRs carry
/// `state = 'closed'` + `merged_at` — a consumer filtering raw `WHERE state = 'closed'`
/// silently drops every merged GitLab MR. Consumers filter on this column, never on raw state.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum NormalizedState {
    Open,
    Closed,
    Merged,
}

impl NormalizedState {
    /// Derive from the provider-truthful pair: a raw `merged` state (GitLab) or a recorded
    /// `merged_at` (GitHub) wins over raw `closed`; anything else non-closed is open. The same
    /// rule the V073 backfill applies to pre-existing rows.
    pub fn derive(state: &str, merged_at: Option<&str>) -> Self {
        if state == "merged" || merged_at.is_some() {
            Self::Merged
        } else if state == "closed" {
            Self::Closed
        } else {
            Self::Open
        }
    }

    /// The exact persisted token (`papertrail_items.state_normalized`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown normalized state token `{value}`"))
    }
}

/// What closed an issue (`papertrail_closing_edges.closer_kind`): a change request (its key in
/// the same project) or a direct commit (the full sha).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum CloserKind {
    ChangeRequest,
    Commit,
}

impl CloserKind {
    /// The exact persisted token (`papertrail_closing_edges.closer_kind`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown closer kind token `{value}`"))
    }
}

/// Trust tier of a closing edge (`papertrail_closing_edges.source`): `provider` rows are
/// attested by the tracker's own closing data; `text` rows are mined from commit/item/comment
/// text. The store upsert never downgrades `provider` back to `text`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ClosingEdgeSource {
    Provider,
    Text,
}

impl ClosingEdgeSource {
    /// The exact persisted token (`papertrail_closing_edges.source`).
    pub fn as_db_str(self) -> &'static str {
        self.into()
    }

    /// Parse a persisted token, rejecting anything outside the closed set.
    pub fn from_db_str(value: &str) -> anyhow::Result<Self> {
        value.parse().map_err(|_| anyhow::anyhow!("unknown closing-edge source token `{value}`"))
    }
}

/// One page of PROVIDER-ATTESTED closure data (#702 stage 2): closing edges the tracker itself
/// asserts (GraphQL closing references, closed-event closers) plus per-item outcome updates.
#[derive(Debug, Clone, Default)]
pub struct AttestedClosersPage {
    pub edges: Vec<ClosingEdge>,
    pub item_updates: Vec<AttestedItemUpdate>,
    /// Issues whose ClosedEvent closer was re-read this page. Reaping is ISSUE-KEYED: an issue has
    /// exactly ONE authoritative closer (its last ClosedEvent), so re-reading it lets the walk
    /// replace EVERY provider closer edge targeting it (commit AND change-request), then reinsert
    /// the current closer. This is why there is no closer-keyed (per-PR) replace-set: a PR closes
    /// many issues, so reaping the PR's outgoing edges would clobber UI-linked rows the PR phase
    /// (`closingIssuesReferences`) never sees — those live only on the issue's ClosedEvent.
    pub replaced_issue_closers: Vec<String>,
    /// Opaque continuation for the NEXT page; `None` when the walk is complete.
    pub next: Option<String>,
    /// The newest `updated_at` seen on the FIRST page — the next walk's `since` watermark once
    /// this walk completes.
    pub frontier: Option<String>,
}

/// A provider-attested outcome update for a CACHED item (never creates rows).
#[derive(Debug, Clone)]
pub struct AttestedItemUpdate {
    pub item_kind: ItemKind,
    pub item_key: String,
    pub resolution: Option<ItemResolution>,
    /// INVARIANT: applied only to items the store already normalized as merged.
    pub merge_commit_sha: Option<String>,
}

/// One issue↔closer edge (`papertrail_closing_edges`). First-class — NOT a `papertrail_refs`
/// row: the ref layer's identity coalesces on `source_text` and its contract is
/// annotation-only, while a closing edge's identity is the (issue, closer) pair and its trust
/// tier rides the `source` attribute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosingEdge {
    pub project: String,
    pub issue_kind: ItemKind,
    pub issue_key: String,
    pub closer_kind: CloserKind,
    /// The closer item's key (`closer_kind = change_request`) or the full commit sha
    /// (`closer_kind = commit`).
    pub closer_key: String,
    /// The closing/merge commit sha when known (a change request closer's merge commit).
    pub closer_commit: Option<String>,
    pub source: ClosingEdgeSource,
}

/// One tracker item (issue or change request), normalized across providers. `item_key` is a
/// string because provider keys are not uniformly numeric (Jira: `PROJ-123`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PapertrailItem {
    /// Provider project path (`owner/repo` on GitHub).
    pub project: String,
    pub item_kind: ItemKind,
    pub item_key: String,
    pub url: String,
    pub state: String,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Change requests only; `None` for issues and unmerged change requests.
    pub merged_at: Option<String>,
    /// When the item closed — the temporal axis for supersession ordering (`created_at` is
    /// NOT it).
    #[serde(default)]
    pub closed_at: Option<String>,
    /// Provider-neutral outcome; `None` when the provider attests nothing (open items, GitLab).
    #[serde(default)]
    pub resolution: Option<ItemResolution>,
    /// INVARIANT: `Some` ONLY for merged change requests. GitHub returns a non-null
    /// `merge_commit_sha` for closed-UNMERGED PRs too (its ephemeral test-merge commit,
    /// possibly on no branch); providers must gate on the merged state, never on the field's
    /// presence.
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    /// Thread-shape facets, already in the parsed payloads (`user.type`, `author_association`).
    #[serde(default)]
    pub author_kind: Option<String>,
    #[serde(default)]
    pub author_association: Option<String>,
    /// Provider facets used by the binding's client-side OR filter (GitHub labels today).
    pub tags: Vec<String>,
}

/// One comment on a tracker item, unified across the provider review models: a plain thread
/// comment carries neither optional; a review event carries `review_state` (e.g. `APPROVED`);
/// a file-anchored comment carries `anchor_path`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PapertrailComment {
    pub project: String,
    /// Kind of the parent item — part of the parent identity, because namespaced providers
    /// (GitLab) can have an issue and a change request sharing the same key.
    pub item_kind: ItemKind,
    pub item_key: String,
    /// Provider comment id, normalized into a source-qualified namespace when one provider has
    /// multiple comment resources (for example `comment:9`, `review:9`, `review_comment:9`).
    pub comment_id: String,
    pub url: Option<String>,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// Thread-shape facets (`user.type: "Bot"` etc.), already in the parsed payloads.
    #[serde(default)]
    pub author_kind: Option<String>,
    #[serde(default)]
    pub author_association: Option<String>,
    /// `Some` marks a review event (approval / changes requested / review summary).
    pub review_state: Option<String>,
    /// `Some` marks a file-anchored comment; the value is the repo-relative path.
    pub anchor_path: Option<String>,
}

/// Position in a provider's `updated_at`-ordered item/comment stream. The mirror sync keeps one
/// persisted cursor per (tracker, project); a page fetch returns the cursor to continue from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageCursor {
    /// Provider-defined logical stream within an operation (for example GitHub issue comments
    /// versus review comments). Each stream owns independent durable progress.
    pub stream: Option<String>,
    /// Fetch entries updated at-or-after this provider-format timestamp.
    pub updated_since: Option<String>,
    /// Strict upper boundary for newest-first historical descent.
    pub updated_before: Option<String>,
    /// Provider-opaque continuation token, when the provider paginates by token.
    pub page_token: Option<String>,
    /// Provider-opaque state needed to interpret a continuation (for example a Search tie
    /// boundary and the number of results already observed). The mirror persists but never
    /// interprets this value.
    pub provider_state: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FreshnessProbe {
    pub updated_since: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FreshnessResult {
    pub latest: Option<String>,
    pub etag: Option<String>,
    pub not_modified: bool,
}

#[derive(Debug, Clone)]
pub struct ItemsPage {
    pub items: Vec<PapertrailItem>,
    pub next: Option<PageCursor>,
    /// Provider-confirmed strict boundary below every item consumed in this logical backfill
    /// page. Compound provider streams may need this because the last physical response does not
    /// necessarily contain the oldest item observed across the whole page.
    pub backfill_boundary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CommentsPage {
    pub comments: Vec<PapertrailComment>,
    pub next: Option<PageCursor>,
    /// Provider-confirmed consumed watermark for THIS page: everything in the stream at or
    /// before it has been returned or deliberately skipped. The mirror trusts it on EVERY page
    /// (unlike the returned comments, whose maximum is trusted only on the scan's first page),
    /// so only feeds with immutable append-only ordering may set it — GitLab's events feed,
    /// ordered by creation. It carries a drained multi-page window past its LAST page (a
    /// first-page-only frontier pins a busy day forever) and advances past entries that map to
    /// no comment (commit/snippet notes). Mutable updated-order pages (GitHub) leave it None.
    pub frontier: Option<String>,
}

/// Provider outcomes that affect mirror control flow rather than representing a retryable
/// transport or payload failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PapertrailClientError {
    #[error("tracker item not found")]
    ItemNotFound,
}

/// Advance decision for a freshness probe: the newest `updated_at` the provider reported vs the
/// cursor's position. `Some(latest)` when the tracker moved past the cursor, `None` when nothing
/// did (provider timestamps compare lexicographically — ISO-8601 UTC).
pub(crate) fn probe_advance(latest: Option<String>, since: Option<&str>) -> Option<String> {
    match (latest, since) {
        (Some(latest), Some(since)) if latest.as_str() <= since => None,
        (latest, _) => latest,
    }
}

/// Provider client behind the papertrail sync. Implementations exist per tracker (GitHub today;
/// GitLab / Bitbucket / Jira to follow) and own URL building, pagination, and payload mapping.
///
/// The futures are driven exclusively through [`block_on`] on a private current-thread runtime —
/// nothing here is spawned — so the omitted `Send` bounds of `async fn` in a public trait are
/// deliberate, not an oversight.
#[allow(async_fn_in_trait)]
pub trait PapertrailClient {
    /// One item. `kind` is part of the identity: namespaced providers (GitLab) MUST treat it as
    /// binding — an issue and a change request can share a key — while a shared-numbering
    /// provider (GitHub) treats it as advisory and returns the item with its kind resolved.
    async fn item(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<PapertrailItem>;
    /// Every comment on one item, unified (thread comments, review events, file-anchored).
    /// `kind` identifies the parent exactly as in [`Self::item`] — pass the RESOLVED kind.
    async fn item_comments(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<PapertrailComment>>;
    /// Stable logical comment streams for one item. Providers with several independently paged
    /// resources expose each one here so the mirror can checkpoint after every returned page.
    fn item_comment_streams(&self, _kind: ItemKind) -> &'static [&'static str] {
        &["default"]
    }

    /// One page from exactly one [`Self::item_comment_streams`] lane. The default adapts legacy
    /// clients that return a whole thread in one call; native paginated clients override it.
    async fn item_comments_page(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        anyhow::ensure!(cursor.page_token.is_none(), "legacy item comments cannot resume a page");
        Ok(CommentsPage {
            comments: self.item_comments(project, kind, key).await?,
            next: None,
            frontier: None,
        })
    }
    /// Complete provider-specific item fields before the mirror starts the item's durable thread.
    /// The mirror invokes this one item at a time and checkpoints each completed item, so a
    /// transport pause cannot force an entire provider page to be enriched again.
    async fn enrich_item(&self, _item: &mut PapertrailItem) -> anyhow::Result<()> {
        Ok(())
    }
    /// A page of items updated since the cursor, newest first (mirror backfill/delta).
    async fn items_page(&self, project: &str, cursor: &PageCursor) -> anyhow::Result<ItemsPage>;
    /// Stable logical comment streams whose cursors must advance independently.
    fn comment_streams(&self) -> &'static [&'static str] {
        &["default"]
    }

    /// A page of comments from exactly one [`Self::comment_streams`] lane. Implementations must not
    /// advance into another stream through `next`; the mirror persists each stream
    /// independently.
    async fn comments_page(
        &self,
        project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage>;
    /// Cheap "anything changed?" check: `Some(latest updated_at)` when the tracker has items
    /// newer than the cursor, `None` when nothing moved.
    async fn freshness_probe(
        &self,
        project: &str,
        probe: &FreshnessProbe,
    ) -> anyhow::Result<FreshnessResult>;
    /// One page of provider-attested closure data for `project`, `Ok(None)` when this provider
    /// has no attested supply (or its capability probe failed — e.g. a GHE build without the
    /// GraphQL endpoint). `cursor` is the previous page's `next`; `since` is the completed-walk
    /// watermark — implementations stop paging once nodes fall behind it.
    async fn attested_closers_page(
        &self,
        _project: &str,
        _cursor: Option<&str>,
        _since: Option<&str>,
    ) -> anyhow::Result<Option<AttestedClosersPage>> {
        Ok(None)
    }
}

/// Drive a papertrail future to completion from the synchronous call paths (CLI, maintenance).
/// Uses a private lazily-built current-thread runtime. Callable from inside a multi-thread tokio
/// runtime too (the worker is demoted via `block_in_place`, matching how the pre-async code
/// simply blocked). A caller on a CURRENT-THREAD ambient runtime has no sound way to block the
/// thread — and the future cannot be offloaded, it borrows the `!Sync` connection — so that case
/// returns an error instead of panicking; such callers need an async entry point.
pub fn block_on<T>(
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("papertrail runtime")
    });
    match tokio::runtime::Handle::try_current() {
        Err(_) => runtime.block_on(future),
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
            tokio::task::block_in_place(|| runtime.block_on(future)),
        Ok(_) => anyhow::bail!(
            "papertrail sync cannot block inside a current-thread tokio runtime; drive the async \
             sync entry points on that runtime instead"
        ),
    }
}

#[derive(Default)]
pub struct SyncRefsReport {
    synced_items: usize,
    skipped_refs: usize,
    failed_refs: usize,
    errors: Vec<PapertrailSyncError>,
}

pub(crate) struct FtsRow<'a> {
    tracker: &'a str,
    project: &'a str,
    item_kind: &'a str,
    item_key: &'a str,
    /// `item` for the item's own title/body row, `comment` for a comment row.
    doc_kind: &'a str,
    /// The provider comment id for a `comment` row; empty string on `item` rows.
    comment_id: &'a str,
    url: &'a str,
    title: &'a str,
    body: &'a str,
    /// The active repo that owns this papertrail row (V041). Stamped into
    /// `papertrail_fts.repo_id` (UNINDEXED) so a MATCH in a consolidated DB filters to one repo.
    repo_id: &'a str,
}

/// A tracker reference parsed out of source/commit/branch text. The grammar is GitHub's today
/// (`#N`, `owner/repo#N`, `GH-N`, issue/PR URLs), so the parsed key is numeric; `project` is the
/// `owner/repo` path.
#[derive(Debug, Clone)]
pub(crate) struct ParsedRef {
    project: String,
    number: i64,
    kind: String,
}

impl ParsedRef {
    /// Lift a parsed GitHub-grammar ref into a storable [`PapertrailRef`] with its discovery
    /// provenance. The grammar is GitHub-only today, so the tracker is fixed here.
    pub(crate) fn into_ref(
        self,
        source_kind: &str,
        source_path: Option<String>,
        source_commit: Option<String>,
        source_text: String,
    ) -> PapertrailRef {
        PapertrailRef {
            tracker: Tracker::Github,
            project: self.project,
            item_key: self.number.to_string(),
            item_kind: None,
            ref_kind: self.kind,
            source_kind: source_kind.to_string(),
            source_path,
            source_commit,
            source_text,
        }
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    // The persisted token sets are CLOSED and exact: `from_db_str` must round-trip every
    // `as_db_str` output and reject everything else (no trimming, no case folding) — a papertrail
    // row read back with a drifted token is a bug to surface, not to coerce.
    #[test]
    fn tracker_tokens_are_exact_and_closed() {
        for (tracker, token) in [
            (Tracker::Github, "github"),
            (Tracker::Gitlab, "gitlab"),
            (Tracker::Bitbucket, "bitbucket"),
            (Tracker::Jira, "jira"),
        ] {
            assert_eq!(tracker.as_db_str(), token);
            assert_eq!(Tracker::from_db_str(token).unwrap(), tracker);
        }
        for rejected in ["GitHub", "GITHUB", " github", "github ", "sourcehut", ""] {
            assert!(Tracker::from_db_str(rejected).is_err(), "must reject `{rejected}`");
        }
    }

    #[test]
    fn item_kind_tokens_are_exact_and_closed() {
        assert_eq!(ItemKind::Issue.as_db_str(), "issue");
        assert_eq!(ItemKind::ChangeRequest.as_db_str(), "change_request");
        assert_eq!(ItemKind::from_db_str("issue").unwrap(), ItemKind::Issue);
        assert_eq!(ItemKind::from_db_str("change_request").unwrap(), ItemKind::ChangeRequest);
        for rejected in ["Issue", "pull", "change-request", " issue", ""] {
            assert!(ItemKind::from_db_str(rejected).is_err(), "must reject `{rejected}`");
        }
    }

    #[test]
    fn implicit_cloud_token_never_crosses_an_enterprise_binding() {
        let tracker = |base_url: Option<&str>| ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: base_url.map(str::to_string),
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        };
        let mut trackers = vec![tracker(None), tracker(Some("https://github.example.com"))];
        apply_implicit_github_auth(&mut trackers, |name| {
            (name == "GH_TOKEN").then(|| "cloud-secret".to_string())
        });

        assert!(matches!(
            trackers[0].auth,
            Some(rag_rat_base::config::TrackerAuth::Env(ref name)) if name == "GH_TOKEN"
        ));
        assert_eq!(trackers[0].authentication, TrackerAuthentication::AuthConfigured);
        assert!(trackers[1].auth.is_none());
        assert_eq!(trackers[1].authentication, TrackerAuthentication::AuthMissing);
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    #[test]
    fn probe_advance_only_reports_movement_past_the_cursor() {
        assert_eq!(probe_advance(None, None), None);
        assert_eq!(probe_advance(None, Some("2026-01-01T00:00:00Z")), None);
        assert_eq!(
            probe_advance(Some("2026-01-02T00:00:00Z".into()), None).as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
        assert_eq!(
            probe_advance(Some("2026-01-02T00:00:00Z".into()), Some("2026-01-01T00:00:00Z"))
                .as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
        // At-or-before the cursor is quiet — equality must not loop the delta lane forever.
        assert_eq!(
            probe_advance(Some("2026-01-01T00:00:00Z".into()), Some("2026-01-01T00:00:00Z")),
            None
        );
        assert_eq!(
            probe_advance(Some("2025-12-31T00:00:00Z".into()), Some("2026-01-01T00:00:00Z")),
            None
        );
    }

    // The bridge must behave like the pre-async code (block) wherever blocking is sound, and
    // refuse gracefully where it is not — never panic.
    #[test]
    fn block_on_bridges_from_a_multi_thread_runtime_worker() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let value = runtime
            .block_on(async {
                tokio::task::spawn(async { block_on(async { anyhow::Ok(7) }) }).await.unwrap()
            })
            .unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn block_on_refuses_gracefully_on_a_current_thread_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err =
            runtime.block_on(async { block_on(async { anyhow::Ok(7) }) }).unwrap_err().to_string();
        assert!(err.contains("current-thread"), "{err}");
    }
}

/// Hooks for this crate's own scratch-DB test fixtures: the papertrail rebuild is REAL (these
/// tests exercise the legacy-migration path that rebuilds the mirror FTS), the foreign domains'
/// builders are no-ops (their tables are empty on a scratch DB).
#[cfg(test)]
pub(crate) fn test_hooks() -> rag_rat_db::MigrationHooks {
    rag_rat_db::MigrationHooks {
        rebuild_papertrail_fts: crate::rebuild_fts,
        ..rag_rat_db::MigrationHooks::noop()
    }
}

#[cfg(test)]
mod distill_substrate_tests {
    use super::*;

    #[test]
    fn distill_substrate_db_tokens_are_stable_and_round_trip() {
        // These tokens are SCHEMA (persisted in papertrail_items / papertrail_closing_edges);
        // a rename or reorder must not silently repoint stored rows.
        for (resolution, token) in [
            (ItemResolution::Completed, "completed"),
            (ItemResolution::NotPlanned, "not_planned"),
            (ItemResolution::Duplicate, "duplicate"),
            (ItemResolution::Superseded, "superseded"),
            (ItemResolution::Unknown, "unknown"),
        ] {
            assert_eq!(resolution.as_db_str(), token);
            assert_eq!(ItemResolution::from_db_str(token).unwrap(), resolution);
        }
        for (state, token) in [
            (NormalizedState::Open, "open"),
            (NormalizedState::Closed, "closed"),
            (NormalizedState::Merged, "merged"),
        ] {
            assert_eq!(state.as_db_str(), token);
            assert_eq!(NormalizedState::from_db_str(token).unwrap(), state);
        }
        for (kind, token) in
            [(CloserKind::ChangeRequest, "change_request"), (CloserKind::Commit, "commit")]
        {
            assert_eq!(kind.as_db_str(), token);
            assert_eq!(CloserKind::from_db_str(token).unwrap(), kind);
        }
        for (source, token) in
            [(ClosingEdgeSource::Provider, "provider"), (ClosingEdgeSource::Text, "text")]
        {
            assert_eq!(source.as_db_str(), token);
            assert_eq!(ClosingEdgeSource::from_db_str(token).unwrap(), source);
        }
        assert!(ItemResolution::from_db_str("wontfix").is_err(), "outside the closed set");
    }

    #[test]
    fn normalized_state_derivation_covers_the_provider_shapes() {
        // GitHub merged PR: closed + merged_at. GitLab merged MR: raw state 'merged'.
        assert_eq!(NormalizedState::derive("closed", Some("2026-01-03")), NormalizedState::Merged);
        assert_eq!(NormalizedState::derive("merged", None), NormalizedState::Merged);
        assert_eq!(NormalizedState::derive("closed", None), NormalizedState::Closed);
        assert_eq!(NormalizedState::derive("open", None), NormalizedState::Open);
        assert_eq!(NormalizedState::derive("opened", None), NormalizedState::Open);
    }
}
