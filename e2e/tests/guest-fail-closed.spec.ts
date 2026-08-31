import { test, expect } from '@playwright/test';
import { HOSTS } from '../helpers/mira';

// The load-bearing security property: a guest is minted ONLY when a restriction
// profile is active AND guest sessions are enabled. Anything else must 403.
test.describe('guest mint — fail-closed matrix', () => {
  test('enabled + profile active → 201', async ({ request }) => {
    const res = await request.post(`${HOSTS.main}/api/auth/guest`);
    expect(res.status()).toBe(201);
  });

  test('guest DISABLED (profile active) → 403', async ({ request }) => {
    const res = await request.post(`${HOSTS.noguest}/api/auth/guest`);
    expect(res.status()).toBe(403);
  });

  test('guest enabled but NO profile (misconfig) → 403 — never on an unrestricted instance', async ({ request }) => {
    const res = await request.post(`${HOSTS.unrestricted}/api/auth/guest`);
    expect(res.status()).toBe(403);
  });

  test('capacity: at max_active the mint degrades to 503', async ({ request }) => {
    // capacity instance has max_active = 1.
    const first = await request.post(`${HOSTS.capacity}/api/auth/guest`);
    expect(first.status()).toBe(201);
    const second = await request.post(`${HOSTS.capacity}/api/auth/guest`);
    expect(second.status()).toBe(503);
  });
});
