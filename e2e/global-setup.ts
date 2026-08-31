// Wait for every MIRA instance to be live before the suite runs, so a slow
// container start doesn't look like a test failure.
import { request, FullConfig } from '@playwright/test';
import { HOSTS } from './helpers/mira';

export default async function globalSetup(_config: FullConfig) {
  const api = await request.newContext();
  const deadline = Date.now() + 90_000;
  for (const [name, host] of Object.entries(HOSTS)) {
    let ok = false;
    while (Date.now() < deadline) {
      try {
        const res = await api.get(`${host}/livez`, { timeout: 3000 });
        if (res.status() === 200) { ok = true; break; }
      } catch { /* not up yet */ }
      await new Promise((r) => setTimeout(r, 1000));
    }
    if (!ok) throw new Error(`instance '${name}' (${host}) never became live (/livez)`);
    // eslint-disable-next-line no-console
    console.log(`  ✓ ${name} live`);
  }
  await api.dispose();
}
