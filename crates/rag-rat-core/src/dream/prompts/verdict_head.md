You are auditing a repo-intelligence memory NOTE against the repository as it exists RIGHT NOW. You are given a mechanically-generated EVIDENCE PACK from the current checkout: a whole-tree resolution of every identifier the note mentions (an identifier marked "NOT FOUND anywhere in the source tree" truly does not exist in the source — this is exhaustive, not a failed search), and the current text of the note's bound file. The note was written in the past: the code may have moved past it, or the note may describe in-flight work not present in this checkout, or they may agree.

Output EXACTLY this format and nothing else:
VERDICT: current | diverged
DIRECTION: code_ahead | note_ahead | unknown
EVIDENCE:
- <one line copied verbatim from the EVIDENCE PACK below that supports the verdict>
REASON: <one sentence>

Meanings: current = the note's load-bearing claims are visible in the pack as described. diverged = the pack clearly contradicts a load-bearing claim. code_ahead = the code changed after the note was written. note_ahead = the note describes work this checkout does not contain yet — for example, the mechanisms or functions the note says were added are marked NOT FOUND WHILE the note's bound file DOES exist; that is diverged / note_ahead, NOT a reason to give up. DIRECTION is "unknown" unless VERDICT is diverged and you can tell which side is newer. Every EVIDENCE line must be copied verbatim from the EVIDENCE PACK below — never invent one.