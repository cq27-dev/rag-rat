---
name: init-rag-rat
description: >
  Use to set up rag-rat in a repository that is not indexed yet — when the rag-rat MCP server is
  DORMANT (its tools return {"status":"no_index"} with a "Run rag-rat init … then restart" remedy),
  or when the user asks to initialize / configure / "set up rag-rat" / "index this repo" / choose an
  embedding backend. Drives the setup conversationally: scans the repo, explains only the material
  choices, recommends the local FastEmbed default for a modest machine, and — only when a remote /
  GPU embedder is worth it — loads a reference for local infinity (Docker) or an ephemeral
  Modal/RunPod worker, then previews the config, writes it after confirmation, runs the first index,
  installs the git maintenance hooks (keep the index fresh), and tells the user to restart the MCP
  server. Triggers: "set up rag-rat", "rag-rat is dormant",
  "no rag-rat.toml", "index this repo", "configure rag-rat embeddings".
---

# init-rag-rat — configure a dormant rag-rat repo conversationally

rag-rat's MCP server boots **dormant** in a repo with no `rag-rat.toml`: ordinary tools return
`{"status":"no_index", "remedy":"Run rag-rat init … then restart"}`. This skill turns that into a
short guided setup. **You own the conversation; `rag-rat init` owns the scan and validation** — never
hand-write a config blind, and let a real config load re-check it before indexing.

## The common path

1. **Detect state.** You are here because a rag-rat tool returned `status: "no_index"`, or the user
   asked to set up rag-rat. Confirm there is no `rag-rat.toml` at (or above) the repo root. If one
   already exists, this is a *reconfigure* — send the user to the interactive `rag-rat init` wizard
   and stop.

2. **Resolve the CLI** and use the same form throughout, from the **target repo's root**. Match the
   version the rag-rat MCP server runs, so the index you create is one the server can read — do
   **not** use `@latest`:
   - `rag-rat …` if it is on `PATH`;
   - otherwise `npx -y @rag-rat/bin@0.16.0 …` (the plugin pins `@rag-rat/bin` to its own version and
     caches the binary privately, so `rag-rat` is often not on `PATH`; `npx` runs it in the current
     directory).

3. **Dry-run discovery.** `<rag-rat> init --yes --dry-run` scans the repo and prints the
   auto-detected config (languages, path bindings, the default local FastEmbed backend) **without
   writing**. Read it; tell the user the material bits in a line or two (which languages and
   directories) — not every commented knob.

4. **Default to FastEmbed unless the machine is powerful (or the repo is large).** FastEmbed
   (all-MiniLM, 384-dim, local CPU) needs zero setup and is the right call for most repos on a modest
   workstation. Recommend it by default. Only raise a remote / GPU embedder when it earns its keep: a
   **large repo**, a wish for a **stronger code-specific embedder**, or an available **GPU / cloud
   budget**.

5. **Only if a remote embedder makes sense, load the relevant reference and configure it.** Start
   with the overview, then the one that fits — read the file into context on demand; do not inline
   its content here:
   - **`references/remote-embeddings.md`** — the `[llm.embedding.remote]` block, the local↔remote
     model pairing table, dim parity, and which of the two paths below to take.
   - **`references/local-infinity.md`** — run a local infinity server via Docker (Connect mode); the
     recommended default model + the CPU engine footgun.
   - **`references/ephemeral-providers.md`** — provision an ephemeral Modal/RunPod GPU worker
     (cookbook); GPU classes, the cost/trust boundary, and the local+cloud hybrid.

6. **Preview and confirm.** The two paths apply differently — pick one and confirm with the user
   before writing anything:
   - **FastEmbed (default):** `<rag-rat> init --yes` writes `rag-rat.toml` **and runs the initial
     index in one step.** Preview it first with `<rag-rat> init --yes --dry-run` if the user wants to
     see the config. No hand-editing, and **no separate index step** (step 7 does not re-index).
   - **Custom remote backend:** take the base from `<rag-rat> init --yes --dry-run` (which writes
     nothing), add the `[llm.embedding.remote]` block from step 5, show the final config, and write
     `rag-rat.toml` only after the user confirms.

7. **Index (custom path only), install the git hooks, then restart.**
   - **FastEmbed:** the index already ran in step 6 — **do not run `index --discover` again.**
   - **Custom remote backend:** validate the written config by loading it — `<rag-rat> doctor` fails
     loudly on a bad block (dim mismatch, `gpu` with `endpoint`, missing `query_endpoint`, …) — then
     index **once**: `<rag-rat> index --discover`. (Never index with FastEmbed and then re-embed
     remotely — configure first, index once.)
   - **Install the git maintenance hooks** (both paths): `<rag-rat> hooks install`. These are managed
     `post-checkout` / `post-merge` / `post-rewrite` / `post-commit` hooks that keep the index fresh
     automatically as the repo changes — without them it drifts between manual reindexes. It is
     idempotent and **never clobbers a foreign hook**: if a non-rag-rat hook already occupies a slot
     it errors ("move it aside or merge manually") rather than overwriting — surface that to the user
     and move on. (`init --yes` already installs them on the FastEmbed path, so re-running is
     harmless.) Skip only if the repo isn't a git worktree.
   - **Then restart the rag-rat MCP server** in the agent. A dormant server does not self-activate
     (that would be a half-active server without the watcher and hook listener). After restart it
     discovers `rag-rat.toml`, starts fully active, and ordinary tools work against the new index.

## Guardrails

- **Default to FastEmbed.** Don't push a remote backend onto a small repo or a modest machine.
- **localhost-only** for any local embedder — never bind it to a public interface.
- **Same model + dim everywhere.** Mixing embedding models or dims across local and ephemeral
  corrupts the vector space; `[llm.embedding] model` must be a registry model whose dim matches what
  the server returns. A mismatch is rejected at load.
- **Cost + trust.** Never start a cookbook / ephemeral (cloud) run without explicit go-ahead — it
  spends money and runs third-party provisioning code with the user's cloud credentials.
- **Never write config blind.** Always go through `init --yes --dry-run` for the base and a real load
  (`doctor` / `index`) for validation.
- **Keep the machine-global database.** Never add an `[index] database` key — the keyless config
  `init` produces resolves to the global store, which is what lets the index and memories survive a
  `git clean` or a deleted checkout. A per-repo `database` path is the deprecated, un-consolidated
  deployment (it never syncs); do not introduce one.
