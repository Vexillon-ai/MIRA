import { test, expect, request } from '@playwright/test';
import { mintGuest, bearer, chat } from '../helpers/mira';

// OPT-IN real-LLM smoke against a local model (option 3). Confirms Restricted
// Mode behaves with a genuine, non-deterministic model — assertions are LOOSE.
// Auto-skips unless the mira-local instance is up (start it with:
//   docker compose -f e2e/docker-compose.test.yml --profile local up -d mira-local
// after editing e2e/configs/local.json to point at your LM Studio / Ollama).
const LOCAL = 'http://localhost:18087';

test.describe('local-model smoke (real LLM, opt-in)', () => {
  test.beforeAll(async () => {
    const api = await request.newContext();
    let up = false;
    try { up = (await api.get(`${LOCAL}/livez`, { timeout: 2000 })).status() === 200; } catch { /* down */ }
    await api.dispose();
    test.skip(!up, 'mira-local not running (start it with --profile local)');
  });

  test('a guest can chat with the real model', async ({ request }) => {
    const s = await mintGuest(request, LOCAL);
    const r = await chat(request, LOCAL, s.access_token, 'Say hello in one short sentence.');
    expect(r.status).toBe(200);
    // Loose: the real model produced *some* non-empty reply and it was not a
    // canned cap/refusal message.
    expect(r.reply.trim().length).toBeGreaterThan(0);
    expect(r.raw.toLowerCase()).not.toContain('too quickly');
    expect(r.raw.toLowerCase()).not.toContain('usage limit');
  });

  test('the capability gate still denies a dangerous tool with a real model', async ({ request }) => {
    const s = await mintGuest(request, LOCAL);
    const res = await request.post(`${LOCAL}/api/tools/run`, {
      headers: bearer(s.access_token),
      data: { name: 'weather', args: { location: 'London' } },
    });
    const body = await res.json();
    expect(body.success).toBeFalsy();
    expect((body.error || '').toLowerCase()).toContain('restricted mode');
  });
});
