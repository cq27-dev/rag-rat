//! Embedding step — model picker, remote mode/backend/server-model selectors, remote tuning.

use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;

use rag_rat_base::config::{
    DEFAULT_QUERY_ENDPOINT, MAX_REMOTE_EMBEDDING_CONCURRENCY, RemoteBackend, RemoteEmbeddingConfig,
    endpoint_authority_has_userinfo,
};
use rag_rat_base::embedding_models::{Backend, EMBEDDING_MODELS, EmbeddingModelSpec, spec};
#[cfg(feature = "fastembed")]
use rag_rat_core::index::ai::FastEmbedEmbedder;
#[cfg(feature = "model2vec")]
use rag_rat_core::index::ai::Model2VecEmbedder;
use rag_rat_core::index::ai::{
    Embedder, HashEmbedder, OpenAiEmbedder, verify_ephemeral_remote_cancellable,
};
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::super::catalog::CookbookEntry;
use super::super::draft::{
    OLLAMA_EMBEDDING_MODELS, RemoteDraft, RemoteMode, default_backend_endpoint,
    is_default_backend_endpoint, ollama_model_dim, ollama_model_for, wizard_query_endpoint,
};
use super::super::probe::{ProbeKind, ProbeStatus};
use super::super::state::{PROVISION_CONFIRM_WORD, WizardState, provision_confirm_satisfied};
use super::super::theme;
use super::oracle::send_log;
use super::types::{
    BACKENDS_BY_EFFICIENCY, CheckResult, EmbedFocus, ONE_LINE_FIELD_OUTER_HEIGHT, Outcome,
    REMOTE_BATCH_SIZE_MAX, StepId, StepState,
};

pub(super) const NONE_MODEL: &str = "none";

pub(super) fn model_rows() -> Vec<(String, String)> {
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

/// The default server-side model name for a given local model + remote backend.
///
/// `ollama` maps to a same-family curated Ollama build; infinity/vLLM download the HuggingFace
/// model, so the default is the selected local model's `model_id` (the HF id).
pub(super) fn default_remote_model_for(local_model: &str, backend: RemoteBackend) -> &'static str {
    if backend != RemoteBackend::Ollama {
        return spec(local_model).map_or("all-minilm", |s| s.model_id);
    }
    ollama_model_for(local_model).unwrap_or("all-minilm")
}

fn remote_mode(state: &WizardState) -> usize {
    match state.draft.remote.as_ref().map(|r| &r.mode) {
        Some(RemoteMode::Connect(_)) => 1,
        Some(RemoteMode::Ephemeral(_)) => 2,
        None => 0,
    }
}

/// The remote backend the draft currently carries (`Ollama` when no remote block is configured).
fn draft_backend(state: &WizardState) -> RemoteBackend {
    state.draft.remote.as_ref().map_or(RemoteBackend::Ollama, |r| r.backend)
}

/// Re-sync the Embedding step's `backend_cursor` to the position of the draft's current backend in
/// `BACKENDS_BY_EFFICIENCY` — mirrors how `init_embedding_step` seeds it. Call after any change
/// that replaces `state.draft.remote` (a mode switch), so the picker cursor tracks the selected
/// backend.
fn sync_backend_cursor(state: &mut WizardState) {
    let backend = draft_backend(state);
    let cursor = BACKENDS_BY_EFFICIENCY.iter().position(|&b| b == backend).unwrap_or(0);
    if let Some(StepState::Embedding { backend_cursor, .. }) = &mut state.step {
        *backend_cursor = cursor;
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

pub(super) fn selected_cookbook_idx(state: &WizardState) -> Option<usize> {
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

/// Initialize the Embedding step's cursor/scroll state from the current draft.
pub(super) fn init_embedding_step(state: &WizardState) -> StepState {
    let rows = model_rows();
    let model_cursor = rows.iter().position(|(id, _)| id == &state.draft.model).unwrap_or(0);
    let mode_cursor = remote_mode(state);
    let backend = draft_backend(state);
    let backend_cursor = BACKENDS_BY_EFFICIENCY.iter().position(|&b| b == backend).unwrap_or(0);
    let cookbook_cursor = selected_cookbook_idx(state).unwrap_or(0);
    let server_model = state
        .draft
        .remote
        .as_ref()
        .map(|r| r.model.as_str())
        .unwrap_or_else(|| default_remote_model_for(&state.draft.model, backend));
    let server_models = compatible_server_models(&state.draft.model, backend);
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
        backend_cursor,
        cookbook_cursor,
        server_model_cursor,
        gpu_cursor,
        model_scroll: model_cursor.saturating_sub(4),
        server_model_scroll: server_model_cursor.saturating_sub(4),
        focus: EmbedFocus::Model,
    }
}

/// The server-side model names the user may pick for a given local model + remote backend.
///
/// For `ollama` this is the curated, dimension-compatible Ollama model list. For infinity/vLLM
/// the server downloads the HuggingFace model directly, so the only valid server-side name is the
/// selected local model's `model_id` (the HF id) — a single-entry list.
pub(super) fn compatible_server_models(
    local_model: &str,
    backend: RemoteBackend,
) -> Vec<&'static str> {
    let Some(local) = spec(local_model) else {
        return Vec::new();
    };
    if local.backend != Backend::FastEmbed {
        return Vec::new();
    };
    if backend != RemoteBackend::Ollama {
        return vec![local.model_id];
    }
    OLLAMA_EMBEDDING_MODELS
        .iter()
        .copied()
        .filter(|model| ollama_model_dim(model).is_none_or(|dim| dim == local.dim))
        .collect()
}

pub(super) fn render_embedding(f: &mut Frame, area: Rect, state: &WizardState) {
    let Some(StepState::Embedding {
        model_cursor,
        mode_cursor,
        backend_cursor,
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

    // Backend + server-model row: the backend picker (efficiency-ordered) drives what a server
    // model name means, so it sits immediately left of the server-model list. Shown in BOTH
    // connect and ephemeral modes — the backend selects the embeddings route in either.
    let backend = draft_backend(state);
    // Backend names are short (`infinity`/`vllm`/`ollama`), so a fixed narrow column leaves the
    // server-model list its full width (its HF ids / ollama names are the long strings).
    let picker = Layout::horizontal([Constraint::Length(16), Constraint::Min(0)]).split(right[2]);
    let backend_items: Vec<ListItem> = BACKENDS_BY_EFFICIENCY
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let cursor = if i == *backend_cursor { ">" } else { " " };
            let selected = if *b == backend { "*" } else { " " };
            let style = if i == *backend_cursor { theme::selected() } else { theme::base() };
            ListItem::new(format!("{cursor} [{selected}] {}", b.as_db_str())).style(style)
        })
        .collect();
    f.render_widget(
        List::new(backend_items)
            .style(theme::base())
            .block(theme::block("backend").border_style(dim(EmbedFocus::Backend))),
        picker[0],
    );

    // vLLM needs a GPU and rejects any chunk over the model's context. The relevant knob is the
    // per-text cap `[llm.embedding.runtime] max_embedding_chars` (NOT the wizard's max_batch_chars,
    // which is a batch TOTAL) — surface a short, actionable note.
    let server_area = if backend == RemoteBackend::Vllm {
        let split = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).split(picker[1]);
        f.render_widget(
            Paragraph::new(
                "vLLM: GPU required; rejects chunks past model context. Lower [runtime] \
                 max_embedding_chars or use a long-context model.",
            )
            .style(theme::base())
            .wrap(Wrap { trim: true }),
            split[1],
        );
        split[0]
    } else {
        picker[1]
    };

    let server_models = compatible_server_models(&state.draft.model, backend);
    render_server_model_list(
        f,
        server_area,
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
    // The model's context window (in tokens) — the axis this help now leads with, because a short
    // window silently truncates long code chunks and costs precision/recall.
    let window = s.max_tokens.map(|t| format!(", {t}-token window")).unwrap_or_default();
    let guidance = match s.backend {
        Backend::Hash =>
            "Dependency-free fallback; very fast, but weak semantic recall. Not recommended for \
             big codebases.",
        Backend::FastEmbed if s.display.contains("MiniLM") =>
            "Good default for general text, but its 256-token window TRUNCATES long functions — \
             their tail is not embedded, so it loses precision/recall on large code chunks. For \
             code-heavy repos prefer jina (8192 tokens), which embeds whole chunks.",
        Backend::FastEmbed if s.display.contains("bge") =>
            "General-purpose 384d model with a 512-token window — a bit more context than MiniLM, \
             but still truncates long code. Useful for comparison.",
        Backend::FastEmbed if s.display.contains("jina") =>
            "Code-focused 768d model with an 8192-token window: embeds WHOLE functions with no \
             truncation (unlike MiniLM's 256), so it keeps precision/recall on long chunks. \
             Heavier storage + reconcile — the best fit for code.",
        Backend::FastEmbed =>
            "Local fastembed model. Good when you want local semantic search without a remote \
             server.",
        Backend::Model2Vec =>
            "Small and fast local model (mean-pooled, no token limit). Use when speed matters more \
             than maximum semantic recall.",
        Backend::Ollama =>
            "Remote runtime. Use only with a configured Ollama endpoint or ephemeral provider.",
    };
    vec![
        Line::from(format!(
            "{}: {weight}, {}d, {}{window}.",
            s.display,
            s.dim,
            s.backend.runtime()
        )),
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

pub(super) fn handle_embedding(key: KeyEvent, state: &mut WizardState) -> Outcome {
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

pub(super) fn embed_focus(state: &WizardState) -> Option<EmbedFocus> {
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
        EmbedFocus::Backend,
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
        EmbedFocus::Backend,
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
    let backend = draft_backend(state);
    let server_model_len = if focus == EmbedFocus::ServerModel {
        compatible_server_models(&state.draft.model, backend).len()
    } else {
        0
    };
    if let Some(StepState::Embedding {
        model_cursor,
        mode_cursor,
        backend_cursor,
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
            EmbedFocus::Backend => {
                let last = BACKENDS_BY_EFFICIENCY.len().saturating_sub(1);
                *backend_cursor = (*backend_cursor as isize)
                    .saturating_add(delta)
                    .clamp(0, last as isize) as usize;
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
    let backend = draft_backend(state);
    let server_model_len = if focus == EmbedFocus::ServerModel {
        compatible_server_models(&state.draft.model, backend).len()
    } else {
        0
    };
    let cookbook_len = cookbook_choices(state).len();
    let gpu_len = current_gpu_options(state).len();
    let target = match focus {
        EmbedFocus::Model => model_rows().len().saturating_sub(1),
        EmbedFocus::Mode => 2,
        EmbedFocus::Backend => BACKENDS_BY_EFFICIENCY.len().saturating_sub(1),
        EmbedFocus::Cookbook => cookbook_len.saturating_sub(1),
        EmbedFocus::ServerModel => server_model_len.saturating_sub(1),
        EmbedFocus::Gpu => gpu_len.saturating_sub(1),
        _ => 0,
    };
    if let Some(StepState::Embedding {
        model_cursor,
        mode_cursor,
        backend_cursor,
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
            EmbedFocus::Backend => *backend_cursor = value,
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

pub(super) fn scroll_embedding(delta: isize, state: &mut WizardState) -> bool {
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
                let backend = draft_backend(state);
                let previous_default =
                    default_remote_model_for(&state.draft.model, backend).to_string();
                changed |= state.draft.model != *id;
                state.draft.model = id.clone();
                let default_model =
                    default_remote_model_for(&state.draft.model, backend).to_string();
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
                // A new remote may default to a different backend than the cursor points at (e.g.
                // ephemeral defaults to infinity while the fresh cursor is on ollama). Re-sync the
                // backend_cursor so the picker `>` sits on the actually-selected backend — else
                // Space on Backend would silently flip to whatever the stale cursor pointed at.
                sync_backend_cursor(state);
                changed = true;
            }
            state.ui.show_remote_mode_help_once(cursor);
        },
        EmbedFocus::Backend => {
            let cursor = match &state.step {
                Some(StepState::Embedding { backend_cursor, .. }) => *backend_cursor,
                _ => 0,
            };
            let selected = BACKENDS_BY_EFFICIENCY.get(cursor).copied().unwrap_or_default();
            if let Some(remote) = &mut state.draft.remote
                && remote.backend != selected
            {
                remote.backend = selected;
                // The server-side model NAME differs by backend (an ollama name vs the HF id), so
                // reset it to the new backend's default rather than leave a mismatched name.
                remote.model = default_remote_model_for(&state.draft.model, selected).to_string();
                // Keep a wizard-default connect endpoint coherent with the new backend's
                // route/port; a custom endpoint the user typed is left alone.
                if let RemoteMode::Connect(ep) = &mut remote.mode
                    && is_default_backend_endpoint(ep)
                {
                    *ep = default_backend_endpoint(selected).to_string();
                }
                // Track the ephemeral query_endpoint to the new backend when it is unset or a known
                // wizard default; a custom value the user set is left alone.
                if remote.query_endpoint.as_deref().is_none_or(is_default_backend_endpoint) {
                    remote.query_endpoint =
                        wizard_query_endpoint(&remote.mode, selected).map(str::to_string);
                }
                changed = true;
            }
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
            let server_models = compatible_server_models(&state.draft.model, draft_backend(state));
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

pub(super) fn new_connect_remote(local_model: &str) -> RemoteDraft {
    // Connect default = the common existing local server (Ollama).
    let backend = RemoteBackend::Ollama;
    RemoteDraft {
        model: default_remote_model_for(local_model, backend).to_string(),
        backend,
        mode: RemoteMode::Connect(DEFAULT_QUERY_ENDPOINT.to_string()),
        // CONNECT ignores query_endpoint (queries hit the endpoint directly).
        query_endpoint: None,
        gpu: None,
        num_ctx: None,
        batch_size: 256,
        concurrency: RemoteEmbeddingConfig::omitted_concurrency_default(true),
        max_batch_chars: RemoteEmbeddingConfig::default().max_batch_chars,
        auth_env: None,
    }
}

pub(super) fn new_connect_remote_from(
    local_model: &str,
    existing: Option<&RemoteDraft>,
) -> RemoteDraft {
    let mut remote = new_connect_remote(local_model);
    if let Some(existing) = existing {
        // Carry the previously chosen backend across a mode switch.
        remote.backend = existing.backend;
        remote.model = preserved_server_model(local_model, existing);
        // Pick the connect endpoint in priority order: (a) a CUSTOM connect endpoint the user
        // typed; (b) a CUSTOM ephemeral query_endpoint (the user's reachable LOCAL query server —
        // switching ephemeral→connect should reuse it, not fall back to a localhost default); (c)
        // the (preserved) backend's default local endpoint — so a mode switch never leaves e.g. an
        // infinity backend pointed at the ollama default port.
        let connect_endpoint = match &existing.mode {
            RemoteMode::Connect(ep) if !is_default_backend_endpoint(ep) => ep.clone(),
            _ => match &existing.query_endpoint {
                Some(qe) if !is_default_backend_endpoint(qe) => qe.clone(),
                _ => default_backend_endpoint(remote.backend).to_string(),
            },
        };
        remote.mode = RemoteMode::Connect(connect_endpoint);
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
pub(super) fn new_ephemeral_remote(local_model: &str) -> RemoteDraft {
    new_ephemeral_remote_with_command(local_model, "@rag-rat/cookbook modal")
}

fn new_ephemeral_remote_with_command(local_model: &str, cookbook: &str) -> RemoteDraft {
    // Ephemeral default = the fastest measured backend (infinity, ~1517 texts/s on L4 vs vLLM
    // ~1029 and ollama ~299 for all-MiniLM).
    let backend = RemoteBackend::Infinity;
    let mode = RemoteMode::Ephemeral(cookbook.to_string());
    RemoteDraft {
        model: default_remote_model_for(local_model, backend).to_string(),
        backend,
        // The backend's default LOCAL query endpoint (None for ollama → config default 11434).
        query_endpoint: wizard_query_endpoint(&mode, backend).map(str::to_string),
        mode,
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
        // Carry the previously chosen backend across a mode switch.
        remote.backend = existing.backend;
        remote.model = preserved_server_model(local_model, existing);
        // Carry a CUSTOM query_endpoint the user set; otherwise recompute the (preserved) backend's
        // default — a value left over from a different backend would point queries at the wrong
        // local server.
        remote.query_endpoint = match &existing.query_endpoint {
            Some(qe) if !is_default_backend_endpoint(qe) => Some(qe.clone()),
            _ => wizard_query_endpoint(&remote.mode, remote.backend).map(str::to_string),
        };
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
        default_remote_model_for(local_model, existing.backend).to_string()
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

pub(super) fn validate_embedding(state: &WizardState) -> CheckResult {
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

pub(super) fn remote_config_for(d: &RemoteDraft) -> RemoteEmbeddingConfig {
    let (ep, cb) = match &d.mode {
        RemoteMode::Connect(u) => (Some(u.clone()), None),
        RemoteMode::Ephemeral(c) => (None, Some(c.clone())),
    };
    RemoteEmbeddingConfig {
        model: d.model.clone(),
        backend: d.backend,
        endpoint: ep,
        cookbook: cb,
        // The draft is the single source of truth: for EPHEMERAL it carries the backend default
        // (or a preserved custom value); for CONNECT it is `None` (queries hit the endpoint).
        query_endpoint: d.query_endpoint.clone(),
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
