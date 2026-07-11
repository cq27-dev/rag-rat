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
Runtime is statically linked (pyke ships an `aarch64-linux-android` prebuilt), so only bionic +
`libc++_shared` are needed at runtime, both present on Termux. Requires the jemalloc-off-android gate
(the allocator's `-lgcc` link fails on NDK r23+).
