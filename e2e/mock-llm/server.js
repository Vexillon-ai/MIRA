// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Deterministic OpenAI-compatible mock LLM for the Restricted Mode e2e suite.
// MIRA's keyless `lmstudio` provider points at this. Zero dependencies (Node
// http only) so the container is tiny and starts instantly.
//
// Endpoints:
//   GET  /v1/models              — catalog probe.
//   POST /v1/chat/completions    — streaming (SSE) or non-streaming completion.
//                                  Reply is deterministic: "MOCK_REPLY " + echo
//                                  of the last user message.
//   GET  /debug/last-request     — the last chat-completions request body, so a
//                                  test can assert what MIRA actually SENT the
//                                  model (injected context, clamped max_tokens).
//   POST /debug/reset            — clear the recorded request.
//   GET  /healthz                — liveness for compose.
//
// Env:
//   PORT               (default 8080)
//   MOCK_USAGE_TOTAL   total_tokens reported per turn (default 50) — lets the
//                      daily-token-ceiling cap be exercised deterministically.

const http = require('http');

const PORT        = parseInt(process.env.PORT || '8080', 10);
const USAGE_TOTAL = parseInt(process.env.MOCK_USAGE_TOTAL || '50', 10);
// Artificial per-turn latency (ms) so a turn holds MIRA's concurrency slot long
// enough for a concurrent request to hit the cap deterministically.
const DELAY_MS    = parseInt(process.env.MOCK_DELAY_MS || '0', 10);

let lastRequest = null; // most recent /v1/chat/completions body

function readBody(req) {
  return new Promise((resolve) => {
    let data = '';
    req.on('data', (c) => { data += c; });
    req.on('end', () => {
      try { resolve(data ? JSON.parse(data) : {}); }
      catch { resolve({}); }
    });
  });
}

function lastUserMessage(body) {
  const msgs = Array.isArray(body.messages) ? body.messages : [];
  for (let i = msgs.length - 1; i >= 0; i--) {
    if (msgs[i] && msgs[i].role === 'user') {
      return typeof msgs[i].content === 'string' ? msgs[i].content : '';
    }
  }
  return '';
}

function usageObj(prompt) {
  const p = Math.max(1, Math.round((prompt || '').length / 4));
  const c = Math.max(1, USAGE_TOTAL - p);
  return { prompt_tokens: p, completion_tokens: c, total_tokens: USAGE_TOTAL };
}

function handleChat(req, res, body) {
  lastRequest = {
    at: Date.now(),
    model: body.model,
    stream: !!body.stream,
    max_tokens: body.max_tokens ?? null,
    messages: body.messages || [],
    // Flattened text of the whole prompt, for easy substring assertions.
    prompt_text: (body.messages || [])
      .map((m) => (typeof m.content === 'string' ? m.content : JSON.stringify(m.content)))
      .join('\n'),
  };

  const reply = 'MOCK_REPLY ' + lastUserMessage(body);
  const usage = usageObj(lastRequest.prompt_text);

  if (body.stream) {
    res.writeHead(200, {
      'Content-Type': 'text/event-stream',
      'Cache-Control': 'no-cache',
      'Connection': 'keep-alive',
    });
    const base = { id: 'mock-1', object: 'chat.completion.chunk', model: body.model || 'mock' };
    const send = (obj) => res.write('data: ' + JSON.stringify(obj) + '\n\n');
    // One content delta…
    send({ ...base, choices: [{ index: 0, delta: { content: reply }, finish_reason: null }] });
    // …then a terminal chunk carrying finish_reason + usage (lmstudio parses it).
    send({ ...base, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage });
    res.write('data: [DONE]\n\n');
    res.end();
  } else {
    const payload = {
      id: 'mock-1', object: 'chat.completion', model: body.model || 'mock',
      choices: [{ index: 0, message: { role: 'assistant', content: reply }, finish_reason: 'stop' }],
      usage,
    };
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(payload));
  }
}

const server = http.createServer(async (req, res) => {
  const url = (req.url || '').split('?')[0];

  if (req.method === 'GET' && url === '/healthz') {
    res.writeHead(200, { 'Content-Type': 'text/plain' }); return res.end('ok');
  }
  if (req.method === 'GET' && url === '/v1/models') {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    return res.end(JSON.stringify({ object: 'list', data: [{ id: 'mock', object: 'model', owned_by: 'mock' }] }));
  }
  if (req.method === 'POST' && url === '/v1/chat/completions') {
    const body = await readBody(req);
    if (DELAY_MS > 0) await new Promise((r) => setTimeout(r, DELAY_MS));
    return handleChat(req, res, body);
  }
  if (req.method === 'GET' && url === '/debug/last-request') {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    return res.end(JSON.stringify(lastRequest || {}));
  }
  if (req.method === 'POST' && url === '/debug/reset') {
    lastRequest = null;
    res.writeHead(200, { 'Content-Type': 'application/json' });
    return res.end('{"ok":true}');
  }

  res.writeHead(404, { 'Content-Type': 'text/plain' });
  res.end('not found');
});

server.listen(PORT, '0.0.0.0', () => {
  console.log(`mock-llm listening on :${PORT} (usage_total=${USAGE_TOTAL})`);
});
