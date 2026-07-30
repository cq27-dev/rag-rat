//! In-place hot upgrade of the stdio MCP server via `SIGUSR1` (Unix only).
//!
//! A live `rag-rat mcp` keeps running the OLD binary until the client reconnects. On `SIGUSR1`
//! the process drains in-flight work at a request boundary, hands the negotiated session state to
//! a freshly `exec`'d copy of the newly installed binary (same PID, same stdio pipes), and the
//! new process resumes via [`rmcp::service::serve_directly`] — no re-`initialize`, no dropped or
//! duplicated requests.
//!
//! This module holds the request-boundary machinery:
//! - [`GatedStdin`] — an `AsyncRead` that parks at a line boundary once an upgrade is pending, so
//!   no new request is read past the boundary (zero residue **by construction**: rmcp's transport
//!   wraps us in its own `BufReader` and `read_until(b'\n')`, so we must never hand it bytes past a
//!   single `\n` while the gate is closed).
//! - [`Inflight`] — counts executing tool calls so teardown can wait for the last one to finish.
//! - [`HandoffV1`] — the minimal session snapshot carried across `exec` via a temp file.

#![cfg(unix)]

use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker, ready};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::model::{InitializeRequestParams, ProtocolVersion};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWriteExt, BufReader, ReadBuf, Stdin};
use tokio::sync::Notify;

/// How long teardown waits for in-flight tool calls to finish before aborting the upgrade and
/// staying on the current binary.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Exit code when `exec` of the new binary fails — the process is already torn down, so a non-zero
/// exit makes the client see EOF and relaunch.
const EXEC_FAILED_EXIT: i32 = 70;

/// Env var naming the absolute path of the installed binary to `exec` on upgrade (e.g.
/// `~/.cargo/bin/rag-rat`). Unset ⇒ hot-upgrade disabled. Never `/proc/self/exe` (old inode).
/// Single source of truth in [`rag_rat_core::fleet`], which reads it from other processes'
/// environ to find hot-upgrade-armed servers.
pub use rag_rat_core::fleet::UPGRADE_BIN_ENV;

/// Env var carrying the handoff temp-file path across `exec`. Set by the old process, consumed
/// (read + unlinked) by the new one.
pub const HANDOFF_PATH_ENV: &str = "MCP_HANDOFF_PATH";

/// Minimal session snapshot carried across `exec`. rag-rat is a read-only query server with no
/// subscriptions / roots / notifications, so the only state worth preserving is enough to skip the
/// `initialize` handshake: the client's negotiated params plus a version gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffV1 {
    /// Schema version; always `1`. First field so a future reader can branch before deserializing.
    pub format_version: u32,
    /// Protocol version the client negotiated with the old process. The new process refuses to
    /// resume (clean exit ⇒ client reconnects + renegotiates) if it can't honor this.
    pub negotiated_protocol_version: String,
    /// The client's `initialize` params — exactly what [`serve_directly`] wants as `peer_info`.
    ///
    /// [`serve_directly`]: rmcp::service::serve_directly
    pub peer_info: InitializeRequestParams,
    /// Unframed bytes already read past the last request boundary. Always empty in v1 (the gate
    /// parks at a `\n` boundary), kept so the format can carry residue without a version bump.
    pub residue: Vec<u8>,
    /// Inode of the old binary at handoff time — lets the fleet trigger target only old processes.
    pub old_binary_inode: u64,
    /// Wall-clock ms when the upgrade began (diagnostics only).
    pub upgrade_started_unix_ms: u64,
}

impl HandoffV1 {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn new(
        negotiated_protocol_version: String,
        peer_info: InitializeRequestParams,
        old_binary_inode: u64,
    ) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            negotiated_protocol_version,
            peer_info,
            residue: Vec::new(),
            old_binary_inode,
            upgrade_started_unix_ms: now_unix_ms(),
        }
    }

    /// Serialize to a uniquely-named temp file under `dir` and return its path. The new process
    /// reads it via [`HandoffV1::read_and_unlink`]. JSON keeps the format legible for debugging.
    pub fn write_temp(&self, dir: &Path) -> io::Result<PathBuf> {
        let path = dir.join(format!("rag-rat-handoff-{}.json", std::process::id()));
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    /// Read + deserialize, then unlink the file (best-effort) so it never leaks.
    pub fn read_and_unlink(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let _ = std::fs::remove_file(path);
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The shared upgrade flag. Set when `SIGUSR1` is received; the gate and teardown both observe it.
#[derive(Debug, Default)]
pub struct UpgradeGate {
    pending: AtomicBool,
    /// Waker for a [`GatedStdin`] parked at a line boundary, so an *aborted* upgrade (drain
    /// timeout) can resume serving. On the happy path the parked task is never woken — `exec`
    /// replaces the process.
    parked: Mutex<Option<Waker>>,
}

impl UpgradeGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Signal that an upgrade is pending. Idempotent: a second `SIGUSR1` is a no-op.
    pub fn request(&self) {
        self.pending.store(true, Ordering::Release);
    }

    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// Abort a pending upgrade and wake any parked reader so serving resumes on the old binary.
    pub fn abort(&self) {
        self.pending.store(false, Ordering::Release);
        if let Some(waker) = self.parked.lock().expect("upgrade gate poisoned").take() {
            waker.wake();
        }
    }
}

/// `AsyncRead` over stdin that stops handing out bytes at a line boundary once an upgrade is
/// pending. To keep zero residue in rmcp's own `BufReader`, it returns **at most one line per
/// read** (up to and including the first `\n`), so the outer `read_until(b'\n')` never buffers a
/// following request that would be lost at `exec`.
///
/// Generic over the inner reader so the line-splitting is unit-testable with an in-memory source;
/// production always uses [`Stdin`].
pub struct GatedStdin<R = Stdin> {
    inner: BufReader<R>,
    gate: Arc<UpgradeGate>,
    /// True while the last delivered byte was not `\n` — i.e. we're mid-line and must finish it
    /// before the gate is allowed to park.
    partial_line: bool,
}

impl GatedStdin<Stdin> {
    pub fn new(stdin: Stdin, gate: Arc<UpgradeGate>) -> Self {
        Self::with_reader(stdin, gate)
    }
}

impl<R: AsyncRead> GatedStdin<R> {
    fn with_reader(reader: R, gate: Arc<UpgradeGate>) -> Self {
        Self { inner: BufReader::new(reader), gate, partial_line: false }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for GatedStdin<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // At a line boundary with an upgrade pending: park. Register the waker *before* the final
        // re-check so an abort racing with us can't be lost.
        if this.gate.is_pending() && !this.partial_line {
            *this.gate.parked.lock().expect("upgrade gate poisoned") = Some(cx.waker().clone());
            if this.gate.is_pending() && !this.partial_line {
                return Poll::Pending;
            }
        }

        let available = ready!(Pin::new(&mut this.inner).poll_fill_buf(cx))?;
        if available.is_empty() {
            return Poll::Ready(Ok(())); // EOF
        }
        // Hand over at most one line: through the first `\n`, capped by the caller's space.
        let line_end =
            available.iter().position(|&b| b == b'\n').map_or(available.len(), |i| i + 1);
        let n = line_end.min(buf.remaining());
        let ends_line = available[n - 1] == b'\n';
        buf.put_slice(&available[..n]);
        Pin::new(&mut this.inner).consume(n);
        this.partial_line = !ends_line;
        Poll::Ready(Ok(()))
    }
}

/// Counts in-flight tool calls so teardown can wait for the last one to finish before `exec`.
/// All tools funnel through one dispatch chokepoint, so a single guard there is sufficient.
#[derive(Debug, Default)]
pub struct Inflight {
    count: AtomicUsize,
    notify: Notify,
}

impl Inflight {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Acquire a guard for the duration of one tool call. Increments on entry, decrements on drop.
    pub fn guard(self: &Arc<Self>) -> InflightGuard {
        self.count.fetch_add(1, Ordering::AcqRel);
        InflightGuard(Arc::clone(self))
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Resolve once no tool call is in flight. Registers the notification *before* re-checking the
    /// count so a decrement racing with us can't be missed.
    pub async fn wait_zero(&self) {
        loop {
            let notified = self.notify.notified();
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// RAII guard decrementing the in-flight count when a tool call returns.
#[derive(Debug)]
pub struct InflightGuard(Arc<Inflight>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.notify.notify_waiters();
        }
    }
}

/// Absolute path of the installed binary to `exec` on upgrade, from [`UPGRADE_BIN_ENV`]. `None`
/// (unset or empty) disables hot-upgrade.
pub fn install_path() -> Option<PathBuf> {
    std::env::var_os(UPGRADE_BIN_ENV).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// If [`HANDOFF_PATH_ENV`] is set, read + unlink the handoff file the previous process left for us.
/// Returns `None` on a cold start or if the file is unreadable (treated as cold).
pub fn take_handoff() -> Option<HandoffV1> {
    let path = PathBuf::from(std::env::var_os(HANDOFF_PATH_ENV)?);
    // The env var is intentionally left set: the file is consumed here, and the next `exec`
    // overrides this key via `Command::env`, so a stale value is harmless and we avoid the
    // edition-2024 `unsafe { remove_var }`.
    match HandoffV1::read_and_unlink(&path) {
        Ok(handoff) => Some(handoff),
        Err(err) => {
            eprintln!("hot-upgrade: unreadable handoff {}: {err}", path.display());
            None
        },
    }
}

/// Drop `SIGUSR1` on the floor for now — the FIRST half of the fleet interlock, and the first
/// thing `rag-rat mcp` should do.
///
/// [`rag_rat_core::fleet::trigger`] picks its targets by reading other processes' environ: a
/// `rag-rat mcp` carrying [`UPGRADE_BIN_ENV`] is taken as proof that a `SIGUSR1` handler is
/// installed, and is signaled on that basis. That environ is visible from `execve` onward, while
/// the real handler cannot exist until there is a Tokio runtime to own it — and config discovery,
/// logging setup, and runtime construction all happen in between. Left alone, a trigger landing in
/// that gap terminates the server outright (`SIGUSR1`'s default disposition) instead of upgrading
/// it. Ignoring costs the process at most one upgrade opportunity — it stays on the old binary
/// until the client disconnects, exactly like a session with nothing to hand off — which is the
/// documented fallback, and infinitely better than dying.
///
/// [`arm_sigusr1`] replaces this with the real handler as soon as the runtime is up.
pub fn suppress_sigusr1_until_armed() {
    // Gate on what the TRIGGER considers eligible, not on whether we could actually upgrade. Those
    // differ: `install_path` rejects a set-but-empty variable, while the scan predicate matches on
    // presence alone — so an empty value yields a process that is targetable but cannot upgrade,
    // which is exactly the case that must not die on the signal.
    if !rag_rat_core::fleet::self_advertises_upgrade() {
        // Not a target, so leave the default disposition alone rather than silently changing
        // signal behavior for every `rag-rat mcp`.
        return;
    }
    // SAFETY: `signal(2)` with a valid signal number and the standard `SIG_IGN` disposition. No
    // handler runs and no memory is touched.
    unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
}

/// Observe `SIGUSR1` on the Tokio runtime — the SECOND half of the interlock, replacing the
/// blanket ignore installed by [`suppress_sigusr1_until_armed`] with a stream the server can act
/// on. Returns `None` when the OS refused, having left the signal ignored rather than fatal.
///
/// Call this BEFORE serving. Serving blocks on the client's first message, which may never arrive,
/// so arming afterwards would leave the process merely ignoring upgrades for an unbounded stretch.
/// What the signal *does* is decided later, once the session is understood.
pub(crate) fn arm_sigusr1() -> Option<tokio::signal::unix::Signal> {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::user_defined1()) {
        Ok(stream) => Some(stream),
        Err(err) => {
            eprintln!(
                "hot-upgrade: could not install SIGUSR1 handler ({err}); ignoring the signal"
            );
            // Stay ignored (already the case if `suppress_sigusr1_until_armed` ran): this process
            // advertises itself as signal-safe through its environ either way, so it must not die
            // on a signal it turned out it cannot handle. Hot-upgrade is lost for this session.
            // SAFETY: as in `suppress_sigusr1_until_armed`.
            unsafe { libc::signal(libc::SIGUSR1, libc::SIG_IGN) };
            None
        },
    }
}

/// Whether this binary speaks `version` — the resume version gate. An unknown version means the
/// new binary can't honor the client's negotiated session, so it must not skip `initialize`.
pub fn protocol_supported(version: &str) -> bool {
    ProtocolVersion::KNOWN_VERSIONS.iter().any(|known| known.as_str() == version)
}

/// Inode of the binary image this process is executing — recorded in the handoff so the fleet
/// trigger can target only processes still on the old binary. Best-effort (`0` if unavailable).
pub fn current_exe_inode() -> u64 {
    std::fs::metadata("/proc/self/exe")
        .or_else(|_| std::env::current_exe().and_then(std::fs::metadata))
        .map(|meta| meta.ino())
        .unwrap_or(0)
}

/// Everything the signal-driven teardown needs to hand the session to a fresh `exec` of the new
/// binary. Held by the signal task for the lifetime of the server.
pub struct Upgrade {
    pub gate: Arc<UpgradeGate>,
    pub inflight: Arc<Inflight>,
    pub install_path: PathBuf,
    pub handoff_dir: PathBuf,
    pub peer_info: InitializeRequestParams,
    pub negotiated_protocol_version: String,
}

/// Result of one teardown attempt. `Aborted` means we stayed on the current binary (drain timed
/// out); the success path never returns (`exec` replaces the process, or the process exits).
#[derive(Debug, PartialEq, Eq)]
pub enum UpgradeOutcome {
    Aborted,
}

impl Upgrade {
    /// Drain in-flight work at a request boundary, snapshot the session, and `exec` the new binary
    /// in place. Returns [`UpgradeOutcome::Aborted`] if the drain times out; on `exec` failure the
    /// process exits non-zero (it is already torn down). Otherwise it does not return.
    pub async fn run(&self) -> UpgradeOutcome {
        // 1. Close the gate; no new request is read past the next line boundary.
        self.gate.request();

        // 2. Drain in-flight tool calls, bounded — a stuck tool must not wedge the server.
        if tokio::time::timeout(DRAIN_TIMEOUT, self.inflight.wait_zero()).await.is_err() {
            eprintln!(
                "hot-upgrade: drain timed out after {}s; staying on the current binary",
                DRAIN_TIMEOUT.as_secs()
            );
            self.gate.abort();
            return UpgradeOutcome::Aborted;
        }

        // 3. Flush stdout so the last response is on the wire before we hand off.
        let _ = tokio::io::stdout().flush().await;

        // 4. Snapshot the session to a temp file.
        let handoff = HandoffV1::new(
            self.negotiated_protocol_version.clone(),
            self.peer_info.clone(),
            current_exe_inode(),
        );
        let handoff_path = match handoff.write_temp(&self.handoff_dir) {
            Ok(path) => path,
            Err(err) => {
                eprintln!("hot-upgrade: could not write handoff: {err}; aborting upgrade");
                self.gate.abort();
                return UpgradeOutcome::Aborted;
            },
        };

        // 5. `exec` the new binary, reusing our argv and inheriting stdio (fds 0/1/2 lack CLOEXEC,
        //    so the client's pipe is preserved). The handoff path rides the child env only — we
        //    never mutate our own env. The watcher's lock fds *are* CLOEXEC, so `exec` releases
        //    them; SQLite WAL recovers any pass interrupted mid-write.
        let err = Command::new(&self.install_path)
            .args(std::env::args_os().skip(1))
            .env(HANDOFF_PATH_ENV, &handoff_path)
            .exec();

        // `exec` only returns on failure.
        let _ = std::fs::remove_file(&handoff_path);
        eprintln!("hot-upgrade: exec {} failed: {err}", self.install_path.display());
        std::process::exit(EXEC_FAILED_EXIT);
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    #[test]
    fn protocol_gate_accepts_known_and_rejects_unknown_versions() {
        assert!(protocol_supported(ProtocolVersion::V_2025_06_18.as_str()));
        assert!(protocol_supported(ProtocolVersion::LATEST.as_str()));
        assert!(!protocol_supported("1999-01-01"));
        assert!(!protocol_supported(""));
    }

    #[tokio::test]
    async fn handoff_round_trips_through_temp_file() {
        let dir = std::env::temp_dir();
        let handoff =
            HandoffV1::new("2025-06-18".to_string(), InitializeRequestParams::default(), 42);
        let path = handoff.write_temp(&dir).unwrap();
        let read = HandoffV1::read_and_unlink(&path).unwrap();
        assert_eq!(read.format_version, HandoffV1::FORMAT_VERSION);
        assert_eq!(read.negotiated_protocol_version, "2025-06-18");
        assert_eq!(read.old_binary_inode, 42);
        assert!(read.residue.is_empty());
        assert!(!path.exists(), "temp file is unlinked after read");
    }

    /// The handoff file is a contract between two DIFFERENT binaries: the predecessor writes it,
    /// then `exec`s the newly installed one, which reads it. Those binaries can be built against
    /// different rmcp releases, so `peer_info` must stay readable across an rmcp upgrade — the
    /// whole point of the hot path is resuming without re-`initialize`. A model change that
    /// renames or newly requires a field inside `InitializeRequestParams` silently downgrades
    /// every hot-upgrade to a cold restart (`take_handoff` swallows the parse error), which no
    /// same-version round-trip test can catch. This literal is the exact JSON a predecessor emits.
    #[test]
    fn handoff_written_by_a_differently_built_binary_still_deserializes() {
        let written_by_predecessor = r#"{
            "format_version": 1,
            "negotiated_protocol_version": "2025-06-18",
            "peer_info": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "claude-code", "version": "2.0.0" }
            },
            "residue": [],
            "old_binary_inode": 42,
            "upgrade_started_unix_ms": 1700000000000
        }"#;
        let handoff: HandoffV1 = serde_json::from_str(written_by_predecessor)
            .expect("a predecessor's handoff must deserialize after an rmcp upgrade");
        assert_eq!(handoff.format_version, HandoffV1::FORMAT_VERSION);
        assert_eq!(handoff.negotiated_protocol_version, "2025-06-18");
        assert_eq!(handoff.peer_info.protocol_version.as_str(), "2025-06-18");
        assert_eq!(handoff.peer_info.client_info.name, "claude-code");
        assert!(protocol_supported(&handoff.negotiated_protocol_version));
    }

    #[tokio::test]
    async fn inflight_wait_zero_resolves_after_guards_drop() {
        let inflight = Inflight::new();
        let g1 = inflight.guard();
        let g2 = inflight.guard();
        assert_eq!(inflight.count(), 2);
        let waiter = {
            let inflight = Arc::clone(&inflight);
            tokio::spawn(async move { inflight.wait_zero().await })
        };
        drop(g1);
        drop(g2);
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("wait_zero must resolve once all guards drop")
            .unwrap();
        assert_eq!(inflight.count(), 0);
    }

    /// The load-bearing property: a single read delivers at most one line (through the first
    /// `\n`), so rmcp's outer `read_until(b'\n')` never buffers a following pipelined request that
    /// would be lost at `exec`.
    #[tokio::test]
    async fn gated_stdin_hands_at_most_one_line_per_read() {
        let gate = UpgradeGate::new();
        let input: &[u8] = b"first\nsecond\nthird\n";
        let mut gated = GatedStdin::with_reader(input, gate);
        let mut buf = [0u8; 64];
        let n = gated.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"first\n", "first read stops at the first newline");
        let n = gated.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"second\n", "second read yields the next line only");
    }

    /// Once an upgrade is pending and we're at a line boundary, the gate parks (no new line is
    /// delivered); aborting wakes it so serving resumes.
    #[tokio::test]
    async fn gated_stdin_parks_at_boundary_then_resumes_on_abort() {
        let gate = UpgradeGate::new();
        let input: &[u8] = b"alpha\nbeta\n";
        let mut gated = GatedStdin::with_reader(input, Arc::clone(&gate));
        let mut buf = [0u8; 64];
        // Read the first full line — leaves us exactly at a boundary.
        let n = gated.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"alpha\n");

        gate.request();
        // With the gate closed at the boundary, the next read must not complete.
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(100), gated.read(&mut buf));
        assert!(blocked.await.is_err(), "read parks while an upgrade is pending at a boundary");

        gate.abort();
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), gated.read(&mut buf))
            .await
            .expect("abort must wake the parked read")
            .unwrap();
        assert_eq!(&buf[..n], b"beta\n", "serving resumes with the next line after abort");
    }
}
