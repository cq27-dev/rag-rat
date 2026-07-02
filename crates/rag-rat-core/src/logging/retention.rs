use std::path::Path;
use std::time::{Duration, SystemTime};

/// How recently a file must have been touched to be spared regardless of process liveness — a
/// belt-and-suspenders guard beside the pid-liveness check (e.g. across a pid recycle).
const RECENT_SECS: u64 = 300;

/// Best-effort cleanup of the per-process log dir. Any IO error is swallowed — logging cleanup must
/// never abort a command.
///
/// Only rag-rat's own per-process logs (`<role>-<pid>-<start_ms>.log`, `role` in
/// `mcp`/`hook`/`cli-*`) are ever considered, so a shared `[log].dir` with other apps' `*.log` is
/// untouched. A log is a prune CANDIDATE only if its creating process is gone AND it wasn't touched
/// recently; otherwise it is PROTECTED (a live process may hold the fd — unlinking it would strand
/// the writer on a deleted inode). Protected files are still COUNTED against `max_files`.
///
/// Candidates are pruned by (a) age > `retention_days` — every platform; (b) size >
/// `max_file_bytes` and (c) total-count > `max_files` — only where process liveness is verifiable
/// (unix). On a platform without a liveness probe the size/count rules could evict a live-but-idle
/// log, so they are skipped there and only the age rule (which reaps files untouched for whole
/// days) runs.
pub(super) fn sweep_retention(
    dir: &Path,
    retention_days: u64,
    max_files: u64,
    max_file_bytes: u64,
) {
    let recent_cutoff = SystemTime::now().checked_sub(Duration::from_secs(RECENT_SECS));
    let mut candidates: Vec<(std::path::PathBuf, SystemTime, u64)> = Vec::new();
    let mut protected: u64 = 0;
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for entry in rd.flatten() {
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(rag_rat_log_pid) else {
                    continue; // not one of ours
                };
                let Ok(meta) = entry.metadata() else { continue };
                let Ok(mtime) = meta.modified() else { continue };
                let live_or_recent =
                    process_is_alive(pid) || recent_cutoff.is_some_and(|cutoff| mtime >= cutoff);
                if live_or_recent {
                    protected += 1;
                } else {
                    candidates.push((entry.path(), mtime, meta.len()));
                }
            }
        },
        Err(_) => return,
    }

    // Age rule — safe on every platform (a file untouched for whole days can't be an active log).
    if retention_days > 0
        && let Some(cutoff) = SystemTime::now()
            .checked_sub(Duration::from_secs(retention_days.saturating_mul(86_400)))
    {
        candidates.retain(|(path, mtime, _)| {
            if *mtime < cutoff {
                let _ = std::fs::remove_file(path);
                false
            } else {
                true
            }
        });
    }

    // Size + count could evict a live-but-idle log where liveness can't be checked — unix only.
    if cfg!(unix) {
        if max_file_bytes > 0 {
            candidates.retain(|(path, _, len)| {
                if *len > max_file_bytes {
                    let _ = std::fs::remove_file(path);
                    false
                } else {
                    true
                }
            });
        }
        // `max_files` bounds the WHOLE dir: protected files count too, so delete oldest candidates
        // until the total is within the cap (or no removable candidates remain).
        let total = protected + candidates.len() as u64;
        if max_files > 0 && total > max_files {
            candidates.sort_by_key(|(_, mtime, _)| *mtime); // oldest first
            let remove = ((total - max_files) as usize).min(candidates.len());
            for (path, _, _) in candidates.into_iter().take(remove) {
                let _ = std::fs::remove_file(path);
            }
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
/// gone. Conservative on error (an unknown state counts as alive → don't prune).
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
    // No cheap portable liveness probe; size/count pruning is skipped on this platform (see
    // `sweep_retention`), so a live log is protected by the recent-mtime window + age-only pruning.
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
    fn prunes_by_age() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            write_log(dir.path(), &format!("mcp-{DEAD_PID}-{i}.log"), 1, 3600);
        }
        // Age the first one deep into the past; the age rule runs on every platform.
        std::fs::File::options()
            .write(true)
            .open(dir.path().join(format!("mcp-{DEAD_PID}-0.log")))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        sweep_retention(
            dir.path(),
            /* days */ 7,
            /* max_files */ 0,
            /* max_bytes */ 0,
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "the aged-out file is gone");
    }

    #[test]
    fn keeps_recently_modified_files() {
        let dir = tempfile::tempdir().unwrap();
        // Dead pid but touched just now → spared by the recent-mtime window (every platform).
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

    // Size / count pruning + pid-liveness are unix-only (see `sweep_retention` /
    // `process_is_alive`).
    #[cfg(unix)]
    #[test]
    fn prunes_oversize_then_count() {
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn max_files_counts_protected_files() {
        let dir = tempfile::tempdir().unwrap();
        let live = std::process::id();
        // 2 live (protected) + 3 dead+idle (candidates); cap the whole dir at 2.
        for i in 0..2 {
            write_log(dir.path(), &format!("mcp-{live}-{i}.log"), 1, 3600);
        }
        for i in 0..3 {
            write_log(dir.path(), &format!("mcp-{DEAD_PID}-{i}.log"), 1, 3600);
        }
        // `days` high so the age rule doesn't fire — this exercises the count rule alone.
        sweep_retention(
            dir.path(),
            /* days */ 3650,
            /* max_files */ 2,
            /* max_bytes */ 0,
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "trimmed to max_files, counting the 2 protected files");
        assert!(
            names.iter().all(|n| n.contains(&live.to_string())),
            "only the live/protected logs remain, got {names:?}"
        );
    }
}
