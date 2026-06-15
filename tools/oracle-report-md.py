#!/usr/bin/env python3
"""Render C2 oracle resolution reports (JSON) into a Markdown table for a CI job summary / PR comment.

rag-rat emits the typed `OracleResolutionReport` as JSON only; turning it into Markdown is a glue
concern (see the output-rendering decision) — this is that glue for the per-PR small tier. Each
report becomes a row: the per-run before→after resolution (heuristic vs compiler) plus precision /
recall / monikers. When a comparable BASELINE report (main's last run for the same corpus) is given,
a Δ-vs-baseline column shows the regression signal.

Comparability mirrors `OracleResolutionReport::comparable_to`: a baseline is only diffed when its
`report_schema_version`, `corpus_profile_hash`, AND `tool_version` all match — otherwise the numbers
describe different corpora/indexers, so the Δ is omitted (shown as `— (baseline incomparable)`)
rather than silently subtracting apples from oranges. A missing baseline degrades to absolute-only.

Usage:
  oracle-report-md.py --reports a.json b.json [--baselines x.json y.json] > comment.md

Reports/baselines are matched by `corpus_id`. Prints a stable marker comment so a PR sticky-comment
updater can find and replace its own previous comment.
"""

from __future__ import annotations

import argparse
import json
import sys

# A stable HTML-comment marker so the PR sticky-comment step can find + update its prior comment.
MARKER = "<!-- rag-rat-oracle-report -->"


def load(paths: list[str]) -> dict[str, dict]:
    """Load reports keyed by corpus_id (last wins on a dup id)."""
    out: dict[str, dict] = {}
    for path in paths or []:
        with open(path) as fh:
            report = json.load(fh)
        out[report["corpus_id"]] = report
    return out


def rate(numerator: int, denominator: int) -> float:
    # Mirror the engine's vacuous-100% convention for an empty denominator.
    return 100.0 if denominator == 0 else 100.0 * numerator / denominator


def comparable(report: dict, baseline: dict | None) -> bool:
    """The `comparable_to` rule: same schema version, profile hash, AND tool version."""
    return baseline is not None and all(
        report.get(key) == baseline.get(key)
        for key in ("report_schema_version", "corpus_profile_hash", "tool_version")
    )


def resolved_after_pct(report: dict) -> float:
    res = report["resolution"]
    return rate(res["resolved_after"], res["total_edges"])


def row(report: dict, baseline: dict | None) -> str:
    res = report["resolution"]
    before = rate(res["resolved_before"], res["total_edges"])
    after = resolved_after_pct(report)
    metrics = report.get("metrics", {})
    precision = 100.0 * metrics.get("precision", 0.0)
    recall = 100.0 * metrics.get("recall", 0.0)

    if comparable(report, baseline):
        delta = after - resolved_after_pct(baseline)
        # Signed pp delta vs main; a regression (negative) reads at a glance.
        delta_cell = f"{delta:+.1f}pp"
    elif baseline is None:
        delta_cell = "— (no baseline)"
    else:
        delta_cell = "— (baseline incomparable)"

    return (
        f"| `{report['corpus_id']}` | {report['tool']} | {res['total_edges']} "
        f"| {before:.1f}% → {after:.1f}% | {precision:.1f}% | {recall:.1f}% "
        f"| {report.get('symbols_with_moniker', 0)} | {delta_cell} |"
    )


def render(reports: dict[str, dict], baselines: dict[str, dict]) -> str:
    # Explicit `+` joins on the multi-line literals (not adjacent-literal implicit concatenation,
    # which CodeQL flags in a list as a likely missing comma).
    lines = [
        MARKER,
        "## SCIP oracle — resolution report",
        "",
        (
            "Heuristic→compiler edge resolution per corpus. **Δ** compares *resolved-after* to the "
            + "`main` baseline (only when the corpus profile + tool version match)."
        ),
        "",
        (
            "| corpus | tool | edges | resolved (heuristic → compiler) | precision | recall "
            + "| monikers | Δ vs main |"
        ),
        "|---|---|--:|---|--:|--:|--:|--:|",
    ]
    for corpus_id in sorted(reports):
        lines.append(row(reports[corpus_id], baselines.get(corpus_id)))
    lines.append("")
    lines.append(
        "<sub>resolved = `Exact`/`Syntactic` + compiler upgrades + resolved-external, over edge "
        + "candidates with a callee range. precision/recall are the oracle eval metrics.</sub>"
    )
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reports", nargs="+", required=True)
    parser.add_argument("--baselines", nargs="*", default=[])
    args = parser.parse_args()

    reports = load(args.reports)
    if not reports:
        sys.exit("oracle-report-md: no reports given")
    baselines = load(args.baselines)
    sys.stdout.write(render(reports, baselines))


if __name__ == "__main__":
    main()
