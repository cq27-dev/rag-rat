#!/usr/bin/env python3
"""Freeze bounded #957 intent context for the fixed #954 reviewed replay.

The exporter is read-only. It records the source-pack commit separately from the live index commit
that supplied context so an experiment cannot silently present a same-snapshot causal claim when
the context was selected later.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import sqlite3
import subprocess
import tempfile

HARNESS = pathlib.Path(__file__).resolve().parent
EVAL_DIR = HARNESS.parent
REPO_ROOT = EVAL_DIR.parents[1]
CORPUS = EVAL_DIR / "corpus"
CASES_PATH = CORPUS / "reviewed-verify-replay.json"
OUTPUT_PATH = CORPUS / "reviewed-verify-intent-context.json"
VERIFIED_MEMORY_LIMIT = 3
PAPERTRAIL_LIMIT = 3
TARGET_LEAKAGE_ITEMS = {"954", "957", "959"}


def note_content_hash(title: str, body: str) -> str:
    text = f"{title.strip()}\n{body.strip()}"
    return hashlib.sha256(text.encode()).hexdigest()


def repo_and_index_commit(conn: sqlite3.Connection, memory_id: str) -> tuple[str, str]:
    rows = conn.execute(
        "SELECT DISTINCT repo_id FROM repo_memories WHERE id = ?1", (memory_id,)
    ).fetchall()
    if len(rows) != 1:
        raise SystemExit(f"{memory_id}: expected one repo, found {len(rows)}")
    repo_id = rows[0][0]
    row = conn.execute(
        "SELECT value FROM repo_meta WHERE repo_id = ?1 AND key = 'git_commit'",
        (repo_id,),
    ).fetchone()
    if row is None:
        raise SystemExit(f"{memory_id}: repo {repo_id} has no indexed git_commit")
    return repo_id, row[0]


def current_bound_paths(
    conn: sqlite3.Connection, memory_id: str, repo_id: str
) -> list[str]:
    return [
        row[0]
        for row in conn.execute(
            """
            SELECT DISTINCT path
            FROM repo_memory_bindings
            WHERE repo_id = ?1 AND memory_id = ?2 AND path IS NOT NULL
              AND anchor_status IN ('current', 'relocated')
            ORDER BY path
            """,
            (repo_id, memory_id),
        ).fetchall()
    ]


def current_input_hashes(
    bin_path: pathlib.Path,
    db_path: pathlib.Path,
    repo_id: str,
    memory_ids: list[str],
) -> dict[str, str]:
    with tempfile.TemporaryDirectory(prefix="reviewed-intent-hashes-") as temp:
        temp_path = pathlib.Path(temp)
        ids_path = temp_path / "memory-ids.json"
        out_path = temp_path / "hashes.json"
        ids_path.write_text(json.dumps(memory_ids))
        subprocess.run(
            [
                str(bin_path),
                "dump-memory-input-hashes",
                "--db",
                str(db_path.resolve()),
                "--repo-id",
                repo_id,
                "--memory-ids",
                str(ids_path),
                "--out",
                str(out_path),
            ],
            cwd=REPO_ROOT,
            check=True,
        )
        return json.loads(out_path.read_text())


def candidate_memory_ids(
    conn: sqlite3.Connection,
    repo_id: str,
    index_commit: str,
    prompt_version: str,
) -> list[str]:
    return [
        row[0]
        for row in conn.execute(
            """
            SELECT memory_id
            FROM memory_reality
            WHERE repo_id = ?1 AND verdict = 'current' AND prompt_version = ?2
              AND checked_against_commit = ?3 AND checked_inputs_hash IS NOT NULL
            ORDER BY memory_id
            """,
            (repo_id, prompt_version, index_commit),
        ).fetchall()
    ]


def verified_memories(
    conn: sqlite3.Connection,
    case: dict,
    repo_id: str,
    index_commit: str,
    prompt_version: str,
    compact_prompt_version: str,
    bound_paths: list[str],
    input_hashes: dict[str, str],
) -> list[dict]:
    if not bound_paths:
        return []
    path_params = ", ".join("?" for _ in bound_paths)
    rows = conn.execute(
        f"""
        SELECT DISTINCT
            m.id, m.kind, m.title, m.body, mr.content_hash, mr.checked_inputs_hash,
            mr.prompt_version, mr.checked_against_commit, ms.summary, mr.checked_at_ms, rb.path
        FROM repo_memory_bindings AS rb
        JOIN repo_memories AS m
          ON m.repo_id = rb.repo_id AND m.id = rb.memory_id
        JOIN memory_reality AS mr
          ON mr.repo_id = m.repo_id AND mr.memory_id = m.id
        LEFT JOIN memory_summaries AS ms
          ON ms.repo_id = m.repo_id AND ms.memory_id = m.id
         AND ms.content_hash = mr.content_hash AND ms.prompt_version = ?
        WHERE rb.repo_id = ?
          AND rb.path IN ({path_params})
          AND rb.anchor_status IN ('current', 'relocated')
          AND m.id != ?
          AND m.status = 'active'
          AND mr.verdict = 'current'
          AND mr.prompt_version = ?
          AND mr.checked_against_commit = ?
          AND mr.checked_inputs_hash IS NOT NULL
          AND NOT EXISTS (
              SELECT 1 FROM repo_memory_bindings AS changed
              WHERE changed.repo_id = m.repo_id AND changed.memory_id = m.id
                AND changed.created_at_ms > mr.checked_at_ms
          )
        ORDER BY m.id, rb.path
        """,
        # `?` params are POSITIONAL: the LEFT JOIN placeholder is the first in the SQL text.
        (compact_prompt_version, repo_id, *bound_paths, case["memory_id"], prompt_version, index_commit),
    ).fetchall()
    selected = []
    seen = set()
    for row in rows:
        if row[4] != note_content_hash(row[2], row[3]):
            continue
        if input_hashes.get(row[0]) != row[5]:
            continue
        if row[0] in seen:
            continue
        seen.add(row[0])
        selected.append(
            {
                "memory_id": row[0],
                "kind": row[1],
                "title": row[2],
                "summary": row[8],
                "shared_binding": row[10],
                "freshness": {
                    "verdict": "current",
                    "content_hash": row[4],
                    "checked_inputs_hash": row[5],
                    "prompt_version": row[6],
                    "checked_against_commit": row[7],
                    "checked_at_ms": row[9],
                },
            }
        )
        if len(selected) == VERIFIED_MEMORY_LIMIT:
            break
    return selected


def rejected_alternatives(conn: sqlite3.Connection, repo_id: str, row: sqlite3.Row) -> list[dict]:
    alternatives = conn.execute(
        """
        SELECT alternative, reason
        FROM papertrail_distill_alternatives
        WHERE repo_id = ?1 AND tracker = ?2 AND project = ?3
          AND item_kind = ?4 AND item_key = ?5
        ORDER BY ordinal
        """,
        (repo_id, row["tracker"], row["project"], row["item_kind"], row["item_key"]),
    ).fetchall()
    return [
        {"alternative": alternative, "reason": reason}
        for alternative, reason in alternatives
    ]


def papertrail_records(
    conn: sqlite3.Connection, repo_id: str, bound_paths: list[str]
) -> list[dict]:
    if not bound_paths:
        return []
    path_params = ", ".join("?" for _ in bound_paths)
    rows = conn.execute(
        f"""
        SELECT DISTINCT
            d.tracker, d.project, d.item_kind, d.item_key, d.root_issue, d.root_cause,
            d.decision_chosen, d.outcome_summary, d.distilled_at_ms, a.selected, a.file_path,
            pi.title AS thread_title
        FROM papertrail_distill_anchors AS a
        JOIN papertrail_distill AS d
          ON d.repo_id = a.repo_id AND d.tracker = a.tracker AND d.project = a.project
         AND d.item_kind = a.item_kind AND d.item_key = a.item_key
        JOIN papertrail_items AS pi
          ON pi.repo_id = d.repo_id AND pi.tracker = d.tracker AND pi.project = d.project
         AND pi.item_kind = d.item_kind AND pi.item_key = d.item_key
        WHERE a.repo_id = ? AND a.file_path IN ({path_params}) AND a.resolved = 1
        ORDER BY a.selected DESC, d.distilled_at_ms DESC,
                 d.tracker, d.project, d.item_kind, d.item_key
        """,
        (repo_id, *bound_paths),
    ).fetchall()
    selected = []
    seen = set()
    for row in rows:
        key = (row["tracker"], row["project"], row["item_kind"], row["item_key"])
        if key in seen or row["item_key"] in TARGET_LEAKAGE_ITEMS:
            continue
        alternatives = rejected_alternatives(conn, repo_id, row)
        if not any(
            (
                row["root_issue"],
                row["root_cause"],
                row["decision_chosen"],
                row["outcome_summary"],
                row["thread_title"],
                alternatives,
            )
        ):
            continue
        seen.add(key)
        selected.append(
            {
                "tracker": row["tracker"],
                "project": row["project"],
                "item_kind": row["item_kind"],
                "item_key": row["item_key"],
                "thread_title": row["thread_title"],
                "root_issue": row["root_issue"],
                "root_cause": row["root_cause"],
                "decision_chosen": row["decision_chosen"],
                "outcome_summary": row["outcome_summary"],
                "rejected_alternatives": alternatives,
                "shared_anchor": row["file_path"],
                "selected_anchor": bool(row["selected"]),
                "distilled_at_ms": row["distilled_at_ms"],
                "unreviewed": True,
            }
        )
        if len(selected) == PAPERTRAIL_LIMIT:
            break
    return selected


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=pathlib.Path, required=True)
    # Must track VERDICT_PROMPT_VERSION in crates/rag-rat-query/src/memory/evidence.rs: this one
    # sits in a WHERE clause over an INNER join, so a stale value drops every verified memory from
    # the export rather than nulling a column.
    parser.add_argument("--verdict-prompt-version", default="verify-pack-v6")
    # Must track COMPACT_PROMPT_VERSION in crates/rag-rat-query/src/memory/evidence.rs: the summary
    # join is a LEFT JOIN, so a stale value yields NULL summaries silently instead of erroring.
    parser.add_argument("--compact-prompt-version", default="compact-v2")
    parser.add_argument(
        "--bin",
        type=pathlib.Path,
        default=REPO_ROOT / "target" / "debug" / "rag-rat",
        help="rag-rat binary built with --features eval",
    )
    args = parser.parse_args()

    cases = json.loads(CASES_PATH.read_text())
    uri = f"file:{args.db.resolve()}?mode=ro"
    with sqlite3.connect(uri, uri=True) as conn:
        conn.row_factory = sqlite3.Row
        repo_commits = {repo_and_index_commit(conn, case["memory_id"]) for case in cases}
        if len(repo_commits) != 1:
            raise SystemExit(f"reviewed cases crossed context snapshots: {sorted(repo_commits)}")
        repo_id, index_commit = next(iter(repo_commits))
        candidate_ids = candidate_memory_ids(
            conn, repo_id, index_commit, args.verdict_prompt_version
        )
        input_hashes = current_input_hashes(args.bin, args.db, repo_id, candidate_ids)
        contexts = {}
        for case in cases:
            bound_paths = current_bound_paths(conn, case["memory_id"], repo_id)
            contexts[case["id"]] = {
                "current_bound_paths": bound_paths,
                "verified_memories": verified_memories(
                    conn,
                    case,
                    repo_id,
                    index_commit,
                    args.verdict_prompt_version,
                    args.compact_prompt_version,
                    bound_paths,
                    input_hashes,
                ),
                "papertrail": papertrail_records(conn, repo_id, bound_paths),
            }
    fixture = {
        "selection_version": "intent-context-v1",
        "source_pack_commit": cases[0]["source_commit"],
        "context_index_commit": index_commit,
        "context_repo_id": repo_id,
        "verdict_prompt_version": args.verdict_prompt_version,
        "compact_prompt_version": args.compact_prompt_version,
        "limits": {
            "verified_memories": VERIFIED_MEMORY_LIMIT,
            "papertrail": PAPERTRAIL_LIMIT,
            "rendered_context_bytes": 6000,
        },
        "cases": contexts,
    }
    OUTPUT_PATH.write_text(json.dumps(fixture, indent=1, sort_keys=True) + "\n")
    memory_cases = sum(bool(context["verified_memories"]) for context in contexts.values())
    papertrail_cases = sum(bool(context["papertrail"]) for context in contexts.values())
    print(
        f"wrote {len(contexts)} cases to {OUTPUT_PATH.relative_to(EVAL_DIR.parents[1])}: "
        f"verified-memory context for {memory_cases}, papertrail context for {papertrail_cases}"
    )


if __name__ == "__main__":
    main()
