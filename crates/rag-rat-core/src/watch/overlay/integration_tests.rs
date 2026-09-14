use std::time::{Duration, Instant};

use crate::index::ai::ReconcileOptions;
use crate::watch::*;

#[test]
fn changed_overlay_skips_backlog_probe() {
    let budget =
        ReconcileBudget::new(ReconcileOptions::default(), Instant::now() - Duration::from_secs(1));
    let needs_embed = overlay_needs_embed(true, false, Some(&budget), |_| false);

    assert!(needs_embed, "a changed overlay still embeds inline");
}

#[test]
fn unchanged_overlay_on_a_scoped_pass_never_probes_the_backlog() {
    // #577: the per-worktree backlog probe (an O(scope) candidate scan) belongs to the `All`
    // sweep only. On an event-scoped pass an unchanged worktree must pay NOTHING.
    let budget =
        ReconcileBudget::new(ReconcileOptions::default(), Instant::now() - Duration::from_secs(1));
    let needs_embed = overlay_needs_embed(false, false, Some(&budget), |_| {
        panic!("the backlog probe must not run on an event-scoped pass")
    });

    assert!(!needs_embed, "unchanged + scoped pass: no embed work");
}

#[test]
fn unchanged_overlay_on_a_sweep_probes_and_retries_a_backlog() {
    let budget =
        ReconcileBudget::new(ReconcileOptions::default(), Instant::now() - Duration::from_secs(1));
    assert!(
        overlay_needs_embed(false, true, Some(&budget), |_| true),
        "a sweep retries a pending overlay backlog (a Partial drain heals within one sweep)"
    );
    assert!(
        !overlay_needs_embed(false, true, Some(&budget), |_| false),
        "a sweep with no backlog does no embed work"
    );
}

#[test]
fn reconcile_budget_is_shared_across_overlays_and_base() {
    // #219 review: each overlay reconcile (and the base) starts its OWN `max_seconds` timer, so
    // handing every one the same options lets the pass spend (N+1)× the advertised budget.
    // `next_options` recomputes `max_seconds` from the time remaining in the shared budget.
    let options = ReconcileOptions { max_seconds: Some(30), ..ReconcileOptions::default() };
    // A budget whose clock STARTED 30s ago is already exhausted → skip the reconcile.
    let spent =
        ReconcileBudget::new(options.clone(), Instant::now() - std::time::Duration::from_secs(30));
    assert!(spent.next_options().is_none(), "an exhausted budget yields no reconcile");

    // A fresh budget yields options whose `max_seconds` is at most the total (the remaining
    // time), never a fresh full budget per call.
    let fresh = ReconcileBudget::new(options, Instant::now());
    let next = fresh.next_options().expect("a fresh budget has time left");
    assert!(
        next.max_seconds.is_some_and(|s| s <= 30),
        "the per-call budget is bounded by the time remaining, not a fresh full budget: {:?}",
        next.max_seconds,
    );

    // An uncapped budget (`max_seconds: None`) always yields the base options.
    let uncapped = ReconcileBudget::new(ReconcileOptions::default(), Instant::now());
    assert_eq!(uncapped.next_options().and_then(|o| o.max_seconds), None);
}
