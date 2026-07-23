//! Distill step — the opt-in issue-distillation model pass. Gated on a resolvable issue tracker
//! (nothing to distill without one) and off by default; the validated-box choice reuses the typed
//! `provision` confirm because it provisions a paid GPU.

use rag_rat_base::config::valid_tracker_base_url;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::super::draft::DistillMode;
use super::super::state::{PROVISION_CONFIRM_WORD, WizardState, provision_confirm_satisfied};
use super::super::theme;
use super::embedding::one_line_field;
use super::tracker::tracker_is_set_up;
use super::types::{CheckResult, DistillZone, Outcome, StepState};
use super::widgets::{help_panel, option_list};

/// Focusable zones for the current mode. Empty when no tracker is set up (the step is inactive).
fn distill_zones(state: &WizardState) -> Vec<DistillZone> {
    if !tracker_is_set_up(state) {
        return Vec::new();
    }
    let mut zones = vec![DistillZone::Mode];
    match state.draft.distill.mode {
        DistillMode::Off => {},
        // A preserved existing box needs no confirmation (it's already provisioned).
        DistillMode::ValidatedBox =>
            if !state.draft.distill.preserved_ephemeral {
                zones.push(DistillZone::ProvisionConfirm);
            },
        DistillMode::Connect => {
            zones.push(DistillZone::Endpoint);
            zones.push(DistillZone::Model);
        },
    }
    zones
}

fn border(focused: bool) -> Style {
    if focused { theme::focused_border() } else { theme::border() }
}

fn is_editable(zone: DistillZone) -> bool {
    matches!(zone, DistillZone::Endpoint | DistillZone::Model | DistillZone::ProvisionConfirm)
}

pub(super) fn render_distillation(f: &mut Frame, area: Rect, state: &WizardState) {
    let Some(StepState::Distill { focus }) = state.step else { return };
    if !tracker_is_set_up(state) {
        render_inactive(f, area);
        return;
    }
    let d = &state.draft.distill;
    let zones = distill_zones(state);
    let focused = |z: DistillZone| zones.get(focus).copied() == Some(z);

    let outer = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).split(area);

    let mode_opts = vec![
        ("off".to_string(), theme::base()),
        ("validated 30B GPU box".to_string(), theme::base()),
        ("connect to a server".to_string(), theme::base()),
    ];
    let mode_sel = match d.mode {
        DistillMode::Off => 0,
        DistillMode::ValidatedBox => 1,
        DistillMode::Connect => 2,
    };
    option_list(
        f,
        outer[0],
        "Distillation  ↑↓ move  ←→ change",
        &mode_opts,
        mode_sel,
        focused(DistillZone::Mode),
    );

    match d.mode {
        DistillMode::Off => help_panel(f, outer[1], "Off", vec![Line::from(Span::styled(
            " Distillation off. Closed issues and PRs stay as raw papertrail.",
            theme::muted(),
        ))]),
        DistillMode::ValidatedBox => render_validated_box(f, outer[1], state, focused),
        DistillMode::Connect => render_connect(f, outer[1], state, &zones, focus),
    }
}

fn render_inactive(f: &mut Frame, area: Rect) {
    help_panel(f, area, "Distillation", vec![
        Line::from(Span::styled(
            " Distillation needs an issue tracker — none is set up.",
            theme::warning(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "It turns closed issues and PRs into typed decision records, so it needs a tracker \
             feeding it threads. Configure or auto-detect one on the Papertrail step first.",
            theme::muted(),
        )),
    ]);
}

fn render_validated_box(
    f: &mut Frame,
    area: Rect,
    state: &WizardState,
    focused: impl Fn(DistillZone) -> bool,
) {
    if state.draft.distill.preserved_ephemeral {
        help_panel(f, area, "GPU box (from your config)", vec![Line::from(Span::styled(
            "An existing ephemeral GPU box from your config, preserved as-is. Pick another mode \
             to replace it with the validated box or a connect endpoint.",
            theme::muted(),
        ))]);
        return;
    }
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
    f.render_widget(
        one_line_field(
            &state.ui.provision_confirm,
            "confirm the paid box",
            border(focused(DistillZone::ProvisionConfirm)),
        ),
        rows[0],
    );
    help_panel(f, rows[1], "Validated 30B box", vec![
        Line::from(Span::styled(
            "Ephemeral Modal L40S running the validated 30B model, provisioned per run and billed \
             while it runs. Nothing else to configure.",
            theme::muted(),
        )),
        Line::from(Span::styled(
            format!("Type '{PROVISION_CONFIRM_WORD}' above to acknowledge the paid GPU."),
            theme::muted(),
        )),
    ]);
}

fn render_connect(
    f: &mut Frame,
    area: Rect,
    state: &WizardState,
    zones: &[DistillZone],
    focus: usize,
) {
    let d = &state.draft.distill;
    let focused = |z: DistillZone| zones.get(focus).copied() == Some(z);
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Length(3), Constraint::Min(0)])
        .split(area);
    f.render_widget(
        one_line_field(&d.endpoint, "endpoint", border(focused(DistillZone::Endpoint))),
        rows[0],
    );
    f.render_widget(
        one_line_field(&d.model, "model", border(focused(DistillZone::Model))),
        rows[1],
    );
    help_panel(f, rows[2], "Connect", vec![
        Line::from(Span::styled(
            "Use your own running OpenAI-compatible server; nothing is provisioned.",
            theme::muted(),
        )),
        Line::from(Span::styled(
            "endpoint: base URL of the server. model: the server-side model name.",
            theme::muted(),
        )),
    ]);
}

pub(super) fn handle_distillation(key: KeyEvent, state: &mut WizardState) -> Outcome {
    // Inactive without a tracker: keep the pass off; only tab navigation works.
    if !tracker_is_set_up(state) {
        state.draft.distill.mode = DistillMode::Off;
        return match key.code {
            KeyCode::Enter => Outcome::Advance,
            KeyCode::Esc => Outcome::Back,
            _ => Outcome::Pass,
        };
    }
    let zones = distill_zones(state);
    let mut focus = match &state.step {
        Some(StepState::Distill { focus }) => (*focus).min(zones.len().saturating_sub(1)),
        _ => return Outcome::Pass,
    };
    let zone = zones.get(focus).copied().unwrap_or(DistillZone::Mode);
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
        KeyCode::Left if zone == DistillZone::Mode => {
            cycle_mode(state, false);
            Outcome::Consumed
        },
        KeyCode::Right if zone == DistillZone::Mode => {
            cycle_mode(state, true);
            Outcome::Consumed
        },
        KeyCode::Char(' ') if zone == DistillZone::Mode => {
            cycle_mode(state, true);
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
    if let Some(StepState::Distill { focus: f }) = &mut state.step {
        *f = focus;
    }
    outcome
}

fn cycle_mode(state: &mut WizardState, forward: bool) {
    const MODES: [DistillMode; 3] =
        [DistillMode::Off, DistillMode::ValidatedBox, DistillMode::Connect];
    let cur = MODES.iter().position(|m| *m == state.draft.distill.mode).unwrap_or(0);
    let next =
        if forward { (cur + 1) % MODES.len() } else { (cur + MODES.len() - 1) % MODES.len() };
    state.draft.distill.mode = MODES[next];
    // ANY deliberate mode change means the user is actively choosing — drop the preserved-existing
    // flag so the box requires confirmation and emits the shipped default, never silently restoring
    // a loaded custom block (whatever path the cycles take). An UNTOUCHED distill keeps its loaded
    // state because this only fires on a cycle.
    state.draft.distill.preserved_ephemeral = false;
}

/// The mutable field behind an editable zone. `ProvisionConfirm` edits the shared UI confirm word
/// (not the draft) — the same one the Embedding ephemeral gate uses.
fn field_mut(zone: DistillZone, state: &mut WizardState) -> Option<&mut String> {
    match zone {
        DistillZone::Endpoint => Some(&mut state.draft.distill.endpoint),
        DistillZone::Model => Some(&mut state.draft.distill.model),
        DistillZone::ProvisionConfirm => Some(&mut state.ui.provision_confirm),
        DistillZone::Mode => None,
    }
}

pub(super) fn validate_distillation(state: &WizardState) -> CheckResult {
    if !tracker_is_set_up(state) {
        return CheckResult::ok();
    }
    match state.draft.distill.mode {
        DistillMode::Off => CheckResult::ok(),
        // A preserved existing box is already provisioned/acknowledged — only a FRESH box choice
        // needs the paid-GPU confirmation.
        DistillMode::ValidatedBox =>
            if state.draft.distill.preserved_ephemeral || provision_confirm_satisfied(&state.ui) {
                CheckResult::ok()
            } else {
                CheckResult::block(format!(
                    "type '{PROVISION_CONFIRM_WORD}' on the Distill step to confirm the paid GPU \
                     box"
                ))
            },
        DistillMode::Connect => {
            let endpoint = state.draft.distill.endpoint.trim();
            if endpoint.is_empty() || state.draft.distill.model.trim().is_empty() {
                CheckResult::block("distill connect needs an endpoint and a model")
            } else if !valid_tracker_base_url(endpoint) {
                // Same generic http(s)-URL-without-credentials check the tracker base_url uses — a
                // malformed endpoint (`gpu:8000`, `not-a-url`) or a credential-bearing one
                // otherwise passes `Config::load` and only fails when the HTTP
                // client builds a request.
                CheckResult::block(
                    "distill endpoint must be a valid http(s) URL with no credentials",
                )
            } else {
                CheckResult::ok()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::RepoScan;
    use crate::init::wizard::draft::{TrackerMode, WizardDraft};
    use crate::init::wizard::state::{PROVISION_CONFIRM_WORD, WizardState};

    /// A wizard state whose tracker resolution is deterministic: a temp-dir root has no `origin`,
    /// so nothing is auto-detected and the gate is driven purely by the test.
    fn state(tracker_mode: TrackerMode, detected: bool) -> WizardState {
        let dir = tempfile::tempdir().unwrap();
        let mut d =
            WizardDraft::from_scan(&RepoScan::default(), ".".into(), dir.path().to_path_buf());
        d.tracker.mode = tracker_mode;
        let mut s = WizardState::new(d, RepoScan::default());
        let make = || {
            detected
                .then(|| {
                    rag_rat_papertrail::detect_tracker_from_remote_url("https://github.com/o/r")
                })
                .flatten()
        };
        s.detected_origin = make();
        s.detected_configured = make();
        s
    }

    #[test]
    fn distill_is_gated_on_a_resolvable_tracker() {
        // Auto-detect: gated on the origin resolving to something.
        assert!(!tracker_is_set_up(&state(TrackerMode::AutoDetect, false)));
        assert!(tracker_is_set_up(&state(TrackerMode::AutoDetect, true)));

        // Configure with an explicit project resolves regardless of detection.
        let mut resolved = state(TrackerMode::Configure, false);
        resolved.draft.tracker.project = "owner/repo".into();
        assert!(tracker_is_set_up(&resolved));
        assert!(!distill_zones(&resolved).is_empty());

        // Configure with no project and nothing detected can't resolve → inactive.
        let bare = state(TrackerMode::Configure, false);
        assert!(!tracker_is_set_up(&bare));
        assert!(distill_zones(&bare).is_empty());
    }

    #[test]
    fn validate_blocks_connect_without_fields_and_box_without_confirm() {
        let mut s = state(TrackerMode::Configure, false);
        s.draft.tracker.project = "owner/repo".into(); // a resolvable tracker → distill is active

        s.draft.distill.mode = DistillMode::Connect;
        assert!(validate_distillation(&s).message.is_some(), "empty connect must block");
        s.draft.distill.endpoint = "http://x".into();
        s.draft.distill.model = "m".into();
        assert!(validate_distillation(&s).message.is_none(), "filled connect is ok");

        s.draft.distill.mode = DistillMode::ValidatedBox;
        assert!(validate_distillation(&s).message.is_some(), "paid box needs the typed confirm");
        s.ui.provision_confirm = PROVISION_CONFIRM_WORD.to_string();
        assert!(validate_distillation(&s).message.is_none(), "confirmed box is ok");
    }

    #[test]
    fn any_mode_cycle_clears_preservation() {
        // preserved_ephemeral holds only while the user leaves distill UNTOUCHED. The first
        // deliberate mode change (from any starting mode) clears it, so a box then needs
        // confirmation + the shipped default — never a silently-restored loaded custom block.
        for start in [DistillMode::ValidatedBox, DistillMode::Off, DistillMode::Connect] {
            let mut s = state(TrackerMode::Configure, false);
            s.draft.tracker.project = "owner/repo".into();
            s.draft.distill.mode = start;
            s.draft.distill.preserved_ephemeral = true;
            cycle_mode(&mut s, true);
            assert!(!s.draft.distill.preserved_ephemeral, "cycling from {start:?} clears it");
        }
    }

    #[test]
    fn an_inactive_step_validates_ok_regardless_of_mode() {
        let mut s = state(TrackerMode::AutoDetect, false);
        s.draft.distill.mode = DistillMode::ValidatedBox;
        assert!(validate_distillation(&s).message.is_none());
    }

    #[test]
    fn a_preserved_ephemeral_box_needs_no_confirmation() {
        let mut s = state(TrackerMode::Configure, false);
        s.draft.tracker.project = "owner/repo".into(); // resolvable → distill active
        s.draft.distill.mode = DistillMode::ValidatedBox;
        s.draft.distill.preserved_ephemeral = true;
        // No typed confirm, yet valid — an existing box is already provisioned.
        assert!(validate_distillation(&s).message.is_none());
        // ...and it exposes no confirm zone.
        assert!(!distill_zones(&s).contains(&DistillZone::ProvisionConfirm));

        // A fresh box (not preserved) still requires the confirm.
        s.draft.distill.preserved_ephemeral = false;
        assert!(validate_distillation(&s).message.is_some());
        assert!(distill_zones(&s).contains(&DistillZone::ProvisionConfirm));
    }

    #[test]
    fn connect_endpoint_with_credentials_is_rejected() {
        let mut s = state(TrackerMode::Configure, false);
        s.draft.tracker.project = "owner/repo".into(); // resolvable → distill active
        s.draft.distill.mode = DistillMode::Connect;
        s.draft.distill.model = "m".into();
        s.draft.distill.endpoint = "https://user:pass@host".into();
        assert!(validate_distillation(&s).message.is_some(), "credential endpoint must block");
        s.draft.distill.endpoint = "gpu:8000".into();
        assert!(validate_distillation(&s).message.is_some(), "malformed endpoint must block");
        s.draft.distill.endpoint = "https://host".into();
        assert!(validate_distillation(&s).message.is_none());
    }
}
