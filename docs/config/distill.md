# Distill model (`[llm.distill]` / `[llm.distill.remote]`)

Part of the [config reference](../config.md). For the feature itself, see
[Issue distillation](../distillation.md).

`[llm.distill]` configures the **model half** of issue distillation — the LLM pass that fills a
distilled record's `root_cause` / `decision` / `outcome` fields. Like [`[llm.dream]`](dream.md) it is
**out-of-process, opt-in, and gated**: `rag-rat distill extract` builds the deterministic substrate
with no model, and `rag-rat distill drain` runs the model pass only when `enabled = true`. The
serving config lives under `[llm.distill.remote]`, mirroring `[llm.dream.remote]` — a **connect**
endpoint (an already-running chat server) or an **ephemeral** cookbook-provisioned GPU box.

```toml
[llm.distill]
enabled = false             # off by default — `distill drain` refuses if work is pending and this is false
```

With no `[llm.distill.remote]` block, distill defaults to the **validated 30B ephemeral box** — an
on-demand vLLM GPU provisioned through the cookbook (the same mechanism embeddings and dream use).
Distillation prompts are dense (full commit bodies + the fix diff + coalesced partners + cross-refs),
so a small local model produces poor records and CPU inference is impractical at corpus scale — the
big model earns its place. This is the exact config the full-corpus verification ran on; the
equivalent explicit block:

```toml
# DEFAULT when [llm.distill.remote] is omitted, shown explicitly:
[llm.distill.remote]
backend  = "vllm"                                   # ollama | vllm  (infinity is embed-only — rejected here)
cookbook = "@rag-rat/cookbook modal"                # provision an ephemeral box (mutually exclusive with endpoint)
gpu      = "L40S"                                    # provider-specific GPU class (validated at provision time)
model    = "Qwen/Qwen3-30B-A3B-Instruct-2507-FP8"   # HF id for vllm (an ollama name for the ollama backend)
provision_timeout_s = 1500                           # cold-pull budget; stays under the box's 30-min hard lifetime
request_timeout_s = 240
```

On the model-comparison run this beat a 4B on every content metric (rejected-alternatives coverage,
root-cause coverage, qualified anchors) while ~3× faster and ~20% cheaper per thread (MoE, ~3B
active); a 4B on an `L4` is the constrained fallback. Nothing is provisioned until you set
`enabled = true` **and** the drain has pending work — a zero-work run never cold-starts a paid box.

**Connect to a running server instead.** To point distill at an already-running OpenAI-compatible
chat server (a local Ollama, a shared vLLM, …) rather than provisioning a box, set `endpoint` in
place of `cookbook`:

```toml
[llm.distill.remote]
backend  = "ollama"
endpoint = "http://localhost:11434"     # OpenAI-compatible chat server (mutually exclusive with cookbook)
model    = "qwen3:4b-instruct"          # server-side model name
# auth_env = "DISTILL_TOKEN"            # optional: names the env var holding the bearer token for a protected endpoint
request_timeout_s = 240
```

A local 4B connect is cheap but low-quality on distill's dense prompts — fine for a smoke test, not a
corpus backfill.

**`provision_timeout_s`.** A 30B re-downloads ~30 GB of weights from Hugging Face on a cold box and
can exceed the vLLM boot default (15 min). The default (1500 s / 25 min) gives that cold pull headroom
while staying under the cookbook box's **hard 30-min lifetime** — a box that spends its whole budget
provisioning still has room to serve at least one full `request_timeout_s` request before it
self-destructs. Don't raise it past ~28 min: the box vanishes at 30, so a larger budget only fails the
slow cold start it was meant to cover. The first cold box is slow; a clean teardown publishes the HF +
compile cache markers, so every later box boots in ~2 min.

**`request_timeout_s`.** Keep this **low** (≈240 s, not the 900 s dream uses). A bounded record
generates in well under a minute, but a rare guided-decoding whitespace loop otherwise blocks the
sequential drain for the full timeout; a low cap fails it fast and re-queues the thread. See #874.

## Running the pass

```bash
rag-rat distill extract              # deterministic substrate only — no model, no cost
rag-rat distill drain --limit 100    # extract, then drain up to 100 queued threads through the model
```

The box is provisioned only when there is pending work (a zero-work run never cold-starts a paid
GPU), and torn down when the run ends. The client speaks the standard `/v1/chat/completions` route
(temperature 0, no streaming, guided JSON where the backend supports it).

**Batch long runs.** The cookbook caps a box's lifetime (~30 min), so a single `drain` cannot finish
a large corpus on one box — size `--limit` to one box's serving window (~70–150 threads by thread
size) and loop `distill drain` until the queue empties. The queue is resumable and every completed
record is durable, so a killed box loses nothing. See #875.
