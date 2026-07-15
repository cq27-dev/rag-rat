//! Watch-placement failure count round-trips through `repo_meta` into `IndexStatus` (#658), so a
//! resident watcher's silently-dropped watches are observable rather than invisible.

use super::*;

#[test]
fn index_status_surfaces_watch_placement_failures() {
    let root = fixture_temp_root("held-mini");
    let config = source_config(root.clone(), Language::Rust);
    let db = IndexDatabase::rebuild(&config).unwrap();

    // A fresh index has recorded no watch-placement failures.
    assert_eq!(db.status(&config.database).unwrap().watch_placement_failures, 0);

    // The watcher records a failure count during a pass; it must surface in status.
    let changed = db.record_watch_placement_failures(3).unwrap();
    assert!(changed, "first write of a non-zero count is a change");
    assert_eq!(db.status(&config.database).unwrap().watch_placement_failures, 3);

    // A repeated same-value write is a no-op (no WAL churn); the count is unchanged.
    assert!(!db.record_watch_placement_failures(3).unwrap(), "same value is not a change");
    assert_eq!(db.status(&config.database).unwrap().watch_placement_failures, 3);

    // HIGH-WATER MARK: a LOWER count (a healthy or freshly-restarted watcher sharing this DB, whose
    // process-local counter is 0 or low) must NOT erase a degraded watcher's recorded failures.
    assert!(!db.record_watch_placement_failures(0).unwrap(), "a lower count writes nothing");
    assert_eq!(
        db.status(&config.database).unwrap().watch_placement_failures,
        3,
        "a healthy watcher's 0 must not clobber a degraded watcher's count"
    );

    // A HIGHER count raises the mark.
    assert!(db.record_watch_placement_failures(5).unwrap(), "a higher count is a change");
    assert_eq!(db.status(&config.database).unwrap().watch_placement_failures, 5);

    let _ = fs::remove_dir_all(root);
}
