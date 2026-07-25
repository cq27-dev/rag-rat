#!/usr/bin/env node
// Validates the android-patched @rag-rat/bin package: run AFTER patch-npm-package.mjs against the
// same package dir, with its deps installed (binary.js requires detect-libc). Asserts that
//   - Termux/Android (process.platform="android", os.arch="arm64") resolves to the android tarball, and
//   - a desktop platform (Darwin/arm64) still resolves to its own artifact (no regression).
// Exits non-zero on mismatch so the release workflow refuses to publish a broken installer.

const os = require("os");
const path = require("path");
const assert = require("assert");

const pkgDir = path.resolve(process.argv[2] || "target/distrib/rag-rat-npm-package");
const binaryPath = path.join(pkgDir, "binary.js");
const pkgJsonPath = path.join(pkgDir, "package.json");
const pkg = require(pkgJsonPath);

assert.strictEqual(
  pkg.mcpName,
  "io.github.cq27-dev/rag-rat",
  "npm package must carry the MCP Registry ownership marker",
);
assert.strictEqual(
  pkg.homepage,
  "https://rag-rat.cq27.dev/",
  "npm package should direct users to the public homepage",
);

// Load a fresh binary.js under a faked os.type()/os.arch()/process.platform, and return the
// download URL getPackage() would fetch. Cache-busts so each case re-evaluates getPlatform().
function urlUnder({ platform, type, arch }) {
  const origType = os.type;
  const origArch = os.arch;
  const origPlatform = process.platform;
  os.type = () => type;
  os.arch = () => arch;
  Object.defineProperty(process, "platform", { value: platform, configurable: true });
  try {
    delete require.cache[binaryPath];
    delete require.cache[pkgJsonPath];
    const binary = require(binaryPath);
    return binary.getPackage().url;
  } finally {
    os.type = origType;
    os.arch = origArch;
    Object.defineProperty(process, "platform", { value: origPlatform, configurable: true });
  }
}

const androidUrl = urlUnder({ platform: "android", type: "Linux", arch: "arm64" });
assert.ok(
  androidUrl.endsWith("/rag-rat-aarch64-linux-android.tar.gz"),
  `Termux/android should resolve to the android tarball, got: ${androidUrl}`,
);

// Desktop regression guard: Darwin/arm64 takes no libc branch, so it needs no detect-libc mocking.
const darwinUrl = urlUnder({ platform: "darwin", type: "Darwin", arch: "arm64" });
assert.ok(
  darwinUrl.endsWith("/rag-rat-aarch64-apple-darwin.tar.xz"),
  `desktop (darwin/arm64) should still resolve to apple-darwin, got: ${darwinUrl}`,
);

console.error("test-npm-package: registry metadata + android + desktop resolution OK");
console.error(`  android: ${androidUrl}`);
console.error(`  darwin:  ${darwinUrl}`);
