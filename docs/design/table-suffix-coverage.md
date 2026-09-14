# Interrupted table suffix delivery

A retention floor identifies an accepted root, not a complete state snapshot. If a source compacts a delete into a later restatement, an intermediary can accept the floor and disconnect before that restatement arrives. A fresh peer that first received the old upsert from another source then adopts the intermediary’s floor. Its old physical row survives: re-rooting does not erase projections. Advertising only the intermediary’s accepted tail incorrectly hides the remaining delivery work.

## Delivery obligation

V129 records one outstanding `(stream, device, floor, tip lamport, tip hash)` when a new floor is adopted through a session carrying an advertised tip. The record commits atomically with adoption. It survives restart and propagates through the existing chain inventory: an incomplete store advertises the owed tip while receive frontiers still describe only accepted entries. Exhausting local entries before the offered tip reports `continuation_pending`; a requested cursor ahead of the local tail but within that outstanding suffix returns an empty pending page so the reverse direction can deliver missing entries.

The obligation clears only when its exact signed target is in the accepted chain, including promotion after a missing predecessor arrives. Unknown payloads may satisfy delivery while remaining pending projection. Rejected, gapped, unsigned or numerically higher tips cannot satisfy delivery. While an obligation remains, another discontinuous root is refused; source switching may supply contiguous entries but cannot overwrite the original target.

Authoring, re-adoption and compaction wait for delivery on the affected stream. Physical edits stay local and unsent until recovery. Other streams remain available. Repository purge removes row data, entries and floors, but retains the obligation alongside chain witnesses. A same-incarnation rejoin still owes the original suffix; a different incarnation derives another stream ID and is unaffected. Obligations alone never advertise entries or recreate a directory. Restoring a witness does not reconstruct projections intentionally discarded by purge.

## Recovery and limits

Reconnect to any authorized source retaining the missing contiguous suffix. Two partial replicas may advance one another even when neither has the final tip. If every source has compacted beyond the required target, or the signer is no longer authorized by the current roster, delivery remains explicitly pending. A higher advertised floor is not an ancestry proof. Recovery from that condition requires a separate authenticated snapshot or coverage protocol; silently deleting the obligation would reintroduce the original ambiguity.

This is encoding-compatible with the existing table wire format. Transitive protection requires updated intermediaries: older receivers do not persist the promised tip. Existing floor records contain no historical promised tip, so migration cannot reconstruct earlier interrupted sessions and does not invent evidence or block all existing compacted stores. The guarantee concerns newly observed adoptions, not retroactive validation of legacy floors.

Tips are authenticated-peer routing advice, not independently signed completeness certificates. They never raise accepted-chain witnesses or Lamport clocks. An incorrect promise can leave the stream pending; the record is bounded to one obligation per accepted device chain. Closed-scope admission remains unchanged.

Suffix delivery is not projection success, global completeness, historical write authority, roster-wide acknowledgement, or permission to collect tombstone rows. Those remain separate requirements in #892 and #1295.
