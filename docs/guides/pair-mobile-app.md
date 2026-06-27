---
title: Pair the mobile app & enable mobile push
description: Connect the MIRA Android app in one QR scan, and (optionally) turn on Firebase Cloud Messaging so proactive notifications reach your phone.
sidebar:
  order: 13
---

The MIRA mobile app connects to **your** MIRA instance — the same server you
run for the web UI. Pairing is one scan: no typing the server URL, no
re-entering your password. This guide covers pairing a device and, for
admins, turning on push delivery to the app via Firebase Cloud Messaging
(FCM).

## Pair a device

1. In the web UI, open **Settings → Notifications**.
2. Under **Pair a mobile device**, click **Show pairing code**. A QR code
   appears with a countdown.
3. Open the MIRA app on your phone and scan it.

That's it — the app now knows your server's address and is signed in as you.
The pairing code is **single-use** and expires after about two minutes; if it
lapses before you scan, click **Regenerate code**. The panel flips to
**"✓ Paired"** once your phone connects.

### How it stays safe

- The pairing secret is generated per request, shown only in your
  authenticated browser (inside the QR), stored **hashed** on the server, and
  **consumed on first use** — a second attempt to claim the same code fails.
- Repeated bad claim attempts are rate-limited and IP-banned exactly like
  failed logins.
- The QR also carries your server's **display name** and **canonical base
  URL** so the app shows the right instance. Set those under
  `server.display_name` and `server.public_base_url` (the latter matters when
  MIRA is behind a reverse proxy — otherwise the app may be handed an internal
  address).

## Enable mobile push (admins)

Browser and phone **Web Push** work out of the box. The native app receives
push through **Firebase Cloud Messaging**, which is **off by default** — with
it disabled, nothing about your server changes.

To turn it on:

1. Create a Firebase project and download a **service-account JSON** with
   permission to send FCM messages.
2. Put the file somewhere only the MIRA process user can read it.
3. In `mira_config.json`:

   ```jsonc
   "notifications": {
     "fcm": {
       "enabled": true,
       "project_id": "your-firebase-project-id",
       "service_account_json_path": "/etc/mira/fcm-service-account.json"
     }
   }
   ```

4. Restart MIRA.

The service-account path is treated as a **secret** — MIRA redacts it when you
read config back through the API, and won't let it be overwritten blindly.

### What the app receives

Every proactive notification carries a small, stable metadata envelope so the
app can route it to the right Android notification channel:

- `type` — `message`, `conversation`, `care`, `system`, or `guardian`
- `severity` — `high` for care/wellbeing alerts, `normal` otherwise
- `title`, `body`, and (when relevant) `conversation_id` / `url`

Care and wellbeing alerts are delivered at **high** Android priority so they
aren't held back; everything else is normal priority.

## Test it

Settings → Notifications → **Send test push** delivers a synthetic
notification to every registered device for your account — browsers and paired
phones alike. Paired phones show up in the **Registered devices** list with a
📱 and their device name.
