use super::*;
use crate::index::{repo_meta, schema, scoped_table_row_count, set_repo_meta};

pub(crate) async fn sync_mirror(
    conn: &Connection,
    root: &Path,
    full: bool,
    ctx: &PapertrailContext,
) -> anyhow::Result<PapertrailSyncReport> {
    ensure_unique_mirror_bindings(&ctx.trackers)?;
    let refs = discover_and_store_refs(conn, root, ctx)?;
    let registry = transport::GovernorRegistry::default();
    let options = ctx.transport_options.clone();
    let mut synced_items = 0;
    let mut bindings = Vec::new();
    let mut errors = Vec::new();
    for binding in &ctx.trackers {
        let attempted_at = now_ms();
        record_attempt(conn, binding, attempted_at)?;
        if binding.provider != Tracker::Github {
            errors.push(PapertrailSyncError {
                tracker: binding.provider,
                project: binding.project.clone(),
                item_key: String::new(),
                status: "provider_client_pending".to_string(),
                error: format!(
                    "{} mirror client is not implemented yet",
                    binding.provider.as_db_str()
                ),
            });
            continue;
        }
        match GitHubClient::new(binding, &registry, options.clone()) {
            Ok(client) => match mirror_binding(conn, binding, &client, full).await {
                Ok(report) => {
                    synced_items += report.stored_items;
                    if let Some(operation) = completed_mirror_operation(&report, full) {
                        record_success(conn, binding, operation, now_ms())?;
                    } else if let Some(resume_at_ms) = report.paused_until_ms {
                        record_pause(conn, binding, resume_at_ms)?;
                    }
                    bindings.push(report);
                },
                Err(error) => {
                    record_failure(
                        conn,
                        binding,
                        PapertrailErrorClass::Provider,
                        Some(&error.to_string()),
                    )?;
                    errors.push(PapertrailSyncError {
                        tracker: binding.provider,
                        project: binding.project.clone(),
                        item_key: String::new(),
                        status: "failed".to_string(),
                        error: error.to_string(),
                    });
                },
            },
            Err(error) => {
                let persisted_detail = authentication_failure_detail(binding, &error);
                record_failure(
                    conn,
                    binding,
                    PapertrailErrorClass::Authentication,
                    persisted_detail.as_deref(),
                )?;
                errors.push(PapertrailSyncError {
                    tracker: binding.provider,
                    project: binding.project.clone(),
                    item_key: String::new(),
                    status: "authentication_or_transport".to_string(),
                    error: error.to_string(),
                });
            },
        }
    }
    let repo_id = schema::active_repo_id(conn)?;
    set_repo_meta(conn, &repo_id, "papertrail_last_sync_ms", &now_ms().to_string())?;
    Ok(PapertrailSyncReport {
        offline: false,
        discovered_refs: refs.len(),
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

fn completed_mirror_operation(
    report: &MirrorBindingReport,
    full: bool,
) -> Option<SuccessfulOperation> {
    if report.paused_until_ms.is_some() {
        return None;
    }
    Some(if full || report.completed_full_walk {
        SuccessfulOperation::FullMirror
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
        Tracker::Github => TrackerSynchronization::Native,
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
        };
        assert_eq!(
            completed_mirror_operation(&report, false),
            Some(SuccessfulOperation::FullMirror)
        );
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
                tracker(Tracker::Gitlab, "group/repo", Some("https://gitlab.com".to_string())),
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
        let pending =
            report.status.bindings.iter().find(|binding| binding.project == "group/repo").unwrap();
        assert!(!pending.failed);
        assert!(!pending.overdue);
        assert_eq!(pending.error_class, None);
    }
}
