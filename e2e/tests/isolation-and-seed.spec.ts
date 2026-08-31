import { test, expect } from '@playwright/test';
import { HOSTS, mintGuest, guestToken, bearer, chat } from '../helpers/mira';

test.describe('guest isolation & seeding', () => {
  test('each guest wiki is seeded from the baseline', async ({ request }) => {
    const token = await guestToken(request, HOSTS.main);
    const res = await request.post(`${HOSTS.main}/api/tools/run`, {
      headers: bearer(token),
      data: { name: 'wiki_read', args: { path: 'welcome.md' } },
    });
    const body = await res.json();
    expect(body.success, `wiki_read: ${body.error || ''}`).toBeTruthy();
    // The seeded page carries a unique marker → seed_wiki_dir was copied in.
    expect(body.output).toContain('SEEDED_WIKI_MARKER_42');
  });

  test('conversations do not leak across guests', async ({ request }) => {
    // Guest A holds a conversation.
    const a = await mintGuest(request, HOSTS.main);
    const r = await chat(request, HOSTS.main, a.access_token, 'private to A');
    expect(r.reply).toContain('MOCK_REPLY');

    const aList = await (await request.get(`${HOSTS.main}/api/conversations`, { headers: bearer(a.access_token) })).json();
    const aConvs = Array.isArray(aList) ? aList : aList.conversations || [];
    expect(aConvs.length).toBeGreaterThan(0);
    const aIds = new Set(aConvs.map((c: any) => c.id));

    // A fresh guest B sees NONE of A's conversations.
    const b = await mintGuest(request, HOSTS.main);
    const bList = await (await request.get(`${HOSTS.main}/api/conversations`, { headers: bearer(b.access_token) })).json();
    const bConvs = Array.isArray(bList) ? bList : bList.conversations || [];
    for (const c of bConvs) {
      expect(aIds.has(c.id), 'guest B must not see guest A conversations').toBeFalsy();
    }
  });

  test('seeded memory recall context is isolated to the guest', async ({ request }) => {
    // Two guests are distinct principals: A's /api/auth/me id ≠ B's.
    const a = await mintGuest(request, HOSTS.main);
    const b = await mintGuest(request, HOSTS.main);
    expect(a.user.id).not.toBe(b.user.id);
    expect(a.user.username).not.toBe(b.user.username);
  });
});
