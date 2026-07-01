//! The EPHEMERAL cookbook lifecycle (#318): rag-rat spawns a "cookbook" recipe as a subprocess that
//! provisions an on-demand Ollama box (e.g. a Modal GPU sandbox), prints a one-line handshake when
//! the box is serving, then stays alive until rag-rat tears it down at the end of the bulk
//! reconcile.
//!
//! THE PROCESS CONTRACT (rag-rat ⇄ cookbook — both sides build to this exact shape):
//! - INPUT via env `RAG_RAT_COOKBOOK_INPUT` = JSON
//!   `{"model","request_timeout_s","provision_timeout_s","gpu","ollama_num_parallel"}`.
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
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
#[cfg(windows)]
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::Embedder;
use super::openai::{OpenAiEmbedder, ProvisionedEmbedderParams};
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

/// The active cookbook child on Windows. Windows has no Unix-style process group, so quit-time
/// abort uses `taskkill /T` to terminate the child and descendants.
#[cfg(windows)]
static ACTIVE_CHILD_PID: AtomicU32 = AtomicU32::new(0);

/// One-time install guard for the process-wide SIGINT/SIGTERM handler.
#[cfg(unix)]
static SIGNAL_HANDLER_INSTALLED: OnceLock<()> = OnceLock::new();

/// SIGTERM→SIGKILL grace IN THE SIGNAL HANDLER (R2/#330-7). Must be >= the RECIPE teardown budget
/// (`cookbook/recipes/*-ollama.mts`'s `TEARDOWN_TIMEOUT_MS = 8000`), or a recipe whose `terminate`
/// takes >grace is SIGKILLed mid-teardown → the no-backstop pod keeps billing. We align it with the
/// `Drop` path's `TEARDOWN_GRACE` (10s) so both teardown paths give the recipe the same window. The
/// handler does NOT block this whole duration unconditionally: it polls the group in small steps
/// (`SIGNAL_TEARDOWN_POLL_NSEC`) and SIGKILLs early the instant the group is gone — so a fast
/// teardown still exits promptly, while a slow one gets the full budget before the backstop.
#[cfg(unix)]
const SIGNAL_TEARDOWN_GRACE_SECS: i64 = TEARDOWN_GRACE.as_secs() as i64;

/// Poll step for the signal handler's grace loop (50ms). Small enough that a fast recipe teardown
/// is detected (and the process exits) promptly, but coarse enough that the loop is mostly asleep.
/// `nanosleep` is async-signal-safe, so polling in steps is sound inside the handler.
#[cfg(unix)]
const SIGNAL_TEARDOWN_POLL_NSEC: i64 = 50 * 1_000_000;

/// The `sigaction`s that were installed BEFORE ours (e.g. init's `TerminalResetGuard`), saved per
/// signal at install so the handler can RESTORE + re-raise them — chaining the terminal reset / the
/// default disposition instead of clobbering it. Written ONCE under `SIGNAL_HANDLER_INSTALLED`
/// before any cookbook spawns (so the write happens-before any handler run); read only in the
/// async-signal-safe handler.
#[cfg(unix)]
static mut SAVED_SIGINT: Option<libc::sigaction> = None;
#[cfg(unix)]
static mut SAVED_SIGTERM: Option<libc::sigaction> = None;

static PROVISION_LOG_SINK: OnceLock<Mutex<Option<mpsc::Sender<String>>>> = OnceLock::new();

pub struct ProvisionLogSinkGuard {
    previous: Option<mpsc::Sender<String>>,
}

impl Drop for ProvisionLogSinkGuard {
    fn drop(&mut self) {
        let sink = PROVISION_LOG_SINK.get_or_init(|| Mutex::new(None));
        *sink.lock().expect("provision log sink poisoned") = self.previous.take();
    }
}

pub fn install_provision_log_sink(tx: mpsc::Sender<String>) -> ProvisionLogSinkGuard {
    let sink = PROVISION_LOG_SINK.get_or_init(|| Mutex::new(None));
    let previous = sink.lock().expect("provision log sink poisoned").replace(tx);
    ProvisionLogSinkGuard { previous }
}

/// The JSON written to `RAG_RAT_COOKBOOK_INPUT` for the cookbook subprocess.
#[derive(Debug, Clone, Serialize)]
pub struct CookbookInput {
    /// The Ollama server-side model name the box should serve (the `[remote] model`).
    pub model: String,
    /// Per-REQUEST HTTP timeout the cookbook may forward to its box config — the
    /// `OpenAiEmbedder`'s per-`/api/embed` budget. UNRELATED to provisioning; do NOT use it as
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
    /// Ollama server parallelism the recipe should set as `OLLAMA_NUM_PARALLEL`. rag-rat sends the
    /// user's `[remote] concurrency` CAP; the box is provisioned to handle up to that many
    /// parallel requests, and rag-rat tunes the actual client fan-out (within the cap) itself
    /// against the box.
    pub ollama_num_parallel: u32,
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
    /// name), or `null` for an unauthenticated box. `ready` carries only the endpoint + token;
    /// throughput tuning runs in rag-rat (Rust) against the box after this event.
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
    let line = match event {
        CookbookEvent::Status { phase, provider, detail } => {
            let provider = if provider.is_empty() { String::new() } else { format!("{provider} ") };
            let detail = if detail.is_empty() { String::new() } else { format!(": {detail}") };
            format!("[cookbook] {provider}{phase}{detail}")
        },
        CookbookEvent::Log { level, message } => {
            let level = if level.is_empty() { "info" } else { level.as_str() };
            format!("[cookbook] {level}: {message}")
        },
        CookbookEvent::Error { message } => format!("[cookbook] error: {message}"),
        // `Ready` is the handshake, consumed by the caller — never routed here.
        CookbookEvent::Ready { .. } => return,
    };
    emit_provision_log(line);
}

fn emit_provision_log(line: String) {
    let tx = PROVISION_LOG_SINK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("provision log sink poisoned")
        .clone();
    if let Some(tx) = tx {
        let _ = tx.send(line);
    } else {
        eprintln!("{line}");
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

#[cfg(all(test, unix))]
impl ProvisionedBox {
    /// The process-GROUP id, for the leak-safety harness (#334) to probe `group_alive(pgid)` AFTER
    /// `Drop`, asserting no fault leaves a leaked group. Test-only — production teardown owns it.
    pub(crate) fn pgid(&self) -> i32 {
        self.pgid
    }
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

    fn provision_cancellable(
        cookbook: &str,
        input: &CookbookInput,
        cancel: impl Fn() -> bool,
    ) -> anyhow::Result<ProvisionedBox> {
        Self::provision_with_command_cancellable(
            cookbook_command(cookbook),
            cookbook,
            input,
            PROVISION_TIMEOUT,
            cancel,
        )
    }

    /// The provisioning core, given an already-built `Command` (the public `provision` builds it
    /// via `cookbook_command`) and the handshake `timeout`. Split out so tests can drive the
    /// lifecycle with a portable stub recipe + a short timeout, without needing `node`/`npx` on
    /// the test machine; `label` names the cookbook in errors.
    fn provision_with_command(
        command: Command,
        label: &str,
        input: &CookbookInput,
        timeout: Duration,
    ) -> anyhow::Result<ProvisionedBox> {
        Self::provision_with_command_cancellable(command, label, input, timeout, || false)
    }

    fn provision_with_command_cancellable(
        mut command: Command,
        label: &str,
        input: &CookbookInput,
        timeout: Duration,
        cancel: impl Fn() -> bool,
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

        if cancel() {
            anyhow::bail!("cookbook `{label}` provisioning cancelled before start");
        }

        let mut child = command.spawn().map_err(|e| {
            anyhow::anyhow!("failed to spawn cookbook `{label}`: {e} (is `node`/`npx` on PATH?)")
        })?;

        // The child is its own group leader, so pgid == pid. Register it so the signal handler can
        // reach the box even though `Drop` won't run on Ctrl-C / process exit.
        #[cfg(unix)]
        let pgid = child.id() as i32;
        #[cfg(unix)]
        ACTIVE_PGID.store(pgid, Ordering::SeqCst);
        #[cfg(windows)]
        ACTIVE_CHILD_PID.store(child.id(), Ordering::SeqCst);

        if cancel() {
            #[cfg(unix)]
            teardown_group(pgid, &mut child);
            #[cfg(windows)]
            {
                let pid = child.id();
                taskkill_tree(pid);
                clear_active_child_pid(pid);
                let _ = child.wait();
            }
            #[cfg(all(not(unix), not(windows)))]
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            anyhow::bail!("cookbook `{label}` provisioning cancelled before handshake");
        }

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Drain stderr on a thread: forward each (length-capped) line to OUR stderr AND retain only
        // the last `MAX_CAPTURED_LINES` in a ring buffer, so a failure can report what the cookbook
        // last said WITHOUT an uncapped accumulator a hostile recipe could OOM us with.
        let (err_tx, err_rx) = mpsc::channel::<Vec<String>>();
        let stderr_handle = std::thread::spawn(move || {
            let mut captured: std::collections::VecDeque<String> =
                std::collections::VecDeque::new();
            let mut reader = BufReader::new(stderr);
            // `read_capped_line` bounds each line's memory at the READ level (a newline-less flood
            // is drained, not buffered) — `BufRead::lines()` would allocate the whole
            // line first.
            while let Ok(Some(line)) = read_capped_line(&mut reader) {
                emit_provision_log(format!("cookbook: {line}"));
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
            let mut reader = BufReader::new(stdout);
            // Bounded read (see `read_capped_line`): a hostile recipe emitting a giant newline-less
            // line on stdout can't OOM us before the JSON parse. A capped/truncated line simply
            // fails to parse as an event and is forwarded raw, exactly like other non-event noise.
            while let Ok(Some(line)) = read_capped_line(&mut reader) {
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
                    // Not a typed event (e.g. npx install noise) → forward raw. `line` is already
                    // length-capped by `read_capped_line`, so no further truncation is needed.
                    Err(_) => emit_provision_log(format!("cookbook: {line}")),
                }
            }
        });

        // Wait for the handshake, the child exiting first, or the provision timeout — whichever
        // comes first. Poll so we can notice an early exit without blocking on the
        // handshake channel.
        // On ANY provisioning-failure exit below, the group is reclaimed (SIGKILL) and THEN
        // `ACTIVE_PGID` cleared — `reap_group` keeps the pgid visible until the kill lands, so a
        // signal mid-reap still reaches the group rather than reading a prematurely-cleared 0.
        let deadline = Instant::now() + timeout;
        let (endpoint, auth_token) = loop {
            match hs_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(handshake) => break handshake,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The stdout thread ended without a `ready` event → the child closed stdout
                    // (exited).
                    #[cfg(unix)]
                    reap_group(pgid);
                    #[cfg(windows)]
                    clear_active_child_pid(child.id());
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
                        #[cfg(windows)]
                        clear_active_child_pid(child.id());
                        return Err(provision_failed(
                            label,
                            &mut child,
                            stderr_handle,
                            err_rx,
                            &last_error,
                        ));
                    }
                    if Instant::now() >= deadline {
                        // Timed out — but the recipe may hold a LIVE box mid-pull/verify, so give
                        // it its SIGTERM teardown window before SIGKILL
                        // (R1): `teardown_group` does SIGTERM → grace →
                        // SIGKILL. (`reap_group` = hard SIGKILL is ONLY for the
                        // already-exited paths above.)
                        #[cfg(unix)]
                        teardown_group(pgid, &mut child);
                        #[cfg(not(unix))]
                        {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                        #[cfg(windows)]
                        clear_active_child_pid(child.id());
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

/// Build the `CookbookInput` (the env-passed provisioning request) from an ephemeral remote config.
/// Split out from `provision_and_build` so the config→input mapping — notably that the configured
/// `gpu` is forwarded — is unit-testable without spawning a real recipe.
fn cookbook_input_for(remote: &RemoteEmbeddingConfig) -> CookbookInput {
    CookbookInput {
        model: remote.model.trim().to_string(),
        request_timeout_s: remote.request_timeout_s,
        // Give the recipe a provisioning budget just under the Rust hard ceiling, so ITS budget
        // runs out first (clean provider-side teardown) before the Rust SIGKILL backstop
        // fires.
        provision_timeout_s: PROVISION_TIMEOUT.as_secs().saturating_sub(20),
        // The configured GPU (provider-specific; validated by the provider at provision time). The
        // recipe picks its own default when `None`. Config validation guarantees `gpu` is only set
        // in ephemeral mode, which is the only mode reaching this function.
        gpu: remote.gpu.clone(),
        // The user's `[remote] concurrency` cap — the box's server parallelism ceiling. rag-rat
        // fans out up to this many parallel requests and auto-tunes the client knee within it.
        ollama_num_parallel: remote.bounded_concurrency(),
    }
}

/// Provision an ephemeral cookbook box for the selected `spec` over `remote` and build an
/// [`OpenAiEmbedder`] against it. The single place that wires `cookbook` → `CookbookInput` →
/// `CookbookProvisioner::provision` → `OpenAiEmbedder::from_provisioned`; shared by the reconcile
/// ephemeral chunk path AND the install probe (status.rs) so the model→input→handshake→embedder
/// chain isn't duplicated. The returned [`ProvisionedBox`] MUST be kept alive for as long as the
/// embedder is used (its `Drop` is the box teardown).
/// Returns `(embedder, box, persisted_remote, window_concurrency)`. `persisted_remote` keeps the
/// user's `concurrency` CAP (for the active-config meta); `window_concurrency` is the tuned client
/// knee (<= cap) — the embedder's real fan-out, which the reconcile path uses to size its selection
/// window so it doesn't load a cap-wide window the embedder will only drain `knee`-at-a-time.
/// Context for the in-Rust throughput sweep (see [`crate::index::ai::throughput_tune`]). Present
/// only on the reconcile path (which has the DB `conn` for the tune cache and the configured chunk
/// size); the install probe / wizard verify pass `None` (they just ping — no sweep).
pub(crate) struct TuneRequest<'a> {
    pub conn: &'a rusqlite::Connection,
    pub max_embedding_chars: usize,
    /// Whether to run a NEW concurrency sweep on a cache miss. The tune cache is ALWAYS consulted
    /// (a prior knee beats the raw cap); this only gates a fresh sweep — false for a bounded
    /// `--max-seconds` pass or a run too small to fan out, so we don't spend paid-box time
    /// measuring a fan-out the live loop can't use. See
    /// `throughput_tune::sweep_is_worthwhile`.
    pub allow_sweep: bool,
}

pub(crate) fn provision_and_build(
    remote: &RemoteEmbeddingConfig,
    spec: &EmbeddingModelSpec,
    tune: Option<TuneRequest<'_>>,
) -> anyhow::Result<(OpenAiEmbedder, ProvisionedBox, RemoteEmbeddingConfig, u32)> {
    provision_and_build_cancellable(remote, spec, || false, tune)
}

fn provision_and_build_cancellable(
    remote: &RemoteEmbeddingConfig,
    spec: &EmbeddingModelSpec,
    cancel: impl Fn() -> bool,
    tune: Option<TuneRequest<'_>>,
) -> anyhow::Result<(OpenAiEmbedder, ProvisionedBox, RemoteEmbeddingConfig, u32)> {
    let cookbook = remote
        .cookbook
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("ephemeral remote config has no cookbook"))?;
    let input = cookbook_input_for(remote);
    let provisioned = CookbookProvisioner::provision_cancellable(cookbook, &input, cancel)?;
    // `effective_remote` PERSISTS the user's `concurrency` CAP unchanged (cap model). The live
    // embedder + reconcile window use the tuned knee, CLAMPED to that cap. The knee comes from the
    // in-Rust sweep against the box with the REAL embedder (reconcile path); the install/verify
    // pings pass `tune = None` and just use the cap.
    let effective_remote = remote.clone();
    let cap = remote.bounded_concurrency();
    let client_concurrency = match tune {
        Some(t) => crate::index::ai::throughput_tune::tune_remote_concurrency(
            t.conn,
            remote.cookbook.as_deref().unwrap_or("cookbook"),
            &provisioned.endpoint,
            provisioned.auth_token.as_deref(),
            remote,
            spec,
            t.max_embedding_chars,
            t.allow_sweep,
        ),
        None => cap,
    };
    let embedder = OpenAiEmbedder::from_provisioned(ProvisionedEmbedderParams {
        endpoint: &provisioned.endpoint,
        auth_token: provisioned.auth_token.as_deref(),
        server_model: effective_remote.model.trim(),
        selected_model_id: spec.model_id,
        dim: spec.dim,
        request_timeout_s: effective_remote.request_timeout_s,
        batch_size: effective_remote.batch_size,
        concurrency: client_concurrency,
        max_batch_chars: effective_remote.max_batch_chars,
    });
    Ok((embedder, provisioned, effective_remote, client_concurrency))
}

/// Init-wizard ephemeral spin-up TEST: provision a cookbook box for `remote` + `spec`, embed a
/// single `"ping"` to confirm the full provision→handshake→embed chain works, then tear the box
/// down (the [`ProvisionedBox`] guard's `Drop` at end of scope). `Ok(())` on a clean round-trip, an
/// error otherwise. This path passes `tune = None`, so it does NOT run the throughput sweep (a
/// throwaway box wouldn't warm the reconcile cache) — it just verifies the round-trip at the user's
/// `[remote] concurrency` cap. Bounded by the same internal [`PROVISION_TIMEOUT`] (300s) that gates
/// `provision`.
///
/// This is the `pub` seam the CLI's Remote step probes against: `provision_and_build` is
/// `pub(crate)`, so a connection-less verify that the CLI can call lives HERE, not in the wizard.
/// The box is ALWAYS torn down before this returns (Drop is the teardown), so a passing test leaks
/// no billing instance. The caller still confirms intent via the type-`provision` gate; this fn
/// trusts that gate and just runs the round-trip.
pub fn verify_ephemeral_remote(
    remote: &RemoteEmbeddingConfig,
    spec: &EmbeddingModelSpec,
) -> anyhow::Result<()> {
    verify_ephemeral_remote_cancellable(remote, spec, || false)
}

pub fn verify_ephemeral_remote_cancellable(
    remote: &RemoteEmbeddingConfig,
    spec: &EmbeddingModelSpec,
    cancel: impl Fn() -> bool,
) -> anyhow::Result<()> {
    // `_box` is bound (not `_`) so it lives to end of scope — `OpenAiEmbedder` holds only the
    // endpoint URL, not the process, so dropping the box early would tear down the server before
    // the ping. Drop at function exit is the teardown (SIGTERM → grace → SIGKILL on the group).
    let (embedder, _box, _effective_remote, _knee) =
        provision_and_build_cancellable(remote, spec, cancel, None)?;
    embedder.embed_batch(&["ping".to_string()]).map_err(|err| {
        anyhow::anyhow!("ephemeral spin-up test embed failed for `{}`: {err}", spec.model_id)
    })?;
    Ok(())
}

/// The published cookbook npm scope+name. The single-token slash form
/// `@rag-rat/cookbook/<provider>` is normalized to this package + a `<provider>` arg (see
/// [`normalize_cookbook_tokens`]).
const COOKBOOK_PACKAGE: &str = "@rag-rat/cookbook";

/// The slash-form prefix we normalize: `@rag-rat/cookbook/`. A test pins it to
/// `COOKBOOK_PACKAGE` + "/" so the two can't drift.
const COOKBOOK_SLASH_PREFIX: &str = "@rag-rat/cookbook/";

/// Build the `Command` for a cookbook spec. The spec is split on whitespace into tokens (a package
/// or recipe path FOLLOWED by provider subcommand/args, e.g. `@rag-rat/cookbook modal`). The FIRST
/// token decides the runner; the first token + the rest are passed as SEPARATE process args:
/// - first token ends `.mjs`/`.js` → `node <first> <rest...>`
/// - first token ends `.ts`/`.mts` → `npx -y tsx <first> <rest...>` (recipes are `.mts`; `-y`
///   auto-confirms the `tsx` install, since the child's stdin is null and an unconfirmed prompt
///   would hang)
/// - else (a package spec) → `npx -y <first> <rest...>` (`-y` auto-confirms the npx install)
///
/// SLASH-FORM NORMALIZATION (#330-2): the docs/tests also show the single-token form
/// `@rag-rat/cookbook/<provider>`. Passed verbatim to `npx -y @rag-rat/cookbook/<provider>`, npx
/// treats the whole thing as a PACKAGE PATH (a subpath of a package that doesn't publish one) and
/// dispatch fails before the recipe ever sees `<provider>`. So we rewrite the FIRST token
/// `@rag-rat/cookbook/<provider>` → package `@rag-rat/cookbook` + arg `<provider>` BEFORE resolving
/// the runner. Scoped to OUR package only — any other slash path (a real package subpath, a
/// filesystem path) is left exactly as-is.
///
/// Whitespace splitting is SIMPLE (no shell quoting): a recipe path containing spaces is NOT
/// supported — point `cookbook` at a space-free path (or an npm spec) instead. Empty tokens are
/// dropped. An all-empty spec degrades to a bare `npx -y` (which fails to spawn → a clear error).
fn cookbook_command(cookbook: &str) -> Command {
    let raw: Vec<&str> = cookbook.split_whitespace().collect();
    let tokens = normalize_cookbook_tokens(&raw);
    let first = tokens.first().map(String::as_str).unwrap_or("");
    let rest = &tokens[tokens.len().min(1)..];
    let lower = first.to_ascii_lowercase();
    let mut c = if lower.ends_with(".mjs") || lower.ends_with(".js") {
        let mut c = Command::new("node");
        c.arg(first);
        c
    } else if lower.ends_with(".ts") || lower.ends_with(".mts") {
        // `-y` auto-confirms npx's install-confirm prompt for `tsx` (#330: stdin is
        // `Stdio::null()`, so an unconfirmed prompt would hang/fail on a machine without
        // `tsx` installed — same as the package-spec branch below).
        let mut c = Command::new("npx");
        c.args(["-y", "tsx", first]);
        c
    } else {
        let mut c = Command::new("npx");
        c.args(["-y", first]);
        c
    };
    c.args(rest);
    c
}

/// Normalize the whitespace-split cookbook tokens, rewriting the single-token slash form
/// `@rag-rat/cookbook/<provider>` into the two tokens `[@rag-rat/cookbook, <provider>]` so `npx -y`
/// dispatches the package with `<provider>` as its subcommand arg (#330-2). Only the FIRST token is
/// rewritten, and only for our own `@rag-rat/cookbook/` scope with a non-empty single-segment
/// provider — a deeper subpath (`@rag-rat/cookbook/a/b`), any other scope, or a filesystem path is
/// returned unchanged. All other tokens (an explicitly-supplied provider arg, etc.) pass through.
fn normalize_cookbook_tokens(tokens: &[&str]) -> Vec<String> {
    let Some((&first, rest)) = tokens.split_first() else {
        return Vec::new();
    };
    if let Some(provider) = first.strip_prefix(COOKBOOK_SLASH_PREFIX)
        && !provider.is_empty()
        && !provider.contains('/')
    {
        // `@rag-rat/cookbook/modal` (+ any already-present args) → `@rag-rat/cookbook modal
        // <args>`.
        let mut out = Vec::with_capacity(rest.len() + 2);
        out.push(COOKBOOK_PACKAGE.to_string());
        out.push(provider.to_string());
        out.extend(rest.iter().map(|t| t.to_string()));
        return out;
    }
    tokens.iter().map(|t| t.to_string()).collect()
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

/// Read one newline-terminated line from `reader` into a String, but stop reading at
/// `MAX_DRAIN_LINE` bytes — the rest of an over-long line is DRAINED to the newline without being
/// retained, so memory stays bounded REGARDLESS of input (#330-5). Returns `Ok(None)` at EOF with
/// no bytes read; otherwise `Ok(Some(line))` where `line` is at most ~`MAX_DRAIN_LINE` bytes plus
/// the elision marker.
///
/// WHY NOT `BufRead::lines()`: that allocates the ENTIRE line before any cap runs, so a cookbook
/// emitting a multi-GB line with no newline would OOM rag-rat before `cap_line` could truncate it.
/// Here the read buffer never grows past the cap; bytes beyond it are discarded as they arrive.
fn read_capped_line<R: BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        // Pull whatever the BufReader has buffered without copying it yet, so we can decide how
        // much to keep vs discard before growing `buf`.
        let available = reader.fill_buf()?;
        if available.is_empty() {
            // EOF. If we read nothing at all, signal end-of-stream; otherwise return the last
            // (newline-less) line.
            if buf.is_empty() && !truncated {
                return Ok(None);
            }
            break;
        }
        // Consume up to and including the next newline from THIS chunk.
        let (chunk, consumed, hit_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(nl) => (&available[..nl], nl + 1, true),
            None => (available, available.len(), false),
        };
        // Keep only up to the cap; everything past it is drained (read+discarded), never retained.
        if !truncated && buf.len() < MAX_DRAIN_LINE {
            let room = MAX_DRAIN_LINE - buf.len();
            let take = room.min(chunk.len());
            buf.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                truncated = true;
            }
        } else if !chunk.is_empty() {
            truncated = true;
        }
        reader.consume(consumed);
        if hit_newline {
            break;
        }
    }
    // Decode lossily so invalid UTF-8 from arbitrary npx output can't error the drain; then
    // re-apply `cap_line` (it appends the elision marker once we crossed the cap).
    let mut line = String::from_utf8_lossy(&buf).into_owned();
    if truncated && line.len() <= MAX_DRAIN_LINE {
        // Force the marker even when the lossy decode shrank `line` below the cap (multi-byte
        // replacement chars): we DID discard input, so the line must read as truncated.
        line.push_str("…[truncated]");
        return Ok(Some(line));
    }
    Ok(Some(cap_line(line)))
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
        #[cfg(windows)]
        clear_active_child_pid(self.child.id());
    }
}

/// Tear down the box's process GROUP: ask politely (killpg SIGTERM) so the cookbook can release its
/// cloud box, wait a grace period, then killpg SIGKILL if anything in the group is still alive. The
/// cookbook contract is "SIGTERM → tear down + exit 0"; the grace period gives it time to actually
/// reclaim the remote box. We signal the GROUP (not `child.id()`) because `npx` makes the recipe
/// holding the box a grandchild.
///
/// INVARIANT (#330-6 leak fix): `ACTIVE_PGID` stays SET until the group is FULLY reaped — it is
/// cleared at the END here (and in the early-return), NEVER at the start. Why: clearing it first
/// opened a race — a SIGINT/SIGTERM arriving during the grace poll would `swap(0)` and read 0, so
/// the handler did NOTHING and `_exit`ed, interrupting this in-progress teardown BEFORE its SIGKILL
/// backstop → a leaked live box. With the pgid kept visible, a mid-teardown signal's `swap(0)`
/// takes OWNERSHIP and the handler runs its OWN complete SIGTERM→grace→SIGKILL before exiting, so
/// the group is reaped either way. The `swap` is also what prevents a double-teardown (whoever
/// clears it owns it). If the handler already took it (swap returned non-0 there), our `store(0)`
/// at the end is a harmless idempotent re-clear.
///
/// We poll the WHOLE GROUP (`killpg(pgid, 0)`), NOT just the leader's `waitpid`. The `npx` leader
/// is the group leader, but the recipe GRANDCHILD is what holds the paid box and runs the SIGTERM
/// teardown. The leader can exit while the grandchild is still alive — at the START (the wrapper
/// exits right after `ready`, the #330-7 early-return edge) or mid-teardown (a stuck grandchild).
/// In BOTH cases trusting `waitpid` on the leader would skip the group teardown and orphan the box.
/// So we never trust the wrapper's exit: we reap it (no zombie) but always PROBE the group, and
/// only return once EVERY member is gone (`ESRCH`); a group still alive at the deadline gets
/// `killpg(SIGKILL)`.
#[cfg(unix)]
fn teardown_group(pgid: i32, child: &mut Child) {
    // Reap the immediate child if it already exited (avoid a zombie) — but DO NOT trust the
    // WRAPPER's exit as proof the GROUP is gone (#330-7): `npx`/Node is the wrapper, and the recipe
    // GRANDCHILD holding the paid box can OUTLIVE it (the wrapper exits/crashes right after `ready`
    // while the recipe keeps running). So always PROBE the group below; only return early when no
    // member survives.
    let _ = child.try_wait();
    if !group_alive(pgid) {
        // The whole group is genuinely gone (e.g. a failure path already killed it). Release the
        // pgid so a later signal doesn't act on a recycled pid.
        ACTIVE_PGID.store(0, Ordering::SeqCst);
        return;
    }
    killpg(pgid, libc::SIGTERM);
    let deadline = Instant::now() + TEARDOWN_GRACE;
    loop {
        // Reap the leader as soon as it exits so it doesn't linger as a zombie, but DO NOT treat
        // that as "the group is gone" — the box-holding grandchild may still be tearing down.
        let _ = child.try_wait();
        if !group_alive(pgid) {
            // Every member is gone (the recipe finished its teardown) → release the pgid.
            ACTIVE_PGID.store(0, Ordering::SeqCst);
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // The group is STILL alive past the grace period (a stuck grandchild teardown) → force the
    // whole group, reap the leader, THEN clear the pgid (only now is the group actually gone).
    killpg(pgid, libc::SIGKILL);
    let _ = child.wait();
    ACTIVE_PGID.store(0, Ordering::SeqCst);
}

/// `true` while ANY member of the process group `pgid` is still alive. `killpg(pgid, 0)` sends no
/// signal but performs the existence/permission check: success means at least one member exists;
/// `ESRCH` means the whole group is gone. This is what lets teardown wait on the box-holding
/// GRANDCHILD, not just the leader's `waitpid`.
///
/// EPERM is the honest unhappy case (R7): a group member exists that WE CANNOT SIGNAL — our SIGKILL
/// would also EPERM, so we cannot actually reclaim it. We still report it "alive" (so the caller
/// doesn't falsely believe the box is gone) and warn loudly that the box may leak, rather than
/// silently treating SIGKILL as effective.
///
/// SAFETY: `killpg(2)` with signal 0 touches no memory and only probes the group.
#[cfg(unix)]
fn group_alive(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only probes the group; no memory is touched.
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    // Portable errno read (works on every unix, unlike a glibc-specific `__errno_location`).
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => false, // the whole group is gone
        Some(libc::EPERM) => {
            // A member we can't signal — SIGKILL won't reach it either. Don't pretend we reclaimed
            // it: warn that the paid box may leak.
            eprintln!(
                "rag-rat: cookbook group {pgid} has a member we cannot signal (EPERM); the remote \
                 box may leak — check your cloud provider"
            );
            true
        },
        _ => true, // any other errno: assume a member is still around
    }
}

/// Async-signal-safe group-existence probe for the SIGNAL HANDLER's grace loop: `killpg(pgid, 0)`
/// returns 0 while ANY member is alive, fails with `ESRCH` once the whole group is gone. Unlike
/// [`group_alive`], this does NOT call `eprintln!` (stdio is NOT async-signal-safe) — on `EPERM` (a
/// member we cannot signal) it conservatively reports "alive" SILENTLY, so the handler simply rides
/// out the full grace and SIGKILL-backstops rather than performing forbidden I/O in the handler.
/// The loud EPERM leak warning stays on the `Drop`/`teardown_group` path (`group_alive`), which is
/// not a signal context.
///
/// SAFETY: `killpg(2)` with signal 0 touches no memory and only probes the group;
/// async-signal-safe.
#[cfg(unix)]
fn group_alive_in_handler(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only probes the group; no memory is touched; async-signal-safe.
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    // ESRCH = the whole group is gone (the only case that lets the handler exit early). Any other
    // errno (incl. EPERM) → assume a member is still around and keep waiting out the grace.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Reap a process group on a provisioning-FAILURE path (early exit / child already dead): SIGKILL
/// the group (no graceful wait — provisioning never reached the serving state), THEN clear
/// `ACTIVE_PGID`. Same invariant as `teardown_group`: the pgid stays visible until the group is
/// killed, so a signal mid-reap takes ownership and SIGKILLs rather than reading a cleared 0 and
/// skipping it.
#[cfg(unix)]
fn reap_group(pgid: i32) {
    killpg(pgid, libc::SIGKILL);
    ACTIVE_PGID.store(0, Ordering::SeqCst);
}

/// Abort the currently-active provisioning process group from outside the cookbook lifecycle —
/// used by the init wizard's quit path to tear down an in-flight ephemeral probe (Task 14b).
///
/// On unix: if `ACTIVE_PGID` is non-zero (a box is live), runs the same bounded
/// SIGTERM → grace → SIGKILL teardown as [`ProvisionedBox::drop`] does via `teardown_group`.
/// On non-unix: no-op (no process group machinery exists on non-unix).
///
/// INVARIANT (#330-6 leak fix — same as `teardown_group`/`reap_group`): `ACTIVE_PGID` is LOADED,
/// not swapped, at the START and stays VISIBLE until the group is FULLY reclaimed; it is cleared
/// ONLY at the END. Clearing it up front re-opened the money-leak race: a second SIGINT/SIGTERM
/// arriving during the ≤10s grace would have `handle_terminating_signal` read `ACTIVE_PGID == 0`,
/// do NOTHING, and `_exit` — interrupting THIS in-progress abort BEFORE its SIGKILL backstop → a
/// leaked live box. With the pgid kept visible, a mid-teardown signal's `swap(0)` takes OWNERSHIP
/// and runs its OWN complete SIGTERM→grace→SIGKILL, so the group is reaped either way (the `swap`
/// also prevents a double-teardown — whoever clears it owns it; our `store(0)` at the end is an
/// idempotent re-clear if the handler already took it).
///
/// Note: `teardown_group` requires a `&mut Child` to reap the leader (avoid a zombie). We don't
/// hold the `Child` here (the worker owns it), so we run the same bounded teardown inline against
/// the GROUP — SIGTERM, poll `group_alive`, SIGKILL backstop — and clear `ACTIVE_PGID` at the END.
/// The worker's detached thread (or the OS at exit) reaps the real leader.
pub fn abort_active_provisioning() {
    #[cfg(unix)]
    {
        // LOAD, not swap: keep the pgid VISIBLE to the signal handler until teardown finishes, so a
        // mid-grace signal can take ownership and SIGKILL-backstop rather than reading a cleared 0.
        let pgid = ACTIVE_PGID.load(Ordering::SeqCst);
        if pgid > 1 {
            killpg(pgid, libc::SIGTERM);
            let deadline = std::time::Instant::now() + TEARDOWN_GRACE;
            loop {
                if !group_alive(pgid) {
                    // The group is gone (graceful teardown finished) → release the pgid at the END.
                    ACTIVE_PGID.store(0, Ordering::SeqCst);
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // Still alive past the grace → force the whole group, THEN clear the pgid (only now is
            // the group actually gone). Clearing at the END is the load-bearing half of the fix.
            killpg(pgid, libc::SIGKILL);
            ACTIVE_PGID.store(0, Ordering::SeqCst);
        }
    }
    #[cfg(windows)]
    {
        let pid = ACTIVE_CHILD_PID.swap(0, Ordering::SeqCst);
        if pid != 0 {
            taskkill_tree(pid);
        }
    }
}

#[cfg(windows)]
fn clear_active_child_pid(pid: u32) {
    let _ = ACTIVE_CHILD_PID.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
}

#[cfg(windows)]
fn taskkill_tree(pid: u32) {
    let pid = pid.to_string();
    let _ = Command::new("taskkill").args(["/PID", &pid, "/T", "/F"]).status();
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
/// so without this a Ctrl-C mid-ephemeral-reconcile leaks the paid box. We use `sigaction` (not
/// `signal`) so we can SAVE the previously-installed handler per signal (e.g. init's
/// `TerminalResetGuard`) — the handler restores + re-raises it so the terminal reset / default
/// disposition still runs after we reclaim the box.
#[cfg(unix)]
fn install_signal_handler() {
    SIGNAL_HANDLER_INSTALLED.get_or_init(|| {
        // SAFETY: `sigaction(2)` installs our async-signal-safe handler and writes the PREVIOUS
        // action into our `SAVED_*` statics. This runs once (OnceLock) before any cookbook spawns,
        // so the writes happen-before any handler invocation reads them.
        unsafe {
            install_one(libc::SIGINT, &raw mut SAVED_SIGINT);
            install_one(libc::SIGTERM, &raw mut SAVED_SIGTERM);
        }
    });
}

/// Install `handle_terminating_signal` for `signo`, saving the previous action into `*saved`.
/// SAFETY: `sigaction` with a zeroed-then-populated `struct sigaction`; `saved` points at a valid
/// `static mut Option<sigaction>`.
#[cfg(unix)]
unsafe fn install_one(signo: libc::c_int, saved: *mut Option<libc::sigaction>) {
    unsafe {
        let mut new_action: libc::sigaction = std::mem::zeroed();
        // Cast through a fn POINTER (not the fn item) for the `sa_sigaction` usize.
        let handler = handle_terminating_signal as extern "C" fn(libc::c_int);
        new_action.sa_sigaction = handler as usize;
        libc::sigemptyset(&raw mut new_action.sa_mask);
        // No SA_RESTART/SA_SIGINFO: a plain `void(int)` handler is all we need.
        new_action.sa_flags = 0;
        let mut old_action: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(signo, &raw const new_action, &raw mut old_action) == 0 {
            *saved = Some(old_action);
        }
    }
}

/// SIGINT/SIGTERM handler (async-signal-safe — only `killpg`/`nanosleep`/`sigaction`/`raise`, no
/// allocation/locks/stdio): reclaim the active cookbook group with a real SIGTERM → grace → SIGKILL
/// backstop (so a hung recipe teardown can't leak the box), then RESTORE + re-raise the previously
/// installed handler (e.g. init's terminal reset) so it runs too. If there was no prior handler,
/// the default disposition (restored as `SIG_DFL` by `raise`) terminates us with the right status.
///
/// The grace is `SIGNAL_TEARDOWN_GRACE_SECS` (= `TEARDOWN_GRACE`, 10s) — long enough for the
/// recipe's own `TEARDOWN_TIMEOUT_MS` (8s) teardown to complete (#330-7: a 2s grace SIGKILLed Node
/// mid-pull of `sb.terminate()`/`podTerminate`, leaking the no-backstop pod). But we do NOT block
/// the full grace blindly: we POLL the group in `SIGNAL_TEARDOWN_POLL_NSEC` (50ms) steps and
/// SIGKILL-and-exit EARLY the instant the group is gone, so a fast teardown still terminates
/// promptly. Only a genuinely hung teardown rides out the whole budget before the backstop fires.
#[cfg(unix)]
extern "C" fn handle_terminating_signal(signo: libc::c_int) {
    let pgid = ACTIVE_PGID.swap(0, Ordering::SeqCst);
    if pgid > 1 {
        unsafe {
            // SIGTERM first so the recipe can release the cloud box…
            libc::killpg(pgid, libc::SIGTERM);
            // …then poll the group up to the full teardown grace, SIGKILLing the instant it's gone
            // (early exit for a fast teardown; a hung one waits out the budget). Step-sleeping with
            // `nanosleep` + the `group_alive_in_handler` probe is async-signal-safe (no
            // stdio/locks).
            let steps = (SIGNAL_TEARDOWN_GRACE_SECS * 1_000_000_000) / SIGNAL_TEARDOWN_POLL_NSEC;
            let step = libc::timespec { tv_sec: 0, tv_nsec: SIGNAL_TEARDOWN_POLL_NSEC };
            let mut elapsed = 0;
            while elapsed < steps && group_alive_in_handler(pgid) {
                libc::nanosleep(&raw const step, std::ptr::null_mut());
                elapsed += 1;
            }
            // SIGKILL backstop: a no-op if the group already exited (the early-out above), the
            // hard reclaim if the recipe teardown hung past the grace.
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    // Restore the previously-installed handler for THIS signal and re-raise, so init's terminal
    // reset (or the default disposition) runs and the process terminates with the right status.
    // SAFETY: reads a `static mut` written once at install (happens-before); `sigaction`/`raise`/
    // `_exit` are async-signal-safe.
    unsafe {
        // Read the saved action through the raw pointer (`.read()`, not a deref-of-ref) — sound for
        // a `static mut` whose write happened-before this handler, and `sigaction` is `Copy`.
        let saved = if signo == libc::SIGINT {
            (&raw const SAVED_SIGINT).read()
        } else {
            (&raw const SAVED_SIGTERM).read()
        };
        match saved {
            Some(prev) => {
                libc::sigaction(signo, &raw const prev, std::ptr::null_mut());
                libc::raise(signo);
                // If the restored handler returned (didn't terminate), fall through to _exit so we
                // don't loop back into our own (now-uninstalled) handler.
                libc::_exit(128 + signo);
            },
            None => libc::_exit(128 + signo),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
            ollama_num_parallel: 32,
        }
    }

    #[test]
    fn provision_cancellation_returns_before_spawning_command() {
        let err = CookbookProvisioner::provision_with_command_cancellable(
            Command::new("definitely-missing-rag-rat-cookbook-test-command"),
            "cancelled",
            &input(),
            Duration::from_millis(10),
            || true,
        )
        .unwrap_err();

        assert!(err.to_string().contains("cancelled before start"), "{err:#}");
    }

    #[test]
    fn provision_cancellation_after_spawn_tears_down_before_handshake() {
        let calls = AtomicUsize::new(0);
        let err = CookbookProvisioner::provision_with_command_cancellable(
            stub_command("stub_hang.sh", &[]),
            "cancelled",
            &input(),
            Duration::from_millis(50),
            || calls.fetch_add(1, Ordering::SeqCst) > 0,
        )
        .unwrap_err();

        assert!(err.to_string().contains("cancelled before handshake"), "{err:#}");
    }

    #[test]
    fn verify_ephemeral_remote_cancellable_stops_before_recipe_spawn() {
        let remote = RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            cookbook: Some("definitely-missing-rag-rat-cookbook-test-command".to_string()),
            query_endpoint: Some(crate::config::DEFAULT_QUERY_ENDPOINT.to_string()),
            ..RemoteEmbeddingConfig::default()
        };
        let spec = crate::embedding_models::spec("sentence-transformers/all-MiniLM-L6-v2").unwrap();

        let err = verify_ephemeral_remote_cancellable(&remote, spec, || true).unwrap_err();

        assert!(err.to_string().contains("cancelled before start"), "{err:#}");
    }

    fn tmp(name: &str) -> PathBuf {
        let id = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ragrat-cookbook-{}-{id}-{name}", std::process::id()))
    }

    #[test]
    fn cookbook_input_carries_configured_gpu_and_parallelism() {
        // The config→CookbookInput mapping must forward provider/runtime knobs to the recipe:
        // `gpu` selects the provider shape, and `ollama_num_parallel` aligns server concurrency
        // with the client-side remote request window.
        let ephemeral = |gpu: Option<&str>, concurrency: u32| RemoteEmbeddingConfig {
            model: "all-minilm".to_string(),
            cookbook: Some("@rag-rat/cookbook modal".to_string()),
            query_endpoint: Some(crate::config::DEFAULT_QUERY_ENDPOINT.to_string()),
            gpu: gpu.map(str::to_string),
            concurrency,
            ..RemoteEmbeddingConfig::default()
        };
        assert_eq!(cookbook_input_for(&ephemeral(Some("A10G"), 16)).gpu.as_deref(), Some("A10G"));
        assert_eq!(cookbook_input_for(&ephemeral(None, 16)).gpu, None);
        assert_eq!(cookbook_input_for(&ephemeral(None, 16)).ollama_num_parallel, 16);
        assert_eq!(cookbook_input_for(&ephemeral(None, 0)).ollama_num_parallel, 1);
        assert_eq!(
            cookbook_input_for(&ephemeral(
                None,
                crate::config::MAX_REMOTE_EMBEDDING_CONCURRENCY + 1
            ))
            .ollama_num_parallel,
            crate::config::MAX_REMOTE_EMBEDDING_CONCURRENCY
        );
        // The model is trimmed into the input regardless of gpu.
        assert_eq!(cookbook_input_for(&ephemeral(Some("A100"), 16)).model, "all-minilm");
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
        // Unknown extra fields (a `ts`, or anything a future recipe adds) are tolerated — the
        // variants intentionally don't `deny_unknown_fields` so the contract stays
        // forward-compatible.
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

        // `.ts`/`.mts` → `npx -y tsx <recipe>` — the `-y` auto-confirms the `tsx` install (#330:
        // the child's stdin is null, so an unconfirmed npx prompt would hang).
        let ts = cookbook_command("./recipe.ts");
        assert_eq!(prog(&ts), "npx");
        assert_eq!(args(&ts), vec!["-y", "tsx", "./recipe.ts"]);

        let mts = cookbook_command("./recipe.mts");
        assert_eq!(prog(&mts), "npx");
        assert_eq!(args(&mts), vec!["-y", "tsx", "./recipe.mts"]);

        // SLASH FORM (#330-2): the single-token `@rag-rat/cookbook/modal` is NORMALIZED to the
        // package + a `modal` arg, NOT passed verbatim (which npx would treat as a package subpath
        // and fail to dispatch). This is the same end command as the two-token form below.
        let pkg = cookbook_command("@rag-rat/cookbook/modal");
        assert_eq!(prog(&pkg), "npx");
        assert_eq!(args(&pkg), vec!["-y", "@rag-rat/cookbook", "modal"]);

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

    #[test]
    fn cookbook_slash_form_is_normalized_only_for_our_scope() {
        // #330-2: the slash prefix const must stay in lockstep with the package name.
        assert_eq!(COOKBOOK_SLASH_PREFIX, format!("{COOKBOOK_PACKAGE}/"));

        let norm = |s: &str| normalize_cookbook_tokens(&s.split_whitespace().collect::<Vec<_>>());

        // Our scope, single provider segment → split into package + provider arg.
        assert_eq!(norm("@rag-rat/cookbook/modal"), vec!["@rag-rat/cookbook", "modal"]);
        assert_eq!(norm("@rag-rat/cookbook/runpod"), vec!["@rag-rat/cookbook", "runpod"]);

        // Slash form WITH extra explicit args keeps them after the rewritten provider.
        assert_eq!(norm("@rag-rat/cookbook/modal --gpu T4"), vec![
            "@rag-rat/cookbook",
            "modal",
            "--gpu",
            "T4"
        ]);

        // A DEEPER subpath is NOT a provider subcommand — leave it verbatim (real package subpath).
        assert_eq!(norm("@rag-rat/cookbook/sub/dir"), vec!["@rag-rat/cookbook/sub/dir"]);
        // Trailing slash (empty provider) → unchanged.
        assert_eq!(norm("@rag-rat/cookbook/"), vec!["@rag-rat/cookbook/"]);
        // The two-token form is already correct → untouched.
        assert_eq!(norm("@rag-rat/cookbook modal"), vec!["@rag-rat/cookbook", "modal"]);
        // A DIFFERENT scope's slash path is left alone.
        assert_eq!(norm("@other/pkg/modal"), vec!["@other/pkg/modal"]);
        // A filesystem path is left alone.
        assert_eq!(norm("/abs/recipe.mjs"), vec!["/abs/recipe.mjs"]);
        // Empty spec → no tokens.
        assert!(norm("   ").is_empty());

        // End-to-end through cookbook_command: the runpod slash form resolves to the package + arg.
        let prog = |c: &Command| c.get_program().to_string_lossy().to_string();
        let args =
            |c: &Command| c.get_args().map(|a| a.to_string_lossy().to_string()).collect::<Vec<_>>();
        let rp = cookbook_command("@rag-rat/cookbook/runpod");
        assert_eq!(prog(&rp), "npx");
        assert_eq!(args(&rp), vec!["-y", "@rag-rat/cookbook", "runpod"]);
    }

    #[test]
    fn read_capped_line_truncates_a_giant_newlineless_line_without_unbounded_memory() {
        // #330-5: a cookbook can emit a huge line with NO newline. `read_capped_line` must drain it
        // bounded — the retained String stays within ~MAX_DRAIN_LINE + the marker, regardless of
        // how many bytes the source produces. We feed 4 MiB of 'a' with no '\n'.
        let huge = "a".repeat(4 * 1024 * 1024);
        let mut reader = BufReader::new(std::io::Cursor::new(huge.into_bytes()));
        let line = read_capped_line(&mut reader).unwrap().expect("a line");
        assert!(
            line.ends_with("…[truncated]"),
            "over-long line is marked truncated: {}",
            &line[..40]
        );
        // Retained length is bounded: the kept prefix is at most MAX_DRAIN_LINE bytes + the marker.
        assert!(
            line.len() <= MAX_DRAIN_LINE + "…[truncated]".len(),
            "retained {} bytes exceeds the cap",
            line.len()
        );
        // EOF after the (newline-less) line.
        assert!(read_capped_line(&mut reader).unwrap().is_none(), "stream is drained");
    }

    #[test]
    fn read_capped_line_passes_short_lines_through_and_splits_on_newlines() {
        let input = "first\nsecond line\n\nlast-no-newline";
        let mut reader = BufReader::new(std::io::Cursor::new(input.as_bytes().to_vec()));
        assert_eq!(read_capped_line(&mut reader).unwrap().as_deref(), Some("first"));
        assert_eq!(read_capped_line(&mut reader).unwrap().as_deref(), Some("second line"));
        assert_eq!(read_capped_line(&mut reader).unwrap().as_deref(), Some(""));
        assert_eq!(read_capped_line(&mut reader).unwrap().as_deref(), Some("last-no-newline"));
        assert_eq!(read_capped_line(&mut reader).unwrap(), None);
    }

    #[test]
    fn provision_does_not_oom_on_a_huge_unterminated_stdout_line() {
        // The integration form of #330-5: a stub emits a multi-MiB line WITHOUT a newline before
        // its `ready` event. The drain must stay bounded (the line is truncated, not
        // buffered whole) and the handshake must still parse. If `read_capped_line`
        // regressed to `lines()`, this would allocate the whole line first.
        let cmd = stub_command("stub_huge_line.sh", &[("STUB_ENDPOINT", "http://127.0.0.1:7")]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_huge_line.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parses despite a giant pre-ready line");
        assert_eq!(provisioned.endpoint, "http://127.0.0.1:7");
        drop(provisioned);
    }

    // ───────────────────────────────────────────────────────────────────────────────────────────
    // Leak-safety harness (#334): one parameterized table pinning the MONEY-LEAK invariant — "no
    // fault leaves a leaked cookbook process group". Every scenario drives a fault, then asserts
    // the group is GONE via `killpg(pgid, 0) == Err(ESRCH)` (i.e. `!group_alive(pgid)`). The
    // point is a single source of truth: a future teardown edge fails its case HERE instead of
    // reaching a reviewer (and a leaked GPU box). When the leak-class regresses, the failing
    // scenario names the gap. The standalone `drop_*`/`provision_*` tests above remain as
    // focused per-behavior probes; these harness cases pin the GROUP-level invariant uniformly
    // across the fault matrix.
    // ───────────────────────────────────────────────────────────────────────────────────────────

    /// A group-member pid (the box-holding grandchild) recorded by a stub, plus the group id. After
    /// the fault the harness asserts BOTH: the group probe is `ESRCH` AND the recorded member is
    /// reaped — restating the one invariant at the group and at a concrete member.
    struct GroupProbe {
        pgid: i32,
        /// The grandchild pid the stub wrote to its pidfile, if the scenario uses a grandchild
        /// stub.
        member: Option<i32>,
    }

    /// Wait (bounded) for the stub's grandchild to record its pid, then confirm it's alive — proof
    /// the box is "up" before we trigger the fault. Returns the grandchild pid.
    fn wait_for_grandchild(pidfile: &std::path::Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(s) = std::fs::read_to_string(pidfile)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                assert!(pid_alive(pid), "grandchild should be alive while the box is up");
                return pid;
            }
            assert!(Instant::now() < deadline, "grandchild never recorded its pid");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// THE invariant assertion: after the fault, the whole process GROUP must be gone. We poll
    /// briefly because some teardown paths (a lingering grandchild, the timeout's grace) complete
    /// asynchronously, but the group must be reaped well within the bound — a TRUE leak never
    /// clears.
    fn assert_group_reaped(probe: &GroupProbe, scenario: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while group_alive(probe.pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !group_alive(probe.pgid),
            "[{scenario}] LEAK: process group {} is still alive — a fault orphaned the box \
             (killpg(pgid,0) must be ESRCH)",
            probe.pgid,
        );
        if let Some(member) = probe.member {
            assert!(
                !pid_alive(member),
                "[{scenario}] LEAK: the box-holding grandchild {member} survived the teardown",
            );
        }
    }

    #[test]
    fn leak_safety_normal_serve_then_drop() {
        // Case 1 — the happy path: the box serves, then `Drop` (SIGTERM → the stub exits) reaps it.
        // The baseline the leak invariant must hold on, not just the fault paths.
        let cmd = stub_command("stub_ok.sh", &[("STUB_ENDPOINT", "http://127.0.0.1:1")]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_ok.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parsed");
        let pgid = provisioned.pgid();
        assert!(group_alive(pgid), "the group is alive while serving");
        drop(provisioned);
        assert_group_reaped(&GroupProbe { pgid, member: None }, "normal_serve_then_drop");
    }

    #[test]
    fn leak_safety_wrapper_exits_early_grandchild_lingers() {
        // Case 2 — THE Part-1 edge (#330-7). The WRAPPER (leader) exits RIGHT AFTER `ready` while
        // the box-holding grandchild lingers and IGNORES a direct SIGTERM. So by the time
        // `Drop` runs, `child.try_wait()` ALREADY reports the leader exited. The buggy
        // early-return trusted that as "the group is gone" → `store(0)` + return with NO
        // group probe / NO killpg → the grandchild (the paid box) is orphaned. This case
        // FAILS before the Part-1 fix and PASSES after it: the fix always probes the GROUP
        // and runs the full SIGTERM→grace→SIGKILL teardown.
        let pidfile = tmp("wrapper-early-gc-pid");
        let _ = std::fs::remove_file(&pidfile);
        let cmd = stub_command("stub_wrapper_exits_early.sh", &[
            ("STUB_ENDPOINT", "http://127.0.0.1:1"),
            ("STUB_GRANDCHILD_PIDFILE", pidfile.to_str().unwrap()),
        ]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_wrapper_exits_early.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parsed (wrapper emits ready, then exits)");
        let pgid = provisioned.pgid();
        let gc = wait_for_grandchild(&pidfile);

        drop(provisioned);
        assert_group_reaped(&GroupProbe { pgid, member: Some(gc) }, "wrapper_exits_early");
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn leak_safety_hung_grandchild_teardown_hits_the_sigkill_backstop() {
        // Case 3 — a STUCK grandchild teardown. The leader exits promptly on SIGTERM; the
        // grandchild ignores SIGTERM and lingers. Teardown must NOT return on the leader's
        // `waitpid` — it polls the GROUP and (for a long-enough hang) SIGKILLs at the grace
        // deadline. `stub_grandchild`'s grandchild ignores TERM and sleeps long, so only
        // the group SIGKILL backstop reaps it.
        let pidfile = tmp("hung-gc-pid");
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
        let pgid = provisioned.pgid();
        let gc = wait_for_grandchild(&pidfile);

        drop(provisioned);
        assert_group_reaped(&GroupProbe { pgid, member: Some(gc) }, "hung_grandchild_teardown");
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn leak_safety_provision_timeout_tears_down_a_live_box() {
        // Case 4 — a PROVISION_TIMEOUT while a box is LIVE. The stub never emits `ready` but spawns
        // a SIGTERM-ignoring grandchild, so the Rust provision deadline fires with the box
        // up. The timeout path must run the full `teardown_group` (R1: grace, not a hard
        // SIGKILL-only) and reap the group — NOT return the timeout error while leaking the
        // grandchild.
        let pidfile = tmp("timeout-live-gc-pid");
        let _ = std::fs::remove_file(&pidfile);
        // The box is never returned (it never serves), so the grandchild's pidfile is our handle on
        // the group. Run provisioning on a thread so we can read the pidfile while it blocks on the
        // (short, test-only) timeout, then resolve the group and assert it was reaped.
        let pf = pidfile.clone();
        let handle = std::thread::spawn(move || {
            CookbookProvisioner::provision_with_command(
                stub_command("stub_hang_grandchild.sh", &[(
                    "STUB_GRANDCHILD_PIDFILE",
                    pf.to_str().unwrap(),
                )]),
                "stub_hang_grandchild.sh",
                &input(),
                // A short test-only provision timeout (vs. the 5-min production ceiling).
                Duration::from_millis(600),
            )
        });
        let gc = wait_for_grandchild(&pidfile);
        // The grandchild is the group leader's child → its pgid is the group we tear down.
        let pgid = unsafe { libc::getpgid(gc) };
        assert!(pgid > 1, "resolved the grandchild's process group");

        let result = handle.join().expect("provision thread");
        let err = result.expect_err("a cookbook that never serves must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_group_reaped(&GroupProbe { pgid, member: Some(gc) }, "provision_timeout_live_box");
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn leak_safety_huge_unterminated_line_is_bounded_and_still_reaped() {
        // Case 5 — a huge newline-less stdout line before `ready`. Two invariants in one: the drain
        // stays BOUNDED (no OOM — covered byte-exactly by the `read_capped_line` unit test), AND
        // the box is still torn down cleanly afterward (no leak from the odd I/O shape).
        // The stub emits ~4 MiB with no newline, then `ready`, then parks until SIGTERM.
        let cmd = stub_command("stub_huge_line.sh", &[("STUB_ENDPOINT", "http://127.0.0.1:7")]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_huge_line.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parses despite a giant pre-ready line");
        let pgid = provisioned.pgid();
        drop(provisioned);
        assert_group_reaped(&GroupProbe { pgid, member: None }, "huge_unterminated_line");
    }

    // ── Case 6 — a signal arriving MID-TEARDOWN: the SIGNAL HANDLER's backstop must reap the
    // group.
    //
    // The handler ends in `_exit(128 + signo)` and never returns, so it can't run in-process (it
    // would kill the test runner). We RE-EXEC the test binary into an env-gated child harness
    // (`RAGRAT_LEAK_CASE6=1`): the child installs the signal handler, provisions a lingering-
    // grandchild stub (so `ACTIVE_PGID` is set to a live group), hands the grandchild pid back to
    // the parent via a file, and parks. The parent sends SIGTERM to the child, waits for it to
    // exit (through the handler's reap → restore → re-raise → `_exit`), then asserts the
    // grandchild — the only surviving group member after the leader-child exits — is GONE. That
    // proves the handler's backstop reaped the WHOLE group on the way out, not just the leader.

    #[test]
    fn leak_safety_mid_teardown_signal_handler_reaps_the_group() {
        // The PARENT side. If we're the re-exec'd child, run the child body instead and exit.
        if std::env::var_os("RAGRAT_LEAK_CASE6_CHILD").is_some() {
            case6_child_body();
            // case6_child_body parks until the parent's SIGTERM; the handler `_exit`s us. If it
            // ever returns (it shouldn't), fail loudly so the parent's wait sees a
            // non-handler exit.
            std::process::exit(97);
        }

        let exe = std::env::current_exe().expect("test binary path");
        let gc_pidfile = tmp("case6-grandchild-pid");
        let ready_file = tmp("case6-ready");
        let _ = std::fs::remove_file(&gc_pidfile);
        let _ = std::fs::remove_file(&ready_file);

        // Re-exec THIS exact test, single-threaded, into the child harness. The libtest filter is
        // the FULLY-QUALIFIED test path WITHOUT the crate-name segment: `module_path!()` is
        // `rag_rat_core::index::…::tests`, but libtest registers tests as `index::…::tests::<fn>`
        // (no crate prefix). `--exact` on the bare fn name (or the crate-prefixed path) selects
        // ZERO tests and the child never arms — strip the leading `<crate>::` so the filter
        // matches.
        let module =
            module_path!().split_once("::").map(|(_, rest)| rest).unwrap_or(module_path!());
        let test_path =
            format!("{module}::leak_safety_mid_teardown_signal_handler_reaps_the_group");
        let mut child = Command::new(&exe)
            .args(["--exact", &test_path, "--nocapture", "--test-threads=1"])
            .env("RAGRAT_LEAK_CASE6_CHILD", "1")
            .env("RAGRAT_LEAK_CASE6_GC_PIDFILE", &gc_pidfile)
            .env("RAGRAT_LEAK_CASE6_READY_FILE", &ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("re-exec the test binary as the case-6 child harness");

        // Wait for the child to signal "handler installed, box provisioned, parked".
        let deadline = Instant::now() + Duration::from_secs(20);
        let gc = loop {
            if ready_file.exists()
                && let Ok(s) = std::fs::read_to_string(&gc_pidfile)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                break pid;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("case-6 child never became ready");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        assert!(pid_alive(gc), "the child's box-holding grandchild is up before we signal");

        // Send SIGTERM to the CHILD (the leader of the cookbook group lives inside it). This lands
        // in the child's installed handler, which must SIGTERM→grace→SIGKILL the cookbook
        // group before exiting. We send to the child process directly (not its group) to
        // model a terminating signal delivered to rag-rat itself mid-run.
        unsafe {
            assert_eq!(libc::kill(child.id() as i32, libc::SIGTERM), 0, "SIGTERM to the child");
        }
        let status = child.wait().expect("child harness exits");
        // The handler re-raises SIGTERM after restoring the default disposition → death by signal
        // 15. (If the child returned 97/anything else, the handler path didn't run as
        // designed.)
        assert!(
            status.signal() == Some(libc::SIGTERM) || status.code() == Some(128 + libc::SIGTERM),
            "child should die via the re-raised SIGTERM, got {status:?}",
        );

        // THE assertion: the box-holding grandchild — the surviving group member after the child
        // process (the leader) is gone — must be reaped by the handler's backstop. Poll briefly for
        // the SIGKILL to land, then require it.
        let deadline = Instant::now() + Duration::from_secs(15);
        while pid_alive(gc) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !pid_alive(gc),
            "LEAK: a mid-teardown SIGTERM left the grandchild {gc} alive — the signal handler's \
             backstop must reap the WHOLE group on the way out",
        );
        let _ = std::fs::remove_file(&gc_pidfile);
        let _ = std::fs::remove_file(&ready_file);
    }

    /// The CHILD side of case 6 (runs only under `RAGRAT_LEAK_CASE6_CHILD`). Installs the real
    /// signal handler, provisions a lingering-grandchild stub (so `ACTIVE_PGID` points at a
    /// live group), records the grandchild pid + a readiness marker for the parent, then parks
    /// forever waiting for the parent's SIGTERM. The handler — not this code — terminates us,
    /// reaping the group on exit.
    fn case6_child_body() {
        let gc_pidfile = std::env::var("RAGRAT_LEAK_CASE6_GC_PIDFILE").expect("gc pidfile env");
        let ready_file = std::env::var("RAGRAT_LEAK_CASE6_READY_FILE").expect("ready file env");

        let cmd = stub_command("stub_grandchild.sh", &[
            ("STUB_ENDPOINT", "http://127.0.0.1:1"),
            ("STUB_GRANDCHILD_PIDFILE", gc_pidfile.as_str()),
        ]);
        // provision_with_command installs the signal handler and sets ACTIVE_PGID. We deliberately
        // LEAK the box (forget it) so `Drop` doesn't tear it down — the SIGNAL HANDLER must be what
        // reaps the group when the parent's SIGTERM arrives.
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_grandchild.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("child: handshake parsed");
        std::mem::forget(provisioned);

        // Tell the parent we're armed (handler installed, group live, grandchild pid recorded).
        std::fs::write(&ready_file, b"ready").expect("write ready marker");

        // Park forever; the parent's SIGTERM drives the handler, which `_exit`s us.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}
