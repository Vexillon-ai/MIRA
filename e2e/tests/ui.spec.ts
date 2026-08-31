import { test, expect } from '@playwright/test';
import { HOSTS } from '../helpers/mira';

// Light UI smoke — the restricted instance still serves the SPA. (The guest
// "Try now" front-end flow is app-team scope; the security surface is covered by
// the API specs.)
test.describe('web UI', () => {
  test('the SPA loads on the restricted instance', async ({ page }) => {
    const res = await page.goto(`${HOSTS.main}/`, { waitUntil: 'domcontentloaded' });
    expect(res?.status()).toBeLessThan(400);
    // The app mounts into #root; wait for it to exist.
    await expect(page.locator('#root')).toBeAttached();
    // Title is set by the SPA shell.
    await expect(page).toHaveTitle(/mira/i);
  });

  test('an unknown app route falls back to the SPA (not a hard 404 page)', async ({ page }) => {
    const res = await page.goto(`${HOSTS.main}/some/spa/route`, { waitUntil: 'domcontentloaded' });
    expect(res?.status()).toBeLessThan(400);
    await expect(page.locator('#root')).toBeAttached();
  });
});
