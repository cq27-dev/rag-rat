// Pure version-string formatting shared by `build.rs` (which stamps `RAG_RAT_VERSION` at build
// time from the git state) and its unit tests. `include!`d verbatim by `build.rs` (so it must use
// only `//` comments — an inner `//!` doc is illegal at an include point); compiled into the crate
// itself only under `cfg(test)` (the runtime reads the baked `RAG_RAT_VERSION`, never this fn), so
// it carries no dead code in a release build.

/// The reported version string. Plain `pkg` for a pristine release — built at an exact release tag
/// with a clean tree, or with no git at all (a crates.io tarball / prebuilt binary). Otherwise
/// `pkg+g<short_hash>`, with a trailing `.dirty` when the working tree has uncommitted changes — so
/// a development build names itself in `--version` and in the migration-provenance record (#585).
pub(crate) fn describe_version(
    pkg: &str,
    exact_tag: bool,
    short_hash: Option<&str>,
    dirty: bool,
) -> String {
    match short_hash {
        // No git → a released tarball / prebuilt binary: report the plain version.
        None => pkg.to_string(),
        // A pristine release: exactly on a tag with nothing uncommitted.
        Some(_) if exact_tag && !dirty => pkg.to_string(),
        // Anything else is a development build — fingerprint it.
        Some(hash) => {
            let mut version = format!("{pkg}+g{hash}");
            if dirty {
                version.push_str(".dirty");
            }
            version
        },
    }
}

/// Whether GitHub Actions' tag signal proves HEAD is exactly the `v<pkg>` release tag. A cargo-dist
/// release build checks out shallowly with no tags fetched, so `git describe --exact-match` finds
/// nothing even AT the release commit; the `GITHUB_REF_TYPE` / `GITHUB_REF_NAME` pair is the
/// reliable exact-tag proof there. `ref_type` / `ref_name` are the raw env values (`None` when
/// unset — e.g. a local build, which falls back to `git describe`).
pub(crate) fn ci_release_tag(ref_type: Option<&str>, ref_name: Option<&str>, pkg: &str) -> bool {
    ref_type == Some("tag") && ref_name == Some(format!("v{pkg}").as_str())
}

#[cfg(test)]
mod tests {
    use super::{ci_release_tag, describe_version};

    #[test]
    fn pristine_release_tag_reads_plain() {
        assert_eq!(describe_version("0.16.0", true, Some("abc1234"), false), "0.16.0");
    }

    #[test]
    fn no_git_reads_plain() {
        assert_eq!(describe_version("0.16.0", false, None, false), "0.16.0");
    }

    #[test]
    fn off_tag_gets_a_hash_suffix() {
        assert_eq!(describe_version("0.16.0", false, Some("abc1234"), false), "0.16.0+gabc1234");
    }

    #[test]
    fn uncommitted_tree_is_marked_dirty() {
        assert_eq!(
            describe_version("0.16.0", false, Some("abc1234"), true),
            "0.16.0+gabc1234.dirty"
        );
    }

    #[test]
    fn a_dirty_tree_even_at_a_tag_is_not_a_pristine_release() {
        assert_eq!(
            describe_version("0.16.0", true, Some("abc1234"), true),
            "0.16.0+gabc1234.dirty"
        );
    }

    #[test]
    fn ci_tag_ref_matching_pkg_is_a_release() {
        // The release build: GitHub Actions building the `v0.16.0` tag. Proves exact-tag even
        // though a shallow checkout gives `git describe` no tag to find.
        assert!(ci_release_tag(Some("tag"), Some("v0.16.0"), "0.16.0"));
    }

    #[test]
    fn ci_branch_ref_is_not_a_release() {
        assert!(!ci_release_tag(Some("branch"), Some("main"), "0.16.0"));
    }

    #[test]
    fn ci_tag_ref_for_a_different_version_is_not_this_release() {
        assert!(!ci_release_tag(Some("tag"), Some("v0.15.0"), "0.16.0"));
    }

    #[test]
    fn no_ci_env_is_not_a_release() {
        // A local build (env unset) — falls back to `git describe`, never the CI signal.
        assert!(!ci_release_tag(None, None, "0.16.0"));
    }
}
