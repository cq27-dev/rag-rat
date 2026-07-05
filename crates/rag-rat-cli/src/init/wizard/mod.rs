//! Init wizard shell: event loop, tabs, footer, dispatch.
//!
//! Steps are plain functions in `steps.rs`.  No traits, no widget abstractions.
//! Every frame starts with `Clear` on the full area so nothing bleeds.

mod catalog;
mod draft;
mod probe;
mod review;
mod state;
mod steps;
mod theme;

use std::collections::HashMap;
use std::io::{Stdout, stdout};
use std::path::Path;
use std::time::Duration;

use rag_rat_core::Config;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Tabs, Wrap};

use self::catalog::CookbookCatalog;
pub(crate) use self::draft::{HooksDraft, WizardDraft};
use self::review::ReviewModel;
use self::state::{OneShotHelp, WizardState};
pub(crate) use self::steps::hooks::{HookConflict, render_chained_hook};
use self::steps::{StepId, init_step, render_step, step_footer, step_handle_key, step_title};
use crate::init::RepoScan;
use crate::init::render::config_root_value;

pub(crate) struct WizardResult {
    pub toml: String,
    pub hooks: HooksDraft,
    pub hook_conflicts: HashMap<&'static str, HookConflict>,
}

struct TerminalGuard(Terminal<CrosstermBackend<Stdout>>);
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
    }
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
        original(info);
    }));
}

enum Tick {
    Continue,
    Quit,
    Confirm(WizardResult),
}

struct Wizard {
    state: WizardState,
    review_open: bool,
    quit_prompt: bool,
    original: Option<String>,
    tab_area: Option<Rect>,
}

impl Wizard {
    fn new(state: WizardState, original: Option<String>) -> Self {
        let mut s =
            Self { state, review_open: false, quit_prompt: false, original, tab_area: None };
        // Init the focused step's state so the first render shows content, not a blank screen.
        let id = s.state.ui.focused;
        s.state.step = Some(init_step(id, &s.state));
        s
    }

    fn dispatch(&mut self, key: KeyEvent) -> Tick {
        if key.kind == KeyEventKind::Release {
            return Tick::Continue;
        }

        // ── quit prompt traps all input ─────────────────────────────────────
        if self.quit_prompt {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.state.probes.on_quit();
                    return Tick::Quit;
                },
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.quit_prompt = false;
                },
                KeyCode::Char('w') | KeyCode::Char('W') => {
                    self.quit_prompt = false;
                    self.open_review();
                },
                _ => {},
            }
            return Tick::Continue;
        }

        if self.state.ui.provision_log_open {
            self.dispatch_provision_log(key);
            return Tick::Continue;
        }

        if self.state.ui.popup.take().is_some() {
            return Tick::Continue;
        }

        if self.review_open {
            return self.dispatch_review(key);
        }

        if matches!(key.code, KeyCode::Esc) {
            self.quit_prompt = true;
            return Tick::Continue;
        }

        // ── Ctrl+arrows: step navigation ────────────────────────────────────
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Right => {
                    self.advance();
                    return Tick::Continue;
                },
                KeyCode::Left => {
                    self.advance_back();
                    return Tick::Continue;
                },
                _ => {},
            }
        }

        // ── step dispatch ───────────────────────────────────────────────────
        let focused = self.state.ui.focused;
        let outcome = step_handle_key(focused, key, &mut self.state);
        if matches!(outcome, steps::Outcome::Advance | steps::Outcome::Consumed) {
            self.refresh_check(focused);
        }
        match outcome {
            steps::Outcome::Advance => self.advance(),
            steps::Outcome::Back => self.advance_back(),
            steps::Outcome::Consumed => {},
            steps::Outcome::Pass => {
                // ── globals ─────────────────────────────────────────────────
                match key.code {
                    KeyCode::Char('w') => self.open_review(),
                    KeyCode::Char('q') => self.quit_prompt = true,
                    KeyCode::Esc => self.quit_prompt = true,
                    KeyCode::Char('?') => self.state.ui.help_visible = !self.state.ui.help_visible,
                    KeyCode::Char('l') | KeyCode::Char('L')
                        if !self.state.provision_log_lines.is_empty() =>
                    {
                        self.state.ui.provision_log_open = true;
                    },
                    _ => {},
                }
            },
        }
        Tick::Continue
    }

    fn open_review(&mut self) {
        self.refresh_all_checks();
        self.review_open = true;
        self.state.ui.review_scroll = 0;
        self.state.ui.ephemeral_keep_acknowledged = false;
    }

    fn dispatch_provision_log(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_provision_log(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_provision_log(1),
            KeyCode::PageUp => self.scroll_provision_log(-10),
            KeyCode::PageDown => self.scroll_provision_log(10),
            KeyCode::Home => {
                self.state.ui.provision_log_scroll = 0;
                self.state.ui.provision_log_follow = false;
            },
            KeyCode::End => {
                self.state.ui.provision_log_scroll = u16::MAX;
                self.state.ui.provision_log_follow = true;
            },
            KeyCode::Char('q') | KeyCode::Esc => self.state.ui.provision_log_open = false,
            _ => {},
        }
    }

    fn dispatch_review(&mut self, key: KeyEvent) -> Tick {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_review(-1);
                Tick::Continue
            },
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_review(1);
                Tick::Continue
            },
            KeyCode::PageUp => {
                self.scroll_review(-10);
                Tick::Continue
            },
            KeyCode::PageDown => {
                self.scroll_review(10);
                Tick::Continue
            },
            KeyCode::Home => {
                self.state.ui.review_scroll = 0;
                Tick::Continue
            },
            KeyCode::End => {
                self.state.ui.review_scroll = self.review_scroll_max() as u16;
                Tick::Continue
            },
            KeyCode::Char('c') => {
                self.refresh_all_checks();
                let model = ReviewModel::build(&self.state, self.original.as_deref());
                if model.can_confirm(self.state.ui.ephemeral_keep_acknowledged) {
                    match self.build_result() {
                        Ok(r) => {
                            self.state.probes.on_quit();
                            Tick::Confirm(r)
                        },
                        Err(_) => Tick::Continue,
                    }
                } else {
                    Tick::Continue
                }
            },
            KeyCode::Char('K') => {
                self.state.ui.ephemeral_keep_acknowledged = true;
                Tick::Continue
            },
            KeyCode::Char('q') | KeyCode::Esc => {
                self.review_open = false;
                Tick::Continue
            },
            _ => Tick::Continue,
        }
    }

    fn dispatch_mouse(&mut self, mouse: MouseEvent) -> Tick {
        if self.state.ui.provision_log_open {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_provision_log(-3),
                MouseEventKind::ScrollDown => self.scroll_provision_log(3),
                _ => {},
            }
            return Tick::Continue;
        }
        if self.state.ui.popup.take().is_some() {
            return Tick::Continue;
        }
        if self.quit_prompt {
            return Tick::Continue;
        }
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(area) = self.tab_area
            && let Some(next) = tab_at(area, mouse.column, mouse.row, &self.state.checks)
        {
            if next != self.state.ui.tab {
                self.move_to_step(next);
            }
            return Tick::Continue;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp =>
                if self.review_open {
                    self.scroll_review(-3);
                } else {
                    steps::scroll_step(self.state.ui.focused, -3, &mut self.state);
                },
            MouseEventKind::ScrollDown =>
                if self.review_open {
                    self.scroll_review(3);
                } else {
                    steps::scroll_step(self.state.ui.focused, 3, &mut self.state);
                },
            _ => {},
        }
        Tick::Continue
    }

    fn scroll_review(&mut self, delta: isize) {
        let max = self.review_scroll_max();
        let cur = self.state.ui.review_scroll as isize;
        self.state.ui.review_scroll = cur.saturating_add(delta).clamp(0, max as isize) as u16;
    }

    fn scroll_provision_log(&mut self, delta: isize) {
        let max = self.state.provision_log_lines.len().saturating_sub(1);
        let cur = self.state.ui.provision_log_scroll.min(max as u16) as isize;
        let next = cur.saturating_add(delta).clamp(0, max as isize) as u16;
        self.state.ui.provision_log_scroll = next;
        self.state.ui.provision_log_follow = next as usize >= max;
    }

    fn review_scroll_max(&self) -> usize {
        ReviewModel::build(&self.state, self.original.as_deref())
            .body_line_count()
            .saturating_sub(1)
    }

    fn advance(&mut self) {
        self.move_step(true);
    }
    fn advance_back(&mut self) {
        self.move_step(false);
    }

    fn move_step(&mut self, forward: bool) {
        let i = self.state.ui.focused as usize;
        let n = StepId::COUNT;
        let next = if forward { (i + 1) % n } else { (i + n - 1) % n };
        self.move_to_step(next);
    }

    fn move_to_step(&mut self, next: usize) {
        self.refresh_check(self.state.ui.focused);
        self.state.ui.focused = StepId::ALL[next];
        self.state.ui.tab = next;
        // Re-init step state for the newly focused step.
        let id = self.state.ui.focused;
        self.state.step = Some(init_step(id, &self.state));
        self.refresh_check(id);
    }

    fn refresh_check(&mut self, id: StepId) {
        let static_check = steps::validate_step(id, &self.state);
        let probe_check = match self.state.probes.status(id) {
            probe::ProbeStatus::Done { result, .. } => Some(result.clone()),
            _ => None,
        };
        self.state.checks[id.index()] = merge_checks(static_check, probe_check);
    }

    fn refresh_all_checks(&mut self) {
        for id in StepId::ALL {
            self.refresh_check(id);
        }
    }

    fn poll_probes_into_checks(&mut self) {
        // The ephemeral provision test is a throwaway pass/fail spin-up (`tune = None`); it runs no
        // throughput sweep and never touches the user's configured `[remote] concurrency` cap, so
        // there is nothing to apply back to the draft here.
        for (step, _kind, _result) in self.state.probes.poll() {
            self.refresh_check(step);
        }
    }

    fn build_result(&self) -> anyhow::Result<WizardResult> {
        let draft = &self.state.draft;
        let toml = match &self.original {
            Some(orig) => draft.patch_existing(orig)?,
            None => draft.write_fresh(),
        };
        Ok(WizardResult {
            toml,
            hooks: draft.hooks.clone(),
            hook_conflicts: self.state.hook_conflicts.clone(),
        })
    }

    // ── render ──────────────────────────────────────────────────────────────

    fn render(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        f.render_widget(Clear, area); // full-frame clear — no bleed ever
        f.render_widget(Block::default().style(theme::base()), area);

        if self.review_open {
            self.tab_area = None;
            review::render(
                &ReviewModel::build(&self.state, self.original.as_deref()),
                f,
                area,
                self.state.ui.review_scroll,
            );
            return;
        }

        let rows =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
                .split(area);

        self.tab_area = Some(rows[0]);
        self.render_tabs(f, rows[0]);
        self.render_main(f, rows[1]);
        self.render_footer(f, rows[2]);

        if self.state.ui.help_visible {
            self.render_help(f, area);
        }
        if let Some(help) = self.state.ui.popup {
            self.render_one_shot_help(f, area, help);
        }
        if self.state.ui.provision_log_open {
            self.render_provision_log(f, area);
        }
        if self.quit_prompt {
            self.render_quit_prompt(f, area);
        }
    }

    fn render_tabs(&self, f: &mut ratatui::Frame, area: Rect) {
        let titles: Vec<Line> = StepId::ALL
            .iter()
            .enumerate()
            .map(|(i, id)| tab_title_line(*id, self.state.checks[i].severity))
            .collect();
        f.render_widget(
            Tabs::new(titles)
                .select(self.state.ui.tab)
                .block(theme::block("Steps"))
                .style(theme::muted())
                .highlight_style(theme::selected())
                .divider(" ")
                .padding("", ""),
            area,
        );
    }

    fn render_main(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let block = theme::block(step_title(self.state.ui.focused));
        let inner = block.inner(area);
        f.render_widget(block, area);
        render_step(self.state.ui.focused, f, inner, &mut self.state);
    }

    fn render_footer(&self, f: &mut ratatui::Frame, area: Rect) {
        let bar = theme::footer();
        let footer = step_footer(self.state.ui.focused);
        let mut spans = vec![Span::styled(format!(" {}  w review  ? help  q quit ", footer), bar)];
        if let Some(msg) = &self.state.checks[self.state.ui.focused as usize].message {
            spans.push(Span::styled(format!(" ⚠ {} ", msg), theme::footer_warning()));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
    }

    fn render_help(&self, f: &mut ratatui::Frame, area: Rect) {
        let popup = centered(50, 50, area);
        f.render_widget(Clear, popup);
        let lines = vec![
            Line::from(Span::styled("Keys", theme::title())),
            Line::from("  ↑↓/j/k    move cursor or scroll"),
            Line::from("  PgUp/PgDn scroll containers"),
            Line::from("  Space     toggle / select"),
            Line::from("  Enter     advance / expand"),
            Line::from("  Esc       back / quit prompt"),
            Line::from("  Ctrl+←→   next/prev step"),
            Line::from("  w         review"),
            Line::from("  q         quit"),
            Line::from("  ?         this help"),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .style(theme::popup())
                .wrap(Wrap { trim: true })
                .block(theme::popup_block("Help")),
            popup,
        );
    }

    fn render_one_shot_help(&self, f: &mut ratatui::Frame, area: Rect, help: OneShotHelp) {
        let popup = centered(58, 35, area);
        f.render_widget(Clear, popup);
        let (title, body): (&str, &[&str]) = match help {
            OneShotHelp::EmbeddingOff => ("Remote mode: none", &[
                "Remote embeddings are off. Search uses local lexical and structural signals only.",
                "This is the cheapest setup, but it loses semantic recall on larger or more \
                 ambiguous codebases.",
            ]),
            OneShotHelp::Connect => ("Remote mode: connect", &[
                "Connect uses an existing Ollama server. Fill endpoint with the server URL, \
                 choose the server model, then press t to test /api/embed.",
                "Use this when the model is already running somewhere reliable.",
            ]),
            OneShotHelp::Ephemeral => ("Remote mode: ephemeral", &[
                "Ephemeral can create a temporary provider box during reconcile or a provision \
                 test.",
                "Because that may start paid remote compute, the wizard asks you to type \
                 provision in the confirm field before p will run the test.",
            ]),
        };
        let lines = body
            .iter()
            .flat_map(|line| [Line::from(*line), Line::from("")])
            .chain([Line::from("Press any key to continue.")])
            .collect::<Vec<_>>();
        f.render_widget(
            Paragraph::new(lines)
                .style(theme::popup())
                .wrap(Wrap { trim: true })
                .block(theme::popup_block(title)),
            popup,
        );
    }

    fn render_provision_log(&self, f: &mut ratatui::Frame, area: Rect) {
        let popup = centered(78, 66, area);
        f.render_widget(Clear, popup);
        let inner_height = usize::from(popup.height.saturating_sub(2)).max(1);
        let max = self.state.provision_log_lines.len().saturating_sub(inner_height);
        let scroll = self.state.ui.provision_log_scroll.min(max as u16);
        let title = if self.state.provision_log_rx.is_some() {
            format!("{}: running", self.state.provision_log_title)
        } else {
            self.state.provision_log_title.clone()
        };
        let text = if self.state.provision_log_lines.is_empty() {
            "No provision log yet.".to_string()
        } else {
            self.state.provision_log_lines.join("\n")
        };
        f.render_widget(
            Paragraph::new(text)
                .style(theme::popup())
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .block(theme::popup_block(title)),
            popup,
        );
        if popup.height > 2 {
            let footer = Rect::new(
                popup.x.saturating_add(2),
                popup.y + popup.height - 1,
                popup.width.saturating_sub(4),
                1,
            );
            f.render_widget(
                Paragraph::new(" ↑↓/j/k PgUp/PgDn scroll  End follow  Esc/q close ")
                    .style(theme::popup()),
                footer,
            );
        }
    }

    fn render_quit_prompt(&self, f: &mut ratatui::Frame, area: Rect) {
        let popup = centered(40, 32, area);
        f.render_widget(Clear, popup);
        let lines = vec![
            Line::from(Span::styled("Quit without saving?", theme::title())),
            Line::from(""),
            Line::from("  y  quit (discard)"),
            Line::from("  n  go back"),
            Line::from("  w  review"),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .style(theme::popup())
                .block(theme::popup_block("Quit").border_style(theme::warning())),
            popup,
        );
    }
}

fn merge_checks(
    static_check: steps::CheckResult,
    probe_check: Option<steps::CheckResult>,
) -> steps::CheckResult {
    let Some(probe_check) = probe_check else { return static_check };
    match (static_check.severity, probe_check.severity) {
        (steps::Sev::Block, _) => static_check,
        (_, steps::Sev::Block) => probe_check,
        (steps::Sev::Warn, steps::Sev::Warn) => combine_warnings(static_check, probe_check),
        (steps::Sev::Warn, steps::Sev::Ok) => static_check,
        (_, steps::Sev::Warn) => probe_check,
        (steps::Sev::Ok, steps::Sev::Ok) => static_check,
    }
}

fn combine_warnings(
    static_check: steps::CheckResult,
    probe_check: steps::CheckResult,
) -> steps::CheckResult {
    match (probe_check.message, static_check.message) {
        (Some(probe), Some(static_msg)) if probe != static_msg =>
            steps::CheckResult::warn(format!("{probe}; {static_msg}")),
        (Some(message), _) | (None, Some(message)) => steps::CheckResult::warn(message),
        (None, None) => steps::CheckResult { severity: steps::Sev::Warn, message: None },
    }
}

fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

fn tab_at(area: Rect, column: u16, row: u16, checks: &[steps::CheckResult]) -> Option<usize> {
    let inner = tab_inner(area)?;
    if row != inner.y || column < inner.x || column >= inner.x.saturating_add(inner.width) {
        return None;
    }

    let mut x = inner.x;
    for (i, id) in StepId::ALL.iter().enumerate() {
        let severity = checks.get(i).map_or(steps::Sev::Ok, |check| check.severity);
        let Some((start, end)) = tab_hit_range(inner, x, *id, severity) else { break };
        if column >= start && column < end {
            return Some(i);
        }
        x = end;
        if i + 1 < StepId::COUNT {
            x = x.saturating_add(1);
        }
    }
    None
}

fn tab_inner(area: Rect) -> Option<Rect> {
    if area.width <= 2 || area.height <= 2 {
        return None;
    }
    Some(Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    ))
}

fn tab_title_width(id: StepId, severity: steps::Sev) -> u16 {
    tab_title_line(id, severity).width() as u16
}

fn tab_hit_range(inner: Rect, x: u16, id: StepId, severity: steps::Sev) -> Option<(u16, u16)> {
    if x >= inner.right() {
        return None;
    }
    let end = x.saturating_add(tab_title_width(id, severity)).min(inner.right());
    (end > x).then_some((x, end))
}

fn tab_title_line(id: StepId, severity: steps::Sev) -> Line<'static> {
    let Some((sev, sev_style)) = (match severity {
        steps::Sev::Ok => None,
        steps::Sev::Warn => Some(("!", theme::warning())),
        steps::Sev::Block => Some(("✗", theme::error())),
    }) else {
        return Line::from(Span::raw(format!(" {} ", step_title(id))));
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled(sev, sev_style),
        Span::raw(format!(" {} ", step_title(id))),
    ])
}

// ── run_wizard ────────────────────────────────────────────────────────────

pub(crate) fn run_wizard(
    scan: RepoScan,
    existing: Option<(String, Config)>,
    config_path: &Path,
    scan_root: std::path::PathBuf,
) -> anyhow::Result<Option<WizardResult>> {
    let (draft, original, cookbooks) =
        initial_draft(&scan, existing.as_ref(), config_path, scan_root);
    let mut wizard = Wizard::new(WizardState::with_cookbooks(draft, scan, cookbooks), original);
    for id in StepId::ALL {
        wizard.state.checks[id as usize] = steps::validate_step(id, &wizard.state);
    }

    install_panic_hook();
    let mut guard = TerminalGuard(Terminal::new(CrosstermBackend::new(stdout()))?);
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture, cursor::Hide)?;

    let result = loop {
        guard.0.draw(|f| wizard.render(f))?;
        wizard.poll_probes_into_checks();
        wizard.state.poll_provision_log();
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let tick = match event::read()? {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    wizard.state.probes.on_quit();
                    break None;
                }
                wizard.dispatch(key)
            },
            Event::Mouse(mouse) => wizard.dispatch_mouse(mouse),
            _ => continue,
        };
        match tick {
            Tick::Continue => {},
            Tick::Quit => break None,
            Tick::Confirm(r) => {
                wizard.state.probes.on_quit();
                break Some(r);
            },
        }
    };
    drop(guard);
    Ok(result)
}

fn initial_draft(
    scan: &RepoScan,
    existing: Option<&(String, Config)>,
    config_path: &Path,
    scan_root: std::path::PathBuf,
) -> (WizardDraft, Option<String>, CookbookCatalog) {
    match existing {
        Some((raw, cfg)) => (
            WizardDraft::from_existing(raw, cfg, config_path),
            Some(raw.clone()),
            CookbookCatalog::from_raw(raw),
        ),
        None => (fresh_draft(scan, config_path, scan_root), None, CookbookCatalog::default()),
    }
}

fn fresh_draft(scan: &RepoScan, config_path: &Path, root_abs: std::path::PathBuf) -> WizardDraft {
    let root_value = config_root_value(&root_abs, config_path);
    WizardDraft::from_scan(scan, root_value, root_abs)
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::style::Color;

    use super::*;

    fn test_scan() -> RepoScan {
        RepoScan::default()
    }

    /// A synthetic ABSOLUTE path for these logic tests. A bare `/repo/sub` is absolute on Unix but
    /// NOT on Windows (no drive), which would send `config_root_value` down its relative branch and
    /// mis-count depth. Prefix a drive on Windows so `is_absolute()` holds on both. `Path` treats
    /// `/` and `\` as equivalent separators on Windows, so the forward slashes are fine.
    fn abs(unix_style: &str) -> std::path::PathBuf {
        #[cfg(windows)]
        {
            std::path::PathBuf::from(format!("C:{unix_style}"))
        }
        #[cfg(not(windows))]
        {
            std::path::PathBuf::from(unix_style)
        }
    }

    #[test]
    fn fresh_draft_roots_relative_to_config_file_parent() {
        let root = std::path::PathBuf::from("/repo");
        let draft = fresh_draft(&test_scan(), Path::new(".config/rag-rat.toml"), root);

        assert_eq!(draft.root_value, "..");
    }

    #[test]
    fn fresh_initial_draft_uses_scan_root_not_cwd() {
        let root = abs("/repo/sub");
        let config_path = abs("/repo/sub/rag-rat.toml");
        let (draft, original, cookbooks) =
            initial_draft(&test_scan(), None, &config_path, root.clone());

        assert_eq!(draft.root_abs, root);
        assert_eq!(draft.root_value, ".");
        assert!(original.is_none());
        assert_eq!(cookbooks.entries()[0].key, "modal");
    }

    #[test]
    fn polling_probe_results_updates_step_checks() {
        let mut w = headless(test_scan(), None);
        w.state.probes.spawn(StepId::Integration, probe::ProbeKind::VersionCheck, || {
            steps::CheckResult::warn("new version available")
        });

        for _ in 0..100 {
            w.poll_probes_into_checks();
            if w.state.checks[StepId::Integration.index()]
                .message
                .as_deref()
                .is_some_and(|message| message.contains("new version"))
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        panic!("probe result did not update Integration check");
    }

    #[test]
    fn ephemeral_probe_does_not_overwrite_the_concurrency_cap() {
        // Cap model: a passing ephemeral provision test must NOT change the user's configured
        // `[remote] concurrency` (the cap) — the tuned knee is surfaced in the provision log and
        // the host cache, never baked back over the cap.
        let mut w = headless(test_scan(), None);
        w.state.draft.remote = Some(draft::RemoteDraft {
            model: "all-minilm".to_string(),
            backend: rag_rat_core::config::RemoteBackend::Ollama,
            mode: draft::RemoteMode::Ephemeral("@rag-rat/cookbook modal".to_string()),
            query_endpoint: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: 32,
            max_batch_chars: rag_rat_core::config::RemoteEmbeddingConfig::default().max_batch_chars,
            auth_env: None,
        });
        w.state.probes.spawn(
            StepId::Embedding,
            probe::ProbeKind::EphemeralTest,
            steps::CheckResult::ok,
        );

        for _ in 0..100 {
            w.poll_probes_into_checks();
            if matches!(w.state.probes.status(StepId::Embedding), probe::ProbeStatus::Done {
                kind: probe::ProbeKind::EphemeralTest,
                ..
            }) {
                assert_eq!(
                    w.state.draft.remote.as_ref().unwrap().concurrency,
                    32,
                    "the configured concurrency cap must be left untouched",
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("ephemeral probe never completed");
    }

    #[test]
    fn opening_review_refreshes_stale_embedding_check() {
        let mut w = headless(test_scan(), None);
        w.state.draft.model = "none".to_string();
        w.state.draft.remote = Some(draft::RemoteDraft {
            model: "all-minilm".to_string(),
            backend: rag_rat_core::config::RemoteBackend::Ollama,
            mode: draft::RemoteMode::Connect("http://localhost:11434".to_string()),
            query_endpoint: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: rag_rat_core::config::RemoteEmbeddingConfig::default().concurrency,
            max_batch_chars: rag_rat_core::config::RemoteEmbeddingConfig::default().max_batch_chars,
            auth_env: None,
        });
        w.state.checks[StepId::Embedding.index()] = steps::CheckResult::ok();

        w.dispatch(KeyEvent::from(KeyCode::Char('w')));

        assert!(w.review_open);
        assert_eq!(w.state.checks[StepId::Embedding.index()].severity, steps::Sev::Block);
    }

    #[test]
    fn opening_review_preserves_probe_warning() {
        let mut w = headless(test_scan(), None);
        let probe_id = w.state.probes.current(StepId::Embedding);
        assert!(w.state.probes.apply(probe::ProbeMsg {
            probe_id,
            kind: probe::ProbeKind::Download,
            result: steps::CheckResult::warn("download failed"),
        }));
        w.state.checks[StepId::Embedding.index()] = steps::CheckResult::ok();

        w.dispatch(KeyEvent::from(KeyCode::Char('w')));

        assert!(w.review_open);
        let check = &w.state.checks[StepId::Embedding.index()];
        assert_eq!(check.severity, steps::Sev::Warn);
        assert_eq!(check.message.as_deref(), Some("download failed"));
    }

    #[test]
    fn merge_checks_combines_static_and_probe_warnings() {
        let check = merge_checks(
            steps::CheckResult::warn("quality may vary"),
            Some(steps::CheckResult::warn("connect failed")),
        );

        assert_eq!(check.severity, steps::Sev::Warn);
        assert_eq!(check.message.as_deref(), Some("connect failed; quality may vary"));
    }

    #[test]
    fn confirming_review_aborts_running_ephemeral_probe() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut w = headless(test_scan(), None);
        w.state
            .draft
            .bindings
            .insert(rag_rat_core::language::Language::Rust, vec![Path::new(".").to_path_buf()]);
        w.state.draft.remote = Some(draft::RemoteDraft {
            model: "all-minilm".to_string(),
            backend: rag_rat_core::config::RemoteBackend::Ollama,
            mode: draft::RemoteMode::Ephemeral("@rag-rat/cookbook modal".to_string()),
            query_endpoint: None,
            gpu: None,
            num_ctx: None,
            batch_size: 256,
            concurrency: rag_rat_core::config::RemoteEmbeddingConfig::default().concurrency,
            max_batch_chars: rag_rat_core::config::RemoteEmbeddingConfig::default().max_batch_chars,
            auth_env: None,
        });
        w.state.ui.ephemeral_keep_acknowledged = true;
        w.review_open = true;
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        w.state.probes.spawn(StepId::Embedding, probe::ProbeKind::EphemeralTest, move || {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(20));
            worker_finished.store(true, Ordering::Release);
            steps::CheckResult::ok()
        });
        started_rx.recv().unwrap();

        let tick = w.dispatch(KeyEvent::from(KeyCode::Char('c')));

        assert!(matches!(tick, Tick::Confirm(_)));
        assert!(matches!(w.state.probes.status(StepId::Embedding), probe::ProbeStatus::Idle));
        assert!(finished.load(Ordering::Acquire));
    }

    fn headless(scan: RepoScan, existing: Option<(String, Config)>) -> Wizard {
        let (draft, original, cookbooks) = match existing {
            Some((raw, cfg)) => (
                WizardDraft::from_existing(&raw, &cfg, Path::new("rag-rat.toml")),
                Some(raw.clone()),
                CookbookCatalog::from_raw(&raw),
            ),
            None => (
                WizardDraft::from_scan(&scan, ".".to_string(), Path::new(".").to_path_buf()),
                None,
                CookbookCatalog::default(),
            ),
        };
        let mut w = Wizard::new(WizardState::with_cookbooks(draft, scan, cookbooks), original);
        for id in StepId::ALL {
            w.state.checks[id as usize] = steps::validate_step(id, &w.state);
        }
        w
    }

    fn feed(w: &mut Wizard, key: KeyEvent) -> Option<WizardResult> {
        match w.dispatch(key) {
            Tick::Confirm(r) => Some(r),
            _ => None,
        }
    }

    fn ctrl_right() -> KeyEvent {
        KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)
    }

    fn focus_embedding(w: &mut Wizard) {
        w.state.ui.focused = StepId::Embedding;
        w.state.ui.tab = StepId::Embedding.index();
        w.state.step = Some(init_step(StepId::Embedding, &w.state));
    }

    fn tab_click_column(w: &Wizard, area: Rect, index: usize) -> u16 {
        let y = area.y + 1;
        (area.x..area.x + area.width)
            .find(|column| tab_at(area, *column, y, &w.state.checks) == Some(index))
            .expect("tab should fit in test area")
    }

    fn left_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn selected_tab_bg_columns(w: &Wizard, area: Rect) -> Vec<u16> {
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| w.render_tabs(f, area)).unwrap();
        let buffer = terminal.backend().buffer();
        let selected_bg = theme::selected().bg.expect("selected style has background");
        let y = area.y + 1;
        (area.x..area.x + area.width).filter(|x| buffer[(*x, y)].bg == selected_bg).collect()
    }

    fn tab_cell_symbol(w: &Wizard, area: Rect, column: u16) -> String {
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| w.render_tabs(f, area)).unwrap();
        terminal.backend().buffer()[(column, area.y + 1)].symbol().to_string()
    }

    fn assert_bg(buffer: &Buffer, area: Rect, bg: Color) {
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                assert_eq!(buffer[(x, y)].bg, bg, "cell {x},{y} has unexpected background");
            }
        }
    }

    fn buffer_text(buffer: &Buffer, width: u16, height: u16) -> String {
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn enter_advances_and_w_opens_review() {
        let mut w = headless(test_scan(), None);
        for _ in 0..StepId::COUNT {
            w.dispatch(ctrl_right());
        }
        w.dispatch(KeyEvent::from(KeyCode::Char('w')));
        assert!(w.review_open);
    }

    #[test]
    fn release_keys_are_ignored_and_ctrl_arrows_move_steps() {
        let mut w = headless(test_scan(), None);

        w.dispatch(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(w.state.ui.focused, StepId::Oracle);

        w.dispatch(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(w.state.ui.focused, StepId::Indexing);

        w.dispatch(KeyEvent::new_with_kind(
            KeyCode::Right,
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        ));
        assert_eq!(w.state.ui.focused, StepId::Indexing);
    }

    #[test]
    fn q_prompts_then_y_quits() {
        let mut w = headless(test_scan(), None);
        assert!(feed(&mut w, KeyEvent::from(KeyCode::Char('q'))).is_none()); // q → prompt
        assert!(w.quit_prompt);
        // y → quit
        match w.dispatch(KeyEvent::from(KeyCode::Char('y'))) {
            Tick::Quit => {},
            _ => panic!("expected Quit"),
        }
    }

    #[test]
    fn quit_prompt_n_dismisses() {
        let mut w = headless(test_scan(), None);
        w.dispatch(KeyEvent::from(KeyCode::Char('q')));
        assert!(w.quit_prompt);
        w.dispatch(KeyEvent::from(KeyCode::Char('n')));
        assert!(!w.quit_prompt);
    }

    #[test]
    fn quit_prompt_w_opens_review() {
        let area = Rect::new(0, 0, 80, 24);
        let mut w = headless(test_scan(), None);
        w.dispatch(KeyEvent::from(KeyCode::Char('q')));

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| w.render_quit_prompt(f, area)).unwrap();
        let rendered: String =
            terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("w  review"), "{rendered:?}");

        w.dispatch(KeyEvent::from(KeyCode::Char('w')));

        assert!(!w.quit_prompt);
        assert!(w.review_open);
    }

    #[test]
    fn esc_opens_quit_prompt_without_moving_tabs() {
        let mut w = headless(test_scan(), None);
        w.state.ui.focused = StepId::Embedding;
        w.state.ui.tab = StepId::Embedding.index();

        w.dispatch(KeyEvent::from(KeyCode::Esc));

        assert!(w.quit_prompt);
        assert_eq!(w.state.ui.focused, StepId::Embedding);
        assert_eq!(w.state.ui.tab, StepId::Embedding.index());
    }

    #[test]
    fn clicking_tab_switches_step() {
        let mut w = headless(test_scan(), None);
        let area = Rect::new(0, 0, 120, 3);
        w.tab_area = Some(area);

        let target = StepId::Embedding.index();
        let tick = w.dispatch_mouse(left_click(tab_click_column(&w, area, target), area.y + 1));

        assert!(matches!(tick, Tick::Continue));
        assert_eq!(w.state.ui.focused, StepId::Embedding);
        assert_eq!(w.state.ui.tab, target);
    }

    #[test]
    fn rendered_selected_tab_background_matches_hitbox() {
        let mut w = headless(test_scan(), None);
        let area = Rect::new(0, 0, 120, 3);
        let target = StepId::Embedding.index();
        w.state.ui.tab = target;

        let y = area.y + 1;
        let rendered = selected_tab_bg_columns(&w, area);
        let hitbox: Vec<u16> = (area.x..area.x + area.width)
            .filter(|x| tab_at(area, *x, y, &w.state.checks) == Some(target))
            .collect();

        assert_eq!(rendered, hitbox);
        assert!(!rendered.is_empty());
        assert_eq!(tab_cell_symbol(&w, area, rendered[0]), " ");
        assert_ne!(tab_cell_symbol(&w, area, rendered[0] + 1), " ");
        assert_eq!(tab_cell_symbol(&w, area, rendered[rendered.len() - 1]), " ");
        assert_ne!(tab_at(area, rendered[0].saturating_sub(1), y, &w.state.checks), Some(target));
        assert_ne!(
            tab_at(area, rendered[rendered.len() - 1].saturating_add(1), y, &w.state.checks),
            Some(target)
        );
    }

    #[test]
    fn review_scrolls_without_taking_keep_acknowledgement() {
        let mut w = headless(test_scan(), None);
        w.dispatch(KeyEvent::from(KeyCode::Char('w')));
        assert!(w.review_open);
        w.dispatch(KeyEvent::from(KeyCode::PageDown));
        assert!(w.state.ui.review_scroll > 0);
        w.dispatch(KeyEvent::from(KeyCode::Char('k')));
        assert!(!w.state.ui.ephemeral_keep_acknowledged);
        w.dispatch(KeyEvent::from(KeyCode::Char('K')));
        assert!(w.state.ui.ephemeral_keep_acknowledged);
    }

    #[test]
    fn remote_mode_help_popups_are_once_per_mode() {
        let mut w = headless(test_scan(), None);
        focus_embedding(&mut w);

        w.dispatch(KeyEvent::from(KeyCode::Tab));
        w.dispatch(KeyEvent::from(KeyCode::Down));
        w.dispatch(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(w.state.ui.popup, Some(OneShotHelp::Connect));

        w.dispatch(KeyEvent::from(KeyCode::Enter));
        assert_eq!(w.state.ui.popup, None);
        w.dispatch(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(w.state.ui.popup, None);

        w.dispatch(KeyEvent::from(KeyCode::Down));
        w.dispatch(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(w.state.ui.popup, Some(OneShotHelp::Ephemeral));
    }

    #[test]
    fn provision_log_popup_captures_scrolls_and_reopens() {
        let mut w = headless(test_scan(), None);
        let (tx, rx) = std::sync::mpsc::channel();
        w.state.start_provision_log(rx);
        tx.send("first".to_string()).unwrap();
        tx.send("second".to_string()).unwrap();
        w.state.poll_provision_log();

        assert!(w.state.ui.provision_log_open);
        assert!(w.state.provision_log_lines.iter().any(|line| line == "first"));
        assert_eq!(w.state.ui.provision_log_scroll, u16::MAX);

        w.dispatch(KeyEvent::from(KeyCode::Home));
        assert_eq!(w.state.ui.provision_log_scroll, 0);
        assert!(!w.state.ui.provision_log_follow);

        w.dispatch(KeyEvent::from(KeyCode::Esc));
        assert!(!w.state.ui.provision_log_open);
        w.dispatch(KeyEvent::from(KeyCode::Char('l')));
        assert!(w.state.ui.provision_log_open);
    }

    #[test]
    fn provision_log_scroll_keys_cover_edges() {
        let mut w = headless(test_scan(), None);
        let (tx, rx) = std::sync::mpsc::channel();
        w.state.start_provision_log(rx);
        for i in 0..40 {
            tx.send(format!("line {i}")).unwrap();
        }
        w.state.poll_provision_log();

        w.dispatch(KeyEvent::from(KeyCode::Home));
        assert_eq!(w.state.ui.provision_log_scroll, 0);

        w.dispatch(KeyEvent::from(KeyCode::PageDown));
        assert!(w.state.ui.provision_log_scroll > 0);

        w.dispatch(KeyEvent::from(KeyCode::End));
        assert!(w.state.ui.provision_log_follow);

        w.dispatch(KeyEvent::from(KeyCode::PageUp));
        assert!(!w.state.ui.provision_log_follow);

        w.dispatch(KeyEvent::from(KeyCode::Char('q')));
        assert!(!w.state.ui.provision_log_open);
    }

    #[test]
    fn full_render_covers_normal_review_help_popup_log_and_quit_layers() {
        let area = Rect::new(0, 0, 120, 36);
        let mut w = headless(test_scan(), None);
        let (tx, rx) = std::sync::mpsc::channel();
        w.state.start_provision_log(rx);
        tx.send("first".to_string()).unwrap();
        w.state.poll_provision_log();

        for setup in [
            |w: &mut Wizard| {
                w.review_open = false;
                w.quit_prompt = false;
                w.state.ui.help_visible = false;
                w.state.ui.popup = None;
                w.state.ui.provision_log_open = false;
            },
            |w: &mut Wizard| {
                w.review_open = true;
            },
            |w: &mut Wizard| {
                w.review_open = false;
                w.state.ui.help_visible = true;
            },
            |w: &mut Wizard| {
                w.state.ui.help_visible = false;
                w.state.ui.popup = Some(OneShotHelp::Ephemeral);
            },
            |w: &mut Wizard| {
                w.state.ui.popup = None;
                w.state.ui.provision_log_open = true;
            },
            |w: &mut Wizard| {
                w.state.ui.provision_log_open = false;
                w.quit_prompt = true;
            },
        ] {
            setup(&mut w);
            let backend = TestBackend::new(area.width, area.height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| w.render(f)).unwrap();
            let rendered: String =
                terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();
            assert!(!rendered.trim().is_empty());
        }
    }

    #[test]
    fn modal_popups_use_main_background() {
        let area = Rect::new(0, 0, 100, 40);
        let mut w = headless(test_scan(), None);
        let (tx, rx) = std::sync::mpsc::channel();
        w.state.start_provision_log(rx);
        tx.send("first".to_string()).unwrap();
        w.state.poll_provision_log();
        let popup_bg = theme::base().bg.expect("base style has background");

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                w.render_help(f, area);
                assert_bg(f.buffer_mut(), centered(50, 50, area), popup_bg);

                w.render_one_shot_help(f, area, OneShotHelp::Ephemeral);
                assert_bg(f.buffer_mut(), centered(58, 35, area), popup_bg);

                w.render_provision_log(f, area);
                assert_bg(f.buffer_mut(), centered(78, 66, area), popup_bg);

                w.render_quit_prompt(f, area);
                assert_bg(f.buffer_mut(), centered(40, 32, area), popup_bg);
            })
            .unwrap();
    }

    #[test]
    fn provision_log_wraps_long_lines_inside_popup() {
        let area = Rect::new(0, 0, 80, 18);
        let mut w = headless(test_scan(), None);
        w.state.provision_log_title = "Oracle tool test".to_string();
        w.state.provision_log_lines = vec![
            concat!(
                "oracle probe produced a long diagnostic with enough words to need wrapping \
                 before ",
                "the popup edge and it must still show tail-token-inside-popup"
            )
            .to_string(),
        ];

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| w.render_provision_log(f, area)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), area.width, area.height);

        assert!(
            rendered.contains("tail-token-inside-popup"),
            "long provision log line should wrap instead of clipping:\n{rendered}"
        );
    }

    #[test]
    fn review_shortcut_is_visible_on_every_step_footer() {
        let area = Rect::new(0, 0, 120, 1);
        let mut w = headless(test_scan(), None);

        for step in StepId::ALL {
            w.state.ui.focused = step;
            let backend = TestBackend::new(area.width, area.height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| w.render_footer(f, area)).unwrap();
            let rendered: String =
                terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();

            assert!(rendered.contains("w review"), "{step:?} footer: {rendered:?}");
        }
    }
}
