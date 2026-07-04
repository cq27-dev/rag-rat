"""Memory-compaction model eval on Modal.

Candidates summarize the same 35-memory corpus (30 real rag-rat memories + 5 synthetic
negation-trap memories) into 3-4 sentence compactions. Scoring:
  - probe judge (non-candidate Qwen3-14B): TRUE/FALSE/ABSENT per hand-authored claim,
    negation-flips = critical failures
  - HHEM-2.1-open (vectara) windowed max-pool consistency score vs the source
  - deterministic format checks + latency, computed locally afterwards

Run:
  modal run eval_app.py::smoke          # 1 model x 3 items, eyeball output
  modal run eval_app.py::summarize_all  # all candidates, parallel
  modal run eval_app.py::judge_all      # probe judge over all summaries
  modal run eval_app.py::hhem_all       # HHEM scores over all summaries
"""

import json
import pathlib
import time

import modal

app = modal.App("memory-compaction-eval")

CACHE = modal.Volume.from_name("memory-eval-hf-cache", create_if_missing=True)
CACHE_PATH = "/cache"

vllm_image = (
    modal.Image.debian_slim(python_version="3.12")
    .pip_install("vllm", "huggingface_hub")
    .env({"HF_HOME": CACHE_PATH, "VLLM_LOGGING_LEVEL": "WARNING",
          "VLLM_USE_FLASHINFER_SAMPLER": "0"})
)

hhem_image = (
    modal.Image.debian_slim(python_version="3.12")
    .pip_install("transformers==4.46.3", "torch==2.5.1", "sentencepiece")
    .env({"HF_HOME": CACHE_PATH})
)

DATA_DIR = pathlib.Path(__file__).parent.parent  # evals/memory-compaction
REPO_ROOT = pathlib.Path(__file__).parents[3]  # repository root (…/evals/memory-compaction/harness → root)
CORPUS = DATA_DIR / "corpus" / "eval-corpus.json"
RESULTS = DATA_DIR / "results"
RESULTS.mkdir(parents=True, exist_ok=True)

# (model_id, chat_template_kwargs, system_prefix)
CANDIDATES = [
    ("HuggingFaceTB/SmolLM3-3B", None, "/no_think"),
    ("microsoft/Phi-4-mini-instruct", None, None),
    ("Qwen/Qwen3-4B-Instruct-2507", None, None),
    ("unsloth/gemma-3-4b-it", None, None),
    ("Qwen/Qwen3.5-4B", {"enable_thinking": False}, None),
    ("Qwen/Qwen3-8B", {"enable_thinking": False}, None),
    ("unsloth/gemma-3-12b-it", None, None),
    ("microsoft/phi-4", None, None),
]
JUDGE_MODEL = "Qwen/Qwen3-14B"  # deliberately NOT a candidate

SUMMARIZE_PROMPT = """You compact engineering memory notes into high-signal summaries for a code-intelligence index.

Rewrite the note below as exactly 3-4 sentences (at most 90 words) that a coding agent can act on. Preserve with exact polarity: the core claim, its conditions and exceptions (words like ONLY, NEVER, NOT, UNLESS, EXCEPT must keep their meaning), and the load-bearing identifiers (function/table/config names, issue numbers). Do not add facts that are not in the note. Do not soften, generalize, or invert any conditional. Output only the summary text.

TITLE: {title}
NOTE:
{body}"""

JUDGE_PROMPT = """You are verifying a compressed summary against factual probe claims. Read ONLY the summary below; ignore any outside knowledge. For each numbered claim decide:
- TRUE: the summary states or clearly implies the claim
- FALSE: the summary states or clearly implies the OPPOSITE of the claim
- ABSENT: the summary does not contain enough information to decide

Summary:
{summary}

Claims:
{claims}

Answer with exactly one line per claim in the form `<number>: TRUE` or `<number>: FALSE` or `<number>: ABSENT`. No other text."""


def strip_think(text: str) -> str:
    if "</think>" in text:
        text = text.split("</think>", 1)[1]
    return text.strip()


@app.function(image=vllm_image, gpu="L40S", volumes={CACHE_PATH: CACHE}, timeout=3600)
def run_model(model_id: str, chat_kwargs: dict | None, system_prefix: str | None,
              prompts: list[str], max_tokens: int) -> dict:
    """Batch-generate with one model. Returns summaries + timings, or an error record."""
    from vllm import LLM, SamplingParams

    t0 = time.monotonic()
    try:
        llm = LLM(model=model_id, max_model_len=8192, dtype="bfloat16",
                  gpu_memory_utilization=0.92, enforce_eager=False)
    except Exception as e:  # engine/arch unsupported -> report, don't crash the sweep
        return {"model": model_id, "error": f"engine init failed: {e!r}"}
    load_s = time.monotonic() - t0

    conversations = []
    for p in prompts:
        msgs = []
        if system_prefix:
            msgs.append({"role": "system", "content": system_prefix})
        msgs.append({"role": "user", "content": p})
        conversations.append(msgs)

    sp = SamplingParams(temperature=0.0, max_tokens=max_tokens)
    t1 = time.monotonic()
    try:
        kwargs = {"chat_template_kwargs": chat_kwargs} if chat_kwargs else {}
        outs = llm.chat(conversations, sp, **kwargs)
    except Exception as e:
        return {"model": model_id, "error": f"generation failed: {e!r}", "load_s": load_s}
    gen_s = time.monotonic() - t1

    texts = [strip_think(o.outputs[0].text) for o in outs]
    CACHE.commit()
    return {"model": model_id, "load_s": round(load_s, 1), "gen_s": round(gen_s, 1),
            "outputs": texts}


@app.function(image=hhem_image, gpu="T4", volumes={CACHE_PATH: CACHE}, timeout=1800)
def hhem_scores(pairs: list[tuple[str, str]]) -> list[float]:
    """HHEM-2.1-open consistency scores. Long premises: windowed max-pool (approximate)."""
    from transformers import AutoModelForSequenceClassification

    model = AutoModelForSequenceClassification.from_pretrained(
        "vectara/hallucination_evaluation_model", trust_remote_code=True)
    model = model.to("cuda")

    def windows(text: str, size: int = 350, stride: int = 175) -> list[str]:
        words = text.split()
        if len(words) <= size:
            return [text]
        return [" ".join(words[i:i + size]) for i in range(0, len(words) - stride, stride)]

    scores = []
    for premise, hypothesis in pairs:
        ws = windows(premise)
        batch = [(w, hypothesis) for w in ws]
        s = model.predict(batch)
        scores.append(float(max(s)))
    CACHE.commit()
    return scores


# ---------------------------------------------------------------------------
# v2: reference-free prompt + anchor-context / tool-navigation variants
# ---------------------------------------------------------------------------

V2_MODELS = [
    ("Qwen/Qwen3-4B-Instruct-2507", None, None),
    ("unsloth/gemma-3-12b-it", None, None),
]

REF_RULE = """
Self-containment rule: the reader of your summary can see the codebase but CANNOT see issue trackers or review threads. Do not cite issue numbers, PR numbers, phase labels, or review-round labels (e.g. "#330-6", "PR #414", "phase A5", "R2", "round-6") — state the fact they stand for instead. In-code identifiers (function/table/config names, migration names like V042) must be kept. If the note describes a bug and its fix, state the post-fix behavior as current."""

SUMMARIZE_PROMPT_V2 = SUMMARIZE_PROMPT.replace(
    "Output only the summary text.", REF_RULE.strip() + "\n\nOutput only the summary text.")

ANCHOR_SECTION = """

ANCHORED CODE (excerpts of the source this note is bound to, for grounding only — summarize the NOTE, not the code):
{anchor}"""

TOOL_RULES = """

Before writing the summary you may investigate the code this note is anchored to, using AT MOST 3 tool calls total. To call a tool, reply with EXACTLY one line and nothing else:
CALL grep <pattern>            — regex-search the repository source; returns up to 25 matching lines as path:line:text
CALL read <path> <start> <end> — read up to 100 lines of a source file
If the note is already self-sufficient, skip the tools and write the summary immediately. Never mix a CALL line with summary text."""

# /repo mounts the current checkout's crates; /repo-drift mounts the doctored tree that
# `make-drift-tree.py` produces (regenerate it before running the tool-based verify entrypoints).
vllm_repo_image = (
    vllm_image
    .add_local_dir(str(REPO_ROOT / "crates"), "/repo/crates", copy=True)
    .add_local_dir(str(DATA_DIR / "drift-crates"), "/repo-drift/crates", copy=True)
)


def _tool_grep(pattern: str, root: str = "/repo") -> str:
    import re as _re
    try:
        rx = _re.compile(pattern)
    except _re.error as e:
        return f"bad regex: {e}"
    hits = []
    root = pathlib.Path(root)
    for p in sorted(root.rglob("*.rs")):
        try:
            for n, line in enumerate(p.read_text(errors="replace").splitlines(), 1):
                if rx.search(line):
                    hits.append(f"{p.relative_to(root)}:{n}:{line.strip()[:160]}")
                    if len(hits) >= 25:
                        return "\n".join(hits)
        except OSError:
            continue
    return "\n".join(hits) if hits else "(no matches)"


def _tool_read(path: str, start: int, end: int, root: str = "/repo") -> str:
    p = pathlib.Path(root) / path.lstrip("/")
    if not p.is_file():
        return f"not a file: {path}"
    lines = p.read_text(errors="replace").splitlines()
    end = min(end, start + 99, len(lines))
    seg = lines[max(0, start - 1):end]
    return "\n".join(f"{i}: {l}" for i, l in enumerate(seg, start))


def _parse_call(text: str):
    # Returns None for "not a CALL", else a fixed-length 4-tuple (tag, a, b, c) so callers never
    # branch on tuple arity: grep uses (tag, pattern, None, None); read uses (tag, path, start, end);
    # malformed uses (tag, None, None, None).
    t = text.strip()
    if not t.startswith("CALL "):
        return None
    body = t[5:].strip()
    if body.startswith("grep "):
        return ("grep", body[5:].strip(), None, None)
    if body.startswith("read "):
        parts = body[5:].split()
        if len(parts) >= 3:
            try:
                return ("read", parts[0], int(parts[1]), int(parts[2]))
            except ValueError:
                return ("malformed", None, None, None)
    return ("malformed", None, None, None)


@app.function(image=vllm_repo_image, gpu="L40S", volumes={CACHE_PATH: CACHE}, timeout=3600)
def run_model_tools(model_id: str, chat_kwargs: dict | None, prompts: list[str],
                    max_tokens: int, tool_root: str = "/repo", max_calls: int = 3) -> dict:
    """Round-batched tool loop: each round generates for every unfinished conversation,
    executes CALL lines, appends results, until summary or budget exhaustion."""
    from vllm import LLM, SamplingParams

    t0 = time.monotonic()
    llm = LLM(model=model_id, max_model_len=16384, dtype="bfloat16",
              gpu_memory_utilization=0.92)
    load_s = time.monotonic() - t0

    convs = [[{"role": "user", "content": p}] for p in prompts]
    done: dict[int, str] = {}
    calls_used = [0] * len(prompts)
    call_log: list[list[str]] = [[] for _ in prompts]
    sp = SamplingParams(temperature=0.0, max_tokens=max_tokens)
    kwargs = {"chat_template_kwargs": chat_kwargs} if chat_kwargs else {}

    t1 = time.monotonic()
    for _round in range(max_calls + 2):
        pending = [i for i in range(len(prompts)) if i not in done]
        if not pending:
            break
        outs = llm.chat([convs[i] for i in pending], sp, **kwargs)
        for i, o in zip(pending, outs):
            text = strip_think(o.outputs[0].text)
            call = _parse_call(text)
            if call is None or calls_used[i] >= max_calls:
                done[i] = text
                continue
            convs[i].append({"role": "assistant", "content": text})
            if call[0] == "grep":
                result = _tool_grep(call[1], tool_root)
            elif call[0] == "read":
                result = _tool_read(call[1], call[2], call[3], tool_root)
            else:
                result = "malformed call"
            calls_used[i] += 1
            call_log[i].append(text.strip()[:120])
            suffix = ("\nTool budget exhausted. Write the final answer now."
                      if calls_used[i] >= max_calls else "")
            convs[i].append({"role": "user", "content": f"TOOL RESULT:\n{result}{suffix}"})
    for i in range(len(prompts)):  # round cap safety
        done.setdefault(i, "(no final summary produced)")
    gen_s = time.monotonic() - t1
    CACHE.commit()
    return {"model": model_id, "load_s": round(load_s, 1), "gen_s": round(gen_s, 1),
            "outputs": [done[i] for i in range(len(prompts))],
            "calls_used": calls_used, "call_log": call_log}


TEMPORAL_RULE = """

TEMPORAL RULE: the NOTE and the ANCHORED CODE are snapshots from different times — either may be the newer one (the note may describe in-flight work the code does not show yet, or the code may have moved past the note). The NOTE is the record you are compacting: summarize the NOTE faithfully even where the code disagrees, and never silently blend the two. If the anchored code clearly contradicts a factual claim in the note, append ONE final sentence in exactly this form: [CODE-DRIFT: <one clause stating what the code currently shows>]. If there is no clear contradiction, do not emit a CODE-DRIFT sentence."""


@app.local_entrypoint()
def summarize_v2d():
    items = json.loads(CORPUS.read_text())
    anchors = json.loads((DATA_DIR / "corpus" / "anchors.json").read_text())
    base = [SUMMARIZE_PROMPT_V2.format(title=i["title"], body=i["body"]) for i in items]
    prompts = [
        p + (ANCHOR_SECTION.format(anchor=anchors[i["id"]]) + TEMPORAL_RULE
             if i["id"] in anchors else "")
        for p, i in zip(base, items)]
    for model, ck, sp_ in V2_MODELS:
        r = run_model.remote(model, ck, sp_, prompts, 300)
        r["variant"] = "v2d"
        slug = model.replace("/", "__")
        (RESULTS / f"summaries-v2d-{slug}.json").write_text(json.dumps(r, indent=1))
        print(f"v2d {model}: " + (f"ERROR {r['error'][:80]}" if "error" in r
                                  else f"ok gen={r['gen_s']}s"))


@app.local_entrypoint()
def drift_test():
    items = json.loads(CORPUS.read_text())
    drift = json.loads((DATA_DIR / "corpus" / "drift-anchors.json").read_text())
    sel = [i for i in items if i["id"] in drift]
    base = [SUMMARIZE_PROMPT_V2.format(title=i["title"], body=i["body"]) for i in sel]
    control = [p + ANCHOR_SECTION.format(anchor=drift[i["id"]])
               for p, i in zip(base, sel)]                       # v2b shape: no temporal rule
    treated = [p + ANCHOR_SECTION.format(anchor=drift[i["id"]]) + TEMPORAL_RULE
               for p, i in zip(base, sel)]
    out = []
    for model, ck, sp_ in V2_MODELS:
        for label, prompts in [("control-no-rule", control), ("temporal-rule", treated)]:
            r = run_model.remote(model, ck, sp_, prompts, 300)
            if "error" in r:
                print(f"{model} {label}: ERROR {r['error'][:80]}")
                continue
            for it, s in zip(sel, r["outputs"]):
                out.append({"model": model, "prompt": label, "item": it["id"], "summary": s})
            print(f"{model} {label}: ok")
    (RESULTS / "drift-summaries.json").write_text(json.dumps(out, indent=1))


VERIFY_PROMPT = """You are auditing a repo-intelligence memory NOTE against the repository as it exists RIGHT NOW, reachable through your tools. The note was written at some point in the past. The codebase may have moved past it, the note may describe in-flight work not yet present in this checkout, or the two may agree.

Investigate with the tools (AT MOST 5 calls), then output your verdict in EXACTLY this format and nothing else:
VERDICT: current | diverged | unverifiable
DIRECTION: code_ahead | note_ahead | unknown
EVIDENCE:
- <path>:<line>: "<exact line copied verbatim from a tool result you received>"
REASON: <one sentence>

Meanings: current = the note's load-bearing claims are visible in the code as described. diverged = the code clearly contradicts a load-bearing claim (code_ahead: the code changed after the note was written; note_ahead: the note describes work this checkout does not contain yet). unverifiable = the paths/symbols the note names cannot be found at all. DIRECTION is "unknown" unless VERDICT is diverged and you can tell which side is newer. Give 1-3 EVIDENCE lines; every quote must be copied verbatim from a tool result — never invent one.

To call a tool, reply with EXACTLY one line and nothing else:
CALL grep <pattern>            — regex-search the repository source (up to 25 hits as path:line:text)
CALL read <path> <start> <end> — read up to 100 lines of a source file

NOTE (anchored to {binding}):
TITLE: {title}
{body}"""

# (item_id, expected_verdict, expected_direction_or_None, root)
# The MODEL-verdict accuracy manifest: the 15 cases the shipped 2-way (current | diverged) prompt is
# graded on. The two `unverifiable` synthetics are NOT here — they are decided deterministically in
# pass 0 (verify.rs `unverifiable_findings`) and never reach the model, so they live in
# PASS0_UNVERIFIABLE below, excluded from the model gate.
VERIFY_MANIFEST = [
    ("real_16", "current", None, "/repo"), ("real_17", "current", None, "/repo"),
    ("real_20", "current", None, "/repo"), ("real_21", "current", None, "/repo"),
    ("real_22", "current", None, "/repo"), ("real_26", "current", None, "/repo"),
    ("real_27", "current", None, "/repo"), ("real_29", "current", None, "/repo"),
    ("real_0", "diverged", "note_ahead", "/repo"), ("real_1", "diverged", "note_ahead", "/repo"),
    ("real_3", "diverged", "note_ahead", "/repo"), ("real_9", "diverged", "note_ahead", "/repo"),
    ("real_14", "diverged", "note_ahead", "/repo"),
    ("real_22", "diverged", "code_ahead", "/repo-drift"),
    ("real_27", "diverged", "code_ahead", "/repo-drift"),
]

# Deterministic pass-0 cases: the fictional synthetics whose named modules resolve NOWHERE, decided
# by verify.rs `unverifiable_findings` with no model. Kept for the historical agentic 3-way arm
# (`verify_test`, which CAN emit `unverifiable`), but EXCLUDED from the shipped model-verdict gate —
# the 2-way pack prompt never emits `unverifiable`, so scoring these against it is meaningless.
PASS0_UNVERIFIABLE = [
    ("syn_0", "unverifiable", None, "/repo"), ("syn_2", "unverifiable", None, "/repo"),
]


@app.local_entrypoint()
def verify_test():
    items = json.loads(CORPUS.read_text())
    by_id = {it["id"]: it for it in items}
    mem_full = json.loads((DATA_DIR / "corpus" / "memories-full.json").read_text())
    bind_of = {f"real_{i}": (m.get("bindings") or [{}])[0].get("path", "(unknown)")
               for i, m in enumerate(mem_full)}
    out = []
    for model, ck, _sp in V2_MODELS:
        for root in ["/repo", "/repo-drift"]:
            # The historical 3-way research arm covers the pass-0 unverifiable cases too (its prompt
            # can emit `unverifiable`); the shipped 2-way pack gate below does not.
            rows = [r for r in VERIFY_MANIFEST + PASS0_UNVERIFIABLE if r[3] == root]
            prompts = [VERIFY_PROMPT.format(
                binding=bind_of.get(iid, "(no source binding — a conceptual note)"),
                title=by_id[iid]["title"], body=by_id[iid]["body"])
                for iid, _, _, _ in rows]
            r = run_model_tools.remote(model, ck, prompts, 400,
                                       tool_root=root, max_calls=5)
            if "error" in r:
                print(f"{model} {root}: ERROR {r['error'][:80]}")
                continue
            for (iid, exp_v, exp_d, _), ans, cu in zip(rows, r["outputs"], r["calls_used"]):
                out.append({"model": model, "root": root, "item": iid,
                            "expected": exp_v, "expected_direction": exp_d,
                            "calls_used": cu, "answer": ans})
            print(f"{model} {root}: ok ({len(rows)} items, "
                  f"{sum(r['calls_used'])} tool calls)")
    (RESULTS / "verify-results.json").write_text(json.dumps(out, indent=1))


# Mirrors the SHIPPED evidence-pack verdict prompt: dream/verdict.rs `VERDICT_PROMPT_HEAD` +
# `render_verdict_prompt` tail (PROMPT_VERSION "verify-pack-v1"). It is a 2-way verdict (current |
# diverged) — `unverifiable` is decided deterministically in pass 0 and NEVER asked of the model, so
# it is absent here. RE-SYNC this string whenever PROMPT_VERSION bumps in dream/verdict.rs.
VERIFY_PACK_PROMPT = """You are auditing a repo-intelligence memory NOTE against the repository as it exists RIGHT NOW. You are given a mechanically-generated EVIDENCE PACK from the current checkout: a whole-tree resolution of every identifier the note mentions (an identifier marked "NOT FOUND anywhere in the source tree" truly does not exist in the source — this is exhaustive, not a failed search), and the current text of the note's bound file. The note was written in the past: the code may have moved past it, or the note may describe in-flight work not present in this checkout, or they may agree.

Output EXACTLY this format and nothing else:
VERDICT: current | diverged
DIRECTION: code_ahead | note_ahead | unknown
EVIDENCE:
- <one line copied verbatim from the EVIDENCE PACK below that supports the verdict>
REASON: <one sentence>

Meanings: current = the note's load-bearing claims are visible in the pack as described. diverged = the pack clearly contradicts a load-bearing claim. code_ahead = the code changed after the note was written. note_ahead = the note describes work this checkout does not contain yet — for example, the mechanisms or functions the note says were added are marked NOT FOUND WHILE the note's bound file DOES exist; that is diverged / note_ahead, NOT a reason to give up. DIRECTION is "unknown" unless VERDICT is diverged and you can tell which side is newer. Every EVIDENCE line must be copied verbatim from the EVIDENCE PACK below — never invent one.

NOTE (anchored to {binding}):
TITLE: {title}
{body}

EVIDENCE PACK:
{pack}"""


@app.local_entrypoint()
def verify_pack_test():
    items = json.loads(CORPUS.read_text())
    by_id = {it["id"]: it for it in items}
    mem_full = json.loads((DATA_DIR / "corpus" / "memories-full.json").read_text())
    bind_of = {f"real_{i}": (m.get("bindings") or [{}])[0].get("path", "(unknown)")
               for i, m in enumerate(mem_full)}
    packs = json.loads((DATA_DIR / "corpus" / "verify-packs.json").read_text())
    rows = VERIFY_MANIFEST
    prompts = [VERIFY_PACK_PROMPT.format(
        binding=bind_of.get(iid, "(no source binding — a conceptual note)"),
        title=by_id[iid]["title"], body=by_id[iid]["body"],
        pack=packs[f"{iid}|{root}"])
        for iid, _, _, root in rows]
    out = []
    for model, ck, sp_ in V2_MODELS:
        r = run_model.remote(model, ck, sp_, prompts, 350)
        if "error" in r:
            print(f"{model}: ERROR {r['error'][:80]}")
            continue
        for (iid, exp_v, exp_d, root), ans in zip(rows, r["outputs"]):
            out.append({"model": model, "root": root, "item": iid, "expected": exp_v,
                        "expected_direction": exp_d, "calls_used": 0, "answer": ans})
        print(f"{model}: ok gen={r['gen_s']}s")
    (RESULTS / "verify-pack-results.json").write_text(json.dumps(out, indent=1))


@app.local_entrypoint()
def judge_v2d():
    import re as _re
    items = json.loads(CORPUS.read_text())
    judge_prompts, keys = [], []
    for f in sorted(RESULTS.glob("summaries-v2d-*.json")):
        r = json.loads(f.read_text())
        for item, summary in zip(items, r["outputs"]):
            if not item["probes"]:
                continue
            stripped = _re.sub(r"\[CODE-DRIFT:[^\]]*\]", "", summary).strip()
            claims = "\n".join(f"{j + 1}. {p['claim']}" for j, p in enumerate(item["probes"]))
            judge_prompts.append(JUDGE_PROMPT.format(summary=stripped, claims=claims))
            keys.append({"model": r["model"], "variant": "v2d", "item": item["id"]})
    r = run_model.remote(JUDGE_MODEL, {"enable_thinking": False}, None, judge_prompts, 300)
    if "error" in r:
        raise SystemExit(f"judge failed: {r['error']}")
    out = [{**k, "verdict_raw": v} for k, v in zip(keys, r["outputs"])]
    (RESULTS / "judge-verdicts-v2d.json").write_text(json.dumps(out, indent=1))
    print(f"wrote {len(out)} verdicts")


@app.local_entrypoint()
def summarize_v2():
    items = json.loads(CORPUS.read_text())
    anchors = json.loads((DATA_DIR / "corpus" / "anchors.json").read_text())
    base = [SUMMARIZE_PROMPT_V2.format(title=i["title"], body=i["body"]) for i in items]
    with_anchor = [
        p + (ANCHOR_SECTION.format(anchor=anchors[i["id"]]) if i["id"] in anchors else "")
        for p, i in zip(base, items)]
    with_tools = [p + TOOL_RULES for p in base]
    RESULTS.mkdir(exist_ok=True)
    calls = []
    for model, ck, sp_ in V2_MODELS:
        calls.append((run_model.spawn(model, ck, sp_, base, 260), model, "v2a"))
        calls.append((run_model.spawn(model, ck, sp_, with_anchor, 260), model, "v2b"))
        calls.append((run_model_tools.spawn(model, ck, with_tools, 260), model, "v2c"))
    for call, model, variant in calls:
        r = call.get()
        r["variant"] = variant
        slug = model.replace("/", "__")
        (RESULTS / f"summaries-{variant}-{slug}.json").write_text(json.dumps(r, indent=1))
        status = f"ERROR: {r['error'][:100]}" if "error" in r else \
            f"ok load={r['load_s']}s gen={r['gen_s']}s"
        if "calls_used" in r:
            status += f" tool-calls={sum(r['calls_used'])}"
        print(f"{variant} {model}: {status}")


@app.local_entrypoint()
def judge_v2():
    items = json.loads(CORPUS.read_text())
    judge_prompts, keys = [], []
    for f in sorted(RESULTS.glob("summaries-v2*-*.json")):
        r = json.loads(f.read_text())
        if "error" in r:
            continue
        for item, summary in zip(items, r["outputs"]):
            if not item["probes"]:
                continue
            claims = "\n".join(f"{j + 1}. {p['claim']}" for j, p in enumerate(item["probes"]))
            judge_prompts.append(JUDGE_PROMPT.format(summary=summary, claims=claims))
            keys.append({"model": r["model"], "variant": r["variant"], "item": item["id"]})
    print(f"judging {len(judge_prompts)} pairs with {JUDGE_MODEL}")
    r = run_model.remote(JUDGE_MODEL, {"enable_thinking": False}, None, judge_prompts, 300)
    if "error" in r:
        raise SystemExit(f"judge failed: {r['error']}")
    out = [{**k, "verdict_raw": v} for k, v in zip(keys, r["outputs"])]
    (RESULTS / "judge-verdicts-v2.json").write_text(json.dumps(out, indent=1))
    print(f"wrote {len(out)} verdicts, gen={r['gen_s']}s")


@app.local_entrypoint()
def smoke():
    items = json.loads(CORPUS.read_text())[:3]
    prompts = [SUMMARIZE_PROMPT.format(title=i["title"], body=i["body"]) for i in items]
    r = run_model.remote("HuggingFaceTB/SmolLM3-3B", None, "/no_think", prompts, 260)
    print(json.dumps(r, indent=1)[:4000])


@app.local_entrypoint()
def summarize_all():
    items = json.loads(CORPUS.read_text())
    prompts = [SUMMARIZE_PROMPT.format(title=i["title"], body=i["body"]) for i in items]
    RESULTS.mkdir(exist_ok=True)
    calls = [(run_model.spawn(m, ck, sp, prompts, 260), m) for m, ck, sp in CANDIDATES]
    for call, model in calls:
        r = call.get()
        slug = model.replace("/", "__")
        (RESULTS / f"summaries-{slug}.json").write_text(json.dumps(r, indent=1))
        status = f"ERROR: {r['error'][:120]}" if "error" in r else \
            f"ok load={r['load_s']}s gen={r['gen_s']}s"
        print(f"{model}: {status}")


@app.local_entrypoint()
def judge_all():
    items = json.loads(CORPUS.read_text())
    judge_prompts, keys = [], []
    for f in sorted(RESULTS.glob("summaries-*.json")):
        r = json.loads(f.read_text())
        if "error" in r:
            continue
        for item, summary in zip(items, r["outputs"]):
            if not item["probes"]:
                continue
            claims = "\n".join(f"{j + 1}. {p['claim']}" for j, p in enumerate(item["probes"]))
            judge_prompts.append(JUDGE_PROMPT.format(summary=summary, claims=claims))
            keys.append({"model": r["model"], "item": item["id"]})
    print(f"judging {len(judge_prompts)} (model,item) pairs with {JUDGE_MODEL}")
    r = run_model.remote(JUDGE_MODEL, {"enable_thinking": False}, None, judge_prompts, 300)
    if "error" in r:
        raise SystemExit(f"judge failed: {r['error']}")
    out = [{**k, "verdict_raw": v} for k, v in zip(keys, r["outputs"])]
    (RESULTS / "judge-verdicts.json").write_text(json.dumps(out, indent=1))
    print(f"wrote {len(out)} verdicts, gen={r['gen_s']}s")


@app.local_entrypoint()
def hhem_all():
    items = json.loads(CORPUS.read_text())
    out = {}
    for f in sorted(RESULTS.glob("summaries-*.json")):
        r = json.loads(f.read_text())
        if "error" in r:
            continue
        pairs = [(i["body"], s) for i, s in zip(items, r["outputs"])]
        out[r["model"]] = hhem_scores.remote(pairs)
        print(f"{r['model']}: mean HHEM {sum(out[r['model']]) / len(pairs):.3f}")
    (RESULTS / "hhem-scores.json").write_text(json.dumps(out, indent=1))
