// rag-rat opencode plugin: bundles the rag-rat MCP server + hook integrations.
//
// opencode's plugin shape differs from Claude Code / Codex (no hooks.json manifest, no
// additionalContext channel on tool.execute.before), so this module is a thin shim over the same
// shared pieces the other harnesses use:
//   - MCP: registered via the `config` hook as `npx -y @rag-rat/bin@0.19.0<version> mcp` (pinned; kept
//     in lockstep with the release by tools/sync-plugin-version.mjs).
//   - Hooks: routed through the shared launcher (`../scripts/launch.js --no-install agent-hook`)
//     when this file lives in the repo's plugin bundle, or — for a standalone single-file install
//     — through the same resolve-only policy inline ($RAG_RAT_BIN → managed cache → npx cache →
//     version-matched PATH). Either way a hook NEVER blocks on an install or a network fetch: it
//     is a fast no-op until the MCP server's first npx run has installed the binary.
//   - Payload translation: opencode tool names/args are lowercase/camelCase; the harness-neutral
//     `agent-hook` contract expects Claude-style `tool_name` / `tool_input`, so the shim maps
//     bash→Bash, grep→Grep, read→Read, write→Write, edit→Edit.
//   - Context injection: Claude's PreToolUse additionalContext becomes (a) appended text on the
//     tool result in `tool.execute.after` and (b) a system-prompt entry via
//     `experimental.chat.system.transform` for the SessionStart orientation digest.
//
// Every path fails silent: a missing binary, a slow launcher, or a bad payload must never break
// or delay an opencode tool call.

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
// Type-only import: erased at compile time, so a standalone install needs no node_modules.
import type { Plugin } from "@opencode-ai/plugin";

// realpath: when the plugin is symlinked into ~/.config/opencode/plugins/ from the repo bundle,
// the launcher must resolve relative to the REAL location, not the symlink's dir.
const PLUGIN_DIR = fs.realpathSync(path.dirname(fileURLToPath(import.meta.url)));
const BUNDLED_LAUNCHER = path.join(PLUGIN_DIR, "..", "scripts", "launch.js");

// Single source of truth for the version: the pinned npm package (kept in lockstep with the
// release by tools/sync-plugin-version.mjs — do not hand-edit the pin).
const MCP_PACKAGE = "@rag-rat/bin@0.19.0";
const VERSION = MCP_PACKAGE.split("@").pop()!;

const TOOL_TIMEOUT_MS = 10_000;
const SESSION_TIMEOUT_MS = 5_000;

// ---- binary resolution (standalone fallback — mirrors scripts/launch.js --no-install) ----------

const BIN = os.platform() === "win32" ? "rag-rat.exe" : "rag-rat";

function isExecutable(p: string): boolean {
  try {
    fs.accessSync(p, fs.constants.X_OK);
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
}

function readBinVersion(p: string): string {
  const r = spawnSync(p, ["--version"], { encoding: "utf8" });
  return r.status === 0
    ? (r.stdout.match(/rag-rat\s+([0-9][^\s]*)/) || [])[1] || ""
    : "";
}

function whichVersioned(): string | null {
  const exts =
    process.platform === "win32" ? (process.env.PATHEXT || ".EXE").split(";") : [""];
  for (const dir of (process.env.PATH || "").split(path.delimiter)) {
    for (const e of exts) {
      const full = path.join(dir, BIN.endsWith(".exe") ? BIN : BIN + e);
      if (isExecutable(full) && readBinVersion(full) === VERSION) return full;
    }
  }
  return null;
}

function npxCachedBin(): string | null {
  const npmCache =
    process.env.npm_config_cache ||
    (process.platform === "win32"
      ? path.join(
          process.env.LOCALAPPDATA || path.join(os.homedir(), "AppData", "Local"),
          "npm-cache",
        )
      : path.join(os.homedir(), ".npm"));
  let hashes: string[];
  try {
    hashes = fs.readdirSync(path.join(npmCache, "_npx"));
  } catch {
    return null;
  }
  for (const h of hashes) {
    const p = path.join(
      npmCache,
      "_npx",
      h,
      "node_modules",
      "@rag-rat",
      "bin",
      "node_modules",
      ".bin_real",
      BIN,
    );
    if (isExecutable(p) && readBinVersion(p) === VERSION) return p;
  }
  return null;
}

type HookCommand = { cmd: string; args: string[] };

// The command + args to invoke for one hook call, or null when nothing resolvable exists yet
// (the hook no-op path). Prefers the bundled launcher; falls back to direct binary resolution so
// a lone `rag-rat.ts` dropped into ~/.config/opencode/plugins/ still works.
function resolveHookCommand(): HookCommand | null {
  if (process.env.RAG_RAT_BIN) {
    return isExecutable(process.env.RAG_RAT_BIN)
      ? { cmd: process.env.RAG_RAT_BIN, args: ["agent-hook"] }
      : null;
  }
  if (fs.existsSync(BUNDLED_LAUNCHER)) {
    return { cmd: "node", args: [BUNDLED_LAUNCHER, "--no-install", "agent-hook"] };
  }
  const cacheHome = process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache");
  const managed = path.join(cacheHome, "rag-rat", "bin", VERSION, BIN);
  const bin = isExecutable(managed) ? managed : npxCachedBin() || whichVersioned();
  return bin ? { cmd: bin, args: ["agent-hook"] } : null;
}

// ---- hook invocation ----------------------------------------------------------------------------

// Run one hook payload; resolve stdout ("" on any failure or timeout — silence is the contract).
function runHook(payload: Record<string, unknown>, timeoutMs: number): Promise<string> {
  const resolved = resolveHookCommand();
  if (!resolved) return Promise.resolve("");
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(resolved.cmd, resolved.args, { stdio: ["pipe", "pipe", "ignore"] });
    } catch {
      return resolve("");
    }
    let out = "";
    let settled = false;
    const finish = (v: string) => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve(v);
      }
    };
    const timer = setTimeout(() => {
      try {
        child.kill();
      } catch {}
      finish("");
    }, timeoutMs);
    child.stdout.on("data", (d) => {
      out += d;
    });
    child.on("error", () => finish(""));
    child.on("close", () => finish(out));
    child.stdin.on("error", () => finish(out));
    child.stdin.end(JSON.stringify(payload));
  });
}

// PreToolUse events emit `{"hookSpecificOutput":{"additionalContext": ...}}` on one JSON line;
// tolerate any surrounding noise by scanning lines. SessionStart emits the digest as PLAIN
// stdout — that path uses the raw output, not this extractor.
function extractAdditionalContext(stdout: string): string | null {
  for (const line of stdout.split("\n")) {
    const t = line.trim();
    if (!t.startsWith("{")) continue;
    try {
      const c = JSON.parse(t)?.hookSpecificOutput?.additionalContext;
      if (typeof c === "string" && c.trim()) return c;
    } catch {}
  }
  return null;
}

// ---- opencode → harness-neutral payload translation ----------------------------------------------

type Translated = { tool_name: string; tool_input: Record<string, unknown> };

// Map an opencode tool call to the harness-neutral agent-hook payload, or null when the tool
// carries no searchable/editable intent the hook understands.
function translate(tool: string, args: any): Translated | null {
  switch (tool) {
    case "bash":
      if (typeof args?.command !== "string") return null;
      return { tool_name: "Bash", tool_input: { command: args.command } };
    case "grep":
      if (typeof args?.pattern !== "string") return null;
      return { tool_name: "Grep", tool_input: { pattern: args.pattern, path: args.path } };
    case "read":
      if (typeof args?.filePath !== "string") return null;
      return { tool_name: "Read", tool_input: { file_path: args.filePath } };
    case "write":
      if (typeof args?.filePath !== "string") return null;
      return {
        tool_name: "Write",
        tool_input: { file_path: args.filePath, content: args.content ?? "" },
      };
    case "edit":
      if (typeof args?.filePath !== "string") return null;
      return {
        tool_name: "Edit",
        tool_input: { file_path: args.filePath, new_string: args.newString ?? "" },
      };
    default:
      return null;
  }
}

function isEditTool(tool: string): boolean {
  return tool === "write" || tool === "edit";
}

function sessionIdOf(event: any): string {
  return event?.properties?.info?.id ?? event?.properties?.sessionID ?? "";
}

// ---- the plugin -----------------------------------------------------------------------------------

export const RagRat: Plugin = async ({ directory }) => {
  // sessionID → SessionStart orientation digest, consumed once by system.transform. Key "" holds
  // a digest whose session could not be identified (consumed by the first transform call).
  const digests = new Map<string, string>();

  const takeDigest = (sessionID: string): string | undefined => {
    const key = digests.has(sessionID) ? sessionID : "";
    const d = digests.get(key);
    if (d !== undefined) digests.delete(key);
    return d;
  };

  const sessionStart = async (source: string, sessionID: string) => {
    const out = await runHook(
      { hook_event_name: "SessionStart", source, session_id: sessionID, cwd: directory },
      SESSION_TIMEOUT_MS,
    );
    const digest = out.trim();
    if (digest) digests.set(sessionID, digest);
  };

  return {
    // Self-register the rag-rat MCP server unless the user already configured one.
    config: async (cfg) => {
      cfg.mcp ??= {};
      cfg.mcp["rag-rat"] ??= {
        type: "local",
        command: ["npx", "-y", MCP_PACKAGE, "mcp"],
        enabled: true,
      };
    },

    event: async ({ event }) => {
      if (event?.type === "session.created") {
        await sessionStart("startup", sessionIdOf(event));
      }
    },

    // Inject the orientation digest into the system prompt on the session's first LLM call.
    "experimental.chat.system.transform": async (input, output) => {
      const digest = takeDigest(input.sessionID ?? "");
      if (digest) output.system.push(digest);
    },

    // Compaction parity with Claude's SessionStart(compact): re-inject a fresh digest into the
    // compaction prompt so orientation survives context truncation.
    "experimental.session.compacting": async (input, output) => {
      const out = await runHook(
        {
          hook_event_name: "SessionStart",
          source: "compact",
          session_id: input.sessionID ?? "",
          cwd: directory,
        },
        SESSION_TIMEOUT_MS,
      );
      const digest = out.trim();
      if (digest) output.context.push(digest);
    },

    "tool.execute.after": async (input, output) => {
      const translated = translate(input.tool, input.args);
      if (!translated) return;
      const base = { session_id: input.sessionID ?? "", cwd: directory };
      // Edits: fire the PostToolUse scoped reindex first (detached server-side; no stdout), then
      // the write-time clone check, which reports via additionalContext.
      if (isEditTool(input.tool)) {
        await runHook(
          { ...base, hook_event_name: "PostToolUse", ...translated },
          TOOL_TIMEOUT_MS,
        );
      }
      const out = await runHook(
        { ...base, hook_event_name: "PreToolUse", ...translated },
        TOOL_TIMEOUT_MS,
      );
      const context = extractAdditionalContext(out);
      if (context) output.output += `\n\n${context}`;
    },
  };
};
