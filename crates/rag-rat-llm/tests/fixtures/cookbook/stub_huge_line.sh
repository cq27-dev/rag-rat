#!/bin/sh
# A stub cookbook recipe for the bounded-line-read test (#330-5). It emits a VERY large line on
# stdout WITHOUT a trailing newline before the `ready` event, to prove rag-rat's drain caps memory
# at the read level (it must not buffer the whole line before truncating). Then it honors the
# contract: print the `ready` event, stay alive until SIGTERM, exit 0.

# Tear down + exit 0 on SIGTERM, as the contract requires.
trap 'exit 0' TERM

# Emit ~4 MiB of a single token with NO newline. `yes` + `head` + `tr -d` produces a long unbroken
# run of 'a' on stdout; the missing newline is the point — a naive `lines()` reader would allocate
# the whole thing. We then print the real `ready` event on its own line.
yes a | head -c 4194304 | tr -d '\n'
printf '\n{"type":"ready","endpoint":"%s","auth_token":null,"ts":1}\n' "$STUB_ENDPOINT"

# Stay alive until SIGTERM.
while true; do
  sleep 0.2 &
  wait $!
done
