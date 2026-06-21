# Pinned environment for the heavyweight benchmarks (#71/#79): the kernel index bench
# (bench-kernel.sh) and the C edge-resolution oracle (kernel-c-oracle.sh). Packaging these makes
# them reproducible across machines and — critically for the oracle — pins the SCIP indexer
# versions, which are the content-addressed `tool_version` baked into every verdict's identity.
#
# The built image (by tag/digest) IS the reproducibility unit: rebuild deliberately to bump a
# pinned tool, and the Bencher testbed should be re-baselined when the image changes.
#
# NOT used by the lightweight iai-callgrind push/PR bench — that one is deterministic by design and
# runs on the GitHub-hosted runner. valgrind + the iai runner are included anyway so this image can
# reproduce that bench too if ever needed.
#
# Base must be trixie, not bookworm: the pinned scip-clang prebuilt links against GLIBC_2.38, which
# bookworm's glibc 2.36 can't satisfy ("version `GLIBC_2.38' not found"). Trixie (Debian 13) ships
# glibc 2.41, which covers scip-clang and the rust-analyzer prebuilt. Same Debian tooling + package
# names; only the libc floor moves.
FROM debian:trixie-slim

ARG RUST_VERSION=1.96.0
ARG SCIP_CLANG_VERSION=v0.4.0
ARG SCIP_TYPESCRIPT_VERSION=0.4.0
ARG IAI_CALLGRIND_VERSION=0.16.1
# rust-analyzer: the 'latest' release asset at image-build time; the image tag pins it thereafter.
# Its version is recorded as the oracle tool_version, so a change is visible in the verdicts.
ARG RUST_ANALYZER_URL=https://github.com/rust-lang/rust-analyzer/releases/latest/download/rust-analyzer-x86_64-unknown-linux-gnu.gz

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        # rag-rat build + general (rusqlite bundles SQLite via cc; build-essential covers the C compiler)
        build-essential pkg-config git python3 curl ca-certificates xz-utils gzip \
        # Linux kernel build (for compile_commands.json: the bc-class deps that bit us)
        bc flex bison libelf-dev libssl-dev \
        # scip-clang / rust-analyzer prebuilt-binary runtime libs
        zlib1g libtinfo6 \
        # node toolchain for scip-typescript
        nodejs npm \
        # iai-callgrind bench (optional; lets this image reproduce the push/PR bench too)
        valgrind \
    && rm -rf /var/lib/apt/lists/*

# Rust toolchain, pinned (the bench binaries build with stable; CI fmt's nightly is separate).
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain "${RUST_VERSION}" --profile minimal \
    && cargo install iai-callgrind-runner --version "${IAI_CALLGRIND_VERSION}"

# scip-clang (C/C++ SCIP backend, #71), pinned by release tag.
RUN curl --proto '=https' --tlsv1.2 -sSfL \
      "https://github.com/sourcegraph/scip-clang/releases/download/${SCIP_CLANG_VERSION}/scip-clang-x86_64-linux" \
      -o /usr/local/bin/scip-clang \
    && chmod +x /usr/local/bin/scip-clang \
    && scip-clang --version

# rust-analyzer (Rust SCIP backend) — needs the `scip` subcommand the manifest probe checks.
RUN curl --proto '=https' --tlsv1.2 -sSfL "${RUST_ANALYZER_URL}" | gunzip > /usr/local/bin/rust-analyzer \
    && chmod +x /usr/local/bin/rust-analyzer \
    && rust-analyzer --version

# scip-typescript (TS/TSX SCIP backend), pinned.
RUN npm install -g "@sourcegraph/scip-typescript@${SCIP_TYPESCRIPT_VERSION}" \
    && scip-typescript --version

# Sanity: every tool the heavy benches invoke is on PATH (portable sh — Docker RUN uses /bin/sh).
RUN echo '1+1' | bc >/dev/null && for t in cargo scip-clang rust-analyzer scip-typescript valgrind python3 git; do \
      command -v "$t" >/dev/null || { echo "missing $t" >&2; exit 1; }; done

WORKDIR /repo
