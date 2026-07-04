"""Score v2 variants and compare against v1 for the same models."""

import json
import pathlib
import re
import statistics

BASE = pathlib.Path(__file__).parent.parent
items = json.loads((BASE / "corpus" / "eval-corpus.json").read_text())
by_id = {it["id"]: it for it in items}

LINE_RE = re.compile(r"^\s*(\d+)\s*[:.)]\s*(TRUE|FALSE|ABSENT)\s*$", re.I | re.M)
# external refs: issue/PR numbers, phase/round labels, bare review-round tokens (not V-migrations)
LEAK_RE = re.compile(r"#\d+|\bPR\b|\bphase\s+[A-Z0-9]|\bround[- ]?\d|\b(?!V\d)[APR]\d{1,2}\b")


def parse_verdict(raw, n):
    out = [None] * n
    for m in LINE_RE.finditer(raw):
        i = int(m.group(1)) - 1
        if 0 <= i < n:
            out[i] = m.group(2).upper()
    return out


def sentence_count(text):
    return len(re.findall(r"[.!?](?=\s+[A-Z`“(]|\s*$)", text.strip()))


def score_run(vs, summaries):
    trap_n = trap_flip = true_n = true_hit = true_flip = core_n = core_hit = 0
    flips = []
    for v in vs:
        item = by_id[v["item"]]
        answers = parse_verdict(v["verdict_raw"], len(item["probes"]))
        for p, a in zip(item["probes"], answers):
            if a is None:
                continue
            if p["gold"]:
                true_n += 1
                true_hit += a == "TRUE"
                if p["weight"] == "core":
                    core_n += 1
                    core_hit += a == "TRUE"
                if a == "FALSE":
                    true_flip += 1
                    flips.append((v["item"], "asserted-opposite", p["claim"][:70]))
            else:
                trap_n += 1
                if a == "TRUE":
                    trap_flip += 1
                    flips.append((v["item"], "trap-inverted", p["claim"][:70]))
    fmt_ok = leak = 0
    wlist = []
    for s in summaries.values():
        w = len(s.split())
        wlist.append(w)
        fmt_ok += (3 <= sentence_count(s) <= 4) and w <= 110 and "\n\n" not in s
    leak = sum(1 for s in summaries.values() if LEAK_RE.search(s))
    n = len(summaries)
    return {
        "trap_flip_pct": round(100 * trap_flip / trap_n, 1),
        "false_assert_pct": round(100 * true_flip / true_n, 1),
        "core_cov_pct": round(100 * core_hit / core_n, 1),
        "cov_pct": round(100 * true_hit / true_n, 1),
        "fmt_pct": round(100 * fmt_ok / n, 1),
        "leak_pct": round(100 * leak / n, 1),
        "median_words": int(statistics.median(wlist)),
        "flips": flips,
    }


runs = {}  # (variant, model) -> verdicts
for v in json.loads((BASE / "results" / "judge-verdicts-v2.json").read_text()):
    runs.setdefault((v["variant"], v["model"]), []).append(v)
for v in json.loads((BASE / "results" / "judge-verdicts.json").read_text()):
    if v["model"] in {m for _, m in runs.keys()} or v["model"] in (
            "Qwen/Qwen3-4B-Instruct-2507", "unsloth/gemma-3-12b-it"):
        runs.setdefault(("v1", v["model"]), []).append(v)

summaries = {}
for f in (BASE / "results").glob("summaries-*.json"):
    r = json.loads(f.read_text())
    if "error" in r:
        continue
    variant = r.get("variant", "v1")
    summaries[(variant, r["model"])] = dict(
        zip([it["id"] for it in items], r["outputs"]))

rows = []
for key in sorted(runs):
    variant, model = key
    if key not in summaries:
        continue
    s = score_run(runs[key], summaries[key])
    rows.append({"model": model, "variant": variant, **s})

(BASE / "results" / "scoreboard-v2.json").write_text(json.dumps(rows, indent=1))
hdr = f"{'model':34s} {'var':>4} {'trapflip%':>9} {'flsassert%':>10} {'corecov%':>8} {'cov%':>5} {'fmt%':>6} {'leak%':>6} {'med.w':>6}"
print(hdr)
print("-" * len(hdr))
for r in rows:
    print(f"{r['model'].split('/')[-1]:34s} {r['variant']:>4} {r['trap_flip_pct']:>9} "
          f"{r['false_assert_pct']:>10} {r['core_cov_pct']:>8} {r['cov_pct']:>5} "
          f"{r['fmt_pct']:>6} {r['leak_pct']:>6} {r['median_words']:>6}")
for r in rows:
    if r["flips"]:
        print(f"\n{r['model'].split('/')[-1]} {r['variant']} critical:")
        for f_ in r["flips"]:
            print("  ", f_)
