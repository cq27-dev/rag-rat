use rag_rat_oracle::{CheckoutScope, IndexedCorpus, LivePassAbort};

use super::*;

fn backend(tool: rag_rat_oracle::OracleTool) -> LiveBackend {
    LiveBackend::for_tool(tool).expect("a live backend")
}

/// A tail holding one checkout, for the tests that exercise BACKEND-level behaviour (backlog,
/// backoff, wake scheduling). Those properties are per backend within a checkout and are
/// unchanged by the per-checkout split.
fn single_checkout_tail(now: Instant) -> LiveOracleTail {
    LiveOracleTail {
        checkouts: vec![CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now)],
    }
}

fn backends_of(tail: &mut LiveOracleTail) -> &mut Vec<LiveBackendTail> {
    &mut tail.checkouts[0].backends
}

#[test]
fn worklist_dedupes_backlog_against_changed_and_filters_other_languages() {
    let rust = backend(rag_rat_oracle::OracleTool::RaLsp);
    let backlog = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
    let changed =
        BTreeSet::from(["src/b.rs".to_string(), "src/c.rs".to_string(), "Cargo.toml".to_string()]);
    let worklist = assemble_worklist(backlog, &mut BTreeSet::new(), Some(&changed), &rust);
    // Backlog order first, then new changed paths; duplicates collapse; non-Rust dropped.
    assert_eq!(worklist, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
}

#[test]
fn worklist_without_changed_set_rides_backlog_only() {
    let rust = backend(rag_rat_oracle::OracleTool::RaLsp);
    let backlog = vec!["src/a.rs".to_string()];
    // A heal/bootstrap pass (None) contributes no paths.
    assert_eq!(assemble_worklist(backlog, &mut BTreeSet::new(), None, &rust), vec!["src/a.rs"]);
    assert!(assemble_worklist(Vec::new(), &mut BTreeSet::new(), None, &rust).is_empty());
}

#[test]
fn each_backend_claims_only_its_own_languages_changed_paths() {
    // One changed set, several backends: a `.ts` file reaching the Rust session would be
    // opened under the wrong languageId and burn budget on a file that server cannot
    // resolve, and vice versa.
    let changed = BTreeSet::from([
        "src/a.rs".to_string(),
        "src/b.ts".to_string(),
        "src/c.tsx".to_string(),
        "README.md".to_string(),
    ]);
    assert_eq!(
        assemble_worklist(
            Vec::new(),
            &mut BTreeSet::new(),
            Some(&changed),
            &backend(rag_rat_oracle::OracleTool::RaLsp)
        ),
        vec!["src/a.rs"],
    );
    assert_eq!(
        assemble_worklist(
            Vec::new(),
            &mut BTreeSet::new(),
            Some(&changed),
            &backend(rag_rat_oracle::OracleTool::TsLsp)
        ),
        vec!["src/b.ts", "src/c.tsx"],
    );
}

#[test]
fn the_tail_wakes_for_the_earliest_backend_that_needs_one() {
    // A single pass services every backend, so the tail's deadline is the minimum across
    // them — a backend with a backlog must not wait for another backend's longer idle timer.
    let now = Instant::now();
    let mut tail = single_checkout_tail(now);
    assert!(backends_of(&mut tail).len() >= 2, "the multi-backend case must actually be exercised");
    let idle = Duration::from_secs(600);
    // One backend holds a backlog (retry cadence); another holds an idle session.
    backends_of(&mut tail)[0].backlog.push("src/a.rs".to_string());
    backends_of(&mut tail)[1].lifecycle.on_spawned(now);
    let earliest = backends_of(&mut tail)
        .iter()
        .filter_map(|backend| backend.next_wake_in(idle, now))
        .min()
        .expect("at least one backend schedules a wake");
    assert_eq!(earliest, LIVE_ORACLE_RETRY_INTERVAL, "the backlog's retry wins over idle");
}

#[test]
fn the_claim_order_rotates_so_no_backend_starves_the_shared_budget() {
    // `max_requests_per_pass` bounds the whole pass, so the backends share one allowance. If
    // the order were fixed, a language whose change set always exhausts it would keep every
    // other language's backlog permanently unserviced.
    let order = |first| claim_order(3, first).collect::<Vec<_>>();
    assert_eq!(order(0), vec![0, 1, 2]);
    assert_eq!(order(1), vec![1, 2, 0]);
    assert_eq!(order(2), vec![2, 0, 1]);
    // Every backend is still visited exactly once per pass, whatever the rotation.
    assert_eq!(order(7).len(), 3);
    assert_eq!(order(7).iter().collect::<HashSet<_>>().len(), 3);
    // A wrapped counter must not panic or skip anyone.
    assert_eq!(claim_order(2, usize::MAX).collect::<Vec<_>>(), vec![1, 0]);
    assert_eq!(claim_order(0, 5).count(), 0, "no backends is not a division by zero");
}

#[test]
fn a_server_that_never_warms_is_reported_once_then_stays_quiet() {
    // Refusing to ask a warming server is correct but SILENT — on its own it is
    // indistinguishable from the backend working, and the backlog just rides forever. The
    // watcher has to say so, once, and stop as soon as the backend gets anywhere.
    let mut tail =
        LiveBackendTail::new(LiveBackend::for_tool(rag_rat_oracle::OracleTool::TsLsp).unwrap());
    let warming =
        LivePassReport { status: rag_rat_oracle::RunStatus::Warming, ..LivePassReport::default() };
    for _ in 0..WARMING_PASSES_BEFORE_REPORT - 1 {
        tail.note_warming(&warming);
        assert!(!tail.warming_reported, "a normally-warming server must stay quiet");
    }
    tail.note_warming(&warming);
    assert!(tail.warming_reported, "a server that never warms must be reported");
    tail.note_warming(&warming);
    assert!(tail.warming_reported, "reported ONCE, not on every later pass");

    // Any progress at all clears the streak, so a later cold start reports afresh.
    tail.note_warming(&LivePassReport {
        status: rag_rat_oracle::RunStatus::Completed,
        ..LivePassReport::default()
    });
    assert_eq!(tail.warming_passes, 0);
    assert!(!tail.warming_reported);
}

/// A `MakeWriter` that appends every formatted log line into a shared buffer, so a test can
/// assert on the `tracing` events a pass actually emitted — and on how many times.
#[derive(Clone)]
struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `body` with warnings captured, returning what it logged. The subscriber is thread-local
/// (`with_default`), so parallel tests do not see each other's output.
fn captured_warnings(body: impl FnOnce()) -> String {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(CaptureWriter(std::sync::Arc::clone(&buffer)))
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let logged = buffer.lock().unwrap().clone();
    String::from_utf8(logged).expect("formatted log lines are UTF-8")
}

#[test]
fn a_backend_that_can_configure_nothing_is_reported_once_then_stays_quiet() {
    // A candidate skipped because the session cannot configure its file is deliberately NOT
    // deferred — retrying cannot help until the checkout's layout changes. So a pass made
    // entirely of such skips writes no rows and leaves no backlog, and without a report of its
    // own the backend resolves nothing pass after pass while saying nothing at all.
    let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
    let all_skipped = || LivePassReport { skipped_unconfigured: 3, ..LivePassReport::default() };
    let occurrences = |logged: &str| logged.matches("cannot configure their files").count();

    let logged = captured_warnings(|| {
        tail.note_unconfigured(&all_skipped());
        tail.note_unconfigured(&all_skipped());
    });
    assert_eq!(occurrences(&logged), 1, "reported ONCE, not on every later pass: {logged:?}");

    // A pass that issues a request proves the session configures something here, so the
    // report is not repeated for it…
    let quiet = captured_warnings(|| {
        tail.note_unconfigured(&LivePassReport {
            requests_used: 1,
            skipped_unconfigured: 1,
            ..LivePassReport::default()
        });
    });
    assert_eq!(occurrences(&quiet), 0, "a pass that resolves anything is not a dry spell");
    // …and the streak is cleared, so a later all-skipped pass reports afresh.
    let again = captured_warnings(|| tail.note_unconfigured(&all_skipped()));
    assert_eq!(occurrences(&again), 1, "a new dry spell must be reported: {again:?}");
}

#[test]
fn an_unconfigured_warning_names_each_database_cause() {
    struct OneSourceCorpus;

    impl IndexedCorpus for OneSourceCorpus {
        fn indexes_file(&self, absolute: &std::path::Path) -> bool {
            absolute.ends_with("src/main.c")
        }

        fn may_hold_indexed_files(&self, _dir: &std::path::Path) -> bool {
            true
        }
    }

    let warning = |report: LivePassReport| {
        let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
        captured_warnings(|| tail.note_unconfigured(&report))
    };

    // No database fact set: the terminal branch. It must OFFER its causes rather than pick
    // one — the set it covers is the complement of the branches above, so it cannot know.
    let unidentified =
        warning(LivePassReport { skipped_unconfigured: 1, ..LivePassReport::default() });
    assert!(
        unidentified.contains("do not identify which cause"),
        "the terminal branch must not assert a diagnosis: {unidentified:?}",
    );
    // Both remedies are present and BOTH are conditional. A terminal branch that named one
    // cause would drop a conditional, which is what these two read.
    assert!(
        unidentified.contains("if it holds several"),
        "the several-databases remedy must stay conditional: {unidentified:?}",
    );
    assert!(
        unidentified.contains("If it holds exactly one"),
        "and so must the sole-database one: {unidentified:?}",
    );
    assert!(
        unidentified.contains("leave a single compilation database"),
        "…while still carrying the several-databases remedy: {unidentified:?}",
    );
    assert!(
        unidentified.contains("a `command` or `arguments` a compiler would accept"),
        "…and a sole-database remedy that does not presume which field is wrong: {unidentified:?}",
    );
    // Not even parsing may be asserted: an incomplete scan withholds the unreadable-database
    // fact, so a checkout whose only database could not be read reaches this branch too.
    assert!(
        unidentified.contains("may not have parsed"),
        "the terminal branch cannot claim the database parsed: {unidentified:?}",
    );
    assert!(!unidentified.contains("names no file this checkout indexes"));

    let governs_nothing = warning(LivePassReport {
        skipped_unconfigured: 1,
        database_governs_nothing: true,
        ..LivePassReport::default()
    });
    assert!(
        governs_nothing.contains("names no file this checkout indexes"),
        "a non-governing database needs its own remedy: {governs_nothing:?}",
    );
    assert!(!governs_nothing.contains("leave a single compilation database"));

    let fixture = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(fixture.path().join("src")).unwrap();
    std::fs::write(fixture.path().join("src/main.c"), "int main(void) { return 0; }\n").unwrap();
    std::fs::write(
        fixture.path().join("compile_commands.json"),
        "[\n  # generated by hand\n  {\"directory\":\"/x\",\"file\":\"/x/a.c\",\"command\":\"cc \
         -c a.c\"},\n]\n",
    )
    .unwrap();
    let corpus = OneSourceCorpus;
    let scope = CheckoutScope::resolve(fixture.path(), &corpus);
    let clangd = backend(rag_rat_oracle::OracleTool::ClangdLsp);
    let layout = clangd.resolve_layout(&scope);
    assert!(
        clangd.checkout_can_signal_readiness(&scope, &layout),
        "a sole YAML-flavoured database is warmable even though this reader cannot parse it",
    );
    assert!(
        !clangd.session_can_resolve(&scope, "src/main.c", &layout),
        "the unreadable marker is not trusted for persisted resolutions while it is being \
         diagnosed",
    );

    let unreadable = warning(LivePassReport {
        skipped_unconfigured: 1,
        database_unreadable: layout.has_unreadable_database(),
        ..LivePassReport::default()
    });
    assert!(
        unreadable.contains("compile_commands.json"),
        "a sole unreadable database warning must identify its file: {unreadable:?}",
    );
    assert!(unreadable.contains("unsupported syntax"));
    assert!(unreadable.contains("unsupported entry key"));
    assert!(!unreadable.contains("leave a single compilation database"));
    assert!(!unreadable.contains("names no file this checkout indexes"));

    std::fs::write(
        fixture.path().join("compile_commands.json"),
        r#"[{"directory":"/x","file":"/x/a.c","command":"cc -c a.c","extra":"x"}]"#,
    )
    .unwrap();
    let strict_json_unknown_key_layout = clangd.resolve_layout(&scope);
    assert!(
        strict_json_unknown_key_layout.has_unreadable_database(),
        "a strict-JSON entry with an unknown key still yields the Unknown layout fact",
    );
    let strict_json_unknown_key = warning(LivePassReport {
        skipped_unconfigured: 1,
        database_unreadable: strict_json_unknown_key_layout.has_unreadable_database(),
        ..LivePassReport::default()
    });
    assert!(
        strict_json_unknown_key.contains("may be unreadable"),
        "strict JSON with an unknown key parses, so unreadability may only be OFFERED as a cause, \
         never asserted: {strict_json_unknown_key:?}",
    );
    assert!(
        strict_json_unknown_key.contains("compile_commands.json"),
        "the warning must identify the database file: {strict_json_unknown_key:?}",
    );
    assert!(
        strict_json_unknown_key.contains("unsupported entry key"),
        "the warning must describe the accepted-problem class: {strict_json_unknown_key:?}",
    );

    // An existing marker path is recorded even when it cannot be opened. A directory with
    // the marker's name reaches the same Unknown layout fact as a file-open failure, so the
    // warning must not claim that its contents have either of the reader-level problems.
    std::fs::remove_file(fixture.path().join("compile_commands.json")).unwrap();
    std::fs::create_dir(fixture.path().join("compile_commands.json")).unwrap();
    let unreadable_marker_layout = clangd.resolve_layout(&scope);
    assert!(
        unreadable_marker_layout.has_unreadable_database(),
        "an existing but unopenable marker still reaches the unreadable-database branch",
    );
    let unreadable_marker = warning(LivePassReport {
        skipped_unconfigured: 1,
        database_unreadable: unreadable_marker_layout.has_unreadable_database(),
        ..LivePassReport::default()
    });
    assert!(
        unreadable_marker.contains("compile_commands.json"),
        "the warning must identify an unreadable marker path: {unreadable_marker:?}",
    );
    assert!(
        unreadable_marker.contains("could not be used"),
        "the warning must state the reader could not use the marker: {unreadable_marker:?}",
    );
    assert!(
        unreadable_marker.contains("may be unreadable"),
        "the warning must present unreadability as a possible cause: {unreadable_marker:?}",
    );

    // The shape that made the terminal branch lie: a SOLE database whose entries parse, so
    // the unreadable branch declines it, and whose `file` this reader cannot read as a path,
    // so `Governs::Unknown` makes the governs-nothing branch decline it too. It falls
    // through — and the fallthrough used to tell an operator with one database to leave a
    // single one.
    std::fs::remove_dir(fixture.path().join("compile_commands.json")).unwrap();
    std::fs::write(
        fixture.path().join("compile_commands.json"),
        r#"[{"directory":"/x","file":42,"command":"cc -c a.c"}]"#,
    )
    .unwrap();
    let non_string_file = clangd.resolve_layout(&scope);
    assert!(
        !non_string_file.has_unreadable_database(),
        "entries that parse are not an unreadable database",
    );
    assert!(
        !non_string_file.has_database_governing_nothing_indexed(),
        "a `file` this reader cannot read is unknown governance, not proven non-governance",
    );
    let sole_unreadable_entry = warning(LivePassReport {
        skipped_unconfigured: 1,
        database_unreadable: non_string_file.has_unreadable_database(),
        database_governs_nothing: non_string_file.has_database_governing_nothing_indexed(),
        ..LivePassReport::default()
    });
    assert!(
        sole_unreadable_entry.contains("do not identify which cause"),
        "a sole database that reaches the terminal branch must not be diagnosed as several: \
         {sole_unreadable_entry:?}",
    );

    // A sole database that PARSES and is still unusable — a string `file` naming an indexed
    // source, with an empty `command` — reaches the terminal branch too: entries counted zero
    // makes it NotLoadable, so the unreadable branch declines it, and NotLoadable is not the
    // Loadable the governs-nothing branch requires. Nothing about this entry's `file` is
    // wrong, so a remedy naming that field would send its operator to the wrong line.
    std::fs::write(
        fixture.path().join("compile_commands.json"),
        format!(
            "[{{\"directory\":\"/x\",\"file\":{:?},\"command\":\"\"}}]",
            fixture.path().join("src/main.c").display().to_string()
        ),
    )
    .unwrap();
    let empty_command = clangd.resolve_layout(&scope);
    assert!(!empty_command.has_unreadable_database());
    assert!(!empty_command.has_database_governing_nothing_indexed());
    let unusable_entry = warning(LivePassReport {
        skipped_unconfigured: 1,
        database_unreadable: empty_command.has_unreadable_database(),
        database_governs_nothing: empty_command.has_database_governing_nothing_indexed(),
        ..LivePassReport::default()
    });
    assert!(
        unusable_entry.contains("do not identify which cause"),
        "a database whose entries are malformed some other way is still undiagnosed: \
         {unusable_entry:?}",
    );
    assert!(
        unusable_entry.contains("a `command` or `arguments` a compiler would accept"),
        "and the remedy must reach the field that IS wrong: {unusable_entry:?}",
    );

    // A checkout that holds NO database never reaches this warning: it cannot signal
    // readiness, so it blocks on the prerequisite instead of spawning, and a running session
    // whose checkout loses every database ends its pass at the layout-refresh check. The
    // prerequisite already words that remedy; a branch here would be unreachable.
    std::fs::remove_file(fixture.path().join("compile_commands.json")).unwrap();
    let no_database = clangd.resolve_layout(&scope);
    assert!(no_database.has_no_database(), "the checkout holds no database to find");
    assert!(
        !clangd.checkout_can_signal_readiness(&scope, &no_database),
        "so no session spawns against it, and no pass can report it",
    );
}

/// A pass that skipped `paths` because the session could not configure their files.
fn all_skipped_report(paths: &[String]) -> LivePassReport {
    LivePassReport {
        skipped_unconfigured: paths.len() as u64,
        skipped_unconfigured_paths: paths.to_vec(),
        ..LivePassReport::default()
    }
}

/// A pass that ended early for `abort` without reaching any file.
fn aborted_report(abort: LivePassAbort) -> LivePassReport {
    LivePassReport { abort: Some(abort), ..LivePassReport::default() }
}

#[test]
fn a_path_the_session_cannot_configure_is_retained_without_scheduling_another_pass() {
    // The skip is deliberately not deferred, and a non-empty backlog is exactly what makes the
    // watcher schedule another pass — so parking these in the backlog would spin it forever on
    // work every pass can only skip again. They still have to be kept somewhere, or the layout
    // change that makes them resolvable has nothing to bring back.
    let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
    let worklist = vec!["b/main.c".to_string()];
    tail.retain_unconfigured(&worklist, &all_skipped_report(&worklist));

    assert!(tail.backlog.is_empty(), "a permanently-skipped path must not ride the backlog");
    assert_eq!(tail.unconfigured_paths, BTreeSet::from(["b/main.c".to_string()]));
    assert_eq!(
        tail.next_wake_in(Duration::from_secs(600), Instant::now()),
        None,
        "what is retained here must not schedule a pass on its own",
    );

    // Deduped across passes: re-editing the same unconfigurable file cannot grow the set.
    tail.retain_unconfigured(&worklist, &all_skipped_report(&worklist));
    assert_eq!(tail.unconfigured_paths.len(), 1);

    // A pass that carries the path and does NOT skip it drops it again — whatever it is now,
    // it is no longer a file waiting on a layout change.
    tail.retain_unconfigured(&worklist, &LivePassReport::default());
    assert!(tail.unconfigured_paths.is_empty());
}

#[test]
fn a_parked_path_rides_along_with_real_work_but_never_causes_a_pass() {
    // The operator fix this exists for — consolidating the checkout's compilation databases —
    // changes no file in this backend's languages, so it can never build a worklist of its own.
    // Waiting for a pass to ABORT on the layout change is too narrow a trigger: an ordinary
    // pass re-answers "can I configure this?" against a freshly resolved layout just as well,
    // and costs nothing extra because an unconfigurable path is skipped before the request
    // budget is touched.
    let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
    let parked = vec!["b/main.c".to_string()];
    tail.retain_unconfigured(&parked, &all_skipped_report(&parked));

    // Alone, it still schedules nothing — parking these in the backlog would spin the watcher
    // forever on work every pass can only re-skip. Driven through `assemble_worklist`, which
    // is the ONE place a pass's worklist is built, so this covers the wiring and not just a
    // helper the pass might not call.
    let empty = assemble_worklist(
        Vec::new(),
        &mut tail.unconfigured_paths,
        Some(&BTreeSet::new()),
        &tail.backend,
    );
    assert!(empty.is_empty(), "a parked path must not manufacture a worklist");
    assert_eq!(tail.unconfigured_paths.len(), 1, "…and must not be consumed by trying");
    assert_eq!(
        tail.next_wake_in(Duration::from_secs(600), Instant::now()),
        None,
        "what is parked here must not schedule a pass on its own",
    );

    // But any pass that is happening anyway carries it.
    let changed = BTreeSet::from(["a/other.c".to_string()]);
    let worklist =
        assemble_worklist(Vec::new(), &mut tail.unconfigured_paths, Some(&changed), &tail.backend);
    assert_eq!(worklist, vec!["a/other.c".to_string(), "b/main.c".to_string()]);
    assert!(tail.unconfigured_paths.is_empty(), "drained into the worklist, not copied");

    // Still unconfigurable → parked again, and the cycle settles rather than growing.
    tail.retain_unconfigured(&worklist, &all_skipped_report(&parked));
    assert_eq!(tail.unconfigured_paths, BTreeSet::from(["b/main.c".to_string()]));

    // Resolvable now → the pass carries it without skipping, and it is simply gone.
    let worklist =
        assemble_worklist(Vec::new(), &mut tail.unconfigured_paths, Some(&changed), &tail.backend);
    tail.retain_unconfigured(&worklist, &LivePassReport::default());
    assert!(tail.unconfigured_paths.is_empty());
    assert!(tail.backlog.is_empty(), "resolving a parked path leaves nothing behind");
}

#[test]
fn a_parked_path_is_not_dropped_by_an_abort_that_never_reached_it() {
    // An abort requeues the whole worklist through `unfinished_paths`, so a parked path the
    // pass was carrying rides the backlog rather than the parked set. It must not be lost in
    // the handover: `retain_unconfigured` clears carried paths, and the backlog is what brings
    // this one back.
    let mut tail = LiveBackendTail::new(backend(rag_rat_oracle::OracleTool::ClangdLsp));
    let parked = vec!["b/main.c".to_string()];
    tail.retain_unconfigured(&parked, &all_skipped_report(&parked));

    let changed = BTreeSet::from(["a/other.c".to_string()]);
    let worklist =
        assemble_worklist(Vec::new(), &mut tail.unconfigured_paths, Some(&changed), &tail.backend);
    let mut aborted = aborted_report(LivePassAbort::Server);
    aborted.unfinished_paths = worklist.clone();
    tail.backlog = aborted.unfinished_paths.clone();
    tail.retain_unconfigured(&worklist, &aborted);

    assert!(
        tail.backlog.contains(&"b/main.c".to_string()),
        "a parked path the pass never reached must survive the abort: {:?}",
        tail.backlog,
    );
    assert!(tail.unconfigured_paths.is_empty(), "it is in the backlog, not parked twice");
}

/// A tail carrying two checkouts, both with a backlog so both count as having work.
fn two_checkout_tail(now: Instant) -> LiveOracleTail {
    let mut tail = LiveOracleTail {
        checkouts: vec![
            CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
            CheckoutTail::new("/wt/feat".to_string(), std::path::PathBuf::from("/wt/feat"), now),
        ],
    };
    for checkout in &mut tail.checkouts {
        checkout.backends[0].backlog.push("src/a.rs".to_string());
    }
    tail
}

/// A minimal enabled-live config. These tests exercise the checkout bookkeeping, which reads
/// only `oracle.live` and `root`.
fn live_config(max_checkouts: usize) -> Config {
    let mut config = Config {
        trackers: Vec::new(),
        papertrail: Default::default(),
        sync: Default::default(),
        repo_id_override: None,
        database_key_pinned: true,
        database: std::path::PathBuf::from("/repo/.rag-rat/index.sqlite"),
        root: std::path::PathBuf::from("/repo"),
        targets: vec![rag_rat_base::config::ResolvedTarget {
            name: "rust".to_string(),
            language: rag_rat_base::language::Language::Rust,
            directories: vec![std::path::PathBuf::from("src")],
            include: vec!["**/*.rs".to_string()],
            exclude: Vec::new(),
            kind: rag_rat_base::config::TargetKind::Source,
        }],
        llm: Default::default(),
        watch: Default::default(),
        version_check: Default::default(),
        mcp: Default::default(),
        oracle: Default::default(),
        search: Default::default(),
        memory: Default::default(),
        log: Default::default(),
        source_root_reanchored_from: None,
        allow_empty: false,
    };
    config.oracle.live.enabled = true;
    config.oracle.live.max_checkouts = max_checkouts;
    config
}

#[test]
fn the_checkout_cap_serves_the_most_recently_worked_checkout_and_keeps_the_others_backlog() {
    // A resident language server is the expensive thing here — routinely gigabytes — and one
    // per (backend x checkout) multiplies that by the worktree fleet. The cap bounds SERVERS,
    // so a checkout outside it stops holding sessions but must not lose its work (#1010).
    let now = Instant::now();
    let mut tail = two_checkout_tail(now);
    // The linked checkout worked more recently than the base one.
    tail.checkouts[0].last_worked_at = now - Duration::from_secs(60);
    tail.checkouts[1].last_worked_at = now;

    tail.checkouts[1].had_new_work = true;

    let ranked = tail.rank();

    assert_eq!(ranked.first(), Some(&1), "the checkout being edited now leads: {ranked:?}",);
    assert_eq!(ranked.last(), Some(&0), "the older one follows it: {ranked:?}");
    assert_eq!(
        tail.checkouts[0].backends[0].backlog,
        vec!["src/a.rs".to_string()],
        "an unserved checkout defers its work — it must never be dropped",
    );
}

#[test]
fn a_checkout_the_cap_excludes_still_retains_this_passs_new_paths() {
    // The cap decides which checkouts are SERVED, never which are remembered. Retaining work
    // only inside the per-backend pass — which an unserved checkout never reaches — silently
    // discarded edits made in whichever checkout happened to fall outside the cap, with
    // nothing left to retry them (#1010).
    let now = Instant::now();
    let mut tail = LiveOracleTail {
        checkouts: vec![
            CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
            CheckoutTail::new("/wt/feat".to_string(), std::path::PathBuf::from("/wt/feat"), now),
        ],
    };
    let config = live_config(1);
    let base = BTreeSet::from(["src/base.rs".to_string()]);
    let overlays = std::collections::BTreeMap::from([(
        "/wt/feat".to_string(),
        crate::watch::CheckoutReindex {
            source_root: std::path::PathBuf::from("/wt/feat"),
            paths: vec![std::path::PathBuf::from("src/linked.rs")],
            coverage: crate::index::ChangedPathsCoverage::Complete,
        },
    )]);

    tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &overlays }, now);
    // Both are eligible for a turn — the cap counts turns in the serve loop, not here...
    assert_eq!(tail.rank().len(), 2, "both checkouts have work to run");
    // ...and whichever the cap leaves out, BOTH kept their paths.
    assert_eq!(
        backlog_union(&tail.checkouts[0]),
        vec!["src/base.rs".to_string()],
        "the base checkout retained its path",
    );
    assert_eq!(
        backlog_union(&tail.checkouts[1]),
        vec!["src/linked.rs".to_string()],
        "the excluded checkout retained its path too — deferred, not dropped",
    );
}

#[test]
fn a_waiting_checkout_is_admitted_ahead_of_one_that_has_already_been_served() {
    // Neither checkout has new work, so both are merely waiting. Ranking on "still has a
    // backlog" would stamp both with the same instant every pass and fall through to a
    // tie-break that rewards holding a session — pinning the slot to the incumbent forever.
    // The one kept out longest must go first (#1010).
    let now = Instant::now();
    let mut tail = two_checkout_tail(now);
    for checkout in &mut tail.checkouts {
        checkout.had_new_work = false;
    }
    // The incumbent was edited MORE recently and has already been served for it; the other has
    // never run. Service age must still win — comparing work recency first would let the
    // incumbent keep the slot for as long as it stays backlogged (warming, retrying), which is
    // exactly the starvation this ordering exists to prevent.
    tail.checkouts[0].last_worked_at = now;
    tail.checkouts[0].last_served_at = Some(now);
    tail.checkouts[1].last_worked_at = now - Duration::from_secs(600);
    tail.checkouts[1].last_served_at = None;

    let ranked = tail.rank();

    assert_eq!(
        ranked.first(),
        Some(&1),
        "the never-served checkout leads despite the incumbent's newer work: {ranked:?}",
    );
}

#[test]
fn an_empty_base_change_set_does_not_claim_a_checkout_slot() {
    // A pass that touched only a linked worktree still reports `Some(empty)` for the base —
    // the hint means "this is a reliable superset", not "there is something in it". Admitting
    // that as work gave the base checkout a phantom claim: it tied every linked checkout on
    // recency, sorted ahead of them, spent the only slot doing nothing, and was pruned as
    // idle — every pass, so the linked checkout the edit belongs to never ran (#1010).
    let now = Instant::now();
    let mut tail = LiveOracleTail {
        checkouts: vec![
            CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
            CheckoutTail::new("/wt/feat".to_string(), std::path::PathBuf::from("/wt/feat"), now),
        ],
    };
    let config = live_config(1);
    let empty_base = BTreeSet::new();
    let overlays = std::collections::BTreeMap::from([(
        "/wt/feat".to_string(),
        crate::watch::CheckoutReindex {
            source_root: std::path::PathBuf::from("/wt/feat"),
            paths: vec![std::path::PathBuf::from("src/linked.rs")],
            coverage: crate::index::ChangedPathsCoverage::Complete,
        },
    )]);

    tail.absorb(&config, &LiveChangedSets { base: Some(&empty_base), overlays: &overlays }, now);
    let ranked = tail.rank();

    assert!(!tail.checkouts[0].had_new_work, "an empty base set is not work");
    assert!(!ranked.contains(&0), "the idle base checkout is not eligible for a turn: {ranked:?}",);
    assert!(ranked.contains(&1), "the linked checkout that actually changed is: {ranked:?}",);
}

#[test]
fn a_checkout_whose_config_no_longer_indexes_the_language_is_not_admitted() {
    // A branch that DROPS its last target for a live language still reports the pruned source
    // paths as changed, so their extensions still look live. Admitting on the extension alone
    // hands the checkout a slot it cannot use: the backend takes the backlog, returns at its
    // language gate, and the checkout is pruned as idle — while a sibling with real work stays
    // unserved. Admission asks the same question the backend will (#1010).
    let now = Instant::now();
    let mut tail = LiveOracleTail {
        checkouts: vec![CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now)],
    };
    // A `.rs` path — claimed by the Rust backend on extension — but nothing indexed here.
    let mut config = live_config(1);
    config.targets.clear();
    let base = BTreeSet::from(["src/a.rs".to_string()]);
    let no_overlays = std::collections::BTreeMap::new();

    tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &no_overlays }, now);

    assert!(
        !tail.checkouts[0].had_new_work,
        "a path whose language this checkout no longer indexes is not work for it",
    );
    assert!(
        backlog_union(&tail.checkouts[0]).is_empty(),
        "and it is not retained: {:?}",
        backlog_union(&tail.checkouts[0]),
    );
}

#[test]
fn a_path_outside_this_checkouts_targets_is_not_admitted_even_in_a_live_language() {
    // The narrow form of the same bug: a branch drops ONE Rust target and keeps another. The
    // pruned paths still end in `.rs` and a Rust target still exists, so any predicate that
    // tests those two things independently admits the checkout — and it then holds the cap
    // slot, possibly with a resident server, for paths that are not in its corpus at all.
    // Matching each path against its own target is what closes this (#1010).
    let now = Instant::now();
    let mut tail = LiveOracleTail {
        checkouts: vec![CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now)],
    };
    // The fixture indexes `src` only; this path is Rust, but under a directory that is not a
    // target of THIS checkout.
    let config = live_config(1);
    assert!(
        config
            .targets
            .iter()
            .any(|target| target.language == rag_rat_base::language::Language::Rust),
        "a Rust target must still exist, or this tests the wrong thing",
    );
    let base = BTreeSet::from(["extra/dropped.rs".to_string()]);
    let no_overlays = std::collections::BTreeMap::new();

    tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &no_overlays }, now);

    assert!(
        !tail.checkouts[0].had_new_work,
        "a Rust path outside this checkout's targets is not work for it",
    );
    assert!(
        backlog_union(&tail.checkouts[0]).is_empty(),
        "and it is not retained: {:?}",
        backlog_union(&tail.checkouts[0]),
    );

    // The control: the same language, under the target this checkout DOES index.
    let indexed = BTreeSet::from(["src/live.rs".to_string()]);
    tail.absorb(&config, &LiveChangedSets { base: Some(&indexed), overlays: &no_overlays }, now);
    assert!(
        tail.checkouts[0].had_new_work,
        "a path inside the target is still admitted — the gate must not reject everything",
    );
}

#[test]
fn an_unreachable_checkout_keeps_its_backlog_and_yields_its_turn() {
    // A registered worktree that cannot be validated right now (unmounted, briefly unreadable)
    // is a DEFERRAL, not a removal: forgetting its backlog would lose work nothing can
    // rebuild, since a checkout returning with unchanged contents produces no changed paths
    // for the overlay refresh to report. Its turn is still spent, so it cannot hold the slot
    // every pass (#1010).
    let now = Instant::now();
    let mut tail = two_checkout_tail(now);

    tail.checkouts[1].defer_turn(now, "test");

    assert_eq!(
        tail.checkouts[1].backends[0].backlog,
        vec!["src/a.rs".to_string()],
        "the deferred checkout keeps its work",
    );
    assert_eq!(
        tail.checkouts[1].last_served_at,
        Some(now),
        "but it has spent its turn, so a sibling ranks ahead of it next pass",
    );
    assert!(
        !tail.checkouts[1].backends[0].lifecycle.can_respawn(now),
        "and the retry is backed off rather than re-attempted every pass",
    );
    assert!(
        tail.checkouts[1].deferred,
        "it is marked deferred, which is what keeps it in the wake computation",
    );
}

#[test]
fn a_deferred_checkout_still_schedules_the_pass_that_will_retry_it() {
    // A deferred checkout had its turn and could not take it, so its backoff is a real retry
    // deadline. Excluding it from the wake computation — as an unranked checkout correctly is
    // — meant that when every retained checkout was unreachable, nothing scheduled a pass at
    // all and the work stayed stranded even after the checkouts came back (#1010).
    let now = Instant::now();
    let mut tail = two_checkout_tail(now);
    let config = live_config(1);
    // Nothing was served this pass; the sole checkout with work was unreachable.
    tail.checkouts.truncate(1);
    tail.checkouts[0].defer_turn(now, "test");

    let wake = tail.next_wake_in(&config, now);

    assert!(
        wake.is_some(),
        "a deferred checkout must schedule the pass that retries it, or its backlog is stranded \
         whenever the periodic sweep is off",
    );
}

#[test]
fn a_checkout_holding_only_parked_paths_does_not_claim_a_slot() {
    // Parked paths ride along with a real worklist and can only be re-skipped on their own, so
    // a checkout holding nothing else has nothing to run. Letting it rank would spend the slot
    // on a pass that asks its server nothing — and since an unadmitted checkout schedules no
    // wake, the sibling it displaced would have its work stranded with the periodic sweep off.
    let now = Instant::now();
    let mut tail = two_checkout_tail(now);
    // The base checkout keeps only parked paths; the linked one holds real work.
    backends_of(&mut tail)[0].backlog.clear();
    backends_of(&mut tail)[0].unconfigured_paths.insert("src/parked.c".to_string());
    for checkout in &mut tail.checkouts {
        checkout.had_new_work = false;
    }

    let ranked = tail.rank();

    assert!(
        !ranked.contains(&0),
        "a parked-only checkout has nothing to run, so it is not ranked: {ranked:?}",
    );
    assert!(ranked.contains(&1), "the sibling with a real backlog is: {ranked:?}");
    assert_eq!(
        tail.checkouts[0].backends[0].unconfigured_paths.len(),
        1,
        "and its parked paths are kept, not discarded",
    );
}

#[test]
fn a_change_set_no_live_backend_claims_does_not_claim_a_checkout_slot() {
    // The general form: "nonempty" is not the test. Most repos change files no live backend
    // can answer — Python, markdown, config. Admitting those gave the checkout a phantom claim
    // on the cap: it ranked as freshly worked, won the slot, resolved nothing, and was pruned
    // as idle, repeating every pass while a sibling's real backlog stayed out (#1010).
    let now = Instant::now();
    let mut tail = LiveOracleTail {
        checkouts: vec![
            CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
            CheckoutTail::new("/wt/feat".to_string(), std::path::PathBuf::from("/wt/feat"), now),
        ],
    };
    let config = live_config(1);
    // A busy base checkout, but every path is in a language no live backend serves.
    let base = BTreeSet::from([
        "scripts/build.py".to_string(),
        "README.md".to_string(),
        "rag-rat.toml".to_string(),
    ]);
    let overlays = std::collections::BTreeMap::from([(
        "/wt/feat".to_string(),
        crate::watch::CheckoutReindex {
            source_root: std::path::PathBuf::from("/wt/feat"),
            paths: vec![std::path::PathBuf::from("src/linked.rs")],
            coverage: crate::index::ChangedPathsCoverage::Complete,
        },
    )]);

    tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &overlays }, now);
    let ranked = tail.rank();

    assert!(
        !tail.checkouts[0].had_new_work,
        "a change set no live backend claims is not work for this stage",
    );
    assert!(!ranked.contains(&0), "so the base checkout is not ranked: {ranked:?}");
    assert!(
        backlog_union(&tail.checkouts[0]).is_empty(),
        "and nothing of it is retained: {:?}",
        backlog_union(&tail.checkouts[0]),
    );
    assert!(
        ranked.contains(&1),
        "while the linked checkout that actually changed is ranked: {ranked:?}",
    );
}

#[test]
fn both_checkouts_with_work_are_eligible_for_a_turn() {
    // The cap is the whole knob: the same state that serves one checkout at the default must
    // serve both when an operator pays for it.
    let now = Instant::now();
    let mut tail = two_checkout_tail(now);
    tail.checkouts[0].last_worked_at = now - Duration::from_secs(60);

    assert_eq!(
        tail.rank().len(),
        2,
        "both checkouts have work, so both are eligible; how many actually run is the cap's \
         business in the serve loop",
    );
}

/// Every path a checkout is holding, across its backends.
fn backlog_union(checkout: &CheckoutTail) -> Vec<String> {
    let mut paths: Vec<String> =
        checkout.backends.iter().flat_map(|backend| backend.backlog.iter().cloned()).collect();
    paths.sort();
    paths.dedup();
    paths
}

#[test]
fn a_partial_overlay_report_still_contributes_the_paths_it_does_list() {
    // `Partial` means the list may be MISSING paths — the checkout's working-tree status read
    // failed, so dirty/untracked/deleted files never became candidates. It does NOT mean the
    // listed paths are doubtful: the committed tree-diff half still ran and every path here
    // had its overlay row written. Sound, merely incomplete.
    //
    // A best-effort freshness patch loses nothing by refreshing a subset, and the omitted half
    // surfaces on the next refresh (a partial pass clears the overlay basis). Discarding the
    // entry threw away work that was known stale and known which — and since a non-empty
    // backlog is what schedules the next wake, left nothing to schedule one (#1010).
    let now = Instant::now();
    let mut tail = LiveOracleTail {
        checkouts: vec![CheckoutTail::new(
            "/wt/feat".to_string(),
            std::path::PathBuf::from("/wt/feat"),
            now,
        )],
    };
    let config = live_config(1);
    let overlays = std::collections::BTreeMap::from([(
        "/wt/feat".to_string(),
        crate::watch::CheckoutReindex {
            source_root: std::path::PathBuf::from("/wt/feat"),
            paths: vec![std::path::PathBuf::from("src/a.rs")],
            coverage: crate::index::ChangedPathsCoverage::Partial,
        },
    )]);

    tail.absorb(&config, &LiveChangedSets { base: None, overlays: &overlays }, now);

    assert_eq!(
        backlog_union(&tail.checkouts[0]),
        vec!["src/a.rs".to_string()],
        "the paths a partial report DOES list are real committed changes, and are retained",
    );
    assert!(
        tail.checkouts[0].had_new_work,
        "and they count as work, so the checkout can be ranked and its backlog schedules a wake",
    );
}

#[test]
fn a_complete_overlay_report_becomes_that_checkouts_worklist() {
    // The counterpart: a complete list IS the checkout's changed set, and it is keyed to that
    // checkout rather than folded into the base one — a linked worktree's paths handed to the
    // main checkout's server would resolve against the wrong files.
    let now = Instant::now();
    let mut tail = LiveOracleTail {
        checkouts: vec![
            CheckoutTail::new(String::new(), std::path::PathBuf::from("/repo"), now),
            CheckoutTail::new("/wt/feat".to_string(), std::path::PathBuf::from("/wt/feat"), now),
        ],
    };
    let config = live_config(1);
    let base = BTreeSet::from(["src/base.rs".to_string()]);
    let overlays = std::collections::BTreeMap::from([(
        "/wt/feat".to_string(),
        crate::watch::CheckoutReindex {
            source_root: std::path::PathBuf::from("/wt/feat"),
            paths: vec![std::path::PathBuf::from("src/linked.rs")],
            coverage: crate::index::ChangedPathsCoverage::Complete,
        },
    )]);

    tail.absorb(&config, &LiveChangedSets { base: Some(&base), overlays: &overlays }, now);

    assert_eq!(
        backlog_union(&tail.checkouts[0]),
        vec!["src/base.rs".to_string()],
        "the base checkout holds only its own path",
    );
    assert_eq!(
        backlog_union(&tail.checkouts[1]),
        vec!["src/linked.rs".to_string()],
        "the linked checkout's path stays with it — never folded into the base checkout, whose \
         server reads different files",
    );
}

#[test]
fn one_backends_failure_does_not_disturb_another() {
    // Backends are independent: a crash streak on one must not gate the other's respawn, or
    // a wedged rust-analyzer would silently stop TypeScript resolution.
    let now = Instant::now();
    let mut tail = single_checkout_tail(now);
    backends_of(&mut tail)[0].lifecycle.on_failure(now);
    assert!(!backends_of(&mut tail)[0].lifecycle.can_respawn(now));
    assert!(
        backends_of(&mut tail)[1].lifecycle.can_respawn(now),
        "sibling backoff must be separate"
    );
}

#[test]
fn respawn_backoff_doubles_and_caps_without_allowing_early_retries() {
    let mut backoff = RespawnBackoff::default();
    let mut now = Instant::now();
    for expected in [30, 60, 120, 240, 300, 300] {
        backoff.record_failure(now);
        assert!(!backoff.ready(now));
        assert_eq!(backoff.remaining(now), Some(Duration::from_secs(expected)));
        now += Duration::from_secs(expected);
        assert!(backoff.ready(now));
    }
    backoff.reset();
    assert!(backoff.ready(now));
    assert_eq!(backoff.remaining(now), None);
}

#[test]
fn idle_wake_counts_from_last_session_use() {
    let last_used = Instant::now();
    assert_eq!(
        idle_wake_in(last_used, last_used + Duration::from_secs(3), Duration::from_secs(10)),
        Duration::from_secs(7),
    );
    assert_eq!(
        idle_wake_in(last_used, last_used + Duration::from_secs(10), Duration::from_secs(10)),
        Duration::ZERO,
    );
    assert_eq!(
        scheduled_idle_wake_in(
            last_used,
            last_used + Duration::from_secs(10),
            Duration::from_secs(10),
        ),
        LIVE_ORACLE_RETRY_INTERVAL,
        "an unserviced overdue deadline must not spin the maintenance loop",
    );
    assert!(should_idle_shutdown(
        false,
        last_used,
        last_used + Duration::from_secs(10),
        Duration::from_secs(10),
    ));
    assert!(
        !should_idle_shutdown(
            true,
            last_used,
            last_used + Duration::from_secs(60),
            Duration::from_secs(10),
        ),
        "pending warming work must win even when the session is otherwise idle",
    );
}

#[test]
fn lifecycle_prioritizes_pending_work_and_rearms_unserviced_idle_wakes() {
    let mut lifecycle = LiveOracleLifecycle::default();
    let started = Instant::now();
    let idle = Duration::from_secs(10);
    lifecycle.on_spawned(started);
    assert_eq!(lifecycle.next_wake_in(false, idle, started), Some(idle));

    let overdue = started + Duration::from_secs(60);
    assert!(!lifecycle.idle_shutdown_due(true, idle, overdue));
    assert!(lifecycle.idle_shutdown_due(false, idle, overdue));
    assert_eq!(
        lifecycle.next_wake_in(false, idle, overdue),
        Some(LIVE_ORACLE_RETRY_INTERVAL),
        "a pass that did not service the overdue wake must not spin",
    );

    lifecycle.on_session_ended();
    assert_eq!(lifecycle.next_wake_in(false, idle, overdue), None);

    lifecycle.on_failure(overdue);
    assert!(!lifecycle.can_respawn(overdue));
    assert_eq!(lifecycle.next_wake_in(true, idle, overdue), Some(LIVE_ORACLE_RETRY_INTERVAL),);
}
