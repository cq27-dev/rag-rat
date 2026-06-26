//! The EPHEMERAL cookbook lifecycle (#318): rag-rat spawns a "cookbook" recipe as a subprocess that
//! provisions an on-demand Ollama box (e.g. a Modal GPU sandbox), prints a one-line handshake when
//! the box is serving, then stays alive until rag-rat tears it down at the end of the bulk
//! reconcile.
//!
//! THE PROCESS CONTRACT (rag-rat ⇄ cookbook — both sides build to this exact shape):
//! - INPUT via env `RAG_RAT_COOKBOOK_INPUT` = JSON `{"model","request_timeout_s","gpu"}`.
//! - The cookbook prints ONE stdout line — the handshake `{"endpoint","auth_token"}` — when the box
//!   is serving, then stays alive. All other cookbook output goes to stderr.
//! - On SIGTERM the cookbook tears the box down and exits 0.
//! - If the cookbook exits BEFORE the handshake → provisioning failed (we capture its stderr).
//!
//! Teardown is GUARANTEED: [`ProvisionedBox`]'s `Drop` sends SIGTERM (then SIGKILL) on unix, so a
//! success, an error, or a panic anywhere in the reconcile loop still reclaims the remote box.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The env var carrying the cookbook's JSON input. The recipe reads + parses it at startup.
pub const COOKBOOK_INPUT_ENV: &str = "RAG_RAT_COOKBOOK_INPUT";

/// How long to wait for the handshake before giving up on provisioning. Cold-starting a GPU sandbox
/// + pulling a model can take a couple of minutes; 5 minutes is a generous ceiling.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait after SIGTERM before escalating to SIGKILL during teardown.
const TEARDOWN_GRACE: Duration = Duration::from_secs(10);

/// The JSON written to `RAG_RAT_COOKBOOK_INPUT` for the cookbook subprocess.
#[derive(Debug, Clone, Serialize)]
pub struct CookbookInput {
    /// The Ollama server-side model name the box should serve (the `[remote] model`).
    pub model: String,
    /// Per-request HTTP timeout the cookbook may forward to its box config.
    pub request_timeout_s: u64,
    /// GPU hint for the recipe (e.g. `"T4"`); `None` lets the recipe decide. Carried as JSON
    /// `null` when absent so the contract field is always present.
    pub gpu: Option<String>,
}

/// The one-line handshake the cookbook prints on stdout once its box is serving.
#[derive(Debug, Clone, Deserialize)]
struct Handshake {
    endpoint: String,
    /// A direct bearer token (NOT an env-var name) — the provisioned box's per-run credential, or
    /// `null` for an unauthenticated box.
    auth_token: Option<String>,
}

/// A live provisioned box: the parsed handshake + the running child. Holding this keeps the box
/// alive; dropping it tears the box down (SIGTERM → SIGKILL on unix).
#[derive(Debug)]
pub struct ProvisionedBox {
    /// The serving endpoint from the handshake (`https://...`); `/api/embed` is appended downstream.
    pub endpoint: String,
    /// The box's bearer token, or `None` for an unauthenticated box. A DIRECT token, not an env
    /// name.
    pub auth_token: Option<String>,
    child: Child,
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

        let mut child = command.spawn().map_err(|e| {
            anyhow::anyhow!("failed to spawn cookbook `{label}`: {e} (is `node`/`npx` on PATH?)")
        })?;

        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Drain stderr on a thread: forward each line to OUR stderr (so the operator sees
        // provisioning progress live) AND accumulate it, so a failure can report what the
        // cookbook last said.
        let (err_tx, err_rx) = mpsc::channel::<String>();
        let stderr_handle = std::thread::spawn(move || {
            let mut captured = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("cookbook: {line}");
                captured.push_str(&line);
                captured.push('\n');
            }
            let _ = err_tx.send(captured);
        });

        // Read stdout on a thread: the FIRST line that parses as a handshake is the signal; forward
        // every other stdout line to our stderr (it's diagnostic, not the handshake).
        let (hs_tx, hs_rx) = mpsc::channel::<Handshake>();
        let stdout_handle = std::thread::spawn(move || {
            let mut sent = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if !sent && let Ok(handshake) = serde_json::from_str::<Handshake>(line.trim()) {
                    let _ = hs_tx.send(handshake);
                    sent = true;
                    continue;
                }
                eprintln!("cookbook: {line}");
            }
        });

        // Wait for the handshake, the child exiting first, or the provision timeout — whichever
        // comes first. Poll so we can notice an early exit without blocking on the
        // handshake channel.
        let deadline = Instant::now() + timeout;
        let handshake = loop {
            match hs_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(handshake) => break handshake,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The stdout thread ended without a handshake → the child closed stdout
                    // (exited).
                    return Err(provision_failed(label, &mut child, stderr_handle, err_rx));
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Ok(Some(_status)) = child.try_wait() {
                        // Child exited before the handshake.
                        return Err(provision_failed(label, &mut child, stderr_handle, err_rx));
                    }
                    if Instant::now() >= deadline {
                        // Timed out: kill the child (it never served) and report.
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_handle.join();
                        let _ = stderr_handle.join();
                        anyhow::bail!(
                            "cookbook `{label}` did not print a handshake within {}s — \
                             provisioning timed out",
                            timeout.as_secs()
                        );
                    }
                },
            }
        };

        // The box is serving. The stdout thread keeps forwarding diagnostic lines until teardown;
        // detach it (the child closing stdout on SIGTERM ends it). The stderr thread likewise.
        drop(stdout_handle);
        drop(stderr_handle);
        Ok(ProvisionedBox { endpoint: handshake.endpoint, auth_token: handshake.auth_token, child })
    }
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

/// Build the "provisioning failed" error after the child exited before a handshake: reap it, join
/// the stderr drain, and include the captured stderr (the recipe's own diagnostics) in the message.
fn provision_failed(
    cookbook: &str,
    child: &mut Child,
    stderr_handle: std::thread::JoinHandle<()>,
    err_rx: mpsc::Receiver<String>,
) -> anyhow::Error {
    let status = child.wait().ok();
    let _ = stderr_handle.join();
    let captured = err_rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let tail: String = captured.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().fold(
        String::new(),
        |mut acc, l| {
            acc.push_str(l);
            acc.push('\n');
            acc
        },
    );
    anyhow::anyhow!(
        "cookbook `{cookbook}` exited (status {status:?}) before printing a handshake — \
         provisioning failed.\ncookbook stderr:\n{}",
        if tail.is_empty() { "<none>".to_string() } else { tail }
    )
}

impl Drop for ProvisionedBox {
    fn drop(&mut self) {
        // Already reaped? Nothing to do.
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        teardown(&mut self.child);
    }
}

/// Tear down the provisioned box: ask politely (SIGTERM) so the cookbook can release its cloud box,
/// wait a grace period, then SIGKILL if it's still alive. The cookbook contract is "SIGTERM → tear
/// down + exit 0", so the grace period is what gives it time to actually reclaim the remote box.
#[cfg(unix)]
fn teardown(child: &mut Child) {
    let pid = child.id() as i32;
    // SAFETY: `kill(2)` with a valid signal; a vanished PID just returns an ignored error.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + TEARDOWN_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return, // exited cleanly on SIGTERM
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => break,
        }
    }
    // Still alive (or wait errored) past the grace period → force it.
    let _ = child.kill();
    let _ = child.wait();
}

/// Non-unix teardown: `Child::kill` sends the platform terminate (no graceful SIGTERM). Ephemeral
/// provisioning is unix-only in practice (the cookbook contract relies on POSIX signals); this
/// keeps the build green elsewhere.
#[cfg(not(unix))]
fn teardown(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
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
        CookbookInput { model: "all-minilm".to_string(), request_timeout_s: 30, gpu: None }
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

    #[test]
    fn drop_tears_down_the_box() {
        // The stub touches STUB_TEARDOWN_MARKER on SIGTERM; after dropping the box, the marker must
        // appear — proof that `Drop` reclaimed the box (sent SIGTERM, the stub tore down).
        let marker = tmp("teardown-marker");
        let _ = std::fs::remove_file(&marker);
        let cmd = stub_command("stub_ok.sh", &[
            ("STUB_ENDPOINT", "http://127.0.0.1:1"),
            ("STUB_TEARDOWN_MARKER", marker.to_str().unwrap()),
        ]);
        let provisioned = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_ok.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect("handshake parsed");
        assert!(!marker.exists(), "marker must not exist while the box is alive");
        drop(provisioned);
        // The SIGTERM handler runs + touches the marker; give it a brief moment.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists(), "Drop must SIGTERM the box → the stub's teardown ran");
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn provision_errors_when_the_cookbook_exits_before_the_handshake() {
        let cmd = stub_command("stub_fail.sh", &[]);
        let err = CookbookProvisioner::provision_with_command(
            cmd,
            "stub_fail.sh",
            &input(),
            Duration::from_secs(10),
        )
        .expect_err("a cookbook that exits before the handshake must error");
        let msg = err.to_string();
        assert!(msg.contains("before printing a handshake"), "{msg}");
        // The captured stderr (the recipe's own failure message) is included for diagnosis.
        assert!(msg.contains("could not reach the cloud provider"), "stderr captured: {msg}");
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
