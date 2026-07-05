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

/** rag-rat's own skills — used to scope destructive/refresh subcommands to ONLY these. */
const RAG_RAT_SKILLS = ["using-rag-rat", "dream-review"];

/** "install" verbs — mapped to `skills add <SOURCE>`. Includes every upstream `add` alias. */
const INSTALL = new Set(["add", "install", "i", "a"]);

/** Maintenance verbs scoped to rag-rat's own skills when the user didn't name a skill themselves — a
 * bare `skills remove`/`update` acts over EVERY installed skill (a selector / update-all), so a
 * branded "remove them"/"update them" must never touch unrelated skills. Includes ALL upstream
 * aliases (verified against the `skills` CLI source: remove = rm|r, update = check|upgrade) — an
 * un-listed alias would slip through unscoped. */
const SCOPED = new Set(["remove", "rm", "r", "update", "check", "upgrade"]);

/** Flags that consume the NEXT token as their value (so that value is not mistaken for a positional
 * skill name). */
const VALUE_FLAGS = new Set(["-a", "--agent", "-s", "--skill"]);

/** Did the user already name a skill to act on — via `-s`/`--skill` OR a bare positional token? If
 * so, the scoped verbs must NOT append the default rag-rat skill set (that would widen the action to
 * skills the user didn't ask for). Walks args, skipping the value of value-taking flags so `-a
 * claude-code` is not read as the positional skill `claude-code`. */
function namesASkill(rest) {
  for (let i = 0; i < rest.length; i++) {
    const t = rest[i];
    if (t === "-s" || t === "--skill" || t.startsWith("-s=") || t.startsWith("--skill=")) {
      return true;
    }
    if (VALUE_FLAGS.has(t)) {
      i++; // consume this flag's value
      continue;
    }
    if (!t.startsWith("-")) {
      return true; // a bare positional = a skill name
    }
  }
  return false;
}

function usage() {
  process.stdout.write(
    `@rag-rat/skills — install rag-rat's agent skills (using-rag-rat, dream-review)\n\n` +
      `Usage:\n` +
      `  npx @rag-rat/skills [add|install]   install into your agent(s) (default)\n` +
      `  npx @rag-rat/skills update          refresh rag-rat's installed skills\n` +
      `  npx @rag-rat/skills list            list installed skills\n` +
      `  npx @rag-rat/skills remove          remove rag-rat's skills\n\n` +
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
  } else if (SCOPED.has(sub)) {
    // `remove`/`update` (+ aliases) with no explicit skill → scope to OUR skills, so the branded
    // command never removes/refreshes unrelated skills. If the user named a skill (via `-s`/`--skill`
    // OR a positional), respect their selection and add nothing.
    const rest = argv.slice(1);
    const scope = namesASkill(rest) ? [] : RAG_RAT_SKILLS.flatMap((n) => ["-s", n]);
    skillsArgs = [sub, ...rest, ...scope];
  } else {
    skillsArgs = argv;
  }

  const res = spawnSync("npx", ["-y", "skills", ...skillsArgs], {
    stdio: "inherit",
    // On Windows `npx` is a `.cmd` shim that spawn can't execute directly (ENOENT); run it through
    // the shell so cmd.exe resolves it. Our args carry no spaces, so no shell-quoting hazard. POSIX
    // needs no shell (and keeping it off there preserves exact arg passing).
    shell: process.platform === "win32",
  });
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
