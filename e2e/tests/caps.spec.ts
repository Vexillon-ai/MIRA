import { test, expect } from '@playwright/test';
import { HOSTS, MOCKS, guestToken, chat, mockLastRequest, mockReset } from '../helpers/mira';

// caps instance: messages_per_min=2, max_concurrent_sessions=1, max_tokens_per_turn=64.
// daily instance: daily_token_ceiling=120 (mock reports 50 tokens/turn).
test.describe('resource & cost caps', () => {
  test('per-user rate limit throttles the 3rd message in a minute', async ({ request }) => {
    const token = await guestToken(request, HOSTS.caps);
    const r1 = await chat(request, HOSTS.caps, token, 'one');
    const r2 = await chat(request, HOSTS.caps, token, 'two');
    const r3 = await chat(request, HOSTS.caps, token, 'three');
    expect(r1.reply).toContain('MOCK_REPLY');
    expect(r2.reply).toContain('MOCK_REPLY');
    // 3rd is over the 2/min cap → friendly throttle, no real turn.
    expect(r3.denied).toBeTruthy();
    expect(r3.raw.toLowerCase()).toContain('too quickly');
  });

  test('per-turn token cap clamps the provider max_tokens', async ({ request }) => {
    await mockReset(request, MOCKS.caps);
    const token = await guestToken(request, HOSTS.caps);
    const r = await chat(request, HOSTS.caps, token, 'clamp me');
    expect(r.reply).toContain('MOCK_REPLY');
    const last = await mockLastRequest(request, MOCKS.caps);
    // max_tokens_per_turn = 64 → MIRA must set GenerationOptions.max_tokens=64.
    expect(last.max_tokens).toBe(64);
  });

  test('global concurrency cap: a parallel over-cap turn degrades to busy', async ({ request }) => {
    // max_concurrent_sessions=1 + a ~1.2s mock delay: fire two at once; the
    // second can't get a slot and gets the friendly "busy" message.
    const token = await guestToken(request, HOSTS.caps);
    const [a, b] = await Promise.all([
      chat(request, HOSTS.caps, token, 'concurrent A'),
      chat(request, HOSTS.caps, token, 'concurrent B'),
    ]);
    const replies = [a, b];
    const busy = replies.filter((r) => r.denied && r.raw.toLowerCase().includes('busy'));
    const ran  = replies.filter((r) => r.reply.includes('MOCK_REPLY'));
    expect(busy.length, 'exactly one turn should be shed as busy').toBe(1);
    expect(ran.length, 'the other should run').toBe(1);
  });

  test('global daily token ceiling degrades once reached', async ({ request }) => {
    const token = await guestToken(request, HOSTS.daily);
    // 50 tokens/turn, ceiling 120: turns 1-3 run, turn 4 is over the ceiling.
    const results = [];
    for (let i = 0; i < 4; i++) {
      results.push(await chat(request, HOSTS.daily, token, `msg ${i}`));
    }
    expect(results[0].reply).toContain('MOCK_REPLY');
    const last = results[3];
    expect(last.denied).toBeTruthy();
    expect(last.raw.toLowerCase()).toContain('usage limit');
  });
});
