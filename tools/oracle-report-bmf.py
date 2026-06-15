#!/usr/bin/env python3
"""Convert a C2 oracle resolution report (JSON) into Bencher Metric Format (BMF).

rag-rat emits the typed `OracleResolutionReport` as JSON only; turning it into the shape a consumer
wants is a glue concern (see the output-rendering decision). This is the Bencher-headline glue for
the heavy tier: one BMF benchmark `<corpus_id>/oracle` carrying the resolution rates + oracle
verdict metrics, so the release run tracks how compiler-grade resolution moves over time.

(The PR Δ-table glue is a separate script, tools/oracle-report-md.py, C5.)

Usage:
  oracle-report-bmf.py <report.json> [<report2.json> ...] > bmf.json

Each report becomes one `<corpus_id>/oracle` benchmark; multiple reports merge into one BMF object,
so a single Bencher upload can carry every heavy corpus from one run.
"""

from __future__ import annotations

import json
import sys


def benchmark(report: dict) -> tuple[str, dict]:
    resolution = report["resolution"]
    total = resolution["total_edges"]
    metrics = report.get("metrics", {})

    def rate(numerator: int) -> float:
        # Mirror the engine's vacuous-1.0 convention for an empty denominator.
        return 100.0 if total == 0 else 100.0 * numerator / total

    name = f"{report['corpus_id']}/oracle"
    body = {
        "total_edges": {"value": resolution["total_edges"]},
        "resolved_rate_before": {"value": rate(resolution["resolved_before"])},
        "resolved_rate_after": {"value": rate(resolution["resolved_after"])},
        "precision": {"value": 100.0 * metrics.get("precision", 0.0)},
        "recall": {"value": 100.0 * metrics.get("recall", 0.0)},
        "name_only_recovery": {"value": 100.0 * metrics.get("name_only_recovery_rate", 0.0)},
        "confirmed": {"value": report.get("confirmed", 0)},
        "contradicted": {"value": report.get("contradicted", 0)},
        "upgraded": {"value": report.get("upgraded", 0)},
        "resolved_external": {"value": report.get("resolved_external", 0)},
        "symbols_with_moniker": {"value": report.get("symbols_with_moniker", 0)},
    }
    return name, body


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit("usage: oracle-report-bmf.py <report.json> [<report2.json> ...]")
    bmf: dict[str, dict] = {}
    for path in sys.argv[1:]:
        with open(path) as fh:
            report = json.load(fh)
        name, body = benchmark(report)
        bmf[name] = body
    json.dump(bmf, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
