//! Step functions — render, handle_key, validate. No traits, no widget abstractions.
//!
//! This module is a curated index over the per-step siblings:
//! - [`types`] — step-state enums + shared result/state types ([`StepId`], [`Outcome`], [`Sev`],
//!   [`CheckResult`], [`StepState`], [`EmbedFocus`], [`IndexZone`]).
//! - [`dispatch`] — step titles/footers, state init, and the render/handle_key/validate fan-out.
//! - [`indexing`] / [`oracle`] / [`embedding`] / [`tracker`] / [`distillation`] / [`integration`] —
//!   the per-step logic.
//! - [`widgets`] — small shared step widgets (option list, help panel).
//! - [`hooks`] — git-hook conflict choices + the chained-hook renderer.

pub(crate) mod hooks;

mod dispatch;
mod distillation;
mod embedding;
mod indexing;
mod integration;
mod oracle;
mod tracker;
mod types;
mod widgets;

#[cfg(test)]
mod tests;

pub(crate) use dispatch::{
    init_step, render_step, scroll_step, step_footer, step_handle_key, step_title, validate_step,
};
pub(crate) use tracker::tracker_is_set_up;
pub(crate) use types::{CheckResult, Outcome, Sev, StepId, StepState, can_write};
