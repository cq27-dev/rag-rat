Field rules:
- root_issue: what was broken or requested, 1-2 sentences, from the reporter's point of view. null if the thread does not establish one (e.g. a pure review stream with no stated problem).
- root_cause_units: the [U#] numbers of the units that establish the underlying cause ([] if none).
- root_cause: the underlying technical cause, 1-2 sentences, or null if the thread does not establish one.
- root_cause_class: a 2-5 word failure-class label (e.g. "lock contention", "stale cache invalidation"), or null.
- decision_units: the [U#] numbers of the units where the approach was decided.
- decision.chosen: the CORE approach only, at most 2 sentences; exclude incidental changes (dependency bumps, test de-flaking). null if the thread settled no clear approach (e.g. a thin thread or a pure review stream).
- decision.rejected: alternatives EXPLICITLY considered and not taken, as an array of {"alternative": ..., "reason": ...} objects; an item's reason is null when the thread gave no rationale (e.g. [{"alternative": "plan A", "reason": null}]). [] if none were discussed. The decision is what LANDED, not what a reviewer merely proposed.
- outcome_units: the [U#] numbers of the units that establish what actually happened ([] if none, e.g. the outcome is known only from the fixing commit).
- outcome.status: landed | descoped | superseded | reverted | unclear.
- outcome.summary: what actually happened, 1-2 sentences, concrete, or null if the thread does not establish it. State measured results as results and projections as projections — never assert a projected improvement as a delivered result.

A PARTNER THREAD (the paired issue or pull request) may inform how you READ the primary thread, but its units are not numbered and cannot be cited. Every claim must be grounded in the numbered THREAD UNITS: if only the partner thread establishes something, leave that field null (or its citations []) rather than assert it without evidence — the fixing commit may still establish the outcome.
