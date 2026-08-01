# syntax=docker/dockerfile:1
# Lens web appliance (#1106): unmodified code-server (Code-OSS) + the published Lens
# extension vsix + the rag-rat binary + the Lens backend (`rag-rat serve`).
#
# The build context is editors/appliance. CI stages two artifacts before building:
#   artifacts/rag-rat           (`cargo build --profile dist -p rag-rat`)
#   artifacts/rag-rat-lens.vsix (`npm run package` in editors/vscode — the SAME
#                                artifact that is published to the marketplaces)
# For local builds, stage those two files by hand, or run ./stage-artifacts.sh.

# Pinned by digest: `latest` would let `docker compose build --pull` swap Code-OSS for
# an unreviewed upstream release on an unchanged repo revision — including changes to
# the public security surface. Bump deliberately; the tag stays for readability.
FROM codercom/code-server:4.131.0@sha256:3623e6362abdec6258472882b06fdeec9d6ce2ad3fda316b3c5d7ed092b89add

USER root
COPY artifacts/rag-rat /usr/local/bin/rag-rat
COPY --chown=coder:coder artifacts/rag-rat-lens.vsix /tmp/rag-rat-lens.vsix
COPY rag-rat.toml /srv/config/rag-rat.toml
# Overwrite code-server's bootstrapped default config: the extension install below
# already ran code-server once, which wrote a config.yaml containing a generated
# password — a credential no public visitor should be able to browse.
COPY --chown=coder:coder code-server.yaml /home/coder/.config/code-server/config.yaml
# VS Code user settings for the public read-only demo. code-server's user-data dir is
# ~/.local/share/code-server; compose ALSO bind-mounts this file read-only over that
# path so a public session cannot flip the terminal/task/debug profiles back to a real
# shell through the Settings UI. The image copy keeps the same defaults for runs that
# do not use the compose bind.
COPY --chown=coder:coder settings.json /home/coder/.local/share/code-server/User/settings.json
COPY entrypoint.sh /usr/local/bin/lens-appliance-entrypoint
COPY serve-entrypoint.sh /usr/local/bin/lens-serve-entrypoint
COPY loopback-proxy.js /usr/local/bin/lens-loopback-proxy.js
RUN chmod +x /usr/local/bin/rag-rat /usr/local/bin/lens-appliance-entrypoint \
    /usr/local/bin/lens-serve-entrypoint \
    && mkdir -p /opt/lens-ext/extensions \
    && chown -R coder:coder /opt/lens-ext

USER coder
# Install at image build so the extension in the container is byte-identical to the
# published vsix; no marketplace round-trip at runtime. The extensions dir is mounted
# READ-ONLY at runtime (public demo: no extension installs).
RUN code-server --install-extension /tmp/rag-rat-lens.vsix --extensions-dir /opt/lens-ext \
    && rm /tmp/rag-rat-lens.vsix
# The demo is STATELESS per boot: no user-data in the image. Runtime state lives on a
# size-bounded tmpfs (compose), recreated empty at every start — nothing a visitor does
# accumulates, and there is nothing to browse from previous sessions.
RUN rm -rf /home/coder/.local/share/code-server

ENTRYPOINT ["/usr/local/bin/lens-appliance-entrypoint"]
