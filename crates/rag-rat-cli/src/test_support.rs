use rag_rat_base::config::{Config, ResolvedTarget};
use rag_rat_base::test_scratch::{self, ScratchDir};

pub(crate) fn scratch_config(tag: &str, target: ResolvedTarget) -> (ScratchDir, Config) {
    let scratch = ScratchDir::new(tag);
    let root = test_scratch::canonical_config_root(scratch.to_path_buf());
    let config = Config {
        database_key_pinned: true,
        database: root.join(".rag-rat/index.sqlite"),
        root,
        targets: vec![target],
        ..Default::default()
    };
    (scratch, config)
}
