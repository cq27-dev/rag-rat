//! Stamps `RAG_RAT_VERSION` at build time: a plain `CARGO_PKG_VERSION` for a pristine release
//! (built at a `v*` tag — proven by `git describe` or the CI tag signal — with a clean tree, or
//! from a cargo-packaged crates.io source), else
//! `<version>+g<short_hash>` (`.dirty` for uncommitted changes) for a git dev build, or
//! `<version>+src` for a source tree with neither git nor cargo package metadata. So any build that
//! isn't a vetted release names itself in `--version` and the migration-provenance record (#585),
//! and the gate treats it as a dev build. The formatter is shared with the crate's tests via
//! `include!`.

use std::path::Path;
use std::process::Command;

include!("src/version_describe.rs");

fn main() {
    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let version = match git_state(&manifest_dir, &pkg) {
        Some(state) =>
            describe_version(&pkg, state.exact_tag, Some(&state.short_hash), state.dirty),
        // No git: a cargo-packaged (crates.io) build is a real release → plain version; a raw
        // source tree (GitHub source archive, copied checkout) is NOT vetted → mark it `+src` so
        // the migration gate (#585) sees the `+` and treats it as a dev build rather than a
        // release.
        None if is_packaged_release(&manifest_dir) => pkg,
        None => format!("{pkg}+src"),
    };
    println!("cargo:rustc-env=RAG_RAT_VERSION={version}");
    force_rerun();
}

/// A cargo-packaged build (what crates.io serves) carries `.cargo_vcs_info.json` beside its
/// manifest; a raw source tree does not. Distinguishes a published release (plain version) from an
/// untracked source build (marked `+src`, so #585 gates it like any other dev build).
fn is_packaged_release(manifest_dir: &str) -> bool {
    Path::new(manifest_dir).join(".cargo_vcs_info.json").exists()
}

struct GitState {
    exact_tag: bool,
    short_hash: String,
    dirty: bool,
}

/// The git state for `dir`, or `None` when git is unavailable / this isn't the rag-rat repo. The
/// toplevel-sentinel guard (`rag-rat.toml` at the repo root) means a copy of these sources vendored
/// inside a FOREIGN git repo can't stamp that repo's hash — it degrades to the plain version.
fn git_state(dir: &str, pkg: &str) -> Option<GitState> {
    let toplevel = git_output(dir, &["rev-parse", "--show-toplevel"])?;
    if !Path::new(toplevel.trim()).join("rag-rat.toml").exists() {
        return None;
    }
    let short_hash = git_output(dir, &["rev-parse", "--short=12", "HEAD"])?.trim().to_string();
    if short_hash.is_empty() {
        return None;
    }
    // Exact-tag proof, either way: `git describe` in a normal full clone, OR GitHub Actions' tag
    // signal — the release build (cargo-dist) checks out shallowly with NO tags, so `git describe`
    // finds nothing even at the release commit. Without the CI fallback every release binary would
    // stamp `+g<hash>` and #585 would gate it as a dev build.
    let exact_tag = git_describes_exact_tag(dir)
        || ci_release_tag(
            std::env::var("GITHUB_REF_TYPE").ok().as_deref(),
            std::env::var("GITHUB_REF_NAME").ok().as_deref(),
            pkg,
        );
    // Only TRACKED changes count as dirty. cargo-dist drops build artifacts
    // (`plan-dist-manifest.json`, …) into the release working tree; those are untracked and
    // must not stamp a real release `.dirty`.
    let dirty = git_output(dir, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|out| !out.trim().is_empty());
    Some(GitState { exact_tag, short_hash, dirty })
}

/// True when `git describe` resolves HEAD to an exact `v*` tag — the normal full-clone path.
fn git_describes_exact_tag(dir: &str) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(["describe", "--tags", "--match", "v*", "--exact-match", "HEAD"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// stdout of a successful `git` invocation in `dir`, else `None`.
fn git_output(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Re-run this build script on EVERY build. The stamp encodes the whole-worktree `git status` dirty
/// bit and the HEAD hash, and NEITHER maps to a stable set of file triggers a `rerun-if-changed`
/// could name: an uncommitted edit anywhere in the workspace changes `git status` but touches no
/// git ref, and (because emitting any `rerun-if-changed` disables Cargo's default package scan) a
/// plain stamp from a clean tag build would otherwise survive a later dirty `cargo install
/// --force`. The migration gate (#585) keys on this stamp's `+g` suffix, so a stale one would
/// misclassify a dirty dev build as a release and let it migrate the shared global DB. Depending on
/// a path that never exists is Cargo's documented way to force a re-run every build; the script is
/// cheap (a few `git` calls) and only the tiny leaf CLI crate carries it.
fn force_rerun() {
    println!("cargo:rerun-if-changed=RAG_RAT_ALWAYS_RESTAMP");
}
