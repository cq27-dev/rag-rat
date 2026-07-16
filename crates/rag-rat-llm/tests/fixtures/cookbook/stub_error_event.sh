#!/bin/sh
# A stub cookbook that FAILS by emitting a typed `error` event on stdout (the JSONL stream) before
# exiting non-zero — never reaching `ready`. The test asserts `provision` surfaces the error event's
# message (the cookbook's own diagnosis), which is more actionable than the raw stderr tail.
printf '{"type":"status","phase":"provisioning","provider":"runpod","detail":"creating pod","ts":1}\n'
printf '{"type":"error","message":"runpod: no GPU capacity in region","ts":2}\n'
echo "stub: exiting after error event" >&2
exit 19
