#!/bin/sh
# Workbench entrypoint: Code-OSS only. The Lens backend runs in its OWN container
# (serve-entrypoint.sh) so this container — the one public visitors drive — carries no
# DB mount and no write path to the index at all.
set -eu

# code-server user data is a fresh tmpfs at every boot (see compose): recreate the
# directory the read-only settings.json bind mounts into.
install -d /home/coder/.local/share/code-server/User

# Make the sibling `serve` container reachable at 127.0.0.1:18120: the extension's
# discovery contract accepts loopback URLs only, so the backend appears loopback-shaped
# from here while the DB mount stays exclusively in the serve container.
/usr/lib/code-server/lib/node /usr/local/bin/lens-loopback-proxy.js &

# PUBLIC READ-ONLY DEMO: no login gate (--auth none). The containment is structural,
# not a password: this container has no DB mount and no outbound network, the workspace
# and extensions dir are read-only, the default terminal shell is /bin/false, and the
# cgroup bounds are in compose (see docker-compose.yml and settings.json).
# --disable-proxy: kill the /proxy/<port> routes. A public session could otherwise
# reach container-local services (the Lens API on 18120) through the workbench,
# bypassing the backend's bearer auth; remote.autoForwardPorts stays off in the
# pinned settings for the same reason.
# --disable-file-downloads: the workbench file dialog can browse the whole container
# filesystem; without this a visitor could exfiltrate browsed files through the UI's
# download action. --disable-file-uploads closes the other direction (drag-drop a
# runnable payload into the workspace, then debug it — F5 needs no launch.json).
exec code-server --bind-addr 0.0.0.0:8080 --disable-telemetry --disable-update-check \
  --auth none --disable-proxy --disable-file-downloads --disable-file-uploads \
  --extensions-dir /opt/lens-ext /srv/workspace
