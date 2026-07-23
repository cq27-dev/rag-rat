//! WizardState and WizardUiState — minimal state container.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, TryRecvError};

use rag_rat_papertrail::ResolvedTracker;

use super::catalog::CookbookCatalog;
use super::draft::{TrackerMode, WizardDraft};
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

/// Seed the explicit-binding defaults from auto-detection — but ONLY for a still-auto-detect draft.
/// A reconfigure draft that already recovered an explicit `[[tracker]]` (mode `Configure`) keeps
/// its own provider/base_url; overwriting them with the origin-derived values would silently
/// corrupt a binding (e.g. a Jira tracker in a GitHub-hosted repo becomes GitHub while keeping the
/// Jira project/auth). For a fresh draft it just pre-selects the detected provider so switching to
/// Configure starts from the right place.
fn seed_tracker_from_detection(draft: &mut WizardDraft, detected: Option<&ResolvedTracker>) {
    if draft.tracker.mode != TrackerMode::AutoDetect {
        return;
    }
    if let Some(det) = detected {
        draft.tracker.provider = det.provider;
        if let Some(base) = &det.base_url {
            draft.tracker.base_url = base.clone();
        }
    }
}

pub(crate) struct WizardState {
    pub draft: WizardDraft,
    pub cookbooks: CookbookCatalog,
    /// What auto-detection resolves from the `origin` remote — the AutoDetect gate + panel
    /// (runtime auto-detection always uses `origin`, regardless of a binding's configured
    /// remote).
    pub detected_origin: Option<ResolvedTracker>,
    /// What the binding's CONFIGURED remote resolves to (`origin` by default) — the Configure
    /// gate. Distinct from `detected_origin` only for an explicit non-`origin` remote. Both
    /// are computed once at construction (git-remote reads), never re-run per frame.
    pub detected_configured: Option<ResolvedTracker>,
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
        mut draft: WizardDraft,
        scan: RepoScan,
        cookbooks: CookbookCatalog,
    ) -> Self {
        let n = StepId::COUNT;
        // Detect against BOTH origin (what AutoDetect binds at runtime) and the binding's
        // configured remote (what a Configure binding derives from) — they differ only for an
        // explicit non-`origin` remote, and conflating them makes the gate use the wrong one.
        let detected_origin = rag_rat_papertrail::auto_detect_tracker(&draft.root_abs);
        let configured_remote = match draft.tracker.remote.trim() {
            "" => "origin",
            other => other,
        };
        let detected_configured =
            rag_rat_papertrail::detect_tracker_for_remote(&draft.root_abs, configured_remote);
        seed_tracker_from_detection(&mut draft, detected_configured.as_ref());
        Self {
            draft,
            cookbooks,
            detected_origin,
            detected_configured,
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

#[cfg(test)]
mod tests {
    use rag_rat_base::config::Tracker;

    use super::*;
    use crate::init::RepoScan;
    use crate::init::wizard::draft::WizardDraft;

    fn draft() -> WizardDraft {
        WizardDraft::from_scan(&RepoScan::default(), ".".into(), std::path::PathBuf::from("."))
    }

    #[test]
    fn detection_seeds_an_auto_detect_draft_but_never_a_configured_one() {
        let github = rag_rat_papertrail::detect_tracker_from_remote_url("https://github.com/o/r");

        // Fresh auto-detect draft: the detected provider is seeded.
        let mut d = draft();
        d.tracker.provider = Tracker::Bitbucket; // a stale placeholder
        seed_tracker_from_detection(&mut d, github.as_ref());
        assert_eq!(d.tracker.provider, Tracker::Github);

        // Reconfigure draft (already Configure): detection must NOT overwrite the recovered
        // provider — a Jira binding in a GitHub-hosted repo must stay Jira.
        let mut d = draft();
        d.tracker.mode = TrackerMode::Configure;
        d.tracker.provider = Tracker::Jira;
        seed_tracker_from_detection(&mut d, github.as_ref());
        assert_eq!(d.tracker.provider, Tracker::Jira, "a configured binding must be preserved");
    }
}
