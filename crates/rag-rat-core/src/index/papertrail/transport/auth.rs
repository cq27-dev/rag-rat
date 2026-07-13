//! Secret-free auth resolution for tracker bindings: config names WHERE the token lives (an env
//! var or a command that prints it), never the token itself — `rag-rat.toml` stays committable.

use std::process::Command;

use anyhow::Context as _;

use crate::config::TrackerAuth;
use crate::index::papertrail::TrackerAuthentication;

/// Snapshot a binding's authentication capability without running configured shell code merely
/// because an index is opened for search/status. Environment lookup is side-effect free; a token
/// command is treated as configured and resolved later by explicit transport construction.
pub(crate) fn authentication(spec: Option<&TrackerAuth>) -> TrackerAuthentication {
    match spec {
        Some(TrackerAuth::Env(var))
            if std::env::var(var).is_ok_and(|token| !token.trim().is_empty()) =>
            TrackerAuthentication::AuthConfigured,
        Some(TrackerAuth::TokenCommand(command)) if !command.trim().is_empty() =>
            TrackerAuthentication::AuthConfigured,
        _ => TrackerAuthentication::AuthMissing,
    }
}

/// Resolve the binding's token. `None` spec → `Ok(None)` (anonymous); a configured-but-missing
/// token is an error — the operator asked for auth, silently degrading to the anonymous quota
/// would both surprise them and burn the shared unauthenticated pool. Error text names the env
/// var / command, never any token value.
pub(crate) fn resolve_token(spec: Option<&TrackerAuth>) -> anyhow::Result<Option<String>> {
    resolve_token_with(spec, |var| std::env::var(var).ok(), run_token_command)
}

/// Dependency-injected core: `env_lookup` stands in for the process environment and
/// `run_command` for the shell, because env mutation is `unsafe` (and flaky under nextest's
/// parallel runner) in Rust 2024 — the same pattern as the embedding backends' auth resolution.
fn resolve_token_with(
    spec: Option<&TrackerAuth>,
    env_lookup: impl Fn(&str) -> Option<String>,
    run_command: impl Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<Option<String>> {
    match spec {
        None => Ok(None),
        Some(TrackerAuth::Env(var)) => {
            let var = var.trim();
            anyhow::ensure!(!var.is_empty(), "tracker auth env var name is empty");
            let token = env_lookup(var)
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
                .with_context(|| {
                    format!(
                        "tracker auth env var `{var}` is configured but missing or empty in the \
                         environment"
                    )
                })?;
            Ok(Some(token))
        },
        Some(TrackerAuth::TokenCommand(command)) => {
            let command = command.trim();
            anyhow::ensure!(!command.is_empty(), "tracker token_command is empty");
            let token = run_command(command)?.trim().to_string();
            anyhow::ensure!(!token.is_empty(), "token_command `{command}` printed no token");
            Ok(Some(token))
        },
    }
}

/// Run the token command through the platform shell and return its stdout. Stderr is surfaced on
/// failure (that's where `gh`/`glab` explain a missing login); stdout is NOT — it may hold a
/// partial token.
fn run_token_command(command: &str) -> anyhow::Result<String> {
    let output = shell_command(command)
        .output()
        .with_context(|| format!("token_command `{command}` could not be spawned"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "token_command `{command}` failed ({}): {}",
            output.status,
            stderr.trim().chars().take(300).collect::<String>()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("cmd");
    shell.arg("/C").arg(command);
    shell
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.arg("-c").arg(command);
    shell
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_lookup(var: &str) -> Option<String> {
        std::env::var(var).ok()
    }

    #[test]
    fn none_spec_resolves_to_anonymous() {
        let token = resolve_token_with(None, env_lookup, run_token_command).unwrap();
        assert_eq!(token, None);
    }

    #[test]
    fn env_spec_resolves_and_trims_the_token() {
        let spec = TrackerAuth::Env("GITHUB_TOKEN".to_string());
        let token = resolve_token_with(
            Some(&spec),
            |var| (var == "GITHUB_TOKEN").then(|| "  ghp_tok \n".to_string()),
            run_token_command,
        )
        .unwrap();
        assert_eq!(token.as_deref(), Some("ghp_tok"));
    }

    #[test]
    fn env_spec_errors_when_the_var_is_missing_or_empty() {
        let spec = TrackerAuth::Env("GITHUB_TOKEN".to_string());
        let missing =
            resolve_token_with(Some(&spec), |_| None, run_token_command).unwrap_err().to_string();
        assert!(missing.contains("GITHUB_TOKEN"), "{missing}");
        let empty = resolve_token_with(Some(&spec), |_| Some("   ".to_string()), run_token_command)
            .unwrap_err()
            .to_string();
        assert!(empty.contains("missing or empty"), "{empty}");
        // A blank var NAME is a config mistake, not an anonymous binding.
        let blank = TrackerAuth::Env("  ".into());
        assert!(resolve_token_with(Some(&blank), |_| None, run_token_command).is_err());
    }

    #[test]
    fn command_spec_uses_trimmed_stdout() {
        let spec = TrackerAuth::TokenCommand("gh auth token".to_string());
        let token = resolve_token_with(Some(&spec), env_lookup, |command| {
            assert_eq!(command, "gh auth token");
            Ok("tok-from-command\n".to_string())
        })
        .unwrap();
        assert_eq!(token.as_deref(), Some("tok-from-command"));
    }

    #[test]
    fn command_spec_errors_on_failure_or_empty_output() {
        let spec = TrackerAuth::TokenCommand("gh auth token".to_string());
        let failed =
            resolve_token_with(Some(&spec), env_lookup, |_| anyhow::bail!("not logged in"))
                .unwrap_err()
                .to_string();
        assert!(failed.contains("not logged in"), "{failed}");
        let empty = resolve_token_with(Some(&spec), env_lookup, |_| Ok("  \n".to_string()))
            .unwrap_err()
            .to_string();
        assert!(empty.contains("printed no token"), "{empty}");
        assert!(
            resolve_token_with(
                Some(&TrackerAuth::TokenCommand(" ".into())),
                env_lookup,
                run_token_command,
            )
            .is_err(),
            "blank command is a config mistake"
        );
    }

    // The real shell path, kept portable: `echo` exists in both `sh -c` and `cmd /C`, and the
    // trim handles the `\r\n` that `cmd` emits.
    #[test]
    fn real_shell_round_trip_prints_and_trims_a_token() {
        let spec = TrackerAuth::TokenCommand("echo shell-token".to_string());
        let token = resolve_token(Some(&spec)).unwrap();
        assert_eq!(token.as_deref(), Some("shell-token"));
    }

    #[test]
    fn real_shell_failure_and_silence_are_errors() {
        // `exit 3` fails under both shells; the error names the command, never a token.
        let failed = resolve_token(Some(&TrackerAuth::TokenCommand("exit 3".to_string())))
            .unwrap_err()
            .to_string();
        assert!(failed.contains("exit 3"), "{failed}");
        // `exit 0` succeeds while printing nothing under both shells.
        let silent = resolve_token(Some(&TrackerAuth::TokenCommand("exit 0".to_string())))
            .unwrap_err()
            .to_string();
        assert!(silent.contains("printed no token"), "{silent}");
    }

    #[test]
    fn authentication_probes_env_but_defers_configured_commands_until_transport() {
        assert_eq!(authentication(None), TrackerAuthentication::AuthMissing);
        assert!(env_lookup("PATH").is_some(), "test process must expose PATH");
        assert_eq!(
            authentication(Some(&TrackerAuth::Env("PATH".to_string()))),
            TrackerAuthentication::AuthConfigured
        );
        assert_eq!(
            authentication(Some(&TrackerAuth::TokenCommand("exit 3".to_string()))),
            TrackerAuthentication::AuthConfigured
        );
        assert_eq!(
            authentication(Some(&TrackerAuth::TokenCommand("  ".to_string()))),
            TrackerAuthentication::AuthMissing
        );
        assert_eq!(
            authentication(Some(&TrackerAuth::Env("RAG_RAT_TEST_UNSET_TOKEN_VAR".to_string()))),
            TrackerAuthentication::AuthMissing
        );
    }
}
