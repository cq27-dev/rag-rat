use std::path::PathBuf;

use rag_rat_core::config::{DEFAULT_QUERY_ENDPOINT, RemoteBackend, RemoteEmbeddingConfig};
use rag_rat_core::embedding_models::{Backend, EMBEDDING_MODELS};
use rag_rat_core::language::Language;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tempfile::TempDir;

use super::super::draft::{MODAL_GPUS, RemoteDraft, RemoteMode, WizardDraft};
use super::super::probe::{ProbeKind, ProbeMsg, ProbeStatus};
use super::super::state::{PROVISION_CONFIRM_WORD, WizardState, provision_confirm_satisfied};
use super::dispatch::{init_step, render_step, step_handle_key, validate_step};
use super::embedding::{
    NONE_MODEL, compatible_server_models, default_remote_model_for, embed_focus, handle_embedding,
    model_rows, new_connect_remote, new_connect_remote_from, new_ephemeral_remote,
    remote_config_for, render_embedding, selected_cookbook_idx, validate_embedding,
};
use super::hooks;
use super::indexing::{candidates_for, render_indexing, selected_dir_path, validate_indexing};
use super::integration::validate_hooks;
use super::oracle::{handle_oracle, render_oracle};
use super::types::{
    BACKENDS_BY_EFFICIENCY, CheckResult, EmbedFocus, IndexZone, Outcome, Sev, StepId, StepState,
};
use crate::init::RepoScan;
use crate::init::scan::scan_repo;
use crate::init::wizard::catalog::CookbookCatalog;
use crate::{MANAGED_HOOKS, git_paths};

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

    (0..height).map(|y| (0..width).map(|x| buffer[(x, y)].symbol().to_string()).collect()).collect()
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

    assert!(!state.draft.bindings.get(&Language::Rust).unwrap().contains(&PathBuf::from("src")));
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

    assert_eq!(step_handle_key(StepId::Indexing, key(KeyCode::Tab), &mut state), Outcome::Consumed);
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
    assert_eq!(remote.model, default_remote_model_for(&state.draft.model, RemoteBackend::Ollama));
    assert_eq!(remote.concurrency, RemoteEmbeddingConfig::omitted_concurrency_default(true));
    assert_eq!(compatible_server_models(&state.draft.model, RemoteBackend::Ollama), vec![
        "all-minilm",
        "qllama/bge-small-en-v1.5:f16"
    ]);

    // Focus is on Mode; tab past Endpoint + Backend to reach the server-model list.
    while embed_focus(&state) != Some(EmbedFocus::ServerModel) {
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    }
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
        .position(|(id, _)| {
            default_remote_model_for(id, RemoteBackend::Ollama) != original_server_model
        })
        .expect("registry should contain a model with a different remote default");
    if let Some(StepState::Embedding { model_cursor, .. }) = &mut state.step {
        *model_cursor = target_cursor;
    }

    step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

    let remote = state.draft.remote.as_ref().unwrap();
    assert_ne!(remote.model, original_server_model);
    assert_eq!(remote.model, default_remote_model_for(&state.draft.model, RemoteBackend::Ollama));
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
fn embedding_ephemeral_defaults_to_infinity_and_connect_to_ollama() {
    // Ephemeral mode defaults to the fastest measured backend (infinity); connect defaults to
    // the common local server (ollama).
    let ephemeral = new_ephemeral_remote("sentence-transformers/all-MiniLM-L6-v2");
    assert_eq!(ephemeral.backend, RemoteBackend::Infinity);
    // Infinity downloads the HuggingFace model → server model = the local model_id.
    assert_eq!(ephemeral.model, "sentence-transformers/all-MiniLM-L6-v2");

    let connect = new_connect_remote("sentence-transformers/all-MiniLM-L6-v2");
    assert_eq!(connect.backend, RemoteBackend::Ollama);
    // Ollama uses its own model name.
    assert_eq!(connect.model, "all-minilm");
}

#[test]
fn connect_from_ephemeral_infinity_keeps_backend_with_matching_endpoint() {
    // A mode switch ephemeral→connect PRESERVES the backend, so the connect endpoint default
    // must MATCH it (infinity → 7997), not fall back to the ollama default (11434) — otherwise
    // the connect probe posts infinity's route to a local ollama server.
    let existing = new_ephemeral_remote("sentence-transformers/all-MiniLM-L6-v2");
    assert_eq!(existing.backend, RemoteBackend::Infinity, "ephemeral defaults to infinity");
    let connect =
        new_connect_remote_from("sentence-transformers/all-MiniLM-L6-v2", Some(&existing));
    assert_eq!(connect.backend, RemoteBackend::Infinity);
    assert!(
        matches!(connect.mode, RemoteMode::Connect(ref ep) if ep == "http://localhost:7997"),
        "connect endpoint must match the preserved backend, got {:?}",
        connect.mode,
    );
}

#[test]
fn embedding_backend_pick_switches_to_infinity_and_sets_hf_model() {
    // Start from a connect (ollama) remote whose server model is the ollama name.
    let mut state = empty_state();
    state.draft.remote = Some(new_connect_remote(&state.draft.model));
    assert_eq!(state.draft.remote.as_ref().unwrap().model, "all-minilm");
    state.step = Some(init_step(StepId::Embedding, &state));

    // Focus the backend picker; cursor 0 is infinity (efficiency-ordered).
    while embed_focus(&state) != Some(EmbedFocus::Backend) {
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    }
    step_handle_key(StepId::Embedding, key(KeyCode::Home), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

    let remote = state.draft.remote.as_ref().unwrap();
    assert_eq!(remote.backend, RemoteBackend::Infinity);
    // Switching to infinity must reset the server model to the HF id, not leave an ollama name.
    assert_eq!(remote.model, "sentence-transformers/all-MiniLM-L6-v2");
    assert_ne!(remote.model, "all-minilm");

    // The persisted config carries the picked backend through.
    let config = remote_config_for(remote);
    assert_eq!(config.backend, RemoteBackend::Infinity);
}

#[test]
fn connect_from_ephemeral_custom_query_endpoint_uses_it_as_connect_endpoint() {
    // Switching an EPHEMERAL config that had a CUSTOM query_endpoint (the user's reachable
    // LOCAL query server) to CONNECT must reuse that endpoint, not fall back to the backend's
    // localhost default. The backend is preserved across the switch.
    let mut existing = new_ephemeral_remote("sentence-transformers/all-MiniLM-L6-v2");
    assert_eq!(existing.backend, RemoteBackend::Infinity);
    existing.query_endpoint = Some("http://gpu-box.local:9999".to_string());

    let connect =
        new_connect_remote_from("sentence-transformers/all-MiniLM-L6-v2", Some(&existing));

    assert_eq!(connect.backend, RemoteBackend::Infinity, "backend must be preserved");
    assert!(
        matches!(connect.mode, RemoteMode::Connect(ref ep) if ep == "http://gpu-box.local:9999"),
        "custom ephemeral query_endpoint must become the connect endpoint, got {:?}",
        connect.mode,
    );
}

#[test]
fn mode_select_ephemeral_syncs_backend_cursor_to_infinity() {
    // A fresh wizard seeds backend_cursor from the ollama default (position of Ollama in
    // BACKENDS_BY_EFFICIENCY). Selecting Ephemeral mode creates a remote defaulting to Infinity
    // — the cursor MUST move to Infinity so an immediate Space on Backend keeps infinity,
    // rather than silently re-selecting whatever the stale cursor pointed at (ollama).
    let mut state = empty_state();
    state.step = Some(init_step(StepId::Embedding, &state));

    // Tab to Mode, cursor down to ephemeral (index 2), Space to select.
    step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    assert_eq!(embed_focus(&state), Some(EmbedFocus::Mode));
    step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);
    assert_eq!(state.draft.remote.as_ref().unwrap().backend, RemoteBackend::Infinity);

    // The cursor must now sit on Infinity (position 0 in BACKENDS_BY_EFFICIENCY).
    let cursor = match &state.step {
        Some(StepState::Embedding { backend_cursor, .. }) => *backend_cursor,
        _ => panic!("expected embedding step"),
    };
    assert_eq!(BACKENDS_BY_EFFICIENCY[cursor], RemoteBackend::Infinity);

    // Focus Backend and press Space immediately: the backend must STAY infinity, not flip.
    while embed_focus(&state) != Some(EmbedFocus::Backend) {
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    }
    step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);
    assert_eq!(
        state.draft.remote.as_ref().unwrap().backend,
        RemoteBackend::Infinity,
        "Space on Backend right after selecting ephemeral must keep infinity, not flip to ollama",
    );
}

#[test]
fn backend_pick_tracks_ephemeral_query_endpoint_to_new_backend() {
    // On an ephemeral config, switching the backend must re-point the wizard-default
    // query_endpoint at the new backend's local port (a stale value points queries at the wrong
    // server). Start on infinity (7997), switch to vLLM (8000).
    let mut state = empty_state();
    state.draft.remote = Some(new_ephemeral_remote(&state.draft.model));
    assert_eq!(
        state.draft.remote.as_ref().unwrap().query_endpoint.as_deref(),
        Some("http://localhost:7997"),
    );
    state.step = Some(init_step(StepId::Embedding, &state));

    while embed_focus(&state) != Some(EmbedFocus::Backend) {
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    }
    // Cursor 1 is vLLM in BACKENDS_BY_EFFICIENCY = [Infinity, Vllm, Ollama].
    step_handle_key(StepId::Embedding, key(KeyCode::Home), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

    let remote = state.draft.remote.as_ref().unwrap();
    assert_eq!(remote.backend, RemoteBackend::Vllm);
    assert_eq!(remote.query_endpoint.as_deref(), Some("http://localhost:8000"));

    // A CUSTOM query_endpoint is preserved across a backend switch.
    state.draft.remote.as_mut().unwrap().query_endpoint =
        Some("http://gpu-box.local:9999".to_string());
    step_handle_key(StepId::Embedding, key(KeyCode::Home), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state); // → infinity
    assert_eq!(state.draft.remote.as_ref().unwrap().backend, RemoteBackend::Infinity);
    assert_eq!(
        state.draft.remote.as_ref().unwrap().query_endpoint.as_deref(),
        Some("http://gpu-box.local:9999"),
        "a custom query_endpoint must survive a backend switch",
    );
}

#[test]
fn embedding_backend_pick_switches_to_vllm() {
    let mut state = empty_state();
    state.draft.remote = Some(new_connect_remote(&state.draft.model));
    state.step = Some(init_step(StepId::Embedding, &state));

    while embed_focus(&state) != Some(EmbedFocus::Backend) {
        step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    }
    // Cursor 1 is vLLM in BACKENDS_BY_EFFICIENCY = [Infinity, Vllm, Ollama].
    step_handle_key(StepId::Embedding, key(KeyCode::Home), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);

    let remote = state.draft.remote.as_ref().unwrap();
    assert_eq!(remote.backend, RemoteBackend::Vllm);
    assert_eq!(remote.model, "sentence-transformers/all-MiniLM-L6-v2");
    assert_eq!(compatible_server_models(&state.draft.model, RemoteBackend::Vllm), vec![
        "sentence-transformers/all-MiniLM-L6-v2"
    ]);
}

#[test]
fn embedding_backend_shows_in_connect_mode_and_surfaces_vllm_caveat() {
    let mut state = empty_state();
    let mut remote = new_connect_remote(&state.draft.model);
    remote.backend = RemoteBackend::Vllm;
    remote.model = "sentence-transformers/all-MiniLM-L6-v2".to_string();
    state.draft.remote = Some(remote);
    state.step = Some(init_step(StepId::Embedding, &state));

    let cells = render_embedding_cells(&state, 120, 24);
    let screen = screen_text(&cells);
    // The backend picker renders in connect mode too.
    assert!(screen.contains("backend"), "{screen}");
    assert!(screen.contains("[*] vllm"), "{screen}");
    // The vLLM caveat is surfaced near the picker and points at the ACTUAL per-text knob
    // (`max_embedding_chars`), not the wizard's batch-total `max_batch_chars`.
    assert!(screen.contains("max_embedding_chars"), "{screen}");
}

#[test]
fn embedding_backend_pick_visits_before_server_model() {
    let mut state = empty_state();
    state.step = Some(init_step(StepId::Embedding, &state));

    step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    assert_eq!(embed_focus(&state), Some(EmbedFocus::Mode));
    step_handle_key(StepId::Embedding, key(KeyCode::Down), &mut state);
    step_handle_key(StepId::Embedding, key(KeyCode::Char(' ')), &mut state);
    assert!(matches!(
        state.draft.remote.as_ref().map(|remote| &remote.mode),
        Some(RemoteMode::Connect(_))
    ));

    step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    assert_eq!(embed_focus(&state), Some(EmbedFocus::Endpoint));
    step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    assert_eq!(embed_focus(&state), Some(EmbedFocus::Backend));
    step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    assert_eq!(embed_focus(&state), Some(EmbedFocus::ServerModel));
}

#[test]
fn embedding_mode_reselect_preserves_remote_settings() {
    let mut state = empty_state();
    state.draft.remote = Some(RemoteDraft {
        model: "custom-model".to_string(),
        backend: RemoteBackend::Ollama,
        mode: RemoteMode::Connect("http://remote:11434".to_string()),
        query_endpoint: None,
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
    remote.auth_env = Some(format!("__RAG_RAT_WIZARD_MISSING_AUTH_ENV_{}__", std::process::id()));
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

    assert_eq!(state.draft.remote.as_ref().and_then(|remote| remote.gpu.as_deref()), Some("huge"));
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
    assert_eq!(embed_focus(&state), Some(EmbedFocus::Backend));

    step_handle_key(StepId::Embedding, key(KeyCode::Tab), &mut state);
    assert_eq!(embed_focus(&state), Some(EmbedFocus::ServerModel));
}

#[test]
fn embedding_server_model_list_hides_incompatible_dimensions() {
    let mut state = empty_state();
    state.draft.remote = Some(new_connect_remote(&state.draft.model));
    state.step = Some(init_step(StepId::Embedding, &state));

    // Wide enough for the full server-model names beside the (fixed-width) backend picker.
    let cells = render_embedding_cells(&state, 130, 18);
    let screen = screen_text(&cells);

    assert!(screen.contains("all-minilm"), "{screen}");
    assert!(screen.contains("qllama/bge-small-en-v1.5:f16"), "{screen}");
    assert!(!screen.contains("nomic-embed-text"), "{screen}");
    assert!(!screen.contains("mxbai-embed-large"), "{screen}");

    state.draft.model = "jinaai/jina-embeddings-v2-base-code".to_string();
    state.draft.remote = Some(new_connect_remote(&state.draft.model));
    state.step = Some(init_step(StepId::Embedding, &state));

    let cells = render_embedding_cells(&state, 130, 18);
    let screen = screen_text(&cells);

    assert!(screen.contains("ordis/jina-embeddings-v2-base-code"), "{screen}");
    assert!(screen.contains("nomic-embed-text"), "{screen}");
    assert!(!screen.contains("all-minilm"), "{screen}");
    assert!(!screen.contains("mxbai-embed-large"), "{screen}");

    let hash_model =
        EMBEDDING_MODELS.iter().find(|spec| spec.backend == Backend::Hash).unwrap().model_id;
    assert!(
        compatible_server_models(hash_model, RemoteBackend::Ollama).is_empty(),
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
        EmbedFocus::Backend,
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
        let model = EMBEDDING_MODELS.iter().find(|s| s.display.contains(needle)).unwrap().model_id;
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
    let endpoint_y =
        cells.iter().position(|row| row_text(row).contains("endpoint")).expect("endpoint field");
    let after_endpoint = row_segment(&cells, endpoint_y + 3, 45, 55);

    assert!(row_text(&cells[endpoint_y + 1]).contains(DEFAULT_QUERY_ENDPOINT));
    assert!(
        after_endpoint.trim().is_empty(),
        "endpoint field should leave blank space after a single content row: {after_endpoint:?}"
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

    step_handle_key(StepId::Integration, key(KeyCode::Char('g')), &mut state);
    assert!(!state.draft.hooks.git);
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
