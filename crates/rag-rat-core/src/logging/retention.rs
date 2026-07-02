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
    use super::sweep_retention;

    #[test]
    fn prunes_by_age_then_count() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("mcp-{i}.log")), "x").unwrap();
        }
        // Age out the first two via an mtime deep in the past.
        let old = std::time::SystemTime::UNIX_EPOCH;
        for i in 0..2 {
            let file = std::fs::File::options()
                .write(true)
                .open(dir.path().join(format!("mcp-{i}.log")))
                .unwrap();
            file.set_modified(old).unwrap();
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
        sweep_retention(dir.path(), 0, 0, /* max_bytes */ 1024);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["mcp-small.log".to_string()], "oversize pruned, small kept");
    }
}
