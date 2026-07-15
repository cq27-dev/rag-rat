# Issue trackers (`[[tracker]]`) and papertrail sync (`[papertrail]`)

Part of the [config reference](../config.md).

`[[tracker]]` binds the repo to the issue tracker(s) whose items annotate its papertrail. It is
list-valued: one repo can bind a code host **and** Jira at the same time.

```toml
[[tracker]]
provider = "github"            # github | gitlab | bitbucket | jira
# project = "owner/repo"       # default: derived from the git remote; jira: the project KEY
# remote = "origin"            # which git remote to derive `project` from
# base_url = "https://gitlab.example.com"   # self-hosted / enterprise instance
# auth = { env = "GITLAB_TOKEN" }           # or { token_command = "glab auth token" }
# tags = ["bug", "perf"]       # tracked tags/labels; empty or missing = track ALL
```

**Zero-config default.** With no `[[tracker]]` at all, the binding is auto-detected from the git
`origin` remote URL (ssh and https forms alike): a `github.com` remote binds GitHub, a `gitlab.*`
host (gitlab.com or self-hosted — the base URL is taken from the host) binds GitLab, and
`bitbucket.org` binds Bitbucket. Most repos never need a `[[tracker]]` section. Writing one
**replaces** auto-detection entirely.

Per-key notes:

- `provider` — required, one of `github` / `gitlab` / `bitbucket` / `jira`.
- `project` — optional for the code hosts (derived from the git remote named by `remote`,
  default `origin`); GitLab keeps full subgroup paths (`group/sub/repo`). **Required for Jira**
  (the project key, e.g. `PROJ`): Jira is not a code host, so it is never auto-detected and has
  no remote to derive from.
- `base_url` — self-hosted / enterprise instances (`https://gitlab.example.com`). Must be an
  origin URL without inline credentials, and must use **https**: the transport never sends the
  binding's token over plaintext, so `http` origins are accepted for loopback hosts only (local
  testing). Self-hosted GitHub Enterprise and Bitbucket instances are configured this way —
  only the three cloud hosts auto-detect.
- `auth` — exactly one of `env` (the NAME of an env var holding the token — never the token
  itself) or `token_command` (a command printing the token to stdout). Omitted = anonymous /
  provider-native fallback.
- `tags` — the tracked subset, OR-matched case-insensitively against the provider's label-like
  facets (labels on GitHub/GitLab; labels + components on Jira; kind + component on Bitbucket
  issues). Empty or missing tracks everything; item kinds with no tag facet (e.g. Bitbucket pull
  requests) are always tracked. The normalized list is fingerprinted into the sync cursor, so
  widening the list restarts the backfill and narrowing prunes.

Ref grammar per provider (what the annotation layer recognizes in commits, files, and branch
names): GitHub `#N` / `GH-N` / `owner/repo#N` / issue+PR URLs; GitLab `#N` (issues), `!N` (merge
requests), `namespace/path#N` / `!N` with subgroups, and `/-/issues/N` / `/-/merge_requests/N`
URLs; Bitbucket `#N` (issues) and `/issues/N` / `/pull-requests/N` URLs; Jira bare keys
(`PROJ-123`) for the bound project. A bare `#N` resolves against the first code-host binding
only. A self-hosted binding's URL grammar matches its own host, not the cloud host.

All bindings participate in production reference discovery, annotation lookup, and manual mirror
dispatch. GitHub and GitLab bindings use their native clients now; Bitbucket and Jira bindings
report a provider-client-pending result until their provider PRs land, and are never sent through
another provider's client.

GitLab notes: auth is a personal access token with `read_api` (sent as a bearer token) via
`auth = { env = "GITLAB_TOKEN" }` or a `token_command`; subgroup project paths are fully
supported; issues `#N` and merge requests `!N` are separate numbering namespaces and mirror as
`issue` / `change_request` items. New comments arrive incrementally through the project events
feed (GitLab has no "notes updated since" endpoint, and commenting does not touch the parent's
`updated_at`); an EDIT to an old comment produces no event, so edited bodies converge at the
daily full re-walk unless the comment's creation event is still inside the incremental replay
window. Merge-request approvals mirror as review comments (`review_state`) — approving emits no
`commented` event either, so approvals also converge at the daily full re-walk. A comment whose
parent item is excluded by the `tags` filter is skipped; if that item later gains a tracked tag,
its earlier comments appear at the next full re-walk. The shared runner keeps every result, cursor, auth source, and quota governor scoped to its
binding. A repository may have at most one resolved binding for a given `(provider, project)`:
the mirror rejects duplicates before dispatch because API origin and tag filters are not part of
the persisted item/cache cursor identity. Use one binding and one combined tag set for that
provider project.

The shared transport keeps part of each header-reported quota untouched for the user's own tools.
The reserve defaults to 35% and can be changed independently of provider clients:

```toml
[papertrail]
rate_limit_reserve = 0.35   # finite fraction in 0.0 <= value < 1.0
```

The provider mirror scheduler accepts three cadence keys. Decisions are per binding: attempts are
coalesced by the minimum interval, lightweight probes run on the probe cadence, and a full walk
periodically heals historical drift. Persisted rate-limit horizons and unfinished cursor work take
priority over these ordinary cadences.

```toml
[papertrail]
probe_interval_secs = 900       # default: 15 minutes; must be positive
sync_min_interval_secs = 900    # default: 15 minutes between attempts; 0 = no attempt gate
full_sync_interval_secs = 86400 # default: daily healing walk; must be positive
```

Synchronization is **automatic** once an index exists — no cron entry and no manual sync command
needed:

- **Git hooks.** Every git-hook `rag-rat maintenance` trigger (post-commit, post-merge,
  post-rewrite, post-checkout) finishes its ordinary index pass, releases its coordination lock,
  then runs an incremental papertrail evaluation. Manual or scripted `maintenance` runs (no
  `--trigger`, or a non-hook value) never start mirror work — they stay bounded by their index
  budget. A broken or unreachable mirror never fails the hook; the outcome rides the maintenance
  report's `papertrail` field.
- **The watcher.** A live watcher (the MCP server) evaluates the schedule at the tightest
  configured cadence above — every 15 minutes by default — even when the filesystem is idle, and
  the daily full-walk backstop rides the same deadline. An in-flight index pass never postpones
  the evaluation.
- **One flight per repository.** All triggers — watcher ticks, hooks, concurrent processes —
  coalesce on a per-repo flight lock: at most one mirror run is in the air, triggers arriving
  mid-run queue at most one follow-up, and a queued full walk is never weakened by a later
  incremental trigger.
- **Failures stay per binding.** Due bindings sync concurrently under one shared quota governor;
  a failed or rate-paused binding never cancels its siblings, its failure class is persisted on
  the binding's health row (see `papertrail_sync_status`), and the cadence above retries it.
  Auto-sync never holds the repository write lock, so mirror traffic cannot stall ordinary index
  maintenance.
- **Indexed repos only.** Automatic sync starts after the repo's first index pass — a repo that
  is merely registered in a shared database (read-only opens do that) is never mirrored
  automatically.

Manual `rag-rat papertrail sync` remains the unconditional pass: it dispatches every binding
regardless of cadence, reports provider-client-pending bindings explicitly, and also refreshes
reference discovery (commit / file / branch refs), which the automatic path deliberately skips.
It shares the per-repo flight lock with automatic sync — two mirror runs over one binding would
clobber each other's cursor — so a request issued while an automatic flight is running waits for
that flight to finish (the wait is announced on stderr and can be interrupted) and then runs the
full manual pass; it is never downgraded to a policy-gated follow-up.
