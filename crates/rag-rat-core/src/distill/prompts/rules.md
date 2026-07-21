Field rules:
- root_issue: what was broken or requested, 1-2 sentences, from the reporter's point of view. null if the thread does not establish one (e.g. a pure review stream with no stated problem).
- root_cause_units: the [U#] numbers of the units that establish the underlying cause ([] if none).
- root_cause: the underlying technical cause, 1-2 sentences, or null if the thread does not establish one.
- root_cause_class: a 2-5 word failure-class label (e.g. "lock contention", "stale cache invalidation"), or null.
- decision_units: the [U#] numbers of the units where the approach was decided.
- decision.chosen: the CORE approach only, at most 2 sentences; exclude incidental changes (dependency bumps, test de-flaking). null if the thread settled no clear approach (e.g. a thin thread or a pure review stream).
- decision.rejected: alternatives EXPLICITLY considered and not taken, as an array of {"alternative": ..., "reason": ...} objects; an item's reason is null when the thread gave no rationale (e.g. [{"alternative": "plan A", "reason": null}]). [] if none were discussed. The decision is what LANDED, not what a reviewer merely proposed.
- outcome_units: the [U#] numbers of the units that establish what actually happened ([] if none, e.g. the outcome is known only from the fixing commit).
- anchor_indices: the unique [A#] indices of the bounded ANCHOR CANDIDATES that identify the code most directly involved in the core decision and outcome. Select only visible candidate indices; use [] when none applies. Never invent a path, symbol, or index.
- outcome.status: landed | descoped | superseded | reverted | unclear.
- outcome.summary: what actually happened, 1-2 sentences, concrete, or null if the thread does not establish it. State measured results as results and projections as projections — never assert a projected improvement as a delivered result.

PARTNER THREADs (the paired issue(s) or pull request(s)) may inform how you READ the primary thread, but their units are not numbered and cannot be cited. Every claim must be grounded in the numbered THREAD UNITS: if only a partner thread establishes something, leave that field null (or its citations []) rather than assert it without evidence — the fixing commit may still establish the outcome.
