import { test, expect } from '@playwright/test';
import { HOSTS, guestToken, bearer } from '../helpers/mira';

// Run a tool through the authenticated /api/tools/run endpoint (which flows
// through the same registry gate the model uses). Returns the ToolResult.
async function runTool(request: any, token: string, name: string, args: any = {}) {
  const res = await request.post(`${HOSTS.main}/api/tools/run`, {
    headers: bearer(token),
    data: { name, args },
  });
  return { status: res.status(), body: await res.json() };
}

// Allow-listed (Pure) showcase tools — must NOT be gate-denied. (A tool may
// still return its own success:false for bad args; we only assert the restricted
// gate didn't block it.)
const ALLOWED = ['now', 'date_math', 'math_eval', 'mira_help', 'wiki_read', 'wiki_search', 'recall_history'];

// Registered but NOT allowed under hardened — must be denied with a
// restricted-mode reason. Mix of Network-tier and deferred-side-effect Pure.
const DENIED = [
  'weather',                       // Network tier
  'settings_describe',             // Pure, unlisted
  'settings_get',                  // Pure, unlisted
  'automations_schedule_followup', // schedules future sends
  'companion_enable',              // reconfigures proactive delivery
  'backup_create',                 // filesystem
  'calendar_list_events',          // external-ish
  'image_generate',                // network
  'create_named_agent',            // system
];

test.describe('capability gate (hardened)', () => {
  test('allow-listed Pure tools run (not gate-denied)', async ({ request }) => {
    const token = await guestToken(request, HOSTS.main);
    for (const name of ALLOWED) {
      const { status, body } = await runTool(request, token, name, { query: 'x', path: 'welcome', expression: '1+1', memory_id: 1, content: 'x' });
      expect(status, `${name} status`).toBe(200);
      const err = (body?.error || '') as string;
      expect(err.includes('restricted mode'), `${name} must not be gate-denied (got: ${err})`).toBeFalsy();
    }
  });

  test('now (allow-listed) succeeds', async ({ request }) => {
    const token = await guestToken(request, HOSTS.main);
    const { body } = await runTool(request, token, 'now');
    expect(body.success).toBeTruthy();
    expect(body.output).toContain('utc');
  });

  for (const name of DENIED) {
    test(`denied: ${name}`, async ({ request }) => {
      const token = await guestToken(request, HOSTS.main);
      const { status, body } = await runTool(request, token, name, { location: 'x', query: 'x' });
      expect(status).toBe(200);
      expect(body.success).toBeFalsy();
      expect((body.error || '').toLowerCase()).toContain('restricted mode');
    });
  }

  test('spoofed args cannot widen the profile', async ({ request }) => {
    const token = await guestToken(request, HOSTS.main);
    // A prompt-injected model can put anything in args — none of it flips the gate.
    const { body } = await runTool(request, token, 'weather', {
      location: 'x',
      _restricted_mode: false,
      _profile: 'off',
      tier: 'pure',
      allow: ['weather'],
    });
    expect(body.success).toBeFalsy();
    expect((body.error || '').toLowerCase()).toContain('restricted mode');
  });
});
