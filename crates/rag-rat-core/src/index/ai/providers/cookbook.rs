//! The EPHEMERAL cookbook lifecycle (#318): rag-rat spawns a "cookbook" recipe as a subprocess that
//! provisions an on-demand Ollama box (e.g. a Modal GPU sandbox), prints a one-line handshake when
//! the box is serving, then stays alive until rag-rat tears it down at the end of the bulk
//! reconcile.
//!
//! THE PROCESS CONTRACT (rag-rat ⇄ cookbook — both sides build to this exact shape):
//! - INPUT via env `RAG_RAT_COOKBOOK_INPUT` = JSON
//!   `{"model","request_timeout_s","provision_timeout_s","gpu"}`.
//! - The cookbook's STDOUT is a TYPED JSONL event stream — one JSON object per line, each with a
//!   `"type"` tag and a `"ts"` (epoch-ms) field (see [`CookbookEvent`]). The four `type`s are
//!   `status` (a provisioning-phase update: `provisioning`/`pulling`/`verifying`/`tearing_down`),
//!   `log` (a free-form `info`/`warn`/`error` message), `ready` (the box is SERVING: `{endpoint,
//!   auth_token}` — this is the handshake), and `error` (provisioning failed before `ready`). A
//!   line that does NOT parse as a typed event is forwarded raw to stderr (npx/npm install noise
//!   that precedes the cookbook's own output). All `status`/`log`/`error` events route through the
//!   ONE [`handle_event`] seam (the #329 status-bus hook); for now it renders a prefixed stderr
//!   line.
//! - On SIGTERM the cookbook tears the box down and exits 0.
//! - If the cookbook exits BEFORE a `ready` event → provisioning failed (we surface the last
//!   `error` event + the captured stderr tail).
//!
//! Teardown is GUARANTEED on unix by THREE mechanisms working together (a single one is not enough
//! — an adversarial review found the naive `kill(child.id())` is FALSE on the `npx` path):
//! 1. PROCESS GROUP: the cookbook spawns in its OWN process group (`process_group(0)`). `npx -y …`
//!    makes `npx` the immediate child and the Node recipe holding the box a GRANDCHILD, so killing
//!    `child.id()` would hit `npx`, not the recipe → an orphaned, leaked paid box. We `killpg` the
//!    WHOLE group instead, reaching the grandchild.
//! 2. `Drop` (SIGTERM-group → grace → SIGKILL-group) reclaims the box on success / error / panic.
//! 3. A process-wide SIGINT/SIGTERM HANDLER killpg's the active group before exit — `Drop` does NOT
//!    run on Ctrl-C / `exit()`, so a Ctrl-C mid-reconcile would otherwise leak the box.

use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::ollama::{OllamaEmbedder, ProvisionedEmbedderParams};
use crate::config::RemoteEmbeddingConfig;
use crate::embedding_models::EmbeddingModelSpec;

/// The env var carrying the cookbook's JSON input. The recipe reads + parses it at startup.
pub const COOKBOOK_INPUT_ENV: &str = "RAG_RAT_COOKBOOK_INPUT";

/// How long to wait for the handshake before giving up on provisioning. Cold-starting a GPU sandbox
/// + pulling a model can take a couple of minutes; 5 minutes is a generous ceiling.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait after SIGTERM before escalating to SIGKILL during teardown.
const TEARDOWN_GRACE: Duration = Duration::from_secs(10);

/// Cap on a single drained stdout/stderr line: a cookbook is arbitrary npx code, so a giant line
/// must not blow up rag-rat's memory. Excess is truncated with an elision marker.
const MAX_DRAIN_LINE: usize = 16 * 1024;

/// Cap on retained captured-stderr lines (the failure report's tail): keep only the last N so an
/// uncapped flood can't OOM us.
const MAX_CAPTURED_LINES: usize = 200;

/// The PGID of the SINGLE active cookbook process group (at most one ProvisionedBox is live per
/// reconcile). Set on provision, cleared on `Drop`. The signal handler reads it to killpg the box
/// on Ctrl-C / SIGTERM — where `Drop` never runs. `0` = none active (a real group leader pid is >
/// 1).
#[cfg(unix)]
static ACTIVE_PGID: AtomicI32 = AtomicI32::new(0);

/// One-time install guard for the process-wide SIGINT/SIGTERM handler.
#[cfg(unix)]
static SIGNAL_HANDLER_INSTALLED: OnceLock<()> = OnceLock::new();

/// The JSON written to `RAG_RAT_COOKBOOK_INPUT` for the cookbook subprocess.
#[derive(Debug, Clone, Serialize)]
pub struct CookbookInput {
    /// The Ollama server-side model name the box should serve (the `[remote] model`).
    pub model: String,
    /// Per-REQUEST HTTP timeout the cookbook may forward to its box config — the
    /// `OllamaEmbedder`'s per-`/api/embed` budget. UNRELATED to provisioning; do NOT use it as
    /// the boot budget.
    pub request_timeout_s: u64,
    /// The cookbook's PROVISIONING budget (seconds): how long the recipe may spend booting the
    /// box, pulling the model, and verifying it serves before giving up. Decoupled from
    /// `request_timeout_s` because remote boot + model pull over the proxy takes MINUTES, not the
    /// ~60s per-request default (the live RunPod e2e timed out at 60s). Set just UNDER the
    /// Rust-side hard [`PROVISION_TIMEOUT`] so the recipe's own budget expires first (a
    /// cleaner provider-side teardown) before the Rust SIGKILL backstop fires.
    pub provision_timeout_s: u64,
    /// GPU hint for the recipe (e.g. `"T4"`); `None` lets the recipe decide. Carried as JSON
    /// `null` when absent so the contract field is always present.
    pub gpu: Option<String>,
}

/// One line of the cookbook's typed JSONL stdout stream. Tagged on `"type"`; the `"ts"` field and
/// any unknown fields are tolerated/ignored (forward-compatible). A line that doesn't match any
/// variant is NOT a `CookbookEvent` (serde returns `Err`) and is forwarded raw to stderr.
///
/// `phase`/`level` are kept as plain `String`s rather than enums so a future recipe can add a phase
/// or level without breaking the parse — [`handle_event`] just renders them.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CookbookEvent {
    /// A provisioning-phase update (e.g. `provisioning`/`pulling`/`verifying`/`tearing_down`).
    Status {
        phase: String,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        detail: String,
    },
    /// A free-form log line at `info`/`warn`/`error`.
    Log {
        #[serde(default)]
        level: String,
        message: String,
    },
    /// The box is SERVING — the handshake. `auth_token` is a DIRECT bearer token (NOT an env-var
    /// name), or `null` for an unauthenticated box.
    Ready { endpoint: String, auth_token: Option<String> },
    /// Provisioning failed before `ready`.
    Error { message: String },
}

/// THE SEAM (#329): every non-`ready` cookbook event (`status`/`log`/`error`) is routed through
/// this ONE function. For NOW it renders a clean, prefixed line to rag-rat's stderr (the crate's
/// logging convention). When the ratatui `log` view (#329) lands, this is where the typed event is
/// pushed onto the status bus INSTEAD of (or in addition to) the stderr render — keep all event
/// presentation here so that change is a single edit. `ready` is NOT routed here (it's the
/// handshake the caller consumes); `error`'s message is ALSO retained by the caller for the
/// provision-failed context.
fn handle_event(event: &CookbookEvent) {
    match event {
        CookbookEvent::Status { phase, provider, detail } => {
            let provider = if provider.is_empty() { String::new() } else { format!("{provider} ") };
            let detail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
            eprintln!("[cookbook] {provider}{phase}{detail}");
        },
        CookbookEvent::Log { level, message } => {
            let level = if level.is_empty() { "info" } else { level.as_str() };
            eprintln!("[cookbook] {level}: {message}");
        },
        CookbookEvent::Error { message } => eprintln!("[cookbook] error: {message}"),
        // `Ready` is the handshake, consumed by the caller — never routed here.
        CookbookEvent::Ready { .. } => {},
    }
}

/// A live provisioned box: the parsed handshake + the running child. Holding this keeps the box
/// alive; dropping it tears the box down (killpg SIGTERM → SIGKILL on unix — the whole process
/// group, so an `npx`-spawned GRANDCHILD recipe is reached, not just the immediate `npx` child).
#[derive(Debug)]
pub struct ProvisionedBox {
    /// The serving endpoint from the handshake (`https://...`); `/api/embed` is appended downstream.
    pub endpoint: String,
    /// The box's bearer token, or `None` for an unauthenticated box. A DIRECT token, not an env
    /// name.
    pub auth_token: Option<String>,
    child: Child,
    /// The cookbook's process-GROUP id (= the immediate child's pid; it's the group leader because
    /// it spawned in its own group). Teardown `killpg`s this whole group. Unix only.
    #[cfg(unix)]
    pgid: i32,
}

/// Spawns the cookbook subprocess and provisions an ephemeral box, returning once it is serving.
pub struct CookbookProvisioner;

impl CookbookProvisioner {
    /// Spawn the `cookbook` recipe, hand it `input` via env, and block until it prints the
    /// handshake (the box is serving). Non-handshake stdout + all stderr are forwarded to
    /// rag-rat's stderr (the crate convention). On the handshake, returns a live
    /// [`ProvisionedBox`] whose `Drop` reclaims the box. Errors when the child exits before the
    /// handshake (with its captured stderr) or when [`PROVISION_TIMEOUT`] elapses.
    ///
    /// `cookbook` resolution: a `.mjs`/`.js` path → `node <path>`; a `.ts` path → `npx tsx <path>`;
    /// anything else → `npx -y <cookbook>` (an npm package spec).
    pub fn provision(cookbook: &str, input: &CookbookInput) -> anyhow::Result<ProvisionedBox> {
        Self::provision_with_command(cookbook_command(cookbook), cookbook, input, PROVISION_TIMEOUT)
    }

    /// The provisioning core, given an already-built `Command` (the public `provision` builds it
    /// via `cookbook_command`) and the handshake `timeout`. Split out so tests can drive the
    /// lifecycle with a portable stub recipe + a short timeout, without needing `node`/`npx` on
    /// the test machine; `label` names the cookbook in errors.
    fn provision_with_command(
        mut command: Command,
        label: &str,
        input: &CookbookInput,
        timeout: Duration,
    ) -> anyhow::Result<ProvisionedBox> {
        let input_json = serde_json::to_string(input)
            .map_err(|e| anyhow::anyhow!("failed to serialize cookbook input: {e}"))?;

        command
            .env(COOKBOOK_INPUT_ENV, input_json)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Put the cookbook in its OWN process group so teardown can killpg the WHOLE tree — `npx
        // -y` makes the recipe holding the box a grandchild, unreachable by a kill of the
        // immediate child. The group leader's pid == the child's pid.
        #[cfg(unix)]
        command.process_group(0);

        // Install the SIGINT/SIGTERM handler before the box exists, so a Ctrl-C during provisioning
        // (after the child spawns) still reclaims the group via `ACTIVE_PGID`.
        #[cfg(unix)]
        install_signal_handler();

        let mut child = command.spawn().map_err(|e| {
            anyhow::anyhow!("failed to spawn cookbook `{label}`: {e} (is `node`/`npx` on PATH?)")
        })?;

        // The child is its own group leader, so pgid == pid. Register it so the signal handler can
        // reach the box even though `Drop` won't run on Ctrl-C / process exit.
        #[cfg(unix)]
        let pgid = child.id() as i32;
        #[cfg(unix)]
        ACTIVE_PGID.store(pgid, Ordering::SeqCst);

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Drain stderr on a thread: forward each (length-capped) line to OUR stderr AND retain only
        // the last `MAX_CAPTURED_LINES` in a ring buffer, so a failure can report what the cookbook
        // last said WITHOUT an uncapped accumulator a hostile recipe could OOM us with.
        let (err_tx, err_rx) = mpsc::channel::<Vec<String>>();
        let stderr_handle = std::thread::spawn(move || {
            let mut captured: std::collections::VecDeque<String> =
                std::collections::VecDeque::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let line = cap_line(line);
                eprintln!("cookbook: {line}");
                captured.push_back(line);
                if captured.len() > MAX_CAPTURED_LINES {
                    captured.pop_front();
                }
            }
            let _ = err_tx.send(captured.into_iter().collect());
        });

        // Read stdout on a thread: parse each line as a typed `CookbookEvent`.
        //  - `Ready`  → the handshake signal `(endpoint, auth_token)` (sent ONCE).
        //  - `Status`/`Log`/`Error` → routed through the ONE `handle_event` seam (#329 status bus);
        //    `Error.message` is retained in `last_error` for the provision-failed context.
        //  - a line that does NOT parse as an event → forwarded raw to stderr (npx/npm noise).
        let (hs_tx, hs_rx) = mpsc::channel::<(String, Option<String>)>();
        let last_error = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let last_error_reader = std::sync::Arc::clone(&last_error);
        let stdout_handle = std::thread::spawn(move || {
            let mut sent = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                match serde_json::from_str::<CookbookEvent>(line.trim()) {
                    Ok(CookbookEvent::Ready { endpoint, auth_token }) =>
                        if !sent {
                            let _ = hs_tx.send((endpoint, auth_token));
                            sent = true;
                        },
                    Ok(event) => {
                        if let CookbookEvent::Error { message } = &event {
                            *last_error_reader.lock().unwrap() = Some(message.clone());
                        }
                        handle_event(&event);
                    },
                    // Not a typed event (e.g. npx install noise) → forward raw (length-capped).
                    Err(_) => eprintln!("cookbook: {}", cap_line(line)),
                }
            }
        });

        // Wait for the handshake, the child exiting first, or the provision timeout — whichever
        // comes first. Poll so we can notice an early exit without blocking on the
        // handshake channel.
        // On ANY provisioning-failure exit below, the group is reclaimed and `ACTIVE_PGID` cleared
        // (so a later signal can't killpg a recycled pid). `reap_group` does both.
        let deadline = Instant::now() + timeout;
        let (endpoint, auth_token) = loop {
            match hs_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(handshake) => break handshake,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The stdout thread ended without a `ready` event → the child closed stdout
                    // (exited).
                    #[cfg(unix)]
                    reap_group(pgid);
                    return Err(provision_failed(
                        label,
                        &mut child,
                        stderr_handle,
                        err_rx,
                        &last_error,
                    ));
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Ok(Some(_status)) = child.try_wait() {
                        // Child exited before a `ready` event.
                        #[cfg(unix)]
                        reap_group(pgid);
                        return Err(provision_failed(
                            label,
                            &mut child,
                            stderr_handle,
                            err_rx,
                            &last_error,
                        ));
                    }
                    if Instant::now() >= deadline {
                        // Timed out: kill the WHOLE group (it never served) and report.
                        #[cfg(unix)]
                        reap_group(pgid);
                        #[cfg(not(unix))]
                        {
                            let _ = child.kill();
                        }
                        let _ = child.wait();
                        let _ = stdout_handle.join();
                        let _ = stderr_handle.join();
                        anyhow::bail!(
                            "cookbook `{label}` did not emit a `ready` event within {}s — \
                             provisioning timed out",
                            timeout.as_secs()
                        );
                    }
                },
            }
        };

        // The box is serving. The stdout thread keeps routing `status`/`log` events until teardown;
        // detach it (the child closing stdout on SIGTERM ends it). The stderr thread likewise.
        drop(stdout_handle);
        drop(stderr_handle);
        Ok(ProvisionedBox {
            endpoint,
            auth_token,
            child,
            #[cfg(unix)]
            pgid,
        })
    }
}

/// Provision an ephemeral cookbook box for the selected `spec` over `remote` and build an
/// [`OllamaEmbedder`] against it. The single place that wires `cookbook` → `CookbookInput` →
/// `CookbookProvisioner::provision` → `OllamaEmbedder::from_provisioned`; shared by the reconcile
/// ephemeral chunk path AND the install probe (status.rs) so the model→input→handshake→embedder
/// chain isn't duplicated. The returned [`ProvisionedBox`] MUST be kept alive for as long as the
/// embedder is used (its `Drop` is the box teardown).
pub(crate) fn provision_and_build(
    remote: &RemoteEmbeddingConfig,
    spec: &EmbeddingModelSpec,
) -> anyhow::Result<(OllamaEmbedder, ProvisionedBox)> {
    let cookbook = remote
        .cookbook
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("ephemeral remote config has no cookbook"))?;
    let input = CookbookInput {
        model: remote.model.trim().to_string(),
        request_timeout_s: remote.request_timeout_s,
        // Give the recipe a provisioning budget just under the Rust hard ceiling, so ITS budget
        // runs out first (clean provider-side teardown) before the Rust SIGKILL backstop
        // fires.
        provision_timeout_s: PROVISION_TIMEOUT.as_secs().saturating_sub(20),
        gpu: None,
    };
    let provisioned = CookbookProvisioner::provision(cookbook, &input)?;
    let embedder = OllamaEmbedder::from_provisioned(ProvisionedEmbedderParams {
        endpoint: &provisioned.endpoint,
        auth_token: provisioned.auth_token.as_deref(),
        server_model: remote.model.trim(),
        selected_model_id: spec.model_id,
        dim: spec.dim,
        request_timeout_s: remote.request_timeout_s,
        batch_size: remote.batch_size,
    });
    Ok((embedder, provisioned))
}

/// Build the `Command` for a cookbook spec. The spec is split on whitespace into tokens (a package
/// or recipe path FOLLOWED by provider subcommand/args, e.g. `@rag-rat/cookbook modal`). The FIRST
/// token decides the runner; the first token + the rest are passed as SEPARATE process args:
/// - first token ends `.mjs`/`.js` → `node <first> <rest...>`
/// - first token ends `.ts`/`.mts` → `npx tsx <first> <rest...>` (recipes are `.mts`)
/// - else (a package spec) → `npx -y <first> <rest...>` (`-y` auto-confirms the npx install)
///
/// Whitespace splitting is SIMPLE (no shell quoting): a recipe path containing spaces is NOT
/// supported — point `cookbook` at a space-free path (or an npm spec) instead. Empty tokens are
/// dropped. An all-empty spec degrades to a bare `npx -y` (which fails to spawn → a clear error).
fn cookbook_command(cookbook: &str) -> Command {
    let tokens: Vec<&str> = cookbook.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");
    let rest = &tokens[tokens.len().min(1)..];
    let lower = first.to_ascii_lowercase();
    let mut c = if lower.ends_with(".mjs") || lower.ends_with(".js") {
        let mut c = Command::new("node");
        c.arg(first);
        c
    } else if lower.ends_with(".ts") || lower.ends_with(".mts") {
        let mut c = Command::new("npx");
        c.args(["tsx", first]);
        c
    } else {
        let mut c = Command::new("npx");
        c.args(["-y", first]);
        c
    };
    c.args(rest);
    c
}

/// Truncate a drained line to `MAX_DRAIN_LINE` chars with an elision marker, so a single huge line
/// from a hostile cookbook can't be retained or printed unbounded.
fn cap_line(mut line: String) -> String {
    if line.len() > MAX_DRAIN_LINE {
        // Truncate on a char boundary at or below the cap.
        let mut end = MAX_DRAIN_LINE;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str("…[truncated]");
    }
    line
}

/// Build the "provisioning failed" error after the child exited before a `ready` event: reap it,
/// join the stderr drain, and include the last `error` event message (the cookbook's own diagnosis)
/// + the captured raw stderr tail in the message.
fn provision_failed(
    cookbook: &str,
    child: &mut Child,
    stderr_handle: std::thread::JoinHandle<()>,
    err_rx: mpsc::Receiver<Vec<String>>,
    last_error: &std::sync::Mutex<Option<String>>,
) -> anyhow::Error {
    let status = child.wait().ok();
    let _ = stderr_handle.join();
    let captured = err_rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    // The drain already retained only the last `MAX_CAPTURED_LINES`; show the final 20 of those.
    let tail: String = captured.iter().rev().take(20).rev().fold(String::new(), |mut acc, l| {
        acc.push_str(l);
        acc.push('\n');
        acc
    });
    // The cookbook's OWN diagnosis (the last `error` event) is the most actionable line; lead with
    // it, then the raw stderr tail for context.
    let reported = last_error.lock().unwrap().clone();
    anyhow::anyhow!(
        "cookbook `{cookbook}` exited (status {status:?}) before a `ready` event — provisioning \
         failed.{}\ncookbook stderr:\n{}",
        match reported {
            Some(msg) => format!("\ncookbook error: {msg}"),
            None => String::new(),
        },
        if tail.is_empty() { "<none>".to_string() } else { tail }
    )
}

impl Drop for ProvisionedBox {
    #[cfg(unix)]
    fn drop(&mut self) {
        teardown_group(self.pgid, &mut self.child);
    }

    #[cfg(not(unix))]
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Tear down the box's process GROUP: ask politely (killpg SIGTERM) so the cookbook can release its
/// cloud box, wait a grace period, then killpg SIGKILL if anything in the group is still alive. The
/// cookbook contract is "SIGTERM → tear down + exit 0"; the grace period gives it time to actually
/// reclaim the remote box. We signal the GROUP (not `child.id()`) because `npx` makes the recipe
/// holding the box a grandchild. `ACTIVE_PGID` is cleared first so the signal handler won't also
/// act on a pgid we're already reclaiming.
///
/// We poll the WHOLE GROUP (`killpg(pgid, 0)`), NOT just the leader's `waitpid`. The `npx` leader
/// is the group leader, but the recipe GRANDCHILD is what holds the paid box and runs the SIGTERM
/// teardown. The leader can exit while the grandchild is still mid-teardown — if we returned the
/// instant the leader was reaped (the old bug), we'd skip the SIGKILL backstop and a STUCK
/// grandchild teardown would orphan the box. So we reap the leader (no zombie) but keep waiting on
/// the group: the grace period ends early only when EVERY member is gone (`ESRCH`), and a group
/// still alive at the deadline gets `killpg(SIGKILL)`.
#[cfg(unix)]
fn teardown_group(pgid: i32, child: &mut Child) {
    ACTIVE_PGID.store(0, Ordering::SeqCst);
    if matches!(child.try_wait(), Ok(Some(_))) {
        return; // already reaped (e.g. a failure path already killed the group)
    }
    killpg(pgid, libc::SIGTERM);
    let deadline = Instant::now() + TEARDOWN_GRACE;
    loop {
        // Reap the leader as soon as it exits so it doesn't linger as a zombie, but DO NOT treat
        // that as "the group is gone" — the box-holding grandchild may still be tearing down.
        let _ = child.try_wait();
        if !group_alive(pgid) {
            return; // every member of the group is gone (the recipe finished its teardown)
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // The group is STILL alive past the grace period (a stuck grandchild teardown) → force the
    // whole group, then reap the leader so it isn't left a zombie.
    killpg(pgid, libc::SIGKILL);
    let _ = child.wait();
}

/// `true` while ANY member of the process group `pgid` is still alive. `killpg(pgid, 0)` sends no
/// signal but performs the existence/permission check: success (or any errno other than `ESRCH`,
/// e.g. `EPERM`) means at least one member exists; `ESRCH` means the whole group is gone. This is
/// what lets teardown wait on the box-holding GRANDCHILD, not just the leader's `waitpid`. SAFETY:
/// `killpg(2)` with signal 0 touches no memory and only probes the group.
#[cfg(unix)]
fn group_alive(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only probes the group; no memory is touched.
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    // Portable errno read (works on every unix, unlike a glibc-specific `__errno_location`): only
    // ESRCH ("no such process group") means fully gone; EPERM etc. means a member is still around.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Reap a process group on a provisioning-FAILURE path (early exit / timeout): SIGKILL the group
/// (no graceful wait — provisioning never reached the serving state) and clear `ACTIVE_PGID`.
#[cfg(unix)]
fn reap_group(pgid: i32) {
    ACTIVE_PGID.store(0, Ordering::SeqCst);
    killpg(pgid, libc::SIGKILL);
}

/// `killpg(pgid, sig)` — signal the whole process group. SAFETY: `killpg(2)` with a valid signal; a
/// vanished group just returns an ignored error and touches no memory.
#[cfg(unix)]
fn killpg(pgid: i32, sig: i32) {
    if pgid > 1 {
        unsafe {
            libc::killpg(pgid, sig);
        }
    }
}

/// Install the process-wide SIGINT/SIGTERM handler ONCE. `Drop` does NOT run on Ctrl-C / `exit()`,
/// so without this a Ctrl-C mid-ephemeral-reconcile leaks the paid box. The handler killpg's the
/// active group (if any) then re-raises the default disposition so the process still dies.
#[cfg(unix)]
fn install_signal_handler() {
    SIGNAL_HANDLER_INSTALLED.get_or_init(|| {
        // SAFETY: `signal(2)` installs our async-signal-safe handler for SIGINT/SIGTERM. The
        // handler only reads an atomic + calls killpg/signal/_exit — all async-signal-safe.
        // Cast through a fn POINTER (not the fn item directly) for the `sighandler_t`
        // integer.
        let handler = handle_terminating_signal as extern "C" fn(libc::c_int);
        unsafe {
            libc::signal(libc::SIGINT, handler as libc::sighandler_t);
            libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        }
    });
}

/// SIGINT/SIGTERM handler: killpg the active cookbook group so a paid box isn't leaked on Ctrl-C,
/// then exit with the conventional 128+signo code. Async-signal-safe: reads one atomic, calls
/// `killpg` + `_exit` (no allocation, no locks, no stdio).
#[cfg(unix)]
extern "C" fn handle_terminating_signal(signo: libc::c_int) {
    let pgid = ACTIVE_PGID.swap(0, Ordering::SeqCst);
    if pgid > 1 {
        unsafe {
            // Politely first so the recipe can release the cloud box; SIGKILL backstops it.
            libc::killpg(pgid, libc::SIGTERM);
        }
    }
    // Exit promptly with the standard 128+signo status (re-raising the default disposition would be
    // cleaner, but `_exit` is the simplest async-signal-safe termination and the OS reaps us).
    unsafe {
        libc::_exit(128 + signo);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static N: AtomicU64 = AtomicU64::new(0);

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cookbook").join(name)
    }

    /// Build a `Command` that runs the stub shell script under `sh` (portable; no node/npx needed).
    /// `envs` supplies the per-test stub knobs (`STUB_ENDPOINT`, `STUB_AUTH`,
    /// `STUB_TEARDOWN_MARKER`).
    fn stub_command(script: &str, envs: &[(&str, &str)]) -> Command {
        let mut c = Command::new("sh");
        c.arg(fixture(script));
        for (k, v) in envs {
            c.env(k, v);
        }
        c
    }

    fn input() -> CookbookInput {
        CookbookInput {
            model: "all-minilm".to_string(),
            request_timeout_s: 30,
            provision_timeout_s: 280,
            gpu: None,
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let id = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ragrat-cookbook-{}-{id}-{name}", std::process::id()))
    }

    #[test]
    fn provision_parses_the_handshake_and_returns_endpoint_and_token() {
        let cmd = stub_command("stub_ok.sh", &[
            ("STUB_ENDPOINT", "http://127.0.0.1:54321"),
            ("STUB_AUTH", "sekret-token"),
        ]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_ok.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parsed");
        assert_eq!(provisioned.endpoint, "http://127.0.0.1:54321");
        assert_eq!(provisioned.auth_token.as_deref(), Some("sekret-token"));
        // Drop tears the box down (the stub exits on SIGTERM); this must not hang.
        drop(provisioned);
    }

    #[test]
    fn provision_handshake_with_null_token() {
        let cmd = stub_command("stub_ok.sh", &[("STUB_ENDPOINT", "http://127.0.0.1:1")]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_ok.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parsed");
        assert_eq!(provisioned.auth_token, None, "absent STUB_AUTH → null token");
    }

    /// `true` while the process with `pid` is still alive (`kill -0`). On unix, `kill(pid, 0)`
    /// succeeds iff the pid exists; ESRCH means gone.
    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn drop_tears_down_the_whole_process_group_reaching_the_grandchild() {
        // THE LEAK-FIX TEST (#318 2a): the stub spawns a GRANDCHILD that ignores a direct SIGTERM —
        // only a process-GROUP kill reaches it. If `Drop` killed just `child.id()` (the old bug),
        // the grandchild would survive (the leaked paid box). We provision, read the grandchild's
        // pid, drop the box, and assert the grandchild is GONE.
        let pidfile = tmp("grandchild-pid");
        let _ = std::fs::remove_file(&pidfile);
        let cmd = stub_command("stub_grandchild.sh", &[
            ("STUB_ENDPOINT", "http://127.0.0.1:1"),
            ("STUB_GRANDCHILD_PIDFILE", pidfile.to_str().unwrap()),
        ]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_grandchild.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parsed");

        // Wait for the grandchild to record its pid.
        let deadline = Instant::now() + Duration::from_secs(5);
        let gc_pid = loop {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "grandchild never recorded its pid");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(pid_alive(gc_pid), "grandchild should be alive while the box is up");

        drop(provisioned);

        // After Drop's killpg, the grandchild (which IGNORES direct TERM) must be reaped by the
        // GROUP signal — proof teardown signals the whole group, not just the immediate child.
        let deadline = Instant::now() + Duration::from_secs(10);
        while pid_alive(gc_pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(gc_pid), "Drop must killpg the GROUP → the grandchild is reaped");
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn drop_waits_for_the_group_when_the_leader_exits_before_the_grandchild() {
        // THE STUCK-TEARDOWN LEAK-FIX TEST (#318 2b): the leader (`npx` role) exits PROMPTLY on
        // SIGTERM while the box-holding GRANDCHILD is still tearing down. The OLD teardown returned
        // the instant the leader's `waitpid` succeeded → it skipped the SIGKILL backstop and `Drop`
        // returned with the grandchild (the paid box) still alive — a leak. The fix polls the whole
        // GROUP (`killpg(pgid, 0)`), so `Drop` must NOT return until the grandchild is gone too.
        let pidfile = tmp("lingering-grandchild-pid");
        let _ = std::fs::remove_file(&pidfile);
        let cmd = stub_command("stub_grandchild_lingers.sh", &[
            ("STUB_ENDPOINT", "http://127.0.0.1:1"),
            ("STUB_GRANDCHILD_PIDFILE", pidfile.to_str().unwrap()),
        ]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_grandchild_lingers.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parsed");

        // Wait for the grandchild to record its pid and confirm it's alive while the box is up.
        let deadline = Instant::now() + Duration::from_secs(5);
        let gc_pid = loop {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "grandchild never recorded its pid");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(pid_alive(gc_pid), "grandchild should be alive while the box is up");

        // `Drop` must SUPERVISE THE GROUP: it returns only once the lingering grandchild is also
        // gone (the old leader-only teardown would have returned while it was still alive).
        drop(provisioned);
        assert!(
            !pid_alive(gc_pid),
            "Drop must wait on the whole GROUP — the box-holding grandchild must be gone when it \
             returns, not just the leader",
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn provision_errors_when_the_cookbook_exits_before_the_ready_event() {
        // `stub_fail.sh` exits non-zero with only STDERR output (no typed `error` event) — the raw
        // stderr tail must still be captured in the failure.
        let cmd = stub_command("stub_fail.sh", &[]);
        let err = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_fail.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect_err("a cookbook that exits before `ready` must error");
        let msg = err.to_string();
        assert!(msg.contains("before a `ready` event"), "{msg}");
        // The captured stderr (the recipe's own failure message) is included for diagnosis.
        assert!(msg.contains("could not reach the cloud provider"), "stderr captured: {msg}");
    }

    #[test]
    fn provision_surfaces_the_last_error_event_message() {
        // `stub_error_event.sh` emits a typed `error` event before exiting — its message is the
        // cookbook's OWN diagnosis and must be surfaced (more actionable than the raw stderr tail).
        let cmd = stub_command("stub_error_event.sh", &[]);
        let err = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_error_event.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect_err("an error event before `ready` must fail provisioning");
        let msg = err.to_string();
        assert!(msg.contains("cookbook error:"), "leads with the error event: {msg}");
        assert!(msg.contains("no GPU capacity in region"), "surfaces the event message: {msg}");
    }

    #[test]
    fn provision_routes_status_log_events_and_still_parses_ready() {
        // `stub_events.sh` emits `status`/`log` events + a non-JSON noise line BEFORE `ready`. The
        // events must NOT be mistaken for the handshake, the noise line must be tolerated, and the
        // `ready` event must still yield the handshake (`endpoint`/`auth_token`).
        let cmd = stub_command("stub_events.sh", &[("STUB_ENDPOINT", "http://127.0.0.1:9")]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_events.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("status/log events before ready must not break handshake parsing");
        assert_eq!(provisioned.endpoint, "http://127.0.0.1:9");
        assert_eq!(provisioned.auth_token, None);
        drop(provisioned);
    }

    #[test]
    fn handle_event_renders_each_variant() {
        // The seam itself: exercise every non-ready variant (the #329 status bus will hook here).
        // No panic; the render goes to stderr. Defaults (empty provider/level) are tolerated.
        handle_event(&CookbookEvent::Status {
            phase: "pulling".to_string(),
            provider: "modal".to_string(),
            detail: "all-minilm".to_string(),
        });
        handle_event(&CookbookEvent::Status {
            phase: "verifying".to_string(),
            provider: String::new(),
            detail: String::new(),
        });
        handle_event(&CookbookEvent::Log {
            level: "warn".to_string(),
            message: "slow pull".to_string(),
        });
        handle_event(&CookbookEvent::Log {
            level: String::new(),
            message: "default level".to_string(),
        });
        handle_event(&CookbookEvent::Error { message: "boom".to_string() });
    }

    #[test]
    fn cookbook_event_parses_the_typed_variants_tolerating_ts_and_unknown_fields() {
        let parse = |s: &str| serde_json::from_str::<CookbookEvent>(s).unwrap();
        assert!(matches!(
            parse(r#"{"type":"ready","endpoint":"http://x","auth_token":null,"ts":1}"#),
            CookbookEvent::Ready { auth_token: None, .. }
        ));
        assert!(matches!(
            parse(r#"{"type":"ready","endpoint":"http://x","auth_token":"tok","ts":1}"#),
            CookbookEvent::Ready { auth_token: Some(_), .. }
        ));
        // Unknown extra field is tolerated.
        assert!(matches!(
            parse(
                r#"{"type":"status","phase":"pulling","provider":"modal","detail":"d","ts":2,"extra":true}"#
            ),
            CookbookEvent::Status { .. }
        ));
        assert!(matches!(
            parse(r#"{"type":"log","level":"info","message":"hi"}"#),
            CookbookEvent::Log { .. }
        ));
        assert!(matches!(
            parse(r#"{"type":"error","message":"x","ts":3}"#),
            CookbookEvent::Error { .. }
        ));
        // A line that is NOT a typed event must FAIL to parse (→ forwarded raw at runtime).
        assert!(serde_json::from_str::<CookbookEvent>("npm warn deprecated foo").is_err());
        assert!(serde_json::from_str::<CookbookEvent>(r#"{"endpoint":"http://x"}"#).is_err());
    }

    #[test]
    fn provision_times_out_when_no_handshake_arrives() {
        let cmd = stub_command("stub_hang.sh", &[]);
        let started = Instant::now();
        let err = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_hang.sh",
            &input(),
            Duration::from_millis(400),
        )
        .expect_err("a cookbook that never prints a handshake must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
        // It must respect the short timeout, not the 5-minute production ceiling.
        assert!(started.elapsed() < Duration::from_secs(3), "honored the short timeout");
    }

    #[test]
    fn cookbook_command_resolves_recipe_shapes() {
        // The resolver routes by the FIRST token's shape: .mjs/.js → node; .ts/.mts → npx tsx; else
        // → npx -y <spec>. The rest of the whitespace-split tokens (provider subcommand/args) are
        // passed through as SEPARATE args.
        let prog = |c: &Command| c.get_program().to_string_lossy().to_string();
        let args =
            |c: &Command| c.get_args().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>();

        // Single-token forms.
        let mjs = cookbook_command("./recipe.mjs");
        assert_eq!(prog(&mjs), "node");
        assert_eq!(args(&mjs), vec!["./recipe.mjs"]);

        let ts = cookbook_command("./recipe.ts");
        assert_eq!(prog(&ts), "npx");
        assert_eq!(args(&ts), vec!["tsx", "./recipe.ts"]);

        // `.mts` (the recipes' actual extension) routes through npx tsx too.
        let mts = cookbook_command("./recipe.mts");
        assert_eq!(prog(&mts), "npx");
        assert_eq!(args(&mts), vec!["tsx", "./recipe.mts"]);

        let pkg = cookbook_command("@rag-rat/cookbook/modal");
        assert_eq!(prog(&pkg), "npx");
        assert_eq!(args(&pkg), vec!["-y", "@rag-rat/cookbook/modal"]);

        // Multi-token: package + provider subcommand → `npx -y <pkg> <subcommand>`.
        let pkg_sub = cookbook_command("@rag-rat/cookbook modal");
        assert_eq!(prog(&pkg_sub), "npx");
        assert_eq!(args(&pkg_sub), vec!["-y", "@rag-rat/cookbook", "modal"]);

        // Multi-token: a recipe path + an arg → `node <path> <arg>`.
        let path_arg = cookbook_command("/abs/cli.mjs runpod");
        assert_eq!(prog(&path_arg), "node");
        assert_eq!(args(&path_arg), vec!["/abs/cli.mjs", "runpod"]);

        // Extra whitespace + empty tokens are trimmed.
        let spaced = cookbook_command("  @rag-rat/cookbook   modal  ");
        assert_eq!(prog(&spaced), "npx");
        assert_eq!(args(&spaced), vec!["-y", "@rag-rat/cookbook", "modal"]);
    }
}
