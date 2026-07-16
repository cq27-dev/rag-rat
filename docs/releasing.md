# Releasing

Releases are automated by [release-plz](https://release-plz.dev) and the three crates ship in
**lockstep** — `rag-rat`, `rag-rat-core`, `rag-rat-mcp`, `rag-rat-db`, and `rag-rat-base` always carry the same version. That
version has a single source of truth in `[workspace.package].version` (root `Cargo.toml`); every
crate inherits it via `version.workspace = true`, and `release-plz.toml` puts all three in one
`version_group` so the bump is shared.

## Flow

Configured in `.github/workflows/release-plz.yml`; runs on every push to `main`.

1. **Release PR.** release-plz opens (and keeps updating) a "Release" PR that bumps the workspace
   version and regenerates the combined `CHANGELOG.md` from [Conventional
   Commits](https://www.conventionalcommits.org). Nothing ships until a human merges it — review the
   bump and changelog there.
2. **Publish + tag.** When that PR merges, release-plz publishes each crate to crates.io in
   dependency order (`rag-rat-base` → `rag-rat-db` → `rag-rat-core` → `rag-rat-mcp` → `rag-rat`, waiting for the registry to
   propagate each before the next), then creates a single `vX.Y.Z` git tag and GitHub release for
   the whole workspace.

So day-to-day there is no manual release step: write Conventional-Commit messages, merge the Release
PR when you want to cut a version.

## Choosing the bump

release-plz derives the next version from the Conventional-Commit messages since the last release.
**We are pre-1.0, where semver demotes every bump one level, and release-plz follows cargo's
compatibility rules:**

| Commit                                    | Post-1.0 | Pre-1.0 (us, at `0.x`) |
| ----------------------------------------- | -------- | ---------------------- |
| `fix:`                                    | patch    | patch (`0.4.0→0.4.1`)  |
| `feat:`                                   | minor    | patch (`0.4.0→0.4.1`)  |
| `feat!:` / `BREAKING CHANGE:` footer      | major    | **minor** (`0.4.0→0.5.0`) |

The catch: at `0.x` a plain `feat:` only bumps the patch, so an ordinary feature commit will **not**
produce a minor bump. To go `0.4.0 → 0.5.0` you have two options:

1. **Signal a breaking change** — land a commit marked `feat!:` (or any commit with a
   `BREAKING CHANGE:` footer). release-plz reads that as a minor bump at `0.x`, applied to all three
   crates (they share a `version_group`).

2. **Force the exact version** with the release-plz CLI — the documented escape hatch for when the
   commit history doesn't compute the version you want:

   ```bash
   release-plz set-version [email protected] [email protected] [email protected]
   ```

   (The workspace form is `<pkg>@<ver>`; the bare `set-version 1.2.3` form is single-crate only.)
   Commit the change and push to `main`; the Release PR then carries `0.5.0`.

**Safety net:** nothing publishes until the Release PR merges, and that PR shows the computed version
and changelog first. If release-plz proposes `0.4.1` when you wanted `0.5.0`, push a `set-version`
commit (or just edit the version in the PR) before merging.

## One-time setup

Already wired except where noted:

- **`CARGO_REGISTRY_TOKEN`** must exist as a repository secret — a crates.io API token with the
  `publish-new` and `publish-update` scopes. Without it the publish step can tag but not publish.
- The default `GITHUB_TOKEN` does **not** trigger `ci.yml` on the Release PR (GitHub blocks
  workflow-created PRs from starting other workflows). To gate the Release PR on CI, swap the token
  in `release-plz.yml` for a PAT or GitHub App token.

## Manual bump (no release-plz CLI)

If you don't have the release-plz CLI installed, you can force a version the same way by hand: edit
`[workspace.package].version` and the two internal-dep `version`s under `[workspace.dependencies]` in
the root `Cargo.toml` — keep all three in step. This is exactly what `set-version` above automates.
