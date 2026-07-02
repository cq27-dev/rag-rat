use std::path::Path;
use std::time::{Duration, SystemTime};

/// Best-effort cleanup of the per-process log dir. Any IO error is swallowed — logging cleanup must
/// never abort a command. Order: (1) drop files older than `retention_days`; (2) drop files larger
/// than `max_file_bytes` (0 disables); (3) enforce `max_files` (oldest by mtime first).
pub(super) fn sweep_retention(
    dir: &Path,
    retention_days: u64,
    max_files: u64,
    max_file_bytes: u64,
) {
    let mut entries: Vec<(std::path::PathBuf, SystemTime, u64)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some((e.path(), meta.modified().ok()?, meta.len()))
            })
            .collect(),
        Err(_) => return,
    };

    // Never prune a file a sibling process is likely still writing: an mcp server's active log can
    // exceed `max_file_bytes` or fall outside `max_files`, and unlinking it here would leave the
    // server writing to a now-deleted inode (lost logs, unreclaimed disk). Skip anything touched in
    // the last few minutes — retention only ever reaps clearly-idle files.
    const RECENT_SECS: u64 = 300;
    if let Some(recent_cutoff) = SystemTime::now().checked_sub(Duration::from_secs(RECENT_SECS)) {
        entries.retain(|(_, mtime, _)| *mtime < recent_cutoff);
    }

    if retention_days > 0
        && let Some(cutoff) = SystemTime::now()
            .checked_sub(Duration::from_secs(retention_days.saturating_mul(86_400)))
    {
        entries.retain(|(path, mtime, _)| {
            if *mtime < cutoff {
                let _ = std::fs::remove_file(path);
                false
            } else {
                true
            }
        });
    }

    if max_file_bytes > 0 {
        entries.retain(|(path, _, len)| {
            if *len > max_file_bytes {
                let _ = std::fs::remove_file(path);
                false
            } else {
                true
            }
        });
    }

    if max_files > 0 && entries.len() as u64 > max_files {
        entries.sort_by_key(|(_, mtime, _)| *mtime); // oldest first
        let remove = entries.len() - max_files as usize;
        for (path, _, _) in entries.into_iter().take(remove) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::sweep_retention;

    /// Backdate a file's mtime `secs` into the past (clear of the RECENT_SECS protection window) so
    /// the sweep treats it as an idle prune candidate.
    fn age_file(path: &std::path::Path, secs: u64) {
        let when = SystemTime::now() - Duration::from_secs(secs);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
    }

    #[test]
    fn prunes_by_age_then_count() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            let path = dir.path().join(format!("mcp-{i}.log"));
            std::fs::write(&path, "x").unwrap();
            age_file(&path, 3600); // all past the recent window → eligible
        }
        // Age out the first two via an mtime deep in the past.
        for i in 0..2 {
            std::fs::File::options()
                .write(true)
                .open(dir.path().join(format!("mcp-{i}.log")))
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH)
                .unwrap();
        }
        sweep_retention(
            dir.path(),
            /* days */ 7,
            /* max_files */ 100,
            /* max_bytes */ 0,
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3, "2 aged out");

        // Cap the count at 1 → oldest-first prune to 1.
        sweep_retention(dir.path(), /* days */ 3650, /* max_files */ 1, 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn prunes_oversize_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcp-big.log"), vec![b'x'; 4096]).unwrap();
        std::fs::write(dir.path().join("mcp-small.log"), b"x").unwrap();
        age_file(&dir.path().join("mcp-big.log"), 3600);
        age_file(&dir.path().join("mcp-small.log"), 3600);
        sweep_retention(dir.path(), 0, 0, /* max_bytes */ 1024);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["mcp-small.log".to_string()], "oversize pruned, small kept");
    }

    #[test]
    fn keeps_recently_modified_files() {
        let dir = tempfile::tempdir().unwrap();
        // A large, freshly-written file (mtime ~= now) may be a live sibling's active log — even an
        // aggressive sweep must leave it alone.
        std::fs::write(dir.path().join("mcp-live.log"), vec![b'x'; 8192]).unwrap();
        sweep_retention(
            dir.path(),
            /* days */ 1,
            /* max_files */ 1,
            /* max_bytes */ 1,
        );
        assert!(dir.path().join("mcp-live.log").exists(), "recent file must not be pruned");
    }
}
