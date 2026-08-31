import { test, expect } from '@playwright/test';
import { HOSTS } from '../helpers/mira';

test.describe('health probes', () => {
  for (const [name, host] of Object.entries(HOSTS)) {
    test(`${name}: /livez returns 200 ok`, async ({ request }) => {
      const res = await request.get(`${host}/livez`);
      expect(res.status()).toBe(200);
      expect((await res.text()).trim()).toBe('ok');
    });

    test(`${name}: /readyz answers (200 or 503)`, async ({ request }) => {
      const res = await request.get(`${host}/readyz`);
      expect([200, 503]).toContain(res.status());
    });
  }
});
