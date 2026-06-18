# How settings work in MIRA

MIRA has two kinds of settings, with different ownership and access.

## 1. Operator / global settings

Server-wide configuration in `mira_config.json`: LLM providers + keys, channels, TTS/STT, agent behaviour, security, proxy, etc. The full, always-current list is in **`settings-reference.md`** (generated from the config schema).

- **Who can view/change:** admins only.
- **Where:** Settings UI (Server tab, provider/channel/voice sections), or `mira_config.json` directly. MIRA can also read and (for admins, with confirmation) change many of these on request.
- **Secrets are redacted.** API keys, tokens, passwords, `jwt_secret`, `webhook_secret`, `hmac_key`, etc. are never shown — MIRA will tell you whether one is *set* or *unset*, but never read the value back (it would otherwise leak into chat logs, voice notes, or relayed messages). You can overwrite a secret, but you can't read it.
- **Some changes need a restart** (Rust/embedded changes); most config edits apply live via the config watcher.

## 2. Per-user settings

Each person's own preferences, separate from everyone else's:

- **Voice preferences** — per-channel response policy (always / on voice input / never) + voice id.
- **Companion** — enable/disable, quiet hours, preferred channels, daily-briefing on/off + hour.
- **Channel accounts** — your Telegram bot, Signal number, email account(s).
- **Connected MCP servers** — the external tool servers you've added.
- **Profile** — facts about you MIRA uses to personalise.

You can view and change your **own** per-user settings via the UI or by asking MIRA. You cannot see or change another user's settings, and a regular user cannot see operator/global config.

## Access model (who can do what)

| | View global config | Change global config | View own settings | Change own settings | See another user's data |
|---|---|---|---|---|---|
| **Admin** | yes (secrets redacted) | yes (confirmed, audited; some keys protected) | yes | yes | no (by design) |
| **User** | no | no | yes | yes | no |

When you ask MIRA about a setting, it checks your access first. If you're not allowed to see or change something, it says so rather than leaking it.

## Changing settings by asking MIRA

You can say things like *"set my Telegram voice to always"*, *"turn on my daily briefing at 8am"*, or (as an admin) *"what's the default TTS backend?"* / *"change the agent max tool rounds to 12"*. MIRA confirms changes that affect the whole server, never echoes secrets, and records an audit entry for global changes. Anything genuinely sensitive (security, providers, proxy) is protected and steered back to the Settings UI.
