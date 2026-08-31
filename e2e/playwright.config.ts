import { defineConfig, devices } from '@playwright/test';

// The suite runs against the Docker stack (docker-compose.test.yml). Bring it up
// first (../e2e/run.sh, or `docker compose -f e2e/docker-compose.test.yml up -d`).
export default defineConfig({
  testDir: './tests',
  globalSetup: './global-setup.ts',
  // Cap parallelism: cap/isolation tests reason about per-instance state, so we
  // keep workers modest and let each spec mint its own fresh guest.
  workers: 4,
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: [['list'], ['html', { open: 'never' }]],
  timeout: 40_000,
  expect: { timeout: 10_000 },
  projects: [
    { name: 'chromium', use: { ...devices['Desktop Chrome'] } },
  ],
});
