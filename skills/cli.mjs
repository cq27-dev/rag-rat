#!/usr/bin/env node
/**
 * @rag-rat/skills — one-command installer for rag-rat's agent skills.
 *
 *   npx @rag-rat/skills                 # install rag-rat's skills into your agent(s)
 *   npx @rag-rat/skills update          # refresh installed skills to the latest
 *   npx @rag-rat/skills list            # show installed skills
 *   npx @rag-rat/skills remove          # remove them
 *
 * It is a THIN wrapper over the vercel-labs `skills` CLI (https://github.com/vercel-labs/skills),
 * pinned to rag-rat's canonical skills directory. Rather than reinvent a multi-agent installer, the
 * default command delegates to `skills add <rag-rat's .agents/skills>`, which knows how to place a
 * SKILL.md into 70+ agents (Claude Code → .claude/skills, Codex → .codex/skills, Cursor, opencode,
 * …), symlink or copy, project or --global. Every flag you pass is forwarded verbatim.
 *
 * SOURCE is pinned to the `.agents/skills` PATH (not the bare repo) on purpose: rag-rat mirrors its
 * skills into .claude/ and .codex/ via symlinks for its own agents, and pointing `skills` at the
 * whole repo would rediscover those mirror copies. Pinning the one canonical directory installs each
 * skill exactly once.
 */

import { spawnSync } from "node:child_process";

/** rag-rat's canonical skill directory on GitHub (walked one level deep: <name>/SKILL.md). */
const SOURCE = "https://github.com/cq27-dev/rag-rat/tree/main/.agents/skills";

/** Subcommands that mean "install" — mapped to `skills add <SOURCE>`. */
const INSTALL = new Set(["add", "install", "i"]);

function usage() {
  process.stdout.write(
    `@rag-rat/skills — install rag-rat's agent skills (using-rag-rat, dream-review)\n\n` +
      `Usage:\n` +
      `  npx @rag-rat/skills [add|install]   install into your agent(s) (default)\n` +
      `  npx @rag-rat/skills update          refresh installed skills\n` +
      `  npx @rag-rat/skills list            list installed skills\n` +
      `  npx @rag-rat/skills remove          remove installed skills\n\n` +
      `Flags are forwarded to the underlying \`skills\` CLI, e.g.:\n` +
      `  -a, --agent <name>   target a specific agent (claude-code, codex, cursor, …)\n` +
      `  -s, --skill <name>   install one skill by name\n` +
      `  -g, --global         install to your home dir instead of the project\n` +
      `  --copy               copy files instead of symlinking\n` +
      `  -y, --yes            skip confirmation prompts\n\n` +
      `Source: ${SOURCE}\n`,
  );
}

function main() {
  const argv = process.argv.slice(2);
  const sub = argv[0];

  if (sub === "--help" || sub === "-h" || sub === "help") {
    usage();
    return 0;
  }

  // Dispatch. The INSTALL path is `skills add <SOURCE> [flags]`; it fires for:
  //   - no subcommand           (`npx @rag-rat/skills`)
  //   - an install alias        (`add` / `install` / `i`) — its own token is dropped
  //   - a LEADING FLAG          (`-a`, `-s`, `-g`, `--copy`, `-y`, …) — these are `skills add`
  //     options, so the documented `npx @rag-rat/skills -a claude-code` forms must install, not
  //     forward a bare `skills -a …` (which the upstream CLI rejects).
  // Only a real subcommand word (update / list / ls / remove / rm / find / use / init) is forwarded
  // verbatim — those operate on the already-installed set, so no source is needed.
  const isInstall = sub === undefined || INSTALL.has(sub) || sub.startsWith("-");
  let skillsArgs;
  if (isInstall) {
    const rest = sub !== undefined && INSTALL.has(sub) ? argv.slice(1) : argv;
    skillsArgs = ["add", SOURCE, ...rest];
  } else {
    skillsArgs = argv;
  }

  const res = spawnSync("npx", ["-y", "skills", ...skillsArgs], { stdio: "inherit" });
  if (res.error) {
    process.stderr.write(
      `failed to run the \`skills\` CLI via npx: ${res.error.message}\n` +
        `install it directly if needed: npm i -g skills\n`,
    );
    return 1;
  }
  return res.status ?? 1;
}

process.exit(main());
