import tseslint from 'typescript-eslint';

// Type-aware linting for the extension sources, scoped to promise safety.
//
// `tsc --noEmit` cannot see a promise nobody awaited: a dropped `.catch()` is well-typed, so an
// unhandled rejection reaches the host as an "extension caused an error" toast with no stack from
// our code. These two rules are the only ones enabled — a broader recommended set would flag
// unrelated style across the whole tree and bury the class this exists to catch.
export default tseslint.config(
  { ignores: ['dist/**', 'node_modules/**', 'esbuild.js'] },
  {
    files: ['src/**/*.ts'],
    extends: [tseslint.configs.base],
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      '@typescript-eslint/no-floating-promises': 'error',
      '@typescript-eslint/no-misused-promises': 'error',
    },
  },
);
