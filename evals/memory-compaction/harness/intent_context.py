"""Pure rendering helpers for the #957 reviewed-verifier intent-context experiment."""

from __future__ import annotations

import json
import pathlib

ARMS = ("source-only", "verified-memory", "papertrail", "combined")
MAX_CONTEXT_BYTES = 6000

CONTEXT_PREAMBLE = """INTENT CONTEXT (NON-CITABLE):
These rows may help interpret whether names in the NOTE are historical, transitional, rejected, or load-bearing. They are not proof of current checkout state. Never cite an INTENT row in EVIDENCE; only complete lines from the EVIDENCE PACK are admissible citations."""


def _one_line(value: object | None) -> str:
    return " ".join(str(value or "").split())


def _memory_row(row: dict) -> str:
    lines = [
        f"INTENT MEMORY {row['memory_id']}",
        f"KIND: {_one_line(row['kind'])}",
        f"TITLE: {_one_line(row['title'])}",
    ]
    if row.get("summary"):
        lines.append(f"SUMMARY: {_one_line(row['summary'])}")
    return "\n".join(lines)


def _papertrail_row(row: dict) -> str:
    lines = [
        "INTENT PAPERTRAIL "
        f"{row['tracker']}:{row['project']}#{row['item_key']} ({row['item_kind']})"
    ]
    for label, key in (
        ("THREAD TITLE", "thread_title"),
        ("ROOT ISSUE", "root_issue"),
        ("ROOT CAUSE", "root_cause"),
        ("DECISION", "decision_chosen"),
        ("OUTCOME", "outcome_summary"),
    ):
        if row.get(key):
            lines.append(f"{label}: {_one_line(row[key])}")
    for alternative in row.get("rejected_alternatives", []):
        text = _one_line(alternative["alternative"])
        reason = _one_line(alternative.get("reason"))
        lines.append(f"REJECTED: {text}" + (f" BECAUSE: {reason}" if reason else ""))
    return "\n".join(lines)


def render_intent_context(context: dict, arm: str) -> str:
    if arm not in ARMS:
        raise ValueError(f"unknown intent-context arm: {arm}")
    if arm == "source-only":
        return ""
    rows = []
    if arm in {"verified-memory", "combined"}:
        rows.extend(_memory_row(row) for row in context["verified_memories"])
    if arm in {"papertrail", "combined"}:
        rows.extend(_papertrail_row(row) for row in context["papertrail"])
    if not rows:
        return ""

    rendered = CONTEXT_PREAMBLE
    for row in rows:
        candidate = f"{rendered}\n\n{row}"
        if len(candidate.encode()) > MAX_CONTEXT_BYTES:
            break
        rendered = candidate
    return rendered


def render_reviewed_prompt(
    prompt_template: str,
    case: dict,
    pack: str,
    context: dict,
    arm: str,
) -> str:
    prompt = prompt_template.format(
        binding=case["binding"]["value"],
        title=case["title"],
        body=case["body"],
        pack=pack,
    )
    rendered_context = render_intent_context(context, arm)
    if not rendered_context:
        return prompt
    marker = "\nEVIDENCE PACK:\n"
    if marker not in prompt:
        raise ValueError("reviewed verifier prompt has no EVIDENCE PACK marker")
    return prompt.replace(marker, f"\n{rendered_context}\n{marker}", 1)


def load_context_fixture(path: pathlib.Path, case_ids: set[str]) -> dict:
    fixture = json.loads(path.read_text())
    contexts = fixture["cases"]
    if set(contexts) != case_ids:
        missing = sorted(case_ids - set(contexts))
        extra = sorted(set(contexts) - case_ids)
        raise ValueError(f"intent-context fixture mismatch: missing={missing} extra={extra}")
    return fixture
