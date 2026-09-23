//! Repository setup, shared by every way rag-rat gets set up: the CLI wizard, `rag-rat init --yes`,
//! and the MCP setup tools (#1439). It owns the domain — scanning a repository, the configuration
//! draft and its rendering to `rag-rat.toml`, and the git maintenance hooks — and nothing about how
//! any one front end presents it.

pub mod draft;
pub mod fs_atomic;
pub mod hooks;
pub mod plan;
pub mod render;
pub mod scan;

pub use draft::SetupDraft;
pub use plan::{DirCandidate, InitPlan, RepoScan, default_plan};
