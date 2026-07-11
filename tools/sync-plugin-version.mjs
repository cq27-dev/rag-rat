#!/usr/bin/env node
// Sync the plugin manifests' version to the workspace crate version.
//
// release-plz bumps only Cargo.toml ([workspace.package].version) — it has no support for non-Cargo
// files. The plugin launcher fetches releases/download/v<plugin-version>, so the manifests MUST match
// the released crate version or it downloads the wrong release. `.github/workflows/release-plz.yml`
// runs this on the Release PR branch so the manifest bump rides in the same PR that cuts the tag.
//
// Idempotent; run from the repo root. Exits non-zero only on a structural mismatch (so a cargo-dist /
// release-plz change that moves the version can't silently ship a stale manifest).
import { readFileSync, writeFileSync } from "node:fs";

function workspaceVersion() {
  const toml = readFileSync("Cargo.toml", "utf8");
  // The canonical version is [workspace.package].version — NOT the [workspace.dependencies] pins,
  // which also carry `version = "..."`. Scope the search to that one table.
  const start = toml.indexOf("[workspace.package]");
  if (start < 0) fail("[workspace.package] table not found in Cargo.toml");
  const rest = toml.slice(start + "[workspace.package]".length);
  const end = rest.search(/\n\[/); // next table header ends the block
  const block = end >= 0 ? rest.slice(0, end) : rest;
  const m = block.match(/^\s*version\s*=\s*"([^"]+)"/m);
  if (!m) fail("[workspace.package].version not found in Cargo.toml");
  return m[1];
}

function fail(msg) {
  console.error(`sync-plugin-version: ${msg}`);
  process.exit(1);
}

const VERSION = workspaceVersion();
// Each manifest has exactly one "version" field; rewrite it surgically to preserve formatting.
const FILES = [
  ".claude-plugin/marketplace.json",
  "plugin/.claude-plugin/plugin.json",
  "plugin/.codex-plugin/plugin.json",
];
const versionRe = /("version"\s*:\s*)"[^"]*"/;

let changed = 0;
for (const file of FILES) {
  const src = readFileSync(file, "utf8");
  if (!versionRe.test(src)) fail(`no "version" field in ${file}`);
  const out = src.replace(versionRe, `$1"${VERSION}"`);
  if (out !== src) {
    writeFileSync(file, out);
    console.error(`sync-plugin-version: ${file} → ${VERSION}`);
    changed++;
  }
}
console.error(
  changed
    ? `sync-plugin-version: updated ${changed} file(s) to ${VERSION}`
    : `sync-plugin-version: manifests already at ${VERSION}`,
);
