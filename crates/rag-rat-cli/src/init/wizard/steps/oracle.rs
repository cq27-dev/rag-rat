//! Oracle step — SCIP tool availability toggle + probe.

use std::sync::mpsc::Sender;

use rag_rat_base::language::Language;
use rag_rat_oracle::{OracleTool, ToolAvailability, ToolManifest, probe_oracle_tool};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::super::probe::{ProbeKind, ProbeStatus};
use super::super::state::WizardState;
use super::super::theme;
use super::types::{CheckResult, Outcome, Sev, StepId};

pub(super) fn render_oracle(f: &mut Frame, area: Rect, state: &WizardState) {
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);

    let on = state.draft.oracle_auto_run;
    let label = if on { "[ ON  ]" } else { "[ OFF ]" };
    f.render_widget(
        Paragraph::new(Span::styled(label, theme::focus()))
            .style(theme::base())
            .block(theme::block("space: toggle  t: test")),
        chunks[0],
    );

    let detected: Vec<&str> = Language::all()
        .iter()
        .filter(|&&l| state.scan.language_counts().get(&l).copied().unwrap_or(0) > 0)
        .map(|&l| l.as_str())
        .collect();

    let mut lines = vec![
        Line::from(Span::styled("SCIP tools for your languages:", theme::base())),
        Line::from(""),
    ];
    match state.probes.status(StepId::Oracle) {
        ProbeStatus::Running(ProbeKind::OracleTool) => {
            lines.push(Line::from(Span::styled(" Tool test running...", theme::warning())));
            lines.push(Line::from(""));
        },
        ProbeStatus::Done { result, .. } => {
            let style = match result.severity {
                Sev::Ok => theme::success(),
                Sev::Warn => theme::warning(),
                Sev::Block => theme::error(),
            };
            let msg = result.message.as_deref().unwrap_or("Oracle tool test passed.");
            lines.push(Line::from(Span::styled(format!(" Tool test: {msg}"), style)));
            lines.push(Line::from(""));
        },
        _ => {},
    }
    for tool in OracleTool::ALL {
        // The wizard lists BATCH (`.scip`-producing) tools only — a live backend (`ra-lsp`) is a
        // watcher feature toggled via `[oracle.live]`, not a tool the user installs separately
        // (#534).
        if !tool.batch_capable() {
            continue;
        }
        let m = ToolManifest::for_tool(*tool);
        let relevant = m.languages.iter().any(|l| detected.contains(l));
        let style = if relevant { theme::accent() } else { theme::muted() };
        let name = format!("{:?}", tool)
            .replace("RustAnalyzer", "rust-analyzer")
            .replace("ScipClang", "scip-clang")
            .replace("ScipPython", "scip-python")
            .replace("ScipTypescript", "scip-typescript")
            .replace("ScipJava", "scip-java");
        lines.push(Line::from(vec![
            Span::styled(format!(" {} {} ", if relevant { "▶" } else { " " }, name), style),
            Span::styled(format!("— {}", m.languages.join(", ")), theme::muted()),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .wrap(Wrap { trim: false })
            .block(theme::block("Tool availability")),
        chunks[1],
    );
}

pub(super) fn handle_oracle(key: KeyEvent, state: &mut WizardState) -> Outcome {
    match key.code {
        KeyCode::Char(' ') => {
            state.draft.oracle_auto_run = !state.draft.oracle_auto_run;
            Outcome::Consumed
        },
        KeyCode::Char('t') | KeyCode::Char('T') => {
            let tools = tools_for_scan(state);
            let (log_tx, log_rx) = std::sync::mpsc::channel();
            state.start_oracle_log(log_rx);
            state.probes.spawn(StepId::Oracle, ProbeKind::OracleTool, move || {
                probe_oracle_tools(tools, log_tx)
            });
            Outcome::Consumed
        },
        KeyCode::Enter => Outcome::Advance,
        KeyCode::Esc => Outcome::Back,
        _ => Outcome::Pass,
    }
}

fn tools_for_scan(state: &WizardState) -> Vec<OracleTool> {
    let detected: Vec<&str> = Language::all()
        .iter()
        .filter(|&&l| state.scan.language_counts().get(&l).copied().unwrap_or(0) > 0)
        .map(|&l| l.as_str())
        .collect();
    OracleTool::ALL
        .iter()
        .copied()
        .filter(|t| t.batch_capable())
        .filter(|&t| ToolManifest::for_tool(t).languages.iter().any(|l| detected.contains(l)))
        .collect()
}

fn probe_oracle_tools(tools: Vec<OracleTool>, log_tx: Sender<String>) -> CheckResult {
    if tools.is_empty() {
        send_log(&log_tx, "No detected languages need Oracle tools.");
        return CheckResult::ok();
    }

    send_log(&log_tx, format!("Checking {} Oracle tool(s)...", tools.len()));
    for tool in tools {
        let manifest = ToolManifest::for_tool(tool);
        send_log(&log_tx, format!("Checking {} ({})...", tool.as_db_str(), manifest.program));
        match probe_oracle_tool(tool) {
            ToolAvailability::Available { program, version, .. } => {
                send_log(&log_tx, format!("available: {program} {version}"));
            },
            ToolAvailability::Blocked { program, hint, .. } => {
                send_log(&log_tx, format!("blocked: {program}"));
                send_log(&log_tx, hint.clone());
                return CheckResult::warn(hint);
            },
        }
    }
    send_log(&log_tx, "All relevant Oracle tools are available.");
    CheckResult::ok()
}

/// Forward a human-readable progress line to the shared provision/test log.
pub(super) fn send_log(log_tx: &Sender<String>, line: impl Into<String>) {
    let _ = log_tx.send(line.into());
}
