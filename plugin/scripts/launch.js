#!/usr/bin/env node
// rag-rat launcher — ensures a version-matched `rag-rat` binary is available, then runs it with the
// forwarded args (the plugin passes `mcp`, or `agent-hook` for hook events) wired straight to the
// agent's stdio.
//
// Why Node (not a .sh): the plugin must work on every OS the coding agents run on, including native
// Windows where there is no bash. Node is present wherever Claude Code / Codex / Cursor run, and one
// .js file behaves identically on Windows, macOS, and Linux. Running a *local* script with node has
// none of the `npx <pkg>` failure modes (npx stages into a shared _npx/<hash> dir, so concurrent
// launches race and fail with ENOENT; it also re-resolves over the network every launch). The npm
// package @rag-rat/bin is still fine as an *install* channel — just not as the per-launch runtime.
//
// Why this exists at all: the plugin ships manifests + this script, NOT a compiled binary. Installing
// the plugin should be one step and must not require a separate `cargo install` / PATH setup. On
// first run this resolves a version-matched prebuilt (the cargo-dist release assets), verifies it
// against the release checksum, installs it into a stable per-user cache, and runs it. Every run
// after is a direct spawn of the cached binary — no network, no per-launch cost.
//
// A leading `--no-install` (passed by hook invocations) resolves from an existing binary only and
// NEVER blocks on a download — a hook must be fast and must not blow its timeout. The MCP server's
// own launch is what triggers the one-time install; until then the hook is a harmless no-op.
//
// Resolution order (first hit wins):
//   1. $RAG_RAT_BIN     — explicit override (a local dev build); used unconditionally.
//   2. managed cache    — <cache>/rag-rat/bin/<version>/rag-rat[.exe]  (version-exact by construction)
//   3. plugin bin/      — a pre-seeded binary shipped next to the plugin
//   4. PATH rag-rat     — only if its --version matches the plugin's declared version
//   5. download+verify  — fetch the platform archive from the GitHub release, checksum, cache it
//                         (skipped under --no-install)
//
// CRITICAL: stdout is the MCP stdio protocol channel. Every diagnostic here goes to stderr.

"use strict";
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const https = require("node:https");
const crypto = require("node:crypto");
const { spawn, spawnSync } = require("node:child_process");

const GH_REPO = "cq27-dev/rag-rat";

// A leading `--no-install` (hook invocations) means: resolve from an existing binary only.
let _args = process.argv.slice(2);
const NO_INSTALL = _args[0] === "--no-install";
if (NO_INSTALL) _args = _args.slice(1);
const FORWARD_ARGS = _args; // e.g. ["mcp"] or ["claude-hook"]

const log = (m) => process.stderr.write(`rag-rat-launch: ${m}\n`);
const die = (m) => {
  process.stderr.write(`rag-rat-launch: error: ${m}\n`);
  process.exit(1);
};

// Run the resolved binary with stdio wired to the agent, forward signals, propagate exit code.
function run(bin) {
  const child = spawn(bin, FORWARD_ARGS, { stdio: "inherit" });
  const signals = ["SIGINT", "SIGTERM", "SIGHUP"];
  const forward = {};
  for (const sig of signals) {
    forward[sig] = () => child.kill(sig);
    process.on(sig, forward[sig]);
  }
  child.on("error", (e) => die(`failed to launch ${bin}: ${e.message}`));
  child.on("exit", (code, signal) => {
    // Drop our forwarders before re-raising, or the self-signal would hit our own handler (which
    // re-kills the already-dead child) instead of terminating this process — a shutdown hang.
    for (const sig of signals) process.off(sig, forward[sig]);
    if (signal) process.kill(process.pid, signal);
    else process.exit(code ?? 0);
  });
}

// ---- plugin root (each harness injects its own var; else derive from this file's location) --------
const pluginRoot =
  process.env.CLAUDE_PLUGIN_ROOT ||
  process.env.CODEX_PLUGIN_ROOT ||
  process.env.PLUGIN_ROOT ||
  path.resolve(__dirname, "..");

// ---- 0) explicit dev override --------------------------------------------------------------------
if (process.env.RAG_RAT_BIN) {
  const b = process.env.RAG_RAT_BIN;
  if (!fs.existsSync(b)) die(`RAG_RAT_BIN=${b} does not exist`);
  log(`using RAG_RAT_BIN override: ${b}`);
  run(b);
  return;
}

// ---- version = the plugin's declared version (single source of truth) -----------------------------
function readVersion() {
  for (const c of [path.join(pluginRoot, ".claude-plugin", "plugin.json"), path.join(pluginRoot, "plugin.json")]) {
    if (fs.existsSync(c)) {
      try {
        const v = JSON.parse(fs.readFileSync(c, "utf8")).version;
        if (v) return String(v);
      } catch (e) {
        die(`could not parse ${c}: ${e.message}`);
      }
    }
  }
  die(`plugin manifest with a "version" not found under ${pluginRoot}`);
}
const VERSION = readVersion();

// ---- platform → cargo-dist target triple ---------------------------------------------------------
function detectTriple() {
  const platform = os.platform(); // 'linux' | 'darwin' | 'win32' | ...
  let arch = os.arch(); // 'x64' | 'arm64' | ...

  if (platform === "darwin" && arch === "x64") {
    // An x64 Node under Rosetta reports x64 on Apple Silicon hardware. Probe the hardware so we
    // fetch the arm64 build rather than erroring out as if it were a real Intel Mac.
    const sysctl = (name) => {
      const r = spawnSync("sysctl", ["-n", name], { encoding: "utf8" });
      return r.status === 0 ? r.stdout.trim() : "";
    };
    if (sysctl("sysctl.proc_translated") === "1" || sysctl("hw.optional.arm64") === "1") arch = "arm64";
  }

  const key = `${platform}-${arch}`;
  switch (key) {
    case "linux-x64": return { triple: "x86_64-unknown-linux-gnu", ext: "tar.xz", bin: "rag-rat" };
    case "linux-arm64": return { triple: "aarch64-unknown-linux-gnu", ext: "tar.xz", bin: "rag-rat" };
    case "darwin-arm64": return { triple: "aarch64-apple-darwin", ext: "tar.xz", bin: "rag-rat" };
    case "win32-x64": return { triple: "x86_64-pc-windows-msvc", ext: "zip", bin: "rag-rat.exe" };
    case "darwin-x64":
      die(
        "Intel Macs (x86_64-apple-darwin) have no prebuilt — `ort` ships no ONNX Runtime for it.\n" +
        "       Install from source with the pure-Rust embedder and point the plugin at it:\n" +
        "           cargo install rag-rat --no-default-features --features model2vec\n" +
        "           export RAG_RAT_BIN=\"$(command -v rag-rat)\""
      );
      break;
    default:
      die(`unsupported platform ${key}. Build from source: cargo install rag-rat`);
  }
}
const { triple, ext, bin } = detectTriple();

// ---- 1) managed cache (version-exact) ------------------------------------------------------------
const cacheHome = process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache");
const cacheDir = path.join(cacheHome, "rag-rat", "bin", VERSION);
const managedBin = path.join(cacheDir, bin);
if (isExecutable(managedBin)) return run(managedBin);

// ---- 2) plugin-local pre-seeded binary -----------------------------------------------------------
const seeded = path.join(pluginRoot, "bin", bin);
if (isExecutable(seeded)) return run(seeded);

// ---- 3) PATH rag-rat, only if it matches the declared version ------------------------------------
const onPath = which(bin);
if (onPath) {
  const r = spawnSync(onPath, ["--version"], { encoding: "utf8" });
  const v = r.status === 0 ? (r.stdout.match(/rag-rat\s+([0-9][^\s]*)/) || [])[1] : "";
  if (v === VERSION) return run(onPath);
  if (v) log(`PATH rag-rat is ${v}, plugin wants ${VERSION} — fetching the matched build`);
}

// ---- 4) download + verify + cache (skipped for hooks via --no-install) ---------------------------
if (NO_INSTALL) {
  // Hook fast path: no cached binary yet. Do nothing rather than block on a download — the MCP
  // server's launch installs it; the hook is a harmless no-op until then.
  log("no cached binary yet — skipping (rag-rat installs on MCP server start)");
  process.exit(0);
}
downloadAndRun().catch((e) => die(e.message));

async function downloadAndRun() {
  fs.mkdirSync(cacheDir, { recursive: true });

  // Serialize concurrent launches: only one downloads per version; the rest wait, then hit the cache.
  const lock = path.join(cacheDir, ".download.lock");
  const release = await acquireLock(lock);
  try {
    if (isExecutable(managedBin)) return run(managedBin); // another launch finished while we waited

    const archive = `rag-rat-${triple}.${ext}`;
    const base = `https://github.com/${GH_REPO}/releases/download/v${VERSION}`;
    // Stage under cacheDir (same filesystem as managedBin) so the final install rename is atomic;
    // a temp dir on a different fs (e.g. tmpfs /tmp) would make fs.renameSync raise EXDEV.
    const tmp = fs.mkdtempSync(path.join(cacheDir, ".dl-"));
    try {
      const archivePath = path.join(tmp, archive);
      log(`downloading rag-rat v${VERSION} for ${triple} …`);
      await fetchToFile(`${base}/${archive}`, archivePath);
      const wantSum = (await fetchText(`${base}/${archive}.sha256`)).trim().split(/\s+/)[0];
      const gotSum = sha256File(archivePath);
      if (!wantSum) throw new Error(`empty checksum for ${archive}`);
      if (wantSum !== gotSum) throw new Error(`checksum mismatch for ${archive} (want ${wantSum}, got ${gotSum})`);

      // bsdtar (Windows 10+, macOS, Linux) extracts both .tar.xz and .zip; GNU tar auto-detects xz.
      const tar = spawnSync("tar", ["-xf", archivePath, "-C", tmp], { stdio: ["ignore", "ignore", "inherit"] });
      if (tar.status !== 0) throw new Error(`failed to extract ${archive} (need a tar that reads ${ext})`);

      const found = findFile(tmp, bin);
      if (!found) throw new Error(`binary ${bin} not found inside ${archive}`);
      fs.chmodSync(found, 0o755);
      fs.renameSync(found, managedBin); // same-filesystem rename → atomic install
      log(`installed rag-rat v${VERSION} → ${managedBin}`);
    } finally {
      fs.rmSync(tmp, { recursive: true, force: true });
    }
  } finally {
    release();
  }
  run(managedBin);
}

// ---- helpers -------------------------------------------------------------------------------------
function isExecutable(p) {
  try {
    fs.accessSync(p, fs.constants.X_OK);
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
}
function which(name) {
  const exts = process.platform === "win32" ? (process.env.PATHEXT || ".EXE").split(";") : [""];
  for (const dir of (process.env.PATH || "").split(path.delimiter)) {
    for (const e of exts) {
      const full = path.join(dir, name.endsWith(".exe") ? name : name + e);
      if (isExecutable(full)) return full;
    }
  }
  return null;
}
function findFile(root, name) {
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) {
      const hit = findFile(full, name);
      if (hit) return hit;
    } else if (entry.name === name) {
      return full;
    }
  }
  return null;
}
function sha256File(p) {
  return crypto.createHash("sha256").update(fs.readFileSync(p)).digest("hex");
}
function httpsGet(url) {
  // Resolve to a response, following GitHub's redirect to its asset CDN.
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "user-agent": "rag-rat-launch" } }, (res) => {
        if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location) {
          res.resume();
          resolve(httpsGet(res.headers.location));
        } else if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`GET ${url} → HTTP ${res.statusCode}`));
        } else {
          resolve(res);
        }
      })
      .on("error", reject);
  });
}
async function fetchToFile(url, dest) {
  const res = await httpsGet(url);
  await new Promise((resolve, reject) => {
    const out = fs.createWriteStream(dest);
    res.pipe(out);
    out.on("finish", resolve);
    out.on("error", reject);
    res.on("error", reject);
  });
}
async function fetchText(url) {
  const res = await httpsGet(url);
  let body = "";
  res.setEncoding("utf8");
  for await (const chunk of res) body += chunk;
  return body;
}
async function acquireLock(lockPath) {
  // Atomic create-if-absent. When the lock is held, only break it if the LOCK FILE itself is stale
  // (a crashed holder), judged by its age — NOT by how long *we* have waited. A slow-but-live
  // download must not have its lock yanked: that would start a second download and race the final
  // rename (on Windows the rename onto an existing file can fail outright).
  const STALE_MS = 15 * 60_000; // no real download approaches this; an older lock ⇒ the holder died
  for (;;) {
    try {
      const fd = fs.openSync(lockPath, "wx");
      fs.closeSync(fd);
      return () => fs.rmSync(lockPath, { force: true });
    } catch (e) {
      if (e.code !== "EEXIST") throw e;
      let age;
      try {
        age = Date.now() - fs.statSync(lockPath).mtimeMs;
      } catch {
        continue; // holder released between the EEXIST and the stat — retry the create now
      }
      if (age > STALE_MS) {
        fs.rmSync(lockPath, { force: true }); // crashed holder — reclaim
        continue;
      }
      await new Promise((r) => setTimeout(r, 200));
    }
  }
}
