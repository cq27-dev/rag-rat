"""Fold judge verdicts + HHEM + format checks into the per-model results table."""

import json
import pathlib
import re
import statistics

BASE = pathlib.Path(__file__).parent.parent
items = json.loads((BASE / "corpus" / "eval-corpus.json").read_text())
by_id = {it["id"]: it for it in items}
verdicts = json.loads((BASE / "results" / "judge-verdicts.json").read_text())
hhem = json.loads((BASE / "results" / "hhem-scores.json").read_text())

LINE_RE = re.compile(r"^\s*(\d+)\s*[:.)]\s*(TRUE|FALSE|ABSENT)\s*$", re.I | re.M)


def parse_verdict(raw: str, n: int) -> list[str | None]:
    out: list[str | None] = [None] * n
    for m in LINE_RE.finditer(raw):
        i = int(m.group(1)) - 1
        if 0 <= i < n:
            out[i] = m.group(2).upper()
    return out


def sentence_count(text: str) -> int:
    # Identifier-tolerant: terminator must be followed by whitespace+capital/backtick or EOL.
    return len(re.findall(r"[.!?](?=\s+[A-Z`“(]|\s*$)", text.strip()))


models = {}
for v in verdicts:
    models.setdefault(v["model"], []).append(v)

summaries = {}
for f in (BASE / "results").glob("summaries-*.json"):
    r = json.loads(f.read_text())
    if "error" not in r:
        summaries[r["model"]] = dict(zip([it["id"] for it in items], r["outputs"]))

rows = []
for model, vs in sorted(models.items()):
    trap_n = trap_flip = trap_kept = 0          # gold=False probes
    true_n = true_hit = true_flip = 0           # gold=True probes
    core_n = core_hit = 0
    syn_trap_n = syn_trap_flip = 0
    unparsed = 0
    flips = []
    for v in vs:
        item = by_id[v["item"]]
        probes = item["probes"]
        answers = parse_verdict(v["verdict_raw"], len(probes))
        for p, a in zip(probes, answers):
            if a is None:
                unparsed += 1
                continue
            if p["gold"]:
                true_n += 1
                if p["weight"] == "core":
                    core_n += 1
                    core_hit += a == "TRUE"
                true_hit += a == "TRUE"
                if a == "FALSE":
                    true_flip += 1
                    flips.append((v["item"], "asserted-opposite", p["claim"][:80]))
            else:
                trap_n += 1
                trap_flip += a == "TRUE"
                trap_kept += a == "FALSE"
                if item["synthetic"]:
                    syn_trap_n += 1
                    syn_trap_flip += a == "TRUE"
                if a == "TRUE":
                    flips.append((v["item"], "trap-inverted", p["claim"][:80]))

    fmt_ok = words = 0
    wlist = []
    for iid, s in summaries[model].items():
        w = len(s.split())
        wlist.append(w)
        sc = sentence_count(s)
        fmt_ok += (3 <= sc <= 4) and w <= 110 and "\n\n" not in s
    h = hhem.get(model, [])
    rows.append({
        "model": model,
        "trap_flip_pct": round(100 * trap_flip / trap_n, 1),
        "trap_kept_pct": round(100 * trap_kept / trap_n, 1),
        "false_assert_pct": round(100 * true_flip / true_n, 1),
        "core_cov_pct": round(100 * core_hit / core_n, 1),
        "cov_pct": round(100 * true_hit / true_n, 1),
        "syn_trap_flips": f"{syn_trap_flip}/{syn_trap_n}",
        "hhem": round(statistics.mean(h), 3) if h else None,
        "fmt_pct": round(100 * fmt_ok / len(summaries[model]), 1),
        "median_words": int(statistics.median(wlist)),
        "unparsed": unparsed,
        "critical_examples": flips[:6],
    })

# headline sort: fewest trap flips, then fewest false asserts, then best core coverage
rows.sort(key=lambda r: (r["trap_flip_pct"], r["false_assert_pct"], -r["core_cov_pct"]))
out = BASE / "results" / "scoreboard.json"
out.write_text(json.dumps(rows, indent=1))

hdr = f"{'model':40s} {'trapflip%':>9} {'falseassert%':>12} {'corecov%':>8} {'cov%':>5} {'syntrap':>8} {'hhem':>6} {'fmt%':>6} {'med.w':>6}"
print(hdr)
print("-" * len(hdr))
for r in rows:
    print(f"{r['model']:40s} {r['trap_flip_pct']:>9} {r['false_assert_pct']:>12} "
          f"{r['core_cov_pct']:>8} {r['cov_pct']:>5} {r['syn_trap_flips']:>8} "
          f"{str(r['hhem']):>6} {r['fmt_pct']:>6} {r['median_words']:>6}")
print(f"\nunparsed verdict lines per model: {[(r['model'].split('/')[-1], r['unparsed']) for r in rows]}")
