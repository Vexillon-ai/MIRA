import { test, expect } from '@playwright/test';
import { HOSTS, mintGuest, bearer, chat } from '../helpers/mira';

test.describe('guest session', () => {
  test('mint returns the documented contract', async ({ request }) => {
    const s = await mintGuest(request, HOSTS.main);
    expect(s.server?.name).toBeTruthy();
    expect(typeof s.access_token).toBe('string');
    expect(s.access_token.length).toBeGreaterThan(20);
    expect(s.token_type).toBe('Bearer');
    expect(s.session_ttl_secs).toBeGreaterThan(0);
    expect(s.expires_at_ms).toBeGreaterThan(Date.now());
    expect(s.user.username.startsWith('guest_')).toBeTruthy();
    expect(s.user.role).toBe('user');
  });

  test('the scoped token authenticates as the guest', async ({ request }) => {
    const s = await mintGuest(request, HOSTS.main);
    const me = await request.get(`${HOSTS.main}/api/auth/me`, { headers: bearer(s.access_token) });
    expect(me.status()).toBe(200);
    const body = await me.json();
    expect(body.id).toBe(s.user.id);
    expect(body.username).toBe(s.user.username);
  });

  test('a guest can chat and gets a reply', async ({ request }) => {
    const s = await mintGuest(request, HOSTS.main);
    const r = await chat(request, HOSTS.main, s.access_token, 'hello from a guest');
    expect(r.status).toBe(200);
    expect(r.reply).toContain('MOCK_REPLY');
    // The turn echoes the user's message through the (mock) model.
    expect(r.reply).toContain('hello from a guest');
  });

  test('no token → chat is rejected', async ({ request }) => {
    const res = await request.post(`${HOSTS.main}/api/chat`, { data: { message: 'hi' } });
    expect(res.status()).toBe(401);
  });
});
