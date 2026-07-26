//! Foundation crate for the rag-rat workspace: configuration, repo identity and discovery,
//! filesystem locations, language registry, embedding-model registry, coordination locks,
//! logging, and small shared primitives. Everything here is below the database layer — no
//! SQLite, no domain logic — so every other crate can depend on it without cycles.

pub mod canonical;
pub mod config;
pub mod data_dir;
pub mod embedding_models;
pub mod hash;
pub mod heap;
pub mod language;
pub mod locks;
pub mod logging;
pub mod path_class;
pub mod paths;
pub mod repo_discover;
pub mod repo_identity;
pub mod serde_big_id;
pub mod single_flight;
pub mod stack;
pub mod test_git;
pub mod test_scratch;
pub mod time;
pub mod version;
