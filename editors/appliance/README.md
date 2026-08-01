# Lens web appliance (#1106)

A turnkey, shareable browser appliance around the rag-rat Lens extension and its HTTP/SSE
backend, for users who do not run VS Code or Cursor locally. It is **unmodified
code-server (Code-OSS)** with the *same* extension vsix that is published to the
marketplaces, plus the `rag-rat` binary serving the Lens API, plus a Cloudflare Tunnel
sidecar for TLS+DNS-less sharing.

Layout:

- `Dockerfile` — code-server base + `artifacts/rag-rat` + `artifacts/rag-rat-lens.vsix`
- `entrypoint.sh` — workbench entrypoint (code-server only)
- `serve-entrypoint.sh` — Lens backend entrypoint (the only container with the DB mount)
- `docker-compose.yml` — workbench + `serve` + `cloudflared` sidecar

Two containers, deliberately: the workbench public visitors drive has **no DB mount and
no outbound network**, so a process spawned from a task or debug config (which settings
cannot pin — a task can name its own shell) has no index to reach or corrupt. The
backend publishes its discovery file with `--advertise-url` (the docker-network
address), which the extension reads from the shared `lens_runtime` volume.
- `rag-rat.toml` — container-side config (root + database paths; both are bind mounts)
- `stage-artifacts.sh` — copies the CI-built binary + vsix into the build context
- `.github/workflows/deploy-appliance.yml` — CI deploy to the production host

## One command

```sh
cd editors/appliance
./stage-artifacts.sh            # or let CI do it
docker compose up -d --build
```

Local-only: reach the workbench with an SSH forward
(`ssh -N -L 8080:127.0.0.1:8080 ragrat@<host>`) and open <http://127.0.0.1:8080>.
Shared: attach the named Cloudflare tunnel (below) and open the tunnel hostname.

## What the browser sees, and how auth works

- code-server serves the workbench on container port 8080 **without a login gate**
  (`--auth none` — the demo is public and read-only). Containment is structural, not a
  password: NO DB MOUNT in the workbench container (the `serve` container owns it),
  read-only workspace, read-only extensions dir (no installs), a dead terminal shell
  (`/bin/false`), no docker socket, bounded CPU/memory/pids, ro rootfs, no-new-privs,
  cap_drop ALL, and an `internal: true` compose network so the workbench has **no
  outbound internet** — only the `cloudflared` sidecar has egress. Treat any future
  switch back to editable mode as a security review.
- The Lens backend (`rag-rat serve`) runs in its own `serve` container on the internal
  network, mints a bearer token per boot, and publishes it to
  `/srv/workspace/.rag-rat/sockets/lens.json` with `--advertise-url` — the file the
  extension's automatic discovery reads. No token handling in the UI; the workbench
  mounts that volume read-only. CORS allows exactly the public origin (the extension's
  fetch Origin when it runs in a browser worker).
- `.rag-rat` is a dedicated volume mounted over the read-only workspace so the discovery
  publish succeeds without making the repo writable; the workbench sees it read-only.

## Bind mounts (host-owned state)

| host path | container path | mode | owner |
|---|---|---|---|
| `/srv/rag-rat/repo` | `/srv/workspace` | **ro** | operator |
| `/srv/rag-rat/db` | `/srv/db` | **rw** | operator |

The workspace **is the rag-rat repo itself** (the appliance dogfoods rag-rat's own
index): `/srv/rag-rat/repo` is a clone of `cq27-dev/rag-rat`, and the container config's
`[target_bindings]` mirror the repo's own `rag-rat.toml` (`rust = ["crates"]`,
`markdown = ["docs"]`).

Host-side setup for that state:

```sh
git clone https://github.com/cq27-dev/rag-rat /srv/rag-rat/repo
cd /srv/rag-rat/repo && rag-rat index          # writes /srv/rag-rat/db/rag-rat.sqlite
```

The DB is **written manually on the host** (`rag-rat index` / `reconcile` against
`/srv/rag-rat/db/rag-rat.sqlite`; point the host's `rag-rat.toml` `database` there).
The whole DIRECTORY is mounted (not the bare file) because `rag-rat serve` needs sibling
writes — the locks dir and SQLite's `-wal`/`-shm` sidecars. Read-write deliberately: serve
migrates on open and self-heals, so a read-only mount breaks serving when a migration is
owed. Keep the host and container
rag-rat binaries on the **same release** — if the container is newer, its migration
makes the DB unreadable to the host binary.

### Retirement

The bind-mounted DB and the manual host-side indexing flow are **transitional**. Once
memory/op-log sync lands, the appliance consumes the synced store (or a sync peer), the
host write path goes away, and the compose drops both bind mounts. Remove this section
then.

## Cloudflare Tunnel (shared posture)

1. Create a **named tunnel** in the Zero Trust dashboard; copy its token.
2. Add a public hostname (e.g. `rag-rat-demo.cq27.dev`) → service `http://code-server:8080`.
   The hostname is a CNAME to the tunnel — no DNS record ever names the host IP.
3. Put `TUNNEL_TOKEN=…` (and `APPLIANCE_PASSWORD=…`) in `.env` next to
   `docker-compose.yml`, `chmod 600`.
4. Recommended: put **Cloudflare Access** in front of the hostname for SSO
   (Google/GitHub/OTP, per-email policy) instead of relying on the password alone.

The tunnel dials outbound only; the host needs no inbound ports beyond SSH. TLS is
terminated at Cloudflare's edge, which is what the workbench's service workers require —
do not expose port 8080 directly over plain HTTP; webviews will not load without a
secure context.

### Edge hardening (dashboard, one-time)

- **Bot Fight Mode** on (Security → Bots).
- **Rate limiting**: one free rule — path `/` ≥ ~100 req/10s per IP → Managed Challenge
  (don't go below ~50/10s; the workbench pulls many assets on load).
- **HSTS** on (SSL/TLS → Edge Certificates) with the nosniff toggle.
- **Response Header Transform Rule**: `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: strict-origin-when-cross-origin`, `X-Frame-Options: SAMEORIGIN`
  (the demo is never legitimately iframed).
- **Notifications**: DDoS alert, origin error rate, tunnel health.
- **Abuse lever**: if the demo gets hammered, put Cloudflare Access (One-Time PIN) in
  front of the hostname — quasi-public, adds identity and a kill switch.

## Production host (one-time setup)

```sh
useradd -m -s /bin/bash ragrat && usermod -aG docker ragrat          # CI deploys as this user; no sudo
ufw default deny incoming
ufw allow ssh
ufw enable
mkdir -p /opt/rag-rat-appliance /srv/rag-rat
```

GitHub `appliance-prod` environment secrets:

- `DEPLOY_HOST` — host IP/hostname (kept out of the repo and masked in logs)
- `DEPLOY_HOST_KEY` — `ssh-keyscan` output, pinned (prevents silent host swaps)
- `DEPLOY_SSH_KEY` — dedicated ed25519 deploy key in `ragrat`'s `authorized_keys`

## Lifecycle and cleanup

- Upgrade: push to `main` touching the appliance/extension/crates → CI rebuilds and
  restarts. The deploy also **pins the host clone to the deployed SHA** (`git checkout
  -qf <sha>`), installs the freshly built host binary, stops the workbench, and runs
  `rag-rat index` before restarting — the served graph is commit-scoped, so the clone,
  the DB, and the binary must move together. Roll back by re-running the workflow from
  the previous commit (the same pin+reindex restores that state).
- code-server user data is a tmpfs (ephemeral per boot); `.rag-rat` runtime is the
  `lens_runtime` volume. The `lens_ext` volume initializes from the image ONCE and then
  shadows newer extension bundles — the deploy drops it before `up` for exactly this
  reason; drop it manually when rebuilding by hand (`docker volume rm
  rag-rat-appliance_lens_ext`).
- The image embeds the pinned code-server/cloudflared bases — rebuilds pull new bases
  (`docker compose build --pull`).
- Local launcher flow (non-Docker, future #1106 follow-up): downloads land in
  `~/.local/lib/code-server-*`; delete that directory to reclaim them.
