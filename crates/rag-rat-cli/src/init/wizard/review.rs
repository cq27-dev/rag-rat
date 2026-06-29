//! Review screen model — summary, reconfigure diff, and write gate.
//!
//! [`ReviewModel::build`] computes:
//! - `summary`: human-readable lines describing the current draft config.
//! - `warnings`: `Warn`-severity messages from `state.checks`.
//! - `diff`: for reconfigure mode (`original` is `Some`), a minimal line diff between the original
//!   TOML and the patched output. `None` for fresh init.
//! - `ephemeral_unverified`: `true` when the draft uses an ephemeral remote BUT the Remote step's
//!   spin-up probe has not completed successfully — prompts the user for an explicit "keep anyway"
//!   acknowledgement before confirming.
//!
//! [`ReviewModel::can_confirm`] is false if any step carries a `Block` severity — the write gate
//! is closed.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::draft::RemoteMode;
use super::probe::{ProbeKind, ProbeStatus};
use super::state::WizardState;
use super::steps::{CheckResult, Sev as CheckSeverity, StepId, can_write};
use super::theme;

/// The computed review model — built once per render tick (or on demand in tests).
#[derive(Debug)]
pub(crate) struct ReviewModel {
    /// Human-readable config summary lines, one concept per line.
    pub summary: Vec<String>,
    /// `Warn`-severity messages from `state.checks`; `Block`s surface via `can_confirm`.
    pub warnings: Vec<String>,
    /// Line diff (reconfigure mode only): changed/added/removed lines between the original TOML
    /// and the patched output. `None` for fresh init.
    pub diff: Option<String>,
    /// `true` when the draft's remote is Ephemeral AND the Remote probe hasn't passed — the UI
    /// must require an explicit "keep anyway" toggle before enabling Confirm.
    pub ephemeral_unverified: bool,
    /// Cached write-gate result — `true` if any step is `Block`. Set at build time.
    blocked: bool,
}

impl ReviewModel {
    /// Build the review model from the current wizard state.
    ///
    /// `original` is the raw contents of the existing `rag-rat.toml` (reconfigure path).
    /// Pass `None` for a fresh `rag-rat init`.
    pub(crate) fn build(state: &WizardState, original: Option<&str>) -> Self {
        let draft = &state.draft;

        // ── write gate ───────────────────────────────────────────────────────────
        let blocked = !can_write(&state.checks);

        // ── summary ──────────────────────────────────────────────────────────────
        let mut summary: Vec<String> = Vec::new();

        // Root
        summary.push(format!("  root:       {}", draft.root_value));

        // Languages + directories
        if draft.bindings.is_empty() {
            summary.push("  languages:  (none selected)".to_string());
        } else {
            for (lang, dirs) in &draft.bindings {
                let dir_list: Vec<&str> = dirs.iter().filter_map(|p| p.to_str()).collect();
                summary.push(format!("  {:10}  {}", lang.as_str(), dir_list.join(", ")));
            }
        }

        // Embedding model
        summary.push(format!("  model:      {}", draft.model));

        // Remote mode
        match &draft.remote {
            None => summary.push("  remote:     none (in-process embeddings)".to_string()),
            Some(r) => match &r.mode {
                RemoteMode::Connect(ep) => {
                    summary.push(format!("  remote:     connect  {}", ep));
                },
                RemoteMode::Ephemeral(cb) => {
                    summary.push(format!("  remote:     ephemeral  cookbook={}", cb));
                },
            },
        }

        // Oracle auto_run
        summary.push(format!(
            "  oracle:     auto_run={}",
            if draft.oracle_auto_run { "yes" } else { "no" }
        ));

        // Version check
        summary.push(format!(
            "  version:    check={}",
            if draft.version_check { "yes" } else { "no" }
        ));

        // Hooks to install
        {
            let mut hooks: Vec<&str> = Vec::new();
            if draft.hooks.git {
                hooks.push("git");
            }
            if draft.hooks.claude {
                if draft.hooks.claude_global {
                    hooks.push("claude (global)");
                } else {
                    hooks.push("claude (project)");
                }
            }
            if hooks.is_empty() {
                summary.push("  hooks:      none".to_string());
            } else {
                summary.push(format!("  hooks:      {}", hooks.join(", ")));
            }
        }

        // ── warnings ─────────────────────────────────────────────────────────────
        let warnings: Vec<String> = state
            .checks
            .iter()
            .filter(|c| c.severity == CheckSeverity::Warn)
            .filter_map(|c| c.message.clone())
            .collect();

        // ── diff ─────────────────────────────────────────────────────────────────
        let diff = original.and_then(|orig| {
            let patched = draft.patch_existing(orig).ok()?;
            Some(line_diff(orig, &patched))
        });

        // ── ephemeral_unverified ─────────────────────────────────────────────────
        let ephemeral_unverified = matches!(&draft.remote, Some(r) if matches!(r.mode, RemoteMode::Ephemeral(_)))
            && !matches!(state.probes.status(StepId::Embedding), ProbeStatus::Done {
                kind: ProbeKind::EphemeralTest,
                result: CheckResult { severity: CheckSeverity::Ok, .. },
            });

        Self { summary, warnings, diff, ephemeral_unverified, blocked }
    }

    /// `true` iff the write gate is open: no step is `Block`, AND an unverified ephemeral remote
    /// has been explicitly kept (the user pressed `K`). `ephemeral_acknowledged` is the UI-state
    /// "keep anyway" flag — irrelevant unless `ephemeral_unverified`, but always required to pass
    /// when it is, so a billable-but-unproven remote can't be written on a single `c`.
    pub(crate) fn can_confirm(&self, ephemeral_acknowledged: bool) -> bool {
        if self.blocked {
            return false;
        }
        if self.ephemeral_unverified && !ephemeral_acknowledged {
            return false;
        }
        true
    }

    pub(crate) fn body_line_count(&self) -> usize {
        let mut count = 1 + self.summary.len();
        if !self.warnings.is_empty() {
            count += 2 + self.warnings.len();
        }
        if self.ephemeral_unverified {
            count += 2;
        }
        if let Some(diff) = &self.diff {
            count += 2 + diff.lines().count();
        }
        if self.blocked {
            count += 2;
        }
        count
    }
}

/// Render the review screen into `area`.
pub(crate) fn render(model: &ReviewModel, f: &mut Frame, area: Rect, scroll: u16) {
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::widgets::Clear;

    // Clear the full area so step content doesn't bleed through.
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(theme::base()), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // Title bar
    let title = Paragraph::new(Line::from(vec![
        Span::styled("Review", theme::title()),
        Span::raw(" — confirm to write"),
    ]))
    .style(theme::base());
    f.render_widget(title, chunks[0]);

    // Body
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled("Configuration", theme::heading())));
    for s in &model.summary {
        lines.push(Line::from(Span::raw(s.clone())));
    }

    if !model.warnings.is_empty() {
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled(
            "Warnings",
            theme::warning().add_modifier(Modifier::UNDERLINED),
        )));
        for w in &model.warnings {
            lines
                .push(Line::from(vec![Span::styled("! ", theme::warning()), Span::raw(w.clone())]));
        }
    }

    if model.ephemeral_unverified {
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled(
            "Ephemeral remote has not been verified — press 'K' to keep anyway.",
            theme::warning(),
        )));
    }

    if let Some(diff) = &model.diff {
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled("Changes", theme::heading())));
        for diff_line in diff.lines() {
            let style = if diff_line.starts_with('+') {
                theme::success()
            } else if diff_line.starts_with('-') {
                theme::error()
            } else {
                theme::base()
            };
            lines.push(Line::from(Span::styled(diff_line.to_string(), style)));
        }
    }

    // Blocking errors close the gate outright; the ephemeral-keep prompt above handles the softer
    // "press K" gate. Use `blocked` directly (same module) rather than `can_confirm`, which also
    // needs the UI-state acknowledgement the renderer doesn't carry.
    if model.blocked {
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled(
            "Cannot confirm: one or more steps have blocking errors.",
            theme::error(),
        )));
    }

    let body = Paragraph::new(lines)
        .style(theme::base())
        .block(theme::block("Summary"))
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(body, chunks[1]);

    let footer = if model.ephemeral_unverified {
        " c save changes after K  K keep unverified remote  q/Esc back "
    } else if model.blocked {
        " c save changes (blocked)  q/Esc back "
    } else {
        " c save changes  q/Esc back "
    };
    f.render_widget(Paragraph::new(footer).style(theme::footer()), chunks[2]);
}

// ── Minimal line diff ─────────────────────────────────────────────────────────

/// Produce a minimal unified-style line diff between `before` and `after`.
///
/// Uses an LCS (longest common subsequence) algorithm so that structural changes —
/// inserting or removing blocks — do not cause unchanged lines to appear as
/// modified. Lines common to both are emitted as context (prefix `  `), lines only
/// in `before` are prefixed with `-`, and lines only in `after` are prefixed with
/// `+`. Context lines are included so the reader can orient themselves.
///
/// The algorithm is a standard O(mn) DP table backtrack, which is perfectly
/// adequate for the small TOML files rag-rat manages. No external crate is used.
fn line_diff(before: &str, after: &str) -> String {
    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();
    let (m, n) = (b.len(), a.len());

    // Build LCS length table.
    // dp[i][j] = LCS length of b[..i] vs a[..j].
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if b[i - 1] == a[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack to produce the edit script.
    #[derive(Debug)]
    enum Edit<'s> {
        Context(&'s str),
        Remove(&'s str),
        Add(&'s str),
    }

    let mut edits: Vec<Edit<'_>> = Vec::new();
    let (mut i, mut j) = (m, n);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && b[i - 1] == a[j - 1] {
            edits.push(Edit::Context(b[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            edits.push(Edit::Add(a[j - 1]));
            j -= 1;
        } else {
            edits.push(Edit::Remove(b[i - 1]));
            i -= 1;
        }
    }
    edits.reverse();

    // Render: only emit context lines that neighbour a change (one line either
    // side) so the output stays compact for large unchanged configs. If there are
    // no changes at all, return an empty string.
    let has_change = edits.iter().any(|e| !matches!(e, Edit::Context(_)));
    if !has_change {
        return String::new();
    }

    // Mark which context lines are adjacent to a change.
    let n_edits = edits.len();
    let mut show = vec![false; n_edits];
    for idx in 0..n_edits {
        if !matches!(edits[idx], Edit::Context(_)) {
            // Show one context line before and after each change.
            if idx > 0 {
                show[idx - 1] = true;
            }
            show[idx] = true;
            if idx + 1 < n_edits {
                show[idx + 1] = true;
            }
        }
    }

    let mut output: Vec<String> = Vec::new();
    for (idx, edit) in edits.iter().enumerate() {
        match edit {
            Edit::Context(line) =>
                if show[idx] {
                    output.push(format!("  {}", line));
                },
            Edit::Remove(line) => output.push(format!("-{}", line)),
            Edit::Add(line) => output.push(format!("+{}", line)),
        }
    }

    output.join("\n")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rag_rat_core::language::Language;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::init::RepoScan;
    use crate::init::wizard::draft::WizardDraft;
    use crate::init::wizard::probe::{ProbeKind, ProbeMsg};
    use crate::init::wizard::state::WizardState;
    use crate::init::wizard::steps::CheckResult;

    fn bare_state() -> WizardState {
        let draft =
            WizardDraft::from_scan(&RepoScan::default(), ".".to_string(), PathBuf::from("."));
        WizardState::new(draft, RepoScan::default())
    }

    fn state_with_block(step: StepId) -> WizardState {
        let mut st = bare_state();
        st.checks[step.index()] = CheckResult::block("test block");
        st
    }

    fn state_with_binding(dir: &str) -> WizardState {
        let mut st = bare_state();
        st.draft.bindings.insert(Language::Rust, vec![PathBuf::from(dir)]);
        st
    }

    /// A state whose draft uses an EPHEMERAL remote with no passing spin-up probe → the Review
    /// model is `ephemeral_unverified` (the probe registry has no `Done(Ok)` entry by default).
    fn state_with_unverified_ephemeral() -> WizardState {
        use crate::init::wizard::draft::{RemoteDraft, RemoteMode};
        let mut st = bare_state();
        st.draft.remote = Some(RemoteDraft {
            model: "all-minilm".to_string(),
            mode: RemoteMode::Ephemeral("some-cookbook".to_string()),
            gpu: None,
            num_ctx: None,
            batch_size: 32,
            auth_env: None,
        });
        st
    }

    // ── brief tests ───────────────────────────────────────────────────────────

    #[test]
    fn review_blocks_confirm_when_a_step_blocks() {
        let st = state_with_block(StepId::Indexing);
        let r = ReviewModel::build(&st, None);
        // A Block closes the gate regardless of the ephemeral acknowledgement.
        assert!(!r.can_confirm(false));
        assert!(!r.can_confirm(true));
    }

    #[test]
    fn review_diff_shows_changed_lines() {
        let original = "[index]\nroot = \".\"\n[target_bindings]\nrust = [\"src\"]\n";
        let st = state_with_binding("crates");
        let r = ReviewModel::build(&st, Some(original));
        assert!(r.diff.unwrap().contains("crates"));
    }

    // ── additional coverage ───────────────────────────────────────────────────

    #[test]
    fn review_allows_confirm_with_no_blocks() {
        let st = bare_state();
        let r = ReviewModel::build(&st, None);
        // No ephemeral remote → the acknowledgement is irrelevant; the gate is open either way.
        assert!(r.can_confirm(false));
        assert!(r.can_confirm(true));
    }

    /// An unverified ephemeral remote gates Confirm on the `k` acknowledgement: closed without it,
    /// open once granted (no step Blocks here, so only the keep-anyway gate applies).
    #[test]
    fn unverified_ephemeral_requires_keep_acknowledgement() {
        let st = state_with_unverified_ephemeral();
        let r = ReviewModel::build(&st, None);
        assert!(
            r.ephemeral_unverified,
            "ephemeral remote with no passing probe must be unverified"
        );
        assert!(!r.can_confirm(false), "Confirm must be closed until the user presses K");
        assert!(r.can_confirm(true), "pressing K (acknowledged) opens Confirm");
    }

    #[test]
    fn only_ephemeral_probe_success_verifies_ephemeral_remote() {
        let mut st = state_with_unverified_ephemeral();
        let probe_id = st.probes.current(StepId::Embedding);
        assert!(st.probes.apply(ProbeMsg {
            probe_id,
            kind: ProbeKind::Download,
            result: CheckResult::ok(),
        }));
        assert!(
            ReviewModel::build(&st, None).ephemeral_unverified,
            "a download probe must not verify an ephemeral remote"
        );

        let probe_id = st.probes.current(StepId::Embedding);
        assert!(st.probes.apply(ProbeMsg {
            probe_id,
            kind: ProbeKind::EphemeralTest,
            result: CheckResult::ok(),
        }));
        assert!(!ReviewModel::build(&st, None).ephemeral_unverified);
    }

    #[test]
    fn review_collects_warnings() {
        let mut st = bare_state();
        st.checks[StepId::Integration.index()] = CheckResult::warn("git not on PATH");
        let r = ReviewModel::build(&st, None);
        assert!(r.warnings.iter().any(|w| w.contains("git not on PATH")));
    }

    #[test]
    fn review_diff_is_none_for_fresh_init() {
        let st = bare_state();
        let r = ReviewModel::build(&st, None);
        assert!(r.diff.is_none());
    }

    #[test]
    fn review_footer_explains_save_key() {
        let st = bare_state();
        let model = ReviewModel::build(&st, None);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&model, f, f.area(), 0)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..20)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("c save changes"));
    }

    #[test]
    fn line_diff_shows_added_and_removed() {
        let before = "a\nb\nc\n";
        let after = "a\nX\nc\n";
        let diff = line_diff(before, after);
        assert!(diff.contains("-b"), "removed line must be prefixed -");
        assert!(diff.contains("+X"), "added line must be prefixed +");
        // "a" and "c" may appear as context (  a /  c) but must not appear as
        // changes (i.e. must not be prefixed with + or -).
        for line in diff.lines() {
            if line.starts_with('+') || line.starts_with('-') {
                assert!(
                    !line.ends_with('a') || line.contains('X') || line.contains('b'),
                    "unchanged line 'a' must not be marked changed"
                );
                assert!(!line.ends_with('c'), "unchanged line 'c' must not be marked changed");
            }
        }
    }

    /// Regression test: inserting a new block must not cause lines *after* the
    /// insertion point to appear as changed.  The positional diff wrongly shifted
    /// every post-insertion line onto the wrong counterpart; the LCS diff must not.
    #[test]
    fn line_diff_structural_insert_does_not_corrupt_trailing_unchanged_lines() {
        // Minimal two-section TOML; `after` gains a new `[remote]` block between
        // `[index]` and the trailing `version_check` line.
        let before = "[index]\nroot = \".\"\nversion_check = true\n";
        let after = "[index]\nroot = \".\"\n[remote]\nmode = \"connect\"\nversion_check = true\n";

        let diff = line_diff(before, after);

        // The two new lines must appear as additions.
        assert!(
            diff.lines().any(|l| l == "+[remote]"),
            "new [remote] header must be marked added; diff:\n{diff}"
        );
        assert!(
            diff.lines().any(|l| l == "+mode = \"connect\""),
            "new mode line must be marked added; diff:\n{diff}"
        );

        // The `version_check` line exists in both — it must NOT be marked changed.
        for line in diff.lines() {
            assert!(
                !(line.starts_with('+') || line.starts_with('-'))
                    || !line.contains("version_check"),
                "unchanged 'version_check' line must not be marked as changed; diff:\n{diff}"
            );
        }
    }
}
