//! Step functions — render, handle_key, validate.  No traits, no widget abstractions.
//!
//! Per-step mutable state lives in [`StepState`], stored on [`WizardState`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;

use rag_rat_core::config::{
    DEFAULT_QUERY_ENDPOINT, MAX_REMOTE_EMBEDDING_CONCURRENCY, RemoteEmbeddingConfig,
};
use rag_rat_core::embedding_models::{Backend, EMBEDDING_MODELS, EmbeddingModelSpec, spec};
#[cfg(feature = "fastembed")]
use rag_rat_core::index::ai::FastEmbedEmbedder;
#[cfg(feature = "model2vec")]
use rag_rat_core::index::ai::Model2VecEmbedder;
use rag_rat_core::index::ai::{
    Embedder, HashEmbedder, OpenAiEmbedder, verify_ephemeral_remote_cancellable,
};
use rag_rat_core::index::oracle::{OracleTool, ToolAvailability, ToolManifest, probe_oracle_tool};
use rag_rat_core::language::Language;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, Wrap};
use tui_tree_widget::{Tree, TreeItem, TreeState};

use super::catalog::CookbookEntry;
use super::draft::{
    OLLAMA_EMBEDDING_MODELS, RemoteDraft, RemoteMode, ollama_model_dim, ollama_model_for,
};
use super::probe::{ProbeKind, ProbeStatus};
use super::state::{PROVISION_CONFIRM_WORD, WizardState, provision_confirm_satisfied};
use super::theme;
use crate::init::DirCandidate;
use crate::init::scan::candidate_dirs;
use crate::{MANAGED_HOOKS, git_paths, is_rag_rat_hook};

// ── public types ───────────────────────────────────────────────────────────────

pub(crate) mod hooks {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[allow(dead_code)] // The installer already supports these choices; the wizard conflict UI lands separately.
    pub(crate) enum HookConflict {
        Skip,
        Overwrite,
        Chain,
        UninstallRagRatOnly,
        Abort,
    }
    pub(crate) fn render_chained_hook(original: &str, hook_name: &str) -> String {
        let body = original.trim_start();
        let inner = if body.starts_with("#!") {
            body.split_once('\n').map(|x| x.1).unwrap_or("").trim_start_matches('\n')
        } else {
            body
        };
        let rag = format!(
            "repo_root=\"$(git rev-parse --show-toplevel 2>/dev/null)\" || exit 0\ncd \
             \"$repo_root\" || exit 0\nunset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE \
             GIT_PREFIX GIT_NAMESPACE GIT_OBJECT_DIRECTORY \
             GIT_ALTERNATE_OBJECT_DIRECTORIES\nRAG_RAT_HOOK_DISABLE=1 rag-rat maintenance \
             --trigger {hook_name} --max-seconds 30 \
             >\"${{TMPDIR:-/tmp}}/rag-rat-{hook_name}.log\" 2>&1 &"
        );
        format!("#!/bin/sh\n# Chained hook\n\n{inner}\n\n{rag}\n\nexit 0\n")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StepId {
    Indexing    = 0,
    Oracle      = 1,
    Embedding   = 2,
    Integration = 3,
}
impl StepId {
    pub const COUNT: usize = 4;
    pub const ALL: [StepId; 4] = [Self::Indexing, Self::Oracle, Self::Embedding, Self::Integration];
    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Outcome {
    Consumed,
    Pass,
    Advance,
    Back,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sev {
    Ok,
    Warn,
    Block,
}

#[derive(Clone, Debug)]
pub(crate) struct CheckResult {
    pub severity: Sev,
    pub message: Option<String>,
}
impl CheckResult {
    pub fn ok() -> Self {
        Self { severity: Sev::Ok, message: None }
    }
    pub fn warn(m: impl Into<String>) -> Self {
        Self { severity: Sev::Warn, message: Some(m.into()) }
    }
    pub fn block(m: impl Into<String>) -> Self {
        Self { severity: Sev::Block, message: Some(m.into()) }
    }
}

pub(crate) fn can_write(checks: &[CheckResult]) -> bool {
    checks.iter().all(|c| c.severity != Sev::Block)
}

const ONE_LINE_FIELD_OUTER_HEIGHT: u16 = 3;
const REMOTE_BATCH_SIZE_MAX: u32 = 4096;

// ── per-step state ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EmbedFocus {
    Model,
    Mode,
    Endpoint,
    Cookbook,
    ServerModel,
    Gpu,
    BatchSize,
    Concurrency,
    MaxBatchChars,
    AuthEnv,
    ProvisionConfirm,
}

pub(crate) enum StepState {
    Indexing {
        lang_toggles: Vec<(Language, bool)>,
        lang_focus: usize,
        zone: IndexZone,
        tree: TreeState<PathBuf>,
        dir_candidates: BTreeMap<Language, Vec<DirCandidate>>,
    },
    Embedding {
        model_cursor: usize,
        mode_cursor: usize,
        cookbook_cursor: usize,
        server_model_cursor: usize,
        gpu_cursor: usize,
        model_scroll: usize,
        server_model_scroll: usize,
        focus: EmbedFocus,
    },
    Oracle,
    Integration {
        focus: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexZone {
    Toggles,
    Tree,
}

// ── step titles / footers ──────────────────────────────────────────────────────

pub(crate) fn step_title(id: StepId) -> &'static str {
    match id {
        StepId::Indexing => "Indexing",
        StepId::Oracle => "Oracle",
        StepId::Embedding => "Embedding",
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
        StepId::Integration =>
            "↑↓ navigate  Space toggle  t version check  g git  c claude  Enter next",
    }
}

// ── step constructors + state init ─────────────────────────────────────────────

fn step_matches(id: StepId, step: &Option<StepState>) -> bool {
    matches!(
        (id, step),
        (StepId::Indexing, Some(StepState::Indexing { .. }))
            | (StepId::Oracle, Some(StepState::Oracle))
            | (StepId::Embedding, Some(StepState::Embedding { .. }))
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
        StepId::Integration => StepState::Integration { focus: 0 },
    }
}

fn init_embedding_step(state: &WizardState) -> StepState {
    let rows = model_rows();
    let model_cursor = rows.iter().position(|(id, _)| id == &state.draft.model).unwrap_or(0);
    let mode_cursor = remote_mode(state);
    let cookbook_cursor = selected_cookbook_idx(state).unwrap_or(0);
    let server_model = state
        .draft
        .remote
        .as_ref()
        .map(|r| r.model.as_str())
        .unwrap_or_else(|| default_remote_model_for(&state.draft.model));
    let server_models = compatible_server_models(&state.draft.model);
    let server_model_cursor = server_models.iter().position(|&m| m == server_model).unwrap_or(0);
    let gpu_options = current_gpu_options(state);
    let gpu_cursor = state
        .draft
        .remote
        .as_ref()
        .and_then(|r| r.gpu.as_deref())
        .and_then(|gpu| gpu_options.iter().position(|g| g == gpu))
        .unwrap_or(0);
    StepState::Embedding {
        model_cursor,
        mode_cursor,
        cookbook_cursor,
        server_model_cursor,
        gpu_cursor,
        model_scroll: model_cursor.saturating_sub(4),
        server_model_scroll: server_model_cursor.saturating_sub(4),
        focus: EmbedFocus::Model,
    }
}

fn compatible_server_models(local_model: &str) -> Vec<&'static str> {
    let Some(local) = spec(local_model) else {
        return Vec::new();
    };
    if local.backend != Backend::FastEmbed {
        return Vec::new();
    };
    OLLAMA_EMBEDDING_MODELS
        .iter()
        .copied()
        .filter(|model| ollama_model_dim(model).is_none_or(|dim| dim == local.dim))
        .collect()
}

// ── dispatch ───────────────────────────────────────────────────────────────────

pub(crate) fn render_step(id: StepId, f: &mut Frame, area: Rect, state: &mut WizardState) {
    match id {
        StepId::Indexing => render_indexing(f, area, state),
        StepId::Oracle => render_oracle(f, area, state),
        StepId::Embedding => render_embedding(f, area, state),
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
        StepId::Integration => handle_integration(key, state),
    }
}

pub(crate) fn validate_step(id: StepId, state: &WizardState) -> CheckResult {
    match id {
        StepId::Indexing => validate_indexing(state),
        StepId::Oracle => CheckResult::ok(),
        StepId::Embedding => validate_embedding(state),
        StepId::Integration => validate_hooks(state),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// INDEXING
// ═══════════════════════════════════════════════════════════════════════════════

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

fn base_candidates_for(scan: &crate::init::RepoScan, lang: Language) -> Vec<DirCandidate> {
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

fn render_indexing(f: &mut Frame, area: Rect, state: &mut WizardState) {
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

fn candidates_for(lang: Language, state: &WizardState) -> Vec<DirCandidate> {
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

fn handle_indexing(key: KeyEvent, state: &mut WizardState) -> Outcome {
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

fn focus_tree_zone(state: &mut WizardState) {
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

fn scroll_dir_tree(state: &mut WizardState, delta: isize) {
    let Some(StepState::Indexing { tree, .. }) = &mut state.step else { return };
    let lines = delta.unsigned_abs();
    if delta < 0 {
        tree.scroll_up(lines);
    } else {
        tree.scroll_down(lines);
    }
}

fn selected_dir_path(state: &WizardState) -> Option<PathBuf> {
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

fn validate_indexing(state: &WizardState) -> CheckResult {
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

// ═══════════════════════════════════════════════════════════════════════════════
// ORACLE
// ═══════════════════════════════════════════════════════════════════════════════

fn render_oracle(f: &mut Frame, area: Rect, state: &WizardState) {
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

fn handle_oracle(key: KeyEvent, state: &mut WizardState) -> Outcome {
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

fn send_log(log_tx: &Sender<String>, line: impl Into<String>) {
    let _ = log_tx.send(line.into());
}

// ═══════════════════════════════════════════════════════════════════════════════
// EMBEDDING
// ═══════════════════════════════════════════════════════════════════════════════

const NONE_MODEL: &str = "none";

fn model_rows() -> Vec<(String, String)> {
    EMBEDDING_MODELS
        .iter()
        .map(|s| {
            (s.model_id.to_string(), format!("{} ({}, {}d)", s.display, s.backend.runtime(), s.dim))
        })
        .chain(std::iter::once((
            NONE_MODEL.to_string(),
            "none — BM25 + structure only".to_string(),
        )))
        .collect()
}

fn default_remote_model_for(local_model: &str) -> &'static str {
    ollama_model_for(local_model).unwrap_or("all-minilm")
}

fn remote_mode(state: &WizardState) -> usize {
    match state.draft.remote.as_ref().map(|r| &r.mode) {
        Some(RemoteMode::Connect(_)) => 1,
        Some(RemoteMode::Ephemeral(_)) => 2,
        None => 0,
    }
}

fn current_ephemeral_cookbook(state: &WizardState) -> Option<&str> {
    match state.draft.remote.as_ref().map(|r| &r.mode) {
        Some(RemoteMode::Ephemeral(cookbook)) => Some(cookbook.trim()),
        _ => None,
    }
}

fn cookbook_choices(state: &WizardState) -> Vec<CookbookEntry> {
    let mut choices = state.cookbooks.entries().to_vec();
    if let Some(cookbook) = current_ephemeral_cookbook(state)
        && state.cookbooks.find_command(cookbook).is_none()
    {
        choices.push(CookbookEntry::custom_current(
            cookbook,
            state.draft.remote.as_ref().and_then(|remote| remote.gpu.as_deref()),
        ));
    }
    choices
}

fn selected_cookbook_idx(state: &WizardState) -> Option<usize> {
    let cookbook = current_ephemeral_cookbook(state)?;
    cookbook_choices(state).iter().position(|entry| entry.command == cookbook)
}

fn current_gpu_options(state: &WizardState) -> Vec<String> {
    let choices = cookbook_choices(state);
    let selected = selected_cookbook_idx(state).unwrap_or(0);
    choices.get(selected).map(|entry| entry.gpus.clone()).unwrap_or_default()
}

fn default_cookbook_command(state: &WizardState) -> String {
    state
        .cookbooks
        .entries()
        .first()
        .map(|entry| entry.command.clone())
        .unwrap_or_else(|| "@rag-rat/cookbook modal".to_string())
}

fn render_embedding(f: &mut Frame, area: Rect, state: &WizardState) {
    let Some(StepState::Embedding {
        model_cursor,
        mode_cursor,
        cookbook_cursor,
        server_model_cursor,
        gpu_cursor,
        model_scroll,
        server_model_scroll,
        focus,
    }) = &state.step
    else {
        return;
    };
    let model_none = state.draft.model == NONE_MODEL || state.draft.model.is_empty();
    let rows = model_rows();

    let cols =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area);
    let left = Layout::vertical([Constraint::Min(7), Constraint::Length(7)]).split(cols[0]);
    render_model_list(
        f,
        left[0],
        state,
        &rows,
        *model_cursor,
        *model_scroll,
        *focus == EmbedFocus::Model,
    );
    render_model_help(f, left[1], rows.get(*model_cursor).map(|(id, _)| id.as_str()));

    let right = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Min(5),
        Constraint::Length(ONE_LINE_FIELD_OUTER_HEIGHT),
    ])
    .split(cols[1]);

    let modes = ["none", "connect", "ephemeral"];
    let mode_items: Vec<ListItem> = modes
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let selected = if i == remote_mode(state) { "*" } else { " " };
            let cursor = if i == *mode_cursor { ">" } else { " " };
            let style = if i == *mode_cursor { theme::selected() } else { theme::base() };
            ListItem::new(format!("{cursor} [{selected}] {m}")).style(style)
        })
        .collect();
    let focused = *focus == EmbedFocus::Mode;
    f.render_widget(
        List::new(mode_items)
            .style(theme::base())
            .block(theme::focused_block("Remote mode", focused)),
        right[0],
    );

    let rmode = remote_mode(state);
    let remote = state.draft.remote.as_ref();
    let ep = match remote.map(|r| &r.mode) {
        Some(RemoteMode::Connect(u)) => u.as_str(),
        _ => "",
    };
    let m = remote.map_or("", |r| r.model.as_str());
    let g = remote.and_then(|r| r.gpu.as_deref()).unwrap_or("");
    let bs = remote.map_or(256, |r| r.batch_size).to_string();
    let concurrency = remote
        .map_or_else(|| RemoteEmbeddingConfig::default().concurrency, |r| r.concurrency)
        .to_string();
    let max_batch_chars = remote
        .map_or_else(|| RemoteEmbeddingConfig::default().max_batch_chars, |r| r.max_batch_chars)
        .to_string();
    let auth = remote.and_then(|r| r.auth_env.as_deref()).unwrap_or("");

    let dim = |f: EmbedFocus| if *focus == f { theme::focused_border() } else { theme::border() };

    if model_none {
        f.render_widget(
            Paragraph::new("Select an embedding model before configuring a remote.")
                .style(theme::base())
                .block(theme::block("Remote")),
            right[1],
        );
        return;
    }

    if rmode == 1 {
        let endpoint =
            Layout::vertical([Constraint::Length(ONE_LINE_FIELD_OUTER_HEIGHT), Constraint::Min(0)])
                .split(right[1]);
        f.render_widget(one_line_field(ep, "endpoint", dim(EmbedFocus::Endpoint)), endpoint[0]);
    } else if rmode == 2 {
        let fields =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(right[1]);
        let cookbook_entries = cookbook_choices(state);
        let selected_cookbook = selected_cookbook_idx(state);
        let cookbook_visible = usize::from(fields[0].height.saturating_sub(2)).max(1);
        let (cookbook_scroll, cookbook_end) =
            visible_list_bounds(cookbook_entries.len(), *cookbook_cursor, 0, cookbook_visible);
        let cookbook_items: Vec<ListItem> = cookbook_entries[cookbook_scroll..cookbook_end]
            .iter()
            .enumerate()
            .map(|(offset, entry)| {
                let i = cookbook_scroll + offset;
                let cursor = if i == *cookbook_cursor { ">" } else { " " };
                let selected = if Some(i) == selected_cookbook { "*" } else { " " };
                let style = if i == *cookbook_cursor { theme::selected() } else { theme::base() };
                ListItem::new(format!("{cursor} [{selected}] {}", entry.label)).style(style)
            })
            .collect();
        f.render_widget(
            List::new(cookbook_items)
                .style(theme::base())
                .block(theme::block("cookbook").border_style(dim(EmbedFocus::Cookbook))),
            fields[0],
        );
        let gpu_opts = current_gpu_options(state);
        let gpu_visible = usize::from(fields[1].height.saturating_sub(2)).max(1);
        let (gpu_scroll, gpu_end) =
            visible_list_bounds(gpu_opts.len(), *gpu_cursor, 0, gpu_visible);
        let gpu_items: Vec<ListItem> = gpu_opts[gpu_scroll..gpu_end]
            .iter()
            .enumerate()
            .map(|(offset, gpu)| {
                let i = gpu_scroll + offset;
                let cursor = if i == *gpu_cursor { ">" } else { " " };
                let selected = if gpu == g { "*" } else { " " };
                let style = if i == *gpu_cursor { theme::selected() } else { theme::base() };
                ListItem::new(format!("{cursor} [{selected}] {gpu}")).style(style)
            })
            .collect();
        f.render_widget(
            List::new(gpu_items)
                .style(theme::base())
                .block(theme::block("gpu").border_style(dim(EmbedFocus::Gpu))),
            fields[1],
        );
    } else {
        f.render_widget(
            Paragraph::new("Remote disabled. Select connect or ephemeral to configure Ollama.")
                .style(theme::base())
                .block(theme::block("Remote")),
            right[1],
        );
    }

    let server_models = compatible_server_models(&state.draft.model);
    render_server_model_list(
        f,
        right[2],
        &server_models,
        m,
        *server_model_cursor,
        *server_model_scroll,
        *focus == EmbedFocus::ServerModel,
    );

    if rmode == 2 {
        let bottom = Layout::horizontal([
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
            Constraint::Ratio(1, 5),
        ])
        .split(right[3]);
        f.render_widget(one_line_field(&bs, "batch", dim(EmbedFocus::BatchSize)), bottom[0]);
        f.render_widget(
            one_line_field(&concurrency, "parallel", dim(EmbedFocus::Concurrency)),
            bottom[1],
        );
        f.render_widget(
            one_line_field(&max_batch_chars, "chars", dim(EmbedFocus::MaxBatchChars)),
            bottom[2],
        );
        f.render_widget(one_line_field(auth, "auth env", dim(EmbedFocus::AuthEnv)), bottom[3]);
        let confirm = if provision_confirm_satisfied(&state.ui) {
            "ready"
        } else {
            state.ui.provision_confirm.as_str()
        };
        let confirm_title = if bottom[4].width >= (PROVISION_CONFIRM_WORD.len() as u16 + 8) {
            format!("type: {PROVISION_CONFIRM_WORD}")
        } else {
            PROVISION_CONFIRM_WORD.to_string()
        };
        f.render_widget(
            one_line_field(confirm, &confirm_title, dim(EmbedFocus::ProvisionConfirm)),
            bottom[4],
        );
    } else {
        let bottom = Layout::horizontal([
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
        ])
        .split(right[3]);
        f.render_widget(one_line_field(&bs, "batch", dim(EmbedFocus::BatchSize)), bottom[0]);
        f.render_widget(
            one_line_field(&concurrency, "parallel", dim(EmbedFocus::Concurrency)),
            bottom[1],
        );
        f.render_widget(
            one_line_field(&max_batch_chars, "max chars", dim(EmbedFocus::MaxBatchChars)),
            bottom[2],
        );
        f.render_widget(one_line_field(auth, "auth env", dim(EmbedFocus::AuthEnv)), bottom[3]);
    }
}

fn one_line_field<'a>(value: &'a str, title: &'a str, border: Style) -> Paragraph<'a> {
    Paragraph::new(Line::from(Span::raw(value)))
        .style(theme::base())
        .block(theme::block(title).border_style(border))
}

fn visible_list_bounds(len: usize, cursor: usize, scroll: usize, visible: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let visible = visible.max(1);
    let cursor = cursor.min(len - 1);
    let max_scroll = len.saturating_sub(visible);
    let mut scroll = scroll.min(max_scroll);
    if cursor < scroll {
        scroll = cursor;
    } else if cursor >= scroll.saturating_add(visible) {
        scroll = cursor.saturating_sub(visible - 1);
    }
    scroll = scroll.min(max_scroll);
    (scroll, (scroll + visible).min(len))
}

fn render_model_list(
    f: &mut Frame,
    area: Rect,
    state: &WizardState,
    rows: &[(String, String)],
    cursor: usize,
    scroll: usize,
    focused: bool,
) {
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let (scroll, end) = visible_list_bounds(rows.len(), cursor, scroll, visible);
    let items: Vec<ListItem> = rows[scroll..end]
        .iter()
        .enumerate()
        .map(|(offset, (id, label))| {
            let idx = scroll + offset;
            let cursor_marker = if idx == cursor { ">" } else { " " };
            let selected = if id == &state.draft.model { "*" } else { " " };
            let style = if idx == cursor { theme::selected() } else { theme::base() };
            ListItem::new(format!("{cursor_marker} [{selected}] {label}")).style(style)
        })
        .collect();
    f.render_widget(
        List::new(items).style(theme::base()).block(theme::focused_block("Model", focused)),
        area,
    );
}

fn render_model_help(f: &mut Frame, area: Rect, model_id: Option<&str>) {
    let text = model_help_lines(model_id);
    f.render_widget(
        Paragraph::new(text)
            .style(theme::base())
            .wrap(Wrap { trim: true })
            .block(theme::block("Model help")),
        area,
    );
}

fn model_help_lines(model_id: Option<&str>) -> Vec<Line<'static>> {
    let Some(model_id) = model_id else {
        return vec![Line::from(
            "Move through the model list to compare cost and retrieval tradeoffs.",
        )];
    };
    if model_id == NONE_MODEL {
        return vec![
            Line::from("No vector embeddings: lowest CPU, disk, and setup cost."),
            Line::from("Not recommended for big codebases or fuzzy natural-language queries."),
        ];
    }
    let Some(s) = spec(model_id) else {
        return vec![Line::from("Unknown model. Pick a registered model before continuing.")];
    };
    let weight = match s.dim {
        0..=384 => "light",
        385..=512 => "medium",
        _ => "heavy",
    };
    let guidance = match s.backend {
        Backend::Hash =>
            "Dependency-free fallback; very fast, but weak semantic recall. Not recommended for \
             big codebases.",
        Backend::FastEmbed if s.display.contains("MiniLM") =>
            "Default balanced choice. Good first pick for most repos; measured as hard to beat \
             here.",
        Backend::FastEmbed if s.display.contains("bge") =>
            "General-purpose 384d model. Similar weight to MiniLM; useful for comparison, not a \
             clear default win.",
        Backend::FastEmbed if s.display.contains("jina") =>
            "Code-focused 768d model. Heavier storage and reconcile cost; not a measured recall \
             win here.",
        Backend::FastEmbed =>
            "Local fastembed model. Good when you want local semantic search without a remote \
             server.",
        Backend::Model2Vec =>
            "Small and fast local model. Use when speed matters more than maximum semantic recall.",
        Backend::Ollama =>
            "Remote runtime. Use only with a configured Ollama endpoint or ephemeral provider.",
    };
    vec![
        Line::from(format!("{}: {weight}, {}d, {}.", s.display, s.dim, s.backend.runtime())),
        Line::from(guidance),
    ]
}

fn render_server_model_list(
    f: &mut Frame,
    area: Rect,
    models: &[&str],
    selected_model: &str,
    cursor: usize,
    scroll: usize,
    focused: bool,
) {
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let (scroll, end) = visible_list_bounds(models.len(), cursor, scroll, visible);
    let items: Vec<ListItem> = models[scroll..end]
        .iter()
        .enumerate()
        .map(|(offset, model)| {
            let idx = scroll + offset;
            let cursor_marker = if idx == cursor { ">" } else { " " };
            let selected = if *model == selected_model { "*" } else { " " };
            let style = if idx == cursor { theme::selected() } else { theme::base() };
            ListItem::new(format!("{cursor_marker} [{selected}] {model}")).style(style)
        })
        .collect();
    f.render_widget(
        List::new(items).style(theme::base()).block(theme::focused_block("server model", focused)),
        area,
    );
}

fn handle_embedding(key: KeyEvent, state: &mut WizardState) -> Outcome {
    match key.code {
        KeyCode::Tab => {
            cycle_embed_focus(state, 1);
            return Outcome::Consumed;
        },
        KeyCode::BackTab => {
            cycle_embed_focus(state, -1);
            return Outcome::Consumed;
        },
        _ => {},
    }
    if edit_embedding_field(key, state) {
        return Outcome::Consumed;
    }

    match key.code {
        KeyCode::Enter => {
            sync_embedding(state);
            Outcome::Advance
        },
        KeyCode::Esc => Outcome::Back,
        KeyCode::Up | KeyCode::Char('k') => {
            move_embedding_cursor(state, -1);
            Outcome::Consumed
        },
        KeyCode::Down | KeyCode::Char('j') => {
            move_embedding_cursor(state, 1);
            Outcome::Consumed
        },
        KeyCode::PageUp => {
            move_embedding_cursor(state, -10);
            Outcome::Consumed
        },
        KeyCode::PageDown => {
            move_embedding_cursor(state, 10);
            Outcome::Consumed
        },
        KeyCode::Home => {
            move_embedding_cursor_to_edge(state, false);
            Outcome::Consumed
        },
        KeyCode::End => {
            move_embedding_cursor_to_edge(state, true);
            Outcome::Consumed
        },
        KeyCode::Char(' ') => {
            select_embedding_focus(state);
            sync_embedding(state);
            Outcome::Consumed
        },
        KeyCode::Char('d') | KeyCode::Char('D') => {
            let id = state.draft.model.clone();
            let (log_tx, log_rx) = std::sync::mpsc::channel();
            state.start_download_log(log_rx, &id);
            if id == NONE_MODEL {
                send_log(&log_tx, "No downloadable embedding model is selected.");
            } else {
                state.probes.spawn(StepId::Embedding, ProbeKind::Download, move || {
                    probe_download(id, log_tx)
                });
            }
            Outcome::Consumed
        },
        KeyCode::Char('t') | KeyCode::Char('T') => {
            sync_embedding(state);
            let remote = state.draft.remote.as_ref().map(remote_config_for);
            let s = spec(&state.draft.model);
            if let (Some(r), Some(s)) = (remote, s) {
                state
                    .probes
                    .spawn(StepId::Embedding, ProbeKind::ConnectTest, move || probe_connect(r, s));
            }
            Outcome::Consumed
        },
        KeyCode::Char('p') | KeyCode::Char('P') => {
            sync_embedding(state);
            if matches!(
                state.probes.status(StepId::Embedding),
                ProbeStatus::Running(ProbeKind::EphemeralTest)
            ) {
                state.provision_log_lines.push("Provision test already running.".to_string());
                state.ui.provision_log_open = true;
                state.ui.provision_log_scroll = u16::MAX;
                state.ui.provision_log_follow = true;
                return Outcome::Consumed;
            }
            if provision_confirm_satisfied(&state.ui) {
                let remote = state.draft.remote.as_ref().map(remote_config_for);
                let s = spec(&state.draft.model);
                if let (Some(r), Some(s)) = (remote, s) {
                    let (log_tx, log_rx) = std::sync::mpsc::channel();
                    state.start_provision_log(log_rx);
                    state.probes.spawn_ephemeral(StepId::Embedding, move |cancel| {
                        let _guard = rag_rat_core::index::ai::install_provision_log_sink(log_tx);
                        probe_ephemeral(r, s, cancel.as_ref())
                    });
                }
            }
            Outcome::Consumed
        },
        _ => Outcome::Pass,
    }
}

fn embed_focus(state: &WizardState) -> Option<EmbedFocus> {
    match &state.step {
        Some(StepState::Embedding { focus, .. }) => Some(*focus),
        _ => None,
    }
}

fn embed_focus_order(rmode: usize, model_none: bool) -> &'static [EmbedFocus] {
    const MODEL_ONLY: &[EmbedFocus] = &[EmbedFocus::Model];
    const LOCAL: &[EmbedFocus] = &[EmbedFocus::Model, EmbedFocus::Mode];
    const CONNECT: &[EmbedFocus] = &[
        EmbedFocus::Model,
        EmbedFocus::Mode,
        EmbedFocus::Endpoint,
        EmbedFocus::ServerModel,
        EmbedFocus::BatchSize,
        EmbedFocus::Concurrency,
        EmbedFocus::MaxBatchChars,
        EmbedFocus::AuthEnv,
    ];
    const EPHEMERAL: &[EmbedFocus] = &[
        EmbedFocus::Model,
        EmbedFocus::Mode,
        EmbedFocus::Cookbook,
        EmbedFocus::Gpu,
        EmbedFocus::ServerModel,
        EmbedFocus::BatchSize,
        EmbedFocus::Concurrency,
        EmbedFocus::MaxBatchChars,
        EmbedFocus::AuthEnv,
        EmbedFocus::ProvisionConfirm,
    ];

    if model_none {
        MODEL_ONLY
    } else {
        match rmode {
            1 => CONNECT,
            2 => EPHEMERAL,
            _ => LOCAL,
        }
    }
}

fn cycle_embed_focus(state: &mut WizardState, delta: isize) {
    let rmode = remote_mode(state);
    let model_none = state.draft.model == NONE_MODEL || state.draft.model.is_empty();
    let order = embed_focus_order(rmode, model_none);
    if let Some(StepState::Embedding { focus, .. }) = &mut state.step {
        let current = order.iter().position(|candidate| candidate == focus).unwrap_or(0);
        let len = order.len();
        let next = if delta >= 0 {
            (current + 1) % len
        } else {
            current.checked_sub(1).unwrap_or(len - 1)
        };
        *focus = order[next];
    }
}

fn list_window(cursor: usize, scroll: &mut usize) {
    const WINDOW: usize = 8;
    if cursor < *scroll {
        *scroll = cursor;
    } else if cursor >= scroll.saturating_add(WINDOW) {
        *scroll = cursor.saturating_sub(WINDOW - 1);
    }
}

fn move_embedding_cursor(state: &mut WizardState, delta: isize) {
    let focus = match embed_focus(state) {
        Some(f) => f,
        None => return,
    };
    if remote_numeric_focus(focus) {
        adjust_remote_numeric(state, focus, delta);
        return;
    }
    let cookbook_len = cookbook_choices(state).len();
    let gpu_len = current_gpu_options(state).len();
    let server_model_len = if focus == EmbedFocus::ServerModel {
        compatible_server_models(&state.draft.model).len()
    } else {
        0
    };
    if let Some(StepState::Embedding {
        model_cursor,
        mode_cursor,
        cookbook_cursor,
        server_model_cursor,
        gpu_cursor,
        model_scroll,
        server_model_scroll,
        ..
    }) = &mut state.step
    {
        match focus {
            EmbedFocus::Model => {
                let last = model_rows().len().saturating_sub(1);
                *model_cursor =
                    (*model_cursor as isize).saturating_add(delta).clamp(0, last as isize) as usize;
                list_window(*model_cursor, model_scroll);
            },
            EmbedFocus::Mode => {
                *mode_cursor = (*mode_cursor as isize).saturating_add(delta).clamp(0, 2) as usize;
            },
            EmbedFocus::Cookbook => {
                let last = cookbook_len.saturating_sub(1);
                *cookbook_cursor = (*cookbook_cursor as isize)
                    .saturating_add(delta)
                    .clamp(0, last as isize) as usize;
            },
            EmbedFocus::ServerModel => {
                let last = server_model_len.saturating_sub(1);
                *server_model_cursor = (*server_model_cursor as isize)
                    .saturating_add(delta)
                    .clamp(0, last as isize) as usize;
                list_window(*server_model_cursor, server_model_scroll);
            },
            EmbedFocus::Gpu => {
                let last = gpu_len.saturating_sub(1);
                *gpu_cursor =
                    (*gpu_cursor as isize).saturating_add(delta).clamp(0, last as isize) as usize;
            },
            _ => {},
        }
    }
}

fn move_embedding_cursor_to_edge(state: &mut WizardState, end: bool) {
    let focus = match embed_focus(state) {
        Some(f) => f,
        None => return,
    };
    let server_model_len = if focus == EmbedFocus::ServerModel {
        compatible_server_models(&state.draft.model).len()
    } else {
        0
    };
    let cookbook_len = cookbook_choices(state).len();
    let gpu_len = current_gpu_options(state).len();
    let target = match focus {
        EmbedFocus::Model => model_rows().len().saturating_sub(1),
        EmbedFocus::Mode => 2,
        EmbedFocus::Cookbook => cookbook_len.saturating_sub(1),
        EmbedFocus::ServerModel => server_model_len.saturating_sub(1),
        EmbedFocus::Gpu => gpu_len.saturating_sub(1),
        _ => 0,
    };
    if let Some(StepState::Embedding {
        model_cursor,
        mode_cursor,
        cookbook_cursor,
        server_model_cursor,
        gpu_cursor,
        model_scroll,
        server_model_scroll,
        ..
    }) = &mut state.step
    {
        let value = if end { target } else { 0 };
        match focus {
            EmbedFocus::Model => {
                *model_cursor = value;
                list_window(*model_cursor, model_scroll);
            },
            EmbedFocus::Mode => *mode_cursor = value,
            EmbedFocus::Cookbook => *cookbook_cursor = value,
            EmbedFocus::ServerModel => {
                *server_model_cursor = value;
                list_window(*server_model_cursor, server_model_scroll);
            },
            EmbedFocus::Gpu => *gpu_cursor = value,
            _ => {},
        }
    }
}

fn scroll_embedding(delta: isize, state: &mut WizardState) -> bool {
    move_embedding_cursor(state, delta);
    true
}

fn select_embedding_focus(state: &mut WizardState) {
    let focus = match embed_focus(state) {
        Some(f) => f,
        None => return,
    };
    let mut changed = false;
    match focus {
        EmbedFocus::Model => {
            let rows = model_rows();
            let cursor = match &state.step {
                Some(StepState::Embedding { model_cursor, .. }) => *model_cursor,
                _ => 0,
            };
            if let Some((id, _)) = rows.get(cursor) {
                let previous_default = default_remote_model_for(&state.draft.model).to_string();
                changed |= state.draft.model != *id;
                state.draft.model = id.clone();
                let default_model = default_remote_model_for(&state.draft.model).to_string();
                if let Some(remote) = &mut state.draft.remote
                    && (remote.model.is_empty() || remote.model == previous_default)
                {
                    changed |= remote.model != default_model;
                    remote.model = default_model;
                }
            }
        },
        EmbedFocus::Mode => {
            let cursor = match &state.step {
                Some(StepState::Embedding { mode_cursor, .. }) => *mode_cursor,
                _ => 0,
            };
            let current = remote_mode(state);
            if cursor != current {
                let existing = state.draft.remote.as_ref().cloned();
                let default_cookbook = default_cookbook_command(state);
                state.draft.remote = match cursor {
                    0 => None,
                    1 => Some(new_connect_remote_from(&state.draft.model, existing.as_ref())),
                    2 => Some(new_ephemeral_remote_from(
                        &state.draft.model,
                        existing.as_ref(),
                        &default_cookbook,
                    )),
                    _ => None,
                };
                changed = true;
            }
            state.ui.show_remote_mode_help_once(cursor);
        },
        EmbedFocus::Cookbook => {
            let cursor = match &state.step {
                Some(StepState::Embedding { cookbook_cursor, .. }) => *cookbook_cursor,
                _ => 0,
            };
            let choices = cookbook_choices(state);
            if let Some(entry) = choices.get(cursor)
                && let Some(remote) = &mut state.draft.remote
            {
                let mode = RemoteMode::Ephemeral(entry.command.clone());
                if remote.mode != mode {
                    remote.mode = mode;
                    remote.gpu = None;
                    changed = true;
                }
            }
        },
        EmbedFocus::ServerModel => {
            let cursor = match &state.step {
                Some(StepState::Embedding { server_model_cursor, .. }) => *server_model_cursor,
                _ => 0,
            };
            let server_models = compatible_server_models(&state.draft.model);
            if let Some(model) = server_models.get(cursor)
                && let Some(remote) = &mut state.draft.remote
            {
                changed |= remote.model != *model;
                remote.model = (*model).to_string();
            }
        },
        EmbedFocus::Gpu => {
            let cursor = match &state.step {
                Some(StepState::Embedding { gpu_cursor, .. }) => *gpu_cursor,
                _ => 0,
            };
            let gpu = current_gpu_options(state).get(cursor).cloned();
            if let Some(gpu) = gpu
                && let Some(remote) = &mut state.draft.remote
            {
                changed |= remote.gpu.as_deref() != Some(gpu.as_str());
                remote.gpu = Some(gpu);
            }
        },
        _ => {},
    }
    if changed {
        state.probes.bump(StepId::Embedding);
    }
}

fn new_connect_remote(local_model: &str) -> RemoteDraft {
    RemoteDraft {
        model: default_remote_model_for(local_model).to_string(),
        mode: RemoteMode::Connect(DEFAULT_QUERY_ENDPOINT.to_string()),
        gpu: None,
        num_ctx: None,
        batch_size: 256,
        concurrency: RemoteEmbeddingConfig::omitted_concurrency_default(true),
        max_batch_chars: RemoteEmbeddingConfig::default().max_batch_chars,
        auth_env: None,
    }
}

fn new_connect_remote_from(local_model: &str, existing: Option<&RemoteDraft>) -> RemoteDraft {
    let mut remote = new_connect_remote(local_model);
    if let Some(existing) = existing {
        remote.model = preserved_server_model(local_model, existing);
        remote.num_ctx = existing.num_ctx;
        remote.batch_size = existing.batch_size;
        if matches!(existing.mode, RemoteMode::Connect(_)) {
            remote.concurrency = existing.concurrency.min(MAX_REMOTE_EMBEDDING_CONCURRENCY);
        }
        remote.max_batch_chars = existing.max_batch_chars;
        remote.auth_env = existing.auth_env.clone();
    }
    remote
}

#[cfg(test)]
fn new_ephemeral_remote(local_model: &str) -> RemoteDraft {
    new_ephemeral_remote_with_command(local_model, "@rag-rat/cookbook modal")
}

fn new_ephemeral_remote_with_command(local_model: &str, cookbook: &str) -> RemoteDraft {
    RemoteDraft {
        model: default_remote_model_for(local_model).to_string(),
        mode: RemoteMode::Ephemeral(cookbook.to_string()),
        gpu: None,
        num_ctx: None,
        batch_size: 256,
        concurrency: RemoteEmbeddingConfig::omitted_concurrency_default(false),
        max_batch_chars: RemoteEmbeddingConfig::default().max_batch_chars,
        auth_env: None,
    }
}

fn new_ephemeral_remote_from(
    local_model: &str,
    existing: Option<&RemoteDraft>,
    default_cookbook: &str,
) -> RemoteDraft {
    let mut remote = new_ephemeral_remote_with_command(local_model, default_cookbook);
    if let Some(existing) = existing {
        remote.model = preserved_server_model(local_model, existing);
        remote.gpu = existing.gpu.clone();
        remote.num_ctx = existing.num_ctx;
        remote.batch_size = existing.batch_size;
        if matches!(existing.mode, RemoteMode::Ephemeral(_)) {
            remote.concurrency = existing.concurrency.min(MAX_REMOTE_EMBEDDING_CONCURRENCY);
        }
        remote.max_batch_chars = existing.max_batch_chars;
        remote.auth_env = existing.auth_env.clone();
    }
    remote
}

fn preserved_server_model(local_model: &str, existing: &RemoteDraft) -> String {
    if existing.model.trim().is_empty() {
        default_remote_model_for(local_model).to_string()
    } else {
        existing.model.clone()
    }
}

fn remote_numeric_focus(focus: EmbedFocus) -> bool {
    matches!(focus, EmbedFocus::BatchSize | EmbedFocus::Concurrency | EmbedFocus::MaxBatchChars)
}

fn adjust_remote_numeric(state: &mut WizardState, focus: EmbedFocus, delta: isize) {
    let changed = {
        let Some(remote) = &mut state.draft.remote else { return };
        match focus {
            EmbedFocus::BatchSize =>
                adjust_u32(&mut remote.batch_size, delta, 1, REMOTE_BATCH_SIZE_MAX),
            EmbedFocus::Concurrency =>
                adjust_u32(&mut remote.concurrency, delta, 1, MAX_REMOTE_EMBEDDING_CONCURRENCY),
            EmbedFocus::MaxBatchChars => adjust_usize(&mut remote.max_batch_chars, delta, 1),
            _ => false,
        }
    };
    if changed {
        state.probes.bump(StepId::Embedding);
    }
}

fn adjust_u32(value: &mut u32, delta: isize, min: u32, max: u32) -> bool {
    let before = *value;
    let next = if delta >= 0 {
        value.saturating_add(delta as u32).min(max)
    } else {
        let magnitude = u32::try_from(delta.unsigned_abs()).unwrap_or(u32::MAX);
        value.saturating_sub(magnitude).max(min)
    };
    *value = next;
    before != next
}

fn adjust_usize(value: &mut usize, delta: isize, min: usize) -> bool {
    let before = *value;
    let next = if delta >= 0 {
        value.saturating_add(delta as usize)
    } else {
        value.saturating_sub(delta.unsigned_abs()).max(min)
    };
    *value = next;
    before != next
}

fn edit_embedding_field(key: KeyEvent, state: &mut WizardState) -> bool {
    let Some(focus) = embed_focus(state) else { return false };
    match focus {
        EmbedFocus::Endpoint => edit_endpoint(key, state),
        EmbedFocus::BatchSize | EmbedFocus::Concurrency | EmbedFocus::MaxBatchChars =>
            edit_remote_numeric(focus, key, state),
        EmbedFocus::AuthEnv => edit_auth_env(key, state),
        EmbedFocus::ProvisionConfirm => edit_provision_confirm(key, state),
        _ => false,
    }
}

fn editable_char(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            Some(c),
        _ => None,
    }
}

fn edit_endpoint(key: KeyEvent, state: &mut WizardState) -> bool {
    let Some(remote) = &mut state.draft.remote else { return false };
    let RemoteMode::Connect(endpoint) = &mut remote.mode else { return false };
    match key.code {
        KeyCode::Backspace => {
            endpoint.pop();
            state.probes.bump(StepId::Embedding);
            true
        },
        _ =>
            if let Some(c) = editable_char(key) {
                endpoint.push(c);
                state.probes.bump(StepId::Embedding);
                true
            } else {
                false
            },
    }
}

fn edit_remote_numeric(focus: EmbedFocus, key: KeyEvent, state: &mut WizardState) -> bool {
    let Some(remote) = &mut state.draft.remote else { return false };
    let changed = match key.code {
        KeyCode::Backspace => match focus {
            EmbedFocus::BatchSize => {
                let before = remote.batch_size;
                remote.batch_size = (remote.batch_size / 10).max(1);
                before != remote.batch_size
            },
            EmbedFocus::Concurrency => {
                let before = remote.concurrency;
                remote.concurrency = (remote.concurrency / 10).max(1);
                before != remote.concurrency
            },
            EmbedFocus::MaxBatchChars => {
                let before = remote.max_batch_chars;
                remote.max_batch_chars = (remote.max_batch_chars / 10).max(1);
                before != remote.max_batch_chars
            },
            _ => return false,
        },
        KeyCode::Char(c) if c.is_ascii_digit() => {
            let digit = c.to_digit(10).unwrap_or(0);
            match focus {
                EmbedFocus::BatchSize => {
                    let before = remote.batch_size;
                    remote.batch_size = remote
                        .batch_size
                        .saturating_mul(10)
                        .saturating_add(digit)
                        .clamp(1, REMOTE_BATCH_SIZE_MAX);
                    before != remote.batch_size
                },
                EmbedFocus::Concurrency => {
                    let before = remote.concurrency;
                    remote.concurrency = remote
                        .concurrency
                        .saturating_mul(10)
                        .saturating_add(digit)
                        .clamp(1, MAX_REMOTE_EMBEDDING_CONCURRENCY);
                    before != remote.concurrency
                },
                EmbedFocus::MaxBatchChars => {
                    let before = remote.max_batch_chars;
                    remote.max_batch_chars = remote
                        .max_batch_chars
                        .saturating_mul(10)
                        .saturating_add(digit as usize)
                        .max(1);
                    before != remote.max_batch_chars
                },
                _ => return false,
            }
        },
        _ => return false,
    };
    if changed {
        state.probes.bump(StepId::Embedding);
    }
    true
}

fn edit_auth_env(key: KeyEvent, state: &mut WizardState) -> bool {
    let Some(remote) = &mut state.draft.remote else { return false };
    let mut value = remote.auth_env.take().unwrap_or_default();
    let handled = match key.code {
        KeyCode::Backspace => {
            value.pop();
            true
        },
        _ =>
            if let Some(c) = editable_char(key) {
                value.push(c);
                true
            } else {
                false
            },
    };
    remote.auth_env = (!value.is_empty()).then_some(value);
    if handled {
        state.probes.bump(StepId::Embedding);
    }
    handled
}

fn edit_provision_confirm(key: KeyEvent, state: &mut WizardState) -> bool {
    if provision_confirm_satisfied(&state.ui)
        && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
    {
        return false;
    }
    match key.code {
        KeyCode::Backspace => {
            state.ui.provision_confirm.pop();
            true
        },
        _ =>
            if let Some(c) = editable_char(key) {
                state.ui.provision_confirm.push(c);
                true
            } else {
                false
            },
    }
}

fn sync_embedding(_state: &mut WizardState) {
    // state.draft is mutated directly in handle_key
}

fn validate_embedding(state: &WizardState) -> CheckResult {
    if state.draft.remote.is_some()
        && !matches!(spec(&state.draft.model).map(|s| s.backend), Some(Backend::FastEmbed))
    {
        return CheckResult::block(
            "remote Ollama embeddings require a transformer model; select MiniLM, BGE, or Jina, \
             or disable remote mode",
        );
    }
    if let Some(r) = &state.draft.remote {
        if r.model.trim().is_empty() {
            return CheckResult::block("remote server model is required");
        }
        if let Some(local) = spec(&state.draft.model)
            && let Some(server_dim) = ollama_model_dim(r.model.trim())
            && server_dim != local.dim
        {
            return CheckResult::block(format!(
                "remote server model `{}` is {server_dim}d but selected model `{}` is {}d",
                r.model.trim(),
                local.model_id,
                local.dim
            ));
        }
        if let Some(auth_env) = &r.auth_env
            && let Some(check) = validate_remote_auth_env(auth_env)
        {
            return check;
        }
        match &r.mode {
            RemoteMode::Connect(endpoint) => {
                if endpoint.trim().is_empty() {
                    return CheckResult::block("connect endpoint is required");
                }
                if endpoint_authority_has_userinfo(endpoint) {
                    return CheckResult::block(
                        "connect endpoint must not include credentials; use auth env instead",
                    );
                }
                if r.gpu.is_some() {
                    return CheckResult::warn("gpu only applies to ephemeral mode");
                }
            },
            RemoteMode::Ephemeral(cookbook) =>
                if cookbook.trim().is_empty() {
                    return CheckResult::block("ephemeral cookbook command is required");
                },
        }
        if let Some(warning) = remote_model_quality_warning(&state.draft.model, r.model.trim()) {
            return CheckResult::warn(warning);
        }
    }
    CheckResult::ok()
}

fn remote_model_quality_warning(local_model: &str, server_model: &str) -> Option<String> {
    let expected = ollama_model_for(local_model)?;
    if server_model == expected || !OLLAMA_EMBEDDING_MODELS.contains(&server_model) {
        return None;
    }
    let local = spec(local_model)?;
    if ollama_model_dim(server_model) != Some(local.dim) {
        return None;
    }
    Some(format!(
        "remote server model `{server_model}` is dimension-compatible with `{}` but not the same \
         embedding family; quality may vary",
        local.model_id
    ))
}

fn validate_remote_auth_env(auth_env: &str) -> Option<CheckResult> {
    if auth_env.trim().is_empty() {
        return Some(CheckResult::block("remote auth env name is empty"));
    }
    match std::env::var(auth_env) {
        Ok(value) if !value.trim().is_empty() => None,
        _ => Some(CheckResult::block(format!("remote auth env `{auth_env}` is not set"))),
    }
}

fn endpoint_authority_has_userinfo(endpoint: &str) -> bool {
    let after_scheme = endpoint.split_once("://").map_or(endpoint, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    authority.contains('@')
}

fn remote_config_for(d: &RemoteDraft) -> RemoteEmbeddingConfig {
    let (ep, cb) = match &d.mode {
        RemoteMode::Connect(u) => (Some(u.clone()), None),
        RemoteMode::Ephemeral(c) => (None, Some(c.clone())),
    };
    RemoteEmbeddingConfig {
        model: d.model.clone(),
        // The wizard's Remote step does not yet offer a backend picker (follow-up), so it drafts
        // the default `ollama` backend; a user wanting infinity/vLLM sets `[remote]
        // backend` in the TOML.
        backend: rag_rat_core::config::RemoteBackend::Ollama,
        endpoint: ep,
        cookbook: cb,
        query_endpoint: None,
        auth_env: d.auth_env.clone(),
        gpu: d.gpu.clone(),
        num_ctx: d.num_ctx,
        batch_size: d.batch_size,
        concurrency: d.concurrency,
        max_batch_chars: d.max_batch_chars,
        request_timeout_s: RemoteEmbeddingConfig::default().request_timeout_s,
    }
}

fn probe_download(id: String, log_tx: Sender<String>) -> CheckResult {
    if id == NONE_MODEL {
        send_log(&log_tx, "No downloadable embedding model is selected.");
        return CheckResult::ok();
    }
    send_log(&log_tx, format!("Preparing embedder for {id}..."));
    match build_embedder(&id) {
        Ok(e) => {
            send_log(&log_tx, "Verifying model with a ping embedding...");
            match e.embed_batch(&["ping".to_string()]) {
                Ok(v) if v.first().is_some_and(|x| !x.is_empty()) => {
                    send_log(&log_tx, "Model download and verification completed.");
                    CheckResult::ok()
                },
                Ok(_) => {
                    let msg = format!("{id} empty embedding");
                    send_log(&log_tx, msg.clone());
                    CheckResult::warn(msg)
                },
                Err(e) => {
                    let msg = format!("verify: {e}");
                    send_log(&log_tx, msg.clone());
                    CheckResult::warn(msg)
                },
            }
        },
        Err(e) => {
            let msg = format!("download: {e}");
            send_log(&log_tx, msg.clone());
            CheckResult::warn(msg)
        },
    }
}

fn probe_connect(r: RemoteEmbeddingConfig, s: &EmbeddingModelSpec) -> CheckResult {
    match OpenAiEmbedder::from_remote_config(&r, s.model_id, s.dim) {
        Ok(e) => match e.embed_batch(&["ping".to_string()]) {
            Ok(v) if v.first().is_some_and(|x| !x.is_empty()) => CheckResult::ok(),
            Ok(_) => CheckResult::warn("empty embedding"),
            Err(e) => CheckResult::warn(format!("connect: {e}")),
        },
        Err(e) => CheckResult::warn(format!("build: {e}")),
    }
}

fn probe_ephemeral(
    r: RemoteEmbeddingConfig,
    s: &EmbeddingModelSpec,
    cancel: &std::sync::atomic::AtomicBool,
) -> CheckResult {
    // A throwaway spin-up test: provision, embed one "ping", tear down. It passes `tune = None`, so
    // it does NOT run the throughput sweep (that runs at reconcile, against a box the index keeps);
    // the probe only reports pass/fail at the user's configured `[remote] concurrency` cap.
    match verify_ephemeral_remote_cancellable(&r, s, || cancel.load(Ordering::Acquire)) {
        Ok(()) => CheckResult::ok(),
        Err(e) => CheckResult::warn(format!("ephemeral: {e}")),
    }
}

fn build_embedder(id: &str) -> anyhow::Result<Box<dyn Embedder>> {
    let s = spec(id).ok_or_else(|| anyhow::anyhow!("unknown model {id}"))?;
    match s.backend {
        Backend::Hash => Ok(Box::new(HashEmbedder)),
        Backend::FastEmbed => {
            #[cfg(feature = "fastembed")]
            {
                Ok(Box::new(FastEmbedEmbedder::for_model_id(s.model_id, s.dim, None)?))
            }
            #[cfg(not(feature = "fastembed"))]
            {
                anyhow::bail!("no fastembed")
            }
        },
        Backend::Model2Vec => {
            #[cfg(feature = "model2vec")]
            {
                Ok(Box::new(Model2VecEmbedder::new()?))
            }
            #[cfg(not(feature = "model2vec"))]
            {
                anyhow::bail!("no model2vec")
            }
        },
        Backend::Ollama => anyhow::bail!("ollama is transport, not local"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// INTEGRATION
// ═══════════════════════════════════════════════════════════════════════════════

fn render_integration(f: &mut Frame, area: Rect, state: &WizardState) {
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

fn handle_integration(key: KeyEvent, state: &mut WizardState) -> Outcome {
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

fn validate_hooks(state: &WizardState) -> CheckResult {
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tempfile::TempDir;

    use super::*;
    use crate::init::RepoScan;
    use crate::init::scan::scan_repo;
    use crate::init::wizard::catalog::CookbookCatalog;
    use crate::init::wizard::draft::{MODAL_GPUS, WizardDraft};
    use crate::init::wizard::probe::ProbeMsg;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn empty_state() -> WizardState {
        let scan = RepoScan::default();
        let draft = WizardDraft::from_scan(&scan, ".".to_string(), PathBuf::from("."));
        WizardState::new(draft, scan)
    }

    fn state_with_catalog(raw: &str) -> WizardState {
        let scan = RepoScan::default();
        let draft = WizardDraft::from_scan(&scan, ".".to_string(), PathBuf::from("."));
        WizardState::with_cookbooks(draft, scan, CookbookCatalog::from_raw(raw))
    }

    #[test]
    fn oracle_test_key_opens_log_immediately() {
        let mut state = empty_state();
        state.step = Some(init_step(StepId::Oracle, &state));

        assert_eq!(handle_oracle(key(KeyCode::Char('t')), &mut state), Outcome::Consumed);

        assert!(state.ui.provision_log_open);
        assert_eq!(state.provision_log_title, "Oracle tool test");
        assert!(
            state
                .provision_log_lines
                .first()
                .is_some_and(|line| line.contains("Starting Oracle tool test"))
        );
    }

    #[test]
    fn embedding_download_key_opens_log_immediately() {
        let mut state = empty_state();
        let hash_model =
            EMBEDDING_MODELS.iter().find(|spec| spec.backend == Backend::Hash).unwrap().model_id;
        state.draft.model = hash_model.to_string();
        state.step = Some(init_step(StepId::Embedding, &state));

        assert_eq!(handle_embedding(key(KeyCode::Char('d')), &mut state), Outcome::Consumed);

        assert!(state.ui.provision_log_open);
        assert_eq!(state.provision_log_title, "Model download");
        assert!(
            state
                .provision_log_lines
                .first()
                .is_some_and(|line| line.contains("Starting model download"))
        );
    }

    fn rust_state(dir_count: usize) -> (TempDir, WizardState) {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..dir_count {
            let path = dir.path().join(format!("src/module_{i}"));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("lib.rs"), "").unwrap();
        }
        let scan = scan_repo(dir.path()).unwrap();
        let draft = WizardDraft::from_scan(&scan, ".".to_string(), dir.path().to_path_buf());
        let mut state = WizardState::new(draft, scan);
        state.step = Some(init_step(StepId::Indexing, &state));
        (dir, state)
    }

    fn language_toggle(state: &WizardState, lang: Language) -> Option<bool> {
        match &state.step {
            Some(StepState::Indexing { lang_toggles, .. }) =>
                lang_toggles.iter().find(|(candidate, _)| *candidate == lang).map(|(_, on)| *on),
            _ => None,
        }
    }

    fn render_embedding_cells(state: &WizardState, width: u16, height: u16) -> Vec<Vec<String>> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_embedding(f, f.area(), state)).unwrap();
        let buffer = terminal.backend().buffer();

        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    fn row_text(row: &[String]) -> String {
        row.concat()
    }

    fn row_segment(cells: &[Vec<String>], y: usize, x: usize, width: usize) -> String {
        cells[y][x..x + width].concat()
    }

    fn screen_text(cells: &[Vec<String>]) -> String {
        cells.iter().map(|row| row_text(row)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn oracle_tool_result_wraps_inside_availability_box() {
        let mut state = empty_state();
        state.step = Some(init_step(StepId::Oracle, &state));
        let probe_id = state.probes.current(StepId::Oracle);
        state.probes.apply(ProbeMsg {
            probe_id,
            kind: ProbeKind::OracleTool,
            result: CheckResult::warn(concat!(
                "scip probe emitted a long diagnostic with enough words to require wrapping ",
                "inside the availability box and still show tail-token-inside-oracle"
            )),
        });

        let width = 52;
        let height = 20;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_oracle(f, f.area(), &state)).unwrap();
        let cells = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                    .collect::<Vec<String>>()
            })
            .collect::<Vec<Vec<String>>>();
        let screen = screen_text(&cells);

        assert!(
            screen.contains("tail-token-inside-oracle"),
            "long Oracle result should wrap instead of clipping:\n{screen}"
        );
    }

    #[test]
    fn step_dispatch_render_and_validation_smoke() {
        for step in StepId::ALL {
            let mut state = empty_state();
            state.step = Some(init_step(step, &state));

            let backend = TestBackend::new(120, 32);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| render_step(step, f, f.area(), &mut state)).unwrap();
            let rendered: String =
                terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();

            assert!(!rendered.trim().is_empty(), "{step:?} rendered an empty screen");
            let _ = validate_step(step, &state);
            let _ = step_handle_key(step, key(KeyCode::Tab), &mut state);
            let _ = step_handle_key(step, key(KeyCode::BackTab), &mut state);
            let _ = step_handle_key(step, key(KeyCode::Enter), &mut state);
        }
    }

    #[test]
    fn chained_hook_renderer_preserves_existing_body_and_appends_maintenance() {
        let rendered = hooks::render_chained_hook("#!/bin/sh\necho before\n", "pre-commit");

        assert!(rendered.starts_with("#!/bin/sh\n# Chained hook"));
        assert!(rendered.contains("echo before"));
        assert!(rendered.contains("rag-rat maintenance"));
        assert!(rendered.contains("--trigger pre-commit"));
    }

    #[test]
    fn indexing_tree_cursor_toggles_the_focused_directory() {
        let (_dir, mut state) = rust_state(1);
        assert!(state.draft.bindings.get(&Language::Rust).unwrap().contains(&PathBuf::from("src")));

        step_handle_key(StepId::Indexing, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Indexing, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Indexing, key(KeyCode::Char(' ')), &mut state);

        assert!(
            !state.draft.bindings.get(&Language::Rust).unwrap().contains(&PathBuf::from("src"))
        );
    }

    #[test]
    fn indexing_reentry_preserves_disabled_detected_languages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("py")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("py/app.py"), "").unwrap();
        let scan = scan_repo(dir.path()).unwrap();
        let mut draft = WizardDraft::from_scan(&scan, ".".to_string(), dir.path().to_path_buf());
        assert!(draft.bindings.contains_key(&Language::Python));
        draft.bindings.remove(&Language::Python);
        let mut state = WizardState::new(draft, scan);

        state.step = Some(init_step(StepId::Indexing, &state));

        assert_eq!(language_toggle(&state, Language::Rust), Some(true));
        assert_eq!(language_toggle(&state, Language::Python), Some(false));
        assert!(!state.draft.bindings.contains_key(&Language::Python));
    }

    #[test]
    fn indexing_tree_page_down_moves_selection() {
        let (_dir, mut state) = rust_state(18);
        step_handle_key(StepId::Indexing, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Indexing, key(KeyCode::PageDown), &mut state);

        let selected = selected_dir_path(&state).expect("tree selection");
        assert_ne!(selected, PathBuf::from("."));
    }

    #[test]
    fn indexing_shift_tab_wraps_to_previous_focus_zone() {
        let (_dir, mut state) = rust_state(1);

        assert_eq!(
            step_handle_key(StepId::Indexing, key(KeyCode::BackTab), &mut state),
            Outcome::Consumed
        );
        assert!(matches!(&state.step, Some(StepState::Indexing { zone: IndexZone::Tree, .. })));

        assert_eq!(
            step_handle_key(StepId::Indexing, key(KeyCode::BackTab), &mut state),
            Outcome::Consumed
        );
        assert!(matches!(&state.step, Some(StepState::Indexing { zone: IndexZone::Toggles, .. })));
    }

    #[test]
    fn indexing_render_and_navigation_edges_are_stable() {
        let (_dir, mut state) = rust_state(6);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_indexing(f, f.area(), &mut state)).unwrap();
        assert!(
            screen_text(
                &(0..24)
                    .map(|y| {
                        (0..100)
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                            .collect()
                    })
                    .collect::<Vec<Vec<String>>>()
            )
            .contains("Directories")
        );

        assert_eq!(
            step_handle_key(StepId::Indexing, key(KeyCode::Char(' ')), &mut state),
            Outcome::Consumed
        );
        assert_eq!(
            step_handle_key(StepId::Indexing, key(KeyCode::Char('a')), &mut state),
            Outcome::Consumed
        );
        assert_eq!(
            step_handle_key(StepId::Indexing, key(KeyCode::Char('A')), &mut state),
            Outcome::Consumed
        );

        assert_eq!(
            step_handle_key(StepId::Indexing, key(KeyCode::Tab), &mut state),
            Outcome::Consumed
        );
        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::End,
            KeyCode::Home,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Enter,
        ] {
            let _ = step_handle_key(StepId::Indexing, key(code), &mut state);
        }
        assert_eq!(step_handle_key(StepId::Indexing, key(KeyCode::Esc), &mut state), Outcome::Back);
    }

    #[test]
    fn indexing_candidates_cover_empty_cache_and_missing_paths() {
        let mut state = empty_state();
        state.step = Some(init_step(StepId::Indexing, &state));
        state.draft.bindings.insert(Language::Rust, vec![PathBuf::from("missing")]);

        let candidates = candidates_for(Language::Rust, &state);
        assert_eq!(candidates[0].path, PathBuf::from("."));
        assert!(matches!(validate_indexing(&state).severity, Sev::Warn));
    }

    #[test]
    fn indexing_blocks_empty_simple_bindings() {
        let mut state = empty_state();
        state.draft.bindings.clear();

        let check = validate_indexing(&state);

        assert_eq!(check.severity, Sev::Block);
        assert!(check.message.unwrap().contains("indexed directory"));
    }

    #[test]
    fn indexing_allows_preserved_rich_targets_without_simple_bindings() {
        let mut state = empty_state();
        state.draft.bindings.clear();
        state.draft.has_rich_targets = true;

        assert_eq!(validate_indexing(&state).severity, Sev::Ok);
    }

    #[test]
    fn indexing_blocks_simple_binding_names_that_conflict_with_preserved_rich_targets() {
        let mut state = empty_state();
        state.draft.has_rich_targets = true;
        state.draft.rich_target_names.insert(Language::Rust.as_str().to_string());
        state.draft.bindings.insert(Language::Rust, Vec::new());

        let check = validate_indexing(&state);

        assert_eq!(check.severity, Sev::Block);
        assert!(check.message.unwrap().contains("rust"));
    }

    #[test]
    fn embedding_server_model_selector_allows_dimension_compatible_alternatives() {
        let mut state = empty_state();
        state.step = Some(init_step(StepId::Embedding, &state));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        let remote = state.draft.remote.as_ref().expect("connect remote should be selected");
        assert!(
            matches!(remote.mode, RemoteMode::Connect(ref endpoint) if endpoint == DEFAULT_QUERY_ENDPOINT)
        );
        assert_eq!(remote.model, default_remote_model_for(&state.draft.model));
        assert_eq!(remote.concurrency, RemoteEmbeddingConfig::omitted_concurrency_default(true));
        assert_eq!(compatible_server_models(&state.draft.model), vec![
            "all-minilm",
            "qllama/bge-small-en-v1.5:f16"
        ]);

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        assert_eq!(state.draft.remote.as_ref().unwrap().model, "qllama/bge-small-en-v1.5:f16");
        assert_eq!(validate_embedding(&state).severity, Sev::Warn);
    }

    #[test]
    fn embedding_ephemeral_defaults_without_gpu() {
        let mut state = empty_state();
        state.step = Some(init_step(StepId::Embedding, &state));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        assert!(matches!(
            state.draft.remote.as_ref().map(|remote| &remote.mode),
            Some(RemoteMode::Ephemeral(_))
        ));
        assert_eq!(state.draft.remote.as_ref().and_then(|remote| remote.gpu.as_deref()), None);
        assert_eq!(
            state.draft.remote.as_ref().map(|remote| remote.concurrency),
            Some(RemoteEmbeddingConfig::omitted_concurrency_default(false))
        );

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        assert_eq!(state.draft.remote.as_ref().and_then(|remote| remote.gpu.as_deref()), None);
    }

    #[test]
    fn embedding_mode_switch_resets_mode_specific_concurrency_default() {
        let mut state = empty_state();
        state.draft.remote = Some(new_ephemeral_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));
        assert_eq!(
            state.draft.remote.as_ref().map(|remote| remote.concurrency),
            Some(RemoteEmbeddingConfig::omitted_concurrency_default(false))
        );

        if let Some(StepState::Embedding { focus, mode_cursor, .. }) = &mut state.step {
            *focus = EmbedFocus::Mode;
            *mode_cursor = 1;
        }
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        let remote = state.draft.remote.as_ref().unwrap();
        assert!(matches!(remote.mode, RemoteMode::Connect(_)));
        assert_eq!(remote.concurrency, RemoteEmbeddingConfig::omitted_concurrency_default(true));

        if let Some(StepState::Embedding { mode_cursor, .. }) = &mut state.step {
            *mode_cursor = 2;
        }
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        let remote = state.draft.remote.as_ref().unwrap();
        assert!(matches!(remote.mode, RemoteMode::Ephemeral(_)));
        assert_eq!(remote.concurrency, RemoteEmbeddingConfig::omitted_concurrency_default(false));
    }

    #[test]
    fn embedding_remote_requires_transformer_model() {
        let mut state = empty_state();
        state.draft.model = "none".to_string();
        state.draft.remote = Some(new_connect_remote("sentence-transformers/all-MiniLM-L6-v2"));

        let check = validate_embedding(&state);

        assert_eq!(check.severity, Sev::Block);
        assert!(check.message.unwrap().contains("transformer"));
    }

    #[test]
    fn embedding_blocks_known_server_model_dimension_mismatch() {
        let mut state = empty_state();
        let mut remote = new_connect_remote(&state.draft.model);
        remote.model = "mxbai-embed-large".to_string();
        state.draft.remote = Some(remote);

        let check = validate_embedding(&state);

        assert_eq!(check.severity, Sev::Block);
        let message = check.message.unwrap();
        assert!(message.contains("1024d"));
        assert!(message.contains("384d"));
    }

    #[test]
    fn embedding_warns_when_dimension_match_uses_different_model_family() {
        let mut state = empty_state();
        state.draft.model = "jinaai/jina-embeddings-v2-base-code".to_string();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));

        assert_eq!(validate_embedding(&state).severity, Sev::Ok);

        state.draft.remote.as_mut().unwrap().model = "nomic-embed-text".to_string();
        let check = validate_embedding(&state);

        assert_eq!(check.severity, Sev::Warn);
        assert!(check.message.unwrap().contains("quality may vary"));
    }

    #[test]
    fn embedding_model_change_refreshes_default_server_model() {
        let mut state = empty_state();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        let original_server_model = state.draft.remote.as_ref().unwrap().model.clone();
        state.step = Some(init_step(StepId::Embedding, &state));
        let target_cursor = model_rows()
            .iter()
            .position(|(id, _)| default_remote_model_for(id) != original_server_model)
            .expect("registry should contain a model with a different remote default");
        if let Some(StepState::Embedding { model_cursor, .. }) = &mut state.step {
            *model_cursor = target_cursor;
        }

        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        let remote = state.draft.remote.as_ref().unwrap();
        assert_ne!(remote.model, original_server_model);
        assert_eq!(remote.model, default_remote_model_for(&state.draft.model));
    }

    #[test]
    fn embedding_model_change_preserves_custom_server_model() {
        let mut state = empty_state();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        state.draft.remote.as_mut().unwrap().model = "custom-ollama-model".to_string();
        state.step = Some(init_step(StepId::Embedding, &state));

        step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        assert_eq!(state.draft.remote.as_ref().unwrap().model, "custom-ollama-model");
    }

    #[test]
    fn embedding_mode_reselect_preserves_remote_settings() {
        let mut state = empty_state();
        state.draft.remote = Some(RemoteDraft {
            model: "custom-model".to_string(),
            mode: RemoteMode::Connect("http://remote:11434".to_string()),
            gpu: None,
            num_ctx: Some(4096),
            batch_size: 17,
            concurrency: 9,
            max_batch_chars: 99_000,
            auth_env: Some("OLLAMA_TOKEN".to_string()),
        });
        state.step = Some(init_step(StepId::Embedding, &state));
        let probe_id = state.probes.current(StepId::Embedding);
        assert!(state.probes.apply(ProbeMsg {
            probe_id,
            kind: ProbeKind::ConnectTest,
            result: CheckResult::warn("connect failed"),
        }));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        let remote = state.draft.remote.as_ref().unwrap();
        assert_eq!(remote.model, "custom-model");
        assert!(matches!(
            &remote.mode,
            RemoteMode::Connect(endpoint) if endpoint == "http://remote:11434"
        ));
        assert_eq!(remote.num_ctx, Some(4096));
        assert_eq!(remote.batch_size, 17);
        assert_eq!(remote.concurrency, 9);
        assert_eq!(remote.max_batch_chars, 99_000);
        assert_eq!(remote.auth_env.as_deref(), Some("OLLAMA_TOKEN"));
        assert!(matches!(state.probes.status(StepId::Embedding), ProbeStatus::Done {
            kind: ProbeKind::ConnectTest,
            ..
        }));
    }

    #[test]
    fn embedding_connect_endpoint_must_be_nonempty_and_secret_free() {
        let mut state = empty_state();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        if let Some(RemoteDraft { mode: RemoteMode::Connect(endpoint), .. }) =
            state.draft.remote.as_mut()
        {
            endpoint.clear();
        }
        let empty = validate_embedding(&state);
        assert_eq!(empty.severity, Sev::Block);
        assert!(empty.message.unwrap().contains("endpoint"));

        if let Some(RemoteDraft { mode: RemoteMode::Connect(endpoint), .. }) =
            state.draft.remote.as_mut()
        {
            *endpoint = "http://user:token@localhost:11434".to_string();
        }
        let secret = validate_embedding(&state);
        assert_eq!(secret.severity, Sev::Block);
        assert!(secret.message.unwrap().contains("credentials"));
    }

    #[test]
    fn embedding_connect_auth_env_must_exist_when_named() {
        let mut state = empty_state();
        let mut remote = new_connect_remote(&state.draft.model);
        remote.auth_env =
            Some(format!("__RAG_RAT_WIZARD_MISSING_AUTH_ENV_{}__", std::process::id()));
        state.draft.remote = Some(remote);

        let check = validate_embedding(&state);

        assert_eq!(check.severity, Sev::Block);
        assert!(check.message.unwrap().contains("auth env"));
    }

    #[test]
    fn embedding_ephemeral_auth_env_must_exist_when_named() {
        let mut state = empty_state();
        let mut remote = new_ephemeral_remote(&state.draft.model);
        remote.auth_env =
            Some(format!("__RAG_RAT_WIZARD_MISSING_EPHEMERAL_AUTH_ENV_{}__", std::process::id()));
        state.draft.remote = Some(remote);

        let check = validate_embedding(&state);

        assert_eq!(check.severity, Sev::Block);
        assert!(check.message.unwrap().contains("auth env"));
    }

    #[test]
    fn embedding_custom_cookbook_is_not_rendered_as_modal_selected() {
        let mut state = empty_state();
        let mut remote = new_ephemeral_remote(&state.draft.model);
        remote.mode = RemoteMode::Ephemeral("./recipes/custom.mts --provider test".to_string());
        state.draft.remote = Some(remote);
        state.step = Some(init_step(StepId::Embedding, &state));

        assert_eq!(selected_cookbook_idx(&state), Some(2));
        let cells = render_embedding_cells(&state, 100, 24);
        let screen = screen_text(&cells);

        assert!(!screen.contains("[*] Modal"));
        assert!(screen.contains("[*] Custom:"), "{screen}");
    }

    #[test]
    fn embedding_custom_catalog_cookbook_selection_writes_command() {
        let mut state = state_with_catalog(
            r#"
            [init.cookbooks.acme]
            label = "Acme GPU"
            command = "./recipes/acme.mjs --pool small"
            gpus = ["acme-small", "acme-large"]
            "#,
        );
        state.draft.remote = Some(new_ephemeral_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));

        while embed_focus(&state) != Some(EmbedFocus::Cookbook) {
            step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        }
        step_handle_key(StepId::Embedding, key(KeyCode::End), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        assert!(matches!(
            state.draft.remote.as_ref().map(|remote| &remote.mode),
            Some(RemoteMode::Ephemeral(command)) if command == "./recipes/acme.mjs --pool small"
        ));
        let cells = render_embedding_cells(&state, 100, 24);
        let screen = screen_text(&cells);
        assert!(screen.contains("Acme GPU"), "{screen}");
    }

    #[test]
    fn embedding_custom_catalog_gpu_list_drives_picker() {
        let mut state = state_with_catalog(
            r#"
            [init.cookbooks.modal]
            gpus = ["tiny", "huge"]
            "#,
        );
        state.draft.remote = Some(new_ephemeral_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));

        while embed_focus(&state) != Some(EmbedFocus::Gpu) {
            step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        }
        step_handle_key(StepId::Embedding, key(KeyCode::End), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

        assert_eq!(
            state.draft.remote.as_ref().and_then(|remote| remote.gpu.as_deref()),
            Some("huge")
        );
        let cells = render_embedding_cells(&state, 100, 24);
        let screen = screen_text(&cells);
        assert!(screen.contains("[*] huge"), "{screen}");
        assert!(!screen.contains("A10"), "{screen}");
    }

    #[test]
    fn embedding_field_edit_clears_stale_probe_success() {
        let mut state = empty_state();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));
        let probe_id = state.probes.current(StepId::Embedding);
        assert!(state.probes.apply(ProbeMsg {
            probe_id,
            kind: ProbeKind::ConnectTest,
            result: CheckResult::ok(),
        }));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char('/')), &mut state);

        assert!(matches!(state.probes.status(StepId::Embedding), ProbeStatus::Idle));
    }

    #[test]
    fn embedding_batch_arrows_clear_stale_probe_success() {
        let mut state = empty_state();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));
        let probe_id = state.probes.current(StepId::Embedding);
        assert!(state.probes.apply(ProbeMsg {
            probe_id,
            kind: ProbeKind::ConnectTest,
            result: CheckResult::ok(),
        }));
        while embed_focus(&state) != Some(EmbedFocus::BatchSize) {
            step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        }
        let before = state.draft.remote.as_ref().unwrap().batch_size;

        step_handle_key(StepId::Embedding, key(KeyCode::Up), &mut state);

        assert_eq!(state.draft.remote.as_ref().unwrap().batch_size, before.saturating_sub(1));
        assert!(matches!(state.probes.status(StepId::Embedding), ProbeStatus::Idle));
    }

    #[test]
    fn embedding_remote_tuning_fields_are_focusable_and_editable() {
        let mut state = empty_state();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));
        let probe_id = state.probes.current(StepId::Embedding);
        assert!(state.probes.apply(ProbeMsg {
            probe_id,
            kind: ProbeKind::ConnectTest,
            result: CheckResult::ok(),
        }));

        while embed_focus(&state) != Some(EmbedFocus::Concurrency) {
            step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        }
        step_handle_key(StepId::Embedding, key(KeyCode::Char('4')), &mut state);
        assert_eq!(state.draft.remote.as_ref().unwrap().concurrency, 14);
        assert!(matches!(state.probes.status(StepId::Embedding), ProbeStatus::Idle));

        while embed_focus(&state) != Some(EmbedFocus::MaxBatchChars) {
            step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        }
        step_handle_key(StepId::Embedding, key(KeyCode::Backspace), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Up), &mut state);
        assert_eq!(state.draft.remote.as_ref().unwrap().max_batch_chars, 38_399);

        let remote = remote_config_for(state.draft.remote.as_ref().unwrap());
        assert_eq!(remote.concurrency, 14);
        assert_eq!(remote.max_batch_chars, 38_399);
    }

    #[test]
    fn embedding_provision_ignores_duplicate_running_probe() {
        use std::sync::mpsc::sync_channel;

        let mut state = empty_state();
        state.draft.remote = Some(new_ephemeral_remote(&state.draft.model));
        state.ui.provision_confirm = PROVISION_CONFIRM_WORD.to_string();
        state.step = Some(init_step(StepId::Embedding, &state));
        let (release_tx, release_rx) = sync_channel::<()>(0);
        state.probes.spawn(StepId::Embedding, ProbeKind::EphemeralTest, move || {
            let _ = release_rx.recv();
            CheckResult::ok()
        });
        let before = state.probes.current(StepId::Embedding);

        assert_eq!(
            step_handle_key(StepId::Embedding, key(KeyCode::Char('p')), &mut state),
            Outcome::Consumed
        );

        assert_eq!(state.probes.current(StepId::Embedding), before);
        assert!(matches!(
            state.probes.status(StepId::Embedding),
            ProbeStatus::Running(ProbeKind::EphemeralTest)
        ));
        assert!(state.provision_log_lines.iter().any(|line| line.contains("already running")));
        let _ = release_tx.send(());
    }

    #[test]
    fn embedding_ephemeral_tab_order_visits_gpu_before_server_model() {
        let mut state = empty_state();
        state.step = Some(init_step(StepId::Embedding, &state));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        assert_eq!(embed_focus(&state), Some(EmbedFocus::Mode));

        step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);
        assert!(matches!(
            state.draft.remote.as_ref().map(|remote| &remote.mode),
            Some(RemoteMode::Ephemeral(_))
        ));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        assert_eq!(embed_focus(&state), Some(EmbedFocus::Cookbook));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        assert_eq!(embed_focus(&state), Some(EmbedFocus::Gpu));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        assert_eq!(embed_focus(&state), Some(EmbedFocus::ServerModel));
    }

    #[test]
    fn embedding_server_model_list_hides_incompatible_dimensions() {
        let mut state = empty_state();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));

        let cells = render_embedding_cells(&state, 100, 18);
        let screen = screen_text(&cells);

        assert!(screen.contains("all-minilm"), "{screen}");
        assert!(screen.contains("qllama/bge-small-en-v1.5:f16"), "{screen}");
        assert!(!screen.contains("nomic-embed-text"), "{screen}");
        assert!(!screen.contains("mxbai-embed-large"), "{screen}");

        state.draft.model = "jinaai/jina-embeddings-v2-base-code".to_string();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));

        let cells = render_embedding_cells(&state, 100, 18);
        let screen = screen_text(&cells);

        assert!(screen.contains("ordis/jina-embeddings-v2-base-code"), "{screen}");
        assert!(screen.contains("nomic-embed-text"), "{screen}");
        assert!(!screen.contains("all-minilm"), "{screen}");
        assert!(!screen.contains("mxbai-embed-large"), "{screen}");

        let hash_model =
            EMBEDDING_MODELS.iter().find(|spec| spec.backend == Backend::Hash).unwrap().model_id;
        assert!(
            compatible_server_models(hash_model).is_empty(),
            "unsupported local models must not expose remote server choices"
        );
    }

    #[test]
    fn embedding_gpu_list_scrolls_to_keep_cursor_visible() {
        let mut state = empty_state();
        state.draft.remote = Some(new_ephemeral_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));

        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        assert_eq!(embed_focus(&state), Some(EmbedFocus::Gpu));
        step_handle_key(StepId::Embedding, key(KeyCode::End), &mut state);

        let cells = render_embedding_cells(&state, 100, 24);
        let screen = screen_text(&cells);
        let expected = format!("> [ ] {}", MODAL_GPUS.last().unwrap());

        assert!(
            screen.contains(&expected),
            "selected GPU should stay visible when the list is taller than its box:\n{screen}"
        );
    }

    #[test]
    fn embedding_text_fields_accept_typing_and_backspace() {
        let mut state = empty_state();
        state.step = Some(init_step(StepId::Embedding, &state));
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);

        step_handle_key(StepId::Embedding, key(KeyCode::Char('/')), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Backspace), &mut state);

        let remote = state.draft.remote.as_ref().unwrap();
        assert!(
            matches!(remote.mode, RemoteMode::Connect(ref endpoint) if endpoint == DEFAULT_QUERY_ENDPOINT)
        );
    }

    #[test]
    fn embedding_focus_navigation_covers_selectors_and_numeric_fields() {
        let mut state = empty_state();
        state.draft.remote = Some(new_ephemeral_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));

        for code in [KeyCode::Down, KeyCode::End, KeyCode::Home, KeyCode::Char(' ')] {
            let _ = step_handle_key(StepId::Embedding, key(code), &mut state);
        }

        for expected in [
            EmbedFocus::Mode,
            EmbedFocus::Cookbook,
            EmbedFocus::Gpu,
            EmbedFocus::ServerModel,
            EmbedFocus::BatchSize,
            EmbedFocus::Concurrency,
            EmbedFocus::MaxBatchChars,
            EmbedFocus::AuthEnv,
            EmbedFocus::ProvisionConfirm,
        ] {
            step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
            assert_eq!(embed_focus(&state), Some(expected));
            let _ = step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
            let _ = step_handle_key(StepId::Embedding, key(KeyCode::End), &mut state);
            let _ = step_handle_key(StepId::Embedding, key(KeyCode::Home), &mut state);
        }

        assert_eq!(
            step_handle_key(StepId::Embedding, key(KeyCode::BackTab), &mut state),
            Outcome::Consumed
        );
        assert_eq!(embed_focus(&state), Some(EmbedFocus::AuthEnv));

        while embed_focus(&state) != Some(EmbedFocus::BatchSize) {
            step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        }
        step_handle_key(StepId::Embedding, key(KeyCode::Char('4')), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Backspace), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char('+')), &mut state);
        step_handle_key(StepId::Embedding, key(KeyCode::Char('-')), &mut state);
        assert!(state.draft.remote.as_ref().unwrap().batch_size >= 1);

        while embed_focus(&state) != Some(EmbedFocus::ProvisionConfirm) {
            step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
        }
        for c in PROVISION_CONFIRM_WORD.chars() {
            step_handle_key(StepId::Embedding, key(KeyCode::Char(c)), &mut state);
        }
        assert!(provision_confirm_satisfied(&state.ui));

        let remote = remote_config_for(state.draft.remote.as_ref().unwrap());
        assert_eq!(remote.cookbook.as_deref(), Some("@rag-rat/cookbook modal"));
    }

    #[test]
    fn embedding_model_help_covers_heavier_model_copy() {
        let mut state = empty_state();
        for needle in ["bge", "jina"] {
            let model =
                EMBEDDING_MODELS.iter().find(|s| s.display.contains(needle)).unwrap().model_id;
            state.draft.model = model.to_string();
            state.step = Some(init_step(StepId::Embedding, &state));

            let cells = render_embedding_cells(&state, 100, 24);
            let screen = screen_text(&cells);

            assert!(screen.contains("Model help"));
        }
    }

    #[test]
    fn embedding_endpoint_field_has_one_content_row() {
        let mut state = empty_state();
        state.draft.remote = Some(new_connect_remote(&state.draft.model));
        state.step = Some(init_step(StepId::Embedding, &state));

        let cells = render_embedding_cells(&state, 100, 24);
        let endpoint_y = cells
            .iter()
            .position(|row| row_text(row).contains("endpoint"))
            .expect("endpoint field");
        let after_endpoint = row_segment(&cells, endpoint_y + 3, 45, 55);

        assert!(row_text(&cells[endpoint_y + 1]).contains(DEFAULT_QUERY_ENDPOINT));
        assert!(
            after_endpoint.trim().is_empty(),
            "endpoint field should leave blank space after a single content row: \
             {after_endpoint:?}"
        );
    }

    #[test]
    fn embedding_remote_fields_have_one_content_row() {
        let mut state = empty_state();
        let mut remote = new_ephemeral_remote(&state.draft.model);
        remote.auth_env = Some("RR_AUTH".to_string());
        state.draft.remote = Some(remote);
        state.step = Some(init_step(StepId::Embedding, &state));

        let cells = render_embedding_cells(&state, 100, 24);
        let batch_y =
            cells.iter().position(|row| row_text(row).contains("batch")).expect("batch field");
        let content = row_text(&cells[batch_y + 1]);
        let bottom = row_segment(&cells, batch_y + 2, 45, 55);

        assert!(content.contains("256"));
        assert!(content.contains("32"));
        assert!(content.contains("384000"));
        assert!(content.contains("RR_AUTH"));
        assert!(bottom.contains('└'));
        assert!(bottom.contains('┘'));
        assert!(screen_text(&cells).contains("provision"));
    }

    #[test]
    fn embedding_model_help_describes_selected_model_tradeoffs() {
        let mut state = empty_state();
        state.draft.model = NONE_MODEL.to_string();
        state.step = Some(init_step(StepId::Embedding, &state));

        let cells = render_embedding_cells(&state, 100, 24);
        let screen = screen_text(&cells);

        assert!(screen.contains("Model help"));
        assert!(screen.contains("Not recommended for big codebases"));
    }

    #[test]
    fn integration_combines_version_check_and_hooks() {
        let mut state = empty_state();
        state.step = Some(init_step(StepId::Integration, &state));
        let initial_version_check = state.draft.version_check;

        step_handle_key(StepId::Integration, key(KeyCode::Char(' ')), &mut state);
        assert_eq!(state.draft.version_check, !initial_version_check);

        step_handle_key(StepId::Integration, key(KeyCode::Down), &mut state);
        step_handle_key(StepId::Integration, key(KeyCode::Char(' ')), &mut state);
        assert!(state.draft.hooks.git);

        step_handle_key(StepId::Integration, key(KeyCode::Char('c')), &mut state);
        assert!(state.draft.hooks.claude);
    }

    #[test]
    fn integration_blocks_unresolved_foreign_hook_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .arg("init")
            .arg(dir.path())
            .status()
            .expect("git init should run");
        assert!(status.success());
        let gp = git_paths(dir.path()).unwrap();
        std::fs::create_dir_all(&gp.hooks_dir).unwrap();
        let hook = MANAGED_HOOKS[0];
        std::fs::write(gp.hooks_dir.join(hook), "#!/bin/sh\necho custom\n").unwrap();
        let scan = scan_repo(dir.path()).unwrap();
        let mut draft = WizardDraft::from_scan(&scan, ".".to_string(), dir.path().to_path_buf());
        draft.hooks.git = true;
        let mut state = WizardState::new(draft, scan);

        let unresolved = validate_hooks(&state);
        state.hook_conflicts.insert(hook, hooks::HookConflict::Skip);
        let resolved = validate_hooks(&state);

        assert_eq!(unresolved.severity, Sev::Block);
        assert!(unresolved.message.unwrap().contains(hook));
        assert_eq!(resolved.severity, Sev::Ok);
    }

    #[test]
    fn integration_blocks_git_hooks_outside_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let scan = scan_repo(dir.path()).unwrap();
        let mut draft = WizardDraft::from_scan(&scan, ".".to_string(), dir.path().to_path_buf());
        draft.hooks.git = true;
        let state = WizardState::new(draft, scan);

        let check = validate_hooks(&state);

        assert_eq!(check.severity, Sev::Block);
        assert!(check.message.unwrap().contains("git worktree"));
    }
}
