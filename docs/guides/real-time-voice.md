---
title: Real-time voice
description: Drive a low-latency speak/listen loop over MIRA's chat API — cancel mid-sentence, answer in a spoken style, authenticate a device, and start turns speculatively.
sidebar:
  order: 20
---

MIRA's chat API has everything a real-time voice assistant needs to run a tight
speak/listen loop against a MIRA instance: a way to identify a turn, stop it the
instant the user interrupts, ask for a spoken-style reply in a given language,
measure where the time goes, sign in a headless device, and even start answering
before the user has finished speaking.

Everything here is **additive and opt-in** — if your client doesn't use these
fields, MIRA behaves exactly as it always has. The web UI, the mobile app, and
the chat channels are unaffected.

## Identify a turn

Every `POST /api/chat` response now opens with a `turn_started` server-sent
event and an `X-Mira-Turn-Id` response header:

```
event: turn_started
data: {"turn_id":"7f3c…","channel":"voice"}
```

Keep that `turn_id` — it's how you cancel or commit the turn while it's running.
(Clients that don't recognise the event simply ignore it.)

## Speak the voice channel

Tell MIRA the turn is a voice turn and how to shape the reply:

```json
POST /api/chat
{ "message": "what's the weather like",
  "channel": "voice",
  "reply_language": "fi",
  "style": "spoken" }
```

- `channel: "voice"` marks the turn as voice (it's recorded on the conversation,
  turns on per-turn timings, and keeps the prompt cache-friendly — see below).
- `style: "spoken"` asks MIRA to answer in plain spoken sentences: no markdown,
  lists, tables, URLs, code blocks or emoji; short by default; a brief clarifying
  question rather than a guess; and **digits kept as digits** for times, dates,
  prices and phone numbers (your speech engine reads them aloud in-language).
- `reply_language` is a BCP-47 tag (`fi`, `en-GB`, `pt-BR`, …). MIRA replies in
  that language whatever the user wrote in.

Unknown values are **ignored with a `warning` event, never an error**, so a typo
degrades gracefully.

## Cancel / barge-in

When the user starts talking over the reply, cancel the turn:

```json
POST /api/chat/{turn_id}/cancel
{ "spoken_text": "The weather today is" }
```

MIRA stops generating promptly and stores **only what was actually delivered**
(`spoken_text`, flagged interrupted) — never the part the user never heard, and
it is never mined into memory. Send `{ "discard": true }` to keep nothing at all.
The route is idempotent; it returns `404` for an unknown turn, `403` for someone
else's, and `409` for a turn older than the cancel window
(`voice.cancel_window_secs`, default 10 min).

**Cancel still works after `done`.** The voice engine often speaks a reply after
MIRA has finished generating it, and the user interrupts mid-speech. A cancel
sent within `voice.cancel_window_secs` of `done` truncates the **stored** reply
to `spoken_text` (marked interrupted) just like a cancel during generation.

On the voice channel, if your SSE connection simply drops mid-reply MIRA treats
that as a cancel (a hang-up is a barge-in). Turn this off with
`voice.implicit_cancel_on_disconnect = false`; it only ever applies to voice.

## Watch the latency

Voice turns (and any request with `"debug_timings": true`) stream `timing`
events so you can tune your pipeline. One arrives just before the first token
with the pre-token breakdown — `memory_recall_ms`, `wiki_ms`,
`tool_selection_ms`, `prompt_build_ms`, `llm_request_to_first_token_ms`,
`total_to_first_token_ms` — and a second at the end with `total_ms`,
`tool_rounds`, `llm_tokens_out`, and the provider's `prompt_tokens` /
`cached_tokens` (so you can confirm your planner's prompt cache is being hit).

## Authenticate a device

A voice device can't do an interactive login, so give it a **device token**. In
**Settings → Notifications → Device tokens**, create one with a name, scopes
(`chat` and `chat.cancel`) and an optional expiry. You'll see the raw
`mdt_…` token **once** — copy it then; MIRA stores only its hash. The device
sends it as a normal bearer token:

```
Authorization: Bearer mdt_1a2b3c…
```

A device token never grants more than the user who created it, is limited to the
chat endpoints, and can be **revoked instantly** from the same panel. You can
also manage them over the API at `/api/users/{user_id}/device-tokens`.

## Start speculatively (optional)

If your speech pipeline can guess the user is nearly done, start the turn early
with `"speculative": true`. MIRA streams tokens as usual but **holds every side
effect** — it won't execute a tool, and it writes nothing to history or memory —
until you commit:

```json
POST /api/chat/{turn_id}/commit
{ "message": "what's the weather like today" }
```

- If the final transcript **matches** what you sent, MIRA proceeds: any held tool
  calls run and the turn is saved.
- If it **differs**, MIRA discards the speculative work and **restarts** the turn
  with the corrected message under the same `turn_id`, emitting a `restarted`
  event (drop whatever you streamed and start fresh).
- To abandon it, `POST /api/chat/{turn_id}/cancel {"discard": true}`. A
  speculative turn that isn't committed within `voice.speculative_timeout_secs`
  (default 5 s) is discarded automatically — no history, no memory, no trace.

A discarded or timed-out speculative turn leaves **nothing** behind: no
conversation is created, no message stored, no title set, and no memory
extraction runs — all of that happens only on commit. Until you commit, the turn
is purely in-flight.

## Point MIRA at your language router

The voice engine typically runs MIRA against a **local OpenAI-compatible
endpoint**. MIRA's OpenAI-compatible provider needs nothing more than a name, a
base URL and a model — set `auth_style = "none"` for an unsecured local endpoint
(no key). Only `/chat/completions` is required; `/v1/models` is optional (it's
used to populate the model picker). Standard OpenAI tool-call format is assumed.

**Give the voice channel its own LLM** without replacing the instance's main
provider. Declare one or more extra providers under
`providers.openai_compat_extra`, then point the voice channel at one by name:

```jsonc
"providers": {
  "openai_compat_extra": [
    { "name": "voice-router", "base_url": "http://192.0.2.10:8088/v1",
      "default_model": "qwen3", "auth_style": "none",
      "context_length_tokens": 11264 }
  ]
},
"voice": { "provider": "voice-router" }
```

Now a `channel:"voice"` turn uses `voice-router` while web, Telegram and every
other channel keep the default provider. If the router is unreachable, the voice
turn **fails over** to the default chain and streams a `warning` naming the
fallback (turn this off with `voice.provider_failover = false`). The provider's
`context_length_tokens` becomes that turn's context budget. On streamed requests
MIRA sends `stream_options: {include_usage: true}`, so the `timing` event's
`cached_tokens` reflect the router's prompt-cache hits.

## Keep spoken replies fast

A spoken reply can't wait through many tool calls. On the voice channel MIRA
caps tool rounds at `voice.max_tool_rounds` (default 2); when the cap is hit it
takes one final round with tools disabled, answers briefly from what it has
(saying plainly if it couldn't find something), and emits a
`voice_tool_cap_reached` event (with `rounds_used`) so your client can show a
holding phrase. Voice turns also prefer answering general-knowledge questions
directly, using tools only for the user's personal data (calendar, memory,
reminders), live data (time, weather, news), or an explicit request to look
something up.

## Settings

All voice settings live under `voice.*` (see the settings reference):
`cancel_window_secs`, `implicit_cancel_on_disconnect`, `speculative_timeout_secs`,
`spoken_max_sentence_chars`, `max_tool_rounds`, `provider`, and
`provider_failover`. Every default preserves existing behaviour.
