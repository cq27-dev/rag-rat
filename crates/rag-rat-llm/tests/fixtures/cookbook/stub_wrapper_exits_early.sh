#!/bin/sh
# A stub cookbook modelling the WRAPPER-EXITS-EARLY leak edge (#330-7, Part-1 fix). The leader (the
# `npx`/Node WRAPPER role, this script) exits RIGHT AFTER emitting `ready`, while the box-holding
# recipe GRANDCHILD keeps running and IGNORES a direct SIGTERM. So by the time `Drop`/`teardown_group`
# runs, the leader's `waitpid` ALREADY succeeds — the buggy early-return would `store(0)` + return
# WITHOUT a group probe or killpg, orphaning the grandchild (the paid box). The fix must always probe
# the GROUP and run the full SIGTERM→grace→SIGKILL teardown.
#
# The grandchild:
#   - IGNORES a direct SIGTERM/SIGINT (only a process-GROUP signal — eventually SIGKILL — reaps it),
#   - writes its own PID to STUB_GRANDCHILD_PIDFILE so the test can check liveness,
#   - holds for LONGER than the test's leak-assertion window. This is the crux of the case: the
#     grandchild must NOT self-terminate before the assertion, or a leak (no killpg) would be masked
#     by the grandchild exiting on its own. Only the fix's group teardown can reap it within the
#     window, so the test FAILS pre-fix (grandchild still alive) and PASSES after.

grandchild() {
  trap '' TERM INT
  echo "$$" > "$STUB_GRANDCHILD_PIDFILE"
  sleep 30
}

grandchild &

# Emit the `ready` event so rag-rat considers the box "serving"…
printf '{"type":"ready","endpoint":"%s","auth_token":null,"ts":1}\n' "$STUB_ENDPOINT"

# …then the WRAPPER exits IMMEDIATELY (does NOT park, does NOT wait for the grandchild). This is the
# shape that broke the `try_wait()==exited → return` early path.
exit 0
