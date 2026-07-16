# dist-android

Android/Termux release machinery that cargo-dist can't provide itself.

cargo-dist does not treat android as a supported platform: `dist plan --target aarch64-linux-android`
skips every installer, so the generated release workflow neither cross-compiles the android binary
nor teaches the `@rag-rat/bin` npm installer about it. `.github/workflows/release-android.yml` fills
both gaps on the same release cargo-dist publishes, using the scripts here:

- **`patch-npm-package.mjs`** — run against cargo-dist's own `@rag-rat/bin` package (downloaded from
  the release as `rag-rat-npm-package.tar.gz`). Adds an `aarch64-linux-android` entry to
  `supportedPlatforms` and a `process.platform === "android"` short-circuit to `binary.js`
  `getPlatform()` (on Termux `os.type()` reports `"Linux"`, so the stock glibc/musl detection would
  mis-resolve). The `getPlatform()` anchor is whitespace-tolerant but still fails loudly if the
  function is gone, so a real cargo-dist template change forces a re-review instead of shipping broken.
- **`test-npm-package.cjs`** — run after patching (with deps installed): asserts Termux/android
  resolves to the android tarball and a desktop platform still resolves to its own artifact.

cargo-dist's own npm publisher is disabled (`publish-jobs = []` in `dist-workspace.toml`) so
release-android.yml is the sole publisher of `@rag-rat/bin` — otherwise it would republish the
android-less package under the same version. The android binary is the full build: FastEmbed's ONNX
Runtime is statically linked (pyke ships an `aarch64-linux-android` prebuilt), and libc++ is
statically linked too (`CXXSTDLIB=c++_static`, honored by both the `cc` crate and `ort-sys`, plus
an explicit `-lc++abi` — the NDK's `libc++_static.a` doesn't pull it in by itself), so bionic is
the only runtime dependency. Termux does ship `libc++_shared.so`, but only in `$PREFIX/lib`,
which the bionic loader searches for Termux-built ELFs (via their DT_RUNPATH) — never for a foreign
NDK-built binary, so dynamic libc++ fails to load there (#708); the workflow greps the built binary's
dynamic section and fails the release if `libc++_shared` sneaks back in. Requires the
jemalloc-off-android gate (the allocator's `-lgcc` link fails on NDK r23+).
