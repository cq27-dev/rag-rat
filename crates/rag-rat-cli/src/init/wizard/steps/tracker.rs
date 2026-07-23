//! Papertrail step — bind an issue tracker: auto-detect from the `origin` remote (the zero-config
//! default) or configure an explicit `[[tracker]]` binding.

use rag_rat_base::config::{Tracker, valid_tracker_base_url, valid_tracker_project};
use rag_rat_papertrail::ResolvedTracker;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::draft::{TrackerAuthMode, TrackerDraft, TrackerMode};
use super::super::state::WizardState;
use super::super::theme;
use super::embedding::one_line_field;
use super::types::{CheckResult, Outcome, PapertrailZone, StepState};
use super::widgets::{help_panel, option_list};

const PROVIDERS: [Tracker; 4] =
    [Tracker::Github, Tracker::Gitlab, Tracker::Bitbucket, Tracker::Jira];

/// Whether an issue tracker will actually be bound — the gate the Distill step keys on.
/// `AutoDetect` binds only when the `origin` remote resolved to a tracker; `Configure` binds only
/// when the explicit binding will actually resolve (see [`configured_tracker_resolves`]).
pub(crate) fn tracker_is_set_up(state: &WizardState) -> bool {
    let t = &state.draft.tracker;
    match t.mode {
        // AutoDetect binds `origin` at runtime, regardless of the binding's configured remote.
        TrackerMode::AutoDetect => state.detected_origin.is_some(),
        TrackerMode::Configure =>
            configured_tracker_resolves(t, state.detected_configured.as_ref()),
    }
}

/// Whether a `Configure` binding will actually resolve to a tracker. An explicit project always
/// resolves; Jira without one never does. A code host with no project must derive it from its git
/// remote — and `detected` is the resolution of the binding's CONFIGURED remote (origin by
/// default; see `WizardState::with_cookbooks`), so this verifies any named remote rather than
/// trusting it: it resolves only when that remote is a recognized host for the SAME provider.
fn configured_tracker_resolves(t: &TrackerDraft, detected: Option<&ResolvedTracker>) -> bool {
    if !t.project.trim().is_empty() {
        return true;
    }
    if t.provider == Tracker::Jira {
        return false; // Jira never auto-derives a project.
    }
    // A self-hosted `base_url` targets a specific host, but the configured remote may point at a
    // DIFFERENT host (e.g. base_url = github.example.com, origin = github.com). Deriving the
    // project from that remote would resolve the wrong repository, so require an explicit project.
    if !t.base_url.trim().is_empty() {
        return false;
    }
    // No explicit project → it must derive from the git remote, which we can only VERIFY for the
    // remote `detected` covers (the binding's configured remote, origin by default): it resolves
    // when that remote is a recognized host for the SAME provider. A self-hosted host, or a remote
    // we can't recognize, needs an explicit project — the wizard asks for one rather than save a
    // binding the runtime resolver might silently drop. (The config file still derives a project
    // from a self-hosted remote for a hand-written binding; this is a wizard-level nudge toward
    // being explicit, not a config constraint — precise verification would need a git read on every
    // keystroke.)
    detected.map(|d| d.provider) == Some(t.provider)
}

/// The focusable zones in render order for the current mode: auto-detect shows only the mode
/// selector; configure reveals the form, with the auth-value field present only for non-anonymous
/// auth. The step's `focus: usize` indexes into this list.
fn papertrail_zones(t: &TrackerDraft) -> Vec<PapertrailZone> {
    let mut zones = vec![PapertrailZone::Mode];
    if t.mode == TrackerMode::Configure {
        zones.push(PapertrailZone::Provider);
        zones.push(PapertrailZone::Auth);
        if t.auth != TrackerAuthMode::Anonymous {
            zones.push(PapertrailZone::AuthValue);
        }
        zones.push(PapertrailZone::BaseUrl);
        zones.push(PapertrailZone::Project);
        zones.push(PapertrailZone::Tags);
    }
    zones
}

fn border(focused: bool) -> Style {
    if focused { theme::focused_border() } else { theme::border() }
}

fn is_editable(zone: PapertrailZone) -> bool {
    matches!(
        zone,
        PapertrailZone::AuthValue
            | PapertrailZone::BaseUrl
            | PapertrailZone::Project
            | PapertrailZone::Tags
    )
}

pub(super) fn render_papertrail(f: &mut Frame, area: Rect, state: &WizardState) {
    let Some(StepState::Papertrail { focus }) = state.step else { return };
    let t = &state.draft.tracker;
    let zones = papertrail_zones(t);
    let focused = |z: PapertrailZone| zones.get(focus).copied() == Some(z);

    let outer = Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).split(area);

    let mode_opts = vec![
        ("auto-detect from origin".to_string(), theme::base()),
        ("configure explicitly".to_string(), theme::base()),
    ];
    let mode_sel = match t.mode {
        TrackerMode::AutoDetect => 0,
        TrackerMode::Configure => 1,
    };
    option_list(
        f,
        outer[0],
        "Tracker  ↑↓ move  ←→ change",
        &mode_opts,
        mode_sel,
        focused(PapertrailZone::Mode),
    );

    match t.mode {
        TrackerMode::AutoDetect => render_autodetect_body(f, outer[1], state),
        TrackerMode::Configure => render_configure_body(f, outer[1], state, focus, &zones),
    }
}

fn render_autodetect_body(f: &mut Frame, area: Rect, state: &WizardState) {
    let mut lines = match &state.detected_origin {
        Some(det) => vec![Line::from(Span::styled(
            format!(" origin → {}: {}", det.provider.as_db_str(), det.project),
            theme::success(),
        ))],
        None => vec![Line::from(Span::styled(
            " No tracker detected from the origin remote — nothing will be mirrored.",
            theme::warning(),
        ))],
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Derives the tracker from the git origin remote. Writes nothing; re-detected on every run.",
        theme::muted(),
    )));
    lines.push(Line::from(Span::styled(
        "Choose 'configure' to pin auth, a self-hosted instance, or Jira (never auto-detected).",
        theme::muted(),
    )));
    help_panel(f, area, "Auto-detect", lines);
}

fn render_configure_body(
    f: &mut Frame,
    area: Rect,
    state: &WizardState,
    focus: usize,
    zones: &[PapertrailZone],
) {
    let t = &state.draft.tracker;
    let focused = |z: PapertrailZone| zones.get(focus).copied() == Some(z);
    let rows = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .split(area);

    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[0]);
    let detected = state.detected_configured.as_ref().map(|d| d.provider);
    let provider_opts: Vec<(String, Style)> = PROVIDERS
        .iter()
        .map(|p| {
            let style = if detected == Some(*p) { theme::accent() } else { theme::base() };
            (p.as_db_str().to_string(), style)
        })
        .collect();
    let provider_sel = PROVIDERS.iter().position(|p| *p == t.provider).unwrap_or(0);
    option_list(
        f,
        cols[0],
        "provider",
        &provider_opts,
        provider_sel,
        focused(PapertrailZone::Provider),
    );

    let auth_opts = vec![
        ("anonymous".to_string(), theme::base()),
        ("env var".to_string(), theme::base()),
        ("command".to_string(), theme::base()),
    ];
    let auth_sel = match t.auth {
        TrackerAuthMode::Anonymous => 0,
        TrackerAuthMode::Env => 1,
        TrackerAuthMode::Command => 2,
    };
    option_list(f, cols[1], "auth", &auth_opts, auth_sel, focused(PapertrailZone::Auth));

    if t.auth != TrackerAuthMode::Anonymous {
        let title = if t.auth == TrackerAuthMode::Env { "env var name" } else { "token command" };
        f.render_widget(
            one_line_field(&t.auth_value, title, border(focused(PapertrailZone::AuthValue))),
            rows[1],
        );
    }

    let br =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[2]);
    f.render_widget(
        one_line_field(&t.base_url, "base url", border(focused(PapertrailZone::BaseUrl))),
        br[0],
    );
    f.render_widget(
        one_line_field(&t.project, "project", border(focused(PapertrailZone::Project))),
        br[1],
    );
    f.render_widget(
        one_line_field(&t.tags, "tags", border(focused(PapertrailZone::Tags))),
        rows[3],
    );

    help_panel(f, rows[4], "help", configure_help(zones.get(focus).copied()));
}

fn configure_help(zone: Option<PapertrailZone>) -> Vec<Line<'static>> {
    let text = match zone {
        Some(PapertrailZone::Provider) =>
            "The issue tracker to bind. Jira and self-hosted instances must be configured here.",
        Some(PapertrailZone::Auth) =>
            "anonymous rate-limits quickly on public repos. A token command like 'gh auth token' \
             keeps no secret in the file.",
        Some(PapertrailZone::AuthValue) =>
            "env var: the NAME of a variable holding the token. command: a shell command printing \
             the token to stdout.",
        Some(PapertrailZone::BaseUrl) =>
            "Self-hosted / enterprise instances only (https://…). Leave empty for the cloud host.",
        Some(PapertrailZone::Project) =>
            "Required for Jira (the project KEY, e.g. PROJ). Elsewhere derived from the remote — \
             owner/repo.",
        Some(PapertrailZone::Tags) =>
            "Optional comma-separated labels; only matching issues and PRs are mirrored. Empty = \
             all.",
        _ => "An explicit [[tracker]] binding replaces auto-detection entirely.",
    };
    vec![Line::from(Span::styled(text, theme::muted()))]
}

pub(super) fn handle_papertrail(key: KeyEvent, state: &mut WizardState) -> Outcome {
    let zones = papertrail_zones(&state.draft.tracker);
    // Copy focus out so the helpers below can take `&mut state` freely (a held `&mut state.step`
    // would conflict). Clamp it in case a prior mode/auth change shrank the zone list.
    let mut focus = match &state.step {
        Some(StepState::Papertrail { focus }) => (*focus).min(zones.len().saturating_sub(1)),
        _ => return Outcome::Pass,
    };
    let zone = zones.get(focus).copied().unwrap_or(PapertrailZone::Mode);
    let outcome = match key.code {
        KeyCode::Enter => return Outcome::Advance,
        KeyCode::Esc => return Outcome::Back,
        KeyCode::Up => {
            focus = focus.saturating_sub(1);
            Outcome::Consumed
        },
        KeyCode::Down => {
            if focus + 1 < zones.len() {
                focus += 1;
            }
            Outcome::Consumed
        },
        KeyCode::Left => {
            cycle_zone(zone, state, false);
            Outcome::Consumed
        },
        KeyCode::Right => {
            cycle_zone(zone, state, true);
            Outcome::Consumed
        },
        KeyCode::Char(' ') if !is_editable(zone) => {
            cycle_zone(zone, state, true);
            Outcome::Consumed
        },
        KeyCode::Backspace if is_editable(zone) => {
            if let Some(field) = field_mut(zone, state) {
                field.pop();
            }
            Outcome::Consumed
        },
        KeyCode::Char(c)
            if is_editable(zone)
                && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(field) = field_mut(zone, state) {
                field.push(c);
            }
            Outcome::Consumed
        },
        _ => Outcome::Pass,
    };
    if let Some(StepState::Papertrail { focus: f }) = &mut state.step {
        *f = focus;
    }
    outcome
}

/// Cycle the selection of a list zone (`forward` = next option, else previous). Field zones ignore
/// this.
fn cycle_zone(zone: PapertrailZone, state: &mut WizardState, forward: bool) {
    let t = &mut state.draft.tracker;
    match zone {
        PapertrailZone::Mode => {
            t.mode = match t.mode {
                TrackerMode::AutoDetect => TrackerMode::Configure,
                TrackerMode::Configure => TrackerMode::AutoDetect,
            };
        },
        PapertrailZone::Provider => {
            let cur = PROVIDERS.iter().position(|p| *p == t.provider).unwrap_or(0);
            t.provider = PROVIDERS[step_index(cur, PROVIDERS.len(), forward)];
        },
        PapertrailZone::Auth => {
            const AUTH: [TrackerAuthMode; 3] =
                [TrackerAuthMode::Anonymous, TrackerAuthMode::Env, TrackerAuthMode::Command];
            let cur = AUTH.iter().position(|a| *a == t.auth).unwrap_or(0);
            t.auth = AUTH[step_index(cur, AUTH.len(), forward)];
        },
        _ => {},
    }
}

/// Wrap-around index step for the option lists.
fn step_index(cur: usize, len: usize, forward: bool) -> usize {
    if forward { (cur + 1) % len } else { (cur + len - 1) % len }
}

fn field_mut(zone: PapertrailZone, state: &mut WizardState) -> Option<&mut String> {
    let t = &mut state.draft.tracker;
    match zone {
        PapertrailZone::AuthValue => Some(&mut t.auth_value),
        PapertrailZone::BaseUrl => Some(&mut t.base_url),
        PapertrailZone::Project => Some(&mut t.project),
        PapertrailZone::Tags => Some(&mut t.tags),
        _ => None,
    }
}

pub(super) fn validate_papertrail(state: &WizardState) -> CheckResult {
    let t = &state.draft.tracker;
    if t.mode == TrackerMode::AutoDetect {
        return CheckResult::ok();
    }
    // A non-anonymous auth mode needs a value.
    if t.auth != TrackerAuthMode::Anonymous && t.auth_value.trim().is_empty() {
        return CheckResult::block("tracker auth needs a value (env var name or token command)");
    }
    // Mirror the config's own constraints so init never writes a file `Config::load` then rejects,
    // aborting after the write. A base_url must be a valid credential-free http(s) origin.
    let base_url = t.base_url.trim();
    if !base_url.is_empty() && !valid_tracker_base_url(base_url) {
        return CheckResult::block("tracker base url must be an https URL with no credentials");
    }
    // An explicit project must match the provider's identity format.
    let project = t.project.trim();
    if !project.is_empty() && !valid_tracker_project(t.provider, project) {
        return CheckResult::block(match t.provider {
            Tracker::Jira => "Jira project must be an uppercase key (e.g. PROJ)",
            Tracker::Gitlab => "GitLab project must be a namespace path (e.g. group/repo)",
            _ => "project must be owner/repo",
        });
    }
    // The binding must actually resolve — otherwise there is no tracker (and distillation, which
    // depends on one, would have no threads). A code host with no project needs the origin to
    // derive one; Jira always needs an explicit key.
    if !configured_tracker_resolves(t, state.detected_configured.as_ref()) {
        return CheckResult::block(match t.provider {
            Tracker::Jira => "Jira has no auto-detected remote — set a project key",
            _ => "set a project (owner/repo) — it can't be derived from this remote",
        });
    }
    CheckResult::ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::RepoScan;
    use crate::init::wizard::draft::WizardDraft;
    use crate::init::wizard::state::WizardState;

    fn state() -> WizardState {
        let dir = tempfile::tempdir().unwrap();
        let d = WizardDraft::from_scan(&RepoScan::default(), ".".into(), dir.path().to_path_buf());
        WizardState::new(d, RepoScan::default())
    }

    #[test]
    fn auto_detect_never_blocks() {
        assert!(validate_papertrail(&state()).message.is_none());
    }

    #[test]
    fn configure_validation_blocks_bad_and_incomplete_bindings() {
        let mut s = state(); // temp-dir root → nothing auto-detected
        s.draft.tracker.mode = TrackerMode::Configure;
        s.draft.tracker.provider = Tracker::Github;

        // Non-anonymous auth needs a value.
        s.draft.tracker.auth = TrackerAuthMode::Env;
        assert!(validate_papertrail(&s).message.is_some(), "env auth needs a value");
        s.draft.tracker.auth_value = "GH_TOKEN".into();

        // A code host with no project and no detectable origin can't resolve → block.
        assert!(validate_papertrail(&s).message.is_some(), "no project + no origin → block");

        // A malformed project → block; a valid one → ok.
        s.draft.tracker.project = "owner".into();
        assert!(validate_papertrail(&s).message.is_some(), "owner is not owner/repo");
        s.draft.tracker.project = "owner/repo".into();
        assert!(validate_papertrail(&s).message.is_none());

        // A malformed base_url → block; https → ok.
        s.draft.tracker.base_url = "gitlab.example.com".into();
        assert!(validate_papertrail(&s).message.is_some(), "base_url needs a scheme");
        s.draft.tracker.base_url = "https://gitlab.example.com".into();
        assert!(validate_papertrail(&s).message.is_none());

        // Jira needs an uppercase key.
        s.draft.tracker.provider = Tracker::Jira;
        s.draft.tracker.project = "proj".into();
        assert!(validate_papertrail(&s).message.is_some(), "jira key must be uppercase");
        s.draft.tracker.project = "PROJ".into();
        assert!(validate_papertrail(&s).message.is_none());
    }

    #[test]
    fn a_configured_binding_needs_its_remote_to_detect_a_matching_provider() {
        // A custom remote with no project must be verified via detection (state resolves it). In a
        // temp-dir root the remote resolves to nothing → not set up → blocked.
        let mut s = state();
        s.draft.tracker.mode = TrackerMode::Configure;
        s.draft.tracker.provider = Tracker::Github;
        s.draft.tracker.remote = "upstream".into();
        assert!(!tracker_is_set_up(&s));
        assert!(validate_papertrail(&s).message.is_some());
        // Once the remote resolves to a matching provider → set up.
        s.detected_configured =
            rag_rat_papertrail::detect_tracker_from_remote_url("https://github.com/o/r");
        assert!(tracker_is_set_up(&s));
        assert!(validate_papertrail(&s).message.is_none());
    }

    #[test]
    fn autodetect_gates_on_origin_not_the_configured_remote() {
        // A binding whose configured (non-origin) remote resolves, but origin does not: AutoDetect
        // binds origin at runtime, so it must NOT be considered set up.
        let mut s = state();
        s.draft.tracker.mode = TrackerMode::AutoDetect;
        s.detected_configured =
            rag_rat_papertrail::detect_tracker_from_remote_url("https://github.com/o/r");
        s.detected_origin = None;
        assert!(!tracker_is_set_up(&s), "AutoDetect gates on origin detection only");
    }

    #[test]
    fn a_self_hosted_binding_without_a_project_asks_for_one() {
        // A self-hosted base_url must not derive the project from the configured remote — that
        // remote may be a DIFFERENT (cloud) host, resolving the wrong repository. Require a project
        // even when origin detection succeeds.
        let mut s = state();
        s.draft.tracker.mode = TrackerMode::Configure;
        s.draft.tracker.provider = Tracker::Github;
        s.draft.tracker.base_url = "https://github.example.com".into();
        s.detected_configured =
            rag_rat_papertrail::detect_tracker_from_remote_url("https://github.com/o/r");
        assert!(!tracker_is_set_up(&s), "self-hosted base_url + cloud origin needs a project");
        assert!(validate_papertrail(&s).message.is_some());
        // With an explicit project it resolves.
        s.draft.tracker.project = "owner/repo".into();
        assert!(tracker_is_set_up(&s));
        assert!(validate_papertrail(&s).message.is_none());
    }
}
