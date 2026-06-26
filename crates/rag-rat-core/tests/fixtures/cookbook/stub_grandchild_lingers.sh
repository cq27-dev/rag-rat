#!/bin/sh
# A stub cookbook that models the STUCK-TEARDOWN leak (#318 leak fix 2b): the leader (`npx` role,
# this script) exits PROMPTLY on SIGTERM, but the box-holding GRANDCHILD is still finishing its own
# teardown when the leader is reaped. The naive teardown (return the instant the leader's `waitpid`
# succeeds) would skip the SIGKILL backstop and leave the grandchild — the paid box — orphaned.
#
# The grandchild here:
#   - IGNORES a direct SIGTERM (so the leader exiting does NOT take it down),
#   - writes its own PID to STUB_GRANDCHILD_PIDFILE so the test can check liveness,
#   - lingers briefly (a slow-but-eventually-completing teardown), then exits ON ITS OWN.
# A correct `teardown_group` must WAIT on the whole process GROUP (killpg(pgid,0)), not just the
# leader — so `Drop` must not return until the grandchild is also gone.

grandchild() {
  trap '' TERM INT
  echo "$$" > "$STUB_GRANDCHILD_PIDFILE"
  # A slow teardown: outlives the leader's prompt exit, then completes on its own (well under the
  # 10s grace, so the SIGKILL backstop need not fire for the GROUP wait to observe the exit).
  sleep 1
}

grandchild &

# The leader exits IMMEDIATELY on SIGTERM (it does NOT wait for the grandchild) — this is the shape
# that broke the leader-only `waitpid` teardown.
trap 'exit 0' TERM

# Emit the `ready` event so rag-rat considers the box "serving".
printf '{"type":"ready","endpoint":"%s","auth_token":null,"ts":1}\n' "$STUB_ENDPOINT"

# Park until SIGTERM; `wait` lets the trap fire promptly without waiting on the grandchild.
while true; do
  sleep 0.2 &
  wait $!
done
