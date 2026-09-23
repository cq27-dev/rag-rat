# ADR 0001: Agent-driven initialization over MCP

- Status: Proposed
- Date: 2026-07-11

> **What ships today.** The `init_*` MCP tools below are not built. Agent-driven setup is the
> `init-rag-rat` skill driving `rag-rat init --yes [--dry-run] [--no-hooks]` from the agent's shell,
> and a dormant server's tool results point to that skill. The interactive path is the terminal
> wizard (`rag-rat init`), which refuses to start without a terminal. The draft/config model under
> `crates/rag-rat-cli/src/init/` is already UI-free; its validators, probes and apply step are not
> yet, which is the work this ADR still describes.

## Context

`rag-rat mcp` can be registered globally and launched in a project that does not yet contain a
`rag-rat.toml`. In that case the MCP server correctly starts in a dormant state instead of exiting,
but its repository-intelligence tools cannot operate until the project has been configured and
indexed.

The primary initialization experience is currently `rag-rat init`, a full-screen ratatui wizard.
That experience is useful for interactive expert configuration, but it is relatively heavy for a
developer who is already working through an agent such as Codex or Claude. The agent cannot
reliably drive terminal navigation, and reproducing terminal events over MCP would expose UI state
rather than the initialization domain.

The current implementation already contains a useful separation:

- repository scanning discovers languages, paths, and relevant project capabilities;
- `WizardDraft` is the persistable configuration model and the only wizard state rendered to TOML;
- validation, probes, review, and application are conceptually distinct from ratatui rendering;
- a server launched dormant deliberately remains dormant until restart.

The last property is a correctness boundary. An active server is not merely a server that can load
a config: it also has the watcher, hook listener, and other lifecycle components that preserve the
guarantee that tool results are validated against current source. Discovering a newly written
config and serving normal tools without starting that complete lifecycle would create a
half-active server.

## Decision

We will expose an agent-native initialization workflow through MCP. It will be a small,
stateful configuration transaction backed by the same scan, draft, validation, rendering, and
application logic as the CLI wizard.

We will not expose the ratatui wizard's navigation model over MCP. Page selection, focused fields,
keyboard events, scrolling, and help popups remain presentation concerns. The agent owns the
conversation with the developer; rag-rat owns configuration state and validation.

### Tool contract

The proposed MCP surface is:

#### `init_start`

Starts an initialization transaction for the MCP process's project root and returns:

- an opaque `session_id`;
- a monotonically increasing `revision`;
- repository scan results;
- a proposed configuration draft;
- warnings and blocking validation errors;
- decisions that still require developer input.

The response should be sufficient for an agent to explain only the material choices instead of
walking every developer through every available wizard option.

#### `init_update`

Accepts `session_id`, the expected `revision`, and a structured patch. It returns:

- the normalized draft;
- a new revision;
- validation and probe results already available;
- remaining unresolved decisions;
- a rendered TOML preview or diff.

Revision checking provides optimistic concurrency control. A repeated, stale, or concurrent agent
call must not silently overwrite a newer choice.

#### `init_probe`

Runs a named, bounded probe against the current draft, such as checking an embedding endpoint,
available model, or integration conflict. Probe results update or annotate the transaction without
requiring UI event emulation.

Potentially destructive actions, external provisioning commands, and secret acquisition are not
probes and require a separate explicit authorization boundary.

#### `init_apply`

Accepts `session_id`, the expected `revision`, and `confirm: true`. It:

1. revalidates the draft and its target;
2. atomically writes `rag-rat.toml`;
3. applies explicitly approved integrations;
4. starts the initial index operation, or returns a background job identifier;
5. reports that the MCP server must be restarted before ordinary repository tools become active.

Applying the same confirmed revision must be idempotent.

#### `init_status`

Returns progress and the terminal result for long-running initialization work such as indexing or
embedding reconciliation. Long work should not depend on one MCP tool call remaining open until
completion.

### State model

Initialization state will contain domain state only:

- repository scan;
- normalized `WizardDraft`;
- validation results;
- completed and pending probes;
- session revision;
- apply or background-job status.

It will not contain terminal navigation state.

Sessions may initially live in memory because the MCP process provides a natural lifetime. The
protocol must treat a lost session as recoverable: the agent can call `init_start` again and receive
a draft reconstructed from the repository and any config that was successfully written. Durable
cross-process sessions are deferred until there is evidence that they are needed.

### Activation boundary

The first implementation will not activate a dormant MCP server in place. A successful apply will
return an explicit result such as:

```json
{
  "configured": true,
  "indexed": true,
  "restart_required": true
}
```

After the MCP client restarts the server, normal startup discovers the config and establishes the
complete active lifecycle.

In-process activation may be considered separately. It requires an atomic lifecycle transition
that loads the config, opens and validates the database, starts all freshness machinery, swaps the
service state safely, and rolls back cleanly on partial failure. It is not part of this decision.

### Safety constraints

The MCP initialization workflow will:

- operate only on the project root assigned to the MCP process, not an arbitrary caller-supplied
  filesystem path;
- preserve the existing rule that the main worktree governs configuration for linked worktrees;
- preview the resulting configuration before mutation and require `confirm: true` to apply it;
- write `rag-rat.toml` atomically;
- default optional hooks, integrations, and external commands to disabled unless explicitly
  approved;
- store references to secret-bearing environment variables, not secret values in the draft or
  generated TOML;
- distinguish read-only probes from side-effecting operations;
- reject stale revisions and make confirmed application idempotent.

### Implementation boundary

Initialization domain logic should be extracted into an agent-neutral layer:

```text
repository scan -> draft -> patch -> validate -> preview -> apply
                         /                            \
                ratatui adapter                    MCP adapter
```

The CLI and MCP adapters must share this layer rather than independently implementing defaults,
normalization, validation, rendering, or application. This prevents the two initialization
experiences from producing subtly different `rag-rat.toml` files.

The MCP tool names are intentionally explicit. A single tool named `init` would obscure whether a
call merely inspects the repository, mutates a draft, writes files, or starts expensive work.

## Consequences

### Positive

- A dormant globally registered MCP server provides a direct path to making itself useful in a new
  repository.
- Developers can initialize rag-rat conversationally without learning or navigating the full-screen
  wizard.
- Agents receive structured choices, validation, and previews instead of scraping terminal output.
- CLI and MCP initialization share one configuration model and one set of invariants.
- Explicit preview, confirmation, revisions, and job status make mutations auditable and retryable.
- Restart-to-activate preserves the existing freshness guarantee and keeps the first implementation
  bounded.

### Negative

- Initialization domain code currently owned by the CLI will need to move behind a reusable crate
  boundary or otherwise become consumable by `rag-rat-mcp`.
- The MCP server gains mutable session and job state.
- The protocol needs expiry, concurrency, retry, and idempotency tests.
- Restarting the MCP connection after initialization is less seamless than live activation.
- The CLI and MCP adapters can still drift if presentation-specific logic leaks back into shared
  decisions.

### Neutral or deferred

- The ratatui wizard remains supported and can offer a richer manual experience.
- `rag-rat init --yes` remains the lightweight non-interactive CLI path.
- Persisting incomplete sessions across MCP restarts is deferred.
- Automatically requesting an MCP client restart is client-specific and outside the initial server
  contract.
- Live dormant-to-active transition is a separate future decision.

## Alternatives considered

### Drive ratatui through MCP

Rejected. It couples the protocol to keyboard events and screen-navigation state, produces poor
agent ergonomics, and makes the terminal UI an accidental public API.

### Expose one stateless `init` call with every option

Rejected as the primary interface. It is simple for defaults-only initialization but performs
poorly when repository-dependent choices, probes, validation failures, or developer confirmation
require more than one conversational turn. A defaults-only convenience operation may later be
built on top of the transaction API.

### Let the agent write `rag-rat.toml` directly

Rejected as the supported flow. It bypasses repository scanning, normalization, validation,
worktree governance, integration checks, atomic application, and initial indexing. Direct file
editing remains possible, but it should not be the product interface offered to agents.

### Activate the dormant server immediately after apply

Deferred. It would provide the smoothest experience, but correctness requires starting the entire
active lifecycle rather than merely replacing `None` with a loaded config. Restarting establishes
that lifecycle using the already-tested startup path.

### Only improve `rag-rat init --yes`

Rejected as a complete solution. It helps shell automation but does not give an MCP-connected agent
structured repository findings, incremental choices, validation feedback, previews, or job status.

## Acceptance criteria

This decision is implemented when:

1. a server launched without `rag-rat.toml` advertises and serves the initialization tools while
   ordinary repository tools continue to return the dormant result;
2. an agent can scan a repository, update a draft over multiple calls, inspect the final TOML, and
   apply it only with explicit confirmation;
3. stale revisions and repeated application are handled safely;
4. linked-worktree configuration rules and secret-handling constraints are preserved;
5. initial indexing exposes bounded progress or background job status;
6. a successful apply clearly requests an MCP restart;
7. after restart, the server starts fully active and ordinary tools operate against the newly
   initialized repository;
8. the CLI wizard and MCP flow share tests for defaults, validation, rendering, and application.

