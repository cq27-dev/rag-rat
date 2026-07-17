//! Native GitHub provider client. Endpoint construction, pagination decoding, quota-lane
//! selection, and GitHub payload mapping live here; mirror policy stays in `mirror`.

use reqwest::Url;
use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::transport::{GovernorRegistry, Transport, TransportOptions, TransportParams};
use super::*;

const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";
const SEARCH_BACKFILL_STREAM: &str = "search_backfill";

#[derive(Debug, Serialize, Deserialize)]
struct SearchContinuation {
    #[serde(default)]
    kind: SearchKind,
    #[serde(default)]
    cycle_before: Option<String>,
    #[serde(default)]
    cycle_boundary: Option<String>,
    boundary: Option<String>,
    total_count: Option<u64>,
    fetched: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchKind {
    #[default]
    Issue,
    PullRequest,
}

impl SearchKind {
    fn qualifier(self) -> &'static str {
        match self {
            Self::Issue => "is:issue",
            Self::PullRequest => "is:pull-request",
        }
    }
}

pub(crate) struct GitHubClient {
    api_origin: String,
    core: Transport,
    search: Transport,
    graphql: Transport,
    /// Capability memo: set after a probe-shaped failure (404/400 from a GHE build without the
    /// GraphQL endpoint) so a dead endpoint is not re-tried on every page of every sync.
    graphql_unavailable: std::sync::atomic::AtomicBool,
}

impl GitHubClient {
    pub fn new(
        binding: &ResolvedTracker,
        registry: &GovernorRegistry,
        options: TransportOptions,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(binding.provider == Tracker::Github, "not a GitHub binding");
        let (api_origin, authority) = resolve_api_origin(
            "GitHub",
            binding.base_url.as_deref(),
            "https://api.github.com",
            "/api/v3",
        )?;
        let token = transport::resolve_token(binding.auth.as_ref())?;
        let transport = |lane| {
            Transport::new_with_token(
                TransportParams {
                    provider: "github",
                    lane,
                    host: &authority,
                    auth: None,
                    registry,
                    options: options.clone(),
                },
                token.as_deref(),
            )
        };
        Ok(Self {
            api_origin,
            core: transport("core")?,
            search: transport("search")?,
            // GraphQL is metered on its OWN points budget — a distinct governor lane so REST
            // and GraphQL response headers can never corrupt each other's remaining-quota view.
            graphql: transport("graphql")?,
            // GitHub's GraphQL endpoint ALWAYS requires a token (unlike its public REST reads),
            // so a token-less public binding has no attested supply — start the lane disabled
            // rather than failing an unauthenticated GraphQL call on every sync.
            graphql_unavailable: std::sync::atomic::AtomicBool::new(token.is_none()),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.api_origin, path.trim_start_matches('/'))
    }

    async fn get_json(
        &self,
        transport: &Transport,
        url: &str,
    ) -> anyhow::Result<(Value, Option<String>)> {
        let response = transport.get(url, &github_headers()).await?;
        ensure_success(response.status, &response.body)?;
        let next = next_link(&response.headers)?;
        Ok((serde_json::from_str(&response.body)?, next))
    }

    async fn resolve_pull(&self, item: &mut PapertrailItem) -> anyhow::Result<()> {
        if item.item_kind == ItemKind::ChangeRequest {
            let url = self.endpoint(&format!("repos/{}/pulls/{}", item.project, item.item_key));
            let (value, _) = self.get_json(&self.core, &url).await?;
            enrich_item_from_pull_value(item, &value);
        }
        Ok(())
    }

    fn search_url(&self, project: &str, before: &str, kind: SearchKind) -> anyhow::Result<String> {
        validate_github_project(project)?;
        let mut url = Url::parse(&self.endpoint("search/issues"))?;
        url.query_pairs_mut()
            .append_pair("q", &format!("repo:{project} {} updated:<{before}", kind.qualifier()))
            .append_pair("sort", "updated")
            .append_pair("order", "desc")
            .append_pair("per_page", "100");
        Ok(url.into())
    }
}

impl GitHubClient {
    /// The GraphQL endpoint for this binding's API origin: cloud `api.github.com` serves
    /// `/graphql`; a GitHub Enterprise origin (`…/api/v3`) serves `…/api/graphql`.
    fn graphql_endpoint(&self) -> String {
        match self.api_origin.strip_suffix("/api/v3") {
            Some(base) => format!("{base}/api/graphql"),
            None => format!("{}/graphql", self.api_origin),
        }
    }

    /// One GraphQL request through the dedicated lane. `Ok(None)` = capability unavailable
    /// (memoized after a probe-shaped 400/404/410 — GHE builds without the endpoint).
    async fn graphql_query(&self, query: &str, variables: Value) -> anyhow::Result<Option<Value>> {
        use std::sync::atomic::Ordering;
        if self.graphql_unavailable.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let body = serde_json::json!({ "query": query, "variables": variables });
        let response =
            self.graphql.post_json(&self.graphql_endpoint(), &github_headers(), &body).await?;
        // 400/404/410 = no endpoint (GHE build without GraphQL); 401/403 = auth the lane can't
        // satisfy (token lacks scope, or SSO not authorized). All are capability-unavailable:
        // memoize and degrade to the text tier rather than retrying the impossible call.
        if matches!(response.status, 400 | 401 | 403 | 404 | 410) {
            self.graphql_unavailable.store(true, Ordering::Relaxed);
            return Ok(None);
        }
        ensure_success(response.status, &response.body)?;
        let value: Value = serde_json::from_str(&response.body)?;
        if let Some(errors) = value.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            // GitHub GraphQL signals a primary/secondary rate limit as HTTP 200 + an errors
            // payload of `type: RATE_LIMITED` (the transport's status-based limiter can't see
            // it). Map it to a transport PAUSE so `run_binding_job` honors the reset time instead
            // of recording the binding healthy with a stale watermark. `x-ratelimit-reset` is
            // epoch seconds; without it there is no resume time, so fall through to a plain error.
            let rate_limited = errors.iter().any(|error| {
                error["type"].as_str() == Some("RATE_LIMITED")
                    || error["message"]
                        .as_str()
                        .is_some_and(|message| message.to_ascii_lowercase().contains("rate limit"))
            });
            if rate_limited
                && let Some(reset_ms) = response
                    .headers
                    .get("x-ratelimit-reset")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<i64>().ok())
                    .map(|reset_s| reset_s.saturating_mul(1000))
            {
                return Err(anyhow::Error::new(transport::TransportError::Paused {
                    resume_at_ms: reset_ms,
                    reason: transport::PauseReason::RetryAfter,
                }));
            }
            anyhow::bail!("GraphQL errors: {}", serde_json::to_string(errors)?);
        }
        Ok(Some(value["data"].clone()))
    }
}

/// The two-phase attested-closers walk, encoded in the page cursor: `prs[:<after>]` then
/// `issues[:<after>]`.
fn attested_phase(cursor: Option<&str>) -> (&'static str, Option<String>) {
    match cursor {
        None | Some("prs") => ("prs", None),
        Some("issues") => ("issues", None),
        Some(other) => match other.split_once(':') {
            Some(("prs", after)) => ("prs", Some(after.to_string())),
            Some(("issues", after)) => ("issues", Some(after.to_string())),
            _ => ("prs", None),
        },
    }
}

const ATTESTED_PRS_QUERY: &str =
    "query($owner: String!, $name: String!, $after: String) { repository(owner: $owner, name: \
     $name) { pullRequests(states: [MERGED, CLOSED], first: 50, after: $after, orderBy: {field: \
     UPDATED_AT, direction: DESC}) { pageInfo { hasNextPage endCursor } nodes { number updatedAt \
     mergedAt mergeCommit { oid } closingIssuesReferences(first: 100) { pageInfo { hasNextPage } \
     nodes { number repository { nameWithOwner } } } } } } }";

const ATTESTED_ISSUES_QUERY: &str =
    "query($owner: String!, $name: String!, $after: String) { repository(owner: $owner, name: \
     $name) { issues(states: [CLOSED], first: 50, after: $after, orderBy: {field: UPDATED_AT, \
     direction: DESC}) { pageInfo { hasNextPage endCursor } nodes { number updatedAt stateReason \
     timelineItems(itemTypes: [CLOSED_EVENT], last: 1) { nodes { ... on ClosedEvent { closer { \
     __typename ... on Commit { oid } ... on PullRequest { number mergeCommit { oid } repository \
     { nameWithOwner } } } } } } } } } }";

impl PapertrailClient for GitHubClient {
    fn comment_streams(&self) -> &'static [&'static str] {
        &["issue_comments", "review_comments"]
    }

    async fn item(
        &self,
        project: &str,
        _kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<PapertrailItem> {
        validate_github_project(project)?;
        let url = self.endpoint(&format!("repos/{project}/issues/{key}"));
        let (value, _) = self.get_json(&self.core, &url).await?;
        validate_positive_id(&value, "number", "GitHub item")?;
        anyhow::ensure!(
            value["updated_at"].as_str().is_some(),
            "GitHub item has no valid updated_at"
        );
        let mut item = item_from_issue_value(project, &value);
        self.resolve_pull(&mut item).await?;
        Ok(item)
    }

    async fn item_comments(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<PapertrailComment>> {
        let mut comments = Vec::new();
        for stream in self.item_comment_streams(kind) {
            let mut cursor =
                PageCursor { stream: Some((*stream).to_string()), ..PageCursor::default() };
            loop {
                let page = self.item_comments_page(project, kind, key, &cursor).await?;
                comments.extend(page.comments);
                let Some(next) = page.next else { break };
                cursor = next;
            }
        }
        Ok(comments)
    }

    fn item_comment_streams(&self, kind: ItemKind) -> &'static [&'static str] {
        match kind {
            ItemKind::Issue => &["issue_comments"],
            ItemKind::ChangeRequest => &["issue_comments", "reviews", "review_comments"],
        }
    }

    async fn item_comments_page(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        validate_github_project(project)?;
        let stream = cursor.stream.as_deref().unwrap_or("issue_comments");
        let path = match stream {
            "issue_comments" => format!("repos/{project}/issues/{key}/comments"),
            "reviews" if kind == ItemKind::ChangeRequest => {
                format!("repos/{project}/pulls/{key}/reviews")
            },
            "review_comments" if kind == ItemKind::ChangeRequest => {
                format!("repos/{project}/pulls/{key}/comments")
            },
            _ => anyhow::bail!("unknown GitHub item-comment stream `{stream}` for {kind:?}"),
        };
        let url = cursor.page_token.clone().unwrap_or_else(|| with_per_page(self.endpoint(&path)));
        let (value, next_url) = self.get_json(&self.core, &url).await?;
        let values = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("GitHub item comments returned a non-array page"))?;
        for value in values {
            validate_positive_id(value, "id", "GitHub item comment")?;
        }
        let comments = values
            .iter()
            .map(|value| match stream {
                "issue_comments" => comment_from_value(project, kind, key, value),
                "reviews" => review_to_comment_from_value(project, kind, key, value),
                "review_comments" =>
                    review_comment_to_comment_from_value(project, kind, key, value),
                _ => unreachable!("validated stream"),
            })
            .collect();
        let next = next_url.map(|page_token| PageCursor {
            stream: Some(stream.to_string()),
            page_token: Some(page_token),
            ..PageCursor::default()
        });
        Ok(CommentsPage { comments, next, frontier: None })
    }

    async fn enrich_item(&self, item: &mut PapertrailItem) -> anyhow::Result<()> {
        self.resolve_pull(item).await
    }

    async fn items_page(&self, project: &str, cursor: &PageCursor) -> anyhow::Result<ItemsPage> {
        validate_github_project(project)?;
        if cursor.updated_before.is_some() && cursor.updated_since.is_some() {
            anyhow::bail!("an item page cannot be both delta and backfill");
        }
        let (transport, url, search) = if let Some(next) = cursor.page_token.clone() {
            let is_search = cursor.stream.as_deref() == Some(SEARCH_BACKFILL_STREAM)
                || next.contains("/search/issues?");
            (if is_search { &self.search } else { &self.core }, next, is_search)
        } else if let Some(before) = cursor.updated_before.as_deref() {
            (&self.search, self.search_url(project, before, SearchKind::Issue)?, true)
        } else {
            let mut url = Url::parse(&self.endpoint(&format!("repos/{project}/issues")))?;
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("state", "all")
                    .append_pair("sort", "updated")
                    // Keep the first page as a consumed prefix. The mirror advances only to that
                    // page's inclusive upper timestamp when mutable REST continuations exist.
                    .append_pair("direction", "asc")
                    .append_pair("per_page", "100");
                if let Some(since) = cursor.updated_since.as_deref() {
                    query.append_pair("since", since);
                }
            }
            (&self.core, url.into(), false)
        };
        let (value, next_url) = self.get_json(transport, &url).await?;
        let (values, next, backfill_boundary) = if search {
            let page = search_items(&value)?;
            let mut state = match cursor.provider_state.as_deref() {
                Some(state) => serde_json::from_str::<SearchContinuation>(state)?,
                None => SearchContinuation {
                    kind: SearchKind::Issue,
                    cycle_before: cursor.updated_before.clone(),
                    cycle_boundary: None,
                    boundary: None,
                    total_count: None,
                    fetched: 0,
                },
            };
            // Pre-split cursors represent one combined issue/PR Search stream. Finish that
            // persisted stream at its original tie boundary; switching it into the split cycle
            // would lose the outer backfill boundary when the new PR phase is empty.
            let legacy_combined = cursor.provider_state.is_some()
                && state.cycle_before.is_none()
                && state.cycle_boundary.is_none();
            if !legacy_combined {
                state.cycle_before = state.cycle_before.or_else(|| cursor.updated_before.clone());
            }
            if state.boundary.is_none() {
                state.boundary = search_boundary(page)?;
                state.total_count = value["total_count"].as_u64();
                state.fetched = 0;
                state.cycle_boundary =
                    safe_search_boundary(state.cycle_boundary.take(), state.boundary.clone());
            }
            state.fetched = state.fetched.saturating_add(page.len());
            let mut reached_older_item = false;
            let mut values = Vec::new();
            for item in page {
                let updated_at = search_item_updated_at(item)?;
                if state.boundary.as_deref().is_some_and(|boundary| updated_at < boundary) {
                    reached_older_item = true;
                    break;
                }
                values.push(item.clone());
            }
            let next_url = (!reached_older_item).then_some(next_url).flatten();
            if next_url.is_none()
                && !reached_older_item
                && let Some(total_count) = state.total_count
            {
                anyhow::ensure!(
                    u64::try_from(state.fetched).unwrap_or(u64::MAX) >= total_count,
                    "GitHub Search capped the result set before the timestamp tie was drained"
                );
            }
            let next = if let Some(page_token) = next_url {
                Some(PageCursor {
                    stream: Some(SEARCH_BACKFILL_STREAM.to_string()),
                    updated_before: if legacy_combined {
                        state.boundary.clone()
                    } else {
                        state.cycle_boundary.clone().or_else(|| state.cycle_before.clone())
                    },
                    page_token: Some(page_token),
                    provider_state: Some(serde_json::to_string(&state)?),
                    ..PageCursor::default()
                })
            } else if state.kind == SearchKind::Issue && !legacy_combined {
                let cycle_before = state
                    .cycle_before
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("GitHub Search continuation lost its cutoff"))?;
                state.kind = SearchKind::PullRequest;
                state.boundary = None;
                state.total_count = None;
                state.fetched = 0;
                Some(PageCursor {
                    stream: Some(SEARCH_BACKFILL_STREAM.to_string()),
                    updated_before: state
                        .cycle_boundary
                        .clone()
                        .or_else(|| Some(cycle_before.clone())),
                    page_token: Some(self.search_url(
                        project,
                        &cycle_before,
                        SearchKind::PullRequest,
                    )?),
                    provider_state: Some(serde_json::to_string(&state)?),
                    ..PageCursor::default()
                })
            } else {
                None
            };
            let boundary = if legacy_combined && next.is_none() {
                state.boundary.clone()
            } else {
                (state.kind == SearchKind::PullRequest && next.is_none())
                    .then(|| state.cycle_boundary.clone())
                    .flatten()
            };
            (values, next, boundary)
        } else {
            (
                value
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("GitHub issues response is not an array"))?
                    .to_vec(),
                next_url.map(|page_token| PageCursor {
                    stream: None,
                    updated_since: cursor.updated_since.clone(),
                    updated_before: cursor.updated_before.clone(),
                    page_token: Some(page_token),
                    provider_state: None,
                }),
                None,
            )
        };
        for value in &values {
            validate_positive_id(value, "number", "GitHub item")?;
            anyhow::ensure!(
                value["updated_at"].as_str().is_some(),
                "GitHub item has no valid updated_at"
            );
        }
        let items =
            values.iter().map(|value| item_from_issue_value(project, value)).collect::<Vec<_>>();
        Ok(ItemsPage { items, next, backfill_boundary })
    }

    async fn comments_page(
        &self,
        project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        validate_github_project(project)?;
        let since = cursor.updated_since.as_deref();
        let stream = |path: &str| -> anyhow::Result<String> {
            let mut url = Url::parse(&self.endpoint(path))?;
            let mut query = url.query_pairs_mut();
            query
                .append_pair("per_page", "100")
                .append_pair("sort", "updated")
                // Ascending order gives the mirror a conservative consumed frontier at the first
                // page's inclusive upper timestamp even though later page numbers are mutable.
                .append_pair("direction", "asc");
            if let Some(since) = since {
                query.append_pair("since", since);
            }
            drop(query);
            Ok(url.into())
        };
        let stream_name = cursor.stream.as_deref().unwrap_or("issue_comments");
        let (path, anchored) = match stream_name {
            "issue_comments" => (format!("repos/{project}/issues/comments"), false),
            "review_comments" => (format!("repos/{project}/pulls/comments"), true),
            other => anyhow::bail!("unknown GitHub comment stream `{other}`"),
        };
        let url = if let Some(next) = cursor.page_token.clone() { next } else { stream(&path)? };
        let (value, next_url) = self.get_json(&self.core, &url).await?;
        let values = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("GitHub comments response is not an array"))?;
        let mut comments = Vec::new();
        for value in values {
            if let Some(comment) = repo_comment_from_value(project, value, anchored) {
                validate_positive_id(value, "id", "GitHub repository comment")?;
                anyhow::ensure!(
                    value["updated_at"].as_str().is_some(),
                    "GitHub repository comment has no valid updated_at"
                );
                comments.push(comment);
            }
        }
        let next = next_url.map(|page_token| PageCursor {
            stream: Some(stream_name.to_string()),
            updated_since: cursor.updated_since.clone(),
            updated_before: None,
            page_token: Some(page_token),
            provider_state: None,
        });
        Ok(CommentsPage { comments, next, frontier: None })
    }

    async fn freshness_probe(
        &self,
        project: &str,
        probe: &FreshnessProbe,
    ) -> anyhow::Result<FreshnessResult> {
        validate_github_project(project)?;
        let mut url = Url::parse(&self.endpoint(&format!("repos/{project}/issues")))?;
        url.query_pairs_mut()
            .append_pair("state", "all")
            .append_pair("sort", "updated")
            .append_pair("direction", "desc")
            .append_pair("per_page", "1");
        let mut headers = github_headers();
        if let Some(etag) = probe.etag.as_deref() {
            headers.push((IF_NONE_MATCH.as_str(), etag));
        }
        let response = self.core.get(url.as_str(), &headers).await?;
        let etag = response
            .headers
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .or_else(|| probe.etag.clone());
        if response.status == 304 {
            return Ok(FreshnessResult { latest: None, etag, not_modified: true });
        }
        ensure_success(response.status, &response.body)?;
        let value: Value = serde_json::from_str(&response.body)?;
        let latest = value
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["updated_at"].as_str())
            .map(str::to_string);
        Ok(FreshnessResult {
            latest: probe_advance(latest, probe.updated_since.as_deref()),
            etag,
            not_modified: false,
        })
    }
    /// Provider-attested closure walk (#702 stage 2), two phases per project:
    /// merged/closed PRs (`closingIssuesReferences` + the ATTESTED merge commit) then closed
    /// issues (`ClosedEvent.closer` — direct-by-commit and UI-linked PR closures the
    /// description's text never carried). ~1 rate point per 100 PRs, on the dedicated lane.
    async fn attested_closers_page(
        &self,
        project: &str,
        cursor: Option<&str>,
        since: Option<&str>,
    ) -> anyhow::Result<Option<AttestedClosersPage>> {
        validate_github_project(project)?;
        let (owner, name) = project
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("GitHub project is owner/repo"))?;
        let (phase, after) = attested_phase(cursor);
        let query = if phase == "prs" { ATTESTED_PRS_QUERY } else { ATTESTED_ISSUES_QUERY };
        let variables = serde_json::json!({ "owner": owner, "name": name, "after": after });
        let Some(data) = self.graphql_query(query, variables).await? else {
            return Ok(None);
        };
        let connection =
            &data["repository"][if phase == "prs" { "pullRequests" } else { "issues" }];
        parse_attested_page(connection, phase, project, after.as_deref(), since).map(Some)
    }
}

/// Parse ONE already-fetched GraphQL connection page into an [`AttestedClosersPage`]. Pure over
/// the JSON — no transport — so the producer's evidence rules are unit-testable directly (the
/// `PapertrailClient` stubs bypass this function entirely, so its bugs are invisible to them).
fn parse_attested_page(
    connection: &Value,
    phase: &str,
    project: &str,
    after: Option<&str>,
    since: Option<&str>,
) -> anyhow::Result<AttestedClosersPage> {
    let mut page = AttestedClosersPage::default();
    let mut reached_watermark = false;
    for node in connection["nodes"].as_array().into_iter().flatten() {
        let number = node["number"].as_u64().unwrap_or_default().to_string();
        let updated_at = node["updatedAt"].as_str().unwrap_or_default();
        if let Some(since) = since
            && !updated_at.is_empty()
            && updated_at < since
        {
            reached_watermark = true;
            break;
        }
        if page.frontier.is_none() && after.is_none() && !updated_at.is_empty() {
            // First page of THIS phase: the phase's own frontier. The mirror keeps the
            // conservative MINIMUM across phases, so neither stream can skip updates.
            page.frontier = Some(updated_at.to_string());
        }
        if phase == "prs" {
            // MERGED-only: a closed-UNMERGED PR closes nothing, yet GitHub still lists its
            // `closingIssuesReferences` — minting an edge for it would assert a false closer
            // (the same trap the text tier and the item store guard against). An unmerged PR is
            // a pure no-op here: it never had a provider edge of ours to replace, either.
            let Some(merged_at) = node["mergedAt"].as_str() else {
                continue;
            };
            let _ = merged_at;
            let merge_commit =
                node.pointer("/mergeCommit/oid").and_then(Value::as_str).map(str::to_string);
            // The PR phase CREATES keyword edges but does NOT reap: reaping is issue-keyed (see
            // `replaced_issue_closers`), because a closer-keyed replace-set would clobber the
            // issue-phase UI-linked rows this PR's `closingIssuesReferences` never lists.
            // No silent caps: a PR closing >100 issues is pathological, but its tail must surface
            // as an explicit gap, not vanish past an unpaginated nested connection.
            if node.pointer("/closingIssuesReferences/pageInfo/hasNextPage")
                == Some(&Value::Bool(true))
            {
                anyhow::bail!(
                    "PR #{number} carries more than 100 closing references; the nested connection \
                     is not paginated"
                );
            }
            for closed in node
                .pointer("/closingIssuesReferences/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let issue_key = closed["number"].as_u64().unwrap_or_default().to_string();
                let issue_project = closed
                    .pointer("/repository/nameWithOwner")
                    .and_then(Value::as_str)
                    .unwrap_or(project);
                // Cross-repository closures are SKIPPED: `closer_key` is same-project by the
                // schema contract, so a bare PR number under the ISSUE's project would name an
                // unrelated item. (A both-projects edge shape can lift this later.) GitHub repo
                // names are CASE-INSENSITIVE: the binding may carry `owner/repo` while GraphQL
                // returns the canonical `Owner/Repo`, so an exact compare would wrongly treat the
                // repo's OWN issues as cross-repo and drop every edge — fold ASCII case.
                if !issue_project.eq_ignore_ascii_case(project) {
                    continue;
                }
                page.edges.push(ClosingEdge {
                    project: project.to_string(),
                    issue_kind: ItemKind::Issue,
                    issue_key,
                    closer_kind: CloserKind::ChangeRequest,
                    closer_key: number.clone(),
                    closer_commit: merge_commit.clone(),
                    source: ClosingEdgeSource::Provider,
                });
            }
            if let Some(sha) = merge_commit {
                page.item_updates.push(AttestedItemUpdate {
                    item_kind: ItemKind::ChangeRequest,
                    item_key: number.clone(),
                    resolution: None,
                    merge_commit_sha: Some(sha),
                });
            }
        } else {
            if let Some(resolution) = parse::resolution_from_state_reason(
                node["stateReason"]
                    .as_str()
                    .map(|reason| {
                        // GraphQL enum tokens are UPPER_SNAKE; the REST mapper speaks lowercase.
                        reason.to_ascii_lowercase()
                    })
                    .as_deref(),
            ) {
                page.item_updates.push(AttestedItemUpdate {
                    item_kind: ItemKind::Issue,
                    item_key: number.clone(),
                    resolution: Some(resolution),
                    merge_commit_sha: None,
                });
            }
            page.replaced_issue_closers.push(number.clone());
            let closer = node
                .pointer("/timelineItems/nodes")
                .and_then(Value::as_array)
                .and_then(|nodes| nodes.last())
                .map(|event| event["closer"].clone())
                .unwrap_or(Value::Null);
            match closer["__typename"].as_str() {
                Some("Commit") =>
                    if let Some(oid) = closer["oid"].as_str() {
                        page.edges.push(ClosingEdge {
                            project: project.to_string(),
                            issue_kind: ItemKind::Issue,
                            issue_key: number.clone(),
                            closer_kind: CloserKind::Commit,
                            closer_key: oid.to_string(),
                            closer_commit: Some(oid.to_string()),
                            source: ClosingEdgeSource::Provider,
                        });
                    },
                Some("PullRequest") => {
                    // Cross-repository PR closers are SKIPPED, symmetric to the PR phase:
                    // `closer_key` is same-project, so a bare PR number under the ISSUE's project
                    // would name an unrelated item. Case-insensitive like the PR phase — GitHub's
                    // canonical `nameWithOwner` casing may differ from the binding's project.
                    let closer_repo = closer
                        .pointer("/repository/nameWithOwner")
                        .and_then(Value::as_str)
                        .unwrap_or(project);
                    if let Some(pr) = closer["number"].as_u64()
                        && closer_repo.eq_ignore_ascii_case(project)
                    {
                        page.edges.push(ClosingEdge {
                            project: project.to_string(),
                            issue_kind: ItemKind::Issue,
                            issue_key: number.clone(),
                            closer_kind: CloserKind::ChangeRequest,
                            closer_key: pr.to_string(),
                            // Preserve the attested merge commit for a UI-linked PR closure that
                            // surfaces ONLY through ClosedEvent (no keyword-derived edge from the
                            // PR phase) — GitHub exposes the closer PR's mergeCommit here.
                            closer_commit: closer
                                .pointer("/mergeCommit/oid")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            source: ClosingEdgeSource::Provider,
                        });
                    }
                },
                _ => {},
            }
        }
    }
    let has_next = connection.pointer("/pageInfo/hasNextPage").and_then(Value::as_bool)
        == Some(true)
        && !reached_watermark;
    let end_cursor =
        connection.pointer("/pageInfo/endCursor").and_then(Value::as_str).unwrap_or_default();
    page.next = if has_next {
        Some(format!("{phase}:{end_cursor}"))
    } else if phase == "prs" {
        Some("issues".to_string())
    } else {
        None
    };
    Ok(page)
}

fn github_headers() -> Vec<(&'static str, &'static str)> {
    vec![("accept", ACCEPT), ("x-github-api-version", API_VERSION)]
}

fn validate_github_project(project: &str) -> anyhow::Result<()> {
    let mut segments = project.split('/');
    let owner = segments.next().unwrap_or_default();
    let repository = segments.next().unwrap_or_default();
    let safe = |segment: &str| {
        !segment.is_empty()
            && !matches!(segment, "." | "..")
            && segment.chars().all(|ch| {
                !ch.is_control() && !ch.is_whitespace() && !matches!(ch, '?' | '#' | '%' | '\\')
            })
    };
    anyhow::ensure!(
        segments.next().is_none() && safe(owner) && safe(repository),
        "invalid GitHub project `{project}`"
    );
    Ok(())
}

fn with_per_page(url: String) -> String {
    format!("{url}?per_page=100")
}

fn ensure_success(status: u16, body: &str) -> anyhow::Result<()> {
    ensure_provider_success("GitHub", status, body)
}

fn search_items(value: &Value) -> anyhow::Result<&[Value]> {
    anyhow::ensure!(
        !value["incomplete_results"].as_bool().unwrap_or(true),
        "GitHub Search returned incomplete_results; refusing to advance backfill"
    );
    value["items"]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow::anyhow!("GitHub Search response has no items array"))
}

fn search_boundary(items: &[Value]) -> anyhow::Result<Option<String>> {
    let timestamps =
        items.iter().map(search_item_updated_at).collect::<anyhow::Result<Vec<_>>>()?;
    Ok(timestamps.into_iter().min().map(str::to_string))
}

fn search_item_updated_at(item: &Value) -> anyhow::Result<&str> {
    item["updated_at"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("GitHub Search item has no updated_at boundary"))
}

fn safe_search_boundary(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        // Each split stream has consumed everything newer than its own tie boundary. Descend only
        // below the newer boundary: the older stream may replay a suffix, but the newer stream
        // cannot skip the interval it has not consumed yet.
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
}

#[cfg(test)]
mod tests {
    use super::super::transport::stub::{StubResponse, spawn_script_stub};
    use super::*;

    fn binding(base_url: String) -> ResolvedTracker {
        ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: Some(base_url),
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        }
    }

    fn issue(number: i64) -> String {
        format!(
            r#"{{"number":{number},"html_url":"{{BASE}}/o/r/issues/{number}","state":"open","title":"item","body":"body","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","labels":[{{"name":"bug"}}]}}"#
        )
    }

    #[test]
    fn enterprise_binding_keeps_items_comments_pagination_search_and_probe_on_its_origin() {
        let mut first_comments = StubResponse::ok(r#"[{"id":10,"body":"first"}]"#);
        first_comments.headers.push((
            "Link".to_string(),
            "<{BASE}/api/v3/repos/o/r/issues/1/comments?per_page=100&page=2>; rel=\"next\""
                .to_string(),
        ));
        let mut not_modified = StubResponse::status("304 Not Modified", "");
        not_modified.headers.push(("ETag".to_string(), "\"v1\"".to_string()));
        let (base, handle) = spawn_script_stub(vec![
            StubResponse::ok(&issue(1)),
            first_comments,
            StubResponse::ok(r#"[{"id":11,"body":"second"}]"#),
            StubResponse::ok(&format!(r#"{{"incomplete_results":false,"items":[{}]}}"#, issue(1))),
            not_modified,
        ]);
        let registry = GovernorRegistry::default();
        let client =
            GitHubClient::new(&binding(base), &registry, TransportOptions::default()).unwrap();

        let item = block_on(client.item("o/r", ItemKind::Issue, "1")).unwrap();
        assert_eq!(item.tags, vec!["bug"]);
        let comments = block_on(client.item_comments("o/r", ItemKind::Issue, "1")).unwrap();
        assert_eq!(comments.len(), 2);
        let page = block_on(client.items_page("o/r", &PageCursor {
            updated_before: Some(INITIAL_BOUNDARY.to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        assert_eq!(page.items.len(), 1);
        let probe = block_on(client.freshness_probe("o/r", &FreshnessProbe {
            updated_since: Some("2026-01-02T00:00:00Z".to_string()),
            etag: Some("\"v1\"".to_string()),
        }))
        .unwrap();
        assert!(probe.not_modified);

        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests.iter().all(|request| request.starts_with("GET /api/v3/")));
        assert!(requests[4].to_ascii_lowercase().contains("if-none-match: \"v1\""));
    }

    #[test]
    fn cross_origin_pagination_is_rejected_before_following_it() {
        let mut response = StubResponse::ok(r#"[{"id":10,"body":"first"}]"#);
        response.headers.push((
            "Link".to_string(),
            "<https://api.github.com/repos/o/r/issues/1/comments?page=2>; rel=\"next\"".to_string(),
        ));
        let (base, handle) = spawn_script_stub(vec![response]);
        let registry = GovernorRegistry::default();
        let client =
            GitHubClient::new(&binding(base), &registry, TransportOptions::default()).unwrap();
        let error = block_on(client.item_comments("o/r", ItemKind::Issue, "1")).unwrap_err();
        assert!(error.to_string().contains("outside the binding"), "{error:#}");
        assert_eq!(handle.join().unwrap().len(), 1, "the foreign URL was never requested");
    }

    #[test]
    fn item_comment_resources_return_one_provider_page_at_a_time() {
        let mut first = StubResponse::ok(r#"[{"id":10,"body":"first"}]"#);
        first.headers.push((
            "Link".to_string(),
            "<{BASE}/api/v3/repos/o/r/issues/1/comments?page=2>; rel=\"next\"".to_string(),
        ));
        let (base, handle) =
            spawn_script_stub(vec![first, StubResponse::ok(r#"[{"id":11,"body":"second"}]"#)]);
        let client = GitHubClient::new(
            &binding(base),
            &GovernorRegistry::default(),
            TransportOptions::default(),
        )
        .unwrap();
        let first_page =
            block_on(client.item_comments_page("o/r", ItemKind::Issue, "1", &PageCursor {
                stream: Some("issue_comments".to_string()),
                ..PageCursor::default()
            }))
            .unwrap();
        assert_eq!(first_page.comments.len(), 1);
        let second_page = block_on(client.item_comments_page(
            "o/r",
            ItemKind::Issue,
            "1",
            first_page.next.as_ref().unwrap(),
        ))
        .unwrap();
        assert_eq!(second_page.comments.len(), 1);
        assert!(second_page.next.is_none());
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn constructor_validates_provider_and_api_origin() {
        let registry = GovernorRegistry::default();
        let mut tracker = binding("https://github.example.com/".to_string());
        tracker.provider = Tracker::Gitlab;
        assert!(GitHubClient::new(&tracker, &registry, TransportOptions::default()).is_err());

        tracker.provider = Tracker::Github;
        tracker.base_url = Some("not an absolute URL".to_string());
        assert!(GitHubClient::new(&tracker, &registry, TransportOptions::default()).is_err());
        for invalid in [
            "https://github.example.com/path",
            "https://github.example.com?query=1",
            "https://github.example.com#fragment",
        ] {
            tracker.base_url = Some(invalid.to_string());
            assert!(GitHubClient::new(&tracker, &registry, TransportOptions::default()).is_err());
        }

        tracker.base_url = Some("http://[::1]:8443".to_string());
        let client = GitHubClient::new(&tracker, &registry, TransportOptions::default()).unwrap();
        assert_eq!(client.api_origin, "http://[::1]:8443/api/v3");

        tracker.base_url = None;
        let client = GitHubClient::new(&tracker, &registry, TransportOptions::default()).unwrap();
        assert_eq!(client.api_origin, "https://api.github.com");
    }

    #[cfg(not(windows))]
    #[test]
    fn multi_lane_client_resolves_a_token_command_once() {
        let counter = tempfile::NamedTempFile::new().unwrap();
        let mut tracker = binding("http://127.0.0.1:9".to_string());
        tracker.auth = Some(rag_rat_base::config::TrackerAuth::TokenCommand(format!(
            "printf x >> {}; printf token",
            counter.path().display()
        )));
        GitHubClient::new(&tracker, &GovernorRegistry::default(), TransportOptions::default())
            .unwrap();
        assert_eq!(std::fs::read_to_string(counter.path()).unwrap(), "x");
    }

    #[test]
    fn malformed_project_is_rejected_before_a_request() {
        let (base, handle) = spawn_script_stub(Vec::new());
        let client = GitHubClient::new(
            &binding(base),
            &GovernorRegistry::default(),
            TransportOptions::default(),
        )
        .unwrap();
        let error =
            block_on(client.items_page("owner/repo?leak=1", &PageCursor::default())).unwrap_err();
        assert!(error.to_string().contains("invalid GitHub project"));
        assert!(handle.join().unwrap().is_empty());
    }

    #[test]
    fn pull_item_and_all_comment_kinds_are_mapped() {
        let pull_issue = r#"{"pull_request":{},"number":7,"html_url":"{BASE}/o/r/pull/7","state":"open","title":"change","body":"body","updated_at":"2026-01-02T00:00:00Z","labels":[]}"#;
        let (base, handle) = spawn_script_stub(vec![
            StubResponse::ok(pull_issue),
            StubResponse::ok(r#"{"state":"closed","merged_at":"2026-01-03T00:00:00Z"}"#),
            StubResponse::ok(r#"[{"id":10,"body":"thread"}]"#),
            StubResponse::ok(
                r#"[{"id":11,"body":"review","state":"APPROVED","submitted_at":"2026-01-03T01:00:00Z"}]"#,
            ),
            StubResponse::ok(r#"[{"id":12,"body":"anchored","path":"src/lib.rs"}]"#),
        ]);
        let registry = GovernorRegistry::default();
        let client =
            GitHubClient::new(&binding(base), &registry, TransportOptions::default()).unwrap();

        let item = block_on(client.item("o/r", ItemKind::Issue, "7")).unwrap();
        assert_eq!(item.item_kind, ItemKind::ChangeRequest);
        assert_eq!(item.state, "closed");
        assert_eq!(item.merged_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        let comments = block_on(client.item_comments("o/r", ItemKind::ChangeRequest, "7")).unwrap();
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[1].review_state.as_deref(), Some("APPROVED"));
        assert_eq!(comments[2].anchor_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(handle.join().unwrap().len(), 5);
    }

    #[test]
    fn app_compatible_backfill_searches_issues_and_pulls_and_enriches_pull_state() {
        let pull_issue = r#"{"pull_request":{},"number":7,"html_url":"{BASE}/o/r/pull/7","state":"closed","title":"change","body":"body","updated_at":"2026-01-02T00:00:00Z","labels":[]}"#;
        let (base, handle) = spawn_script_stub(vec![
            StubResponse::ok(r#"{"total_count":0,"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(&format!(
                r#"{{"total_count":1,"incomplete_results":false,"items":[{pull_issue}]}}"#
            )),
            StubResponse::ok(r#"{"state":"closed","merged_at":"2026-01-03T00:00:00Z"}"#),
        ]);
        let client = GitHubClient::new(
            &binding(base),
            &GovernorRegistry::default(),
            TransportOptions::default(),
        )
        .unwrap();
        let issues = block_on(client.items_page("o/r", &PageCursor {
            updated_before: Some(INITIAL_BOUNDARY.to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        assert!(issues.items.is_empty());
        let pulls = block_on(client.items_page("o/r", issues.next.as_ref().unwrap())).unwrap();
        assert_eq!(pulls.items.len(), 1);
        assert_eq!(pulls.items[0].item_kind, ItemKind::ChangeRequest);
        let mut pull = pulls.items[0].clone();
        block_on(client.enrich_item(&mut pull)).unwrap();
        assert_eq!(pull.merged_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        assert!(pulls.next.is_none());
        assert_eq!(pulls.backfill_boundary.as_deref(), Some("2026-01-02T00:00:00Z"));

        let requests = handle.join().unwrap();
        assert!(requests[0].contains("is%3Aissue"));
        assert!(requests[1].contains("is%3Apull-request"));
        assert!(requests[2].contains("/repos/o/r/pulls/7"));
    }

    #[test]
    fn pull_enrichment_failure_checkpoints_prior_items_and_keeps_the_cursor_retryable() {
        let pull = |number| {
            format!(
                r#"{{"pull_request":{{}},"number":{number},"html_url":"{{BASE}}/o/r/pull/{number}","state":"closed","title":"change","body":"body","updated_at":"2026-01-02T00:00:00Z","labels":[]}}"#
            )
        };
        let (base, handle) = spawn_script_stub(vec![
            StubResponse::ok(r#"{"total_count":0,"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(&format!(
                r#"{{"total_count":2,"incomplete_results":false,"items":[{},{}]}}"#,
                pull(2),
                pull(1)
            )),
            StubResponse::ok(r#"{"state":"closed","merged_at":"2026-01-03T00:00:00Z"}"#),
            StubResponse::ok("[]"),
            StubResponse::ok("[]"),
            StubResponse::ok("[]"),
            StubResponse::status("403 Forbidden", r#"{"message":"denied"}"#),
        ]);
        let binding = binding(base);
        let client =
            GitHubClient::new(&binding, &GovernorRegistry::default(), TransportOptions::default())
                .unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();

        let error = block_on(mirror_binding(
            &conn,
            &binding,
            std::slice::from_ref(&binding),
            &client,
            false,
        ))
        .unwrap_err();
        assert!(error.to_string().contains("GitHub HTTP 403"));
        let stored: i64 =
            conn.query_row("SELECT COUNT(*) FROM papertrail_items", [], |row| row.get(0)).unwrap();
        assert_eq!(stored, 1, "each completed enrichment is checkpointed independently");
        let persisted: String = conn
            .query_row("SELECT backfill_page_cursor FROM papertrail_sync_cursor", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(persisted.contains("is%3Apull-request"));
        let processed: String = conn
            .query_row("SELECT backfill_processed_keys FROM papertrail_sync_cursor", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(processed.contains(r#""key":"2""#));
        assert_eq!(handle.join().unwrap().len(), 7);
    }

    #[test]
    fn delta_pagination_repo_comments_and_freshness_are_native() {
        let mut delta = StubResponse::ok(&format!("[{}]", issue(2)));
        delta.headers.push((
            "Link".to_string(),
            "<{BASE}/api/v3/repos/o/r/issues?page=2>; rel=\"next\"".to_string(),
        ));
        let mut fresh = StubResponse::ok(r#"[{"number":3,"updated_at":"2026-01-04T00:00:00Z"}]"#);
        fresh.headers.push(("ETag".to_string(), "\"v2\"".to_string()));
        let mut issue_comments = StubResponse::ok(
            r#"[{"id":20,"body":"plain","issue_url":"https://example/o/r/issues/2","updated_at":"2026-01-03T00:00:00Z"},{"id":99,"body":"orphan"}]"#,
        );
        issue_comments.headers.push((
            "Link".to_string(),
            "<{BASE}/api/v3/repos/o/r/issues/comments?page=2>; rel=\"next\"".to_string(),
        ));
        let (base, handle) = spawn_script_stub(vec![
            delta,
            StubResponse::ok(&format!("[{}]", issue(1))),
            issue_comments,
            StubResponse::ok("[]"),
            StubResponse::ok(
                r#"[{"id":21,"body":"anchored","pull_request_url":"https://example/o/r/pulls/3","path":"src/lib.rs","updated_at":"2026-01-03T00:00:00Z"}]"#,
            ),
            fresh,
        ]);
        let registry = GovernorRegistry::default();
        let client =
            GitHubClient::new(&binding(base), &registry, TransportOptions::default()).unwrap();

        let first = block_on(client.items_page("o/r", &PageCursor {
            updated_since: Some("2026-01-01T00:00:00Z".to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        let second = block_on(client.items_page("o/r", first.next.as_ref().unwrap())).unwrap();
        assert_eq!(second.items[0].item_key, "1");
        let first_comments = block_on(client.comments_page("o/r", &PageCursor {
            stream: Some("issue_comments".to_string()),
            updated_since: Some("2026-01-02T00:00:00Z".to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        let second_comments =
            block_on(client.comments_page("o/r", first_comments.next.as_ref().unwrap())).unwrap();
        assert!(second_comments.next.is_none());
        let third_comments = block_on(client.comments_page("o/r", &PageCursor {
            stream: Some("review_comments".to_string()),
            updated_since: Some("2026-01-02T00:00:00Z".to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        let comments = first_comments
            .comments
            .into_iter()
            .chain(second_comments.comments)
            .chain(third_comments.comments)
            .collect::<Vec<_>>();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[1].item_kind, ItemKind::ChangeRequest);
        let probe = block_on(client.freshness_probe("o/r", &FreshnessProbe {
            updated_since: Some("2026-01-02T00:00:00Z".to_string()),
            etag: Some("\"v1\"".to_string()),
        }))
        .unwrap();
        assert_eq!(probe.latest.as_deref(), Some("2026-01-04T00:00:00Z"));
        assert_eq!(probe.etag.as_deref(), Some("\"v2\""));
        assert!(!probe.not_modified);

        let requests = handle.join().unwrap();
        assert!(requests[0].contains("since=2026-01-01T00%3A00%3A00Z"));
        assert!(requests[2].contains("since=2026-01-02T00%3A00%3A00Z"));
        assert!(requests[2].contains("sort=updated&direction=asc"));
        assert!(requests[4].contains("sort=updated&direction=asc"));
    }

    #[test]
    fn ascending_updated_order_keeps_an_edited_comment_ahead_of_the_cursor() {
        let mut first = StubResponse::ok(
            r#"[{"id":20,"body":"page-one","issue_url":"https://example/o/r/issues/2","updated_at":"2026-01-02T00:00:00Z"}]"#,
        );
        first.headers.push((
            "Link".to_string(),
            "<{BASE}/api/v3/repos/o/r/issues/comments?sort=updated&direction=asc&page=2>; \
             rel=\"next\""
                .to_string(),
        ));
        let (base, handle) = spawn_script_stub(vec![
            first,
            // Comment 10 was edited between requests. Ascending updated order moves it forward,
            // so it remains reachable from the continuation instead of jumping behind it.
            StubResponse::ok(
                r#"[{"id":10,"body":"moved","issue_url":"https://example/o/r/issues/1","updated_at":"2026-01-03T00:00:00Z"}]"#,
            ),
        ]);
        let registry = GovernorRegistry::default();
        let client =
            GitHubClient::new(&binding(base), &registry, TransportOptions::default()).unwrap();
        let scan = PageCursor {
            stream: Some("issue_comments".to_string()),
            updated_since: Some("2026-01-01T23:59:59Z".to_string()),
            ..PageCursor::default()
        };

        let page_one = block_on(client.comments_page("o/r", &scan)).unwrap();
        let page_two =
            block_on(client.comments_page("o/r", page_one.next.as_ref().unwrap())).unwrap();
        assert_eq!(page_two.comments.len(), 1);
        assert_eq!(page_two.comments[0].body, "moved");

        let requests = handle.join().unwrap();
        assert!(requests[0].contains("sort=updated&direction=asc"));
        assert!(requests[1].contains("sort=updated&direction=asc&page=2"));
    }

    #[test]
    fn legacy_combined_search_cursor_finishes_at_its_persisted_boundary() {
        let (base, handle) = spawn_script_stub(vec![StubResponse::ok(&format!(
            r#"{{"total_count":2,"incomplete_results":false,"items":[{}]}}"#,
            issue(1)
        ))]);
        let client = GitHubClient::new(
            &binding(base.clone()),
            &GovernorRegistry::default(),
            TransportOptions::default(),
        )
        .unwrap();
        let page =
            block_on(
                client.items_page("o/r", &PageCursor {
                    stream: Some(SEARCH_BACKFILL_STREAM.to_string()),
                    updated_before: Some("2026-01-02T00:00:00Z".to_string()),
                    page_token: Some(format!("{base}/api/v3/search/issues?q=legacy-page-2")),
                    provider_state: Some(
                        r#"{"boundary":"2026-01-02T00:00:00Z","total_count":2,"fetched":1}"#
                            .to_string(),
                    ),
                    ..PageCursor::default()
                }),
            )
            .unwrap();

        assert_eq!(page.items.len(), 1);
        assert!(page.next.is_none());
        assert_eq!(page.backfill_boundary.as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn search_backfill_drains_tied_pages_before_returning_a_boundary() {
        let mut first = StubResponse::ok(&format!(
            r#"{{"total_count":3,"incomplete_results":false,"items":[{}]}}"#,
            issue(2)
        ));
        first.headers.push((
            "Link".to_string(),
            "<{BASE}/api/v3/search/issues?q=page-2>; rel=\"next\"".to_string(),
        ));
        let (base, handle) = spawn_script_stub(vec![
            first,
            StubResponse::ok(&format!(
                r#"{{"incomplete_results":false,"items":[{},{}]}}"#,
                issue(1),
                issue(0).replace("2026-01-02T00:00:00Z", "2026-01-01T23:59:59Z")
            )),
            StubResponse::ok(r#"{"total_count":0,"incomplete_results":false,"items":[]}"#),
        ]);
        let client = GitHubClient::new(
            &binding(base),
            &GovernorRegistry::default(),
            TransportOptions::default(),
        )
        .unwrap();
        let first_page = block_on(client.items_page("o/r", &PageCursor {
            updated_before: Some(INITIAL_BOUNDARY.to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        assert_eq!(first_page.items.len(), 1);
        let second_page =
            block_on(client.items_page("o/r", first_page.next.as_ref().unwrap())).unwrap();
        assert_eq!(second_page.items.len(), 1);
        let pull_page =
            block_on(client.items_page("o/r", second_page.next.as_ref().unwrap())).unwrap();
        assert!(pull_page.items.is_empty());
        assert!(pull_page.next.is_none());
        assert_eq!(pull_page.backfill_boundary.as_deref(), Some("2026-01-02T00:00:00Z"));
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].contains("is%3Aissue"));
        assert!(requests[2].contains("is%3Apull-request"));
    }

    #[test]
    fn split_search_descends_only_below_the_newer_stream_boundary() {
        let issue_at =
            |number, timestamp: &str| issue(number).replace("2026-01-02T00:00:00Z", timestamp);
        let pull_at = |number, timestamp: &str| {
            issue_at(number, timestamp).replace(
                &format!(r#"{{"number":{number},"#),
                &format!(r#"{{"pull_request":{{}},"number":{number},"#),
            )
        };
        let mut first_pull = StubResponse::ok(&format!(
            r#"{{"total_count":2,"incomplete_results":false,"items":[{}]}}"#,
            pull_at(2, "2026-01-20T00:00:00Z")
        ));
        first_pull.headers.push((
            "Link".to_string(),
            "<{BASE}/api/v3/search/issues?q=pull-page-2>; rel=\"next\"".to_string(),
        ));
        let (base, handle) = spawn_script_stub(vec![
            StubResponse::ok(&format!(
                r#"{{"total_count":1,"incomplete_results":false,"items":[{}]}}"#,
                issue_at(1, "2026-01-10T00:00:00Z")
            )),
            first_pull,
            StubResponse::ok(&format!(
                r#"{{"incomplete_results":false,"items":[{}]}}"#,
                pull_at(3, "2026-01-15T00:00:00Z")
            )),
        ]);
        let client = GitHubClient::new(
            &binding(base),
            &GovernorRegistry::default(),
            TransportOptions::default(),
        )
        .unwrap();
        let issues = block_on(client.items_page("o/r", &PageCursor {
            updated_before: Some(INITIAL_BOUNDARY.to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        let pulls = block_on(client.items_page("o/r", issues.next.as_ref().unwrap())).unwrap();
        let boundary = block_on(client.items_page("o/r", pulls.next.as_ref().unwrap())).unwrap();

        assert!(boundary.items.is_empty(), "the older PR belongs to the next strict window");
        assert_eq!(boundary.backfill_boundary.as_deref(), Some("2026-01-20T00:00:00Z"));
        assert_eq!(handle.join().unwrap().len(), 3);
    }

    #[test]
    fn search_backfill_rejects_a_cap_before_a_tie_is_drained() {
        let mut first = StubResponse::ok(&format!(
            r#"{{"total_count":1001,"incomplete_results":false,"items":[{}]}}"#,
            issue(2)
        ));
        first.headers.push((
            "Link".to_string(),
            "<{BASE}/api/v3/search/issues?q=page-2>; rel=\"next\"".to_string(),
        ));
        let (base, handle) = spawn_script_stub(vec![
            first,
            StubResponse::ok(&format!(r#"{{"incomplete_results":false,"items":[{}]}}"#, issue(1))),
        ]);
        let client = GitHubClient::new(
            &binding(base),
            &GovernorRegistry::default(),
            TransportOptions::default(),
        )
        .unwrap();
        let first_page = block_on(client.items_page("o/r", &PageCursor {
            updated_before: Some(INITIAL_BOUNDARY.to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        let error =
            block_on(client.items_page("o/r", first_page.next.as_ref().unwrap())).unwrap_err();
        assert!(error.to_string().contains("timestamp tie"));
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn item_page_rejects_ambiguous_and_malformed_provider_pages() {
        let registry = GovernorRegistry::default();
        let (base, handle) = spawn_script_stub(Vec::new());
        let client =
            GitHubClient::new(&binding(base), &registry, TransportOptions::default()).unwrap();
        let error = block_on(client.items_page("o/r", &PageCursor {
            stream: None,
            updated_since: Some("2026-01-01T00:00:00Z".to_string()),
            updated_before: Some("2026-01-02T00:00:00Z".to_string()),
            page_token: None,
            provider_state: None,
        }))
        .unwrap_err();
        assert!(error.to_string().contains("both delta and backfill"));
        assert!(handle.join().unwrap().is_empty());

        let cases = [
            r#"{"incomplete_results":true,"items":[]}"#,
            r#"{"incomplete_results":false}"#,
            r#"{"not":"an array"}"#,
        ];
        for (index, body) in cases.into_iter().enumerate() {
            let (base, handle) = spawn_script_stub(vec![StubResponse::ok(body)]);
            let client = GitHubClient::new(
                &binding(base.clone()),
                &GovernorRegistry::default(),
                TransportOptions::default(),
            )
            .unwrap();
            let cursor = if index < 2 {
                PageCursor {
                    updated_before: Some(INITIAL_BOUNDARY.to_string()),
                    ..PageCursor::default()
                }
            } else {
                PageCursor::default()
            };
            assert!(block_on(client.items_page("o/r", &cursor)).is_err());
            assert_eq!(handle.join().unwrap().len(), 1);
        }
    }

    #[test]
    fn pages_reject_missing_identities_and_cursor_timestamps() {
        let (base, handle) = spawn_script_stub(vec![
            StubResponse::ok(r#"[{"updated_at":"2026-01-01T00:00:00Z"}]"#),
            StubResponse::ok(
                r#"[{"issue_url":"https://example/o/r/issues/1","updated_at":"2026-01-01T00:00:00Z"}]"#,
            ),
            StubResponse::ok(r#"[{"number":1}]"#),
        ]);
        let client = GitHubClient::new(
            &binding(base),
            &GovernorRegistry::default(),
            TransportOptions::default(),
        )
        .unwrap();

        let item_error = block_on(client.items_page("o/r", &PageCursor::default())).unwrap_err();
        assert!(item_error.to_string().contains("valid number"));

        let comment_error = block_on(client.comments_page("o/r", &PageCursor {
            stream: Some("issue_comments".to_string()),
            ..PageCursor::default()
        }))
        .unwrap_err();
        assert!(comment_error.to_string().contains("valid id"));

        let timestamp_error =
            block_on(client.items_page("o/r", &PageCursor::default())).unwrap_err();
        assert!(timestamp_error.to_string().contains("valid updated_at"));
        assert_eq!(handle.join().unwrap().len(), 3);
    }

    #[test]
    fn not_found_status_is_a_typed_item_outcome() {
        let error = ensure_success(404, "missing").unwrap_err();
        assert!(error.downcast_ref::<PapertrailClientError>().is_some());
        assert!(ensure_success(500, "broken").unwrap_err().to_string().contains("HTTP 500"));
    }

    const INITIAL_BOUNDARY: &str = "2970-12-31T23:59:59Z";
    // Direct producer tests over hand-built GraphQL JSON — the `PapertrailClient` stubs bypass
    // `parse_attested_page`, so its evidence rules (merged gate, cross-repo skip, CR replace-set,
    // truncation bail) are ONLY covered here.
    fn prs_connection(nodes: Value) -> Value {
        serde_json::json!({ "pageInfo": { "hasNextPage": false, "endCursor": "c" }, "nodes": nodes })
    }

    #[test]
    fn attested_prs_merged_mint_edge_and_update() {
        let conn = prs_connection(serde_json::json!([{
            "number": 9, "updatedAt": "2026-01-05T00:00:00Z", "mergedAt": "2026-01-05T00:00:00Z",
            "mergeCommit": { "oid": "abc123" },
            "closingIssuesReferences": { "pageInfo": { "hasNextPage": false },
                "nodes": [{ "number": 5, "repository": { "nameWithOwner": "o/r" } }] }
        }]));
        let page = parse_attested_page(&conn, "prs", "o/r", None, None).unwrap();
        assert_eq!(page.edges.len(), 1);
        assert_eq!(page.edges[0].issue_key, "5");
        assert_eq!(page.edges[0].closer_key, "9");
        assert_eq!(page.edges[0].closer_commit.as_deref(), Some("abc123"));
        // The PR phase mints keyword edges but owns NO replace-set — reaping is issue-keyed.
        assert!(page.replaced_issue_closers.is_empty(), "the PR phase never reaps");
        assert_eq!(page.item_updates.len(), 1);
    }

    #[test]
    fn attested_prs_closed_unmerged_mints_nothing() {
        let conn = prs_connection(serde_json::json!([{
            "number": 9, "updatedAt": "2026-01-05T00:00:00Z", "mergedAt": Value::Null,
            "mergeCommit": Value::Null,
            "closingIssuesReferences": { "pageInfo": { "hasNextPage": false },
                "nodes": [{ "number": 5, "repository": { "nameWithOwner": "o/r" } }] }
        }]));
        let page = parse_attested_page(&conn, "prs", "o/r", None, None).unwrap();
        assert!(page.edges.is_empty(), "a closed-unmerged PR closes nothing");
        assert!(page.item_updates.is_empty());
    }

    #[test]
    fn attested_prs_cross_repo_reference_is_skipped() {
        let conn = prs_connection(serde_json::json!([{
            "number": 9, "updatedAt": "2026-01-05T00:00:00Z", "mergedAt": "2026-01-05T00:00:00Z",
            "mergeCommit": { "oid": "abc" },
            "closingIssuesReferences": { "pageInfo": { "hasNextPage": false }, "nodes": [
                { "number": 5, "repository": { "nameWithOwner": "o/other" } },
                { "number": 7, "repository": { "nameWithOwner": "o/r" } }
            ] }
        }]));
        let page = parse_attested_page(&conn, "prs", "o/r", None, None).unwrap();
        assert_eq!(page.edges.len(), 1, "the cross-repo o/other#5 is skipped");
        assert_eq!(page.edges[0].issue_key, "7");
        assert_eq!(page.edges[0].project, "o/r");
    }

    #[test]
    fn attested_prs_truncated_nested_connection_fails_loudly() {
        let conn = prs_connection(serde_json::json!([{
            "number": 9, "updatedAt": "2026-01-05T00:00:00Z", "mergedAt": "2026-01-05T00:00:00Z",
            "mergeCommit": { "oid": "abc" },
            "closingIssuesReferences": { "pageInfo": { "hasNextPage": true }, "nodes": [] }
        }]));
        let err = parse_attested_page(&conn, "prs", "o/r", None, None).unwrap_err();
        assert!(err.to_string().contains("closing references"), "silent truncation must bail");
    }

    #[test]
    fn attested_issues_commit_and_pr_closers_and_resolution() {
        let conn = serde_json::json!({ "pageInfo": { "hasNextPage": false, "endCursor": "c" },
            "nodes": [
                { "number": 5, "updatedAt": "2026-01-05T00:00:00Z", "stateReason": "COMPLETED",
                  "timelineItems": { "nodes": [{ "closer": { "__typename": "Commit", "oid": "sha1" } }] } },
                { "number": 6, "updatedAt": "2026-01-04T00:00:00Z", "stateReason": "NOT_PLANNED",
                  "timelineItems": { "nodes": [{ "closer": { "__typename": "PullRequest", "number": 11 } }] } }
            ] });
        let page = parse_attested_page(&conn, "issues", "o/r", None, None).unwrap();
        assert_eq!(page.replaced_issue_closers, vec!["5".to_string(), "6".to_string()]);
        let commit_edge = page.edges.iter().find(|e| e.issue_key == "5").unwrap();
        assert_eq!(commit_edge.closer_kind, CloserKind::Commit);
        assert_eq!(commit_edge.closer_key, "sha1");
        let pr_edge = page.edges.iter().find(|e| e.issue_key == "6").unwrap();
        assert_eq!(pr_edge.closer_kind, CloserKind::ChangeRequest);
        assert_eq!(pr_edge.closer_key, "11");
        assert_eq!(page.item_updates.iter().filter(|u| u.resolution.is_some()).count(), 2);
    }

    #[test]
    fn attested_since_watermark_cuts_the_scan_and_advances_the_phase() {
        let conn = prs_connection(serde_json::json!([
            { "number": 9, "updatedAt": "2026-01-05T00:00:00Z", "mergedAt": "2026-01-05T00:00:00Z",
              "mergeCommit": { "oid": "a" }, "closingIssuesReferences": { "pageInfo": { "hasNextPage": false }, "nodes": [] } },
            { "number": 8, "updatedAt": "2026-01-01T00:00:00Z", "mergedAt": "2026-01-01T00:00:00Z",
              "mergeCommit": { "oid": "b" }, "closingIssuesReferences": { "pageInfo": { "hasNextPage": false }, "nodes": [] } }
        ]));
        // Watermark cuts at the second (older) node; the phase still advances to issues.
        let page =
            parse_attested_page(&conn, "prs", "o/r", None, Some("2026-01-03T00:00:00Z")).unwrap();
        // Only the fresh PR #9 is processed (its merge sha becomes an item update); #8 is cut
        // before it is reached.
        assert_eq!(page.item_updates.len(), 1, "only the fresh PR is processed past the watermark");
        assert_eq!(page.item_updates[0].item_key, "9");
        assert_eq!(page.next.as_deref(), Some("issues"), "watermark cut still advances the phase");
    }
    #[test]
    fn attested_issue_cross_repo_pr_closer_is_skipped() {
        let conn = serde_json::json!({ "pageInfo": { "hasNextPage": false, "endCursor": "c" },
            "nodes": [{ "number": 5, "updatedAt": "2026-01-05T00:00:00Z", "stateReason": "COMPLETED",
                "timelineItems": { "nodes": [{ "closer": { "__typename": "PullRequest", "number": 9,
                    "repository": { "nameWithOwner": "other/repo" } } }] } }] });
        let page = parse_attested_page(&conn, "issues", "o/r", None, None).unwrap();
        assert!(
            page.edges.is_empty(),
            "a PR closer in another repo names a bare number under the wrong project — skip it",
        );
        // The issue is still replace-set (its whole provider closer set is refreshed regardless).
        assert_eq!(page.replaced_issue_closers, vec!["5".to_string()]);
    }

    /// GitHub repo names are CASE-INSENSITIVE: a binding `o/r` whose GraphQL response echoes the
    /// canonical `O/R` in `nameWithOwner` must still mint the SAME-repo edge in both phases — an
    /// exact compare would drop every attested edge for a mis-cased binding.
    #[test]
    fn attested_same_repo_name_matches_case_insensitively_in_both_phases() {
        let prs = prs_connection(serde_json::json!([{
            "number": 9, "updatedAt": "2026-01-05T00:00:00Z", "mergedAt": "2026-01-05T00:00:00Z",
            "mergeCommit": { "oid": "abc" },
            "closingIssuesReferences": { "pageInfo": { "hasNextPage": false },
                "nodes": [{ "number": 5, "repository": { "nameWithOwner": "O/R" } }] }
        }]));
        let pr_page = parse_attested_page(&prs, "prs", "o/r", None, None).unwrap();
        assert_eq!(pr_page.edges.len(), 1, "canonical-cased same repo is not cross-repo");
        assert_eq!(pr_page.edges[0].issue_key, "5");

        let issues = serde_json::json!({ "pageInfo": { "hasNextPage": false, "endCursor": "c" },
            "nodes": [{ "number": 5, "updatedAt": "2026-01-05T00:00:00Z", "stateReason": "COMPLETED",
                "timelineItems": { "nodes": [{ "closer": { "__typename": "PullRequest", "number": 9,
                    "repository": { "nameWithOwner": "O/R" } } }] } }] });
        let issue_page = parse_attested_page(&issues, "issues", "o/r", None, None).unwrap();
        assert_eq!(issue_page.edges.len(), 1, "the issue-phase PR closer is same-repo too");
        assert_eq!(issue_page.edges[0].closer_key, "9");
    }
    #[test]
    fn a_token_less_binding_has_no_attested_supply_and_never_calls_graphql() {
        // GitHub's GraphQL always requires a token; a public token-less binding must degrade to
        // the text tier at construction, not fail an unauthenticated call every sync. `base` is
        // unreachable — reaching it (a real POST) would error, so `Ok(None)` proves the memo
        // short-circuited before any request.
        let registry = GovernorRegistry::default();
        let client = GitHubClient::new(
            &binding("http://127.0.0.1:1".to_string()),
            &registry,
            TransportOptions::default(),
        )
        .unwrap();
        let page = block_on(client.attested_closers_page("o/r", None, None)).unwrap();
        assert!(page.is_none(), "no token ⇒ no attested supply, no GraphQL call");
    }
    #[test]
    fn attested_issue_pr_closer_carries_the_merge_commit() {
        let conn = serde_json::json!({ "pageInfo": { "hasNextPage": false, "endCursor": "c" },
            "nodes": [{ "number": 5, "updatedAt": "2026-01-05T00:00:00Z", "stateReason": "COMPLETED",
                "timelineItems": { "nodes": [{ "closer": { "__typename": "PullRequest", "number": 9,
                    "mergeCommit": { "oid": "sha9" },
                    "repository": { "nameWithOwner": "o/r" } } }] } }] });
        let page = parse_attested_page(&conn, "issues", "o/r", None, None).unwrap();
        let edge = page.edges.iter().find(|e| e.issue_key == "5").unwrap();
        assert_eq!(edge.closer_kind, CloserKind::ChangeRequest);
        assert_eq!(edge.closer_key, "9");
        assert_eq!(
            edge.closer_commit.as_deref(),
            Some("sha9"),
            "UI-linked PR closer keeps its merge commit"
        );
    }

    #[test]
    fn a_graphql_200_rate_limit_becomes_a_transport_pause() {
        // GitHub GraphQL signals a rate limit as HTTP 200 + errors[RATE_LIMITED] + reset header;
        // it must surface as a PAUSE so the scheduler honors the reset, not a swallowed error.
        let mut limited = StubResponse::ok(
            r#"{"data":null,"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded"}]}"#,
        );
        limited.headers.push(("x-ratelimit-reset".to_string(), "1900000000".to_string()));
        let (base, handle) = spawn_script_stub(vec![limited]);
        let mut tracker = binding(base);
        tracker.auth =
            Some(rag_rat_base::config::TrackerAuth::TokenCommand("printf token".to_string()));
        let registry = GovernorRegistry::default();
        let client = GitHubClient::new(&tracker, &registry, TransportOptions::default()).unwrap();
        let error = block_on(client.attested_closers_page("o/r", None, None)).unwrap_err();
        let paused = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<transport::TransportError>())
            .and_then(|e| match e {
                transport::TransportError::Paused { resume_at_ms, .. } => Some(*resume_at_ms),
                _ => None,
            });
        assert_eq!(paused, Some(1_900_000_000_000), "the reset epoch becomes the resume time (ms)");
        assert_eq!(handle.join().unwrap().len(), 1);
    }
    #[test]
    fn attested_queries_request_every_field_the_parser_reads() {
        // GUARD against the silent query/parser drift that bit this lane twice: the parser reads
        // these paths, so the query strings MUST fetch them, or an edit that reflows the query
        // silently drops a field and the parser only ever sees `None`.
        for field in ["mergeCommit", "closingIssuesReferences", "hasNextPage", "mergedAt"] {
            assert!(ATTESTED_PRS_QUERY.contains(field), "PRs query must request `{field}`");
        }
        for field in ["mergeCommit", "stateReason", "ClosedEvent", "PullRequest", "Commit"] {
            assert!(ATTESTED_ISSUES_QUERY.contains(field), "issues query must request `{field}`");
        }
    }
}
