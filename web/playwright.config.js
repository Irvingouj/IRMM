import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: 'tests',
  timeout: 120_000,
  expect: { timeout: 20_000 },
  workers: 1,
  globalSetup: './tests/global-setup.js',
  use: { trace: 'retain-on-failure' },
});
