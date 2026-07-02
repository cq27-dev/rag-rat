//! Integration step — version check + git/Claude hook toggles.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use super::super::probe::ProbeKind;
use super::super::state::WizardState;
use super::super::theme;
use super::types::{CheckResult, Outcome, Sev, StepId, StepState};
use crate::{MANAGED_HOOKS, git_paths, is_rag_rat_hook};

pub(super) fn render_integration(f: &mut Frame, area: Rect, state: &WizardState) {
    let Some(StepState::Integration { focus }) = state.step else { return };
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .split(area);

    let rows = [
        ("version check (t)", state.draft.version_check),
        ("git hooks (g)", state.draft.hooks.git),
        ("Claude hooks (c)", state.draft.hooks.claude),
        ("global hooks", state.draft.hooks.claude_global),
    ];
    for (i, (label, on)) in rows.iter().enumerate() {
        let style = if focus == i { theme::selected() } else { theme::base() };
        let text = if *on { format!("[x] {}", label) } else { format!("[ ] {}", label) };
        f.render_widget(
            Paragraph::new(Span::styled(text, style))
                .style(theme::base())
                .block(theme::focused_block("", focus == i)),
            chunks[i],
        );
    }
}

pub(super) fn handle_integration(key: KeyEvent, state: &mut WizardState) -> Outcome {
    let Some(StepState::Integration { focus }) = &mut state.step else { return Outcome::Pass };
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if *focus > 0 {
                *focus -= 1;
            }
            Outcome::Consumed
        },
        KeyCode::Down | KeyCode::Char('j') => {
            if *focus + 1 < 4 {
                *focus += 1;
            }
            Outcome::Consumed
        },
        KeyCode::Home => {
            *focus = 0;
            Outcome::Consumed
        },
        KeyCode::End => {
            *focus = 3;
            Outcome::Consumed
        },
        KeyCode::Char(' ') => {
            match *focus {
                0 => {
                    state.draft.version_check = !state.draft.version_check;
                },
                1 => {
                    state.draft.hooks.git = !state.draft.hooks.git;
                },
                2 => {
                    state.draft.hooks.claude = !state.draft.hooks.claude;
                },
                _ => {
                    state.draft.hooks.claude_global = !state.draft.hooks.claude_global;
                },
            };
            Outcome::Consumed
        },
        KeyCode::Char('t') | KeyCode::Char('T') => {
            state.probes.spawn(StepId::Integration, ProbeKind::VersionCheck, || {
                match rag_rat_core::version_check::fetch_latest() {
                    Some(v) =>
                        CheckResult { severity: Sev::Ok, message: Some(format!("latest: {v}")) },
                    None => CheckResult::warn("couldn't reach crates.io"),
                }
            });
            Outcome::Consumed
        },
        KeyCode::Char('g') | KeyCode::Char('G') => {
            state.draft.hooks.git = !state.draft.hooks.git;
            Outcome::Consumed
        },
        KeyCode::Char('c') | KeyCode::Char('C') => {
            state.draft.hooks.claude = !state.draft.hooks.claude;
            Outcome::Consumed
        },
        KeyCode::Enter => Outcome::Advance,
        KeyCode::Esc => Outcome::Back,
        _ => Outcome::Pass,
    }
}

pub(super) fn validate_hooks(state: &WizardState) -> CheckResult {
    if !state.draft.hooks.git {
        return CheckResult::ok();
    }
    let Ok(gp) = git_paths(&state.draft.root_abs) else {
        return CheckResult::block(
            "git hooks require a git worktree; disable git hooks or choose a git repo root",
        );
    };
    for &hook in MANAGED_HOOKS {
        let path = gp.hooks_dir.join(hook);
        if path.exists()
            && !is_rag_rat_hook(&path).unwrap_or(false)
            && !state.hook_conflicts.contains_key(hook)
        {
            return CheckResult::block(format!(
                "resolve foreign hook `{}` before saving or disable git hooks",
                hook
            ));
        }
    }
    CheckResult::ok()
}
