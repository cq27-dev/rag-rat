use std::collections::BTreeSet;
use std::path::PathBuf;

use rag_rat_base::locks;
use rag_rat_base::single_flight::SingleFlight;

use super::*;

fn path_set<const N: usize>(paths: [&str; N]) -> PathSet {
    PathSet(paths.iter().map(PathBuf::from).collect())
}

#[test]
fn path_set_round_trips_through_the_marker_encoding() {
    let set = path_set(["/repo/src/a.rs", "/repo/src/b.rs", "/repo/x.ts"]);
    assert_eq!(PathSet::decode(&set.encode()), set);
    // Empty is well-defined (no queued paths).
    assert_eq!(PathSet::decode(&PathSet::default().encode()), PathSet::default());
    // Blank/trailing-newline tokens never become empty PathBuf entries.
    assert_eq!(PathSet::decode(b"\n/repo/a.rs\n\n"), path_set(["/repo/a.rs"]));
}

#[test]
fn path_set_merge_is_a_union() {
    let merged =
        path_set(["/repo/a.rs", "/repo/b.rs"]).merge(path_set(["/repo/b.rs", "/repo/c.rs"]));
    assert_eq!(merged, path_set(["/repo/a.rs", "/repo/b.rs", "/repo/c.rs"]));
}

/// The #660 single-flight over a `PathSet` never loses an edited path under cross-"process"
/// filesystem contention: many contenders storm the marker with distinct paths (the shape of a
/// refactor firing a PostToolUse burst), and the surviving marker is the UNION of every path.
#[test]
fn concurrent_edit_triggers_coalesce_into_the_path_union() {
    let tmp = tempfile::TempDir::new().unwrap();
    let database = tmp.path().join("locks/index.sqlite");
    let single_flight = || {
        SingleFlight::<PathSet>::new(
            locks::edit_reindex_lock_path(&database, "repo"),
            locks::edit_reindex_pending_path(&database, "repo"),
            locks::edit_reindex_marker_lock_path(&database, "repo"),
        )
    };
    std::thread::scope(|scope| {
        for contender in 0..8 {
            let sf = single_flight();
            scope.spawn(move || {
                let path =
                    PathSet(BTreeSet::from([PathBuf::from(format!("/repo/f{contender}.rs"))]));
                for _ in 0..50 {
                    sf.queue(path.clone()).unwrap();
                }
            });
        }
    });
    let drained = single_flight().take().unwrap().unwrap();
    let expected: PathSet =
        PathSet((0..8).map(|contender| PathBuf::from(format!("/repo/f{contender}.rs"))).collect());
    assert_eq!(drained, expected, "every contender's edited path survives the coalesce");
}
