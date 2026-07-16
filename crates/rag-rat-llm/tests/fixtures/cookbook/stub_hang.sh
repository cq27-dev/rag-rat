#!/bin/sh
# A stub cookbook recipe that NEVER prints a handshake (and never exits on its own): it just stays
# alive forever. The test drives `provision` with a short timeout and asserts it errors with the
# "provisioning timed out" message, then the stub is killed.
echo "stub: pretending to provision, but never serving" >&2
trap 'exit 0' TERM
while true; do
  sleep 0.2 &
  wait $!
done
