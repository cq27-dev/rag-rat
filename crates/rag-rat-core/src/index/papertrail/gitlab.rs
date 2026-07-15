//! Native GitLab provider client. Endpoint construction, `Link`-header pagination decoding, and
//! GitLab payload mapping live here; mirror policy stays in `mirror`.
//!
//! GitLab differs from GitHub in three load-bearing ways this module encodes:
//! - **Separate iid namespaces**: issues and merge requests are distinct objects with independent
//!   numbering, so [`ItemKind`] is BINDING on every item call — issue #5 and MR !5 coexist.
//! - **Native LIFO descent**: the list endpoints accept `updated_before`/`updated_after` with
//!   `order_by=updated_at` directly — no Search-API detour — so a backfill cycle is two chained
//!   legs (issues, then merge requests) over ordinary lists.
//! - **No "notes updated since" feed**: there is no project-level notes endpoint, and note activity
//!   does NOT bump the noteable's `updated_at` (verified live against GitLab CE). New comments
//!   therefore arrive through the project EVENTS feed (`action=commented`, one event per note
//!   CREATION, each embedding the note's CURRENT body); edits to old notes produce no event and no
//!   timestamp movement anywhere, so edited bodies converge at the daily full re-walk.

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::transport::{GovernorRegistry, Transport, TransportOptions, TransportParams};
use super::*;

const LIST_BACKFILL_STREAM: &str = "list_backfill";
const LIST_DELTA_STREAM: &str = "list_delta";

/// Continuation across the two PARALLEL list legs of one items stream. Persisted opaquely inside
/// [`PageCursor::provider_state`]; the mirror never interprets it.
///
/// Both legs advance together — every page fetch pulls one page from EACH still-pending leg and
/// emits the merged items — so the mirror's first-page frontier (`max updated_at` of the scan's
/// first page) covers BOTH iid namespaces. Chaining the legs sequentially instead would leave
/// the merge-request maximum out of the persisted high watermark and replay the same merge
/// requests on every delta scan.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ListContinuation {
    /// Next-page URL per leg; `None` = drained. (A continuation only exists after the first
    /// fetch, so `None` never means "not started".)
    issues_next: Option<String>,
    merge_requests_next: Option<String>,
    /// Oldest `updated_at` consumed across BOTH legs of this backfill cycle — the provider-
    /// confirmed strict boundary reported when the cycle completes. Descending below the global
    /// minimum (not the last page's) keeps the next cycle from re-walking the older leg's tail.
    oldest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListLeg {
    Issues,
    MergeRequests,
}

impl ListLeg {
    fn path_segment(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::MergeRequests => "merge_requests",
        }
    }

    fn item_kind(self) -> ItemKind {
        match self {
            Self::Issues => ItemKind::Issue,
            Self::MergeRequests => ItemKind::ChangeRequest,
        }
    }
}

pub(crate) struct GitLabClient {
    api_origin: String,
    core: Transport,
}

impl GitLabClient {
    pub(crate) fn new(
        binding: &ResolvedTracker,
        registry: &GovernorRegistry,
        options: TransportOptions,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(binding.provider == Tracker::Gitlab, "not a GitLab binding");
        let parsed = match binding.base_url.as_deref() {
            None => Url::parse("https://gitlab.com/api/v4")?,
            Some(base) => {
                let mut base = Url::parse(base)?;
                anyhow::ensure!(
                    matches!(base.scheme(), "http" | "https"),
                    "GitLab base URL must use http(s)"
                );
                anyhow::ensure!(
                    base.username().is_empty() && base.password().is_none(),
                    "GitLab base URL must not contain credentials"
                );
                anyhow::ensure!(
                    matches!(base.path(), "" | "/")
                        && base.query().is_none()
                        && base.fragment().is_none(),
                    "GitLab base URL must be an origin without a path, query, or fragment"
                );
                base.set_path("/api/v4");
                base
            },
        };
        let host =
            parsed.host_str().ok_or_else(|| anyhow::anyhow!("GitLab API origin has no host"))?;
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
        // One lane: GitLab reports a single `RateLimit-*` quota bucket (the governor already
        // parses the IETF-draft header form), unlike GitHub's core/search split.
        let core = Transport::new_with_token(
            TransportParams {
                provider: "gitlab",
                lane: "core",
                host: &authority,
                auth: None,
                registry,
                options,
            },
            token.as_deref(),
        )?;
        Ok(Self { api_origin, core })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.api_origin, path.trim_start_matches('/'))
    }

    /// `projects/:id` path prefix with the namespaced project percent-encoded as ONE segment
    /// (`group%2Fsub%2Fproject`) — the subgroup-correct form. Sound because
    /// [`validate_gitlab_project`] restricts segments to GitLab's path alphabet, leaving `/` the
    /// only character needing escape.
    fn project_path(&self, project: &str, tail: &str) -> anyhow::Result<String> {
        validate_gitlab_project(project)?;
        Ok(self.endpoint(&format!("projects/{}/{tail}", project.replace('/', "%2F"))))
    }

    async fn get_json(&self, url: &str) -> anyhow::Result<(Value, Option<String>)> {
        let response = self.core.get(url, &gitlab_headers()).await?;
        ensure_success(response.status, &response.body)?;
        let next = next_link(&response.headers)?;
        Ok((serde_json::from_str(&response.body)?, next))
    }

    fn list_url(&self, project: &str, leg: ListLeg, cursor: &PageCursor) -> anyhow::Result<String> {
        let mut url = Url::parse(&self.project_path(project, leg.path_segment())?)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("order_by", "updated_at").append_pair("per_page", "100");
            if let Some(before) = cursor.updated_before.as_deref() {
                // Newest-first historical descent, natively.
                query.append_pair("sort", "desc").append_pair("updated_before", before);
            } else {
                // Delta ascends so the first page's inclusive upper timestamp is a conservative
                // consumed frontier even though later page numbers are mutable.
                query.append_pair("sort", "asc");
                if let Some(since) = cursor.updated_since.as_deref() {
                    query.append_pair("updated_after", since);
                }
            }
        }
        Ok(url.into())
    }

    /// One page from one leg: the mapped items plus the leg's next-page URL, if any. `None` =
    /// the namespace is unavailable (GitLab projects can disable issues or merge requests as a
    /// feature, surfacing 403/404 on that list) — the caller treats it as an empty, drained leg
    /// so one disabled tracker never aborts the sibling namespace's mirror.
    async fn leg_page(
        &self,
        project: &str,
        url: &str,
        leg: ListLeg,
    ) -> anyhow::Result<Option<(Vec<PapertrailItem>, Option<String>)>> {
        let response = self.core.get(url, &gitlab_headers()).await?;
        if matches!(response.status, 403 | 404) {
            return Ok(None);
        }
        ensure_success(response.status, &response.body)?;
        let next_url = next_link(&response.headers)?;
        let value: Value = serde_json::from_str(&response.body)?;
        let values = value.as_array().ok_or_else(|| {
            anyhow::anyhow!("GitLab {} response is not an array", leg.path_segment())
        })?;
        let mut items = Vec::with_capacity(values.len());
        for value in values {
            items.push(item_from_gitlab_value(project, leg.item_kind(), value)?);
        }
        Ok(Some((items, next_url)))
    }
}

impl PapertrailClient for GitLabClient {
    /// One lane over the project events feed. GitLab has no "notes updated since" endpoint and
    /// note activity does not touch the noteable, so creation events are the only incremental
    /// comment signal the provider offers (module docs have the full story).
    fn comment_streams(&self) -> &'static [&'static str] {
        &[EVENTS_STREAM]
    }

    async fn item(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<PapertrailItem> {
        // Kind is BINDING: issues and merge requests live in separate iid namespaces.
        let iid = validate_iid(key)?;
        let resource = match kind {
            ItemKind::Issue => "issues",
            ItemKind::ChangeRequest => "merge_requests",
        };
        let url = self.project_path(project, &format!("{resource}/{iid}"))?;
        let (value, _) = self.get_json(&url).await?;
        item_from_gitlab_value(project, kind, &value)
    }

    async fn item_comments(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<PapertrailComment>> {
        let mut comments = Vec::new();
        let mut cursor =
            PageCursor { stream: Some(NOTES_STREAM.to_string()), ..PageCursor::default() };
        loop {
            let page = self.item_comments_page(project, kind, key, &cursor).await?;
            comments.extend(page.comments);
            let Some(next) = page.next else { break };
            cursor = next;
        }
        Ok(comments)
    }

    fn item_comment_streams(&self, _kind: ItemKind) -> &'static [&'static str] {
        &[NOTES_STREAM]
    }

    async fn item_comments_page(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        let iid = validate_iid(key)?;
        let stream = cursor.stream.as_deref().unwrap_or(NOTES_STREAM);
        anyhow::ensure!(stream == NOTES_STREAM, "unknown GitLab item-comment stream `{stream}`");
        let resource = match kind {
            ItemKind::Issue => "issues",
            ItemKind::ChangeRequest => "merge_requests",
        };
        let url = match cursor.page_token.clone() {
            Some(next) => next,
            None => {
                let mut url =
                    Url::parse(&self.project_path(project, &format!("{resource}/{iid}/notes"))?)?;
                url.query_pairs_mut()
                    .append_pair("order_by", "updated_at")
                    .append_pair("sort", "asc")
                    .append_pair("per_page", "100");
                url.into()
            },
        };
        let (value, next_url) = self.get_json(&url).await?;
        let values = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("GitLab notes returned a non-array page"))?;
        let mut comments = Vec::new();
        for value in values {
            validate_positive_id(value, "id", "GitLab note")?;
            if let Some(comment) = comment_from_note_value(project, kind, key, value) {
                comments.push(comment);
            }
        }
        let next = next_url.map(|page_token| PageCursor {
            stream: Some(NOTES_STREAM.to_string()),
            page_token: Some(page_token),
            ..PageCursor::default()
        });
        Ok(CommentsPage { comments, next, frontier: None })
    }

    async fn items_page(&self, project: &str, cursor: &PageCursor) -> anyhow::Result<ItemsPage> {
        validate_gitlab_project(project)?;
        if cursor.updated_before.is_some() && cursor.updated_since.is_some() {
            anyhow::bail!("an item page cannot be both delta and backfill");
        }
        let backfill = cursor.updated_before.is_some();
        // Resume from the persisted continuation, or open BOTH legs' first pages. The delta
        // checkpoint persists only `page_token` and rebuilds its resume request WITHOUT
        // provider_state (mirror sync_item_delta), so the continuation JSON must live in the
        // page token itself — a resume that reopened both first pages would let a pause before
        // the second logical page spend every run's budget replaying page one.
        let continuation = cursor
            .provider_state
            .as_deref()
            .or_else(|| cursor.page_token.as_deref().filter(|token| token.starts_with('{')));
        let (mut state, fresh_open) = match continuation {
            Some(state) => (serde_json::from_str::<ListContinuation>(state)?, false),
            None => (
                ListContinuation {
                    issues_next: Some(self.list_url(project, ListLeg::Issues, cursor)?),
                    merge_requests_next: Some(self.list_url(
                        project,
                        ListLeg::MergeRequests,
                        cursor,
                    )?),
                    oldest: None,
                },
                true,
            ),
        };
        let mut items = Vec::new();
        let mut unavailable = 0usize;
        let mut fetched = 0usize;
        if let Some(url) = state.issues_next.take() {
            fetched += 1;
            match self.leg_page(project, &url, ListLeg::Issues).await? {
                Some((page, next)) => {
                    items.extend(page);
                    state.issues_next = next;
                },
                None => unavailable += 1,
            }
        }
        if let Some(url) = state.merge_requests_next.take() {
            fetched += 1;
            match self.leg_page(project, &url, ListLeg::MergeRequests).await? {
                Some((page, next)) => {
                    items.extend(page);
                    state.merge_requests_next = next;
                },
                None => unavailable += 1,
            }
        }
        // One unavailable namespace is a disabled feature; EVERY namespace unavailable on a
        // fresh open means the project itself is missing or inaccessible — that must surface,
        // not mirror as an empty project.
        if fresh_open && unavailable == fetched {
            return Err(PapertrailClientError::ItemNotFound.into());
        }
        // Keep the emitted page ordered like its legs (mirror correctness is order-agnostic;
        // ordering keeps pages legible in logs and tests).
        items.sort_by(|left, right| {
            let ordering = left.updated_at.cmp(&right.updated_at);
            if backfill { ordering.reverse() } else { ordering }
        });
        if backfill
            && let Some(oldest) = items.iter().filter_map(|item| item.updated_at.clone()).min()
        {
            state.oldest = match state.oldest.take() {
                Some(current) => Some(current.min(oldest)),
                None => Some(oldest),
            };
        }
        let stream = if backfill { LIST_BACKFILL_STREAM } else { LIST_DELTA_STREAM };
        let pending = state.issues_next.is_some() || state.merge_requests_next.is_some();
        let next = if pending {
            let token = serde_json::to_string(&state)?;
            Some(PageCursor {
                stream: Some(stream.to_string()),
                updated_since: cursor.updated_since.clone(),
                updated_before: cursor.updated_before.clone(),
                // The full continuation IS the page token (see resume note above); its
                // non-emptiness also marks every continuation page as NOT the scan's first page
                // for the mirror's conservative-frontier logic.
                page_token: Some(token.clone()),
                provider_state: Some(token),
            })
        } else {
            None
        };
        // The cycle's provider-confirmed strict boundary: both legs drained fully below the
        // cutoff, so the global oldest consumed timestamp bounds everything this cycle stored.
        let backfill_boundary =
            (backfill && next.is_none()).then(|| state.oldest.clone()).flatten();
        Ok(ItemsPage { items, next, backfill_boundary })
    }

    async fn comments_page(
        &self,
        project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        let stream = cursor.stream.as_deref().unwrap_or(EVENTS_STREAM);
        anyhow::ensure!(stream == EVENTS_STREAM, "unknown GitLab comment stream `{stream}`");
        let url = match cursor.page_token.clone() {
            Some(next) => next,
            None => {
                let mut url = Url::parse(&self.project_path(project, "events")?)?;
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("action", "commented")
                    .append_pair("sort", "asc")
                    .append_pair("per_page", "100");
                // `after` is DATE-granular and excludes the named day; naming the day BEFORE the
                // cursor's date keeps every same-day event in scope (the mirror dedups replays).
                if let Some(since) = cursor.updated_since.as_deref() {
                    query.append_pair("after", &day_before(since)?);
                }
                drop(query);
                url.into()
            },
        };
        let (value, next_url) = self.get_json(&url).await?;
        let events = value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("GitLab events response is not an array"))?;
        // The page watermark covers EVERY event on the page — including ones that map to no
        // comment (commit/snippet notes) — so a page of only-skipped events still advances the
        // stream instead of pinning it to the old date forever.
        let frontier = events
            .iter()
            .filter_map(|event| event["created_at"].as_str())
            .max()
            .map(str::to_string);
        let mut comments = Vec::new();
        for event in events {
            // The feed is creation events for notes; each embeds the note's CURRENT state. The
            // target_type varies by note subclass (`Note`, `DiscussionNote` for threaded MR
            // discussions, `DiffNote` for positioned comments — verified live), so the gate is
            // "carries a note on a tracked noteable", never a target_type list. Notes on
            // untracked noteables (commits, snippets) carry no iid and are skipped.
            let note = &event["note"];
            if note.is_null() {
                continue;
            }
            let kind = match note["noteable_type"].as_str() {
                Some("Issue") => ItemKind::Issue,
                Some("MergeRequest") => ItemKind::ChangeRequest,
                _ => continue,
            };
            let Some(iid) = note["noteable_iid"].as_u64().filter(|iid| *iid > 0) else {
                continue;
            };
            validate_positive_id(note, "id", "GitLab note event")?;
            if let Some(mut comment) =
                comment_from_note_value(project, kind, &iid.to_string(), note)
            {
                // The mirror advances this stream's frontier from comment.updated_at, and the
                // frontier must ride the feed's OWN ordering key — the event's created_at. An
                // edited old note embeds a much newer note.updated_at in its old creation
                // event; letting that inflate the first-page frontier shrinks the next scan's
                // replay window and can strand events missed to offset shifts. The stored stamp
                // therefore reflects event time until the next full walk's thread refetch
                // repairs it — cursor correctness over a cosmetic timestamp.
                comment.updated_at = event["created_at"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| comment.updated_at.take())
                    .or_else(|| comment.created_at.clone());
                comments.push(comment);
            }
        }
        let next = next_url.map(|page_token| PageCursor {
            stream: Some(EVENTS_STREAM.to_string()),
            updated_since: cursor.updated_since.clone(),
            page_token: Some(page_token),
            ..PageCursor::default()
        });
        Ok(CommentsPage { comments, next, frontier })
    }

    async fn freshness_probe(
        &self,
        project: &str,
        probe: &FreshnessProbe,
    ) -> anyhow::Result<FreshnessResult> {
        validate_gitlab_project(project)?;
        // Two 1-item calls (separate iid namespaces have separate lists); the max of the two is
        // the project's newest movement. No ETag: two responses cannot share one validator, and
        // `probe_advance` on timestamps suffices.
        let mut latest: Option<String> = None;
        let mut unavailable = 0usize;
        for leg in [ListLeg::Issues, ListLeg::MergeRequests] {
            let mut url = Url::parse(&self.project_path(project, leg.path_segment())?)?;
            url.query_pairs_mut()
                .append_pair("order_by", "updated_at")
                .append_pair("sort", "desc")
                .append_pair("per_page", "1");
            let response = self.core.get(url.as_str(), &gitlab_headers()).await?;
            // A disabled namespace (403/404 on its list) is quiet, not fatal — the sibling
            // namespace still answers the probe. Both unavailable = the project is gone.
            if matches!(response.status, 403 | 404) {
                unavailable += 1;
                continue;
            }
            ensure_success(response.status, &response.body)?;
            let value: Value = serde_json::from_str(&response.body)?;
            let leg_latest = value
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item["updated_at"].as_str())
                .map(str::to_string);
            latest = match (latest.take(), leg_latest) {
                (Some(current), Some(new)) => Some(current.max(new)),
                (current, new) => current.or(new),
            };
        }
        if unavailable == 2 {
            return Err(PapertrailClientError::ItemNotFound.into());
        }
        // GitLab has no probe validator (no shared ETag across the two namespace calls), so the
        // timestamp comparison IS the quiet signal: the mirror keys the delta-scan decision off
        // `not_modified`, and reporting quiet here is what keeps an idle project's poll at two
        // 1-item requests instead of two full delta legs.
        let latest = probe_advance(latest, probe.updated_since.as_deref());
        Ok(FreshnessResult { not_modified: latest.is_none(), latest, etag: probe.etag.clone() })
    }
}

const NOTES_STREAM: &str = "notes";
const EVENTS_STREAM: &str = "events";

/// The civil date one day before `timestamp`'s date part (`YYYY-MM-DD…` → `YYYY-MM-DD`), via
/// days-from-civil round-tripping (Howard Hinnant's algorithm) — no calendar dependency.
fn day_before(timestamp: &str) -> anyhow::Result<String> {
    let date = timestamp.get(..10).unwrap_or_default();
    let mut parts = date.split('-').map(|part| part.parse::<i64>());
    let (year, month, day) = match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(year)), Some(Ok(month)), Some(Ok(day)))
            if (1..=12).contains(&month) && (1..=31).contains(&day) =>
            (year, month, day),
        _ => anyhow::bail!("timestamp `{timestamp}` has no leading YYYY-MM-DD date"),
    };
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468 - 1; // one day earlier
    let era = (days + 719_468).div_euclid(146_097);
    let day_of_era = days + 719_468 - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

fn gitlab_headers() -> Vec<(&'static str, &'static str)> {
    vec![("accept", "application/json")]
}

fn ensure_success(status: u16, body: &str) -> anyhow::Result<()> {
    ensure_provider_success("GitLab", status, body)
}

/// GitLab project paths are `namespace(/subgroup)*/project` with segments drawn from GitLab's
/// path alphabet (letters, digits, `-`, `_`, `.`). Enforcing the alphabet here is what makes the
/// `%2F`-only encoding in [`GitLabClient::project_path`] sound.
fn validate_gitlab_project(project: &str) -> anyhow::Result<()> {
    let segments: Vec<&str> = project.split('/').collect();
    let safe = |segment: &str| {
        !segment.is_empty()
            && !matches!(segment, "." | "..")
            && segment.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    };
    anyhow::ensure!(
        segments.len() >= 2 && segments.iter().all(|segment| safe(segment)),
        "invalid GitLab project `{project}`"
    );
    Ok(())
}

/// GitLab item keys are iids — positive integers within their kind's namespace.
fn validate_iid(key: &str) -> anyhow::Result<i64> {
    key.parse::<i64>()
        .ok()
        .filter(|iid| *iid > 0)
        .ok_or_else(|| anyhow::anyhow!("invalid GitLab iid `{key}`"))
}

fn item_from_gitlab_value(
    project: &str,
    kind: ItemKind,
    value: &Value,
) -> anyhow::Result<PapertrailItem> {
    validate_positive_id(value, "iid", "GitLab item")?;
    anyhow::ensure!(value["updated_at"].as_str().is_some(), "GitLab item has no valid updated_at");
    Ok(PapertrailItem {
        project: project.to_string(),
        item_kind: kind,
        item_key: value["iid"].as_u64().expect("validated").to_string(),
        url: value["web_url"].as_str().unwrap_or_default().to_string(),
        state: value["state"].as_str().unwrap_or_default().to_string(),
        title: value["title"].as_str().unwrap_or_default().to_string(),
        body: value["description"].as_str().unwrap_or_default().to_string(),
        author: value["author"]["username"].as_str().map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
        merged_at: value["merged_at"].as_str().map(str::to_string),
        // GitLab labels are plain strings (GitHub's are objects).
        tags: value["labels"]
            .as_array()
            .map(|labels| labels.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
    })
}

/// Map one note. `None` drops it: system noise ("added label …") never becomes a comment row.
/// Approval events are the exception — they are system notes, and mapping them (instead of the
/// `/approvals` endpoint, which has no per-event history) yields review comments with real
/// authors and timestamps. Scoped to `system: true`, so a user comment cannot spoof
/// `review_state`.
fn comment_from_note_value(
    project: &str,
    kind: ItemKind,
    key: &str,
    value: &Value,
) -> Option<PapertrailComment> {
    let system = value["system"].as_bool().unwrap_or(false);
    let body = value["body"].as_str().unwrap_or_default();
    let review_state = if system {
        match (kind, body) {
            (ItemKind::ChangeRequest, "approved this merge request") => Some("APPROVED"),
            (ItemKind::ChangeRequest, "unapproved this merge request") => Some("UNAPPROVED"),
            (ItemKind::ChangeRequest, "requested changes") => Some("CHANGES_REQUESTED"),
            _ => return None,
        }
    } else {
        None
    };
    let anchor_path = value["position"]["new_path"]
        .as_str()
        .or_else(|| value["position"]["old_path"].as_str())
        .map(str::to_string);
    Some(PapertrailComment {
        project: project.to_string(),
        item_kind: kind,
        item_key: key.to_string(),
        comment_id: format!("note:{}", value["id"].as_u64().unwrap_or_default()),
        url: None,
        body: body.to_string(),
        author: value["author"]["username"].as_str().map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
        review_state: review_state.map(str::to_string),
        anchor_path,
    })
}

#[cfg(test)]
mod tests {
    use super::super::transport::stub::{StubResponse, spawn_script_stub};
    use super::*;

    fn binding(base_url: String) -> ResolvedTracker {
        ResolvedTracker {
            provider: Tracker::Gitlab,
            project: "g/s/r".to_string(),
            base_url: Some(base_url),
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        }
    }

    fn client(base: String, registry: &GovernorRegistry) -> GitLabClient {
        GitLabClient::new(&binding(base), registry, TransportOptions::default()).unwrap()
    }

    fn issue(iid: i64, updated_at: &str) -> String {
        format!(
            r#"{{"iid":{iid},"web_url":"{{BASE}}/g/s/r/-/issues/{iid}","state":"opened","title":"item {iid}","description":"body","author":{{"username":"u"}},"created_at":"2026-01-01T00:00:00Z","updated_at":"{updated_at}","labels":["bug","ux"]}}"#
        )
    }

    fn merge_request(iid: i64, updated_at: &str) -> String {
        format!(
            r#"{{"iid":{iid},"web_url":"{{BASE}}/g/s/r/-/merge_requests/{iid}","state":"merged","title":"mr {iid}","description":"change","author":{{"username":"m"}},"created_at":"2026-01-01T00:00:00Z","updated_at":"{updated_at}","merged_at":"2026-01-04T00:00:00Z","labels":[]}}"#
        )
    }

    fn with_next_link(mut response: StubResponse, path_and_query: &str) -> StubResponse {
        response
            .headers
            .push(("Link".to_string(), format!("<{{BASE}}{path_and_query}>; rel=\"next\"")));
        response
    }

    #[test]
    fn constructor_validates_provider_and_api_origin() {
        let registry = GovernorRegistry::default();
        let mut github = binding("http://127.0.0.1:1".to_string());
        github.provider = Tracker::Github;
        let error = GitLabClient::new(&github, &registry, TransportOptions::default())
            .err()
            .expect("provider mismatch must fail")
            .to_string();
        assert!(error.contains("not a GitLab binding"), "{error}");

        for (base, expected) in [
            ("https://gitlab.example.com/sub/path", "without a path"),
            ("https://user:pw@gitlab.example.com", "credentials"),
            ("ftp://gitlab.example.com", "http(s)"),
            ("https://gitlab.example.com?x=1", "without a path"),
        ] {
            let error = GitLabClient::new(
                &binding(base.to_string()),
                &registry,
                TransportOptions::default(),
            )
            .err()
            .expect("malformed base URL must fail")
            .to_string();
            assert!(error.contains(expected), "`{base}`: {error}");
        }
    }

    #[test]
    fn iid_namespaces_are_binding_and_subgroups_encode_as_one_segment() {
        let (base, handle) = spawn_script_stub(vec![
            StubResponse::ok(&issue(5, "2026-01-02T00:00:00Z")),
            StubResponse::ok(&merge_request(5, "2026-01-03T00:00:00Z")),
        ]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let issue_item = block_on(client.item("g/s/r", ItemKind::Issue, "5")).unwrap();
        assert_eq!(issue_item.item_kind, ItemKind::Issue);
        assert_eq!(issue_item.item_key, "5");
        assert_eq!(issue_item.tags, vec!["bug".to_string(), "ux".to_string()]);
        assert_eq!(issue_item.merged_at, None);

        let change = block_on(client.item("g/s/r", ItemKind::ChangeRequest, "5")).unwrap();
        assert_eq!(change.item_kind, ItemKind::ChangeRequest);
        assert_eq!(change.merged_at.as_deref(), Some("2026-01-04T00:00:00Z"));
        assert_eq!(change.state, "merged");

        let heads = handle.join().unwrap();
        assert!(heads[0].starts_with("GET /api/v4/projects/g%2Fs%2Fr/issues/5 "), "{}", heads[0]);
        assert!(
            heads[1].starts_with("GET /api/v4/projects/g%2Fs%2Fr/merge_requests/5 "),
            "{}",
            heads[1]
        );
    }

    #[test]
    fn notes_map_comments_anchors_and_approvals_and_drop_system_noise() {
        let notes = r#"[
            {"id":1,"body":"plain words","system":false,"author":{"username":"a"},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},
            {"id":2,"body":"diff remark","system":false,"position":{"new_path":"src/lib.rs","old_path":"src/old.rs"},"author":{"username":"b"},"created_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-02T00:00:00Z"},
            {"id":3,"body":"approved this merge request","system":true,"author":{"username":"c"},"created_at":"2026-01-03T00:00:00Z","updated_at":"2026-01-03T00:00:00Z"},
            {"id":4,"body":"added ~bug label","system":true,"author":{"username":"d"},"created_at":"2026-01-04T00:00:00Z","updated_at":"2026-01-04T00:00:00Z"}
        ]"#;
        let (base, handle) = spawn_script_stub(vec![StubResponse::ok(notes)]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let comments =
            block_on(client.item_comments("g/s/r", ItemKind::ChangeRequest, "9")).unwrap();
        assert_eq!(comments.len(), 3, "system noise must be dropped");
        assert_eq!(comments[0].comment_id, "note:1");
        assert_eq!(comments[0].review_state, None);
        assert_eq!(comments[0].anchor_path, None);
        assert_eq!(comments[1].anchor_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(comments[2].review_state.as_deref(), Some("APPROVED"));
        assert_eq!(comments[2].author.as_deref(), Some("c"));
        assert_eq!(comments[2].item_kind, ItemKind::ChangeRequest);

        let heads = handle.join().unwrap();
        assert!(
            heads[0].starts_with(
                "GET /api/v4/projects/g%2Fs%2Fr/merge_requests/9/notes?order_by=updated_at&\
                 sort=asc&per_page=100 "
            ),
            "{}",
            heads[0]
        );
    }

    #[test]
    fn approval_words_from_a_user_comment_never_spoof_review_state() {
        let notes = r#"[{"id":8,"body":"approved this merge request","system":false,"author":{"username":"prankster"},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}]"#;
        let (base, _handle) = spawn_script_stub(vec![StubResponse::ok(notes)]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);
        let comments =
            block_on(client.item_comments("g/s/r", ItemKind::ChangeRequest, "9")).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].review_state, None, "a plain comment stays a plain comment");
    }

    #[test]
    fn backfill_fetches_both_legs_in_parallel_and_reports_the_global_boundary() {
        let issues_page_one = format!(
            "[{},{}]",
            issue(20, "2026-01-10T00:00:00Z"),
            issue(19, "2026-01-05T00:00:00Z")
        );
        let issues_page_two = format!("[{}]", issue(18, "2026-01-03T00:00:00Z"));
        let change_page = format!("[{}]", merge_request(7, "2026-01-08T00:00:00Z"));
        let (base, handle) = spawn_script_stub(vec![
            with_next_link(
                StubResponse::ok(&issues_page_one),
                "/api/v4/projects/g%2Fs%2Fr/issues?page=2",
            ),
            StubResponse::ok(&change_page),
            StubResponse::ok(&issues_page_two),
        ]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let mut cursor = PageCursor {
            updated_before: Some("2026-02-01T00:00:00Z".to_string()),
            ..PageCursor::default()
        };
        let first = block_on(client.items_page("g/s/r", &cursor)).unwrap();
        // BOTH legs' first pages arrive in the scan's first page, merged newest-first, so the
        // mirror's initial high mark covers both iid namespaces.
        assert_eq!(
            first
                .items
                .iter()
                .map(|item| (item.item_kind, item.item_key.as_str()))
                .collect::<Vec<_>>(),
            vec![(ItemKind::Issue, "20"), (ItemKind::ChangeRequest, "7"), (ItemKind::Issue, "19"),]
        );
        assert_eq!(first.backfill_boundary, None, "mid-cycle pages report no boundary");
        cursor = first.next.expect("the issues leg still has a page");
        assert!(cursor.page_token.is_some(), "continuations must not look like first pages");

        let second = block_on(client.items_page("g/s/r", &cursor)).unwrap();
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].item_key, "18");
        assert!(second.next.is_none(), "both legs drained");
        assert_eq!(
            second.backfill_boundary.as_deref(),
            Some("2026-01-03T00:00:00Z"),
            "the boundary is the GLOBAL oldest across both legs, not the last page's"
        );

        let heads = handle.join().unwrap();
        assert_eq!(heads.len(), 3);
        assert!(
            heads[0].contains("/issues?order_by=updated_at&per_page=100&sort=desc"),
            "{}",
            heads[0]
        );
        assert!(heads[0].contains("updated_before=2026-02-01"), "{}", heads[0]);
        assert!(heads[1].contains("/merge_requests?"), "{}", heads[1]);
        assert!(heads[1].contains("updated_before=2026-02-01"), "{}", heads[1]);
        assert!(heads[2].contains("/issues?page=2"), "{}", heads[2]);
    }

    #[test]
    fn an_empty_project_backfill_terminates_with_the_done_shape() {
        let (base, handle) =
            spawn_script_stub(vec![StubResponse::ok("[]"), StubResponse::ok("[]")]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let cursor = PageCursor {
            updated_before: Some("2026-02-01T00:00:00Z".to_string()),
            ..PageCursor::default()
        };
        let page = block_on(client.items_page("g/s/r", &cursor)).unwrap();
        assert!(page.items.is_empty());
        assert!(page.next.is_none());
        assert!(page.backfill_boundary.is_none(), "nothing consumed = the mirror's done shape");
        assert_eq!(handle.join().unwrap().len(), 2, "one page per leg, no extra cycles");
    }

    /// The high-watermark half of the parallel-leg contract: the delta scan's FIRST page must
    /// contain both namespaces, or the persisted frontier misses whichever namespace is newer
    /// and replays it on every scan.
    #[test]
    fn delta_merges_both_namespaces_into_the_scan_first_page() {
        let issues = format!("[{}]", issue(21, "2026-01-11T00:00:00Z"));
        let changes = format!("[{}]", merge_request(8, "2026-01-12T00:00:00Z"));
        let (base, handle) =
            spawn_script_stub(vec![StubResponse::ok(&issues), StubResponse::ok(&changes)]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let cursor = PageCursor {
            updated_since: Some("2026-01-09T00:00:00Z".to_string()),
            ..PageCursor::default()
        };
        let page = block_on(client.items_page("g/s/r", &cursor)).unwrap();
        assert_eq!(
            page.items.iter().map(|item| item.item_kind).collect::<Vec<_>>(),
            vec![ItemKind::Issue, ItemKind::ChangeRequest],
            "ascending merge, both namespaces present"
        );
        assert_eq!(
            page.items.iter().filter_map(|item| item.updated_at.as_deref()).max(),
            Some("2026-01-12T00:00:00Z"),
            "the first-page maximum covers the newer namespace"
        );
        assert!(page.next.is_none());
        assert!(page.backfill_boundary.is_none(), "delta never reports a backfill boundary");

        let heads = handle.join().unwrap();
        for head in &heads {
            assert!(head.contains("sort=asc"), "{head}");
            assert!(head.contains("updated_after=2026-01-09"), "{head}");
        }
    }

    /// The mirror's delta checkpoint persists ONLY `page_token` and rebuilds its resume
    /// request without provider_state — a pause between logical pages must resume the saved
    /// legs, not reopen both first pages (which would spend every run's budget on page one).
    #[test]
    fn delta_resumes_from_the_page_token_alone_like_the_mirror_checkpoint() {
        let issues_page_one = format!("[{}]", issue(21, "2026-01-11T00:00:00Z"));
        let issues_page_two = format!("[{}]", issue(22, "2026-01-13T00:00:00Z"));
        let changes = format!("[{}]", merge_request(8, "2026-01-12T00:00:00Z"));
        let (base, handle) = spawn_script_stub(vec![
            with_next_link(
                StubResponse::ok(&issues_page_one),
                "/api/v4/projects/g%2Fs%2Fr/issues?page=2",
            ),
            StubResponse::ok(&changes),
            StubResponse::ok(&issues_page_two),
        ]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let first = block_on(client.items_page("g/s/r", &PageCursor {
            updated_since: Some("2026-01-09T00:00:00Z".to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        let next = first.next.expect("issues leg still pending");

        // Rebuild the resume request the way sync_item_delta does: page_token ONLY.
        let resumed = block_on(client.items_page("g/s/r", &PageCursor {
            updated_since: Some("2026-01-09T00:00:00Z".to_string()),
            page_token: next.page_token.clone(),
            ..PageCursor::default()
        }))
        .unwrap();
        assert_eq!(resumed.items.len(), 1);
        assert_eq!(resumed.items[0].item_key, "22");
        assert!(resumed.next.is_none());
        let heads = handle.join().unwrap();
        assert_eq!(heads.len(), 3, "the resume fetched ONE page, not two reopened legs");
        assert!(heads[2].contains("/issues?page=2"), "{}", heads[2]);
    }

    /// Threaded MR discussions arrive as `target_type: "DiscussionNote"` events (verified
    /// live); a `target_type == "Note"` gate silently drops them and quiet discussion comments
    /// would never mirror incrementally. The frontier falls back to the event's created_at when
    /// a payload omits the note's updated_at — a None frontier replays the feed every sync.
    #[test]
    fn comment_events_accept_discussion_notes_and_fall_back_to_event_timestamps() {
        let events = r#"[
            {"target_type":"DiscussionNote","created_at":"2026-01-13T00:00:00Z","note":{"id":40,"body":"threaded words","system":false,"type":"DiscussionNote","noteable_type":"MergeRequest","noteable_iid":6,"author":{"username":"t"},"created_at":"2026-01-12T23:59:00Z"}}
        ]"#;
        let (base, _handle) = spawn_script_stub(vec![StubResponse::ok(events)]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);
        let page = block_on(client.comments_page("g/s/r", &PageCursor::default())).unwrap();
        assert_eq!(page.comments.len(), 1);
        assert_eq!(page.comments[0].item_kind, ItemKind::ChangeRequest);
        assert_eq!(page.comments[0].item_key, "6");
        assert_eq!(
            page.comments[0].updated_at.as_deref(),
            Some("2026-01-13T00:00:00Z"),
            "missing note updated_at falls back to the event's created_at"
        );
    }

    /// GitLab projects can disable issues or merge requests as a feature; the unavailable leg
    /// mirrors as empty instead of aborting the sibling namespace. EVERY leg unavailable on a
    /// fresh open is a missing project and must surface as the typed not-found outcome.
    #[test]
    fn a_disabled_namespace_never_aborts_the_sibling_leg() {
        let changes = format!("[{}]", merge_request(9, "2026-01-05T00:00:00Z"));
        let (base, _handle) = spawn_script_stub(vec![
            StubResponse::status("403 Forbidden", "{}"),
            StubResponse::ok(&changes),
        ]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);
        let page = block_on(client.items_page("g/s/r", &PageCursor {
            updated_before: Some("2026-02-01T00:00:00Z".to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].item_kind, ItemKind::ChangeRequest);
        assert!(page.next.is_none());
        assert_eq!(page.backfill_boundary.as_deref(), Some("2026-01-05T00:00:00Z"));
    }

    #[test]
    fn a_fully_unavailable_project_surfaces_as_the_typed_not_found_outcome() {
        let (base, _handle) = spawn_script_stub(vec![
            StubResponse::status("404 Not Found", "{}"),
            StubResponse::status("404 Not Found", "{}"),
        ]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);
        let error = block_on(client.items_page("g/s/r", &PageCursor {
            updated_before: Some("2026-02-01T00:00:00Z".to_string()),
            ..PageCursor::default()
        }))
        .unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<PapertrailClientError>(),
                Some(PapertrailClientError::ItemNotFound)
            ),
            "{error:#}"
        );
    }

    #[test]
    fn the_probe_tolerates_one_disabled_namespace_and_fails_on_two() {
        let (base, _handle) = spawn_script_stub(vec![
            StubResponse::status("403 Forbidden", "{}"),
            StubResponse::ok(&format!("[{}]", merge_request(2, "2026-01-07T00:00:00Z"))),
            StubResponse::status("404 Not Found", "{}"),
            StubResponse::status("404 Not Found", "{}"),
        ]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let probe = block_on(client.freshness_probe("g/s/r", &FreshnessProbe {
            updated_since: Some("2026-01-06T00:00:00Z".to_string()),
            etag: None,
        }))
        .unwrap();
        assert_eq!(probe.latest.as_deref(), Some("2026-01-07T00:00:00Z"));

        let error =
            block_on(client.freshness_probe("g/s/r", &FreshnessProbe::default())).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<PapertrailClientError>(),
                Some(PapertrailClientError::ItemNotFound)
            ),
            "{error:#}"
        );
    }

    #[test]
    fn freshness_probe_takes_the_max_of_both_namespaces_and_stays_quiet_at_the_cursor() {
        let (base, handle) = spawn_script_stub(vec![
            StubResponse::ok(&format!("[{}]", issue(1, "2026-01-05T00:00:00Z"))),
            StubResponse::ok(&format!("[{}]", merge_request(2, "2026-01-07T00:00:00Z"))),
            StubResponse::ok(&format!("[{}]", issue(1, "2026-01-05T00:00:00Z"))),
            StubResponse::ok(&format!("[{}]", merge_request(2, "2026-01-07T00:00:00Z"))),
        ]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let moved = block_on(client.freshness_probe("g/s/r", &FreshnessProbe {
            updated_since: Some("2026-01-06T00:00:00Z".to_string()),
            etag: None,
        }))
        .unwrap();
        assert_eq!(moved.latest.as_deref(), Some("2026-01-07T00:00:00Z"));
        assert!(!moved.not_modified);

        let quiet = block_on(client.freshness_probe("g/s/r", &FreshnessProbe {
            updated_since: Some("2026-01-07T00:00:00Z".to_string()),
            etag: None,
        }))
        .unwrap();
        assert_eq!(quiet.latest, None, "at-or-before the cursor must stay quiet");
        assert!(
            quiet.not_modified,
            "quiet must be reported in the flag the mirror keys the delta decision off, or an \
             idle project pays two full delta legs every poll"
        );

        let heads = handle.join().unwrap();
        assert!(heads[0].contains("per_page=1"), "{}", heads[0]);
        assert!(heads[0].contains("sort=desc"), "{}", heads[0]);
    }

    #[test]
    fn not_found_is_a_typed_item_outcome() {
        let (base, _handle) = spawn_script_stub(vec![StubResponse::status("404 Not Found", "{}")]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);
        let error = block_on(client.item("g/s/r", ItemKind::Issue, "3")).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<PapertrailClientError>(),
                Some(PapertrailClientError::ItemNotFound)
            ),
            "{error:#}"
        );
    }

    #[test]
    fn malformed_projects_and_iids_are_rejected_before_any_request() {
        let (base, handle) = spawn_script_stub(Vec::new());
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        for project in ["solo", "g//r", "g/r r", "../r", "g/r%2Fx", "g/r?x"] {
            let error = block_on(client.item(project, ItemKind::Issue, "1")).unwrap_err();
            assert!(error.to_string().contains("invalid GitLab project"), "`{project}`: {error}");
        }
        for iid in ["0", "-3", "abc", "1.5", ""] {
            let error = block_on(client.item("g/s/r", ItemKind::Issue, iid)).unwrap_err();
            assert!(error.to_string().contains("invalid GitLab iid"), "`{iid}`: {error}");
        }
        assert!(handle.join().unwrap().is_empty(), "no request may leave the process");
    }

    #[test]
    fn cross_origin_pagination_is_rejected_before_following_it() {
        let (base, handle) = spawn_script_stub(Vec::new());
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);
        let foreign = "https://gitlab.com/api/v4/projects/g%2Fs%2Fr/issues?page=2";
        let cursor = PageCursor {
            updated_before: Some("2026-02-01T00:00:00Z".to_string()),
            page_token: Some(foreign.to_string()),
            provider_state: Some(format!(r#"{{"issues_next":"{foreign}"}}"#)),
            ..PageCursor::default()
        };
        let error = block_on(client.items_page("g/s/r", &cursor)).unwrap_err();
        assert!(error.to_string().contains("outside the binding"), "{error:#}");
        assert!(handle.join().unwrap().is_empty(), "the foreign URL was never requested");
    }

    #[test]
    fn comment_events_map_embedded_notes_and_skip_untracked_noteables() {
        let events = r#"[
            {"target_type":"Note","created_at":"2026-01-10T00:00:30Z","note":{"id":30,"body":"issue words","system":false,"noteable_type":"Issue","noteable_iid":4,"author":{"username":"a"},"created_at":"2026-01-10T00:00:00Z","updated_at":"2026-01-12T00:00:00Z"}},
            {"target_type":"Note","created_at":"2026-01-11T00:00:30Z","note":{"id":31,"body":"mr words","system":false,"noteable_type":"MergeRequest","noteable_iid":4,"author":{"username":"b"},"created_at":"2026-01-11T00:00:00Z","updated_at":"2026-01-11T00:00:00Z"}},
            {"target_type":"Note","created_at":"2026-01-11T00:01:00Z","note":{"id":32,"body":"commit words","system":false,"noteable_type":"Commit","author":{"username":"c"},"created_at":"2026-01-11T00:00:00Z","updated_at":"2026-01-11T00:00:00Z"}},
            {"target_type":"Issue","target_iid":9}
        ]"#;
        let (base, handle) = spawn_script_stub(vec![StubResponse::ok(events)]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);

        let page = block_on(client.comments_page("g/s/r", &PageCursor {
            stream: Some("events".to_string()),
            updated_since: Some("2026-01-10T08:00:00Z".to_string()),
            ..PageCursor::default()
        }))
        .unwrap();
        assert_eq!(page.comments.len(), 2, "commit notes and non-note events are skipped");
        assert_eq!(page.comments[0].item_kind, ItemKind::Issue);
        assert_eq!(page.comments[0].item_key, "4");
        assert_eq!(page.comments[0].comment_id, "note:30");
        assert_eq!(page.comments[0].body, "issue words", "the embedded note body is current");
        assert_eq!(
            page.comments[0].updated_at.as_deref(),
            Some("2026-01-10T00:00:30Z"),
            "the frontier stamp is the EVENT's created_at — an edited old note must not inflate \
             the first-page frontier past the feed's ordering key"
        );
        assert_eq!(page.comments[1].item_kind, ItemKind::ChangeRequest);
        assert_eq!(page.comments[1].item_key, "4");
        assert!(page.next.is_none());

        let heads = handle.join().unwrap();
        assert!(
            heads[0].starts_with(
                "GET /api/v4/projects/g%2Fs%2Fr/events?action=commented&sort=asc&per_page=100&\
                 after=2026-01-09 "
            ),
            "the day BEFORE the cursor's date keeps same-day events in scope: {}",
            heads[0]
        );
    }

    /// Commit/snippet notes map to no comment; a page of only those must still carry the
    /// events frontier so the mirror can advance instead of replaying the page forever.
    #[test]
    fn a_page_of_untracked_note_events_still_reports_its_frontier() {
        let events = r#"[
            {"target_type":"Note","created_at":"2026-01-14T00:00:00Z","note":{"id":50,"body":"commit words","system":false,"noteable_type":"Commit","author":{"username":"c"},"created_at":"2026-01-14T00:00:00Z"}},
            {"target_type":"Note","created_at":"2026-01-15T00:00:00Z","note":{"id":51,"body":"snippet words","system":false,"noteable_type":"Snippet","author":{"username":"s"},"created_at":"2026-01-15T00:00:00Z"}}
        ]"#;
        let (base, _handle) = spawn_script_stub(vec![StubResponse::ok(events)]);
        let registry = GovernorRegistry::default();
        let client = client(base, &registry);
        let page = block_on(client.comments_page("g/s/r", &PageCursor::default())).unwrap();
        assert!(page.comments.is_empty());
        assert_eq!(
            page.frontier.as_deref(),
            Some("2026-01-15T00:00:00Z"),
            "the frontier covers skipped events so the stream never pins"
        );
    }

    #[test]
    fn day_before_handles_month_year_and_leap_boundaries() {
        for (timestamp, expected) in [
            ("2026-07-15T13:16:18.443Z", "2026-07-14"),
            ("2026-03-01T00:00:00Z", "2026-02-28"),
            ("2024-03-01T00:00:00Z", "2024-02-29"),
            ("2026-01-01T00:00:00Z", "2025-12-31"),
            ("2026-05-01", "2026-04-30"),
        ] {
            assert_eq!(day_before(timestamp).unwrap(), expected, "{timestamp}");
        }
        for garbage in ["", "notadate", "2026-13-01T00:00:00Z", "2026-01"] {
            assert!(day_before(garbage).is_err(), "`{garbage}` must be rejected");
        }
    }
}
