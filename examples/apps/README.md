# Example apps (MIRA apps framework)

Demo apps for the MIRA apps framework (Guardian Phase 2). An **app** is an
installable `.mirapkg` package with an `app` component: it ships its own UI,
declares tools MIRA can call, and declares events it may emit onto the shared bus.

## `demo-hello` — the slice-1 vertical demo

`demo-hello.mirapkg` proves every seam end-to-end:

- **Installs** through the normal package pipeline (it's unsigned, so tick
  "install anyway" — it requests no capabilities).
- **Renders its own UI** — open the **Apps** page in the web UI; its HTML is served
  by MIRA and embedded in a sandboxed iframe (opaque origin: it can't read MIRA's
  session token; it reaches MIRA only via a `postMessage` broker).
- **Exposes a tool MIRA can call** — `app__com-mira-demo-hello__echo` (echoes its
  `text` argument). Ask MIRA to use it in chat.
- **Emits events** — the two buttons emit `app.demo.hello` (severity `info` → flows
  to MIRA's interaction/automations layer) and `app.demo.issue` (severity `warn` →
  an *issue* the **Guardian** monitors: watch the log for
  `MIRA-Guardian: app warn 'app.demo.issue' …` and the `app_issues_total` counter
  at `GET /api/guardian/status`). Benign `info` events do **not** reach the Guardian
  — that's the two-actor split (MIRA interacts; the Guardian watches for problems).

### Install

Web UI → **Plugins** → upload `demo-hello.mirapkg` → confirm (allow untrusted) →
then open **Apps**. Uninstall from **Plugins**; the app's tool + UI drop live.

### Rebuild the bundle

```sh
cd examples/apps
STAGE=$(mktemp -d)/com.mira.demo-hello
mkdir -p "$STAGE/ui"
cp demo-hello/package.json "$STAGE/package.json"
cp demo-hello/ui/index.html "$STAGE/ui/index.html"
tar czf demo-hello.mirapkg -C "$(dirname "$STAGE")" com.mira.demo-hello
```

## `home-assistant` — the first real app

`home-assistant.mirapkg` connects MIRA to a [Home Assistant](https://www.home-assistant.io/)
instance — the apps framework's first real integration (sensor source + control
surface). It exercises the `http` tool handler + per-app config + secrets:

- **Config** — set `base_url` (your HA URL, e.g. `http://homeassistant.local:8123`)
  and a **Long-Lived Access Token** (a `secret`, stored encrypted in the vault, never
  returned to the browser) under the app's settings, or via
  `PUT /api/admin/apps/com.mira.home-assistant/config`.
- **Tools MIRA can call** — all `http` handlers against the HA REST API, so the token
  and URL come from config at call time:
  - `get_state` → `GET ${base_url}/api/states/${entity_id}`
  - `list_states` → `GET ${base_url}/api/states` (with a `response` projection —
    see below — so it returns a compact `entity_id`/`state`/`friendly_name` list
    instead of a 150 KB dump of every entity's full attributes)
  - `call_service` → `POST ${base_url}/api/services/${domain}/${service}`
  Every call runs through MIRA's **SSRF-guarded** HTTP layer. Because HA is a home
  service, the app declares its host as LAN egress (`capabilities.network_egress:
  ["${config.base_url}"]`), so MIRA relaxes the private-network block **for exactly
  the host you configure** — loopback and cloud-metadata addresses stay blocked
  regardless. The `Authorization: Bearer <token>` header is templated from the
  vaulted secret. Then just ask MIRA: *"turn off the kitchen
  lights"*, *"what's the living-room temperature?"*.
- **Response projection** — an `http` handler may declare an optional `response`
  block to reduce a large JSON reply before the model sees it (a purely declarative
  field-and-row reduction — no code runs):
  ```json
  "response": { "select": ["entity_id", "state", "attributes.friendly_name"], "limit": 400 }
  ```
  Each array element (or a single object) is projected to just the selected dotted
  paths — keyed by the last segment, so `attributes.friendly_name` becomes
  `friendly_name` — and arrays are capped to `limit` with a "showing N of M" note.
  This is what makes `list_states` usable on a local model: HA's `/api/states`
  returns every entity with all attributes (~40K tokens), which otherwise overflows
  the model's context. A model-facing size cap still applies as a backstop even
  without a projection.
- **UI** — a status panel (Apps page) with example prompts.

- **Guardian detection** — the app declares a `health_check` (polls `${base_url}/api/`
  with the token header every 5 min through MIRA's SSRF-guarded HTTP). When HA can't be
  reached it emits `app.home_assistant.unreachable` (severity `warn`), which the
  **Guardian** triages into an operator alert — so you find out your home automation is
  down without asking. Recovery is logged, not re-alerted.

### Rebuild the bundle

```sh
cd examples/apps
# the bundle's top-level dir must equal the manifest id
tar czf home-assistant.mirapkg -C examples/apps \
  --transform 's,^home-assistant,com.mira.home-assistant,' home-assistant
```
