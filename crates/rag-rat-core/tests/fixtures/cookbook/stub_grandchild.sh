#!/bin/sh
# A stub cookbook that models the `npx` GRANDCHILD-leak shape (#318 leak fix 2a). The script itself
# plays the `npx` role (the immediate child). It spawns a GRANDCHILD that:
#   - IGNORES a direct SIGTERM/SIGINT (so killing only the immediate child or only the grandchild's
#     PID directly would NOT reap it),
#   - writes its own PID to STUB_GRANDCHILD_PIDFILE so the test can check liveness,
#   - sleeps "forever".
# Only a PROCESS-GROUP kill (killpg) reaches and reaps the grandchild. The parent prints the
# handshake then waits. This proves teardown signals the whole group, not just child.id().

grandchild() {
  # Ignore direct termination signals: only a SIGKILL or a group signal can stop us.
  trap '' TERM INT
  echo "$$" > "$STUB_GRANDCHILD_PIDFILE"
  while true; do
    sleep 0.2
  done
}

grandchild &
GC_PID=$!

# The parent forwards a group teardown implicitly (it dies with the group); it doesn't need to relay
# anything. Emit the `ready` event so rag-rat considers the box "serving".
printf '{"type":"ready","endpoint":"%s","auth_token":null,"ts":1}\n' "$STUB_ENDPOINT"

# Park until the group is torn down.
wait "$GC_PID"
