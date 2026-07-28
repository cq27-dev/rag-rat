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
 * pinned to rag-rat's public plugin skill bundle. Rather than reinvent a multi-agent installer, the
 * default command delegates to `skills add <rag-rat's plugin/skills>`, which knows how to place a
 * SKILL.md into 70+ agents (Claude Code → .claude/skills, Codex → .codex/skills, Cursor, opencode,
 * …), symlink or copy, project or --global. Every flag you pass is forwarded verbatim.
 *
 * SOURCE is pinned to the curated `plugin/skills` PATH (not `.agents/skills`) on purpose:
 * `.agents/skills` also contains contributor-only repo guidance that must not ship through this
 * public installer. The plugin bundle is the public projection and installs each public skill once.
 */

// cross-spawn, not node:child_process — on Windows `npx` is a `.cmd` shim that Node can only run
// through a shell, and forwarding user tokens (`find` queries, `--skill` values) through cmd.exe
// would let shell metacharacters like `&`/`|` execute. cross-spawn resolves the shim and escapes
// args itself, so we never enable `shell` and untrusted args pass through verbatim.
import spawn from "cross-spawn";

/** rag-rat's public skill bundle on GitHub (walked one level deep: <name>/SKILL.md). */
const SOURCE = "https://github.com/cq27-dev/rag-rat/tree/main/plugin/skills";

/** rag-rat's own skills — used to scope destructive/refresh subcommands to ONLY these. */
const RAG_RAT_SKILLS = ["using-rag-rat", "dream-review", "init-rag-rat", "configure-rag-rat-dream"];

/** "install" verbs — mapped to `skills add <SOURCE>`. Includes every upstream `add` alias. */
const INSTALL = new Set(["add", "install", "i", "a"]);

/** Maintenance verbs (remove/update) whose DEFAULT is scoped to rag-rat's own skills — a bare
 * `skills remove`/`update` acts over EVERY installed skill (a selector / update-all), so a branded
 * "remove them"/"update them" must not. Includes ALL upstream aliases (verified against the `skills`
 * CLI source: remove = rm|r, update = check|upgrade) — an un-listed alias would slip through
 * unscoped. */
const SCOPED = new Set(["remove", "rm", "r", "update", "check", "upgrade"]);

/** The ONLY args we combine with an injected default rag-rat skill list: pure scope/confirm toggles
 * that carry no skill/agent SELECTION and take no value. Enumerated from upstream's option parsers
 * (`parseRemoveOptions` / `parseUpdateOptions`): scope = -g/--global, -p/--project; confirm =
 * -y/--yes. We deliberately do NOT reimplement upstream's arg grammar (variadic `--agent`, `--all`
 * precedence, `--skill=` forms). We inject our skills as explicit positional targets only when EVERY
 * arg is one of these known-safe flags (or there are none). Anything else — an agent filter, `--all`,
 * `--skill`, a positional name, or any unknown flag — means the user is driving `skills` directly, so
 * we forward verbatim and never risk injecting a wrong scope. */
const SAFE_MAINT_FLAGS = new Set(["-g", "--global", "-p", "--project", "-y", "--yes"]);

function usage() {
  process.stdout.write(
    `@rag-rat/skills — install rag-rat's public agent skills\n\n` +
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
    // Inject rag-rat's skills as explicit POSITIONAL targets (upstream's `remove|update [skills…]`
    // form) ONLY when every arg is a known-safe scope/confirm flag — then the action is provably
    // limited to our skills. Otherwise the user passed a selection/widening arg we won't second-guess
    // (agent filter, --all, a skill name, …), so forward verbatim.
    const rest = argv.slice(1);
    const scopable = rest.every((t) => SAFE_MAINT_FLAGS.has(t));
    skillsArgs = scopable ? [sub, ...rest, ...RAG_RAT_SKILLS] : [sub, ...rest];
  } else {
    skillsArgs = argv;
  }

  const res = spawn.sync("npx", ["-y", "skills", ...skillsArgs], { stdio: "inherit" });
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
