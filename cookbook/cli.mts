#!/usr/bin/env node
/**
 * @rag-rat/cookbook bin dispatcher — the published entrypoint.
 *
 *   npx @rag-rat/cookbook <provider>      e.g.  npx @rag-rat/cookbook modal
 *                                               npx @rag-rat/cookbook runpod
 *
 * It reads the provider from argv[2] and dynamically imports the matching recipe. Recipes
 * self-run on import (each ends with `await runRecipe(...)`), so importing one IS running it —
 * the dispatcher adds no logic to the contract beyond routing. The recipe files stay directly
 * runnable too (`node dist/recipes/<provider>-ollama.mjs`); the dispatcher is just sugar.
 *
 * Unknown or missing provider → error to stderr listing the providers + exit 1, no handshake.
 * (stdout stays reserved for the single handshake line the chosen recipe emits.)
 */

import { log } from "./src/contract.js";

/** provider keyword → recipe module specifier (relative to this file's location in dist/). */
const PROVIDERS: Readonly<Record<string, string>> = {
  modal: "./recipes/modal-ollama.mjs",
  runpod: "./recipes/runpod-ollama.mjs",
};

function usage(): string {
  return `usage: cookbook <provider>\n  providers: ${Object.keys(PROVIDERS).join(", ")}`;
}

const provider = process.argv[2];

if (provider === undefined || provider.trim() === "") {
  log(`no provider given.\n${usage()}`);
  process.exit(1);
}

const recipeSpecifier = PROVIDERS[provider];
if (recipeSpecifier === undefined) {
  log(`unknown provider "${provider}".\n${usage()}`);
  process.exit(1);
}

log(`dispatching to provider "${provider}"`);
// Importing the recipe runs it (it ends in `await runRecipe(...)`), which never resolves on the
// happy path — it parks holding the box until signaled. await keeps this module alive with it.
await import(recipeSpecifier);
