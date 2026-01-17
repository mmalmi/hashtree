import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'happy-dom',
    include: ['tests/**/*.test.ts'],
    exclude: ['tests/direct-nav.test.ts'],
    globals: true,
  },
});
