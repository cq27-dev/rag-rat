//! Step-state enums and shared result/state types.
//!
//! Per-step mutable state lives in [`StepState`], stored on
//! [`WizardState`](super::super::state::WizardState).

use std::collections::BTreeMap;
use std::path::PathBuf;

use rag_rat_core::config::RemoteBackend;
use rag_rat_core::language::Language;
use tui_tree_widget::TreeState;

use crate::init::DirCandidate;

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

pub(super) const ONE_LINE_FIELD_OUTER_HEIGHT: u16 = 3;
pub(super) const REMOTE_BATCH_SIZE_MAX: u32 = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EmbedFocus {
    Model,
    Mode,
    Endpoint,
    Cookbook,
    Backend,
    ServerModel,
    Gpu,
    BatchSize,
    Concurrency,
    MaxBatchChars,
    AuthEnv,
    ProvisionConfirm,
}

/// The remote embedding backends, ordered most-efficient-first.
///
/// Order (and the ephemeral default) reflect a measured L4-GPU throughput benchmark on
/// all-MiniLM-L6-v2: infinity ~1517 texts/s > vLLM ~1029 > ollama ~299. The picker lists them in
/// this order and ephemeral mode defaults to the fastest (infinity).
pub(super) const BACKENDS_BY_EFFICIENCY: [RemoteBackend; 3] =
    [RemoteBackend::Infinity, RemoteBackend::Vllm, RemoteBackend::Ollama];

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
        backend_cursor: usize,
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
