//! Step titles / footers, per-step state init, and the render / handle_key / validate dispatch.

use std::path::PathBuf;

use rag_rat_base::language::Language;
use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use tui_tree_widget::TreeState;

use super::super::state::WizardState;
use super::distillation::{handle_distillation, render_distillation, validate_distillation};
use super::embedding::{
    handle_embedding, init_embedding_step, render_embedding, scroll_embedding, validate_embedding,
};
use super::indexing::{
    base_candidates_for, focus_tree_zone, handle_indexing, render_indexing, scroll_dir_tree,
    validate_indexing,
};
use super::integration::{handle_integration, render_integration, validate_hooks};
use super::oracle::{handle_oracle, render_oracle};
use super::tracker::{handle_papertrail, render_papertrail, validate_papertrail};
use super::types::{CheckResult, IndexZone, Outcome, StepId, StepState};

pub(crate) fn step_title(id: StepId) -> &'static str {
    match id {
        StepId::Indexing => "Indexing",
        StepId::Oracle => "Oracle",
        StepId::Embedding => "Embedding",
        StepId::Papertrail => "Papertrail",
        StepId::Distill => "Distill",
        StepId::Integration => "Integration",
    }
}

pub(crate) fn step_footer(id: StepId) -> &'static str {
    match id {
        StepId::Indexing =>
            "←→ language/tree  Tab dirs  ↑↓/j/k move  PgUp/PgDn jump  Space toggle  a/A all",
        StepId::Oracle => "Space toggle  t test tools  l log  Enter next",
        StepId::Embedding =>
            "Tab/Shift-Tab field  ↑↓/j/k move  PgUp/PgDn scroll  Space select  type edit  d \
             download  t/p test  l log",
        StepId::Papertrail => "↑↓ move  ←→ change  type edit  Enter next",
        StepId::Distill => "↑↓ move  ←→ change  type edit  Enter next",
        StepId::Integration => "↑↓ navigate  Space toggle  t version check  g git  Enter next",
    }
}

fn step_matches(id: StepId, step: &Option<StepState>) -> bool {
    matches!(
        (id, step),
        (StepId::Indexing, Some(StepState::Indexing { .. }))
            | (StepId::Oracle, Some(StepState::Oracle))
            | (StepId::Embedding, Some(StepState::Embedding { .. }))
            | (StepId::Papertrail, Some(StepState::Papertrail { .. }))
            | (StepId::Distill, Some(StepState::Distill { .. }))
            | (StepId::Integration, Some(StepState::Integration { .. }))
    )
}

pub(crate) fn init_step(id: StepId, state: &WizardState) -> StepState {
    match id {
        StepId::Indexing => {
            let toggles: Vec<(Language, bool)> = Language::all()
                .iter()
                .map(|&l| (l, state.draft.bindings.contains_key(&l)))
                .collect();
            let mut tree = TreeState::default();
            tree.select(vec![PathBuf::from(".")]);
            let dir_candidates = Language::all()
                .iter()
                .map(|&lang| (lang, base_candidates_for(&state.scan, lang)))
                .collect();
            StepState::Indexing {
                lang_toggles: toggles,
                lang_focus: 0,
                zone: IndexZone::Toggles,
                tree,
                dir_candidates,
            }
        },
        StepId::Embedding => init_embedding_step(state),
        StepId::Oracle => StepState::Oracle,
        StepId::Papertrail => StepState::Papertrail { focus: 0 },
        StepId::Distill => StepState::Distill { focus: 0 },
        StepId::Integration => StepState::Integration { focus: 0 },
    }
}

pub(crate) fn render_step(id: StepId, f: &mut Frame, area: Rect, state: &mut WizardState) {
    match id {
        StepId::Indexing => render_indexing(f, area, state),
        StepId::Oracle => render_oracle(f, area, state),
        StepId::Embedding => render_embedding(f, area, state),
        StepId::Papertrail => render_papertrail(f, area, state),
        StepId::Distill => render_distillation(f, area, state),
        StepId::Integration => render_integration(f, area, state),
    }
}

pub(crate) fn step_handle_key(id: StepId, key: KeyEvent, state: &mut WizardState) -> Outcome {
    // Re-init if the variant doesn't match the active step after tab navigation.
    if !step_matches(id, &state.step) {
        state.step = Some(init_step(id, state));
    }
    match id {
        StepId::Indexing => handle_indexing(key, state),
        StepId::Oracle => handle_oracle(key, state),
        StepId::Embedding => handle_embedding(key, state),
        StepId::Papertrail => handle_papertrail(key, state),
        StepId::Distill => handle_distillation(key, state),
        StepId::Integration => handle_integration(key, state),
    }
}

pub(crate) fn validate_step(id: StepId, state: &WizardState) -> CheckResult {
    match id {
        StepId::Indexing => validate_indexing(state),
        StepId::Oracle => CheckResult::ok(),
        StepId::Embedding => validate_embedding(state),
        StepId::Papertrail => validate_papertrail(state),
        StepId::Distill => validate_distillation(state),
        StepId::Integration => validate_hooks(state),
    }
}

pub(crate) fn scroll_step(id: StepId, delta: isize, state: &mut WizardState) -> bool {
    match id {
        StepId::Indexing => {
            focus_tree_zone(state);
            scroll_dir_tree(state, delta);
            true
        },
        StepId::Embedding => scroll_embedding(delta, state),
        _ => false,
    }
}
