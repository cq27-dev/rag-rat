//! `.rag-rat-stream` — the checked-in locator that tells a clone which published stream this repo
//! mirrors, so `rag-rat sync subscribe` needs no hand-carried account id.
//!
//! **The file is untrusted input.** It ships inside the repo, so anyone who can land a commit can
//! edit it. Exactly one field is a trust root — `owner`, the account id — and the caller pins it on
//! first use and refuses a later change (see the subscribe pin). Everything else is routing:
//! `peers` and `relay` are followed unauthenticated and deliberately so, because every byte pulled
//! is verified against the pinned account's signature chain. A hostile peer can stall or withhold;
//! it cannot forge an entry, and it cannot make this store mirror a different account.
//!
//! The owner account id is not merely a name for the key — it commits to it. An `AccountId` is a
//! hash over the founding payload, which carries the founding device key, and every entry chains
//! back to that genesis under signature. Pinning the id therefore pins the trust root, which is
//! what makes a plain 64-hex string sufficient here.
//!
//! Routing is not optional. A subscriber cannot discover a foreign account's host — the discovery
//! tag derives from that account's own secret, which only its own devices hold — so a locator
//! carrying an account id alone would name a stream nobody can reach.
//!
//! Unknown keys are tolerated rather than rejected: this file lives in other people's repos and is
//! read by whatever rag-rat version they happen to run, so a field added later must not turn every
//! older binary into a hard failure.

use std::path::Path;

use serde::Deserialize;

/// The locator's name at the repo root. Checked in, and read from the active checkout.
pub const STREAM_LOCATOR_FILE: &str = ".rag-rat-stream";

/// A parsed, validated `.rag-rat-stream`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLocator {
    /// The published owner account whose memories a subscriber mirrors — 64 lowercase hex. The
    /// only field that carries authority; the pin is taken on this.
    pub owner: String,
    /// Node ids to reach the owner's host. Unauthenticated routing hints.
    pub peers: Vec<String>,
    /// Relay URL for reaching those peers. Unauthenticated.
    pub relay: Option<String>,
    /// Display name for the stream. Never used for identity.
    pub name: Option<String>,
}

#[derive(Deserialize)]
struct RawLocator {
    owner: String,
    #[serde(default)]
    peers: Vec<String>,
    #[serde(default)]
    relay: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Read and validate the locator at `repo_root`, or `None` when the repo checks none in.
///
/// A malformed file is an error rather than a `None`: a repo that ships a locator meant it to work,
/// and silently falling back to "no locator configured" would report the wrong problem.
pub fn load(repo_root: &Path) -> anyhow::Result<Option<StreamLocator>> {
    let path = repo_root.join(STREAM_LOCATOR_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(anyhow::anyhow!("reading {}: {err}", path.display())),
    };
    parse(&text).map(Some).map_err(|err| anyhow::anyhow!("{}: {err}", path.display()))
}

/// Parse locator text. Split from [`load`] so the format is testable without a filesystem.
pub fn parse(text: &str) -> anyhow::Result<StreamLocator> {
    let raw: RawLocator = toml::from_str(text)?;
    let owner = raw.owner.trim().to_string();
    anyhow::ensure!(
        owner.len() == 64 && owner.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
        "`owner` must be a 64-character lowercase hex account id, got `{owner}`"
    );
    Ok(StreamLocator { owner, peers: raw.peers, relay: raw.relay, name: raw.name })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "4ddbda7cea3a613136ed6c10f26d3fb02bc0f0efa5ef4e5d87bc2e5c9e9b7d89";

    #[test]
    fn a_locator_carries_the_owner_and_its_routing() {
        let parsed = parse(&format!(
            "owner = \"{OWNER}\"\npeers = [\"node-a\", \"node-b\"]\nrelay = \"https://r\"\nname = \
             \"demo\"\n"
        ))
        .unwrap();
        assert_eq!(parsed.owner, OWNER);
        assert_eq!(parsed.peers, ["node-a", "node-b"]);
        assert_eq!(parsed.relay.as_deref(), Some("https://r"));
        assert_eq!(parsed.name.as_deref(), Some("demo"));
    }

    #[test]
    fn routing_is_optional_at_the_format_level() {
        // The format does not demand peers: an owner already reachable through configured
        // `server_peers` needs none, and rejecting that here would refuse a working setup. The
        // subscribe path is where an unreachable owner is diagnosed, with the peer flag to fix it.
        let parsed = parse(&format!("owner = \"{OWNER}\"\n")).unwrap();
        assert!(parsed.peers.is_empty());
        assert_eq!(parsed.relay, None);
    }

    #[test]
    fn an_unknown_key_is_tolerated() {
        // This file sits in other people's repos and is read by whatever version they run. A field
        // added in a later release must not turn every older binary into a hard failure.
        let parsed = parse(&format!("owner = \"{OWNER}\"\nsomething_added_later = 3\n")).unwrap();
        assert_eq!(parsed.owner, OWNER);
    }

    #[test]
    fn a_malformed_owner_is_refused_rather_than_carried() {
        // The owner is the trust root. Carrying a bad one forward would surface as an obscure
        // transport failure later instead of naming the file that is wrong.
        for bad in [
            "",
            "not-hex-at-all",
            "4DDBDA7CEA3A613136ED6C10F26D3FB02BC0F0EFA5EF4E5D87BC2E5C9E9B7D89", // uppercase
            "4ddbda7c",                                                         // too short
        ] {
            let err = parse(&format!("owner = \"{bad}\"\n")).unwrap_err().to_string();
            assert!(err.contains("64-character lowercase hex"), "for `{bad}`: {err}");
        }
    }

    #[test]
    fn a_locator_without_an_owner_is_refused() {
        let err = parse("peers = [\"node-a\"]\n").unwrap_err().to_string();
        assert!(err.contains("owner"), "the error must name the missing field: {err}");
    }

    #[test]
    fn a_linked_worktree_reads_its_own_locator_not_the_main_checkouts() {
        // `config.root` is main-anchored in a linked worktree, so a caller that loads the locator
        // from it pins or refuses against the WRONG file while the operator stands in the branch.
        // `worktree_root` is the session-side root that rebases onto the active checkout.
        const OTHER: &str = "99ff249f76f43de5497761bde999a7baa902008491e4cd1a6a943bcbf1d1f7b1";
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        crate::test_git::run(&main, &["init", "-q", "."]);
        crate::test_git::run(&main, &["config", "user.email", "t@t"]);
        crate::test_git::run(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join(STREAM_LOCATOR_FILE), format!("owner = \"{OWNER}\"\n")).unwrap();
        crate::test_git::run(&main, &["add", "-A"]);
        crate::test_git::run(&main, &["commit", "-qm", "main locator"]);

        let linked = tmp.path().join("linked");
        crate::test_git::run(&main, &[
            "worktree",
            "add",
            "-q",
            "-b",
            "branch",
            linked.to_str().unwrap(),
        ]);
        std::fs::write(linked.join(STREAM_LOCATOR_FILE), format!("owner = \"{OTHER}\"\n")).unwrap();

        let active = crate::config::worktree_root(&linked).expect("a linked worktree has a root");
        assert_eq!(
            load(&active).unwrap().expect("the branch checks one in").owner,
            OTHER,
            "the active checkout's locator governs, not the main checkout's",
        );
        assert_eq!(
            load(&main).unwrap().expect("main still has its own").owner,
            OWNER,
            "and the main checkout is unaffected by the branch's",
        );
    }

    #[test]
    fn an_absent_file_is_not_an_error_but_a_malformed_one_is() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None, "a repo may simply check none in");

        std::fs::write(dir.path().join(STREAM_LOCATOR_FILE), "owner = \"nope\"\n").unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains(STREAM_LOCATOR_FILE), "the error names the file: {err}");
    }
}
