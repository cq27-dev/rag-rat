// Dual esbuild: one shared TS source -> Node (desktop) + webworker (web/Cursor-remote) bundles.
// `vscode` stays external in both (provided by the extension host).
const esbuild = require('esbuild');

const production = process.argv.includes('--production');
const watch = process.argv.includes('--watch');

/** @type {import('esbuild').BuildOptions} */
const shared = {
  entryPoints: ['src/extension.ts'],
  bundle: true,
  format: 'cjs',
  minify: production,
  sourcemap: !production,
  sourcesContent: false,
  external: ['vscode'],
  logLevel: 'warning',
};

const nodeConfig = {
  ...shared,
  platform: 'node',
  outfile: 'dist/node/extension.js',
};

const webConfig = {
  ...shared,
  platform: 'browser',
  outfile: 'dist/web/extension.js',
  define: { global: 'globalThis' },
};

async function main() {
  const ctxs = await Promise.all([esbuild.context(nodeConfig), esbuild.context(webConfig)]);
  if (watch) {
    await Promise.all(ctxs.map((c) => c.watch()));
    console.log('[watch] node + web');
  } else {
    await Promise.all(ctxs.map((c) => c.rebuild()));
    await Promise.all(ctxs.map((c) => c.dispose()));
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
