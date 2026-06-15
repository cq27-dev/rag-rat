#!/usr/bin/env python3
"""Read fields out of tools/oracle-corpora.toml for the shell oracle runner.

The runner (tools/oracle-run.sh) is bash, which can't parse TOML; this is the thin reader it shells
out to. `rag-rat oracle report` consumes the corpus profile (tool, bindings, health) from the same
file directly — this helper only surfaces the few fields the runner needs *before* indexing: the
repo + rev to clone, the prepare steps to run, and the bindings to render into the corpus's
rag-rat.toml. It also lists a tier's corpus ids so a CI matrix can fan out over them.

Pure stdlib (tomllib, 3.11+). No third-party deps so it runs on a bare CI runner.

Usage:
  oracle-corpus.py --list-tier small                 # corpus ids in a tier, one per line
  oracle-corpus.py --corpus py-requests --field repo # a scalar field
  oracle-corpus.py --corpus py-requests --field prepare        # one prepare command per line
  oracle-corpus.py --corpus py-requests --field bindings_toml  # rag-rat.toml [target_bindings] body
"""

from __future__ import annotations

import argparse
import sys
import tomllib

SCALAR_FIELDS = ("repo", "rev", "tool", "tier")


def load_corpora(path: str) -> list[dict]:
    with open(path, "rb") as fh:
        data = tomllib.load(fh)
    corpora = data.get("corpus", [])
    if not corpora:
        sys.exit(f"oracle-corpus: {path} has no [[corpus]] entries (wrong table name?)")
    return corpora


def find(corpora: list[dict], corpus_id: str) -> dict:
    for corpus in corpora:
        if corpus.get("corpus_id") == corpus_id:
            return corpus
    sys.exit(f"oracle-corpus: no corpus '{corpus_id}' in the corpora file")


def emit_field(corpus: dict, field: str) -> None:
    if field in SCALAR_FIELDS:
        print(corpus[field])
    elif field == "timeout_minutes":
        print(corpus["health"]["timeout_minutes"])
    elif field == "prepare":
        # One command per line; an empty prepare list prints nothing (the runner loops over zero
        # lines). Commands may contain spaces — the runner reads them line-by-line, not word-split.
        for command in corpus.get("prepare", []):
            print(command)
    elif field == "bindings_toml":
        # Render the `[target_bindings]` body for the corpus's rag-rat.toml: one `lang = ["dir", …]`
        # line per language. TOML string-quote each directory so paths with spaces survive.
        for lang, dirs in corpus["bindings"].items():
            quoted = ", ".join('"' + d + '"' for d in dirs)
            print(f"{lang} = [{quoted}]")
    else:
        sys.exit(f"oracle-corpus: unknown field '{field}'")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpora", default="tools/oracle-corpora.toml")
    parser.add_argument("--corpus")
    parser.add_argument("--field")
    parser.add_argument("--list-tier")
    args = parser.parse_args()

    corpora = load_corpora(args.corpora)

    if args.list_tier:
        for corpus in corpora:
            if corpus.get("tier") == args.list_tier:
                print(corpus["corpus_id"])
        return

    if not args.corpus or not args.field:
        parser.error("either --list-tier, or both --corpus and --field, are required")

    emit_field(find(corpora, args.corpus), args.field)


if __name__ == "__main__":
    main()
