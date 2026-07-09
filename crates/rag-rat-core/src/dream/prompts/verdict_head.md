You are auditing a repo-intelligence memory NOTE against the repository as it exists RIGHT NOW. You are given a mechanically-generated EVIDENCE PACK from the current checkout: a whole-tree resolution of the identifiers the note mentions, plus the current text of the note's bound file. The note was written in the past: the code may have moved past it, the note may describe in-flight work not present in this checkout, or they may agree.

Read each identifier's resolution LITERALLY. The resolutions mean exactly:
- `symbol <path>::<name>` (or `symbols (N): …`) — a defined code symbol of that name EXISTS in the tree. Present.
- `file <path>` (or `files (N): …`) — a file of that path EXISTS in the tree. Present.
- `not a defined symbol; appears verbatim as source text` — the token EXISTS in the source as literal text (a table/column name, a local variable, an expression, an attribute), just not as a DEFINED symbol. Treat it as PRESENT — unless the note specifically claims it is a defined function/type/symbol of that name, in which case "not a defined symbol" is a contradiction.
- `not an indexed file; appears verbatim only as source text` — a path that EXISTS in the source as literal text (mentioned in a comment or string) but is NOT an indexed file. Treat it as PRESENT — unless the note specifically claims a FILE of that path exists, in which case "not an indexed file" is a contradiction.
- `NOT FOUND anywhere in the source tree` — a name-shaped identifier that exists NOWHERE in the tree: not a symbol, not a file, not even as literal text. This is the ONLY resolution that is evidence of ABSENCE.

Absence alone is not divergence: a `NOT FOUND` identifier the note merely mentions in passing does not contradict the note. It is divergence only when the note makes a LOAD-BEARING claim about that name (it says the function/type/table/field was added, exists, or behaves a certain way) and the pack shows it `NOT FOUND` (or present only as text while the note calls it a defined symbol).

Output EXACTLY this format and nothing else:
VERDICT: current | diverged
DIRECTION: code_ahead | note_ahead | unknown
EVIDENCE:
- <one line copied verbatim from the EVIDENCE PACK below that supports the verdict>
REASON: <one sentence>

Meanings: current = the note's load-bearing claims are visible in the pack as described (or nothing in the pack contradicts them). diverged = the pack clearly CONTRADICTS a load-bearing claim. code_ahead = the code changed after the note was written. note_ahead = the note describes work this checkout does not contain yet — a symbol the note says was added is `NOT FOUND` while the note's bound file DOES exist. DIRECTION is "unknown" unless VERDICT is diverged and you can tell which side is newer. Every EVIDENCE line must be copied verbatim from the EVIDENCE PACK below — never invent one.
