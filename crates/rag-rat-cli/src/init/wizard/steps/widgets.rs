//! Small shared step widgets — a single-select option list and a help/info paragraph — used by the
//! Papertrail and Distill steps, whose mode-gated forms share the same look.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::super::theme;

/// A bordered single-select list: `*` marks the selected row; when the zone is focused the selected
/// row is highlighted and the border brightens. `options` carries a per-row base style so callers
/// can accent the relevant/detected row (the Oracle tool-list convention).
pub(super) fn option_list(
    f: &mut Frame,
    area: Rect,
    title: &str,
    options: &[(String, Style)],
    selected: usize,
    focused: bool,
) {
    let items: Vec<ListItem> = options
        .iter()
        .enumerate()
        .map(|(i, (label, base))| {
            let marker = if i == selected { "*" } else { " " };
            let style = if i == selected && focused {
                theme::selected()
            } else if i == selected {
                theme::accent()
            } else {
                *base
            };
            ListItem::new(Line::from(Span::styled(format!(" {marker} {label}"), style)))
        })
        .collect();
    f.render_widget(List::new(items).block(theme::focused_block(title, focused)), area);
}

/// A wrapped help/info paragraph inside a titled block.
pub(super) fn help_panel(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'_>>) {
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::base())
            .wrap(Wrap { trim: false })
            .block(theme::block(title)),
        area,
    );
}
