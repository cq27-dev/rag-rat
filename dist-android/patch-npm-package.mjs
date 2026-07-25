#!/usr/bin/env node
// Inject aarch64-linux-android support into the cargo-dist-generated @rag-rat/bin npm package.
//
// cargo-dist does not treat android as a supported platform: `dist plan --target
// aarch64-linux-android` skips every installer ("not building any supported platforms"). So the
// generated npm installer omits android from its supportedPlatforms map and has no android branch
// in getPlatform(). Termux/Android needs the bionic aarch64-linux-android binary, which we build and
// attach to the GitHub release separately (release-android.yml).
//
// This runs on the freshly generated package BEFORE publish and:
//   1. adds the MCP Registry ownership marker and public homepage to package.json,
//   2. adds an `aarch64-linux-android` entry to package.json `supportedPlatforms`, and
//   3. inserts a `process.platform === "android"` short-circuit at the top of binary.js
//      getPlatform() — os.type() reports "Linux" on Termux, so the stock glibc/musl detection would
//      mis-resolve to a triple that either errors or downloads a non-bionic binary.
//
// It FAILS LOUDLY if the expected structure is absent: a cargo-dist upgrade that changes the npm
// template must be re-reviewed, never silently shipped as a broken package.

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const ANDROID_TRIPLE = "aarch64-linux-android";
const MCP_NAME = "io.github.cq27-dev/rag-rat";
const HOMEPAGE = "https://rag-rat.cq27.dev/";

// `.tar.gz`, not cargo-dist's default `.tar.xz`: Termux always ships gzip, but `xz-utils` is an
// extra package the installer's `tar xf` would otherwise silently need.
const ANDROID_ENTRY = {
  artifactName: `rag-rat-${ANDROID_TRIPLE}.tar.gz`,
  bins: { "rag-rat": "rag-rat" },
  zipExt: ".tar.gz",
};

// Injected verbatim right after `const getPlatform = () => {`. Uses only identifiers already in
// binary.js scope: os, supportedPlatforms, error, name.
const ANDROID_BRANCH = `  // Termux/Android reports os.type() === "Linux" but process.platform === "android", and needs the
  // bionic aarch64-linux-android build (not a glibc/musl one). Resolve it before the Linux/libc path.
  if (process.platform === "android") {
    let androidArch = "";
    switch (os.arch()) {
      case "arm64":
        androidArch = "aarch64";
        break;
      case "x64":
        androidArch = "x86_64";
        break;
    }
    const androidPlatform = supportedPlatforms[\`\${androidArch}-linux-android\`];
    if (!androidPlatform) {
      error(
        \`Platform with type "android" and architecture "\${os.arch()}" is not supported by \${name}.\`,
      );
    }
    return androidPlatform;
  }
`;

function fail(msg) {
  console.error(`patch-npm-package: ${msg}`);
  process.exit(1);
}

const pkgDir = process.argv[2] || "target/distrib/rag-rat-npm-package";

// 1. package.json supportedPlatforms
const pkgPath = join(pkgDir, "package.json");
const pkg = JSON.parse(readFileSync(pkgPath, "utf8"));
if (!pkg.supportedPlatforms || typeof pkg.supportedPlatforms !== "object") {
  fail(`${pkgPath}: no supportedPlatforms object — cargo-dist npm template changed, re-review`);
}
if (pkg.supportedPlatforms[ANDROID_TRIPLE]) {
  fail(`${pkgPath}: ${ANDROID_TRIPLE} already present — double patch?`);
}
pkg.mcpName = MCP_NAME;
pkg.homepage = HOMEPAGE;
pkg.supportedPlatforms[ANDROID_TRIPLE] = ANDROID_ENTRY;
writeFileSync(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`);

// 2. binary.js getPlatform() short-circuit. Match the function's opening brace tolerantly — the
// whitespace/newline after `{` can shift across cargo-dist template revisions — and inject the branch
// as its first statement. Still fails loudly if getPlatform() is gone entirely (a real template change).
const binPath = join(pkgDir, "binary.js");
let bin = readFileSync(binPath, "utf8");
const anchorRe = /const getPlatform = \(\) => \{[ \t]*\r?\n?/;
if (!anchorRe.test(bin)) {
  fail(`${binPath}: getPlatform() opening not found — cargo-dist npm template changed, re-review`);
}
if (bin.includes('process.platform === "android"')) {
  fail(`${binPath}: android branch already present — double patch?`);
}
bin = bin.replace(anchorRe, (opening) => opening + ANDROID_BRANCH);
writeFileSync(binPath, bin);

console.error(`patch-npm-package: injected ${ANDROID_TRIPLE} into ${pkgDir}`);
