//! WizardState and WizardUiState — minimal state container.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, TryRecvError};

use super::catalog::CookbookCatalog;
use super::draft::WizardDraft;
use super::probe::ProbeRegistry;
use super::steps::hooks::HookConflict;
use super::steps::{CheckResult, StepId, StepState};
use crate::init::RepoScan;

pub(crate) const PROVISION_CONFIRM_WORD: &str = "provision";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OneShotHelp {
    EmbeddingOff,
    Connect,
    Ephemeral,
}

impl OneShotHelp {
    fn for_remote_mode(mode: usize) -> Option<Self> {
        match mode {
            0 => Some(Self::EmbeddingOff),
            1 => Some(Self::Connect),
            2 => Some(Self::Ephemeral),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WizardUiState {
    pub focused: StepId,
    pub tab: usize,
    pub help_visible: bool,
    pub popup: Option<OneShotHelp>,
    pub remote_mode_help_seen: [bool; 3],
    pub review_scroll: u16,
    pub provision_log_open: bool,
    pub provision_log_scroll: u16,
    pub provision_log_follow: bool,
    pub provision_confirm: String,
    pub ephemeral_keep_acknowledged: bool,
}

impl WizardUiState {
    pub(crate) fn new() -> Self {
        Self {
            focused: StepId::Indexing,
            tab: 0,
            help_visible: false,
            popup: None,
            remote_mode_help_seen: [false; 3],
            review_scroll: 0,
            provision_log_open: false,
            provision_log_scroll: 0,
            provision_log_follow: true,
            provision_confirm: String::new(),
            ephemeral_keep_acknowledged: false,
        }
    }

    pub(crate) fn show_remote_mode_help_once(&mut self, mode: usize) {
        let Some(help) = OneShotHelp::for_remote_mode(mode) else { return };
        if let Some(seen) = self.remote_mode_help_seen.get_mut(mode)
            && !*seen
        {
            *seen = true;
            self.popup = Some(help);
        }
    }
}

pub(crate) fn provision_confirm_satisfied(ui: &WizardUiState) -> bool {
    ui.provision_confirm.trim() == PROVISION_CONFIRM_WORD
}

pub(crate) struct WizardState {
    pub draft: WizardDraft,
    pub cookbooks: CookbookCatalog,
    pub ui: WizardUiState,
    pub scan: RepoScan,
    pub probes: ProbeRegistry,
    pub checks: Vec<CheckResult>,
    pub hook_conflicts: HashMap<&'static str, HookConflict>,
    pub step: Option<StepState>,
    pub provision_log_rx: Option<Receiver<String>>,
    pub provision_log_lines: Vec<String>,
    pub provision_log_title: String,
}

impl WizardState {
    #[cfg(test)]
    pub(crate) fn new(draft: WizardDraft, scan: RepoScan) -> Self {
        Self::with_cookbooks(draft, scan, CookbookCatalog::default())
    }

    pub(crate) fn with_cookbooks(
        draft: WizardDraft,
        scan: RepoScan,
        cookbooks: CookbookCatalog,
    ) -> Self {
        let n = StepId::COUNT;
        Self {
            draft,
            cookbooks,
            ui: WizardUiState::new(),
            scan,
            probes: ProbeRegistry::new(),
            checks: vec![CheckResult::ok(); n],
            hook_conflicts: HashMap::new(),
            step: None,
            provision_log_rx: None,
            provision_log_lines: Vec::new(),
            provision_log_title: "Log".to_string(),
        }
    }

    pub(crate) fn start_provision_log(&mut self, rx: Receiver<String>) {
        self.start_probe_log(rx, "Provision log", "Starting ephemeral provision test...");
    }

    pub(crate) fn start_oracle_log(&mut self, rx: Receiver<String>) {
        self.start_probe_log(rx, "Oracle tool test", "Starting Oracle tool test...");
    }

    pub(crate) fn start_download_log(&mut self, rx: Receiver<String>, model_id: &str) {
        self.start_probe_log(rx, "Model download", format!("Starting model download: {model_id}"));
    }

    fn start_probe_log(
        &mut self,
        rx: Receiver<String>,
        title: impl Into<String>,
        first_line: impl Into<String>,
    ) {
        self.provision_log_rx = Some(rx);
        self.provision_log_title = title.into();
        self.provision_log_lines.clear();
        self.provision_log_lines.push(first_line.into());
        self.ui.provision_log_open = true;
        self.ui.provision_log_scroll = u16::MAX;
        self.ui.provision_log_follow = true;
    }

    pub(crate) fn poll_provision_log(&mut self) {
        let mut disconnected = false;
        let mut received = false;
        if let Some(rx) = &self.provision_log_rx {
            loop {
                match rx.try_recv() {
                    Ok(line) => {
                        self.provision_log_lines.push(line);
                        received = true;
                    },
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    },
                }
            }
        }
        const MAX_PROVISION_LOG_LINES: usize = 1_000;
        if self.provision_log_lines.len() > MAX_PROVISION_LOG_LINES {
            let excess = self.provision_log_lines.len() - MAX_PROVISION_LOG_LINES;
            self.provision_log_lines.drain(..excess);
        }
        if received && self.ui.provision_log_follow {
            self.ui.provision_log_scroll = u16::MAX;
        }
        if disconnected {
            self.provision_log_rx = None;
        }
    }
}
