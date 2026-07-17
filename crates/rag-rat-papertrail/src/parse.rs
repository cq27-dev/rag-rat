use serde_json::Value;

use super::*;

/// Ref-bearing tokens of `text` — the shared tokenizer of the legacy GitHub-only lane
/// ([`parse_refs`]) and the provider-keyed grammar ([`parse_tracker_refs`]). The split/trim sets
/// deliberately exclude `#`, `!`, and `-`, which are ref syntax; a TRAILING `!`/`?` is sentence
/// punctuation and end-trimmed only ("fixes #5!" — a LEADING `!` is a GitLab MR ref). The
/// boundary discipline mirrors the reference parsers this grammar is checked against (Gitea's
/// `references.go` patterns accept `(`/`[`/quotes before and `)`/`]`/quotes/`[:;,.?!]` after).
fn ref_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || [',', ';', ')', ']', '}'].contains(&c))
        .map(|token| {
            token
                .trim_matches(|c: char| ['(', '[', '{', '.', ':', '\'', '"'].contains(&c))
                .trim_end_matches(['!', '?'])
        })
        .filter(|token| !token.is_empty())
}
pub(crate) fn parse_refs(text: &str, default_repo: Option<&str>) -> Vec<ParsedRef> {
    let mut refs = Vec::new();
    let mut previous = "";
    for token in ref_tokens(text) {
        let kind = ref_kind(previous);
        if let Some(parsed) = parse_issue_ref(token, default_repo) {
            refs.push(ParsedRef { kind, ..parsed });
        }
        previous = token;
    }
    refs
}
/// The legacy GitHub-only parser retained for the manual single-item API and compatibility tests.
/// Production discovery and rationale lookup consume [`parse_tracker_refs`] directly.
pub(crate) fn parse_issue_ref(token: &str, default_repo: Option<&str>) -> Option<ParsedRef> {
    let parsed = github_token_ref(token, default_repo, "https://github.com", true)?;
    // The GitHub grammar only ever emits two-segment `owner/repo` projects.
    split_repo(&parsed.project)?;
    Some(ParsedRef {
        project: parsed.project,
        number: parsed.number,
        kind: parsed.shape.to_string(),
    })
}
struct GithubTokenRef {
    project: String,
    number: i64,
    /// `Some` only for `/pull/`, whose path unambiguously names the kind. `/issues/` and shorthand
    /// refs stay `None`: GitHub serves pull requests through issue URLs too, so the provider must
    /// resolve their kind.
    item_kind: Option<ItemKind>,
    shape: &'static str,
}
/// The GitHub token grammar — `<base>/owner/repo/{issues|pull}/N` URLs, `owner/repo#N`, `GH-N`,
/// bare `#N` — shared by the legacy lane and the provider-keyed grammar so the two can never
/// drift. `local_number` gates the bare-`#N` arm: bare refs resolve against the code-host
/// binding only.
fn github_token_ref(
    token: &str,
    default_project: Option<&str>,
    url_base: &str,
    local_number: bool,
) -> Option<GithubTokenRef> {
    if let Some(rest) = token.strip_prefix(url_base).and_then(|rest| rest.strip_prefix('/')) {
        let parts = rest.split('/').collect::<Vec<_>>();
        if parts.len() >= 4 && (parts[2] == "issues" || parts[2] == "pull") {
            return Some(GithubTokenRef {
                project: format!("{}/{}", parts[0], parts[1]),
                number: ref_number(parts[3])?,
                item_kind: (parts[2] == "pull").then_some(ItemKind::ChangeRequest),
                shape: "url",
            });
        }
    }
    if let Some((repo_ref, number)) = token.split_once('#') {
        let parts = repo_ref.split('/').collect::<Vec<_>>();
        if parts.len() == 2 {
            return Some(GithubTokenRef {
                project: repo_ref.to_string(),
                number: ref_number(number)?,
                item_kind: None,
                shape: "cross_repo",
            });
        }
    }
    // `GH-N` resolves against the binding's OWN project — a local shorthand exactly like bare
    // `#N`, gated the same way (the two-pass source router owns it, never config order).
    if local_number && let Some(number) = token.strip_prefix("GH-") {
        return Some(GithubTokenRef {
            project: default_project?.to_string(),
            number: ref_number(number)?,
            item_kind: None,
            shape: "gh_dash",
        });
    }
    if local_number && let Some(number) = token.strip_prefix('#') {
        return Some(GithubTokenRef {
            project: default_project?.to_string(),
            number: ref_number(number)?,
            item_kind: None,
            shape: "local_number",
        });
    }
    None
}
/// A tracker item reference parsed from free text under one binding's grammar — the
/// provider-keyed successor of [`ParsedRef`]. `item_kind` is `Some` only where the SYNTAX names
/// the kind unambiguously (a GitHub `/pull/` URL, GitLab's `!N`); GitHub `/issues/` and `#N` stay
/// `None` for the shared-numbering provider to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerParsedRef {
    pub provider: Tracker,
    pub project: String,
    /// Provider item key, stringly — numeric for the code hosts, `PROJ-123` for Jira.
    pub item_key: String,
    pub item_kind: Option<ItemKind>,
    /// `closing` / `reference` / `unknown` from the preceding word, exactly as [`parse_refs`].
    pub ref_kind: String,
}

impl TrackerParsedRef {
    pub(crate) fn into_ref(
        self,
        source_kind: &str,
        source_path: Option<String>,
        source_commit: Option<String>,
        source_text: String,
    ) -> PapertrailRef {
        PapertrailRef {
            tracker: self.provider,
            project: self.project,
            item_key: self.item_key,
            item_kind: self.item_kind,
            ref_kind: self.ref_kind,
            source_kind: source_kind.to_string(),
            source_path,
            source_commit,
            source_text,
        }
    }
}
/// Provider-keyed ref discovery over `text` for the resolved bindings — the annotation-layer
/// grammar behind the multi-tracker mirror. Tokenization matches [`parse_refs`]; each token is
/// claimed by the FIRST binding whose grammar matches, which keeps overlapping grammars
/// deterministic (`o/r#1` parses under GitHub and GitLab alike; `GH-1` under GitHub and a Jira
/// `GH` project). Bare `#N` resolves against the FIRST code-host binding only, per the config
/// contract; GitLab's bare `!N` is provider-explicit syntax and resolves against its own
/// binding regardless.
pub fn parse_tracker_refs(text: &str, trackers: &[ResolvedTracker]) -> Vec<TrackerParsedRef> {
    parse_tracker_refs_with_bindings(text, trackers).into_iter().map(|(_, parsed)| parsed).collect()
}

/// [`parse_tracker_refs`] with an explicit BARE-SHORTHAND owner: qualified and URL refs resolve
/// against the CONFIGURED bindings in config order (exactly like the plain form), but a bare
/// `#N` / `!N` belongs to `source` — the item/comment being mined — even when that project is
/// not in the configured list (a fetched foreign item) and even when another code-host binding
/// is configured first. Two passes per token, so source routing can never reroute a fully
/// qualified cross-provider ref.
pub(crate) fn parse_tracker_refs_with_source(
    text: &str,
    trackers: &[ResolvedTracker],
    source: &ResolvedTracker,
) -> Vec<TrackerParsedRef> {
    let mut refs = Vec::new();
    let mut previous = "";
    for token in ref_tokens(text) {
        // Pass 0: the SOURCE's local shorthand shapes (`#N`, `!N`, `GH-N`) — they can never
        // collide with a qualified/URL shape, and they must beat cross-tracker grammars that
        // happen to overlap (a Jira project named `GH` vs a GitHub source's `GH-5`).
        let local = source
            .provider
            .is_code_host()
            .then(|| tracker_token_ref_shaped(token, source, true, true, true))
            .flatten();
        // Pass 1: qualified/URL grammars only, config order (every bare arm disabled).
        let parsed = local.or_else(|| {
            trackers.iter().find_map(|tracker| tracker_token_ref(token, tracker, false, false))
        });
        if let Some(mut parsed) = parsed {
            parsed.ref_kind = ref_kind_for(parsed.provider, previous);
            refs.push(parsed);
        }
        previous = token;
    }
    refs
}

/// The routing-aware form of [`parse_tracker_refs`]. The binding index is the exact binding whose
/// grammar claimed the token under the same first-match rule, so callers can distinguish cloud
/// GitHub from Enterprise without reconstructing provenance from the normalized project.
pub(crate) fn parse_tracker_refs_with_bindings(
    text: &str,
    trackers: &[ResolvedTracker],
) -> Vec<(usize, TrackerParsedRef)> {
    let code_host = trackers.iter().position(|tracker| tracker.provider.is_code_host());
    let mut refs = Vec::new();
    let mut previous = "";
    for token in ref_tokens(text) {
        if let Some((binding_index, mut parsed)) =
            trackers.iter().enumerate().find_map(|(index, tracker)| {
                tracker_token_ref(token, tracker, code_host == Some(index), true)
                    .map(|parsed| (index, parsed))
            })
        {
            parsed.ref_kind = ref_kind_for(parsed.provider, previous);
            refs.push((binding_index, parsed));
        }
        previous = token;
    }
    refs
}
fn tracker_token_ref(
    token: &str,
    tracker: &ResolvedTracker,
    is_code_host: bool,
    allow_bare: bool,
) -> Option<TrackerParsedRef> {
    tracker_token_ref_shaped(token, tracker, is_code_host, allow_bare, false)
}

/// `bare_only` restricts matching to the LOCAL SHORTHAND shapes (`#N`, `!N`, `GH-N`) — the
/// source-probe pass of the two-pass router uses it so a source-local shorthand can win before
/// any cross-tracker grammar (a Jira project literally named `GH` must not claim a GitHub
/// source's `GH-5`), while qualified/URL shapes stay config-order-claimed.
fn tracker_token_ref_shaped(
    token: &str,
    tracker: &ResolvedTracker,
    is_code_host: bool,
    allow_bare: bool,
    bare_only: bool,
) -> Option<TrackerParsedRef> {
    match tracker.provider {
        Tracker::Github => {
            let base = url_base(tracker, "https://github.com");
            let parsed =
                github_token_ref(token, Some(&tracker.project), base, is_code_host && allow_bare)?;
            if bare_only && !matches!(parsed.shape, "local_number" | "gh_dash") {
                return None;
            }
            Some(tracker_ref(tracker, parsed.project, parsed.number.to_string(), parsed.item_kind))
        },
        Tracker::Gitlab => {
            let parsed = gitlab_token_ref(token, tracker, is_code_host, allow_bare)?;
            // GitLab's bare shapes resolve to the binding's own project with no `/` in the
            // token; a qualified `a/b#N` or URL match is never bare.
            if bare_only && (token.contains('/') || parsed.project != tracker.project) {
                return None;
            }
            Some(parsed)
        },
        Tracker::Bitbucket => {
            let parsed = bitbucket_token_ref(token, tracker, is_code_host && allow_bare)?;
            if bare_only && (token.contains('/') || parsed.project != tracker.project) {
                return None;
            }
            Some(parsed)
        },
        Tracker::Jira => (!bare_only)
            .then(|| {
                jira_token_ref(token, &tracker.project).map(|key| {
                    tracker_ref(tracker, tracker.project.clone(), key, Some(ItemKind::Issue))
                })
            })
            .flatten(),
    }
}
fn tracker_ref(
    tracker: &ResolvedTracker,
    project: String,
    item_key: String,
    item_kind: Option<ItemKind>,
) -> TrackerParsedRef {
    // `ref_kind` comes from the surrounding text; the caller stamps it.
    TrackerParsedRef {
        provider: tracker.provider,
        project,
        item_key,
        item_kind,
        ref_kind: String::new(),
    }
}
/// The URL prefix a binding's URL grammar matches. A self-hosted binding matches ONLY its own
/// host — a cloud URL in text names the cloud instance's project, never the self-hosted one.
fn url_base<'a>(tracker: &'a ResolvedTracker, cloud: &'a str) -> &'a str {
    tracker.base_url.as_deref().unwrap_or(cloud)
}
/// GitLab grammar: `<base>/<namespace...>/-/issues/N` + `/-/merge_requests/N` URLs,
/// cross-project `namespace/path#N` / `namespace/path!N` (multi-level subgroup paths), and the
/// bare `#N` / `!N` shorthands. GitLab numbers issues and merge requests in SEPARATE iid
/// namespaces, so every arm names the kind.
fn gitlab_token_ref(
    token: &str,
    tracker: &ResolvedTracker,
    is_code_host: bool,
    allow_bare: bool,
) -> Option<TrackerParsedRef> {
    let base = url_base(tracker, "https://gitlab.com");
    if let Some(rest) = token.strip_prefix(base).and_then(|rest| rest.strip_prefix('/')) {
        let (project, tail) = rest.split_once("/-/")?;
        let parts = tail.split('/').collect::<Vec<_>>();
        if parts.len() < 2 || project.is_empty() {
            return None;
        }
        let item_kind = match parts[0] {
            "issues" => ItemKind::Issue,
            "merge_requests" => ItemKind::ChangeRequest,
            _ => return None,
        };
        let number: i64 = ref_number(parts[1])?;
        return Some(tracker_ref(
            tracker,
            project.to_string(),
            number.to_string(),
            Some(item_kind),
        ));
    }
    for (separator, item_kind) in [('!', ItemKind::ChangeRequest), ('#', ItemKind::Issue)] {
        let Some((path, number)) = token.split_once(separator) else {
            continue;
        };
        let Some(number) = ref_number(number) else {
            continue;
        };
        if path.is_empty() {
            // Bare shorthand → the binding's own project. In the legacy single-pass form `#N`
            // needs the designated code-host owner while `!N` (GitLab-specific syntax) resolves
            // against the first GitLab binding; the two-pass source form disables BOTH here
            // (`allow_bare = false`) and routes them to the SOURCE binding instead — with
            // several GitLab bindings, `!5` must not belong to whichever is configured first.
            if !allow_bare || (separator == '#' && !is_code_host) {
                continue;
            }
            return Some(tracker_ref(
                tracker,
                tracker.project.clone(),
                number.to_string(),
                Some(item_kind),
            ));
        }
        let segments = path.split('/').collect::<Vec<_>>();
        if segments.len() >= 2 && segments.iter().all(|segment| !segment.is_empty()) {
            return Some(tracker_ref(
                tracker,
                path.to_string(),
                number.to_string(),
                Some(item_kind),
            ));
        }
    }
    None
}
/// Bitbucket grammar: Cloud `<base>/<workspace>/<repo>/{issues|pull-requests}/N`, Data Center
/// `<base>/projects/<project>/repos/<repo>/pull-requests/N`, and the bare `#N` shorthand.
fn bitbucket_token_ref(
    token: &str,
    tracker: &ResolvedTracker,
    is_code_host: bool,
) -> Option<TrackerParsedRef> {
    let base = url_base(tracker, "https://bitbucket.org");
    if let Some(rest) = token.strip_prefix(base).and_then(|rest| rest.strip_prefix('/')) {
        let parts = rest.split('/').collect::<Vec<_>>();
        if let ["projects", project, "repos", repo, "pull-requests", number, ..] = parts.as_slice()
        {
            let number: i64 = ref_number(number)?;
            return Some(tracker_ref(
                tracker,
                format!("{project}/{repo}"),
                number.to_string(),
                Some(ItemKind::ChangeRequest),
            ));
        }
        if parts.len() < 4 {
            return None;
        }
        let item_kind = match parts[2] {
            "issues" => ItemKind::Issue,
            "pull-requests" => ItemKind::ChangeRequest,
            _ => return None,
        };
        let number: i64 = ref_number(parts[3])?;
        return Some(tracker_ref(
            tracker,
            format!("{}/{}", parts[0], parts[1]),
            number.to_string(),
            Some(item_kind),
        ));
    }
    if is_code_host
        && let Some(number) = token.strip_prefix('#')
        && let Some(number) = ref_number(number)
    {
        return Some(tracker_ref(
            tracker,
            tracker.project.clone(),
            number.to_string(),
            Some(ItemKind::Issue),
        ));
    }
    None
}
/// Jira bare keys: `[A-Z][A-Z0-9]+-\d+`, WHOLE-token anchored (the tokenizer split is the word
/// boundary, so `XPROJ-12` never matches a `PROJ` binding) and ONLY for the bound project key —
/// an unscoped pattern would drown discovery in `UTF-8` / `ISO-8601` / `SHA-256`-shaped false
/// positives and collide with GitHub's `GH-N` shorthand.
fn jira_token_ref(token: &str, project: &str) -> Option<String> {
    if !is_jira_key_project(project) {
        return None;
    }
    let number = token.strip_prefix(project)?.strip_prefix('-')?;
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| token.to_string())
}
/// Whether the configured Jira project is key-shaped (`[A-Z][A-Z_0-9]+`, underscore included —
/// matching GitLab's `jira_issue_key_regex`). A non-key project can never engage the bare-key
/// grammar — its word-boundary guarantees don't hold.
fn is_jira_key_project(project: &str) -> bool {
    let bytes = project.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_uppercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
}
/// A ref number token: all digits, no leading zero, and a sane length — `#007`-shaped tokens
/// are version/junk fragments, not tracker keys (the reference parsers this is checked against
/// use `[1-9][0-9]*` with a length cap for the same reason).
pub(crate) fn ref_number(digits: &str) -> Option<i64> {
    if digits.is_empty() || digits.len() > 10 || digits.starts_with('0') {
        return None;
    }
    digits.parse().ok()
}

/// Provider-aware claim classifier: GitLab's documented closing-keyword family is wider than
/// GitHub's (the gerunds and the Implement family close issues on default-branch merges/commits),
/// so a ref CLAIMED by a GitLab binding classifies under GitLab's set; every other provider uses
/// the GitHub set (the shared floor).
pub(crate) fn ref_kind_for(provider: Tracker, previous: &str) -> String {
    let base = ref_kind(previous);
    if provider == Tracker::Gitlab && base == "unknown" {
        let previous = previous.to_ascii_lowercase();
        if [
            "closing",
            "fixing",
            "resolving",
            "implement",
            "implements",
            "implemented",
            "implementing",
        ]
        .contains(&previous.as_str())
        {
            return "closing".to_string();
        }
    }
    base
}

pub(crate) fn ref_kind(previous: &str) -> String {
    let previous = previous.to_ascii_lowercase();
    // The full documented GitHub closing-keyword set, singular forms included ("Fix #5").
    // DELIBERATELY the intersection semantics: GitLab additionally honors the gerunds
    // ("Closing"/"Fixing"/"Resolving") and the Implement family — but this shared classifier
    // feeds COMMIT-tier closer minting for both hosts, and GitHub honors none of those, so
    // widening here would mint false GitHub closers. Provider-specific keyword sets can ride
    // the provider lane when it needs them.
    if ["fix", "fixes", "fixed", "close", "closes", "closed", "resolve", "resolves", "resolved"]
        .contains(&previous.as_str())
    {
        "closing".to_string()
    } else if ["reverts", "reverted"].contains(&previous.as_str()) {
        // "Reverts owner/repo#N" — the body GitHub writes into every revert PR: a free PR↔PR
        // revert edge for the distill layer's outcome chains (issue #702).
        "reverts".to_string()
    } else if ["refs", "ref", "see", "related"].contains(&previous.as_str()) {
        "reference".to_string()
    } else {
        "unknown".to_string()
    }
}
pub(crate) fn classify_text(text: &str) -> String {
    let text = text.to_ascii_lowercase();
    if text.contains("decided") || text.contains("decision") || text.contains("we will") {
        "decision"
    } else if text.contains("rejected") || text.contains("alternative") || text.contains("instead")
    {
        "rejected_alternative"
    } else if text.contains("must") || text.contains("constraint") || text.contains("required") {
        "constraint"
    } else if text.contains("risk") || text.contains("concern") || text.contains("blocked") {
        "risk"
    } else if text.contains("obsolete") || text.contains("deprecated") || text.contains("no longer")
    {
        "obsolete"
    } else {
        "context"
    }
    .to_string()
}
pub(crate) fn item_from_issue_value(project: &str, value: &Value) -> PapertrailItem {
    // GitHub's issues endpoints return PR-shadow rows; the `pull_request` object marks the kind
    // and (on list payloads) carries `merged_at`.
    let shadow = value.get("pull_request");
    PapertrailItem {
        project: project.to_string(),
        item_kind: if shadow.is_some() { ItemKind::ChangeRequest } else { ItemKind::Issue },
        item_key: value["number"].as_u64().unwrap_or_default().to_string(),
        url: string_value(value, "html_url"),
        state: string_value(value, "state"),
        title: string_value(value, "title"),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
        merged_at: shadow.and_then(|shadow| shadow["merged_at"].as_str()).map(str::to_string),
        closed_at: value["closed_at"].as_str().map(str::to_string),
        resolution: resolution_from_state_reason(value["state_reason"].as_str()),
        // Stamped by `enrich_item_from_pull_value` ONLY when the pulls payload attests a merge —
        // the issues-endpoint shadow carries no trustworthy merge commit.
        merge_commit_sha: None,
        author_kind: value.pointer("/user/type").and_then(Value::as_str).map(str::to_string),
        author_association: value["author_association"].as_str().map(str::to_string),
        tags: value["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label["name"].as_str().map(str::to_string))
            .collect(),
    }
}
/// GitHub `state_reason` -> the provider-neutral resolution. `reopened` (and anything else
/// unrecognized) maps to `unknown` rather than dropping: the column distinguishes "provider
/// attested nothing" (`NULL`) from "attested something we don't classify" (`unknown`).
pub(crate) fn resolution_from_state_reason(state_reason: Option<&str>) -> Option<ItemResolution> {
    Some(match state_reason? {
        "completed" => ItemResolution::Completed,
        "not_planned" => ItemResolution::NotPlanned,
        "duplicate" => ItemResolution::Duplicate,
        _ => ItemResolution::Unknown,
    })
}

/// Fold the richer pulls-endpoint payload into a change request built from its issue shadow.
pub(crate) fn enrich_item_from_pull_value(item: &mut PapertrailItem, value: &Value) {
    if let Some(state) = value["state"].as_str() {
        item.state = state.to_string();
    }
    if let Some(merged_at) = value["merged_at"].as_str() {
        item.merged_at = Some(merged_at.to_string());
    }
    if let Some(closed_at) = value["closed_at"].as_str() {
        item.closed_at = Some(closed_at.to_string());
    }
    // THE VERIFIED TRAP: closed-UNMERGED PRs carry a non-null `merge_commit_sha` (GitHub's
    // ephemeral test-merge commit, possibly on no branch). Gate on the merge attestation —
    // `merged_at` — never on the field's presence.
    if item.merged_at.is_some() {
        item.merge_commit_sha = value["merge_commit_sha"].as_str().map(str::to_string);
    }
}
pub(crate) fn comment_from_value(
    project: &str,
    kind: ItemKind,
    key: &str,
    value: &Value,
) -> PapertrailComment {
    PapertrailComment {
        project: project.to_string(),
        item_kind: kind,
        item_key: key.to_string(),
        comment_id: format!("comment:{}", value["id"].as_u64().unwrap_or_default()),
        url: value["html_url"].as_str().map(str::to_string),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
        author_kind: value.pointer("/user/type").and_then(Value::as_str).map(str::to_string),
        author_association: value["author_association"].as_str().map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
        review_state: None,
        anchor_path: None,
    }
}
pub(crate) fn review_to_comment_from_value(
    project: &str,
    kind: ItemKind,
    key: &str,
    value: &Value,
) -> PapertrailComment {
    let mut comment = PapertrailComment {
        created_at: value["submitted_at"].as_str().map(str::to_string),
        updated_at: value["submitted_at"].as_str().map(str::to_string),
        review_state: Some(string_value(value, "state")),
        ..comment_from_value(project, kind, key, value)
    };
    comment.comment_id = format!("review:{}", value["id"].as_u64().unwrap_or_default());
    comment
}
pub(crate) fn review_comment_to_comment_from_value(
    project: &str,
    kind: ItemKind,
    key: &str,
    value: &Value,
) -> PapertrailComment {
    let mut comment = PapertrailComment {
        anchor_path: value["path"].as_str().map(str::to_string),
        ..comment_from_value(project, kind, key, value)
    };
    comment.comment_id = format!("review_comment:{}", value["id"].as_u64().unwrap_or_default());
    comment
}
/// Map one entry of a repo-wide comment stream, deriving the parent item key from the payload's
/// `issue_url` / `pull_request_url` tail. `None` when the payload names no parent. The unanchored
/// stream (`issues/comments`) covers BOTH kinds under GitHub's shared numbering and its payload
/// does not say which — that kind is provisional (`Issue`); the mirror sync resolves the real one
/// against the mirrored items.
pub(crate) fn repo_comment_from_value(
    project: &str,
    value: &Value,
    anchored: bool,
) -> Option<PapertrailComment> {
    let (parent_field, kind) = if anchored {
        ("pull_request_url", ItemKind::ChangeRequest)
    } else {
        ("issue_url", ItemKind::Issue)
    };
    let key = value[parent_field].as_str()?.rsplit('/').next()?.to_string();
    let mut comment = comment_from_value(project, kind, &key, value);
    if anchored {
        comment.comment_id = format!("review_comment:{}", value["id"].as_i64().unwrap_or_default());
        comment.anchor_path = value["path"].as_str().map(str::to_string);
    }
    Some(comment)
}
pub(crate) fn string_value(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}
pub(crate) fn split_repo(value: &str) -> Option<(&str, &str)> {
    value.split_once('/')
}
pub(crate) fn snippet(text: &str) -> String {
    text.lines().take(3).collect::<Vec<_>>().join("\n")
}
pub(crate) fn fts_query(query: &str) -> String {
    let terms = query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() { "\"\"".to_string() } else { terms.join(" OR ") }
}
pub(crate) fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod grammar_tests {
    use super::*;

    fn tracker(provider: Tracker, project: &str) -> ResolvedTracker {
        ResolvedTracker {
            provider,
            project: project.to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        }
    }

    fn self_hosted(provider: Tracker, project: &str, base_url: &str) -> ResolvedTracker {
        ResolvedTracker { base_url: Some(base_url.to_string()), ..tracker(provider, project) }
    }

    /// (project, item_key, item_kind) of every ref parsed from `text` under the bindings.
    fn table(text: &str, trackers: &[ResolvedTracker]) -> Vec<(String, String, Option<ItemKind>)> {
        parse_tracker_refs(text, trackers)
            .into_iter()
            .map(|r| (r.project, r.item_key, r.item_kind))
            .collect()
    }

    fn one(text: &str, trackers: &[ResolvedTracker]) -> (String, String, Option<ItemKind>) {
        let mut refs = table(text, trackers);
        assert_eq!(refs.len(), 1, "{text}: expected exactly one ref, got {refs:?}");
        refs.remove(0)
    }

    #[test]
    fn github_grammar_covers_shorthands_and_urls() {
        let gh = [tracker(Tracker::Github, "o/r")];
        // Shorthand kinds stay None — GitHub's shared numbering means the provider resolves.
        assert_eq!(one("fixes #12", &gh), ("o/r".into(), "12".into(), None));
        assert_eq!(one("see GH-7", &gh), ("o/r".into(), "7".into(), None));
        assert_eq!(one("ref other/repo#3", &gh), ("other/repo".into(), "3".into(), None));
        // `/issues/` is ambiguous because GitHub also serves PRs there; `/pull/` is not.
        assert_eq!(one("https://github.com/a/b/issues/42", &gh), ("a/b".into(), "42".into(), None));
        assert_eq!(
            one("https://github.com/a/b/pull/7", &gh),
            ("a/b".into(), "7".into(), Some(ItemKind::ChangeRequest))
        );
        // Non-refs stay quiet.
        assert_eq!(table("#notanumber a/b/c#4 https://github.com/a/b", &gh), Vec::new());
        // The ref-kind words still classify the surrounding text.
        let refs = parse_tracker_refs("fixes #1 and see #2, #3", &gh);
        assert_eq!(refs.iter().map(|r| r.ref_kind.as_str()).collect::<Vec<_>>(), [
            "closing",
            "reference",
            "unknown"
        ]);
    }

    #[test]
    fn gitlab_grammar_names_kinds_and_keeps_subgroup_paths() {
        let gl = [tracker(Tracker::Gitlab, "group/sub/repo")];
        // GitLab iid namespaces are separate, so every arm names the kind.
        assert_eq!(
            one("fixes !7", &gl),
            ("group/sub/repo".into(), "7".into(), Some(ItemKind::ChangeRequest))
        );
        assert_eq!(
            one("closes #9", &gl),
            ("group/sub/repo".into(), "9".into(), Some(ItemKind::Issue))
        );
        // Cross-project shorthands keep multi-level namespace paths.
        assert_eq!(
            one("other/team/app!4", &gl),
            ("other/team/app".into(), "4".into(), Some(ItemKind::ChangeRequest))
        );
        assert_eq!(
            one("other/team/app#5", &gl),
            ("other/team/app".into(), "5".into(), Some(ItemKind::Issue))
        );
        // URLs, subgroups included.
        assert_eq!(
            one("https://gitlab.com/g/s/r/-/issues/11", &gl),
            ("g/s/r".into(), "11".into(), Some(ItemKind::Issue))
        );
        assert_eq!(
            one("https://gitlab.com/g/s/r/-/merge_requests/3", &gl),
            ("g/s/r".into(), "3".into(), Some(ItemKind::ChangeRequest))
        );
        assert_eq!(table("https://gitlab.com/g/r/-/pipelines/3 solo#2", &gl), Vec::new());
    }

    #[test]
    fn self_hosted_bindings_match_only_their_own_host() {
        let gl = [self_hosted(Tracker::Gitlab, "group/repo", "https://gitlab.example.com")];
        assert_eq!(
            one("https://gitlab.example.com/g/s/r/-/issues/6", &gl),
            ("g/s/r".into(), "6".into(), Some(ItemKind::Issue))
        );
        // A cloud URL names the CLOUD instance's project — never the self-hosted binding's.
        assert_eq!(table("https://gitlab.com/g/s/r/-/issues/6", &gl), Vec::new());
    }

    #[test]
    fn bitbucket_grammar_covers_urls_and_bare_issue_refs() {
        let bb = [tracker(Tracker::Bitbucket, "ws/repo")];
        assert_eq!(
            one("https://bitbucket.org/ws/repo/pull-requests/5", &bb),
            ("ws/repo".into(), "5".into(), Some(ItemKind::ChangeRequest))
        );
        assert_eq!(
            one("https://bitbucket.org/ws/repo/issues/8", &bb),
            ("ws/repo".into(), "8".into(), Some(ItemKind::Issue))
        );
        // Bare `#N` is issue-shaped on Bitbucket (independent PR numbering).
        assert_eq!(one("fixes #4", &bb), ("ws/repo".into(), "4".into(), Some(ItemKind::Issue)));
        assert_eq!(table("https://bitbucket.org/ws/repo/branches/x", &bb), Vec::new());

        let dc = [self_hosted(Tracker::Bitbucket, "PROJ/repo", "https://bitbucket.example.com")];
        assert_eq!(
            one("https://bitbucket.example.com/projects/PROJ/repos/repo/pull-requests/11", &dc,),
            ("PROJ/repo".into(), "11".into(), Some(ItemKind::ChangeRequest)),
        );
    }

    #[test]
    fn jira_grammar_matches_only_the_bound_project_key_whole_token() {
        let jira = [tracker(Tracker::Jira, "PROJ")];
        assert_eq!(
            one("fixes PROJ-123", &jira),
            ("PROJ".into(), "PROJ-123".into(), Some(ItemKind::Issue))
        );
        assert_eq!(one("(PROJ-9)", &jira), ("PROJ".into(), "PROJ-9".into(), Some(ItemKind::Issue)));
        // Word-boundary anchored (whole token), digits required, bound project only, and the
        // uppercase key-shaped lookalikes stay quiet.
        assert_eq!(
            table("XPROJ-12 proj-12 PROJ- PROJ-1a OTHER-5 UTF-8 ISO-8601 SHA-256", &jira),
            Vec::new()
        );
        // A bare `#N` never resolves against Jira — it is not a code host.
        assert_eq!(table("fixes #12", &jira), Vec::new());
        // A non-key-shaped configured project cannot engage the bare-key grammar.
        assert_eq!(table("proj-12", &[tracker(Tracker::Jira, "proj")]), Vec::new());
    }

    #[test]
    fn bare_refs_resolve_against_the_first_code_host_binding_only() {
        let jira_then_gitlab =
            [tracker(Tracker::Jira, "PROJ"), tracker(Tracker::Gitlab, "group/repo")];
        // Jira is skipped for code-host resolution even when listed first.
        assert_eq!(
            one("fixes #3", &jira_then_gitlab),
            ("group/repo".into(), "3".into(), Some(ItemKind::Issue))
        );

        let github_then_gitlab =
            [tracker(Tracker::Github, "o/r"), tracker(Tracker::Gitlab, "group/repo")];
        // `#N` goes to the FIRST code host; `!N` is GitLab-specific syntax and still resolves.
        assert_eq!(one("#5", &github_then_gitlab), ("o/r".into(), "5".into(), None));
        assert_eq!(
            one("!5", &github_then_gitlab),
            ("group/repo".into(), "5".into(), Some(ItemKind::ChangeRequest))
        );
        // Overlapping grammars are deterministic: the first binding claims `a/b#1` once.
        assert_eq!(one("a/b#1", &github_then_gitlab), ("a/b".into(), "1".into(), None));
    }

    #[test]
    fn one_repo_can_bind_a_code_host_and_jira_together() {
        let both = [tracker(Tracker::Github, "o/r"), tracker(Tracker::Jira, "PROJ")];
        assert_eq!(table("fixes #2 and PROJ-7", &both), vec![
            ("o/r".to_string(), "2".to_string(), None),
            ("PROJ".to_string(), "PROJ-7".to_string(), Some(ItemKind::Issue)),
        ]);
        // `GH-1` is claimed by the GitHub binding before a Jira `GH` project could see it.
        let gh_project = [tracker(Tracker::Github, "o/r"), tracker(Tracker::Jira, "GH")];
        assert_eq!(one("GH-1", &gh_project), ("o/r".into(), "1".into(), None));
    }

    #[test]
    fn boundary_punctuation_matches_the_reference_parser_canon() {
        // The cases Gitea's boundary classes accept: parens, brackets, quotes, and sentence
        // punctuation around the ref, colon after the keyword ("Fixes: #5", kernel style).
        let github = [tracker(Tracker::Github, "o/r")];
        for text in [
            "fixes (#5)",
            "fixes [#5]",
            "fixes \"#5\"",
            "fixes '#5'",
            "fixes #5!",
            "fixes #5?",
            "Fixes: #5",
        ] {
            let refs = parse_tracker_refs(text, &github);
            assert_eq!(refs.len(), 1, "`{text}` parses");
            assert_eq!(refs[0].item_key, "5", "`{text}`");
            assert_eq!(refs[0].ref_kind, "closing", "`{text}` keeps the closing claim");
        }
        // A LEADING `!` stays ref syntax (GitLab MR shorthand), only trailing is trimmed.
        let gitlab = [tracker(Tracker::Gitlab, "g/r")];
        let refs = parse_tracker_refs("see !5!", &gitlab);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].item_kind, Some(ItemKind::ChangeRequest));
    }

    #[test]
    fn junk_shaped_numbers_do_not_parse() {
        let github = [tracker(Tracker::Github, "o/r")];
        for text in ["fixes #007", "fixes #0", "see #12345678901"] {
            assert!(parse_tracker_refs(text, &github).is_empty(), "`{text}` is junk-shaped");
        }
    }

    #[test]
    fn jira_project_keys_may_carry_underscores() {
        let jira = [tracker(Tracker::Jira, "MY_PROJ")];
        let refs = parse_tracker_refs("relates to MY_PROJ-42", &jira);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].item_key, "MY_PROJ-42");
    }

    #[test]
    fn gh_dash_shorthand_is_source_local_not_config_order() {
        // `GH-5` is a local shorthand like bare `#N`: with two GitHub bindings, mining a source
        // in the SECOND project must not let the first claim it (two-pass source routing).
        let first = tracker(Tracker::Github, "o/first");
        let second = tracker(Tracker::Github, "o/second");
        let refs = parse_tracker_refs_with_source("fixes GH-5", &[first, second.clone()], &second);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].project, "o/second", "GH-N belongs to the source binding");
    }

    #[test]
    fn source_local_gh_dash_beats_a_jira_gh_project() {
        // A Jira project literally named `GH` is configured; a GitHub source's `Fixes GH-5` is
        // the SOURCE's local shorthand and must not be claimed as Jira `GH-5`.
        let github = tracker(Tracker::Github, "o/r");
        let jira = tracker(Tracker::Jira, "GH");
        let refs =
            parse_tracker_refs_with_source("Fixes GH-5", &[jira.clone(), github.clone()], &github);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].provider, Tracker::Github);
        assert_eq!(refs[0].project, "o/r");
        // From a JIRA-side source (or plain discovery), the Jira grammar still claims it.
        let refs = parse_tracker_refs("Fixes GH-5", &[jira, github]);
        assert_eq!(refs[0].provider, Tracker::Jira);
    }

    #[test]
    fn gitlab_claims_classify_under_gitlab_keyword_family() {
        // GitLab's documented closing pattern honors the gerunds and the Implement family; a
        // ref CLAIMED by a GitLab binding classifies under that wider set, while the same word
        // before a GitHub-claimed ref stays unknown (GitHub honors only the base nine).
        let gitlab = [tracker(Tracker::Gitlab, "g/r")];
        for text in ["Closing #5", "Fixing #5", "Implements #5", "resolving #5"] {
            let refs = parse_tracker_refs(text, &gitlab);
            assert_eq!(refs[0].ref_kind, "closing", "`{text}` closes on GitLab");
        }
        let github = [tracker(Tracker::Github, "o/r")];
        for text in ["Closing #5", "Implements #5"] {
            let refs = parse_tracker_refs(text, &github);
            assert_eq!(refs[0].ref_kind, "unknown", "`{text}` does NOT close on GitHub");
        }
    }

    #[test]
    fn legacy_github_lane_stays_projection_equal_to_the_tracker_grammar() {
        // `parse_refs` (the github_* sync lane) and `parse_tracker_refs` under a GitHub binding
        // share `github_token_ref`; pin the projection so the lanes cannot drift apart.
        let text = "fixes #1, see other/repo#2 and https://github.com/a/b/pull/3 GH-4";
        let legacy = parse_refs(text, Some("o/r"));
        let keyed = parse_tracker_refs(text, &[tracker(Tracker::Github, "o/r")]);
        assert_eq!(legacy.len(), 4);
        assert_eq!(legacy.len(), keyed.len());
        for (old, new) in legacy.iter().zip(&keyed) {
            assert_eq!(old.project, new.project);
            assert_eq!(old.number.to_string(), new.item_key);
            assert_eq!(old.kind, new.ref_kind);
        }
    }
}

#[cfg(test)]
mod mapper_tests {
    use serde_json::json;

    use super::*;

    // The gh-payload mappers are the provider boundary: the sync tests drive mock clients that
    // never touch them, so they are pinned here against realistic REST payload shapes.

    #[test]
    fn item_from_issue_value_maps_a_plain_issue() {
        let item = item_from_issue_value(
            "o/r",
            &json!({
                "number": 42,
                "html_url": "https://github.com/o/r/issues/42",
                "state": "open",
                "title": "t",
                "body": "b",
                "user": {"login": "octo"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-02T00:00:00Z",
            }),
        );
        assert_eq!(item.item_kind, ItemKind::Issue);
        assert_eq!(item.project, "o/r");
        assert_eq!(item.item_key, "42");
        assert_eq!(item.author.as_deref(), Some("octo"));
        assert_eq!(item.merged_at, None);
    }

    #[test]
    fn item_from_issue_value_resolves_a_pr_shadow_with_its_merge_stamp() {
        let item = item_from_issue_value(
            "o/r",
            &json!({
                "number": 7,
                "html_url": "https://github.com/o/r/pull/7",
                "state": "closed",
                "title": "t",
                "body": "b",
                "pull_request": {"merged_at": "2026-02-01T00:00:00Z"},
            }),
        );
        assert_eq!(item.item_kind, ItemKind::ChangeRequest);
        assert_eq!(item.merged_at.as_deref(), Some("2026-02-01T00:00:00Z"));
    }

    #[test]
    fn enrich_item_from_pull_value_folds_state_and_merge_stamp() {
        let mut item = item_from_issue_value(
            "o/r",
            &json!({
                "number": 7,
                "state": "closed",
                "title": "t",
                "body": "b",
                "pull_request": {},
            }),
        );
        enrich_item_from_pull_value(
            &mut item,
            &json!({
                "state": "open",
                "merged_at": "2026-02-01T00:00:00Z",
            }),
        );
        assert_eq!(item.state, "open");
        assert_eq!(item.merged_at.as_deref(), Some("2026-02-01T00:00:00Z"));
    }

    #[test]
    fn comment_mappers_stamp_the_unifying_markers() {
        let payload = json!({
            "id": 9,
            "html_url": "https://c",
            "body": "b",
            "user": {"login": "octo"},
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        });
        let plain = comment_from_value("o/r", ItemKind::Issue, "42", &payload);
        assert_eq!(plain.comment_id, "comment:9");
        assert_eq!(plain.item_kind, ItemKind::Issue);
        assert_eq!((plain.review_state, plain.anchor_path), (None, None));

        let review = review_to_comment_from_value(
            "o/r",
            ItemKind::ChangeRequest,
            "7",
            &json!({
                "id": 9,
                "state": "APPROVED",
                "body": "b",
                "submitted_at": "2026-01-03T00:00:00Z",
            }),
        );
        assert_eq!(review.review_state.as_deref(), Some("APPROVED"));
        assert_eq!(review.comment_id, "review:9");
        assert_eq!(review.created_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        assert_eq!(review.updated_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        assert_eq!(review.anchor_path, None);

        let anchored = review_comment_to_comment_from_value(
            "o/r",
            ItemKind::ChangeRequest,
            "7",
            &json!({
                "id": 9,
                "path": "src/lib.rs",
                "html_url": "https://rc",
                "body": "b",
            }),
        );
        assert_eq!(anchored.anchor_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(anchored.comment_id, "review_comment:9");
        assert_eq!(anchored.review_state, None);
    }

    #[test]
    fn repo_comment_from_value_derives_the_parent_key_from_the_stream_payload() {
        let unanchored = repo_comment_from_value(
            "o/r",
            &json!({
                "id": 12,
                "issue_url": "https://api.github.com/repos/o/r/issues/42",
                "body": "b",
            }),
            false,
        )
        .unwrap();
        assert_eq!(unanchored.item_key, "42");
        assert_eq!(unanchored.item_kind, ItemKind::Issue);
        assert_eq!(unanchored.anchor_path, None);

        let anchored = repo_comment_from_value(
            "o/r",
            &json!({
                "id": 13,
                "pull_request_url": "https://api.github.com/repos/o/r/pulls/7",
                "path": "src/lib.rs",
                "body": "b",
            }),
            true,
        )
        .unwrap();
        assert_eq!(anchored.item_key, "7");
        assert_eq!(anchored.item_kind, ItemKind::ChangeRequest);
        assert_eq!(anchored.anchor_path.as_deref(), Some("src/lib.rs"));

        // A payload without its parent URL cannot be attributed — dropped, not misfiled.
        assert!(repo_comment_from_value("o/r", &json!({"id": 14, "body": "b"}), false).is_none());
    }
    #[test]
    fn item_from_issue_value_maps_the_outcome_and_author_facets() {
        let value: Value = serde_json::from_str(
            r#"{"number": 5, "html_url": "u", "state": "closed", "state_reason": "not_planned",
                "title": "t", "body": "b", "closed_at": "2026-01-04T00:00:00Z",
                "author_association": "OWNER",
                "user": {"login": "bot[bot]", "type": "Bot"}, "labels": []}"#,
        )
        .unwrap();
        let item = item_from_issue_value("o/r", &value);
        assert_eq!(item.closed_at.as_deref(), Some("2026-01-04T00:00:00Z"));
        assert_eq!(item.resolution, Some(ItemResolution::NotPlanned));
        assert_eq!(item.author_kind.as_deref(), Some("Bot"));
        assert_eq!(item.author_association.as_deref(), Some("OWNER"));
        assert_eq!(item.merge_commit_sha, None, "the issues shadow never carries a merge commit");
    }

    #[test]
    fn resolution_distinguishes_unattested_from_unclassified() {
        assert_eq!(resolution_from_state_reason(None), None, "provider attested nothing");
        assert_eq!(
            resolution_from_state_reason(Some("reopened")),
            Some(ItemResolution::Unknown),
            "attested-but-unclassified maps to unknown, not NULL",
        );
        assert_eq!(
            resolution_from_state_reason(Some("duplicate")),
            Some(ItemResolution::Duplicate)
        );
    }

    #[test]
    fn enrich_from_pull_gates_merge_commit_on_merged_at_not_field_presence() {
        // THE VERIFIED TRAP: GitHub returns a non-null merge_commit_sha for closed-UNMERGED PRs
        // (its ephemeral test-merge commit). The enrich must gate on the merge attestation.
        let closed_unmerged: Value = serde_json::from_str(
            r#"{"state": "closed", "merged_at": null, "closed_at": "2026-01-04T00:00:00Z",
                "merge_commit_sha": "ephemeral-test-merge"}"#,
        )
        .unwrap();
        let mut item = item_from_issue_value(
            "o/r",
            &serde_json::from_str(
                r#"{"pull_request": {}, "number": 9, "html_url": "u", "state": "open",
                    "title": "t", "body": "b", "labels": []}"#,
            )
            .unwrap(),
        );
        enrich_item_from_pull_value(&mut item, &closed_unmerged);
        assert_eq!(item.merge_commit_sha, None, "closed-unmerged never records the test merge");
        assert_eq!(item.closed_at.as_deref(), Some("2026-01-04T00:00:00Z"));

        let merged: Value = serde_json::from_str(
            r#"{"state": "closed", "merged_at": "2026-01-05T00:00:00Z",
                "merge_commit_sha": "abc123"}"#,
        )
        .unwrap();
        enrich_item_from_pull_value(&mut item, &merged);
        assert_eq!(item.merge_commit_sha.as_deref(), Some("abc123"));
    }

    #[test]
    fn singular_closing_keywords_classify_as_closing() {
        for word in ["fix", "close", "resolve", "Fixes", "resolved"] {
            assert_eq!(ref_kind(word), "closing", "`{word}` is a documented closing keyword");
        }
        assert_eq!(ref_kind("prefix"), "unknown", "substrings of keywords do not match");
    }

    #[test]
    fn reverts_is_a_first_class_ref_kind() {
        let binding = ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        };
        let refs = parse_tracker_refs("Reverts o/r#41", &[binding]);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_kind, "reverts");
        assert_eq!(refs[0].item_key, "41");
    }
}
