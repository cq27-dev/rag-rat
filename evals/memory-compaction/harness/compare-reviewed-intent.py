#!/usr/bin/env python3
"""Compare the four #957 intent-context arms through the production acceptance guard."""

from __future__ import annotations

import collections
import importlib.util
import json
import pathlib

HARNESS = pathlib.Path(__file__).resolve().parent
EVAL_DIR = HARNESS.parent
CORPUS = EVAL_DIR / "corpus"
RESULTS = EVAL_DIR / "results"
ARMS = ("source-only", "verified-memory", "papertrail", "combined")

_score_spec = importlib.util.spec_from_file_location(
    "score_reviewed_verify", HARNESS / "score-reviewed-verify.py"
)
score = importlib.util.module_from_spec(_score_spec)
_score_spec.loader.exec_module(score)


def score_arm(arm: str, cases: dict, packs: dict) -> dict:
    path = RESULTS / f"reviewed-verify-intent-{arm}-results.json"
    rows = json.loads(path.read_text())
    items = [row["item"] for row in rows]
    if sorted(items) != sorted(cases) or len(items) != len(set(items)):
        raise SystemExit(f"{arm}: results are not one complete 36-case replay")
    outcomes = {}
    counts = collections.Counter()
    for row in rows:
        case = cases[row["item"]]
        actual = score.production_accepts(
            case,
            packs[f"{row['item']}|/repo"],
            row["answer"],
        )
        outcomes[row["item"]] = actual
        counts[(case["expected_verdict"], actual)] += 1
    false_positive_items = sorted(
        item
        for item, actual in outcomes.items()
        if cases[item]["expected_verdict"] == "current" and actual == "diverged"
    )
    true_positive_items = sorted(
        item
        for item, actual in outcomes.items()
        if cases[item]["expected_verdict"] == "diverged" and actual == "diverged"
    )
    return {
        "false_positives": counts[("current", "diverged")],
        "true_positives": counts[("diverged", "diverged")],
        "discarded": sum(count for (_, actual), count in counts.items() if actual == "discarded"),
        "false_positive_items": false_positive_items,
        "true_positive_items": true_positive_items,
        "outcomes": outcomes,
    }


def main() -> None:
    cases = {
        case["id"]: case
        for case in json.loads((CORPUS / "reviewed-verify-replay.json").read_text())
    }
    packs = json.loads((CORPUS / "reviewed-verify-packs.json").read_text())
    arms = {arm: score_arm(arm, cases, packs) for arm in ARMS}
    control = arms["source-only"]["outcomes"]
    for arm in ARMS:
        result = arms[arm]
        changed = sorted(
            item for item, outcome in result["outcomes"].items() if outcome != control[item]
        )
        result["changed_from_source_only"] = changed
        print(
            f"{arm:16} fp={result['false_positives']}/33 "
            f"tp={result['true_positives']}/3 discarded={result['discarded']}/36 "
            f"fp_items={','.join(result['false_positive_items']) or '-'} "
            f"tp_items={','.join(result['true_positive_items']) or '-'} "
            f"changed={','.join(changed) or '-'}"
        )
    (RESULTS / "reviewed-verify-intent-scoreboard.json").write_text(
        json.dumps(arms, indent=1, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
