use std::path::Path;
use std::time::{Duration, SystemTime};

/// How recently a file must have been touched to be spared regardless of process liveness — a
/// belt-and-suspenders guard beside the pid-liveness check (e.g. across a pid recycle).
const RECENT_SECS: u64 = 300;

/// Best-effort cleanup of the per-process log dir. Any IO error is swallowed — logging cleanup must
/// never abort a command.
///
/// A file is a prune CANDIDATE only if BOTH hold, so the sweep never deletes something it
/// shouldn't:
/// 1. it is one of rag-rat's own per-process logs — `<role>-<pid>-<start_ms>.log` with `role` in
///    `mcp`/`hook`/`cli-*` (so a shared `[log].dir` containing other apps' `*.log` is untouched);
///    and
/// 2. the process that created it (`<pid>`) is no longer alive AND it wasn't touched in the last
///    few minutes — a live process may still hold the file open, and unlinking it would leave that
///    process writing to a deleted inode (losing exactly the debug log this feature preserves).
///
/// Candidates are then pruned by (a) age > `retention_days`, (b) size > `max_file_bytes` (0
/// disables), (c) count > `max_files` (oldest by mtime first).
pub(super) fn sweep_retention(
    dir: &Path,
    retention_days: u64,
    max_files: u64,
    max_file_bytes: u64,
) {
    let recent_cutoff = SystemTime::now().checked_sub(Duration::from_secs(RECENT_SECS));
    let mut entries: Vec<(std::path::PathBuf, SystemTime, u64)> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name();
                let pid = rag_rat_log_pid(name.to_str()?)?;
                // A live creating process may still hold the fd — never a candidate.
                if process_is_alive(pid) {
                    return None;
                }
                let meta = entry.metadata().ok()?;
                let mtime = meta.modified().ok()?;
                // Recently touched → likely still active; spare it (pid recycle / clock skew
                // guard).
                if recent_cutoff.is_some_and(|cutoff| mtime >= cutoff) {
                    return None;
                }
                Some((entry.path(), mtime, meta.len()))
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

/// The pid of a rag-rat per-process log named `<role>-<pid>-<start_ms>.log` (`role` may itself
/// contain `-`, e.g. `cli-reconcile`). Returns `None` for any file that is not one of ours — the
/// naming filter that keeps the sweep off unrelated `*.log` files.
fn rag_rat_log_pid(name: &str) -> Option<u32> {
    if !(name.starts_with("mcp-") || name.starts_with("hook-") || name.starts_with("cli-")) {
        return None;
    }
    let stem = name.strip_suffix(".log")?;
    // Trailing two `-`-fields are `<pid>-<start_ms>`; require a leading role field too.
    let mut tail = stem.rsplitn(3, '-');
    let _start_ms = tail.next()?;
    let pid = tail.next()?;
    tail.next()?; // role segment must exist (rejects a bare `mcp-123.log`)
    pid.parse::<u32>().ok()
}

/// Whether `pid` names a currently-running process (so its log may still be open). `kill(pid, 0)`
/// probes without signalling: `Ok` → alive; `EPERM` → alive but owned by another user; `ESRCH` →
/// gone. Conservative on error (treats an unknown state as alive → don't prune).
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false; // out of pid_t range → cannot name a real process
    };
    // SAFETY: `kill` with signal 0 performs only the existence/permission check, no signal
    // delivery.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // No cheap portable liveness probe; rely on the recent-mtime window alone.
    false
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{rag_rat_log_pid, sweep_retention};

    /// A pid above Linux's default `pid_max` — `kill(_, 0)` reports it gone, so files named with it
    /// are always treated as belonging to a dead process (deterministic in tests).
    const DEAD_PID: u32 = 2_147_483_646;

    fn write_log(dir: &std::path::Path, name: &str, bytes: usize, age_secs: u64) {
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; bytes.max(1)]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(when).unwrap();
    }

    #[test]
    fn parses_pid_only_from_our_log_names() {
        assert_eq!(rag_rat_log_pid("mcp-1234-5678.log"), Some(1234));
        assert_eq!(rag_rat_log_pid("hook-42-1.log"), Some(42));
        assert_eq!(rag_rat_log_pid("cli-reconcile-99-1000.log"), Some(99)); // role with a '-'
        assert_eq!(rag_rat_log_pid("mcp-123.log"), None); // no start field
        assert_eq!(rag_rat_log_pid("app.log"), None); // not ours
        assert_eq!(rag_rat_log_pid("random-1-2.log"), None); // wrong role prefix
    }

    #[test]
    fn prunes_by_age_then_count() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            write_log(dir.path(), &format!("mcp-{DEAD_PID}-{i}.log"), 1, 3600);
        }
        // Age out the first two deep into the past.
        for i in 0..2 {
            let path = dir.path().join(format!("mcp-{DEAD_PID}-{i}.log"));
            std::fs::File::options()
                .write(true)
                .open(&path)
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

        sweep_retention(dir.path(), /* days */ 3650, /* max_files */ 1, 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn prunes_oversize_files() {
        let dir = tempfile::tempdir().unwrap();
        write_log(dir.path(), &format!("mcp-{DEAD_PID}-1.log"), 4096, 3600);
        write_log(dir.path(), &format!("mcp-{DEAD_PID}-2.log"), 1, 3600);
        sweep_retention(dir.path(), 0, 0, /* max_bytes */ 1024);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![format!("mcp-{DEAD_PID}-2.log")], "oversize pruned, small kept");
    }

    #[test]
    fn keeps_recently_modified_files() {
        let dir = tempfile::tempdir().unwrap();
        // Dead pid but touched just now → spared by the recent-mtime window.
        write_log(dir.path(), &format!("mcp-{DEAD_PID}-9.log"), 8192, 0);
        sweep_retention(
            dir.path(),
            /* days */ 1,
            /* max_files */ 1,
            /* max_bytes */ 1,
        );
        assert!(
            dir.path().join(format!("mcp-{DEAD_PID}-9.log")).exists(),
            "recent file not pruned"
        );
    }

    #[test]
    fn keeps_files_of_live_processes() {
        let dir = tempfile::tempdir().unwrap();
        // Our OWN pid is alive; an old, oversized log named with it must survive an aggressive
        // sweep.
        let live = std::process::id();
        write_log(dir.path(), &format!("mcp-{live}-1.log"), 8192, 3600);
        sweep_retention(
            dir.path(),
            /* days */ 1,
            /* max_files */ 1,
            /* max_bytes */ 1,
        );
        assert!(
            dir.path().join(format!("mcp-{live}-1.log")).exists(),
            "live-process log not pruned"
        );
    }

    #[test]
    fn ignores_foreign_log_files() {
        let dir = tempfile::tempdir().unwrap();
        // A non-rag-rat `*.log` in a shared dir must never be a candidate.
        write_log(dir.path(), "some-other-app.log", 8192, 7 * 86_400);
        sweep_retention(
            dir.path(),
            /* days */ 1,
            /* max_files */ 1,
            /* max_bytes */ 1,
        );
        assert!(dir.path().join("some-other-app.log").exists(), "foreign log untouched");
    }
}
