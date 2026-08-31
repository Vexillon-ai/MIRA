// Shared helpers for the Restricted Mode e2e suite.
import { APIRequestContext } from '@playwright/test';

// Per-scenario MIRA instances (see docker-compose.test.yml).
export const HOSTS = {
  main:         'http://localhost:18081',
  caps:         'http://localhost:18082',
  capacity:     'http://localhost:18083',
  noguest:      'http://localhost:18084',
  unrestricted: 'http://localhost:18085',
  daily:        'http://localhost:18086',
} as const;

// Mock LLMs — the /debug side-channel lets us assert what MIRA sent the model.
export const MOCKS = {
  main:  'http://localhost:18091',
  caps:  'http://localhost:18092',
  daily: 'http://localhost:18093',
} as const;

export function bearer(token: string) {
  return { Authorization: `Bearer ${token}` };
}

export interface GuestSession {
  server: { name: string; base_url?: string };
  access_token: string;
  token_type: string;
  expires_at_ms: number;
  session_ttl_secs: number;
  user: { id: string; username: string; role: string };
}

// Mint a guest and return the parsed session (throws if not 201).
export async function mintGuest(api: APIRequestContext, host: string): Promise<GuestSession> {
  const res = await api.post(`${host}/api/auth/guest`);
  if (res.status() !== 201) {
    throw new Error(`guest mint at ${host} returned ${res.status()}: ${await res.text()}`);
  }
  return res.json();
}

export async function guestToken(api: APIRequestContext, host: string): Promise<string> {
  return (await mintGuest(api, host)).access_token;
}

export interface ChatResult {
  status: number;
  raw: string;        // full SSE body
  reply: string;      // concatenated `event: token` data
  denied: boolean;    // true when no MOCK_REPLY (i.e. a canned cap/throttle message)
}

// POST /api/chat and read the SSE stream to completion.
export async function chat(
  api: APIRequestContext, host: string, token: string, message: string,
): Promise<ChatResult> {
  const res = await api.post(`${host}/api/chat`, {
    headers: bearer(token),
    data: { message },
    timeout: 30_000,
  });
  const raw = await res.text();
  // Collect the data payloads that follow an `event: token` line.
  const reply = raw
    .split('\n')
    .reduce<{ out: string[]; take: boolean }>((acc, line) => {
      if (line.startsWith('event:')) acc.take = line.includes('token');
      else if (line.startsWith('data:') && acc.take) acc.out.push(line.slice(5).trim());
      return acc;
    }, { out: [], take: false })
    .out.join(' ');
  return { status: res.status(), raw, reply, denied: !raw.includes('MOCK_REPLY') };
}

// The last chat-completions request a mock received (for max_tokens / injected
// context assertions).
export async function mockLastRequest(api: APIRequestContext, mock: string): Promise<any> {
  const res = await api.get(`${mock}/debug/last-request`);
  return res.json();
}
export async function mockReset(api: APIRequestContext, mock: string): Promise<void> {
  await api.post(`${mock}/debug/reset`);
}
