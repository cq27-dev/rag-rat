//! Indexing step — language toggles + per-language directory tree.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rag_rat_core::language::Language;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation};
use tui_tree_widget::{Tree, TreeItem, TreeState};

use super::super::state::WizardState;
use super::super::theme;
use super::types::{CheckResult, IndexZone, Outcome, StepState};
use crate::init::DirCandidate;
use crate::init::scan::candidate_dirs;

fn checked_langs(state: &WizardState) -> Vec<Language> {
    match &state.step {
        Some(StepState::Indexing { lang_toggles, .. }) =>
            lang_toggles.iter().filter(|(_, on)| *on).map(|(l, _)| *l).collect(),
        _ => vec![],
    }
}

fn active_tree_lang(state: &WizardState) -> Option<Language> {
    let Some(StepState::Indexing { lang_toggles, lang_focus, .. }) = &state.step else {
        return None;
    };
    lang_toggles
        .get(*lang_focus)
        .and_then(|(l, on)| on.then_some(*l))
        .or_else(|| lang_toggles.iter().find_map(|(l, on)| on.then_some(*l)))
}

pub(super) fn base_candidates_for(
    scan: &crate::init::RepoScan,
    lang: Language,
) -> Vec<DirCandidate> {
    let cs = candidate_dirs(scan, lang);
    if cs.is_empty() {
        vec![DirCandidate { path: PathBuf::from("."), count: 0, default: true }]
    } else {
        cs
    }
}

fn cached_base_candidates_for(lang: Language, state: &WizardState) -> Vec<DirCandidate> {
    match &state.step {
        Some(StepState::Indexing { dir_candidates, .. }) => dir_candidates
            .get(&lang)
            .cloned()
            .unwrap_or_else(|| base_candidates_for(&state.scan, lang)),
        _ => base_candidates_for(&state.scan, lang),
    }
}

fn sync_languages(state: &mut WizardState) {
    let checked = checked_langs(state);
    state.draft.bindings.retain(|l, _| checked.contains(l));
    for &l in &checked {
        if !state.draft.bindings.contains_key(&l) {
            let dirs: Vec<PathBuf> = cached_base_candidates_for(l, state)
                .into_iter()
                .filter(|c| c.default)
                .map(|c| c.path)
                .collect();
            state
                .draft
                .bindings
                .insert(l, if dirs.is_empty() { vec![PathBuf::from(".")] } else { dirs });
        }
    }
}

pub(super) fn render_indexing(f: &mut Frame, area: Rect, state: &mut WizardState) {
    let (lang_toggles, lang_focus, zone) = match &state.step {
        Some(StepState::Indexing { lang_toggles, lang_focus, zone, .. }) =>
            (lang_toggles.clone(), *lang_focus, *zone),
        _ => return,
    };
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);

    // Language toggles
    let mut spans = Vec::new();
    for (i, (l, on)) in lang_toggles.iter().enumerate() {
        let style = if zone == IndexZone::Toggles && i == lang_focus {
            theme::selected()
        } else {
            theme::base()
        };
        spans.push(Span::styled(
            format!("{}{} {}  ", if *on { "[x]" } else { "[ ]" }, l.as_str(), ""),
            style,
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(theme::base()).block(theme::focused_block(
            "Languages ←→ move  Space toggle",
            zone == IndexZone::Toggles,
        )),
        chunks[0],
    );

    if let Some(lang) = active_tree_lang(state) {
        render_dir_tree(f, chunks[1], lang, state, zone == IndexZone::Tree);
    } else {
        f.render_widget(
            Paragraph::new("Select at least one language to choose indexed directories.")
                .style(theme::base())
                .block(theme::block("Directories")),
            chunks[1],
        );
    }
}

pub(super) fn candidates_for(lang: Language, state: &WizardState) -> Vec<DirCandidate> {
    let cs = cached_base_candidates_for(lang, state);
    let draft = state.draft.bindings.get(&lang);
    match (cs.is_empty(), draft) {
        (true, None) => vec![DirCandidate { path: PathBuf::from("."), count: 0, default: true }],
        (true, Some(paths)) => paths
            .iter()
            .map(|p| DirCandidate { path: p.clone(), count: 0, default: true })
            .collect(),
        (false, None) => cs,
        (false, Some(paths)) =>
            cs.into_iter().map(|c| DirCandidate { default: paths.contains(&c.path), ..c }).collect(),
    }
}

#[derive(Clone, Copy, Default)]
struct DirMeta {
    count: usize,
    selected: bool,
}

fn dir_tree_items_for(lang: Language, state: &WizardState) -> Vec<TreeItem<'static, PathBuf>> {
    let candidates = candidates_for(lang, state);
    let mut metas = BTreeMap::<PathBuf, DirMeta>::new();
    let root_selected =
        state.draft.bindings.get(&lang).is_some_and(|v| v.contains(&PathBuf::from(".")));
    metas.insert(PathBuf::from("."), DirMeta { count: 0, selected: root_selected });

    for candidate in candidates {
        insert_path_and_ancestors(&mut metas, &candidate.path);
        metas.insert(candidate.path, DirMeta {
            count: candidate.count,
            selected: candidate.default,
        });
    }

    vec![dir_tree_item(Path::new("."), &metas)]
}

fn insert_path_and_ancestors(metas: &mut BTreeMap<PathBuf, DirMeta>, path: &Path) {
    metas.entry(PathBuf::from(".")).or_default();
    if path == Path::new(".") {
        return;
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if component.as_os_str() == "." {
            continue;
        }
        current.push(component.as_os_str());
        metas.entry(current.clone()).or_default();
    }
}

fn dir_tree_item(path: &Path, metas: &BTreeMap<PathBuf, DirMeta>) -> TreeItem<'static, PathBuf> {
    let children = child_dir_paths(path, metas)
        .into_iter()
        .map(|child| dir_tree_item(&child, metas))
        .collect::<Vec<_>>();
    let text = dir_tree_label(path, metas.get(path).copied().unwrap_or_default());
    if children.is_empty() {
        TreeItem::new_leaf(path.to_path_buf(), text)
    } else {
        TreeItem::new(path.to_path_buf(), text, children).expect("directory tree paths are unique")
    }
}

fn child_dir_paths(parent: &Path, metas: &BTreeMap<PathBuf, DirMeta>) -> Vec<PathBuf> {
    metas
        .keys()
        .filter(|path| path.as_path() != parent)
        .filter(|path| parent_dir_path(path).as_deref() == Some(parent))
        .cloned()
        .collect()
}

fn parent_dir_path(path: &Path) -> Option<PathBuf> {
    if path == Path::new(".") {
        return None;
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    Some(parent.to_path_buf())
}

fn dir_tree_label(path: &Path, meta: DirMeta) -> String {
    let selected = if meta.selected { "[x]" } else { "[ ]" };
    let name = if path == Path::new(".") {
        ".".to_string()
    } else {
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string())
    };
    if meta.count == 0 {
        format!("{selected} {name}")
    } else {
        format!("{selected} {name} ({})", meta.count)
    }
}

fn render_dir_tree(
    f: &mut Frame,
    area: Rect,
    lang: Language,
    state: &mut WizardState,
    focused: bool,
) {
    let items = dir_tree_items_for(lang, state);
    let Some(StepState::Indexing { tree, .. }) = &mut state.step else { return };
    open_dir_tree_nodes(tree, &items);
    ensure_dir_tree_selection(tree, &items);
    let visible = tree.flatten(&items);
    let selected =
        visible.iter().position(|item| item.identifier == tree.selected()).map_or(0, |i| i + 1);
    let title = format!("Directories: {}  {}/{}", lang.as_str(), selected, visible.len());
    let widget = Tree::new(&items)
        .expect("directory tree paths are unique")
        .style(theme::base())
        .block(theme::focused_block(title, focused))
        .experimental_scrollbar(Some(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .track_symbol(None)
                .end_symbol(None),
        ))
        .highlight_style(theme::selected())
        .highlight_symbol("> ")
        .node_open_symbol("▾ ")
        .node_closed_symbol("▸ ")
        .node_no_children_symbol("  ");
    f.render_stateful_widget(widget, area, tree);
}

fn open_dir_tree_nodes(tree: &mut TreeState<PathBuf>, items: &[TreeItem<'static, PathBuf>]) {
    fn rec(
        tree: &mut TreeState<PathBuf>,
        items: &[TreeItem<'static, PathBuf>],
        current: &mut Vec<PathBuf>,
    ) {
        for item in items {
            current.push(item.identifier().clone());
            if !item.children().is_empty() {
                tree.open(current.clone());
                rec(tree, item.children(), current);
            }
            current.pop();
        }
    }
    rec(tree, items, &mut Vec::new());
}

fn ensure_dir_tree_selection(tree: &mut TreeState<PathBuf>, items: &[TreeItem<'static, PathBuf>]) {
    let visible = tree.flatten(items);
    if visible.is_empty() {
        tree.select(Vec::new());
        return;
    }
    if !visible.iter().any(|item| item.identifier == tree.selected()) {
        tree.select(visible[0].identifier.clone());
    }
}

pub(super) fn handle_indexing(key: KeyEvent, state: &mut WizardState) -> Outcome {
    let zone = match &state.step {
        Some(StepState::Indexing { zone, .. }) => *zone,
        _ => return Outcome::Pass,
    };

    match zone {
        IndexZone::Toggles => match key.code {
            KeyCode::Left => {
                move_language_focus(state, -1);
                Outcome::Consumed
            },
            KeyCode::Right => {
                move_language_focus(state, 1);
                Outcome::Consumed
            },
            KeyCode::Char(' ') => {
                if let Some(StepState::Indexing { lang_toggles, lang_focus, .. }) = &mut state.step
                {
                    lang_toggles[*lang_focus].1 = !lang_toggles[*lang_focus].1;
                }
                sync_languages(state);
                ensure_active_dir_tree_selection(state);
                Outcome::Consumed
            },
            KeyCode::Tab => {
                focus_tree_zone(state);
                Outcome::Consumed
            },
            KeyCode::BackTab => {
                focus_tree_zone(state);
                Outcome::Consumed
            },
            KeyCode::Enter => Outcome::Advance,
            KeyCode::Esc => Outcome::Back,
            KeyCode::Char('a') => {
                select_all(state);
                Outcome::Consumed
            },
            KeyCode::Char('A') => {
                deselect_all(state);
                Outcome::Consumed
            },
            _ => Outcome::Pass,
        },
        IndexZone::Tree => match key.code {
            KeyCode::Tab => {
                focus_toggle_zone(state);
                Outcome::Consumed
            },
            KeyCode::BackTab => {
                focus_toggle_zone(state);
                Outcome::Consumed
            },
            KeyCode::Enter => Outcome::Advance,
            KeyCode::Esc => Outcome::Back,
            KeyCode::Char('a') => {
                select_all(state);
                Outcome::Consumed
            },
            KeyCode::Char('A') => {
                deselect_all(state);
                Outcome::Consumed
            },
            KeyCode::Left => {
                fold_dir_tree(state);
                Outcome::Consumed
            },
            KeyCode::Right => {
                unfold_dir_tree(state);
                Outcome::Consumed
            },
            KeyCode::Up | KeyCode::Char('k') => {
                move_dir_selection(state, -1);
                Outcome::Consumed
            },
            KeyCode::Down | KeyCode::Char('j') => {
                move_dir_selection(state, 1);
                Outcome::Consumed
            },
            KeyCode::PageUp => {
                move_dir_selection(state, -10);
                Outcome::Consumed
            },
            KeyCode::PageDown => {
                move_dir_selection(state, 10);
                Outcome::Consumed
            },
            KeyCode::Home => {
                move_dir_selection_to_edge(state, false);
                Outcome::Consumed
            },
            KeyCode::End => {
                move_dir_selection_to_edge(state, true);
                Outcome::Consumed
            },
            KeyCode::Char(' ') => toggle_focused_dir(state),
            _ => Outcome::Pass,
        },
    }
}

fn move_language_focus(state: &mut WizardState, delta: isize) {
    if let Some(StepState::Indexing { lang_toggles, lang_focus, tree, .. }) = &mut state.step {
        let last = lang_toggles.len().saturating_sub(1);
        *lang_focus = (*lang_focus as isize).saturating_add(delta).clamp(0, last as isize) as usize;
        tree.select(vec![PathBuf::from(".")]);
    }
    ensure_active_dir_tree_selection(state);
}

pub(super) fn focus_tree_zone(state: &mut WizardState) {
    if let Some(StepState::Indexing { lang_toggles, lang_focus, zone, tree, .. }) = &mut state.step
    {
        if lang_toggles.is_empty() || !lang_toggles.iter().any(|(_, on)| *on) {
            return;
        }
        let already_tree = *zone == IndexZone::Tree;
        if !lang_toggles.get(*lang_focus).is_some_and(|(_, on)| *on)
            && let Some(i) = lang_toggles.iter().position(|(_, on)| *on)
        {
            *lang_focus = i;
        }
        *zone = IndexZone::Tree;
        if !already_tree {
            tree.select(vec![PathBuf::from(".")]);
        }
    }
    ensure_active_dir_tree_selection(state);
}

fn focus_toggle_zone(state: &mut WizardState) {
    if let Some(StepState::Indexing { zone, .. }) = &mut state.step {
        *zone = IndexZone::Toggles;
    }
}

fn ensure_active_dir_tree_selection(state: &mut WizardState) {
    let Some(lang) = active_tree_lang(state) else { return };
    let items = dir_tree_items_for(lang, state);
    let Some(StepState::Indexing { tree, .. }) = &mut state.step else { return };
    open_dir_tree_nodes(tree, &items);
    ensure_dir_tree_selection(tree, &items);
}

fn move_dir_selection(state: &mut WizardState, delta: isize) {
    let Some(lang) = active_tree_lang(state) else { return };
    let items = dir_tree_items_for(lang, state);
    let Some(StepState::Indexing { tree, .. }) = &mut state.step else { return };
    open_dir_tree_nodes(tree, &items);
    let visible = tree.flatten(&items);
    if visible.is_empty() {
        tree.select(Vec::new());
        return;
    }
    let current = visible.iter().position(|item| item.identifier == tree.selected()).unwrap_or(0);
    let last = visible.len().saturating_sub(1);
    let next = (current as isize).saturating_add(delta).clamp(0, last as isize) as usize;
    tree.select(visible[next].identifier.clone());
}

fn move_dir_selection_to_edge(state: &mut WizardState, end: bool) {
    let Some(lang) = active_tree_lang(state) else { return };
    let items = dir_tree_items_for(lang, state);
    let Some(StepState::Indexing { tree, .. }) = &mut state.step else { return };
    open_dir_tree_nodes(tree, &items);
    let visible = tree.flatten(&items);
    let Some(item) = (if end { visible.last() } else { visible.first() }) else { return };
    tree.select(item.identifier.clone());
}

fn fold_dir_tree(state: &mut WizardState) {
    let Some(StepState::Indexing { tree, .. }) = &mut state.step else { return };
    tree.key_left();
}

fn unfold_dir_tree(state: &mut WizardState) {
    let Some(StepState::Indexing { tree, .. }) = &mut state.step else { return };
    tree.key_right();
}

pub(super) fn scroll_dir_tree(state: &mut WizardState, delta: isize) {
    let Some(StepState::Indexing { tree, .. }) = &mut state.step else { return };
    let lines = delta.unsigned_abs();
    if delta < 0 {
        tree.scroll_up(lines);
    } else {
        tree.scroll_down(lines);
    }
}

pub(super) fn selected_dir_path(state: &WizardState) -> Option<PathBuf> {
    match &state.step {
        Some(StepState::Indexing { tree, .. }) => tree.selected().last().cloned(),
        _ => None,
    }
}

fn toggle_focused_dir(state: &mut WizardState) -> Outcome {
    let Some(lang) = active_tree_lang(state) else { return Outcome::Pass };
    let Some(path) = selected_dir_path(state) else { return Outcome::Pass };
    let paths = state.draft.bindings.entry(lang).or_default();
    if paths.contains(&path) {
        paths.retain(|p| p != &path);
    } else {
        paths.push(path);
    }
    Outcome::Consumed
}

fn select_all(state: &mut WizardState) {
    for lang in checked_langs(state) {
        let cs = candidates_for(lang, state);
        state.draft.bindings.insert(lang, cs.into_iter().map(|c| c.path).collect());
    }
}

fn deselect_all(state: &mut WizardState) {
    for lang in checked_langs(state) {
        state.draft.bindings.insert(lang, vec![]);
    }
}

pub(super) fn validate_indexing(state: &WizardState) -> CheckResult {
    if !state.draft.root_abs.exists() {
        return CheckResult::block(format!("root not found: {}", state.draft.root_abs.display()));
    }
    let conflicts = state.draft.conflicting_rich_target_names();
    if !conflicts.is_empty() {
        return CheckResult::block(format!(
            "simple bindings conflict with preserved [[target]] names: {}",
            conflicts.join(", ")
        ));
    }
    if !state.draft.has_rich_targets && !has_effective_simple_bindings(state) {
        return CheckResult::block("select at least one indexed directory");
    }
    let mut missing = Vec::new();
    for (l, dirs) in &state.draft.bindings {
        for d in dirs {
            let abs = if d.is_absolute() { d.clone() } else { state.draft.root_abs.join(d) };
            if !abs.exists() {
                missing.push(format!("{}: {}", l.as_str(), d.display()));
            }
        }
    }
    if !missing.is_empty() {
        CheckResult::warn(format!("not found: {}", missing.join(", ")))
    } else {
        CheckResult::ok()
    }
}

fn has_effective_simple_bindings(state: &WizardState) -> bool {
    state.draft.bindings.values().any(|dirs| !dirs.is_empty())
}
