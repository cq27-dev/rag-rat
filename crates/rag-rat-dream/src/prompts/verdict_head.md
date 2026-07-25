You are auditing a repo-intelligence memory NOTE against the repository as it exists RIGHT NOW. You are given a mechanically-generated EVIDENCE PACK from the current checkout: indexed resolutions of the identifiers the note mentions, plus current text excerpts from the note's bound files. The note was written in the past: the code may have moved past it, the note may describe in-flight work not present in this checkout, or they may agree.

Read each identifier's resolution LITERALLY. The resolutions mean exactly:
- `symbol <path>::<name>` (or `symbols (N): …`) — a defined code symbol of that name EXISTS in the tree. Present.
- `file <path>` (or `files (N): …`) — a file of that path EXISTS in the tree. Present.
- `not a defined symbol; appears verbatim as source text` — the token EXISTS in the source as literal text (a table/column name, a local variable, an expression, an attribute), just not as a DEFINED symbol. Treat it as PRESENT — unless the note specifically claims it is a defined function/type/symbol of that name, in which case "not a defined symbol" is a contradiction. COMMON FALSE POSITIVE: a table/column name (`content_hash`), a meta/config key (`fts_synced_at_ms`), an env var, or a local variable resolves this way and IS present — do NOT rule `diverged` merely because it is not a defined function/type.
- `not an indexed file; appears verbatim only as source text` — a path that EXISTS in the source as literal text (mentioned in a comment or string) but is NOT an indexed file. Treat it as PRESENT — unless the note specifically claims a FILE of that path exists, in which case "not an indexed file" is a contradiction.
- `NOT FOUND anywhere in the source tree` — an authoritative miss emitted only when the note's exact live-file or live call-path domain is covered. This is the ONLY resolution that is evidence of ABSENCE.

Absence alone is not divergence: a `NOT FOUND` identifier the note merely mentions in passing does not contradict the note. It is divergence only when the note makes a LOAD-BEARING claim about that name (it says the function/type/table/field was added, exists, or behaves a certain way) and the pack shows it `NOT FOUND` (or present only as text while the note calls it a defined symbol).

A note DOCUMENTING ITS OWN HISTORY agrees with reality, it does not contradict it: if the note itself says a name was REMOVED, RENAMED (A→B), REPLACED, or is DELIBERATELY ABSENT, then that name resolving `NOT FOUND` CONFIRMS the note — verdict `current`, not `diverged`. Only a name the note claims currently EXISTS or was ADDED, shown `NOT FOUND`, is divergence.

Output EXACTLY this format and nothing else:
VERDICT: current | diverged
DIRECTION: code_ahead | note_ahead | unknown
CLAIM: <for diverged, one load-bearing claim copied verbatim from the NOTE; for current, NONE>
EVIDENCE:
- <one line copied verbatim from the EVIDENCE PACK below that supports the verdict>
REASON: <one sentence>

Meanings: current = the note's load-bearing claims are visible in the pack as described (or nothing in the pack contradicts them). diverged = the pack clearly CONTRADICTS a load-bearing claim. code_ahead = the code changed after the note was written. note_ahead = the note describes work its exact live-file or live call-path domain does not contain yet — an identifier the note says was added is authoritatively `NOT FOUND`. DIRECTION is "unknown" unless VERDICT is diverged and you can tell which side is newer. For `diverged`, CLAIM must copy the contradicted claim from the NOTE WHOLE and verbatim — the complete sentence or span from the TITLE or the BODY (never spliced across both, never a prefix with additions; capitalization and markdown punctuation may differ). Every EVIDENCE line must be a FULL line copied verbatim from the EVIDENCE PACK below, including its `- `identifier` -> …` row prefix or `path:line:` locator — never invent one, and never cite a bare resolution label such as `NOT FOUND`. For `diverged`, cite the `NOT FOUND` row of an identifier the claim names. Excerpts are context only: do not use an excerpt as the sole proof of contradiction. A `diverged` verdict supported only by an excerpt, present-as-source-text row, or symbol-present row will be rejected. For `current`, write `CLAIM: NONE`.
