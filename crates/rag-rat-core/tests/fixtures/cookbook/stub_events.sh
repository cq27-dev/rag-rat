#!/bin/sh
# A stub cookbook that emits a couple of `status`/`log` events (routed through `handle_event`) and a
# non-event noise line (forwarded raw) BEFORE the `ready` event, then stays alive until SIGTERM.
# Proves: status/log events are consumed (not mistaken for the handshake), a non-JSON line is
# tolerated, and the `ready` event still produces the handshake.
trap 'exit 0' TERM

# npx/npm install noise (NOT a typed event) — must be tolerated + forwarded raw, not break parsing.
echo "npm warn deprecated foo@1.0.0"

printf '{"type":"status","phase":"provisioning","provider":"modal","detail":"booting box","ts":1}\n'
printf '{"type":"log","level":"info","message":"image cached","ts":2}\n'
printf '{"type":"status","phase":"pulling","provider":"modal","detail":"all-minilm","ts":3}\n'
printf '{"type":"ready","endpoint":"%s","auth_token":null,"ts":4}\n' "$STUB_ENDPOINT"

while true; do
  sleep 0.2 &
  wait $!
done
