use super::*;

pub(crate) fn parse_refs(text: &str, default_repo: Option<&str>) -> Vec<ParsedRef> {
    let mut refs = Vec::new();
    let tokens = text
        .split(|c: char| c.is_whitespace() || [',', ';', ')', ']', '}'].contains(&c))
        .map(|token| token.trim_matches(|c: char| ['(', '[', '{', '.', ':'].contains(&c)))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let mut previous = "";
    for token in tokens {
        let kind = ref_kind(previous);
        if let Some(parsed) = parse_issue_ref(token, default_repo) {
            refs.push(ParsedRef { kind, ..parsed });
        }
        previous = token;
    }
    refs
}
pub(crate) fn parse_issue_ref(token: &str, default_repo: Option<&str>) -> Option<ParsedRef> {
    if let Some(rest) = token.strip_prefix("https://github.com/") {
        let parts = rest.split('/').collect::<Vec<_>>();
        if parts.len() >= 4 && (parts[2] == "issues" || parts[2] == "pull") {
            return Some(ParsedRef {
                project: format!("{}/{}", parts[0], parts[1]),
                number: parts[3].parse().ok()?,
                kind: "url".to_string(),
            });
        }
    }
    if let Some((repo_ref, number)) = token.split_once('#') {
        let parts = repo_ref.split('/').collect::<Vec<_>>();
        if parts.len() == 2 {
            return Some(ParsedRef {
                project: repo_ref.to_string(),
                number: number.parse().ok()?,
                kind: "cross_repo".to_string(),
            });
        }
    }
    if let Some(number) = token.strip_prefix("GH-") {
        // The default project must be an `owner/repo` path — reject anything else rather than
        // storing a malformed project key.
        split_repo(default_repo?)?;
        return Some(ParsedRef {
            project: default_repo?.to_string(),
            number: number.parse().ok()?,
            kind: "gh_dash".to_string(),
        });
    }
    if let Some(number) = token.strip_prefix('#') {
        split_repo(default_repo?)?;
        return Some(ParsedRef {
            project: default_repo?.to_string(),
            number: number.parse().ok()?,
            kind: "local_number".to_string(),
        });
    }
    None
}
pub(crate) fn ref_kind(previous: &str) -> String {
    let previous = previous.to_ascii_lowercase();
    if ["fixes", "fixed", "closes", "closed", "resolves", "resolved"].contains(&previous.as_str()) {
        "closing".to_string()
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
        item_key: value["number"].as_i64().unwrap_or_default().to_string(),
        url: string_value(value, "html_url"),
        state: string_value(value, "state"),
        title: string_value(value, "title"),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
        created_at: value["created_at"].as_str().map(str::to_string),
        updated_at: value["updated_at"].as_str().map(str::to_string),
        merged_at: shadow.and_then(|shadow| shadow["merged_at"].as_str()).map(str::to_string),
    }
}
/// Fold the richer pulls-endpoint payload into a change request built from its issue shadow.
pub(crate) fn enrich_item_from_pull_value(item: &mut PapertrailItem, value: &Value) {
    if let Some(state) = value["state"].as_str() {
        item.state = state.to_string();
    }
    if let Some(merged_at) = value["merged_at"].as_str() {
        item.merged_at = Some(merged_at.to_string());
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
        comment_id: value["id"].as_i64().unwrap_or_default().to_string(),
        url: value["html_url"].as_str().map(str::to_string),
        body: string_value(value, "body"),
        author: value.pointer("/user/login").and_then(Value::as_str).map(str::to_string),
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
    PapertrailComment {
        created_at: value["submitted_at"].as_str().map(str::to_string),
        updated_at: value["submitted_at"].as_str().map(str::to_string),
        review_state: Some(string_value(value, "state")),
        ..comment_from_value(project, kind, key, value)
    }
}
pub(crate) fn review_comment_to_comment_from_value(
    project: &str,
    kind: ItemKind,
    key: &str,
    value: &Value,
) -> PapertrailComment {
    PapertrailComment {
        anchor_path: value["path"].as_str().map(str::to_string),
        ..comment_from_value(project, kind, key, value)
    }
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
        comment.anchor_path = value["path"].as_str().map(str::to_string);
    }
    Some(comment)
}
pub(crate) fn gh_api_json(path: &str) -> anyhow::Result<Value> {
    let output = Command::new("gh").args(["api", path]).output()?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}
pub(crate) fn gh_api_paginated(path: &str) -> anyhow::Result<Vec<Value>> {
    let output = Command::new("gh").args(["api", "--paginate", "--slurp", path]).output()?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let mut out = Vec::new();
    if let Some(pages) = value.as_array() {
        for page in pages {
            if let Some(items) = page.as_array() {
                out.extend(items.iter().cloned());
            }
        }
    }
    Ok(out)
}
pub(crate) fn default_repo() -> Option<String> {
    let output = Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}
pub(crate) fn gh_available() -> bool {
    Command::new("gh").arg("--version").output().is_ok_and(|output| output.status.success())
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
        assert_eq!(plain.comment_id, "9");
        assert_eq!(plain.item_kind, ItemKind::Issue);
        assert_eq!((plain.review_state, plain.anchor_path), (None, None));

        let review = review_to_comment_from_value(
            "o/r",
            ItemKind::ChangeRequest,
            "7",
            &json!({
                "id": 10,
                "state": "APPROVED",
                "body": "b",
                "submitted_at": "2026-01-03T00:00:00Z",
            }),
        );
        assert_eq!(review.review_state.as_deref(), Some("APPROVED"));
        assert_eq!(review.created_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        assert_eq!(review.updated_at.as_deref(), Some("2026-01-03T00:00:00Z"));
        assert_eq!(review.anchor_path, None);

        let anchored = review_comment_to_comment_from_value(
            "o/r",
            ItemKind::ChangeRequest,
            "7",
            &json!({
                "id": 11,
                "path": "src/lib.rs",
                "html_url": "https://rc",
                "body": "b",
            }),
        );
        assert_eq!(anchored.anchor_path.as_deref(), Some("src/lib.rs"));
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
}
