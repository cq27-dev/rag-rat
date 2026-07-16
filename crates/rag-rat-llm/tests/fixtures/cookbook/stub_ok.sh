#!/bin/sh
# A stub cookbook recipe for the ephemeral-provisioning tests (#318). It honors the rag-rat ⇄
# cookbook contract: stdout is a typed JSONL event stream, ending with a `ready` event when serving;
# then stay alive until SIGTERM, then exit 0. The serving endpoint is passed in via STUB_ENDPOINT so
# the test can point it at a local stub HTTP server. STUB_AUTH (optional) becomes the ready event's
# auth_token; absent → null.
#
# Plain POSIX sh so the tests need no node/npx on the machine (the production resolver routes
# .mjs/.ts/npm-spec, but the test drives the provisioner with a pre-built `Command`).

# Some diagnostic output on stderr (non-event output is stderr). The test asserts this is forwarded.
echo "stub: provisioning box (input=$RAG_RAT_COOKBOOK_INPUT)" >&2

# Tear down + exit 0 on SIGTERM, as the contract requires. If STUB_TEARDOWN_MARKER is set, touch it
# so the test can prove teardown ran (the box was reclaimed) when the ProvisionedBox dropped.
teardown() {
  echo "stub: SIGTERM → teardown" >&2
  [ -n "$STUB_TEARDOWN_MARKER" ] && : > "$STUB_TEARDOWN_MARKER"
  exit 0
}
trap teardown TERM

throughput=""
if [ -n "$STUB_THROUGHPUT" ]; then
  throughput=',"throughput":{"concurrency":4,"batch_size":128,"server_concurrency":4,"max_batch_chars":96000}'
fi

# The `ready` event (the handshake) — type-tagged JSONL with a ts field.
if [ -n "$STUB_AUTH" ]; then
  printf '{"type":"ready","endpoint":"%s","auth_token":"%s","ts":1%s}\n' "$STUB_ENDPOINT" "$STUB_AUTH" "$throughput"
else
  printf '{"type":"ready","endpoint":"%s","auth_token":null,"ts":1%s}\n' "$STUB_ENDPOINT" "$throughput"
fi

# Stay alive until SIGTERM. `wait` lets the trap fire promptly; the background sleep loop keeps the
# process running without busy-spinning.
while true; do
  sleep 0.2 &
  wait $!
done
