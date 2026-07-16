//! Repository discovery for a configured root — the single gix entry point every subsystem
//! (config anchoring, identity resolution, the indexer's git context) resolves repos through.

use std::path::Path;

/// Open the git repository for `root` via gix. Resolves from `root` (the configured PATH) FIRST,
/// searching upward for a `.git`, and IGNORING `GIT_DIR` / `GIT_WORK_TREE` on that path. The
/// index's git context is defined by `config.root`, so an inherited `GIT_DIR`/`GIT_WORK_TREE` from
/// the launching shell (e.g. a tool operating in a linked worktree — Claude Code, an IDE) must NOT
/// hijack resolution. With the env honored unconditionally, `discover_repo(config.root)` and
/// `discover_repo(linked_path)` BOTH returned the single env-specified repo, regardless of their
/// path argument — collapsing the base↔linked overlay delta to empty and PRUNING/tombstoning every
/// overlay row (the field-reported worktree flip-flop, #219). All gix discovery goes through here.
///
/// Only when no repository is found upward from `root` (a bare git dir + external worktree
/// configured purely through `GIT_DIR`/`GIT_WORK_TREE`, e.g. CI — #213) do we fall back to the
/// environment-override discovery, so that legitimate env-configured layout still resolves instead
/// of leaving git history "unavailable".
///
/// LIMITATION (accepted, #219 review): the env fallback fires only when plain discovery ERRORS. If
/// `config.root` has no `.git` of its own but sits INSIDE an enclosing repo (a monorepo above, a
/// `$HOME` dotfiles repo), plain discovery succeeds on that enclosing repo and the `GIT_DIR`-
/// configured external worktree is silently ignored. We do NOT re-prefer `GIT_DIR` here: that is
/// exactly the env-hijack this fix removed (a worktree shell / git hook exports `GIT_DIR` and would
/// re-capture resolution). The contract is: the index's repo is the one discoverable from
/// `config.root` — don't nest a `GIT_DIR`-only external worktree inside another repo.
pub fn discover_repo(root: &Path) -> Result<gix::Repository, Box<gix::discover::Error>> {
    // Box the error: gix's `discover::Error` is a large enum, and an unboxed large `Err` bloats
    // every `Result` this returns (clippy::result_large_err). Callers use `.ok()` or `?`
    // (anyhow), both of which handle the box transparently.
    match gix::discover(root) {
        Ok(repo) => Ok(repo),
        Err(_) => gix::discover_with_environment_overrides(root).map_err(Box::new),
    }
}
