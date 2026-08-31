# Restricted Mode — Docker e2e (Playwright)

End-to-end tests that exercise the **Restricted Mode** featureset against a real,
Dockerised MIRA — the fail-closed capability gate, resource/cost caps, and
ephemeral guest sessions.

## What's here

- **`docker-compose.test.yml`** — the stack: a deterministic mock LLM plus **six
  MIRA instances**, each the same image with a different baked config, so the
  fail-closed matrix (that one instance can't prove) is real:

  | instance | port | config | proves |
  |---|---|---|---|
  | `mira-main` | 18081 | hardened + guest, generous caps | guest mint/contract/chat, capability gate, isolation, seeding, health |
  | `mira-caps` | 18082 | tight rate / concurrency / token cap | per-user rate limit, `max_tokens` clamp, concurrency shedding |
  | `mira-daily` | 18086 | low daily token ceiling | daily-ceiling degradation |
  | `mira-capacity` | 18083 | `max_active=1` | capacity → `503` |
  | `mira-noguest` | 18084 | hardened, guest **off** | guest mint → `403` |
  | `mira-unrestricted` | 18085 | **no profile**, guest on | guest mint → `403` (fail-closed misconfig) |

- **`mock-llm/`** — a zero-dependency OpenAI-compatible mock so chat-path tests
  are deterministic. Its `/debug/last-request` side-channel lets a test assert
  *what MIRA actually sent the model* (e.g. the clamped `max_tokens`).
- **`Dockerfile.test`** — a fast test image wrapping a **host-built** release
  binary (base `ubuntu:24.04` to match host glibc). This is **not** the
  production image (that's the repo-root `Dockerfile`).
- **`configs/`** — the baked per-instance configs (generated from a canonical
  `mira setup` config; every one is schema-validated).
- **`tests/`** — the Playwright specs. **`helpers/mira.ts`** has the shared API
  helpers (guest mint, SSE chat reader, mock inspector).

## Run it

Docker must be running. One command builds everything, brings up a **fresh**
stack (required — the cap tests reason about per-instance state), and runs the
suite:

```sh
e2e/run.sh
```

Iterating on tests only (reuse the running stack + images):

```sh
e2e/run.sh --no-build          # or: cd e2e && npx playwright test
```

Tear down:

```sh
e2e/run.sh --down
```

## Coverage

- **Fail-closed guest matrix** — `201` only with profile + guest enabled; `403`
  when guest disabled; `403` when guest enabled but **no profile** (never handed
  out on an unrestricted instance); `503` at `max_active`.
- **Guest session** — the documented mint contract, the scoped token
  authenticates as the guest, guest chat end-to-end, no-token → `401`.
- **Capability gate** — allow-listed Pure tools run; a spread of registered
  tools (network-tier + deferred-side-effect) are denied with a restricted-mode
  reason; **spoofed args cannot widen** the profile.
- **Resource & cost caps** — per-user rate limit throttles; the per-turn token
  cap actually sets the provider's `max_tokens`; the concurrency cap sheds a
  parallel turn as "busy"; the daily token ceiling degrades once reached.
- **Isolation & seeding** — a guest's wiki is seeded from the baseline;
  conversations don't leak across guests; guests are distinct principals.
- **Health** — `/livez` and `/readyz` on every instance.
- **Web UI** — the SPA loads on a restricted instance; unknown routes fall back
  to it.

## Optional: real-LLM smoke (local model)

A bonus, **opt-in** pass that points a hardened instance at a real local model
(LM Studio / Ollama on the Windows host) and checks Restricted Mode still behaves
with a genuine model (loose assertions). It **auto-skips** unless started:

```sh
# 1. Edit configs/local.json — set providers.lmstudio.url + default_model to
#    your local endpoint (LM Studio :1234, or Ollama :11434 + a pulled model).
# 2. Start just this instance:
docker compose -f e2e/docker-compose.test.yml --profile local up -d mira-local
# 3. Run the suite; tests/local-smoke.spec.ts now runs instead of skipping.
```
