# Dream verdict model (`[llm.dream]` / `[llm.dream.remote]`)

Part of the [config reference](../config.md).

`[llm.dream]` configures the dream verify/compaction passes' **model** — rag-rat's only
generative-model dependency. It is **out-of-process, opt-in, and gated by a deterministic layer**:
`rag-rat dream` stays 100% deterministic unless you pass `--verify`/`--compact` *and* set
`enabled = true`. The serving config lives under `[llm.dream.remote]`, mirroring
`[llm.embedding.remote]` — a **connect** endpoint (an already-running chat server) or an
**ephemeral** cookbook-provisioned GPU box.

```toml
[llm.dream]
enabled = false             # off by default — the model turn is skipped

# CONNECT mode (default when [llm.dream.remote] is omitted): a local Ollama.
[llm.dream.remote]
backend  = "ollama"
endpoint = "http://localhost:11434"     # OpenAI-compatible chat server
model    = "qwen3:4b-instruct"          # server-side model name
request_timeout_s = 300
```

For a dense evidence pack, CPU inference is pathologically slow (a large bound file can blow past the
timeout and never verify). Run the model on an **ephemeral remote GPU** instead — the same cookbook
mechanism embeddings use — by setting `cookbook` + `gpu` instead of `endpoint`:

```toml
[llm.dream.remote]
backend  = "vllm"                       # ollama | vllm  (infinity is embed-only — rejected here)
cookbook = "@rag-rat/cookbook modal"    # provision an ephemeral box (mutually exclusive with endpoint)
gpu      = "A10G"                        # provider-specific GPU class (validated at provision time)
model    = "Qwen/Qwen3-8B"              # HF id for vllm (an ollama name for the ollama backend)
auth_env = "MODAL_TOKEN"                # optional: env var holding the box's bearer token
request_timeout_s = 900
```

The box is provisioned only when there is pending work (a zero-work guard never cold-starts a paid
GPU for a fully churn-skipped repo) and is torn down when the run ends. The client speaks the
standard `/v1/chat/completions` route (temperature 0, no streaming). Run it with:

```bash
rag-rat dream --verify --max-memories 20
```

`--verify` always runs the **deterministic** verification findings (`memory_unverifiable`). When
`enabled = true`, it additionally runs the model verdict pass: for each memory in a **churn-skip
queue** (only memories whose body or bound files changed, or that were never checked — capped by
`--max-memories`), it renders a mechanical evidence pack, asks the model for a `current | diverged`
verdict (a fabricated citation is rejected, retried once, then discarded), and records the verdict in
the derived `memory_reality` table. A `diverged` verdict opens a `memory_divergence` finding for the
review flow. The model **proposes**; it never changes a memory's status. Leaving `enabled = false`
skips the model turn entirely — no network calls, deterministic findings only.

`--compact` (independent of `--verify`, and gated the same way on `[llm.dream] enabled = true`)
runs the **compaction pass**, which rewrites each un-summarized memory into a 3–5 sentence,
self-contained summary and stores it in the derived `memory_summaries` table:

```bash
rag-rat dream --compact --max-memories 20
```

It is a churn-skip queue too — a memory is (re)compacted only when it has no summary for its
**current** body (a body edit self-invalidates the old one) **and** that body is longer than the
summary envelope. A note already short enough to show whole is never queued: summarizing it could
only make it longer, so the summary surface shows its body verbatim instead. The summary is
generated from the note body alone (no code context, no tools — measured to give the highest
fidelity), and it must pass deterministic acceptance guards (3–5 sentences, ≤150 words, no
paragraph breaks, and every tracker reference it cites must resolve in the indexed papertrail); a
failing summary is retried once, then dropped. No row is stored, so an over-envelope memory keeps
deferring its body behind the one-line expand marker until a later run succeeds. The pass **never**
writes a `repo_memories` column.

## Scheduling the passes (nightly systemd timer / cron)

The verify and compaction passes are a periodic memory-maintenance chore — run them on a schedule so
verdicts and summaries stay fresh as memories churn. Two things shape a good schedule:

- **Run both passes in one invocation** — `rag-rat dream --verify --compact`. A single run provisions
  the ephemeral GPU box *once* and runs both passes against it; two separate jobs pay two cold starts
  (image pull + model load) for the same work. The zero-work guard means an idle run never boots a
  box, so scheduling generously costs nothing when there is nothing to do.
- **`--max-memories` caps *each* model pass over its churn-skip queue — it is per pass, not a shared
  total.** An up-to-date memory is skipped: `--compact` keys that on the body + prompt version, while
  `--verify` *also* re-enqueues when a memory's bound files or cited identifiers change (evidence
  drift), so a nightly run after ordinary code churn still does verify work even if no memory was
  edited. Because the cap is per pass, `--verify --compact --max-memories 200` can drive up to 200
  verifications **and** 200 summaries (~400 model calls) on a full backlog — likely more than one
  box's 30-minute max-lifetime finishes in one run, so a large initial backlog drains over several
  nights (churn-skip resumes where it left off). Size the cap for the per-pass cost you want.

A headless scheduler runs with a **bare environment**, which is where this usually goes wrong. The
run must be able to (a) find the `rag-rat` binary *and* `node`/`npx` (the cookbook recipe shells out
to `npx`), (b) reach the **provider credentials that provision the box** — for Modal, `MODAL_TOKEN_ID`
/ `MODAL_TOKEN_SECRET` in the environment or `~/.modal.toml` (so set `HOME`); these are distinct from
`[llm.dream.remote] auth_env`, which is only the *served endpoint's* bearer token (connect mode, or
the provisioned box's tunnel), not the provisioning credential — and (c) start in the repo whose
`rag-rat.toml` sets `[llm.dream] enabled = true` (the dream config is repo-local; the index store is
machine-global).

A systemd **user** timer, nightly at 04:30:

```ini
# ~/.config/systemd/user/rag-rat-dream.service
[Unit]
Description=rag-rat dream memory maintenance (verify + compact)

[Service]
Type=oneshot
WorkingDirectory=/path/to/your/repo
Environment=HOME=%h
Environment=PATH=%h/.cargo/bin:/path/to/node/bin:/usr/bin:/bin
ExecStart=%h/.cargo/bin/rag-rat dream --verify --compact --max-memories 200
TimeoutStartSec=60min
```

```ini
# ~/.config/systemd/user/rag-rat-dream.timer
[Unit]
Description=Nightly rag-rat dream memory maintenance

[Timer]
OnCalendar=*-*-* 04:30:00
# Catch up on the next boot if the machine was off/asleep at 04:30.
Persistent=true

[Install]
WantedBy=timers.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now rag-rat-dream.timer
systemctl --user list-timers rag-rat-dream.timer   # confirm the next run
loginctl enable-linger "$USER"                      # REQUIRED to fire the timer while logged out
```

`loginctl enable-linger` is easy to forget and load-bearing: without it the user systemd instance is
torn down when your last session ends, and an overnight timer never fires. The **plain-cron
equivalent** is the same command wrapped so `PATH`/`HOME`/cwd are set (cron's environment is even
barer than systemd's).

Either way the run is unattended, so teardown reliability matters. On **Modal**, the cookbook box
carries its own idle + max-lifetime backstops, so a crashed run or a flaked teardown still
self-terminates the box rather than billing a GPU indefinitely. On **RunPod** there is **no
provider-side backstop** — an on-demand pod stops only when the recipe's teardown runs — so a headless
RunPod schedule depends entirely on clean teardown; prefer the Modal cookbook for unattended runs, or
add your own external watchdog.

## Reviewing findings (`rag-rat dream <id> --accept|--dismiss|--reset`)

The worklist `rag-rat dream` prints carries a stable `id` per finding. Dream only *proposes*; a
human (or strong agent) resolves a finding by that id — a full id or an unambiguous prefix
(git-style):

```bash
rag-rat dream                     # print the open worklist (each row has an id + status)
rag-rat dream 3f9a1c22 --accept   # acknowledge it (real, action pending)
rag-rat dream 3f9a1c22 --dismiss  # a false positive / won't-fix — hidden from the worklist
rag-rat dream 3f9a1c22 --reset    # undo a verdict, back to open
rag-rat dream --all               # also list the accepted/dismissed findings (to find one to --reset)
```

A verdict is **preserved across future runs**: a re-run that still reports the finding keeps your
`accepted`/`dismissed` decision, while a finding the code makes obsolete resolves on its own. Only an
`open`/`accepted`/`dismissed` finding is reviewable — a `resolved`/`superseded` one is not (the code
moved on, so there is nothing to act on). Reviewing is repo-scoped and never runs the model.

The same worklist and review actions are available over MCP for a pull-based strong agent: the
`dream` tool returns the ranked worklist (`{ limit?, all? }`; recomputes the deterministic findings
like `rag-rat dream`, but never the opt-in model passes), and `dream_review` applies a verdict
(`{ finding, verdict: "accept" | "dismiss" | "reset" }`).
