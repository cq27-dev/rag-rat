#!/usr/bin/env python3
"""Reproduce the doctored "drift" copy of `crates/` the verify manifest depends on.

The verification benchmark (VERIFY_MANIFEST in eval_app.py) includes two `code_ahead`
divergence cases mounted from a second repo root (`/repo-drift`): the memory notes still
describe two guards that a hypothetical future commit has removed. Rather than vendor a full
stale copy of the tree, this script regenerates it from the CURRENT checkout by applying two
surgical edits, so the drift stays anchored to real code as the repo evolves.

The two edits (both in the write-time clone-check path):
  (a) precompute.rs — remove the linked-overlay gate
        `if self.active_scope_is_linked_overlay() { return Ok(None); }`
      and its preceding comment. Backs `real_22` at /repo-drift (memory says the fast path is
      disabled under a linked overlay; the doctored code no longer disables it).
  (b) scoring.rs — remove the refined-class self-guard
        `if class.refined { return; }`
      and its preceding comment. Backs `real_27` at /repo-drift (memory says a refined class is
      never re-dampened; the doctored code drops that guard).

If either anchor is missing (the code has moved), the script FAILS LOUDLY: the drift can no
longer be reproduced faithfully and the two manifest cases must be re-derived by hand.

Usage:
    python3 make-drift-tree.py            # regenerate ./drift-crates from ../../../crates
    python3 make-drift-tree.py --clean    # remove ./drift-crates
"""

import pathlib
import shutil
import sys

HARNESS_DIR = pathlib.Path(__file__).resolve().parent
EVAL_DIR = HARNESS_DIR.parent  # evals/memory-compaction
REPO_ROOT = HARNESS_DIR.parents[2]  # repository root
SRC_CRATES = REPO_ROOT / "crates"
DRIFT_CRATES = EVAL_DIR / "drift-crates"

# Each edit: (relative path under crates/, the guard's `if` line, human label). The remover
# deletes the `if … { … }` block plus the contiguous `//` comment lines directly above it.
EDITS = [
    (
        "rag-rat-core/src/index/query_api/clones/precompute.rs",
        "if self.active_scope_is_linked_overlay() {",
        "linked-overlay clone-check gate",
    ),
    (
        "rag-rat-core/src/index/query_api/clones/scoring.rs",
        "if class.refined {",
        "refined-class self-guard",
    ),
]


def die(msg: str) -> None:
    sys.exit(f"make-drift-tree: ERROR: {msg}")


def remove_guard_block(text: str, if_line_needle: str, label: str) -> str:
    """Remove the single `if … {` block matching `if_line_needle`, together with the run of
    comment lines immediately preceding it. Fails loudly on any anchor mismatch."""
    lines = text.split("\n")
    matches = [i for i, ln in enumerate(lines) if ln.strip() == if_line_needle]
    if len(matches) != 1:
        die(
            f"{label}: expected exactly one line `{if_line_needle}`, found {len(matches)}. "
            "The code has drifted — re-derive the drift edit."
        )
    i = matches[0]
    indent = " " * (len(lines[i]) - len(lines[i].lstrip()))

    # Matching close brace: the first following line that is exactly this indent + '}'.
    close = None
    for j in range(i + 1, len(lines)):
        if lines[j] == indent + "}":
            close = j
            break
    if close is None:
        die(f"{label}: could not find the closing brace for `{if_line_needle}`.")

    # Walk up over the contiguous comment lines that introduce the guard.
    start = i
    while start - 1 >= 0 and lines[start - 1].lstrip().startswith("//"):
        start -= 1
    if start == i:
        die(f"{label}: expected a preceding `//` comment above `{if_line_needle}`.")

    del lines[start : close + 1]
    return "\n".join(lines)


def clean() -> None:
    if DRIFT_CRATES.exists():
        shutil.rmtree(DRIFT_CRATES)
        print(f"removed {DRIFT_CRATES}")
    else:
        print(f"nothing to clean ({DRIFT_CRATES} absent)")


def build() -> None:
    if not SRC_CRATES.is_dir():
        die(f"source crates dir not found: {SRC_CRATES}")
    if DRIFT_CRATES.exists():
        shutil.rmtree(DRIFT_CRATES)
    # A .rs-only copy keeps the drift tree small; the harness tools only read *.rs.
    shutil.copytree(
        SRC_CRATES,
        DRIFT_CRATES,
        ignore=shutil.ignore_patterns("target", "*.lock", "*.json", "*.md", "*.toml"),
    )

    for rel, needle, label in EDITS:
        path = DRIFT_CRATES / rel
        if not path.is_file():
            die(f"{label}: target file not found in the copy: {rel}")
        before = path.read_text(encoding="utf-8")
        after = remove_guard_block(before, needle, label)
        if after == before:
            die(f"{label}: no change applied to {rel} (anchor mismatch).")
        path.write_text(after, encoding="utf-8")
        removed = before.count("\n") - after.count("\n")
        print(f"applied: {label} ({rel}, -{removed} lines)")

    print(f"drift tree ready at {DRIFT_CRATES}")


def main() -> None:
    args = sys.argv[1:]
    if args == ["--clean"]:
        clean()
    elif not args:
        build()
    else:
        die(f"unknown arguments: {args} (use no args to build, --clean to remove)")


if __name__ == "__main__":
    main()
