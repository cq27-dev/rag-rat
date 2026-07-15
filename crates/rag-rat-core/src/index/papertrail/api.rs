use super::*;
use crate::index::{repo_meta, schema, scoped_table_row_count, set_repo_meta};

/// Mirror every resolved binding, unconditionally (the explicit `papertrail sync` command).
/// Provider-client-pending and unauthenticatable bindings surface as report errors so the
/// operator sees exactly what a manual run could not do.
pub(crate) async fn sync_mirror(
    conn: &Connection,
    root: &Path,
    full: bool,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailSyncReport> {
    ensure_unique_mirror_bindings(&ctx.trackers)?;
    let refs = discover_and_store_refs(conn, root, ctx)?;
    let registry = transport::GovernorRegistry::default();
    let mut errors = Vec::new();
    let mut jobs = Vec::new();
    for binding in &ctx.trackers {
        record_attempt(conn, binding, now_ms())?;
        // One source of truth for native providers: the same capability the status surface and
        // the scheduled filter report.
        if tracker_synchronization(binding) != TrackerSynchronization::Native {
            errors.push(binding_error(
                binding,
                "provider_client_pending",
                format!("{} mirror client is not implemented yet", binding.provider.as_db_str()),
            ));
            continue;
        }
        if let Some(job) = prepare_binding_job(
            conn,
            binding,
            &registry,
            &ctx.transport_options,
            full,
            &mut errors,
        )? {
            jobs.push(job);
        }
    }
    let outcomes = run_binding_jobs(conn, jobs).await?;
    assemble_mirror_report(conn, ctx, refs.len(), outcomes, errors)
}

/// Mirror the bindings the scheduling policy says are due, all evaluated against ONE clock
/// snapshot (the automatic watcher/hook path). Differences from the manual [`sync_mirror`]:
/// provider-client-pending bindings are silently filtered (an automatic tick must not log the
/// same capability gap every 15 minutes — `status` reports it), `Skip` decisions never record an
/// attempt, and reference discovery does not run (it re-reads every indexed file from disk; the
/// annotation layer refreshes on manual sync).
pub(crate) async fn sync_mirror_scheduled(
    conn: &Connection,
    ctx: &PapertrailContext,
    request: AutosyncRequest,
) -> anyhow::Result<PapertrailSyncReport> {
    ensure_unique_mirror_bindings(&ctx.trackers)?;
    let repo_id = schema::active_repo_id(conn)?;
    let registry = transport::GovernorRegistry::default();
    let now = now_ms();
    let mut errors = Vec::new();
    let mut jobs = Vec::new();
    for binding in &ctx.trackers {
        if tracker_synchronization(binding) != TrackerSynchronization::Native {
            continue;
        }
        let (health, _, _, stored_fingerprint) = load_persisted_health(conn, &repo_id, binding)?;
        let filter_changed = stored_fingerprint != binding.filter_fingerprint();
        let change_detected = filter_changed || request >= AutosyncRequest::Incremental;
        let decision = decide_schedule(now, &ctx.schedule, health, change_detected);
        let Some(full) = scheduled_walk_depth(decision, request) else {
            continue;
        };
        record_attempt(conn, binding, now)?;
        if let Some(job) = prepare_binding_job(
            conn,
            binding,
            &registry,
            &ctx.transport_options,
            full,
            &mut errors,
        )? {
            jobs.push(job);
        }
    }
    if jobs.is_empty() && errors.is_empty() {
        // Nothing was due: report current status without stamping a sync that never ran.
        return Ok(PapertrailSyncReport {
            offline: false,
            discovered_refs: 0,
            skipped_refs: 0,
            failed_refs: 0,
            synced_items: 0,
            bindings: Vec::new(),
            errors: Vec::new(),
            status: status(conn, ctx)?,
        });
    }
    let outcomes = run_binding_jobs(conn, jobs).await?;
    assemble_mirror_report(conn, ctx, 0, outcomes, errors)
}

/// Walk depth for one binding the policy evaluated: `None` = do not dispatch, `Some(full)`
/// otherwise. A `Full` request upgrades any allowed decision — a daily/full trigger must never
/// be weakened by a weaker concurrent one — while `Skip` always wins: persisted pauses and the
/// minimum attempt interval gate even explicitly requested work.
fn scheduled_walk_depth(decision: ScheduleDecision, request: AutosyncRequest) -> Option<bool> {
    match decision {
        ScheduleDecision::Skip => None,
        ScheduleDecision::Full => Some(true),
        ScheduleDecision::Probe | ScheduleDecision::Incremental =>
            Some(request == AutosyncRequest::Full),
    }
}

/// One dispatchable per-binding mirror run: the provider client is already constructed and the
/// walk depth decided. Skipped and provider-client-pending bindings never become jobs.
struct BindingJob<'a> {
    binding: &'a ResolvedTracker,
    client: ProviderClient,
    full: bool,
}

/// The native client for one binding, dispatched by provider. An enum rather than a trait object
/// because [`PapertrailClient`]'s `async fn` methods are not dyn-compatible; each arm delegates.
enum ProviderClient {
    Github(GitHubClient),
    Gitlab(GitLabClient),
}

impl ProviderClient {
    fn new(
        binding: &ResolvedTracker,
        registry: &transport::GovernorRegistry,
        options: transport::TransportOptions,
    ) -> anyhow::Result<Self> {
        match binding.provider {
            Tracker::Github => GitHubClient::new(binding, registry, options).map(Self::Github),
            Tracker::Gitlab => GitLabClient::new(binding, registry, options).map(Self::Gitlab),
            other => anyhow::bail!("no native papertrail client for {} yet", other.as_db_str()),
        }
    }
}

impl PapertrailClient for ProviderClient {
    async fn item(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<PapertrailItem> {
        match self {
            Self::Github(client) => client.item(project, kind, key).await,
            Self::Gitlab(client) => client.item(project, kind, key).await,
        }
    }

    async fn item_comments(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
    ) -> anyhow::Result<Vec<PapertrailComment>> {
        match self {
            Self::Github(client) => client.item_comments(project, kind, key).await,
            Self::Gitlab(client) => client.item_comments(project, kind, key).await,
        }
    }

    fn item_comment_streams(&self, kind: ItemKind) -> &'static [&'static str] {
        match self {
            Self::Github(client) => client.item_comment_streams(kind),
            Self::Gitlab(client) => client.item_comment_streams(kind),
        }
    }

    async fn item_comments_page(
        &self,
        project: &str,
        kind: ItemKind,
        key: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        match self {
            Self::Github(client) => client.item_comments_page(project, kind, key, cursor).await,
            Self::Gitlab(client) => client.item_comments_page(project, kind, key, cursor).await,
        }
    }

    async fn enrich_item(&self, item: &mut PapertrailItem) -> anyhow::Result<()> {
        match self {
            Self::Github(client) => client.enrich_item(item).await,
            Self::Gitlab(client) => client.enrich_item(item).await,
        }
    }

    async fn items_page(&self, project: &str, cursor: &PageCursor) -> anyhow::Result<ItemsPage> {
        match self {
            Self::Github(client) => client.items_page(project, cursor).await,
            Self::Gitlab(client) => client.items_page(project, cursor).await,
        }
    }

    fn comment_streams(&self) -> &'static [&'static str] {
        match self {
            Self::Github(client) => client.comment_streams(),
            Self::Gitlab(client) => client.comment_streams(),
        }
    }

    async fn comments_page(
        &self,
        project: &str,
        cursor: &PageCursor,
    ) -> anyhow::Result<CommentsPage> {
        match self {
            Self::Github(client) => client.comments_page(project, cursor).await,
            Self::Gitlab(client) => client.comments_page(project, cursor).await,
        }
    }

    async fn freshness_probe(
        &self,
        project: &str,
        probe: &FreshnessProbe,
    ) -> anyhow::Result<FreshnessResult> {
        match self {
            Self::Github(client) => client.freshness_probe(project, probe).await,
            Self::Gitlab(client) => client.freshness_probe(project, probe).await,
        }
    }
}

/// What one dispatched binding produced: a resumable report, or the error entry to surface.
/// Health rows are already updated either way; a failed binding never cancels its siblings.
struct BindingOutcome {
    report: Option<MirrorBindingReport>,
    error: Option<PapertrailSyncError>,
}

/// Build the binding's client, or record the authentication failure and surface it into
/// `errors`. `None` = not dispatchable this run. Only a storage-layer error propagates.
fn prepare_binding_job<'a>(
    conn: &Connection,
    binding: &'a ResolvedTracker,
    registry: &transport::GovernorRegistry,
    options: &transport::TransportOptions,
    full: bool,
    errors: &mut Vec<PapertrailSyncError>,
) -> anyhow::Result<Option<BindingJob<'a>>> {
    match ProviderClient::new(binding, registry, options.clone()) {
        Ok(client) => Ok(Some(BindingJob { binding, client, full })),
        Err(error) => {
            let persisted_detail = authentication_failure_detail(binding, &error);
            record_failure(
                conn,
                binding,
                PapertrailErrorClass::Authentication,
                persisted_detail.as_deref(),
            )?;
            errors.push(binding_error(binding, "authentication_or_transport", error.to_string()));
            Ok(None)
        },
    }
}

/// Run every prepared job CONCURRENTLY on the papertrail current-thread runtime. Network waits
/// (page fetches, rate-governor sleeps) overlap across bindings, while every database commit
/// stays a short synchronous transaction between awaits on the single driving thread — commits
/// are serialized by construction, and no SQLite transaction, connection guard, or repository
/// write lock ever spans an await. Each job records its own success / pause / failure health, so
/// one binding's provider failure never cancels a sibling; only a storage-layer error (the
/// connection itself is broken) propagates, after every job has finished.
async fn run_binding_jobs(
    conn: &Connection,
    jobs: Vec<BindingJob<'_>>,
) -> anyhow::Result<Vec<BindingOutcome>> {
    futures_util::future::join_all(jobs.into_iter().map(|job| run_binding_job(conn, job)))
        .await
        .into_iter()
        .collect()
}

async fn run_binding_job(conn: &Connection, job: BindingJob<'_>) -> anyhow::Result<BindingOutcome> {
    match mirror_binding(conn, job.binding, &job.client, job.full).await {
        Ok(report) => {
            if let Some(operation) = completed_mirror_operation(&report, job.full) {
                record_success(conn, job.binding, operation, now_ms())?;
            } else if let Some(resume_at_ms) = report.paused_until_ms {
                record_pause(conn, job.binding, resume_at_ms)?;
            }
            Ok(BindingOutcome { report: Some(report), error: None })
        },
        Err(error) => {
            let class = classify_mirror_failure(&error);
            record_failure(conn, job.binding, class, Some(&error.to_string()))?;
            Ok(BindingOutcome {
                report: None,
                error: Some(binding_error(job.binding, "failed", error.to_string())),
            })
        },
    }
}

fn binding_error(binding: &ResolvedTracker, status: &str, error: String) -> PapertrailSyncError {
    PapertrailSyncError {
        tracker: binding.provider,
        project: binding.project.clone(),
        item_key: String::new(),
        status: status.to_string(),
        error,
    }
}

fn assemble_mirror_report(
    conn: &Connection,
    ctx: &PapertrailContext,
    discovered_refs: usize,
    outcomes: Vec<BindingOutcome>,
    mut errors: Vec<PapertrailSyncError>,
) -> anyhow::Result<PapertrailSyncReport> {
    let mut synced_items = 0;
    let mut bindings = Vec::new();
    for outcome in outcomes {
        if let Some(report) = outcome.report {
            synced_items += report.stored_items;
            bindings.push(report);
        }
        errors.extend(outcome.error);
    }
    let repo_id = schema::active_repo_id(conn)?;
    set_repo_meta(conn, &repo_id, "papertrail_last_sync_ms", &now_ms().to_string())?;
    Ok(PapertrailSyncReport {
        offline: false,
        discovered_refs,
        skipped_refs: 0,
        failed_refs: errors.len(),
        synced_items,
        bindings,
        errors,
        status: status(conn, ctx)?,
    })
}

fn authentication_failure_detail(
    binding: &ResolvedTracker,
    error: &anyhow::Error,
) -> Option<String> {
    match binding.auth {
        Some(crate::config::TrackerAuth::TokenCommand(_)) =>
            Some("configured token command failed".to_string()),
        _ => Some(error.to_string()),
    }
}

fn classify_mirror_failure(error: &anyhow::Error) -> PapertrailErrorClass {
    for cause in error.chain() {
        if cause.downcast_ref::<rusqlite::Error>().is_some() {
            return PapertrailErrorClass::Storage;
        }
        if let Some(transport) = cause.downcast_ref::<transport::TransportError>() {
            return match transport {
                transport::TransportError::Http(_) => PapertrailErrorClass::Network,
                transport::TransportError::Paused { .. } => PapertrailErrorClass::RateLimited,
                transport::TransportError::UrlOutsideBinding { .. } =>
                    PapertrailErrorClass::Provider,
            };
        }
        if cause.downcast_ref::<reqwest::Error>().is_some() {
            return PapertrailErrorClass::Network;
        }
    }
    PapertrailErrorClass::Provider
}

fn completed_mirror_operation(
    report: &MirrorBindingReport,
    full: bool,
) -> Option<SuccessfulOperation> {
    if report.paused_until_ms.is_some() {
        return None;
    }
    if full || report.completed_full_walk {
        return Some(SuccessfulOperation::FullMirror);
    }
    // A run whose item probe answered not-modified AND that stored / pruned nothing verified
    // freshness without mirroring anything: advance probe freshness only, so
    // `last_successful_mirror_ms` keeps meaning "content actually moved".
    let probe_only = report.probe_not_modified
        && report.stored_items == 0
        && report.stored_comments == 0
        && report.pruned_items == 0;
    Some(if probe_only {
        SuccessfulOperation::Probe
    } else {
        SuccessfulOperation::IncrementalMirror
    })
}

fn ensure_unique_mirror_bindings(trackers: &[ResolvedTracker]) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for binding in trackers {
        anyhow::ensure!(
            seen.insert((binding.provider.as_db_str(), binding.project.as_str())),
            "duplicate papertrail binding for {} project `{}`; mirror cache and cursor identity \
             require one binding per provider/project",
            binding.provider.as_db_str(),
            binding.project
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn sync_from_refs<C: PapertrailClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailSyncReport> {
    sync_from_refs_with_progress(conn, root, client, offline, ctx, |_| {}).await
}
#[cfg(test)]
pub(crate) async fn sync_from_refs_with_progress<C: PapertrailClient>(
    conn: &Connection,
    root: &Path,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
    mut progress: impl FnMut(PapertrailSyncProgress),
) -> anyhow::Result<PapertrailSyncReport> {
    let refs = discover_and_store_refs(conn, root, ctx)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("papertrail sync requires a client"))?;
        // Production discovery persists every provider's refs, but live sync remains GitHub-only
        // until the provider-client PRs build on the shared transport. Never send another
        // provider's identity through GitHubClient.
        sync_refs(
            conn,
            client,
            refs.iter()
                .filter(|reference| discovered_ref_uses_legacy_github_client(ctx, reference)),
            &mut progress,
        )
        .await?
    };
    let repo_id = schema::active_repo_id(conn)?;
    set_repo_meta(conn, &repo_id, "papertrail_last_sync_ms", &now_ms().to_string())?;
    Ok(PapertrailSyncReport {
        offline,
        discovered_refs: refs.len(),
        skipped_refs: sync.skipped_refs,
        failed_refs: sync.failed_refs,
        synced_items: sync.synced_items,
        bindings: Vec::new(),
        errors: sync.errors,
        status: status(conn, ctx)?,
    })
}
#[cfg(test)]
pub(crate) async fn sync_issue<C: PapertrailClient>(
    conn: &Connection,
    issue_ref: &str,
    client: Option<&C>,
    offline: bool,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailSyncReport> {
    let parsed = parse_issue_ref(issue_ref, ctx.default_repo())
        .ok_or_else(|| anyhow::anyhow!("invalid tracker item reference `{issue_ref}`"))?;
    let project = parsed.project.clone();
    let item_key = parsed.number.to_string();
    store_ref(conn, &parsed.into_ref("manual", None, None, issue_ref.to_string()))?;
    let refs = refs(conn)?;
    let sync = if offline {
        SyncRefsReport::default()
    } else {
        let client = client.ok_or_else(|| anyhow::anyhow!("papertrail sync requires a client"))?;
        sync_refs(
            conn,
            client,
            // `parse_issue_ref` is the explicit legacy GitHub command grammar. Its routing must
            // not depend on which discovery bindings happen to be configured for this repo.
            refs.iter().filter(|reference| {
                reference.tracker == Tracker::Github
                    && reference.project == project
                    && reference.item_key == item_key
            }),
            &mut |_| {},
        )
        .await?
    };
    let repo_id = schema::active_repo_id(conn)?;
    set_repo_meta(conn, &repo_id, "papertrail_last_sync_ms", &now_ms().to_string())?;
    Ok(PapertrailSyncReport {
        offline,
        discovered_refs: refs.len(),
        skipped_refs: sync.skipped_refs,
        failed_refs: sync.failed_refs,
        synced_items: sync.synced_items,
        bindings: Vec::new(),
        errors: sync.errors,
        status: status(conn, ctx)?,
    })
}
pub(crate) fn status(
    conn: &Connection,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailStatus> {
    // The papertrail_* tables are direct-scoped; report only the ACTIVE repo's counts, not the
    // union across a consolidated DB.
    let repo_id = schema::active_repo_id(conn)?;
    let items_by_kind = |kind: ItemKind| -> anyhow::Result<u64> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM papertrail_items WHERE item_kind = ?1 AND repo_id = ?2",
            params![kind.as_db_str(), repo_id],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or_default())
    };
    let now = now_ms();
    let bindings = ctx
        .trackers
        .iter()
        .map(|binding| {
            let (health, error_class, error_detail, stored_filter_fingerprint) =
                load_persisted_health(conn, &repo_id, binding)?;
            let filter_changed = stored_filter_fingerprint != binding.filter_fingerprint();
            let synchronization = tracker_synchronization(binding);
            let decision = decide_schedule(now, &ctx.schedule, health, filter_changed);
            let (overdue, failed) = binding_status_flags(synchronization, decision, error_class);
            Ok(PapertrailBindingStatus {
                tracker: binding.provider,
                project: binding.project.clone(),
                last_attempt_ms: health.last_attempt_ms,
                last_successful_probe_ms: health.last_successful_probe_ms,
                last_successful_mirror_ms: health.last_successful_mirror_ms,
                last_full_walk_ms: health.last_full_walk_ms,
                retry_not_before_ms: health.retry_not_before_ms,
                full_walk_in_progress: health.continuation == MirrorContinuation::Full,
                error_class,
                error_detail,
                overdue,
                failed,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(PapertrailStatus {
        refs: scoped_table_row_count(conn, "papertrail_refs", &repo_id)?,
        issues: items_by_kind(ItemKind::Issue)?,
        change_requests: items_by_kind(ItemKind::ChangeRequest)?,
        comments: scoped_table_row_count(conn, "papertrail_comments", &repo_id)?,
        last_sync_ms: repo_meta(conn, &repo_id, "papertrail_last_sync_ms")?
            .and_then(|value| value.parse().ok()),
        capabilities: ctx
            .trackers
            .iter()
            .map(|binding| TrackerCapability {
                tracker: binding.provider,
                project: binding.project.clone(),
                authentication: binding.authentication,
                synchronization: tracker_synchronization(binding),
            })
            .collect(),
        bindings,
    })
}

/// Test-only compatibility routing for the retired reference-driven sync fixtures. Production
/// synchronization uses [`sync_mirror`]; references are annotations only.
#[cfg(test)]
fn discovered_ref_uses_legacy_github_client(
    ctx: &PapertrailContext,
    reference: &PapertrailRef,
) -> bool {
    if reference.tracker != Tracker::Github {
        return false;
    }
    if ctx.trackers.is_empty() {
        return true;
    }
    parse_tracker_refs_with_bindings(&reference.source_text, &ctx.trackers)
        .into_iter()
        .find(|(_, parsed)| {
            parsed.provider == reference.tracker
                && parsed.project == reference.project
                && parsed.item_key == reference.item_key
                // V060-migrated GitHub refs have NULL kind even when their source URL now parses
                // as `/pull/N`; NULL is unknown, not a conflicting kind.
                && reference
                    .item_kind
                    .is_none_or(|kind| parsed.item_kind == Some(kind))
        })
        .is_some_and(|(binding_index, _)| ctx.trackers[binding_index].base_url.is_none())
}

fn tracker_synchronization(binding: &ResolvedTracker) -> TrackerSynchronization {
    match binding.provider {
        Tracker::Github | Tracker::Gitlab => TrackerSynchronization::Native,
        _ => TrackerSynchronization::ProviderClientPending,
    }
}

fn binding_status_flags(
    synchronization: TrackerSynchronization,
    decision: ScheduleDecision,
    error_class: Option<PapertrailErrorClass>,
) -> (bool, bool) {
    if synchronization == TrackerSynchronization::ProviderClientPending {
        return (false, false);
    }
    (
        decision != ScheduleDecision::Skip,
        error_class.is_some_and(|class| class != PapertrailErrorClass::RateLimited),
    )
}
pub(crate) fn issue_search(
    conn: &Connection,
    query: &str,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    // Item rows only (issues AND change requests — their titles/bodies), not comment rows.
    search_fts(conn, query, Some("item"), limit)
}
pub(crate) fn rationale_search(
    conn: &Connection,
    query: &str,
    limit: u32,
    ctx: &PapertrailContext,
) -> anyhow::Result<Vec<PapertrailEvidence>> {
    let mut evidence = Vec::new();
    for parsed in parse_tracker_refs(query, &ctx.trackers) {
        evidence.extend(evidence_for_item(
            conn,
            parsed.provider,
            &parsed.project,
            &parsed.item_key,
            parsed.item_kind,
            limit,
        )?);
    }
    if ctx.trackers.is_empty() {
        for parsed in parse_refs(query, None) {
            evidence.extend(evidence_for_item(
                conn,
                Tracker::Github,
                &parsed.project,
                &parsed.number.to_string(),
                None,
                limit,
            )?);
        }
    }
    evidence.extend(search_fts(conn, query, None, limit)?);
    dedupe_evidence(&mut evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(evidence)
}
pub(crate) fn refs_for_path(
    conn: &Connection,
    path: &str,
    limit: u32,
) -> anyhow::Result<Vec<PapertrailRef>> {
    let repo_id = schema::active_repo_id(conn)?;
    let mut stmt = conn.prepare(
        "
        SELECT tracker, project, item_key, item_kind, ref_kind, source_kind, source_path, \
         source_commit, source_text
        FROM papertrail_refs
        WHERE source_path = ?1 AND repo_id = ?3
        ORDER BY id DESC
        LIMIT ?2
        ",
    )?;
    let rows = stmt.query_map(params![path, i64::from(limit), repo_id], ref_row)?;
    collect_rows(rows)
}
pub(crate) fn papertrail_for_chunk(
    conn: &Connection,
    chunk: &crate::query::ReadChunk,
    limit: u32,
    ctx: &PapertrailContext,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_path(conn, &chunk.path, limit)?;
    if evidence.is_empty() {
        evidence = rationale_search(conn, &chunk.path, limit, ctx)?;
    }
    Ok(Papertrail {
        current_source: Some(CurrentSourceEvidence {
            chunk_id: Some(chunk.chunk_id),
            path: chunk.path.clone(),
            start_line: Some(chunk.start_line),
            end_line: Some(chunk.end_line),
            symbol: chunk.symbol_path.clone(),
        }),
        evidence,
        fallback_evidence: Vec::new(),
    })
}
pub(crate) fn papertrail_for_symbol(
    conn: &Connection,
    symbol: &crate::query::symbol::SymbolHit,
    limit: u32,
    ctx: &PapertrailContext,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_path(conn, &symbol.path, limit)?;
    evidence.extend(rationale_search(conn, &symbol.qualified_name, limit, ctx)?);
    dedupe_evidence(&mut evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let (start_line, end_line, chunk_id) = current_symbol_span(conn, symbol)?;
    Ok(Papertrail {
        current_source: Some(CurrentSourceEvidence {
            chunk_id,
            path: symbol.path.clone(),
            start_line,
            end_line,
            symbol: Some(symbol.qualified_name.clone()),
        }),
        evidence,
        fallback_evidence: Vec::new(),
    })
}
pub(crate) fn papertrail_for_commit(
    conn: &Connection,
    commit_hash: &str,
    limit: u32,
    ctx: &PapertrailContext,
) -> anyhow::Result<Papertrail> {
    let mut evidence = evidence_for_commit_refs(conn, commit_hash, limit)?;
    let mut fallback_evidence = Vec::new();
    if evidence.is_empty() {
        // `git_file_changes` is direct-scoped (V040): the prefix probe must not resolve a sibling
        // repo's commit in a consolidated DB (forks share hashes).
        let repo_id = schema::active_repo_id(conn)?;
        let mut stmt = conn.prepare(
            "SELECT path FROM git_file_changes WHERE commit_hash LIKE ?1 AND repo_id = ?3 ORDER \
             BY path LIMIT ?2",
        )?;
        let commit_like = format!("{commit_hash}%");
        let rows = stmt.query_map(params![commit_like, i64::from(limit), repo_id], |row| {
            row.get::<_, String>(0)
        })?;
        for row in rows {
            fallback_evidence.extend(evidence_for_path(conn, &row?, limit)?);
        }
        fallback_evidence.extend(rationale_search(conn, commit_hash, limit, ctx)?);
        mark_fallback_evidence(&mut fallback_evidence);
    }
    dedupe_evidence(&mut evidence);
    dedupe_evidence(&mut fallback_evidence);
    evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    fallback_evidence.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(Papertrail { current_source: None, evidence, fallback_evidence })
}
pub(crate) fn mark_fallback_evidence(evidence: &mut [PapertrailEvidence]) {
    for item in evidence {
        item.evidence_kind = match item.evidence_kind {
            "literal_tracker_ref" => "fallback_literal_tracker_ref",
            "historical_tracker" => "fallback_historical_tracker",
            _ => "fallback_tracker_evidence",
        };
        item.score = item.score.min(0.25);
    }
}

#[cfg(test)]
mod capability_tests {
    use super::super::transport::stub::{StubResponse, spawn_script_stub};
    use super::*;
    use crate::index::schema;

    fn github(base_url: Option<&str>) -> ResolvedTracker {
        ResolvedTracker {
            provider: Tracker::Github,
            project: "o/r".to_string(),
            base_url: base_url.map(str::to_string),
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        }
    }

    #[test]
    fn synchronization_reports_native_github_for_cloud_and_enterprise() {
        assert_eq!(tracker_synchronization(&github(None)), TrackerSynchronization::Native);
        let enterprise = PapertrailContext {
            trackers: vec![github(Some("https://github.example.com"))],
            ..PapertrailContext::default()
        };
        assert_eq!(
            tracker_synchronization(&enterprise.trackers[0]),
            TrackerSynchronization::Native
        );
    }

    #[test]
    fn public_binding_status_distinguishes_capability_pause_and_failure() {
        assert_eq!(
            binding_status_flags(
                TrackerSynchronization::ProviderClientPending,
                ScheduleDecision::Full,
                Some(PapertrailErrorClass::Provider),
            ),
            (false, false)
        );
        assert_eq!(
            binding_status_flags(
                TrackerSynchronization::Native,
                ScheduleDecision::Skip,
                Some(PapertrailErrorClass::RateLimited),
            ),
            (false, false)
        );
        assert_eq!(
            binding_status_flags(
                TrackerSynchronization::Native,
                ScheduleDecision::Incremental,
                Some(PapertrailErrorClass::Authentication),
            ),
            (true, true)
        );
    }

    #[test]
    fn persisted_rate_limit_pause_is_not_reported_as_a_failure() {
        let binding = github(None);
        let ctx =
            PapertrailContext { trackers: vec![binding.clone()], ..PapertrailContext::default() };
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        record_pause(&conn, &binding, i64::MAX).unwrap();

        let binding_status = status(&conn, &ctx).unwrap().bindings.remove(0);
        assert_eq!(binding_status.error_class, Some(PapertrailErrorClass::RateLimited));
        assert!(!binding_status.overdue);
        assert!(!binding_status.failed);
    }

    #[test]
    fn paused_mirror_is_not_a_successful_operation() {
        let report = MirrorBindingReport {
            tracker: Tracker::Github,
            project: "o/r".to_string(),
            stored_items: 1,
            stored_comments: 0,
            pruned_items: 0,
            paused_until_ms: Some(42),
            pause_reason: Some("rate_limited".to_string()),
            completed_full_walk: false,
            probe_not_modified: false,
        };
        assert_eq!(completed_mirror_operation(&report, false), None);
        assert_eq!(completed_mirror_operation(&report, true), None);

        let completed = MirrorBindingReport { paused_until_ms: None, ..report };
        assert_eq!(
            completed_mirror_operation(&completed, false),
            Some(SuccessfulOperation::IncrementalMirror)
        );
        assert_eq!(
            completed_mirror_operation(&completed, true),
            Some(SuccessfulOperation::FullMirror)
        );
    }

    #[test]
    fn completed_initial_backfill_is_a_full_walk() {
        let report = MirrorBindingReport {
            tracker: Tracker::Github,
            project: "o/r".to_string(),
            stored_items: 1,
            stored_comments: 0,
            pruned_items: 0,
            paused_until_ms: None,
            pause_reason: None,
            completed_full_walk: true,
            probe_not_modified: false,
        };
        assert_eq!(
            completed_mirror_operation(&report, false),
            Some(SuccessfulOperation::FullMirror)
        );
    }

    #[test]
    fn quiet_probe_run_is_a_probe_success_but_any_stored_work_is_a_mirror() {
        let quiet = MirrorBindingReport {
            tracker: Tracker::Github,
            project: "o/r".to_string(),
            stored_items: 0,
            stored_comments: 0,
            pruned_items: 0,
            paused_until_ms: None,
            pause_reason: None,
            completed_full_walk: false,
            probe_not_modified: true,
        };
        assert_eq!(completed_mirror_operation(&quiet, false), Some(SuccessfulOperation::Probe));
        // Comment deltas can move even when the item probe answers not-modified; stored work
        // makes the run a real mirror.
        let stored_comments = MirrorBindingReport { stored_comments: 2, ..quiet.clone() };
        assert_eq!(
            completed_mirror_operation(&stored_comments, false),
            Some(SuccessfulOperation::IncrementalMirror)
        );
        // An explicitly requested full walk is never downgraded by a quiet probe.
        assert_eq!(completed_mirror_operation(&quiet, true), Some(SuccessfulOperation::FullMirror));
    }

    #[test]
    fn scheduled_walk_depth_upgrades_on_full_requests_and_respects_skip() {
        use ScheduleDecision::{Full, Incremental, Probe, Skip};
        for request in
            [AutosyncRequest::Evaluate, AutosyncRequest::Incremental, AutosyncRequest::Full]
        {
            assert_eq!(scheduled_walk_depth(Skip, request), None, "Skip gates {request:?}");
            assert_eq!(scheduled_walk_depth(Full, request), Some(true));
        }
        for decision in [Probe, Incremental] {
            assert_eq!(scheduled_walk_depth(decision, AutosyncRequest::Evaluate), Some(false));
            assert_eq!(scheduled_walk_depth(decision, AutosyncRequest::Incremental), Some(false));
            assert_eq!(scheduled_walk_depth(decision, AutosyncRequest::Full), Some(true));
        }
    }

    #[test]
    fn manual_mirror_dispatches_every_resolved_github_binding() {
        let script = |project: &str| {
            vec![
                StubResponse::ok(&format!(
                    r#"{{"incomplete_results":false,"items":[{{"number":1,"html_url":"https://example.test/{project}/issues/1","state":"open","title":"{project}","body":"","updated_at":"2026-01-01T00:00:00Z","labels":[]}}]}}"#
                )),
                StubResponse::ok("[]"),
                StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
                StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
                StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
                StubResponse::ok("[]"),
                StubResponse::ok("[]"),
            ]
        };
        let (first_url, first_handle) = spawn_script_stub(script("a/one"));
        let (second_url, second_handle) = spawn_script_stub(script("b/two"));
        let binding = |project: &str, base_url: String| ResolvedTracker {
            provider: Tracker::Github,
            project: project.to_string(),
            base_url: Some(base_url),
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        };
        let ctx = PapertrailContext {
            trackers: vec![binding("a/one", first_url), binding("b/two", second_url)],
            ..PapertrailContext::default()
        };
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        let report = block_on(sync_mirror(&conn, Path::new("."), false, &ctx)).unwrap();
        assert_eq!(report.synced_items, 2);
        assert_eq!(report.bindings.len(), 2);
        assert_eq!(report.bindings[0].project, "a/one");
        assert_eq!(report.bindings[1].project, "b/two");
        assert!(report.bindings.iter().all(|binding| binding.completed_full_walk));
        assert!(report.status.bindings.iter().all(|binding| binding.last_full_walk_ms.is_some()));
        assert_eq!(report.status.issues, 2);
        assert_eq!(first_handle.join().unwrap().len(), 7);
        assert_eq!(second_handle.join().unwrap().len(), 7);
    }

    #[test]
    fn manual_mirror_rejects_duplicate_provider_projects_before_dispatch() {
        let mut first = github(Some("https://github.com"));
        first.tags = vec!["first".to_string()];
        let mut second = github(Some("https://github.example.com"));
        second.tags = vec!["second".to_string()];
        let ctx =
            PapertrailContext { trackers: vec![first, second], ..PapertrailContext::default() };
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();

        let error = block_on(sync_mirror(&conn, Path::new("."), false, &ctx)).unwrap_err();
        assert!(
            error.to_string().contains("duplicate papertrail binding for github project `o/r`")
        );
        let refs: i64 =
            conn.query_row("SELECT COUNT(*) FROM papertrail_refs", [], |row| row.get(0)).unwrap();
        assert_eq!(refs, 0);
    }

    #[test]
    fn token_command_failure_detail_is_redacted_before_persistence() {
        let mut binding = github(None);
        binding.auth =
            Some(crate::config::TrackerAuth::TokenCommand("secret-bearing command".to_string()));
        let error = anyhow::anyhow!("token_command `secret-bearing command` failed: secret stderr");
        assert_eq!(
            authentication_failure_detail(&binding, &error).as_deref(),
            Some("configured token command failed")
        );
    }

    #[test]
    fn mirror_failure_classification_uses_the_error_chain() {
        let storage =
            anyhow::Error::new(rusqlite::Error::InvalidQuery).context("commit mirror page");
        assert_eq!(classify_mirror_failure(&storage), PapertrailErrorClass::Storage);

        let network =
            anyhow::Error::new(reqwest::Client::new().get("://invalid").build().unwrap_err())
                .context("fetch mirror page");
        assert_eq!(classify_mirror_failure(&network), PapertrailErrorClass::Network);

        let provider = anyhow::anyhow!("provider payload did not match the expected schema");
        assert_eq!(classify_mirror_failure(&provider), PapertrailErrorClass::Provider);
    }

    #[test]
    fn manual_mirror_reports_pending_invalid_and_failed_bindings_independently() {
        let (failed_url, failed_handle) =
            spawn_script_stub(vec![StubResponse::status("500 Internal Server Error", "boom")]);
        let tracker = |provider, project: &str, base_url: Option<String>| ResolvedTracker {
            provider,
            project: project.to_string(),
            base_url,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        };
        let ctx = PapertrailContext {
            trackers: vec![
                tracker(Tracker::Bitbucket, "workspace/repo", None),
                tracker(Tracker::Github, "bad/origin", Some("not an absolute URL".to_string())),
                tracker(Tracker::Github, "fails/http", Some(failed_url)),
            ],
            ..PapertrailContext::default()
        };
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();

        let report = block_on(sync_mirror(&conn, Path::new("."), false, &ctx)).unwrap();
        assert_eq!(report.bindings.len(), 0);
        assert_eq!(report.failed_refs, 3);
        assert_eq!(
            report.errors.iter().map(|error| error.status.as_str()).collect::<Vec<_>>(),
            vec!["provider_client_pending", "authentication_or_transport", "failed"]
        );
        assert_eq!(failed_handle.join().unwrap().len(), 1);
        let pending = report
            .status
            .bindings
            .iter()
            .find(|binding| binding.project == "workspace/repo")
            .unwrap();
        assert!(!pending.failed);
        assert!(!pending.overdue);
        assert_eq!(pending.error_class, None);
    }
}

#[cfg(test)]
mod scheduled_tests {
    use super::super::transport::stub::{
        StubResponse, spawn_script_stub, spawn_script_stub_coordinated,
    };
    use super::*;
    use crate::index::schema;

    const HOUR_MS: i64 = 3_600_000;
    const HIGH_MARK: &str = "2026-01-01T00:00:00Z";

    fn github_binding(project: &str, base_url: &str) -> ResolvedTracker {
        ResolvedTracker {
            provider: Tracker::Github,
            project: project.to_string(),
            base_url: Some(base_url.to_string()),
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        }
    }

    fn open_schema() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    /// Persist a completed-backfill cursor whose filter fingerprint matches the binding, so a
    /// scheduled run enters the ordinary probe/delta lane instead of a first-walk backfill.
    fn seed_quiet_cursor(conn: &Connection, binding: &ResolvedTracker) {
        record_attempt(conn, binding, 0).unwrap();
        conn.execute(
            "UPDATE papertrail_sync_cursor
             SET backfill_done=1, high_mark_at=?1, filter_fingerprint=?2",
            params![HIGH_MARK, binding.filter_fingerprint()],
        )
        .unwrap();
    }

    struct SeededHealth {
        attempt: i64,
        probe: i64,
        mirror: i64,
        full: i64,
    }

    fn seed_health(conn: &Connection, binding: &ResolvedTracker, health: &SeededHealth) {
        record_attempt(conn, binding, health.attempt).unwrap();
        conn.execute(
            "UPDATE papertrail_sync_cursor
             SET last_successful_probe_ms=?1, last_successful_mirror_ms=?2, last_full_sync_ms=?3",
            params![health.probe, health.mirror, health.full],
        )
        .unwrap();
    }

    fn persisted(conn: &Connection, binding: &ResolvedTracker) -> BindingScheduleState {
        let repo_id = schema::active_repo_id(conn).unwrap();
        load_persisted_health(conn, &repo_id, binding).unwrap().0
    }

    /// The wire shape of a quiet probe run: a not-modified item probe, then one empty page per
    /// repo-comment stream.
    fn probe_script() -> Vec<StubResponse> {
        vec![
            StubResponse::status("304 Not Modified", ""),
            StubResponse::ok("[]"),
            StubResponse::ok("[]"),
        ]
    }

    fn full_walk_script(project: &str) -> Vec<StubResponse> {
        vec![
            StubResponse::ok(&format!(
                r#"{{"incomplete_results":false,"items":[{{"number":1,"html_url":"https://example.test/{project}/issues/1","state":"open","title":"{project}","body":"","updated_at":"2026-01-01T00:00:00Z","labels":[]}}]}}"#
            )),
            StubResponse::ok("[]"),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok("[]"),
            StubResponse::ok("[]"),
        ]
    }

    #[test]
    fn bindings_inside_the_minimum_attempt_interval_are_skipped_for_every_request() {
        let conn = open_schema();
        let (url, handle) = spawn_script_stub(Vec::new());
        let binding = github_binding("o/r", &url);
        let now = now_ms();
        seed_quiet_cursor(&conn, &binding);
        seed_health(&conn, &binding, &SeededHealth {
            attempt: now - 1_000,
            probe: now - 2 * HOUR_MS,
            mirror: now - 2 * HOUR_MS,
            full: now - 2 * HOUR_MS,
        });
        let ctx =
            PapertrailContext { trackers: vec![binding.clone()], ..PapertrailContext::default() };

        for request in
            [AutosyncRequest::Evaluate, AutosyncRequest::Incremental, AutosyncRequest::Full]
        {
            let report = block_on(sync_mirror_scheduled(&conn, &ctx, request)).unwrap();
            assert!(report.bindings.is_empty(), "{request:?} must not dispatch");
            assert!(report.errors.is_empty());
            assert_eq!(report.synced_items, 0);
        }
        // The recent attempt is untouched — a skipped binding records nothing.
        assert_eq!(persisted(&conn, &binding).last_attempt_ms, Some(now - 1_000));
        assert!(handle.join().unwrap().is_empty(), "no network request may be made");
    }

    #[test]
    fn due_probe_advances_probe_freshness_without_touching_mirror_timestamps() {
        let conn = open_schema();
        let (url, handle) = spawn_script_stub(probe_script());
        let binding = github_binding("o/r", &url);
        let now = now_ms();
        seed_quiet_cursor(&conn, &binding);
        let seeded = SeededHealth {
            attempt: now - HOUR_MS,
            probe: now - HOUR_MS,
            mirror: now - HOUR_MS,
            full: now - 2 * HOUR_MS,
        };
        seed_health(&conn, &binding, &seeded);
        let ctx =
            PapertrailContext { trackers: vec![binding.clone()], ..PapertrailContext::default() };

        let report =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Evaluate)).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.bindings.len(), 1);
        assert!(report.bindings[0].probe_not_modified);
        assert_eq!(report.synced_items, 0);

        let health = persisted(&conn, &binding);
        assert!(health.last_successful_probe_ms.unwrap() >= now, "probe freshness advances");
        assert_eq!(health.last_successful_mirror_ms, Some(seeded.mirror), "mirror is untouched");
        assert_eq!(health.last_full_walk_ms, Some(seeded.full), "full walk is untouched");
        assert_eq!(handle.join().unwrap().len(), 3);
    }

    #[test]
    fn overdue_full_backstop_dominates_a_fresh_probe_and_completes_a_full_walk() {
        let conn = open_schema();
        let (url, _handle) = spawn_script_stub(full_walk_script("o/r"));
        let binding = github_binding("o/r", &url);
        let now = now_ms();
        seed_quiet_cursor(&conn, &binding);
        seed_health(&conn, &binding, &SeededHealth {
            attempt: now - 2 * HOUR_MS,
            probe: now - 60_000, // fresh probe: the daily backstop must still win
            mirror: now - 2 * HOUR_MS,
            full: now - 25 * HOUR_MS,
        });
        let ctx =
            PapertrailContext { trackers: vec![binding.clone()], ..PapertrailContext::default() };

        let report =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Evaluate)).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.bindings.len(), 1);
        assert!(report.bindings[0].completed_full_walk, "the backstop runs a FULL walk");
        assert_eq!(report.synced_items, 1);
        let health = persisted(&conn, &binding);
        assert!(health.last_full_walk_ms.unwrap() >= now, "full-walk freshness advances");
    }

    #[test]
    fn scheduled_bindings_fail_independently_and_persist_health() {
        let conn = open_schema();
        let (failing_url, failing_handle) =
            spawn_script_stub(vec![StubResponse::status("500 Internal Server Error", "boom")]);
        let (good_url, _good_handle) = spawn_script_stub(full_walk_script("b/two"));
        let failing = github_binding("a/one", &failing_url);
        let good = github_binding("b/two", &good_url);
        let ctx = PapertrailContext {
            trackers: vec![failing.clone(), good.clone()],
            ..PapertrailContext::default()
        };

        let report =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Incremental)).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].project, "a/one");
        assert_eq!(report.errors[0].status, "failed");
        assert_eq!(report.bindings.len(), 1, "the sibling binding completes");
        assert_eq!(report.bindings[0].project, "b/two");
        assert!(report.bindings[0].completed_full_walk);

        let failing_status =
            report.status.bindings.iter().find(|binding| binding.project == "a/one").unwrap();
        assert!(failing_status.error_class.is_some(), "the failure class is persisted");
        let good_status =
            report.status.bindings.iter().find(|binding| binding.project == "b/two").unwrap();
        assert_eq!(good_status.error_class, None);
        assert!(good_status.last_full_walk_ms.is_some());
        assert_eq!(failing_handle.join().unwrap().len(), 1);
    }

    #[test]
    fn provider_client_pending_bindings_are_silently_filtered_from_scheduled_runs() {
        let conn = open_schema();
        let binding = ResolvedTracker {
            provider: Tracker::Bitbucket,
            project: "workspace/repo".to_string(),
            base_url: None,
            auth: None,
            authentication: TrackerAuthentication::AuthMissing,
            tags: Vec::new(),
        };
        let ctx =
            PapertrailContext { trackers: vec![binding.clone()], ..PapertrailContext::default() };

        let report = block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Full)).unwrap();
        assert!(report.errors.is_empty(), "an automatic tick must not log the capability gap");
        assert!(report.bindings.is_empty());
        // No attempt is recorded either — the binding was never evaluated for dispatch.
        assert_eq!(persisted(&conn, &binding).last_attempt_ms, None);
    }

    #[test]
    fn due_bindings_overlap_network_requests_while_commits_stay_serialized() {
        let conn = open_schema();
        // Stub A holds its FIRST response until stub B has RECEIVED a request: if the two
        // binding jobs ran serially, A's probe would block the whole run and the gate would time
        // out into a 500 the assertions below catch. Overlapping network futures are the only
        // way both probes complete cleanly.
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let (first_url, first_handle) =
            spawn_script_stub_coordinated(probe_script(), None, Some(gate_rx));
        let (second_url, second_handle) =
            spawn_script_stub_coordinated(probe_script(), Some(gate_tx), None);
        let first = github_binding("a/one", &first_url);
        let second = github_binding("b/two", &second_url);
        let now = now_ms();
        for binding in [&first, &second] {
            seed_quiet_cursor(&conn, binding);
            seed_health(&conn, binding, &SeededHealth {
                attempt: now - HOUR_MS,
                probe: now - HOUR_MS,
                mirror: now - HOUR_MS,
                full: now - 2 * HOUR_MS,
            });
        }
        let ctx = PapertrailContext {
            trackers: vec![first.clone(), second.clone()],
            ..PapertrailContext::default()
        };

        let report =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Evaluate)).unwrap();
        assert!(
            report.errors.is_empty(),
            "serial dispatch would gate-timeout: {:?}",
            report.errors
        );
        assert_eq!(report.bindings.len(), 2);
        assert!(report.bindings.iter().all(|binding| binding.probe_not_modified));
        for binding in [&first, &second] {
            assert!(persisted(&conn, binding).last_successful_probe_ms.unwrap() >= now);
        }
        assert_eq!(first_handle.join().unwrap().len(), 3);
        assert_eq!(second_handle.join().unwrap().len(), 3);
    }

    /// A walk interrupted by a provider failure persists its cursor and failure class; the next
    /// due dispatch resumes from that cursor and completes the walk, clearing the failure.
    #[test]
    fn interrupted_walk_resumes_from_the_persisted_cursor_on_the_next_dispatch() {
        let conn = open_schema();
        // First dispatch: the first logical backfill page lands completely (issue leg with one
        // item, its comment thread, then the empty pull leg), and the SECOND page's first
        // request explodes — the walk is interrupted at a clean page boundary with its low mark
        // persisted.
        let (first_url, first_handle) = spawn_script_stub(vec![
            StubResponse::ok(
                r#"{"incomplete_results":false,"items":[{"number":1,"html_url":"https://example.test/o/r/issues/1","state":"open","title":"one","body":"","updated_at":"2026-01-01T00:00:00Z","labels":[]}]}"#,
            ),
            StubResponse::ok("[]"),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::status("500 Internal Server Error", "boom"),
        ]);
        let binding = github_binding("o/r", &first_url);
        let ctx =
            PapertrailContext { trackers: vec![binding.clone()], ..PapertrailContext::default() };
        let report =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Incremental)).unwrap();
        assert_eq!(report.errors.len(), 1, "the interruption surfaces as a binding error");
        let interrupted = persisted(&conn, &binding);
        // The first walk of a fresh binding is a FULL walk; its interruption must stay a Full
        // decision (never degrade to incremental) so the healing walk actually completes.
        assert_eq!(interrupted.continuation, MirrorContinuation::Full);
        assert_eq!(first_handle.join().unwrap().len(), 4);

        // The minimum attempt interval gates the immediate retry...
        let gated =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Incremental)).unwrap();
        assert!(gated.bindings.is_empty() && gated.errors.is_empty());

        // ...and once it elapses, the next dispatch resumes the descent from the persisted
        // low mark and completes the full walk, clearing the failure.
        conn.execute(
            "UPDATE papertrail_sync_cursor SET last_attempt_ms = last_attempt_ms - 1200000",
            [],
        )
        .unwrap();
        // A full-rewalk continuation skips the probe entirely: the resumed descent continues
        // below the persisted low mark (empty issue + pull legs), then the repo-comment streams
        // close the walk out.
        let (resume_url, resume_handle) = spawn_script_stub(vec![
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok("[]"),
            StubResponse::ok("[]"),
        ]);
        let resumed_binding = github_binding("o/r", &resume_url);
        let ctx = PapertrailContext {
            trackers: vec![resumed_binding.clone()],
            ..PapertrailContext::default()
        };
        let report =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Evaluate)).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.bindings.len(), 1);
        assert!(report.bindings[0].completed_full_walk, "the resumed descent completes the walk");
        let healed = persisted(&conn, &resumed_binding);
        // Regression guard: a walk completing on a CHAINED empty provider leg must clear the
        // consumed page cursor, or the policy misreads the completed walk as interrupted work
        // forever (perpetual incremental dispatches instead of probes).
        assert_eq!(healed.continuation, MirrorContinuation::None);
        assert!(healed.last_full_walk_ms.is_some());
        assert_eq!(resume_handle.join().unwrap().len(), 4);
        // The item stored before the interruption was marked seen by the SAME walk, so the
        // completing rewalk's prune keeps it.
        let repo_id = schema::active_repo_id(&conn).unwrap();
        let items: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM papertrail_items WHERE repo_id = ?1",
                params![repo_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(items, 1);
        let (_, error, _, _) = load_persisted_health(&conn, &repo_id, &resumed_binding).unwrap();
        assert_eq!(error, None, "a completed walk clears the persisted failure");
    }

    /// A quota-reserve pause persists a retry horizon; later evaluations skip the binding until
    /// the horizon passes — even under an explicit Full request — and the pause is reported as a
    /// paused binding, never as a failure.
    #[test]
    fn rate_paused_binding_persists_a_retry_horizon_that_gates_later_dispatches() {
        let conn = open_schema();
        let reset_epoch_s = now_ms() / 1000 + 3600;
        // Quota is governed per LANE: the search-lane backfill pages are unaffected, but the
        // item's comment-thread response (core lane) reports the user reserve reached
        // (30 <= 35% of 100), so the next CORE request — the repo-comment stream — pauses
        // until the provider reset.
        let (url, handle) = spawn_script_stub(vec![
            StubResponse::ok(
                r#"{"incomplete_results":false,"items":[{"number":1,"html_url":"https://example.test/o/r/issues/1","state":"open","title":"one","body":"","updated_at":"2026-01-01T00:00:00Z","labels":[]}]}"#,
            ),
            StubResponse::ok_with_quota("[]", 100, 30, reset_epoch_s),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
        ]);
        let binding = github_binding("o/r", &url);
        let ctx =
            PapertrailContext { trackers: vec![binding.clone()], ..PapertrailContext::default() };

        let report =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Incremental)).unwrap();
        assert!(report.errors.is_empty(), "a pause is not a failure: {:?}", report.errors);
        assert_eq!(report.bindings.len(), 1);
        assert_eq!(report.bindings[0].paused_until_ms, Some(reset_epoch_s * 1000));
        assert_eq!(report.bindings[0].stored_items, 1, "work before the pause is kept");
        assert_eq!(handle.join().unwrap().len(), 5, "the pause lands before the next CORE request");
        let paused = persisted(&conn, &binding);
        assert_eq!(paused.retry_not_before_ms, Some(reset_epoch_s * 1000));

        // Past the attempt interval but inside the retry horizon: nothing dispatches, for any
        // request strength. The stub is gone, so an escaped dispatch would surface as an error.
        conn.execute(
            "UPDATE papertrail_sync_cursor SET last_attempt_ms = last_attempt_ms - 1200000",
            [],
        )
        .unwrap();
        for request in [AutosyncRequest::Evaluate, AutosyncRequest::Full] {
            let gated = block_on(sync_mirror_scheduled(&conn, &ctx, request)).unwrap();
            assert!(
                gated.bindings.is_empty() && gated.errors.is_empty(),
                "{request:?} must stay gated by the retry horizon"
            );
        }
    }

    /// A changed tag filter is a change signal in its own right: even with fresh probe and
    /// mirror freshness, the next evaluation dispatches and the mirror restarts its walk under
    /// the new filter (the fingerprint reset is what re-scopes the cache).
    #[test]
    fn filter_fingerprint_change_dispatches_despite_fresh_probe_and_mirror_timestamps() {
        let conn = open_schema();
        // The filter change dispatches an ordinary incremental run; inside it the mirror resets
        // the backfill under the new fingerprint: probe (not modified) -> repo-comment streams
        // -> re-descent from scratch (one page + its thread) -> done.
        let (url, _handle) = spawn_script_stub(vec![
            StubResponse::status("304 Not Modified", ""),
            StubResponse::ok("[]"),
            StubResponse::ok("[]"),
            StubResponse::ok(
                r#"{"incomplete_results":false,"items":[{"number":1,"html_url":"https://example.test/o/r/issues/1","state":"open","title":"bugged","body":"","updated_at":"2026-01-01T00:00:00Z","labels":[{"name":"bug"}]}]}"#,
            ),
            StubResponse::ok("[]"),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
            StubResponse::ok(r#"{"incomplete_results":false,"items":[]}"#),
        ]);
        let mut binding = github_binding("o/r", &url);
        binding.tags = vec!["bug".to_string()];
        let now = now_ms();
        // The stored cursor was fingerprinted under a DIFFERENT (empty) tag filter.
        record_attempt(&conn, &binding, 0).unwrap();
        conn.execute(
            "UPDATE papertrail_sync_cursor
             SET backfill_done=1, high_mark_at=?1, filter_fingerprint=''",
            params![HIGH_MARK],
        )
        .unwrap();
        seed_health(&conn, &binding, &SeededHealth {
            attempt: now - HOUR_MS,
            probe: now - 60_000,
            mirror: now - 60_000,
            full: now - HOUR_MS,
        });
        let ctx =
            PapertrailContext { trackers: vec![binding.clone()], ..PapertrailContext::default() };

        let report =
            block_on(sync_mirror_scheduled(&conn, &ctx, AutosyncRequest::Evaluate)).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.bindings.len(), 1, "the filter change alone must dispatch");
        assert!(
            report.bindings[0].completed_full_walk,
            "a re-scoped filter restarts and completes the walk"
        );
    }
}
