import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: 'tests',
  timeout: 120_000,
  expect: { timeout: 20_000 },
  workers: 1,
  globalSetup: './tests/global-setup.js',
  use: {
    trace: 'retain-on-failure',
    // Emit real host ICE candidates instead of mDNS .local names, which webrtc-rs
    // can't resolve on the loopback test network.
    launchOptions: {
      args: [
        '--disable-features=WebRtcHideLocalIpsWithMdns',
        '--use-gl=angle',
        '--use-angle=swiftshader',
        '--enable-unsafe-swiftshader',
        '--ignore-gpu-blocklist',
      ],
    },
  },
});
