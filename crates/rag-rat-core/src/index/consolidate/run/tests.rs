use std::fs;
use std::path::{Path, PathBuf};

use super::*;

#[test]
fn imported_marker_appends_the_suffix() {
    assert_eq!(
        rag_rat_base::data_dir::imported_marker_path(Path::new("/repo/.rag-rat/index.sqlite")),
        PathBuf::from("/repo/.rag-rat/index.sqlite.imported"),
    );
}

/// The WAL sidecars travel with the `.imported` archive: a bare main-file rename would orphan
/// `-wal`/`-shm` as permanent litter, and an un-checkpointed `-wal` (a busy checkpoint under a
/// concurrent lockless reader — a sanctioned state) holds frames that BELONG to the archive.
/// Renaming them alongside is what keeps the archive whole regardless of checkpoint outcome —
/// the pinned BUSY posture.
#[test]
fn wal_sidecars_travel_with_the_imported_archive() {
    let dir = rag_rat_base::test_scratch::ScratchDir::new("sidecars");
    let source = dir.join("index.sqlite");
    let imported = rag_rat_base::data_dir::imported_marker_path(&source);
    // Simulate the post-rename state with LEFTOVER sidecars (incl. an un-checkpointed wal).
    fs::write(&source, b"db").unwrap();
    fs::write(path_with_suffix(&source, "-wal"), b"frames").unwrap();
    fs::write(path_with_suffix(&source, "-shm"), b"shm").unwrap();
    fs::rename(&source, &imported).unwrap();

    rename_wal_sidecars(&source, &imported);

    for suffix in ["-wal", "-shm"] {
        assert!(
            !path_with_suffix(&source, suffix).exists(),
            "no {suffix} litter remains at the legacy path"
        );
        assert!(
            path_with_suffix(&imported, suffix).exists(),
            "the {suffix} sidecar travelled with the archive"
        );
    }
}

/// The pinned-`database` refusal is PATH-AWARE: a pin at the default legacy path needs only
/// the key removed, while a CUSTOM pin must move its file first — keyless resolution never
/// consults a custom path, so "remove the key" alone would strand the index unimported. The
/// custom shape prints the literal commands for the user's paths.
#[test]
fn pinned_refusal_message_states_the_move_for_custom_paths() {
    let default_legacy = Path::new("/repo/.rag-rat/index.sqlite");

    let at_default = pinned_refusal_message(default_legacy, default_legacy);
    assert!(at_default.contains("Remove the `database` key"), "default shape: {at_default}");
    assert!(!at_default.contains("mv "), "default shape needs no move: {at_default}");

    let custom = pinned_refusal_message(Path::new("/repo/custom/my.db"), default_legacy);
    assert!(
        custom.contains("mv /repo/custom/my.db /repo/.rag-rat/index.sqlite"),
        "custom shape prints the literal move: {custom}"
    );
    assert!(
        custom.contains("mkdir -p /repo/.rag-rat"),
        "custom shape creates the default dir: {custom}"
    );
    assert!(
        custom.contains("-wal"),
        "custom shape warns about WAL sidecars holding recent writes: {custom}"
    );
}
