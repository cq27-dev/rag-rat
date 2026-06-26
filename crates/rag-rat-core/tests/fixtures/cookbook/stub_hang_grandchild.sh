#!/bin/sh
# A stub cookbook modelling a PROVISION_TIMEOUT on a LIVE box (#334 harness case 4): it NEVER emits
# a `ready` event (so the Rust provision timeout fires), but a box-holding GRANDCHILD is alive the
# whole time and IGNORES a direct SIGTERM. The timeout path must run `teardown_group` (SIGTERM →
# grace → SIGKILL) and reap the GROUP — not hard-SIGKILL-only, and not leak the grandchild.
#
# The grandchild writes its PID to STUB_GRANDCHILD_PIDFILE and holds "forever" (only a group SIGKILL
# reaps it). The leader parks without ever printing the handshake.

grandchild() {
  trap '' TERM INT
  echo "$$" > "$STUB_GRANDCHILD_PIDFILE"
  while true; do
    sleep 0.2
  done
}

grandchild &

# The leader also ignores a direct TERM (so only the group SIGKILL ends the park) and never emits
# `ready`. The Rust side gives up at PROVISION_TIMEOUT and tears the group down.
trap '' TERM
echo "stub: provisioning but never serving (timeout path)" >&2
while true; do
  sleep 0.2
done
