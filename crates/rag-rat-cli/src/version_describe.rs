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

#[cfg(test)]
mod tests {
    use super::describe_version;

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
}
