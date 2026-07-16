#!/bin/sh
# A stub cookbook recipe that FAILS to provision: it prints a diagnostic to stderr then exits
# non-zero BEFORE ever printing a handshake. The test asserts `provision` errors with the captured
# stderr (the recipe's own failure message).
echo "stub: could not reach the cloud provider" >&2
echo "stub: giving up" >&2
exit 17
