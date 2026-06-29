//! Darcula-style palette and TUI style helpers for the init wizard.

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Block;

const BACKGROUND: Color = Color::Rgb(40, 42, 54);
const TEXT: Color = Color::Rgb(187, 187, 187);
const TEXT_STRONG: Color = Color::Rgb(214, 214, 214);
const MUTED: Color = Color::Rgb(128, 128, 128);
const BORDER: Color = Color::Rgb(158, 159, 166);
const ACCENT: Color = Color::Rgb(104, 151, 187);
const FOCUSED_BORDER: Color = Color::Rgb(255, 184, 108);
const SELECTION: Color = Color::Rgb(68, 71, 90);
const WARNING: Color = Color::Rgb(255, 198, 109);
const ERROR: Color = Color::Rgb(204, 102, 102);
const SUCCESS: Color = Color::Rgb(106, 135, 89);
const FOOTER_BG: Color = Color::Rgb(78, 82, 84);

pub(crate) fn base() -> Style {
    Style::default().fg(TEXT).bg(BACKGROUND)
}

pub(crate) fn popup() -> Style {
    Style::default().fg(TEXT).bg(BACKGROUND)
}

pub(crate) fn title() -> Style {
    Style::default().fg(TEXT_STRONG).add_modifier(Modifier::BOLD)
}

pub(crate) fn heading() -> Style {
    title().add_modifier(Modifier::UNDERLINED)
}

pub(crate) fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub(crate) fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub(crate) fn focus() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

pub(crate) fn selected() -> Style {
    Style::default().fg(TEXT_STRONG).bg(SELECTION).add_modifier(Modifier::BOLD)
}

pub(crate) fn border() -> Style {
    Style::default().fg(BORDER).bg(BACKGROUND)
}

pub(crate) fn focused_border() -> Style {
    Style::default().fg(FOCUSED_BORDER).bg(BACKGROUND)
}

pub(crate) fn popup_border() -> Style {
    Style::default().fg(FOCUSED_BORDER).bg(BACKGROUND)
}

pub(crate) fn warning() -> Style {
    Style::default().fg(WARNING)
}

pub(crate) fn error() -> Style {
    Style::default().fg(ERROR)
}

pub(crate) fn success() -> Style {
    Style::default().fg(SUCCESS)
}

pub(crate) fn footer() -> Style {
    Style::default().fg(TEXT_STRONG).bg(FOOTER_BG)
}

pub(crate) fn footer_warning() -> Style {
    Style::default().fg(WARNING).bg(FOOTER_BG)
}

pub(crate) fn block(title: impl Into<String>) -> Block<'static> {
    Block::bordered().title(title.into()).style(base()).border_style(border())
}

pub(crate) fn focused_block(title: impl Into<String>, focused: bool) -> Block<'static> {
    block(title).border_style(if focused { focused_border() } else { border() })
}

pub(crate) fn popup_block(title: impl Into<String>) -> Block<'static> {
    Block::bordered().title(title.into()).style(popup()).border_style(popup_border())
}
