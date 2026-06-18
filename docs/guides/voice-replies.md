---
title: Voice replies & talking to MIRA
description: Get spoken replies as voice notes, pick a natural voice with Kokoro, and send MIRA voice notes it transcribes so you can talk back.
sidebar:
  order: 3
---

MIRA can **speak** — its replies and check-ins can arrive as voice notes — and
it can **listen** — voice notes you send are transcribed so you can talk
instead of type. This guide sets both up.

Voice works **per channel**: you decide, for each channel, whether MIRA speaks.
On a phone channel like Telegram or Signal a spoken reply arrives as a voice
note; in the web UI it plays in the browser.

You'll need a channel connected first — see
**[Connect a channel](connect-a-channel.md)**.

## Choose when MIRA speaks (per channel)

Each channel has its own **voice policy**, set under **Settings**:

- **Off** — text only on this channel.
- **On request** — MIRA speaks only when you ask it to, or when you sent a voice
  note (it replies in kind).
- **Always** — MIRA sends a voice note alongside its text reply, every time.

For example, set Telegram or Signal to **always** if you'd like every reply as a
voice note on your phone, but leave email as text. The policy is per channel, so
mixing styles is fine.

## Pick a voice

MIRA ships with a choice of voices. Pick one under your voice settings — for
example **`af_heart`** or **`bf_emma`** (Kokoro voices). Your voice preference
follows you across channels.

### Enable Kokoro for natural local speech

MIRA's voice runs entirely on your own machine by default — no cloud calls
needed. There are three local quality tiers:

- **Kokoro** — natural, studio-quality speech that runs locally. **Enable this**
  if you want MIRA to actually sound good. It's a larger download (a few hundred
  MB) but needs no cloud round-trip.
- **Piper** — the lightweight local default. Small, fast, and good enough for
  most uses; works out of the box.
- **eSpeak** — a tiny last-resort fallback used only if a model download fails.
  It sounds robotic.

Turn on Kokoro in your voice settings and choose a Kokoro voice (such as
`af_heart` or `bf_emma`). MIRA downloads the model the first time it's needed.

> Kokoro speaks **English only** (American or British) and has no speech-rate
> control. For other languages, MIRA falls back to eSpeak or a configured
> cloud / OpenAI-compatible voice backend. On a plain CPU, expect
> Piper-quality speed; Kokoro shines on capable hardware.

If your operator has configured a cloud or self-hosted voice backend
(OpenAI-compatible, ElevenLabs, Cartesia, and similar), those voices appear as
options too. The local backends are the privacy-preserving default.

## Hear it in the web UI

In the web chat, spoken replies **play in the browser** — no phone channel
needed. With your web channel's voice policy set to **on request** or
**always**, replies are synthesised and played back inline as MIRA writes them.

## Talk back: send MIRA voice notes

Voice isn't one-way. On a channel that carries audio — Telegram, Signal, and
custom voice-capable channels — you can **send MIRA a voice note** and it will
**transcribe** it (using the built-in whisper speech-to-text) and treat it as a
normal message. Combined with an **always** voice policy, that's a full spoken
back-and-forth: you talk, MIRA talks back.

## Troubleshooting

- **No voice note on my phone.** Check that the channel's voice policy is **on
  request** or **always**, and that the channel is connected and enabled.
- **The voice sounds robotic.** You're likely on eSpeak (the fallback). Enable
  **Kokoro** (or Piper) and pick a Kokoro voice.
- **My language isn't English.** Kokoro is English-only; use a configured cloud
  voice backend, or accept eSpeak for other languages.

## Related

- [Connect a channel](connect-a-channel.md) — set up Telegram, Signal, and more.
- [Proactive check-ins & daily briefing](proactive-checkins-and-briefing.md) —
  have proactive check-ins arrive as voice notes too.
- [Channels](../concepts/channels.md) — how voice fits the channel model.
