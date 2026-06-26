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
 * Unknown or missing provider → an `error` event (pre-`ready` failure) + exit 1. Like the recipes,
 * the dispatcher writes ONLY JSONL events to stdout.
 */

import { emit, log } from "./src/contract.js";

/** provider keyword → recipe module specifier (relative to this file's location in dist/). */
const PROVIDERS: Readonly<Record<string, string>> = {
  modal: "./recipes/modal-ollama.mjs",
  runpod: "./recipes/runpod-ollama.mjs",
};

function fail(message: string): never {
  emit({ type: "error", message: `${message}\nproviders: ${Object.keys(PROVIDERS).join(", ")}`, ts: Date.now() });
  process.exit(1);
}

const provider = process.argv[2];

if (provider === undefined || provider.trim() === "") {
  fail("no provider given");
}

const recipeSpecifier = PROVIDERS[provider];
if (recipeSpecifier === undefined) {
  fail(`unknown provider "${provider}"`);
}

log("info", `dispatching to provider "${provider}"`);
// Importing the recipe runs it (it ends in `await runRecipe(...)`), which never resolves on the
// happy path — it parks holding the box until signaled. await keeps this module alive with it.
await import(recipeSpecifier);
