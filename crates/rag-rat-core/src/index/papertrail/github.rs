//! Native GitHub provider client. Endpoint construction, pagination decoding, quota-lane
//! selection, and GitHub payload mapping live here; mirror policy stays in `mirror`.

use reqwest::Url;
use reqwest::header::{ETAG, IF_NONE_MATCH, LINK};
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
}

impl GitHubClient {
    pub(crate) fn new(
        binding: &ResolvedTracker,
        registry: &GovernorRegistry,
        options: TransportOptions,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(binding.provider == Tracker::Github, "not a GitHub binding");
        let parsed = match binding.base_url.as_deref() {
            None => Url::parse("https://api.github.com")?,
            Some(base) => {
                let mut base = Url::parse(base)?;
                anyhow::ensure!(
                    matches!(base.scheme(), "http" | "https"),
                    "GitHub base URL must use http(s)"
                );
                anyhow::ensure!(
                    base.username().is_empty() && base.password().is_none(),
                    "GitHub base URL must not contain credentials"
                );
                anyhow::ensure!(
                    matches!(base.path(), "" | "/")
                        && base.query().is_none()
                        && base.fragment().is_none(),
                    "GitHub base URL must be an origin without a path, query, or fragment"
                );
                base.set_path("/api/v3");
                base
            },
        };
        let host =
            parsed.host_str().ok_or_else(|| anyhow::anyhow!("GitHub API origin has no host"))?;
        let authority_host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let authority = parsed
            .port()
            .map_or_else(|| authority_host.clone(), |port| format!("{authority_host}:{port}"));
        let api_origin = parsed.as_str().trim_end_matches('/').to_string();
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
        Ok(Self { api_origin, core: transport("core")?, search: transport("search")? })
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
        Ok(CommentsPage { comments, next })
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
        Ok(CommentsPage { comments, next })
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
}

fn validate_positive_id(value: &Value, field: &str, resource: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value[field].as_u64().is_some_and(|id| id > 0),
        "{resource} has no valid {field}"
    );
    Ok(())
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
    if status == 404 {
        return Err(PapertrailClientError::ItemNotFound.into());
    }
    anyhow::ensure!((200..300).contains(&status), "GitHub HTTP {status}: {body}");
    Ok(())
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

fn next_link(headers: &reqwest::header::HeaderMap) -> anyhow::Result<Option<String>> {
    let Some(link) = headers.get(LINK) else { return Ok(None) };
    let link = link.to_str()?;
    Ok(link.split(',').find_map(|part| {
        let (url, rel) = part.trim().split_once(';')?;
        rel.trim().eq(r#"rel="next""#).then(|| url.trim().trim_matches(['<', '>']).to_string())
    }))
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
        tracker.auth = Some(crate::config::TrackerAuth::TokenCommand(format!(
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
        crate::index::schema::apply(&conn).unwrap();

        let error = block_on(mirror_binding(&conn, &binding, &client, false)).unwrap_err();
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
}
