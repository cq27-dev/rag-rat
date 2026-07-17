use super::*;

pub(crate) fn discover_and_store_refs(
    conn: &Connection,
    root: &Path,
    ctx: &PapertrailContext,
) -> anyhow::Result<Vec<PapertrailRef>> {
    let mut refs = Vec::new();
    discover_commit_refs(conn, &ctx.trackers, &mut refs)?;
    discover_file_refs(conn, root, &ctx.trackers, &mut refs)?;
    let branch = rag_rat_base::repo_discover::discover_repo(root)
        .ok()
        .and_then(|repo| repo.head_name().ok().flatten())
        .map(|name| name.shorten().to_string())
        .unwrap_or_default();
    for parsed in parse_tracker_refs(&branch, &ctx.trackers) {
        refs.push(parsed.into_ref("branch", None, None, branch.clone()));
    }
    if ctx.trackers.is_empty() {
        for parsed in parse_refs(&branch, None) {
            refs.push(parsed.into_ref("branch", None, None, branch.clone()));
        }
    }
    let mut unique = BTreeSet::new();
    // Collapse duplicates keeping the STRONGEST claim per identity: one commit body saying
    // "Refs #5. Fixes #5" must dedupe to the CLOSING ref, or the closer is silently dropped
    // before edge derivation (the unique index does not fold ref_kind).
    refs.sort_by_key(|r| ref_kind_rank(&r.ref_kind));
    refs.retain(|r| {
        unique.insert((
            r.tracker.as_db_str(),
            r.project.clone(),
            r.item_key.clone(),
            r.item_kind.map(ItemKind::as_db_str),
            r.source_kind.clone(),
            r.source_path.clone(),
            r.source_commit.clone(),
            r.source_text.clone(),
        ))
    });
    for reference in &refs {
        store_ref(conn, reference)?;
    }
    // Commit-tier closer derivation deliberately does NOT run here: it runs once at the END of
    // each sync entry (`rederive_commit_closers`), after targets are cached — the target-kind
    // verification needs the mirror rows, and a single post-sync pass covers both ordering and
    // idempotence.
    Ok(refs)
}
pub async fn sync_refs<'a, C: PapertrailClient>(
    conn: &Connection,
    client: &C,
    trackers: &[ResolvedTracker],
    refs: impl Iterator<Item = &'a PapertrailRef>,
    progress: &mut impl FnMut(PapertrailSyncProgress),
) -> anyhow::Result<SyncRefsReport> {
    let refs = refs.collect::<Vec<_>>();
    let identity = |reference: &PapertrailRef| {
        (
            reference.tracker.as_db_str(),
            reference.project.clone(),
            reference.item_kind.map(ItemKind::as_db_str),
            reference.item_key.clone(),
        )
    };
    let total = refs.iter().map(|reference| identity(reference)).collect::<BTreeSet<_>>().len();
    let mut report = SyncRefsReport::default();
    let mut seen = BTreeSet::new();
    for reference in refs {
        if !seen.insert(identity(reference)) {
            continue;
        }
        let current = seen.len();
        if papertrail_ref_synced(conn, reference)? {
            report.skipped_refs += 1;
            progress(sync_progress(reference, current, total, PapertrailSyncAction::Skipped, None));
            continue;
        }
        progress(sync_progress(reference, current, total, PapertrailSyncAction::Syncing, None));
        match sync_one_ref(conn, client, trackers, reference).await {
            Ok(items) => {
                report.synced_items += items;
                progress(sync_progress(
                    reference,
                    current,
                    total,
                    PapertrailSyncAction::Synced,
                    None,
                ));
            },
            Err(err) => {
                let message = err.to_string();
                let status = if is_not_found_error(&message) { "not_found" } else { "failed" };
                report.failed_refs += 1;
                report.errors.push(PapertrailSyncError {
                    tracker: reference.tracker,
                    project: reference.project.clone(),
                    item_key: reference.item_key.clone(),
                    status: status.to_string(),
                    error: message.clone(),
                });
                progress(sync_progress(
                    reference,
                    current,
                    total,
                    PapertrailSyncAction::Failed,
                    Some(message),
                ));
            },
        }
    }
    Ok(report)
}
/// Sync ONE referenced item: fetch the item AND its comments, then store both. Fetch-then-store
/// ordering is LOAD-BEARING: the per-ref sync state machine (`github_ref_sync`) is gone, so
/// [`papertrail_ref_synced`]'s only skip signal is "the item is cached" — a partial store (item
/// row landed, comment fetch failed) would masquerade as a completed sync forever. Storing nothing
/// until every fetch succeeded keeps a failed ref retryable with no state row. The FTS mirror
/// follows incrementally inside the store writers.
pub(crate) async fn sync_one_ref<C: PapertrailClient>(
    conn: &Connection,
    client: &C,
    trackers: &[ResolvedTracker],
    reference: &PapertrailRef,
) -> anyhow::Result<usize> {
    // Discovered refs don't carry a kind (a bare `#N` could be either); ask as an issue and let
    // the provider resolve, then fetch comments under the RESOLVED kind.
    let item = client.item(&reference.project, ItemKind::Issue, &reference.item_key).await?;
    let comments =
        client.item_comments(&reference.project, item.item_kind, &reference.item_key).await?;
    // The cached item is the ref lane's completion marker. Commit it atomically with every
    // comment/FTS row so a failed comment write cannot leave an item that suppresses retries.
    let tx = conn.unchecked_transaction()?;
    store_item(&tx, reference.tracker, &item)?;
    // The referenced-sync lane honors the same store-time mining contract as the mirror.
    mine_item_refs(&tx, reference.tracker, trackers, &item)?;
    let mut synced = 1;
    for comment in &comments {
        store_comment(&tx, reference.tracker, comment)?;
        mine_comment_refs(&tx, reference.tracker, trackers, comment)?;
        synced += 1;
    }
    tx.commit()?;
    Ok(synced)
}
pub(crate) fn sync_progress(
    reference: &PapertrailRef,
    current: usize,
    total: usize,
    action: PapertrailSyncAction,
    message: Option<String>,
) -> PapertrailSyncProgress {
    PapertrailSyncProgress {
        current,
        total,
        project: reference.project.clone(),
        item_key: reference.item_key.clone(),
        action,
        message,
    }
}
/// Whether a discovered ref's item is already cached — the ONLY skip signal now that the per-ref
/// synced/not_found/failed state machine is deleted (`papertrail_sync_cursor` replaces it for the
/// mirror sync; the ref lane keeps no per-ref state). No `item_kind` filter: a bare `#N` ref could
/// name either kind, and either cached kind means the item was synced. A not-found item retries on
/// every sync (no memo) — acceptable for the referenced-only lane the mirror sync supersedes.
pub fn papertrail_ref_synced(conn: &Connection, reference: &PapertrailRef) -> anyhow::Result<bool> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let cached = conn.query_row(
        "
        SELECT EXISTS(
            SELECT 1 FROM papertrail_items
            WHERE tracker = ?1 AND project = ?2 AND item_key = ?3 AND repo_id = ?4
        )
        ",
        params![reference.tracker.as_db_str(), reference.project, reference.item_key, repo_id],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(cached)
}
pub(crate) fn is_not_found_error(message: &str) -> bool {
    message.contains("HTTP 404") || message.to_ascii_lowercase().contains("not found")
}
pub(crate) fn discover_commit_refs(
    conn: &Connection,
    trackers: &[ResolvedTracker],
    out: &mut Vec<PapertrailRef>,
) -> anyhow::Result<()> {
    // `git_commits` is direct-scoped (V040), so discovery only mines the ACTIVE repo's commit
    // messages for issue refs — a consolidated DB must not attribute a sibling repo's `#N` refs to
    // this repo.
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let mut stmt =
        conn.prepare("SELECT hash, subject, body FROM git_commits WHERE repo_id = ?1")?;
    let rows = stmt.query_map([&repo_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (hash, subject, body) = row?;
        for text in [subject, body] {
            for parsed in parse_tracker_refs(&text, trackers) {
                out.push(parsed.into_ref("commit", None, Some(hash.clone()), text.clone()));
            }
            if trackers.is_empty() {
                for parsed in parse_refs(&text, None) {
                    out.push(parsed.into_ref("commit", None, Some(hash.clone()), text.clone()));
                }
            }
        }
    }
    Ok(())
}
pub(crate) fn discover_file_refs(
    conn: &Connection,
    root: &Path,
    trackers: &[ResolvedTracker],
    out: &mut Vec<PapertrailRef>,
) -> anyhow::Result<()> {
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let path = row?;
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        for line in text.lines() {
            for parsed in parse_tracker_refs(line, trackers) {
                out.push(parsed.into_ref(
                    "file",
                    Some(path.clone()),
                    None,
                    line.trim().to_string(),
                ));
            }
            if trackers.is_empty() {
                for parsed in parse_refs(line, None) {
                    out.push(parsed.into_ref(
                        "file",
                        Some(path.clone()),
                        None,
                        line.trim().to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Version stamp of the mined-evidence backfill. Bump when the mining rules change in a way
/// that requires re-deriving from ALL cached text (identity format, keyword set, gates).
const MINED_REFS_VERSION: &str = "1";
const MINED_REFS_VERSION_KEY: &str = "papertrail_mined_refs_version";

/// The stamp VALUE folds the tracker-config fingerprint: adding a binding (a new Jira grammar)
/// must re-mine cached text — otherwise cross-tracker refs in old bodies stay missing forever,
/// because probe/ref sync skips unchanged cached items.
fn mined_refs_stamp(trackers: &[ResolvedTracker]) -> String {
    let mut grammar: Vec<String> = trackers
        .iter()
        .map(|binding| {
            format!(
                "{}:{}:{}",
                binding.provider.as_db_str(),
                binding.project,
                binding.base_url.as_deref().unwrap_or("")
            )
        })
        .collect();
    grammar.sort();
    format!(
        "{MINED_REFS_VERSION}:{}",
        rag_rat_base::hash::hex_sha256(grammar.join("\u{1e}").as_bytes())
    )
}

/// One-time (per [`MINED_REFS_VERSION`]) backfill: mine every ALREADY-CACHED item and comment
/// body. Rows fetched before store-time mining existed would otherwise keep un-mined text
/// forever — `sync_one_ref` skips cached items, and the mirror only re-stores changed ones.
/// Purely local (the bodies are in the DB); replace-on-remine makes it idempotent.
pub(crate) fn backfill_mined_refs(
    conn: &Connection,
    trackers: &[ResolvedTracker],
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let key = format!("{MINED_REFS_VERSION_KEY}:{repo_id}");
    let stamp = mined_refs_stamp(trackers);
    if rag_rat_db::meta::read_meta(conn, &key)?.as_deref() == Some(stamp.as_str()) {
        return Ok(());
    }
    let items: Vec<(Tracker, PapertrailItem)> = {
        let mut stmt = conn.prepare(
            "SELECT tracker, project, item_kind, item_key, url, state, title, body, merged_at, \
             merge_commit_sha FROM papertrail_items WHERE repo_id = ?1",
        )?;
        let rows = stmt.query_map([&repo_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (tracker, project, kind, key, url, state, title, body, merged_at, sha) = row?;
            let Ok(item_kind) = ItemKind::from_db_str(&kind) else { continue };
            let Ok(tracker) = Tracker::from_db_str(&tracker) else { continue };
            items.push((tracker, PapertrailItem {
                project,
                item_kind,
                item_key: key,
                url,
                state,
                title,
                body,
                author: None,
                created_at: None,
                updated_at: None,
                merged_at,
                closed_at: None,
                resolution: None,
                merge_commit_sha: sha,
                author_kind: None,
                author_association: None,
                tags: Vec::new(),
            }));
        }
        items
    };
    for (tracker, item) in &items {
        mine_item_refs(conn, *tracker, trackers, item)?;
    }
    let comments: Vec<(Tracker, PapertrailComment)> = {
        let mut stmt = conn.prepare(
            "SELECT tracker, project, item_kind, item_key, comment_id, body FROM \
             papertrail_comments WHERE repo_id = ?1",
        )?;
        let rows = stmt.query_map([&repo_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut comments = Vec::new();
        for row in rows {
            let (tracker, project, kind, key, comment_id, body) = row?;
            let Ok(item_kind) = ItemKind::from_db_str(&kind) else { continue };
            let Ok(tracker) = Tracker::from_db_str(&tracker) else { continue };
            comments.push((tracker, PapertrailComment {
                project,
                item_kind,
                item_key: key,
                comment_id,
                url: None,
                body,
                author: None,
                author_kind: None,
                author_association: None,
                created_at: None,
                updated_at: None,
                review_state: None,
                anchor_path: None,
            }));
        }
        comments
    };
    for (tracker, comment) in &comments {
        mine_comment_refs(conn, *tracker, trackers, comment)?;
    }
    rag_rat_db::meta::set_meta(conn, &key, &stamp)?;
    Ok(())
}

/// Mine a stored item's OWN text for tracker refs (`source_kind = 'item'`) — the tracker items'
/// bodies were never mined before #702, even though the text is already in hand (zero API cost).
/// Bare shorthand (`#5` / `!5`) belongs to the SOURCE item's own (provider, project) — including
/// a fetched foreign project not in the configured list — while qualified/URL refs resolve
/// against the configured bindings in config order (two-pass: source routing can never claim a
/// qualified cross-provider ref).
///
/// Deliberately ANNOTATIONS ONLY — no closing edges. Item text is MUTABLE: the current body is
/// not the merge-time body, so a post-merge edit could mint a retroactive closer (or erase a
/// real one) that the provider never acted on. Change-request closure evidence comes exclusively
/// from the provider-attested lane; the only text-tier closers are COMMIT-message ones, whose
/// sources are immutable (see `store_text_closing_edges_from_commit_refs`).
pub(crate) fn mine_item_refs(
    conn: &Connection,
    provider: Tracker,
    trackers: &[ResolvedTracker],
    item: &PapertrailItem,
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let identity = item_source_text(provider, item);
    // REPLACE-on-remine: item text is editable, so a removed link must not survive as evidence.
    // Keyed by the SOURCE identity alone (no tracker predicate — one source can reference
    // several configured trackers).
    conn.execute(
        "DELETE FROM papertrail_refs WHERE repo_id = ?1 AND source_kind = 'item' AND source_text \
         = ?2",
        params![repo_id, identity],
    )?;
    let source = source_binding(provider, &item.project, trackers);
    for text in [item.title.as_str(), item.body.as_str()] {
        for parsed in parse_tracker_refs_with_source(text, trackers, &source) {
            // Self-references are noise — but only a KIND-matching self (a bare `#N` inherits the
            // source's namespace; GitLab MR !5 naming issue #5 is a real cross-kind link).
            if parsed.provider == provider
                && parsed.project == item.project
                && parsed.item_key == item.item_key
                && parsed.item_kind.is_none_or(|kind| kind == item.item_kind)
            {
                continue;
            }
            let reference = parsed.into_ref("item", None, None, identity.clone());
            store_ref(conn, &reference)?;
        }
    }
    Ok(())
}

/// Mine a stored comment's text for tracker refs (`source_kind = 'comment'`), against the
/// FULL configured tracker set.
pub(crate) fn mine_comment_refs(
    conn: &Connection,
    provider: Tracker,
    trackers: &[ResolvedTracker],
    comment: &PapertrailComment,
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    let identity = comment_source_text(provider, comment);
    // REPLACE-on-remine, same contract as item bodies: comment text is editable, and one
    // comment can reference several configured trackers (no tracker predicate).
    conn.execute(
        "DELETE FROM papertrail_refs WHERE repo_id = ?1 AND source_kind = 'comment' AND \
         source_text = ?2",
        params![repo_id, identity],
    )?;
    let source = source_binding(provider, &comment.project, trackers);
    for parsed in parse_tracker_refs_with_source(&comment.body, trackers, &source) {
        if parsed.provider == provider
            && parsed.project == comment.project
            && parsed.item_key == comment.item_key
            && parsed.item_kind.is_none_or(|kind| kind == comment.item_kind)
        {
            continue;
        }
        let reference = parsed.into_ref("comment", None, None, identity.clone());
        store_ref(conn, &reference)?;
    }
    Ok(())
}

/// The stable `source_text` for an item-body ref: the item's own KIND-QUALIFIED identity, not
/// the (large, re-editable) body — the refs unique index folds `source_text`, so a body edit
/// re-mines into the same row, and namespaced providers (GitLab issue `#5` vs MR `!5`) never
/// coalesce two different source items onto one identity. PROVIDER-qualified too: two
/// configured providers mirroring the same `owner/repo` string must not delete each other's
/// mined rows on re-mine or prune.
pub(crate) fn item_source_text(provider: Tracker, item: &PapertrailItem) -> String {
    format!(
        "{}:{}:{}:{}",
        provider.as_db_str(),
        item.project,
        item.item_kind.as_db_str(),
        item.item_key
    )
}

/// The binding the SOURCE item's bare shorthand resolves against: the configured binding for
/// its exact (provider, project) when one exists (keeping base_url for self-hosted grammars),
/// else a synthetic binding for the fetched foreign project.
fn source_binding(
    provider: Tracker,
    project: &str,
    trackers: &[ResolvedTracker],
) -> ResolvedTracker {
    trackers
        .iter()
        .find(|binding| binding.provider == provider && binding.project == project)
        .cloned()
        .unwrap_or_else(|| ResolvedTracker {
            provider,
            project: project.to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        })
}

/// The stable kind-qualified identity of a mined comment (see [`item_source_text`]).
pub(crate) fn comment_source_text(provider: Tracker, comment: &PapertrailComment) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        provider.as_db_str(),
        comment.project,
        comment.item_kind.as_db_str(),
        comment.item_key,
        comment.comment_id
    )
}

/// Re-derive the commit-tier text closers from already-stored commit refs. Runs at the END of a
/// sync as well as during discovery: the target-kind verification needs the target CACHED, and
/// the first sync fetches targets AFTER discovery — without the post-sync pass a first run
/// would leave `Fixes #5` closers missing until the next discovery.
pub(crate) fn rederive_commit_closers(
    conn: &Connection,
    root: &Path,
    ctx: &PapertrailContext,
) -> anyhow::Result<()> {
    let branch = rag_rat_base::repo_discover::discover_repo(root)
        .ok()
        .and_then(|repo| repo.head_name().ok().flatten())
        .map(|name| name.shorten().to_string())
        .unwrap_or_default();
    let eligible = default_branch_checkout_projects(root, &branch, &ctx.trackers);
    let refs = {
        let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
        let mut stmt = conn.prepare(
            "SELECT tracker, project, item_key, item_kind, ref_kind, source_commit, source_text \
             FROM papertrail_refs WHERE repo_id = ?1 AND source_kind = 'commit' AND ref_kind = \
             'closing'",
        )?;
        let rows = stmt.query_map([&repo_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;
        let mut refs = Vec::new();
        for row in rows {
            let (tracker, project, item_key, item_kind, ref_kind, source_commit, source_text) =
                row?;
            let Ok(tracker) = Tracker::from_db_str(&tracker) else { continue };
            let item_kind = match item_kind.as_deref() {
                Some(kind) => Some(ItemKind::from_db_str(kind).ok()).flatten(),
                None => None,
            };
            refs.push(PapertrailRef {
                tracker,
                project,
                item_key,
                item_kind,
                ref_kind,
                source_kind: "commit".to_string(),
                source_path: None,
                source_commit,
                source_text,
            });
        }
        refs
    };
    store_text_closing_edges_from_commit_refs(conn, &refs, &eligible)
}

/// The (provider, project) pairs for which the CURRENT checkout's HEAD is that remote's default
/// branch — read locally from every remote's `HEAD` symref (`refs/remotes/<name>/HEAD`, set by
/// clone / `git remote set-head`) and the remote URL's parsed project. Per-remote, NOT
/// `origin`-hard-coded: a tracker can bind a non-origin remote, and `origin` can be a fork whose
/// default branch differs from the tracked project's.
fn default_branch_checkout_projects(
    root: &Path,
    branch: &str,
    trackers: &[ResolvedTracker],
) -> std::collections::BTreeSet<(Tracker, String)> {
    let mut eligible = std::collections::BTreeSet::new();
    if branch.is_empty() {
        return eligible;
    }
    let Ok(repo) = rag_rat_base::repo_discover::discover_repo(root) else {
        return eligible;
    };
    let Ok(references) = repo.references() else {
        return eligible;
    };
    let Ok(prefixed) = references.prefixed("refs/remotes/") else {
        return eligible;
    };
    for reference in prefixed.flatten() {
        let full = reference.name().as_bstr().to_string();
        let Some(rest) = full.strip_prefix("refs/remotes/") else { continue };
        let Some(remote) = rest.strip_suffix("/HEAD") else { continue };
        let Some(target) = reference.target().try_name().map(|name| name.as_bstr().to_string())
        else {
            continue;
        };
        let Some(default) = target.strip_prefix(&format!("refs/remotes/{remote}/")) else {
            continue;
        };
        if default != branch {
            continue;
        }
        let key = format!("remote.{remote}.url");
        let Some(url) = repo.config_snapshot().string(key.as_str()).map(|url| url.to_string())
        else {
            continue;
        };
        let Some(parts) = crate::trackers::parse_git_remote_url(url.trim()) else { continue };
        // Provider from the remote HOST, never from the path shape: GitLab's parser accepts any
        // two-segment path, so a GitHub remote must not mark a same-named GitLab project
        // eligible. Cloud hosts map via the shared detector; self-hosted instances match a
        // configured binding's base_url host.
        let provider = crate::trackers::detect_provider(&parts.host).or_else(|| {
            trackers
                .iter()
                .find(|binding| {
                    binding.base_url.as_deref().is_some_and(|base| {
                        base.trim_start_matches("https://")
                            .trim_start_matches("http://")
                            .split('/')
                            .next()
                            .is_some_and(|host| host.eq_ignore_ascii_case(&parts.host))
                    })
                })
                .map(|binding| binding.provider)
        });
        let Some(provider) = provider else { continue };
        if !matches!(provider, Tracker::Github | Tracker::Gitlab) {
            continue;
        }
        if let Some(project) = crate::trackers::remote_url_project(&parts, provider) {
            eligible.insert((provider, project));
        }
    }
    eligible
}

/// Text-tier commit closing edges, derived as a REPLACE SET each discovery pass: the prior
/// text/commit rows are dropped and re-derived from the CURRENT indexed history, so a commit
/// rebased or reloaded away takes its closer claim with it (provider-attested rows and the
/// converged rows they upgraded are untouched — the natural key keeps them, and re-insertion
/// under a provider row never downgrades it).
///
/// Providers honor commit closing keywords only on the DEFAULT branch, so edges mint only when
/// the indexed checkout's HEAD IS the default branch (`origin/HEAD`, read locally): then every
/// indexed commit is default-reachable. On a feature/release checkout — or when `origin/HEAD`
/// is unset — the refs stay annotations and the provider tier attests closures.
/// Claim strength for the in-memory duplicate collapse — closing beats reverts beats plain
/// reference beats unknown.
fn ref_kind_rank(kind: &str) -> u8 {
    match kind {
        "closing" => 0,
        "reverts" => 1,
        "reference" => 2,
        _ => 3,
    }
}

pub(crate) fn store_text_closing_edges_from_commit_refs(
    conn: &Connection,
    refs: &[PapertrailRef],
    default_branch_projects: &std::collections::BTreeSet<(Tracker, String)>,
) -> anyhow::Result<()> {
    let repo_id = rag_rat_db::schema::active_repo_id(conn)?;
    conn.execute(
        "DELETE FROM papertrail_closing_edges WHERE repo_id = ?1 AND source = 'text' AND \
         closer_kind = 'commit'",
        params![repo_id],
    )?;
    for reference in refs {
        // Per-project default-branch gate: HEAD must be THIS project's remote default branch.
        if !default_branch_projects.contains(&(reference.tracker, reference.project.clone())) {
            continue;
        }
        if reference.ref_kind != "closing" || reference.source_kind != "commit" {
            continue;
        }
        // Issue targets only: an explicit change-request target is an annotation, not a closer
        // entry in the issue↔closer table.
        if !matches!(reference.item_kind, None | Some(ItemKind::Issue)) {
            continue;
        }
        // Only providers whose COMMIT closing-keyword semantics this tier models: Jira smart
        // commits need explicit #transition commands (and workflow permissions), so an ordinary
        // "Fixes PROJ-123" commit mention stays an annotation.
        if !matches!(reference.tracker, Tracker::Github | Tracker::Gitlab) {
            continue;
        }
        // AFFIRMATIVE target verification: on shared-numbering providers a kind-less `#123`
        // can be a pull request, and closing keywords do nothing for PR targets — mint only
        // when the cached mirror confirms the target IS an issue. (Un-mirrored targets stay
        // annotations; the provider lane attests them.)
        let cached_kind: Option<String> = conn
            .query_row(
                "SELECT item_kind FROM papertrail_items WHERE repo_id = ?1 AND tracker = ?2 AND \
                 project = ?3 AND item_key = ?4 AND item_kind = 'issue'",
                params![
                    repo_id,
                    reference.tracker.as_db_str(),
                    reference.project,
                    reference.item_key
                ],
                |row| row.get(0),
            )
            .optional()?;
        if cached_kind.as_deref() != Some(ItemKind::Issue.as_db_str()) {
            continue;
        }
        let Some(commit) = reference.source_commit.as_deref() else {
            continue;
        };
        store_closing_edge(conn, reference.tracker, &ClosingEdge {
            project: reference.project.clone(),
            issue_kind: reference.item_kind.unwrap_or(ItemKind::Issue),
            issue_key: reference.item_key.clone(),
            closer_kind: CloserKind::Commit,
            closer_key: commit.to_string(),
            closer_commit: Some(commit.to_string()),
            source: ClosingEdgeSource::Text,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod mining_tests {
    use rusqlite::Connection;

    use super::*;
    use crate::store::closing_edges_for_item;
    use crate::{CloserKind, ClosingEdgeSource, TrackerAuthentication};

    fn binding() -> ResolvedTracker {
        ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        }
    }

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        rag_rat_db::schema::apply(&conn, &crate::test_hooks()).unwrap();
        conn
    }

    /// Commit-closer eligibility fixture: HEAD is o/r's default branch.
    fn on_default() -> std::collections::BTreeSet<(Tracker, String)> {
        std::collections::BTreeSet::from([(Tracker::Github, "o/r".to_string())])
    }

    /// Cache the TARGET as a mirrored issue — the strict kind check mints only for targets the
    /// mirror confirms are issues.
    fn cache_issue(conn: &Connection, key: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO papertrail_items(tracker, project, item_kind, item_key, url, \
             state, title, body, synced_at_ms, repo_id, state_normalized) VALUES ('github', \
             'o/r', 'issue', ?1, 'u', 'open', 't', 'b', 1, (SELECT COALESCE((SELECT repo_id FROM \
             repos LIMIT 1), '__unassigned__')), 'open')",
            [key],
        )
        .unwrap();
    }

    fn change_request(key: &str, body: &str) -> PapertrailItem {
        PapertrailItem {
            project: "o/r".into(),
            item_kind: ItemKind::ChangeRequest,
            item_key: key.into(),
            url: format!("http://item/{key}"),
            state: "closed".into(),
            title: "t".into(),
            body: body.into(),
            author: None,
            created_at: None,
            updated_at: None,
            merged_at: Some("2026-01-03T00:00:00Z".into()),
            closed_at: None,
            resolution: None,
            merge_commit_sha: Some("abc123".into()),
            author_kind: None,
            author_association: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn commit_closing_refs_mint_text_tier_commit_edges() {
        let conn = conn();
        let reference = PapertrailRef {
            tracker: Tracker::Github,
            project: "o/r".into(),
            item_key: "5".into(),
            item_kind: None,
            ref_kind: "closing".into(),
            source_kind: "commit".into(),
            source_path: None,
            source_commit: Some("deadbeef".into()),
            source_text: "fixes #5".into(),
        };
        cache_issue(&conn, "5");
        store_text_closing_edges_from_commit_refs(&conn, &[reference], &on_default()).unwrap();
        let edges =
            closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5").unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].closer_kind, CloserKind::Commit);
        assert_eq!(edges[0].closer_key, "deadbeef");
        assert_eq!(edges[0].source, ClosingEdgeSource::Text);
    }

    #[test]
    fn comment_body_mining_stores_comment_refs() {
        let conn = conn();
        let comment = PapertrailComment {
            project: "o/r".into(),
            item_kind: ItemKind::Issue,
            item_key: "5".into(),
            comment_id: "comment:1".into(),
            url: None,
            body: "duplicate of #7".into(),
            author: None,
            author_kind: None,
            author_association: None,
            created_at: None,
            updated_at: None,
            review_state: None,
            anchor_path: None,
        };
        mine_comment_refs(&conn, Tracker::Github, std::slice::from_ref(&binding()), &comment)
            .unwrap();
        let (key, kind): (String, String) = conn
            .query_row("SELECT item_key, source_kind FROM papertrail_refs", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!((key.as_str(), kind.as_str()), ("7", "comment"));
    }

    #[test]
    fn item_identities_are_kind_qualified_so_namespaced_keys_never_coalesce() {
        // GitLab: issue #5 and MR !5 share the numeric key under different kinds. Both bodies
        // reference the same target; the mined rows must remain TWO rows.
        let conn = conn();
        let mut issue = change_request("5", "see #7");
        issue.item_kind = ItemKind::Issue;
        issue.merged_at = None;
        let change = change_request("5", "see #7");
        mine_item_refs(&conn, Tracker::Github, std::slice::from_ref(&binding()), &issue).unwrap();
        mine_item_refs(&conn, Tracker::Github, std::slice::from_ref(&binding()), &change).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM papertrail_refs WHERE source_kind='item' AND item_key='7'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "one mined row per SOURCE item, not one per (target, project)");
    }

    #[test]
    fn cross_kind_self_key_reference_is_not_skipped_as_self() {
        // A namespaced provider's MR !5 body naming issue #5 is a REAL cross-kind link even
        // though project + key match the source item.
        let conn = conn();
        let mut change = change_request("5", "see #7");
        change.body = "Fixes #5".into();
        mine_item_refs(&conn, Tracker::Github, std::slice::from_ref(&binding()), &change).unwrap();
        // GitHub shares numbering, so the parsed target kind is None → treated as self and
        // skipped: the count stays zero here…
        let refs: i64 = conn
            .query_row("SELECT COUNT(*) FROM papertrail_refs WHERE source_kind='item'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(refs, 0, "a kind-less same-key ref inherits the source namespace: self");
    }

    #[test]
    fn pruned_item_takes_its_mined_evidence_with_it() {
        let conn = conn();
        let item = change_request("9", "Fixes #5");
        mine_item_refs(&conn, Tracker::Github, std::slice::from_ref(&binding()), &item).unwrap();
        // A provider-attested edge for the same pair must OUTLIVE the pruned text source.
        crate::store::store_closing_edge(&conn, Tracker::Github, &crate::ClosingEdge {
            project: "o/r".into(),
            issue_kind: ItemKind::Issue,
            issue_key: "5".into(),
            closer_kind: CloserKind::ChangeRequest,
            closer_key: "9".into(),
            closer_commit: Some("attested".into()),
            source: ClosingEdgeSource::Provider,
        })
        .unwrap();
        crate::mirror::delete_item_for_tests(&conn, &binding(), ItemKind::ChangeRequest, "9")
            .unwrap();
        let refs: i64 =
            conn.query_row("SELECT COUNT(*) FROM papertrail_refs", [], |r| r.get(0)).unwrap();
        assert_eq!(refs, 0, "the pruned item's mined refs die with it");
        let edges =
            closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5").unwrap();
        assert_eq!(edges.len(), 1, "the provider-attested edge survives the prune");
        assert_eq!(edges[0].source, ClosingEdgeSource::Provider);
    }
    #[test]
    fn commit_closers_are_a_replace_set_gated_on_the_default_branch() {
        let conn = conn();
        let make_ref = |sha: &str| PapertrailRef {
            tracker: Tracker::Github,
            project: "o/r".into(),
            item_key: "5".into(),
            item_kind: None,
            ref_kind: "closing".into(),
            source_kind: "commit".into(),
            source_path: None,
            source_commit: Some(sha.into()),
            source_text: "fixes #5".into(),
        };
        cache_issue(&conn, "5");
        store_text_closing_edges_from_commit_refs(&conn, &[make_ref("aaa")], &on_default())
            .unwrap();
        assert_eq!(
            closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
                .unwrap()
                .len(),
            1,
        );
        // The commit is rebased away: the next pass rederives WITHOUT it — the stale claim dies.
        store_text_closing_edges_from_commit_refs(&conn, &[make_ref("bbb")], &on_default())
            .unwrap();
        let edges =
            closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5").unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].closer_key, "bbb", "the replace set reflects current history only");
        // Off the default branch (or unknowable): the pass clears and mints nothing.
        store_text_closing_edges_from_commit_refs(&conn, &[make_ref("ccc")], &Default::default())
            .unwrap();
        assert!(
            closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
                .unwrap()
                .is_empty(),
            "commit-text closers require an affirmative default-branch checkout",
        );
    }

    #[test]
    fn duplicate_collapse_keeps_the_closing_claim() {
        assert!(ref_kind_rank("closing") < ref_kind_rank("reference"));
        assert!(ref_kind_rank("reverts") < ref_kind_rank("reference"));
        assert!(ref_kind_rank("reference") < ref_kind_rank("unknown"));
    }

    #[test]
    fn shorthand_resolves_against_the_source_project_binding_first() {
        // Two same-provider bindings: the SOURCE project's own binding must claim bare `#5`.
        let mut other = binding();
        other.project = "o/other".to_string();
        let trackers = vec![other, binding()];
        let item = change_request("9", "Fixes #5");
        let conn = conn();
        mine_item_refs(&conn, Tracker::Github, &trackers, &item).unwrap();
        let project: String = conn
            .query_row(
                "SELECT project FROM papertrail_refs WHERE source_kind='item' AND item_key='5'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(project, "o/r", "the bare ref belongs to the source project, not o/other");
    }
    #[test]
    fn qualified_cross_provider_refs_are_never_source_routed() {
        // A GitLab source body naming a qualified `o/r#5`: the claim follows CONFIG order
        // (GitHub configured first here), exactly like commit/file discovery — being the
        // SOURCE gives GitLab the bare shorthand, never the qualified tokens.
        let conn = conn();
        let mut gitlab = binding();
        gitlab.provider = Tracker::Gitlab;
        gitlab.project = "g/lab".to_string();
        let trackers = vec![binding(), gitlab.clone()];
        let mut item = change_request("9", "Fixes o/r#5 and closes #3");
        item.project = "g/lab".to_string();
        mine_item_refs(&conn, Tracker::Gitlab, &trackers, &item).unwrap();
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT tracker, project FROM papertrail_refs ORDER BY item_key")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("gitlab".to_string(), "g/lab".to_string()),
                ("github".to_string(), "o/r".to_string()),
            ],
            "bare #3 is the GitLab source's; qualified o/r#5 is GitHub's",
        );
    }

    #[test]
    fn fetched_foreign_items_resolve_shorthand_to_their_own_project() {
        // The source project is NOT in the configured list (a discovered foreign item): bare
        // shorthand still belongs to it, via the synthetic source binding.
        let conn = conn();
        let mut item = change_request("9", "Fixes #5");
        item.project = "other/repo".to_string();
        mine_item_refs(&conn, Tracker::Github, std::slice::from_ref(&binding()), &item).unwrap();
        let project: String = conn
            .query_row("SELECT project FROM papertrail_refs WHERE item_key='5'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(project, "other/repo");
    }

    #[test]
    fn jira_commit_mentions_stay_annotations() {
        // Jira smart commits need explicit #transition commands; "Fixes PROJ-123" in a commit
        // is a link, not a closure.
        let conn = conn();
        let reference = PapertrailRef {
            tracker: Tracker::Jira,
            project: "PROJ".into(),
            item_key: "PROJ-123".into(),
            item_kind: Some(ItemKind::Issue),
            ref_kind: "closing".into(),
            source_kind: "commit".into(),
            source_path: None,
            source_commit: Some("deadbeef".into()),
            source_text: "fixes PROJ-123".into(),
        };
        store_text_closing_edges_from_commit_refs(
            &conn,
            &[reference],
            &std::collections::BTreeSet::from([(Tracker::Jira, "PROJ".to_string())]),
        )
        .unwrap();
        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM papertrail_closing_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 0);
    }

    #[test]
    fn backfill_mines_already_cached_rows_once_per_version() {
        let conn = conn();
        // A cached item stored WITHOUT mining (simulating a pre-mining database).
        crate::store::store_item(&conn, Tracker::Github, &change_request("9", "Fixes #5")).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM papertrail_refs WHERE source_kind='item'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "store_item alone mines nothing — mining is the sync paths' job",
        );
        backfill_mined_refs(&conn, std::slice::from_ref(&binding())).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM papertrail_refs WHERE source_kind='item'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
            1,
            "the backfill mines the cached body",
        );
        // Versioned: a second call is a no-op (would double-run every sync otherwise).
        backfill_mined_refs(&conn, std::slice::from_ref(&binding())).unwrap();
    }
    #[test]
    fn item_body_mining_stores_annotation_refs_and_never_closing_edges() {
        // Item text is MUTABLE — the current body is not the merge-time body — so item mining
        // is annotations-only: even a merged change request's "Fixes #5" mints NO closing edge
        // (change-request closures are the provider lane's job).
        let conn = conn();
        let item = change_request("9", "Fixes #5 and see #6. Also mentions #9 itself.");
        mine_item_refs(&conn, Tracker::Github, std::slice::from_ref(&binding()), &item).unwrap();
        let refs: Vec<(String, String, String)> = conn
            .prepare(
                "SELECT item_key, ref_kind, source_kind FROM papertrail_refs ORDER BY item_key",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            refs,
            vec![
                ("5".to_string(), "closing".to_string(), "item".to_string()),
                ("6".to_string(), "reference".to_string(), "item".to_string()),
            ],
            "foreign refs stored (closing kept as the ANNOTATION kind); self-reference dropped",
        );
        let edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM papertrail_closing_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 0, "item text never mints closers — mutable text cannot attest them");
    }

    #[test]
    fn item_body_remining_replaces_the_mined_set() {
        let conn = conn();
        mine_item_refs(
            &conn,
            Tracker::Github,
            std::slice::from_ref(&binding()),
            &change_request("9", "Fixes #5"),
        )
        .unwrap();
        // Edited: the link is REMOVED — the mined ref must die; a reworded survivor converges.
        mine_item_refs(
            &conn,
            Tracker::Github,
            std::slice::from_ref(&binding()),
            &change_request("9", "see #6 now"),
        )
        .unwrap();
        let keys: Vec<String> = conn
            .prepare("SELECT item_key FROM papertrail_refs WHERE source_kind='item'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(keys, vec!["6".to_string()], "the edited-away link cannot survive as evidence");
    }

    #[test]
    fn gitlab_bang_shorthand_is_source_local_in_mining() {
        // Two GitLab bindings: a bare `!5` in the SECOND project's MR must not be claimed by
        // the first (config-order) binding.
        let conn = conn();
        let mut first = binding();
        first.provider = Tracker::Gitlab;
        first.project = "g/first".to_string();
        let mut second = first.clone();
        second.project = "g/second".to_string();
        let trackers = vec![first, second];
        let mut item = change_request("9", "see !5");
        item.project = "g/second".to_string();
        mine_item_refs(&conn, Tracker::Gitlab, &trackers, &item).unwrap();
        let project: String = conn
            .query_row("SELECT project FROM papertrail_refs WHERE item_key='5'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(project, "g/second", "bang shorthand belongs to the source project");
    }
    #[test]
    fn kindless_commit_target_verification_pins_the_issue_kind() {
        // Namespaced twin trap: GitLab issue #5 and MR !5 share the key. The cached-target
        // check must find the ISSUE row even when the change-request twin exists too.
        let conn = conn();
        cache_issue(&conn, "5");
        conn.execute(
            "INSERT INTO papertrail_items(tracker, project, item_kind, item_key, url, state, \
             title, body, synced_at_ms, repo_id, state_normalized) VALUES ('github', 'o/r', \
             'change_request', '5', 'u', 'open', 't', 'b', 1, '__unassigned__', 'open')",
            [],
        )
        .unwrap();
        let reference = PapertrailRef {
            tracker: Tracker::Github,
            project: "o/r".into(),
            item_key: "5".into(),
            item_kind: Some(ItemKind::Issue),
            ref_kind: "closing".into(),
            source_kind: "commit".into(),
            source_path: None,
            source_commit: Some("abc".into()),
            source_text: "fixes #5".into(),
        };
        store_text_closing_edges_from_commit_refs(&conn, &[reference], &on_default()).unwrap();
        assert_eq!(
            closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
                .unwrap()
                .len(),
            1,
            "the CR twin must not shadow the cached issue",
        );
    }
    #[test]
    fn rederive_never_resurrects_a_rebased_away_commits_closer() {
        // Stored refs are the annotation layer (never pruned): a closing ref whose source
        // commit left the indexed history must not re-mint through the post-sync rederive.
        let conn = conn();
        cache_issue(&conn, "5");
        conn.execute(
            "INSERT INTO papertrail_refs(tracker, project, item_key, item_kind, ref_kind, \
             source_kind, source_commit, source_text, discovered_at_ms, repo_id) VALUES \
             ('github', 'o/r', '5', 'issue', 'closing', 'commit', 'gone-sha', 'fixes #5', 0, \
             '__unassigned__')",
            [],
        )
        .unwrap();
        // The stored-ref query requires the commit in git_commits; 'gone-sha' is absent, so the
        // derivation set is empty and the replace pass clears without minting.
        let refs: Vec<PapertrailRef> = Vec::new();
        store_text_closing_edges_from_commit_refs(&conn, &refs, &on_default()).unwrap();
        assert!(
            closing_edges_for_item(&conn, Tracker::Github, "o/r", ItemKind::Issue, "5")
                .unwrap()
                .is_empty(),
        );
    }
}
