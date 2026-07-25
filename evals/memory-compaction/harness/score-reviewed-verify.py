#!/usr/bin/env python3
"""Score the #954 reviewed verifier replay through the production grounding rules."""

import collections
import argparse
import json
import pathlib
import re

EVAL_DIR = pathlib.Path(__file__).resolve().parent.parent
CORPUS = EVAL_DIR / "corpus"
RESULTS = EVAL_DIR / "results"
MIN_CITATION_CHARS = 10
MIN_CLAIM_CHARS = 20
MIN_CLAIM_WORDS = 4

# Comparison/boolean operators, two-character first (mirrors verdict.rs `mixed_stream`).
OPERATORS = ("==", "!=", "<=", ">=", "&&", "||")


def normalize(value: str) -> str:
    return " ".join(value.split())


def ascii_lower(value: str) -> str:
    """Rust `to_ascii_lowercase` / `eq_ignore_ascii_case` semantics: fold ASCII letters only,
    leave non-ASCII code points byte-for-byte case-sensitive."""
    return "".join(
        chr(ord(char) + 32) if "A" <= char <= "Z" else char for char in value
    )


def ascii_case_equal(left: str, right: str) -> bool:
    return ascii_lower(left) == ascii_lower(right)


def mixed_stream(value: str) -> list[tuple]:
    """Words and operators in source order as ("word", text, code, glued_to_prev) /
    ("op", symbol, glued_to_prev), case preserved — mirrors verdict.rs `mixed_stream`:
    arrows are punctuation, a unary `!` counts only glued to a following word, and `code`
    marks backticked word occurrences."""
    items = []
    in_code = False
    index = 0
    prev_item_end = None

    def is_word_char(char: str) -> bool:
        return char.isalnum() or char == "_"

    while index < len(value):
        char = value[index]
        if char == "`":
            in_code = not in_code
            index += 1
            continue
        if value.startswith("->", index) or value.startswith("=>", index):
            index += 2
            continue
        operator = next((op for op in OPERATORS if value.startswith(op, index)), None)
        if operator:
            items.append(("op", operator, prev_item_end == index))
            index += 2
            prev_item_end = index
            continue
        if char in "<>" or (
            char == "!" and index + 1 < len(value) and is_word_char(value[index + 1])
        ):
            items.append(("op", char, prev_item_end == index))
            index += 1
            prev_item_end = index
            continue
        if is_word_char(char):
            end = index
            while end < len(value) and is_word_char(value[end]):
                end += 1
            items.append(("word", value[index:end], in_code, prev_item_end == index))
            index = end
            prev_item_end = end
            continue
        index += 1
    return items


def text_mentions_identifier(text: str, identifier: str) -> bool:
    """Case-exact, separator-preserving identifier match with word boundaries — mirrors
    verdict.rs `text_mentions_identifier` (`foo/bar` never aliases `foo::bar`)."""
    identifier = identifier.strip()
    if not identifier:
        return False

    def is_word(char: str) -> bool:
        return char.isalnum() or char == "_"

    for match in re.finditer(re.escape(identifier), text):
        start, end = match.span()
        left_ok = start == 0 or not is_word(text[start - 1])
        right_ok = end == len(text) or not is_word(text[end])
        if left_ok and right_ok:
            return True
    return False


def claim_grounds_in_span(claim: str, span: str) -> bool:
    """The WHOLE claim grounds in the span: one contiguous verbatim word run (prose case-folded,
    backticked words case-exact per occurrence) whose operators keep the same positions — extended
    over operators GLUED to the boundary words (mirrors verdict.rs)."""
    stream = mixed_stream(span)
    claim_stream = mixed_stream(claim)
    claim_words = [item[1] for item in claim_stream if item[0] == "word"]
    if not claim_words:
        return False
    word_positions = [
        index for index, item in enumerate(stream) if item[0] == "word"
    ]
    if len(word_positions) < len(claim_words):
        return False
    for start in range(len(word_positions) - len(claim_words) + 1):
        for offset, claim_word in enumerate(claim_words):
            note_word, code = (
                stream[word_positions[start + offset]][1],
                stream[word_positions[start + offset]][2],
            )
            if code:
                matches = note_word == claim_word
            else:
                matches = ascii_case_equal(note_word, claim_word)
            if not matches:
                break
        else:
            first = word_positions[start]
            last = word_positions[start + len(claim_words) - 1]
            while (
                first > 0
                and stream[first - 1][0] == "op"
                and stream[first][-1]
            ):
                first -= 1
            while (
                last + 1 < len(stream)
                and stream[last + 1][0] == "op"
                and stream[last + 1][-1]
            ):
                last += 1
            window_shape = [
                item[1] if item[0] == "op" else None
                for item in stream[first : last + 1]
            ]
            claim_shape = [
                item[1] if item[0] == "op" else None for item in claim_stream
            ]
            if window_shape == claim_shape:
                return True
    return False


def claim_mentions_identifier(claim: str, identifier: str) -> bool:
    """The identifier must occur in the claim with its COMPLETE, case-exact shape — not as a
    substring, case variant, or separator variant (mirrors verdict.rs)."""
    return text_mentions_identifier(claim, identifier)


def parse(answer: str) -> dict | None:
    parsed = None
    in_evidence = False
    for raw_line in answer.splitlines():
        line = raw_line.strip()
        marker, separator, value = line.partition(":")
        marker = ascii_lower(marker)
        value = value.strip()
        # A marker requires its colon (mirrors verdict.rs `strip_ci`); a colon-less `EVIDENCE`
        # or `REASON` line must not open/close the evidence section.
        if not separator:
            marker = ""
        if marker == "verdict":
            # Only the FIRST word is load-bearing (mirrors Verdict::parse): small models append
            # prose (`VERDICT: diverged — the helper is gone`) — but an ECHOED CHOICE
            # (`current | diverged`, `current or diverged`) selects nothing and is rejected.
            words = value.split()

            def trim_non_alnum(word: str) -> str:
                start = 0
                end = len(word)
                while start < end and not word[start].isalnum():
                    start += 1
                while end > start and not word[end - 1].isalnum():
                    end -= 1
                return word[start:end]

            verdict = ascii_lower(trim_non_alnum(words[0])) if words else ""
            choice_connector = False
            another_alternative = False
            for word in words[1:]:
                token = ascii_lower(trim_non_alnum(word))
                if choice_connector and token in {"current", "diverged"}:
                    another_alternative = True
                    break
                choice_connector = token in {"or", "not"} or any(
                    char in "|/" for char in word
                )
            parsed = (
                None
                if another_alternative
                else (
                    {
                        "verdict": verdict,
                        "direction": "unknown",
                        "claim": None,
                        "evidence": [],
                    }
                    if verdict in {"current", "diverged"}
                    else None
                )
            )
            in_evidence = False
        elif parsed is not None and marker == "direction":
            parsed["direction"] = ascii_lower(value)
            in_evidence = False
        elif parsed is not None and marker == "claim":
            parsed["claim"] = value or None
            in_evidence = False
        elif parsed is not None and marker == "evidence":
            in_evidence = True
        elif parsed is not None and marker == "reason":
            in_evidence = False
        elif parsed is not None and in_evidence and line.startswith("-"):
            evidence = line.removeprefix("-").strip()
            if evidence:
                parsed["evidence"].append(evidence)
    return parsed


def pack_content_lines(pack: str) -> list[str]:
    content = []
    for raw_line in pack.splitlines():
        line = raw_line.strip()
        if line.startswith("- `"):
            content.append(normalize(line))
            continue
        locator, separator, _ = line.partition(": ")
        if separator and locator[-1:].isdigit():
            content.append(normalize(line))
    return content


def is_bare_locator(citation: str) -> bool:
    citation = citation.strip().removesuffix(":")
    if any(char.isspace() for char in citation):
        return False
    path, separator, line = citation.rpartition(":")
    return bool(separator and path and line.isdigit())




def identifier_from_pack_line(line: str) -> str | None:
    """The identifier of a rendered table row (``- `identifier` -> resolution``), else None —
    mirrors verdict.rs `identifier_from_pack_line`."""
    if not line.startswith("- `"):
        return None
    identifier, separator, _ = line[3:].partition("` ->")
    return identifier if separator else None


def production_accepts(case: dict, pack: str, answer: str) -> str:
    parsed = parse(answer)
    if parsed is None or not parsed["evidence"]:
        return "discarded"
    content = pack_content_lines(pack)
    citations = []
    for evidence in parsed["evidence"]:
        citation = normalize(evidence)
        if len(citation.replace(" ", "")) < MIN_CITATION_CHARS or is_bare_locator(
            citation
        ):
            return "discarded"
        if not any(citation in line for line in content):
            return "discarded"
        citations.append(citation)
    if parsed["verdict"] == "current":
        return "current"

    claim = normalize((parsed["claim"] or "").strip().strip('"'))
    # The claim must ground in the title OR the body — never a span spliced across the seam.
    if (
        len(claim.replace(" ", "")) < MIN_CLAIM_CHARS
        or len(claim.split()) < MIN_CLAIM_WORDS
        or not (
            claim_grounds_in_span(claim, case["title"])
            or claim_grounds_in_span(claim, case["body"])
        )
    ):
        return "discarded"
    text_present = re.compile(
        r"-> (?:not a defined symbol; appears verbatim as source text|"
        r"not an indexed file; appears verbatim only as source text)"
    )
    # Mirror verdict.rs `verdict_is_grounded`'s final predicate EXACTLY: some citation must match
    # some content line that is not a text-present row AND is linked to the claim — an
    # identifier-table row only as ABSENCE evidence the citation names (`-> NOT FOUND`, the
    # citation contains the identifier, the identifier occurs in the claim as a complete token
    # run). Excerpts remain model context, but cannot deterministically prove contradiction.
    if not any(
        citation in line
        and not text_present.search(line)
        and (
            (
                "-> NOT FOUND" in line
                and identifier in citation
                and claim_mentions_identifier(claim, identifier)
            )
            if (identifier := identifier_from_pack_line(line)) is not None
            else False
        )
        for citation in citations
        for line in content
    ):
        return "discarded"
    return "diverged"


def configured_dream_model() -> str:
    """The production verifier model pinned in the checkout's rag-rat.toml
    (`[llm.dream.remote].model`)."""
    import tomllib

    repo_root = EVAL_DIR.parents[1]
    try:
        config = tomllib.loads((repo_root / "rag-rat.toml").read_text())
        return config["llm"]["dream"]["remote"]["model"]
    except (OSError, KeyError, tomllib.TOMLDecodeError) as error:
        raise SystemExit(
            f"cannot read [llm.dream.remote].model from {repo_root / 'rag-rat.toml'}: {error}"
        ) from error


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--results",
        type=pathlib.Path,
        default=RESULTS / "reviewed-verify-results.json",
    )
    args = parser.parse_args()
    # The default results file is the production gate documented in the README: its exit code
    # must enforce the 0/<current> false-positive and >= 2/3 recall bars for the configured
    # model. Resolve BOTH sides — a lexical comparison lets an explicitly-passed path to the
    # same file silently disable the gate.
    gate = args.results.resolve() == (RESULTS / "reviewed-verify-results.json").resolve()
    cases = {
        case["id"]: case
        for case in json.loads((CORPUS / "reviewed-verify-replay.json").read_text())
    }
    diverged_total = sum(1 for case in cases.values() if case["expected_verdict"] == "diverged")
    current_total = len(cases) - diverged_total
    # The recall bar: two-thirds of the genuine findings, at least two.
    recall_bar = max(2, -(-2 * diverged_total // 3))
    packs = json.loads((CORPUS / "reviewed-verify-packs.json").read_text())
    results = json.loads(args.results.read_text())
    if not results:
        raise SystemExit(
            f"{args.results}: no model results — a failed generation run must not read as a "
            f"passing evaluation"
        )
    by_model = collections.defaultdict(list)
    for result in results:
        by_model[result["model"]].append(result)

    for model, rows in sorted(by_model.items()):
        items = [row["item"] for row in rows]
        if sorted(items) != sorted(cases):
            missing = sorted(set(cases) - set(items))
            duplicates = sorted(
                item for item, count in collections.Counter(items).items() if count > 1
            )
            raise SystemExit(
                f"{model}: incomplete replay — missing {missing}, duplicate {duplicates}; "
                f"expected exactly one result for each of the {len(cases)} corpus cases, "
                f"otherwise the fixed denominators misreport an incomplete run"
            )
        counts = collections.Counter()
        for row in rows:
            case = cases[row["item"]]
            actual = production_accepts(
                case,
                packs[f"{row['item']}|/repo"],
                row["answer"],
            )
            counts[(case["expected_verdict"], actual)] += 1
        false_positives = counts[("current", "diverged")]
        true_positives = counts[("diverged", "diverged")]
        accepted_divergences = false_positives + true_positives
        precision = (
            true_positives / accepted_divergences if accepted_divergences else 1.0
        )
        recall = true_positives / diverged_total if diverged_total else 1.0
        discarded = sum(
            count
            for (expected, actual), count in counts.items()
            if actual == "discarded"
        )
        print(
            f"{model}: fp={false_positives}/{current_total} "
            f"tp={true_positives}/{diverged_total} "
            f"precision={precision:.1%} recall={recall:.1%} discarded={discarded}/{len(cases)}"
        )
        if gate and model == configured_dream_model():
            if false_positives > 0 or true_positives < recall_bar:
                raise SystemExit(
                    f"production gate FAILED for {model}: requires 0/{current_total} false "
                    f"positives and >= {recall_bar}/{diverged_total} true positives, got "
                    f"fp={false_positives} tp={true_positives}"
                )
    if gate and configured_dream_model() not in by_model:
        raise SystemExit(
            f"production gate: configured model {configured_dream_model()!r} has no results in "
            f"{args.results} — a missing run must not read as a passing evaluation"
        )


if __name__ == "__main__":
    main()
