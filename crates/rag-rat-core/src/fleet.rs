//! Fleet hot-upgrade trigger.
//!
//! When a new `rag-rat` binary lands at the configured install path (an atomic `cargo install`
//! rename), the elected watcher signals every still-old, hot-upgrade-armed `rag-rat mcp` server
//! — including this process, last — with `SIGUSR1`, so each `exec`s the new binary at its own
//! request boundary. Linux-only (it walks `/proc`); a no-op elsewhere.
//!
//! Targeting is deliberately conservative. A process is signaled only if it is (a) running *our*
//! binary, (b) the `mcp` subcommand, (c) hot-upgrade-armed (its environ carries
//! [`UPGRADE_BIN_ENV`], i.e. it has a `SIGUSR1` handler installed), and (d) on an outdated binary
//! (its exe inode differs from the inode now installed). The environ check is the safety
//! interlock: without it a `SIGUSR1` to an un-armed server would hit the default disposition and
//! terminate it.

use std::path::Path;

/// Env var naming the installed-binary path; presence in a process's environ means it armed the
/// hot-upgrade `SIGUSR1` handler. Shared contract with the MCP server (`rag-rat-mcp` re-exports
/// this constant) and used here to read other processes' environ.
pub const UPGRADE_BIN_ENV: &str = "RAG_RAT_UPGRADE_BIN";

/// A candidate process discovered under `/proc`, reduced to what target selection needs.
// Linux-only: the only consumers are `trigger`/`scan_proc`/`select_targets`, all gated to Linux —
// without this gate it is dead code under `-D warnings` on macOS/Windows.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcInfo {
    pub pid: i32,
    /// Inode of the binary image the process is executing (`/proc/<pid>/exe`).
    pub exe_inode: u64,
    pub is_self: bool,
    /// Our binary, running `mcp`, with the hot-upgrade env armed.
    pub eligible: bool,
}

/// Choose which PIDs to `SIGUSR1`, ordered so this process upgrades **last** (others ascending,
/// self appended). A process is a target iff it is [`ProcInfo::eligible`] and running an outdated
/// binary (`exe_inode != installed_inode`). Pure, so it is unit-testable without `/proc`.
#[cfg(target_os = "linux")]
pub(crate) fn select_targets(procs: &[ProcInfo], installed_inode: u64) -> Vec<i32> {
    let mut others: Vec<i32> = Vec::new();
    let mut own: Option<i32> = None;
    for proc in procs {
        if !proc.eligible || proc.exe_inode == installed_inode {
            continue;
        }
        if proc.is_self {
            own = Some(proc.pid);
        } else {
            others.push(proc.pid);
        }
    }
    others.sort_unstable();
    others.extend(own); // self last
    others
}

/// Signal the fleet to hot-upgrade to the binary now at `install_path`. Best-effort and
/// non-blocking; failures (unreadable `/proc`, vanished PIDs) are skipped silently.
#[cfg(target_os = "linux")]
pub fn trigger(install_path: &Path) {
    let Some(installed_inode) = linux::inode(install_path) else {
        return;
    };
    let Some(bin_name) = install_path.file_name() else {
        return;
    };
    let procs = linux::scan_proc(bin_name);
    for pid in select_targets(&procs, installed_inode) {
        linux::send_sigusr1(pid);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn trigger(_install_path: &Path) {}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use super::{ProcInfo, UPGRADE_BIN_ENV};

    pub(super) fn inode(path: &Path) -> Option<u64> {
        fs::metadata(path).ok().map(|meta| meta.ino())
    }

    /// Walk `/proc/<pid>` and build a [`ProcInfo`] for every numeric PID, classifying eligibility.
    pub(super) fn scan_proc(bin_name: &OsStr) -> Vec<ProcInfo> {
        let self_pid = std::process::id() as i32;
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
                let proc_dir = entry.path();
                // Inode of the running image; `/proc/<pid>/exe` follows to the file even when it
                // was unlinked by the install rename (the classic "(deleted)" case).
                let exe_inode = fs::metadata(proc_dir.join("exe")).ok()?.ino();
                Some(ProcInfo {
                    pid,
                    exe_inode,
                    is_self: pid == self_pid,
                    eligible: is_eligible(&proc_dir, bin_name),
                })
            })
            .collect()
    }

    /// Our binary (`argv[0]` basename matches), running the `mcp` subcommand, with the hot-upgrade
    /// env armed. Any unreadable bit (permission, race) makes the process ineligible — fail safe.
    fn is_eligible(proc_dir: &Path, bin_name: &OsStr) -> bool {
        runs_our_mcp(proc_dir, bin_name) && has_upgrade_env(proc_dir)
    }

    fn runs_our_mcp(proc_dir: &Path, bin_name: &OsStr) -> bool {
        let Ok(cmdline) = fs::read(proc_dir.join("cmdline")) else {
            return false;
        };
        // `/proc/<pid>/cmdline` is NUL-separated argv.
        let mut args = cmdline.split(|&byte| byte == 0).filter(|arg| !arg.is_empty());
        let Some(argv0) = args.next() else {
            return false;
        };
        let argv0_name = Path::new(OsStr::from_bytes(argv0)).file_name();
        let is_our_binary = argv0_name == Some(bin_name);
        let runs_mcp = args.any(|arg| arg == b"mcp");
        is_our_binary && runs_mcp
    }

    fn has_upgrade_env(proc_dir: &Path) -> bool {
        let Ok(environ) = fs::read(proc_dir.join("environ")) else {
            return false; // environ is unreadable across uids — fail safe.
        };
        let needle = format!("{UPGRADE_BIN_ENV}=");
        environ.split(|&byte| byte == 0).any(|entry| entry.starts_with(needle.as_bytes()))
    }

    pub(super) fn send_sigusr1(pid: i32) {
        // SAFETY: `kill(2)` with a valid signal number; an invalid/vanished PID just returns an
        // error we ignore. No memory is touched.
        unsafe {
            libc::kill(pid, libc::SIGUSR1);
        }
    }
}

// `select_targets`/`ProcInfo` exist only on Linux, so their unit tests are Linux-only too.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn proc(pid: i32, exe_inode: u64, is_self: bool, eligible: bool) -> ProcInfo {
        ProcInfo { pid, exe_inode, is_self, eligible }
    }

    #[test]
    fn selects_only_outdated_eligible_processes() {
        let installed = 100;
        let procs = vec![
            proc(10, 99, false, true),  // eligible + outdated  → target
            proc(11, 100, false, true), // eligible but already new → skip
            proc(12, 99, false, false), // outdated but ineligible → skip
            proc(13, 42, false, true),  // eligible + outdated  → target
        ];
        assert_eq!(select_targets(&procs, installed), vec![10, 13]);
    }

    #[test]
    fn self_is_signaled_last() {
        let installed = 100;
        let procs = vec![
            proc(7, 1, true, true),   // self, outdated
            proc(30, 1, false, true), // other, outdated
            proc(20, 1, false, true), // other, outdated
        ];
        // Others ascending, then self.
        assert_eq!(select_targets(&procs, installed), vec![20, 30, 7]);
    }

    #[test]
    fn empty_when_nothing_outdated_or_eligible() {
        let installed = 5;
        let procs = vec![proc(1, 5, false, true), proc(2, 4, false, false)];
        assert!(select_targets(&procs, installed).is_empty());
    }
}
