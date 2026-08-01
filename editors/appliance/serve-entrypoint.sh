#!/bin/sh
# Lens backend container entrypoint (#1106 two-container split): the ONLY process with
# the DB mount. A visitor who spawns a task/debug process in the workbench container
# has no DB to reach — the execution boundary the public demo needs.
set -eu

# Group-share created files (locks, WAL sidecars) with the host operator: the DB dir is
# group-write setgid on the host and the container default umask 022 would stamp them
# group-read-only.
umask 002

# Non-loopback serve REQUIRES an explicit bearer token (no loopback auto-generation).
# Mint one per boot; serve publishes it in the discovery file the extension reads.
RAG_RAT_LENS_TOKEN=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')
export RAG_RAT_LENS_TOKEN

# The extension runs server-side in the WORKBENCH container and its discovery contract
# accepts loopback URLs only — so the discovery advertises 127.0.0.1, where a tiny
# forwarder in the workbench relays to this container over the internal network. CORS
# allows exactly the public origin for the case the extension runs in a browser worker.
LENS_ADVERTISE_URL="${LENS_ADVERTISE_URL:-http://127.0.0.1:18120}"
export LENS_ADVERTISE_URL
exec rag-rat --config /srv/config/rag-rat.toml serve \
  --bind "$(hostname -i)" --port 18120 \
  --token-env RAG_RAT_LENS_TOKEN \
  --allow-origin "${LENS_PUBLIC_ORIGIN:?set LENS_PUBLIC_ORIGIN to the public https origin}" \
  --advertise-url "$LENS_ADVERTISE_URL"
