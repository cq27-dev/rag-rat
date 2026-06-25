#!/usr/bin/env python3
"""Convert a commit-replay eval report (JSON) into Bencher Metric Format (BMF).

`rag-rat --json eval --replay` emits the typed `EvalReport` as JSON; turning it into the shape
Bencher ingests is a glue concern (mirrors tools/oracle-report-bmf.py). This carries the
search-quality headline for the main-branch `bench` workflow: one BMF benchmark `search_replay`
with recall@3 / recall@10 / mrr@10 over the self-index at HEAD, so Bencher tracks how retrieval
quality moves over time and alerts on a real recall regression.

recall@3 is the headline (#120, #109): "did the right chunk land in the first 3 reads". The values
are fractions in [0, 1] — the SAME numbers `eval` prints — so a Bencher plot reads identically to a
local run. Higher is better, so the gate is a statistical LOWER boundary, and it is tracking-only
(no `--err`): a noisy quality metric is alerted on, not hard-failed (see .github/workflows/bench.yml,
and the same stance on the iai/criterion series).

Usage:
  rag-rat --json eval --replay --replay-max-cases 200 > eval.json
  eval-report-bmf.py eval.json > eval_bmf.json
"""

from __future__ import annotations

import json
import sys

# Stable series name over main. There is ONE self-corpus (the rag-rat repo at HEAD), unlike oracle's
# many external corpora, so a benchmark-name comparability hash isn't needed here — an embedder or
# replay-parameter change that makes runs incomparable is handled by re-resetting the baseline
# (bench.yml `--thresholds-reset`, plus the manual workflow_dispatch), not by forking the series.
BENCHMARK = "search_replay"

# The three tracked measures (#19): recall@3 (headline, carries the threshold in bench.yml),
# recall@10, and mrr@10. All are means over the replayed cases, already computed by the harness.
MEASURES = ("recall_at_3", "recall_at_10", "mrr_at_10")


def to_bmf(report: dict) -> dict:
    """One EvalReport JSON -> a single-benchmark BMF object carrying the three recall measures."""
    metrics = report.get("metrics")
    if not isinstance(metrics, dict):
        sys.exit("eval-report-bmf: report has no `metrics` object (not an eval report?)")
    body: dict[str, dict] = {}
    for measure in MEASURES:
        if measure not in metrics:
            sys.exit(f"eval-report-bmf: report.metrics missing `{measure}` (not a --replay report?)")
        body[measure] = {"value": float(metrics[measure])}
    return {BENCHMARK: body}


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: eval-report-bmf.py <eval-report.json>")
    with open(sys.argv[1]) as fh:
        report = json.load(fh)
    json.dump(to_bmf(report), sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
