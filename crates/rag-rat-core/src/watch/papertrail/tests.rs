use std::time::Duration;

use crate::watch::tests::support::*;
use crate::watch::*;

#[test]
fn papertrail_scheduler_single_flights_and_coalesces_max_wins() {
    use rag_rat_papertrail::AutosyncRequest;
    let mut scheduler = PapertrailScheduler::new();
    // Idle → dispatch immediately.
    assert_eq!(scheduler.admit(AutosyncRequest::Evaluate), Some(AutosyncRequest::Evaluate));
    // In flight → any number of requests coalesce into ONE pending follow-up, strongest wins —
    // and a later weaker request must not weaken it.
    assert_eq!(scheduler.admit(AutosyncRequest::Incremental), None);
    assert_eq!(scheduler.admit(AutosyncRequest::Full), None);
    assert_eq!(scheduler.admit(AutosyncRequest::Evaluate), None);
    // Completion dispatches the coalesced follow-up (the scheduler is in flight again)...
    assert_eq!(scheduler.on_done(), Some(AutosyncRequest::Full));
    // ...and the next completion, with nothing queued, dispatches nothing.
    assert_eq!(scheduler.on_done(), None);
    assert_eq!(scheduler.admit(AutosyncRequest::Evaluate), Some(AutosyncRequest::Evaluate));
}

#[test]
fn papertrail_tick_interval_requires_bindings_and_takes_the_tightest_cadence() {
    // No `[[tracker]]` bindings and no git remote to auto-detect one from → disabled.
    let (_scratch, mut config, _) = src_checkout_config("watch-papertrail-cadence");
    assert_eq!(papertrail_tick_interval(&config), None);

    config.trackers = vec![rag_rat_base::config::TrackerConfig {
        provider: rag_rat_base::config::Tracker::Github,
        project: Some("o/r".to_string()),
        remote: "origin".to_string(),
        base_url: None,
        auth: None,
        tags: Vec::new(),
    }];
    assert_eq!(papertrail_tick_interval(&config), Some(Duration::from_secs(900)));
    // The daily full-walk backstop shares the wake-up: a tighter full interval tightens it.
    config.papertrail.full_sync_interval_secs = 600;
    assert_eq!(papertrail_tick_interval(&config), Some(Duration::from_secs(600)));
}
