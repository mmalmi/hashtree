import tseslint from '@typescript-eslint/eslint-plugin';
import tsparser from '@typescript-eslint/parser';
import svelte from 'eslint-plugin-svelte';
import svelteParser from 'svelte-eslint-parser';
import globals from 'globals';

export default [
  // Global ignores
  {
    ignores: ['**/dist/**', '**/node_modules/**', '**/*.d.ts', '**/build/**'],
  },
  // TypeScript files (including .svelte.ts which are plain TS with Svelte conventions)
  {
    files: ['**/*.ts', '**/*.tsx', '**/*.svelte.ts'],
    languageOptions: {
      parser: tsparser,
      parserOptions: {
        ecmaVersion: 'latest',
        sourceType: 'module',
      },
      globals: {
        ...globals.browser,
        ...globals.node,
      },
    },
    plugins: {
      '@typescript-eslint': tseslint,
    },
    rules: {
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_' }],
      '@typescript-eslint/no-explicit-any': 'warn',
      'prefer-const': 'error',
      'no-console': 'off',
    },
  },
  // Svelte files - use recommended flat config
  ...svelte.configs['flat/recommended'].map(config => ({
    ...config,
    // Exclude .svelte.ts files - they are pure TS, not Svelte components
    ignores: [...(config.ignores || []), '**/*.svelte.ts'],
  })),
  {
    files: ['**/*.svelte'],
    languageOptions: {
      parser: svelteParser,
      parserOptions: {
        parser: tsparser,
        ecmaVersion: 'latest',
        sourceType: 'module',
      },
      globals: {
        ...globals.browser,
      },
    },
    plugins: {
      '@typescript-eslint': tseslint,
    },
    rules: {
      // TypeScript rules
      'no-unused-vars': 'off',
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_' }],
      '@typescript-eslint/no-explicit-any': 'warn',
      'no-console': 'off',
      // Disable prefer-const for Svelte files - runes require let
      'prefer-const': 'off',
      // Svelte specific
      'svelte/no-unused-svelte-ignore': 'warn',
      'svelte/valid-compile': ['error', { ignoreWarnings: true }],
      // Warn but don't error on these - good to fix but not critical
      'svelte/require-each-key': 'warn',
      'svelte/no-at-html-tags': 'warn',
      'svelte/prefer-svelte-reactivity': 'warn',
    },
  },
];
