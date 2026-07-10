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
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                number: parts[3].parse().ok()?,
                kind: "url".to_string(),
            });
        }
    }
    if let Some((repo_ref, number)) = token.split_once('#') {
        let parts = repo_ref.split('/').collect::<Vec<_>>();
        if parts.len() == 2 {
            return Some(ParsedRef {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                number: number.parse().ok()?,
                kind: "cross_repo".to_string(),
            });
        }
    }
    if let Some(number) = token.strip_prefix("GH-") {
        let (owner, repo) = split_repo(default_repo?)?;
        return Some(ParsedRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: number.parse().ok()?,
            kind: "gh_dash".to_string(),
        });
    }
    if let Some(number) = token.strip_prefix('#') {
        let (owner, repo) = split_repo(default_repo?)?;
        return Some(ParsedRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
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
