#!/usr/bin/env python3
"""Regenerate `corpus/verify-packs.json` against the CURRENT tree + the doctored drift tree (#695).

The verify-pack corpus is the deterministic evidence each `verify_pack_test` case shows the model. It
must match the SHIPPED `render_pack` output exactly, or the eval measures an obsolete evidence shape.
Rather than hand-maintain it, this regenerates it by re-running the real pack builder:

  1. Build a throwaway rag-rat index over a clean checkout (`/repo`) and over a doctored drift tree
     (`/repo-drift`) — applying only the make-drift-tree edits this run's drift cases are anchored to.
     No embeddings — `rag-rat index` builds the symbols/FTS/chunk-text that `evidence_pack` needs;
     `reconcile` (skipped) is what embeds.
  2. Insert the eval memories (from `memories-full.json`) into each index, honoring each binding's
     KIND (path / dir / logical_symbol), and dump `render_pack(evidence_pack(...))` for the manifest
     cases via `rag-rat dump-verify-packs` (the `eval`-gated committed entrypoint).
  3. Merge the two outputs into `corpus/verify-packs.json`.

Run from `evals/memory-compaction/`:  python3 harness/regen-verify-packs.py
Requires a `rag-rat` binary built with `--features eval` (pass --bin to point at it).
"""

import argparse
import importlib.util
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile

HARNESS = pathlib.Path(__file__).resolve().parent
EVAL_DIR = HARNESS.parent  # evals/memory-compaction
REPO_ROOT = HARNESS.parents[2]  # repository root
CORPUS = EVAL_DIR / "corpus"

# Reuse make-drift-tree.py's guard-remover (its filename has a hyphen, so load it by path) so regen
# applies EXACTLY the drift edits its pack cases need — NOT the full set. This decouples corpus
# regeneration from edits for dropped cases: the precompute gate real_27 used can move or vanish
# without blocking a regen that only needs real_22's scoring edit.
_mdt_spec = importlib.util.spec_from_file_location("make_drift_tree", HARNESS / "make-drift-tree.py")
mdt = importlib.util.module_from_spec(_mdt_spec)
_mdt_spec.loader.exec_module(mdt)

# The manifest cases, by root. Kept in lock-step with VERIFY_MANIFEST in eval_app.py. `real_N` maps to
# `memories-full.json[N]`. (syn_* packs are dropped — they were never in VERIFY_MANIFEST.)
#
# DRIFT (#695): only `real_22` survives as a `code_ahead` case. The drift edits remove a GUARD BLOCK
# (behavioral, symbol-preserving), so the divergence is visible ONLY when the removed guard lands in
# the bound-file excerpt. `real_22`'s scoring.rs guard (`if class.refined`) does; `real_27`'s
# precompute.rs gate sits at line ~303 of a large file, past the 140-line excerpt cap, and the method
# it calls still exists elsewhere — so no pack (production's included) can reflect it. Testing it would
# test a divergence the shipped evidence_pack cannot represent, so `real_27|/repo-drift` is dropped.
REPO_CASES = [0, 1, 3, 9, 14, 16, 17, 20, 21, 22, 26, 27, 29]
DRIFT_CASES = [22]


def die(msg: str) -> None:
    sys.exit(f"regen-verify-packs: ERROR: {msg}")


def binding_for(idx: int, memory: dict) -> dict | None:
    """The spec binding for `real_{idx}`, translated from `memories-full.json`'s first binding to the
    {kind, value} the dump entrypoint takes. `memories-full.json` is the single source of truth for
    anchors (it also feeds the harness's `bind_of` label), so a moved anchor is fixed THERE, not here
    — e.g. real_3 (github→papertrail rename) and real_22 (re-anchored to scoring.rs, the file its memory
    is about, so the /repo-drift `if class.refined` guard removal lands in the excerpt)."""
    bindings = memory.get("bindings") or []
    if not bindings:
        return None
    b = bindings[0]
    kind = b["binding_kind"]
    if kind == "logical_symbol":
        return {"kind": "logical_symbol", "value": b["binding_id"]}
    if kind in ("path", "dir"):
        return {"kind": kind, "value": b["path"]}
    die(f"real_{idx}: unhandled binding kind `{kind}`")
    return None  # unreachable (die exits); explicit so all paths return the same type


def spec_memory(idx: int, memory: dict) -> dict:
    return {
        "eval_id": f"real_{idx}",
        "kind": memory["kind"],
        "title": memory["title"],
        "body": memory["body"],
        "confidence": memory.get("confidence", "high"),
        "binding": binding_for(idx, memory),
    }


def write_config(root: pathlib.Path, db: pathlib.Path) -> pathlib.Path:
    cfg = root / "regen-rag-rat.toml"
    cfg.write_text(
        f'[index]\nroot = "{root}"\ndatabase = "{db}"\n\n'
        f'[target_bindings]\nrust = ["crates"]\nmarkdown = ["docs"]\n'
    )
    return cfg


def run(bin_path: str, cfg: pathlib.Path, *args: str) -> None:
    cmd = [bin_path, "--config", str(cfg), *args]
    proc = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True)
    if proc.returncode != 0:
        die(f"`{' '.join(cmd)}` failed:\n{proc.stdout}\n{proc.stderr}")


def drift_edits_for(cases: list, memories_full: list) -> list:
    """The make-drift-tree `EDITS` whose target file a drift CASE is anchored to — so regen doctors
    ONLY what its pack cases surface (real_22 → scoring.rs), never an edit for a dropped case."""
    wanted = {binding_for(i, memories_full[i])["value"] for i in cases}
    edits = [e for e in mdt.EDITS if f"crates/{e[0]}" in wanted]
    if len(edits) != len(cases):
        die(f"drift cases {cases} need edits {wanted}, but only matched {[e[0] for e in edits]}")
    return edits


def build_repo_root(work: pathlib.Path, doctored: bool, drift_edits: list) -> pathlib.Path:
    """A checkout of the WORKING TREE's tracked crates/ + docs/ under `work/root`. When `doctored`,
    apply `drift_edits` (make-drift-tree guard removals) — only the edits this run's cases need."""
    root = work / ("drift-root" if doctored else "repo-root")
    root.mkdir(parents=True)
    # Copy the WORKING TREE's tracked crates/ + docs/ (NOT `git archive HEAD`): a dev who edits
    # indexing/source and regenerates BEFORE committing must see those edits reflected, or the corpus
    # silently captures the old committed code. `git ls-files` scopes to tracked paths (untracked /
    # gitignored excluded); `tar` reads their CURRENT content — so on a clean checkout this is
    # byte-identical to HEAD and the committed corpus stays reproducible. Subtract `--deleted` so a
    # tracked file removed/renamed but not yet committed doesn't make `tar` fail on a missing path
    # (modified-but-present files are kept).
    def ls(*flags: str) -> set:
        out = subprocess.run(
            ["git", "ls-files", "-z", *flags, "crates", "docs"], cwd=REPO_ROOT,
            capture_output=True, check=True,
        ).stdout
        return {p for p in out.split(b"\0") if p}

    listing = b"\0".join(sorted(ls() - ls("--deleted"))) + b"\0"
    archived = subprocess.run(
        ["tar", "--null", "-T", "-", "-cf", "-"], cwd=REPO_ROOT, input=listing, capture_output=True,
        check=True,
    ).stdout
    subprocess.run(["tar", "-x", "-C", str(root)], input=archived, check=True)
    for rel, if_line, label in drift_edits:
        target = root / "crates" / rel
        target.write_text(mdt.remove_guard_block(target.read_text(encoding="utf-8"), if_line, label),
                          encoding="utf-8")
    # rag-rat indexes git history; give the throwaway root a clean single-commit git identity.
    for cmd in (
        ["git", "init", "-q"],
        ["git", "add", "-A"],
        ["git", "-c", "user.email=eval@local", "-c", "user.name=eval", "commit", "-qm", "regen"],
    ):
        subprocess.run(cmd, cwd=root, check=True, capture_output=True)
    return root


def dump_for_root(bin_path: str, work: pathlib.Path, memories_full: list, doctored: bool) -> dict:
    root_label = "/repo-drift" if doctored else "/repo"
    cases = DRIFT_CASES if doctored else REPO_CASES
    drift_edits = drift_edits_for(cases, memories_full) if doctored else []
    root = build_repo_root(work, doctored, drift_edits)
    db = work / (f"{'drift' if doctored else 'repo'}.sqlite")
    cfg = write_config(root, db)
    print(f"  indexing {root_label} at {root} …", flush=True)
    run(bin_path, cfg, "index")
    spec = {
        "memories": [spec_memory(i, memories_full[i]) for i in cases],
        "dump": [f"real_{i}" for i in cases],
    }
    spec_path = work / f"spec{'-drift' if doctored else ''}.json"
    spec_path.write_text(json.dumps(spec))
    out_path = work / f"out{'-drift' if doctored else ''}.json"
    run(
        bin_path, cfg,
        "dump-verify-packs", "--spec", str(spec_path),
        "--root-label", root_label, "--out", str(out_path),
    )
    return json.loads(out_path.read_text())


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=str(REPO_ROOT / "target" / "debug" / "rag-rat"),
                    help="rag-rat binary built with --features eval")
    ap.add_argument("--keep", action="store_true", help="keep the scratch dir for inspection")
    args = ap.parse_args()

    memories_full = json.loads((CORPUS / "memories-full.json").read_text())
    work = pathlib.Path(tempfile.mkdtemp(prefix="regen-packs-"))
    try:
        packs = {}
        packs.update(dump_for_root(args.bin, work, memories_full, doctored=False))
        packs.update(dump_for_root(args.bin, work, memories_full, doctored=True))
    finally:
        if args.keep:
            print(f"  scratch kept at {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)

    out = CORPUS / "verify-packs.json"
    out.write_text(json.dumps(packs, indent=1, sort_keys=True) + "\n")
    print(f"wrote {len(packs)} packs to {out.relative_to(REPO_ROOT)}")
    for key in sorted(packs):
        print(f"  {key}")


if __name__ == "__main__":
    main()
