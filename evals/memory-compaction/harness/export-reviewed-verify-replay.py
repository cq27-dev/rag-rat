#!/usr/bin/env python3
"""Freeze the manually reviewed dream-verify batch behind issue #954.

The source ledger is a live rag-rat index, but the outputs are committed fixtures: the note,
review verdict, and evidence pack as reviewed against commit c5303848. Future verifier changes
replay these exact prompt inputs instead of treating newer model verdicts as ground truth.

Run from `evals/memory-compaction/`:

    cargo build -p rag-rat --features eval
    python3 harness/export-reviewed-verify-replay.py \
        --db ~/.local/share/rag-rat/rag-rat.sqlite

This intentionally fails if the selected review window is not the known 36-case batch.
"""

import argparse
import importlib.util
import json
import pathlib
import shutil
import sqlite3
import subprocess
import tempfile

HARNESS = pathlib.Path(__file__).resolve().parent
EVAL_DIR = HARNESS.parent
REPO_ROOT = HARNESS.parents[2]
CORPUS = EVAL_DIR / "corpus"
SOURCE_COMMIT = "c5303848"
FIRST_REVIEWED_AT_MS = 1784970235600
LAST_REVIEWED_AT_MS = 1784970238179
EXPECTED_CASES = 36

_regen_spec = importlib.util.spec_from_file_location(
    "regen_verify_packs", HARNESS / "regen-verify-packs.py"
)
regen = importlib.util.module_from_spec(_regen_spec)
_regen_spec.loader.exec_module(regen)


def selected_rows(conn: sqlite3.Connection) -> list[sqlite3.Row]:
    conn.row_factory = sqlite3.Row
    return conn.execute(
        """
        SELECT
            f.id AS finding_id,
            f.repo_id,
            f.subject AS memory_id,
            f.status AS reviewed_status,
            f.evidence AS original_evidence,
            f.reviewed_at_ms,
            m.kind,
            m.title,
            m.body,
            m.confidence,
            mr.direction,
            rb.binding_kind,
            CASE rb.binding_kind
                WHEN 'dir' THEN rb.binding_id
                ELSE rb.path
            END AS binding_value
        FROM dream_findings AS f
        JOIN repo_memories AS m
          ON m.repo_id = f.repo_id AND m.id = f.subject
        LEFT JOIN memory_reality AS mr
          ON mr.repo_id = f.repo_id AND mr.memory_id = f.subject
        JOIN repo_memory_bindings AS rb
          ON rb.repo_id = f.repo_id AND rb.memory_id = f.subject
        WHERE f.kind = 'memory_divergence'
          AND f.status IN ('accepted', 'dismissed')
          AND f.reviewed_at_ms BETWEEN ?1 AND ?2
        ORDER BY f.reviewed_at_ms, f.id
        """,
        (FIRST_REVIEWED_AT_MS, LAST_REVIEWED_AT_MS),
    ).fetchall()


def replay_cases(rows: list[sqlite3.Row]) -> list[dict]:
    if len(rows) != EXPECTED_CASES:
        raise SystemExit(
            f"expected {EXPECTED_CASES} reviewed findings in the fixed window, found {len(rows)}"
        )
    cases = []
    for index, row in enumerate(rows):
        accepted = row["reviewed_status"] == "accepted"
        cases.append(
            {
                "id": f"reviewed_{index:02d}",
                "source_commit": SOURCE_COMMIT,
                "finding_id": row["finding_id"],
                "memory_id": row["memory_id"],
                "reviewed_status": row["reviewed_status"],
                "expected_verdict": "diverged" if accepted else "current",
                "expected_direction": row["direction"] if accepted else None,
                "original_evidence": row["original_evidence"],
                "reviewed_at_ms": row["reviewed_at_ms"],
                "kind": row["kind"],
                "title": row["title"],
                "body": row["body"],
                "confidence": row["confidence"],
                "binding": {
                    "kind": row["binding_kind"],
                    "value": row["binding_value"],
                },
            }
        )
    accepted = sum(case["expected_verdict"] == "diverged" for case in cases)
    if accepted != 3:
        raise SystemExit(f"expected 3 accepted divergences, found {accepted}")
    return cases


def dump_packs(bin_path: str, cases: list[dict]) -> dict:
    work = pathlib.Path(tempfile.mkdtemp(prefix="reviewed-verify-replay-"))
    try:
        root = regen.build_repo_root(work, False, [])
        config = regen.write_config(root, work / "repo.sqlite")
        regen.run(bin_path, config, "index")
        spec = {
            "memories": [
                {
                    "eval_id": case["id"],
                    "kind": case["kind"],
                    "title": case["title"],
                    "body": case["body"],
                    "confidence": case["confidence"],
                    "binding": case["binding"],
                }
                for case in cases
            ],
            "dump": [case["id"] for case in cases],
        }
        spec_path = work / "spec.json"
        spec_path.write_text(json.dumps(spec))
        out_path = work / "packs.json"
        regen.run(
            bin_path,
            config,
            "dump-verify-packs",
            "--spec",
            str(spec_path),
            "--root-label",
            "/repo",
            "--out",
            str(out_path),
        )
        return json.loads(out_path.read_text())
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=pathlib.Path, required=True)
    parser.add_argument(
        "--bin",
        default=str(REPO_ROOT / "target" / "debug" / "rag-rat"),
        help="rag-rat binary built with --features eval",
    )
    args = parser.parse_args()

    head = subprocess.run(
        ["git", "rev-parse", "--short=8", "HEAD"],
        cwd=REPO_ROOT,
        capture_output=True,
        check=True,
        text=True,
    ).stdout.strip()
    if head != SOURCE_COMMIT:
        raise SystemExit(
            f"reviewed replay must be exported at {SOURCE_COMMIT}, current HEAD is {head}"
        )

    uri = f"file:{args.db.resolve()}?mode=ro"
    with sqlite3.connect(uri, uri=True) as conn:
        cases = replay_cases(selected_rows(conn))
    packs = dump_packs(args.bin, cases)
    if len(packs) != EXPECTED_CASES:
        raise SystemExit(
            f"expected {EXPECTED_CASES} rendered packs, found {len(packs)}"
        )

    cases_path = CORPUS / "reviewed-verify-replay.json"
    packs_path = CORPUS / "reviewed-verify-packs.json"
    cases_path.write_text(json.dumps(cases, indent=1) + "\n")
    packs_path.write_text(json.dumps(packs, indent=1, sort_keys=True) + "\n")
    print(f"wrote {len(cases)} reviewed cases to {cases_path.relative_to(REPO_ROOT)}")
    print(f"wrote {len(packs)} evidence packs to {packs_path.relative_to(REPO_ROOT)}")


if __name__ == "__main__":
    main()
