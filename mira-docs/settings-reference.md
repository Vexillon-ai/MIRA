# MIRA settings reference

> Auto-generated from `config/mira_config.schema.json` — the single source of truth for global/operator settings. Do not hand-edit; regenerate with `scripts/gen_settings_reference.py` when the schema changes.

> These are **server-wide (operator) settings** stored in `mira_config.json`. Viewing/changing them requires admin access. Secret values (API keys, tokens, passwords) are always redacted on read.


## config_version

- **`config_version`** (string; one of: `1`) — Configuration file format version. Managed by MIRA — do not edit manually. Used to detect when a migration is needed.

## data_dir

- **`data_dir`** (string) — Root directory for all MIRA data files (databases, history, memory, auth, exports). Supports ~ for the home directory. Default: ~/.mira/data. Pick this during `mira setup`, or override at runtime with the `--data-dir` flag / MIRA_DATA_DIR env (which win over this field). `mira install` bakes the resolved absolute path into the service so a supervised service reads the same location regardless of which account runs it.

## primary_provider

- **`primary_provider`** (string; one of: `ollama`, `lmstudio`, `openrouter`, `openai`, `deepseek`, `moonshot`, `groq`, `xai`, `openai_compat`, `anthropic`, `gemini`) — The AI provider MIRA uses for chat by default. Must match the slug of a configured provider under `providers`. Can be switched at runtime with /provider-use.

## failover_providers

- **`failover_providers`** (array) — Ordered list of provider slugs used as AUTOMATIC failover after the primary provider — presence = enabled for fallback, order = priority. Null (default) is fail-closed local-only: only local providers (lmstudio, ollama, a loopback/LAN openai_compat) receive conversations automatically when the primary fails; cloud providers never do. Cloud providers stay available for EXPLICIT model selection regardless — this governs only the silent auto-failover chain, so a local 'heart' can't leak conversations off-box on a crash/timeout. An empty list disables auto-failover entirely (hard fail-closed).

## providers

_AI provider connection and model settings. Only configure the providers you intend to use._

- **`providers.ollama`** (object) — Ollama local inference server (https://ollama.com). Install Ollama and run 'ollama serve' before starting MIRA.
- **`providers.ollama.enabled`** (boolean) — When false, the provider is omitted from the failover chain at startup AND from the chat-page model dropdown. Default true for backwards compatibility.
- **`providers.ollama.url`** (string) — Ollama API base URL. Overridable with the OLLAMA_HOST environment variable.
- **`providers.ollama.default_model`** (string) — Model to use on startup. Run 'ollama list' to see installed models, 'ollama pull <model>' to download one.
- **`providers.ollama.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.ollama.timeout_secs`** (integer) — HTTP request timeout in seconds. Increase for large models or slow hardware.
- **`providers.lmstudio`** (object) — LM Studio local inference server (https://lmstudio.ai). Start the local server from the LM Studio application before running MIRA.
- **`providers.lmstudio.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.lmstudio.url`** (string) — LM Studio server base URL (OpenAI-compatible API endpoint).
- **`providers.lmstudio.default_model`** (string) — Model identifier exactly as shown in the LM Studio UI.
- **`providers.lmstudio.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.lmstudio.timeout_secs`** (integer) — HTTP request timeout in seconds. Set higher (300–600) for large models.
- **`providers.openrouter`** (object) — OpenRouter cloud gateway (https://openrouter.ai). Provides access to 100+ hosted models including GPT-4, Claude, Llama, and more.
- **`providers.openrouter.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.openrouter.api_key`** (string) — OpenRouter API key. Can alternatively be set via the OPENROUTER_API_KEY environment variable (env var takes precedence). Obtain a key at https://openrouter.ai/keys.
- **`providers.openrouter.base_url`** (string) — OpenRouter API base URL. Change only if using a self-hosted OpenRouter-compatible gateway.
- **`providers.openrouter.default_model`** (string) — Default model to use via OpenRouter. Browse available models at https://openrouter.ai/models.
- **`providers.openrouter.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.openrouter.catalog_refresh_hours`** (integer) — How long the cached OpenRouter model catalog is reused before re-fetching. Cache lives at <data_dir>/cache/openrouter-models.json. 0 disables caching.
- **`providers.openai`** (object) — OpenAI hosted API (https://platform.openai.com). Set api_key to enable; leave null to skip registration.
- **`providers.openai.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.openai.api_key`** (string) — 
- **`providers.openai.base_url`** (string) — Override only for Azure-style proxies. Default https://api.openai.com/v1.
- **`providers.openai.default_model`** (string) — Examples: gpt-4o, gpt-4o-mini, o1, o3-mini.
- **`providers.openai.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.openai.timeout_secs`** (integer) — 
- **`providers.deepseek`** (object) — DeepSeek (https://platform.deepseek.com). OpenAI-compatible. Default model deepseek-chat; deepseek-reasoner for R1-style chain-of-thought.
- **`providers.deepseek.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.deepseek.api_key`** (string) — 
- **`providers.deepseek.base_url`** (string) — 
- **`providers.deepseek.default_model`** (string) — 
- **`providers.deepseek.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.deepseek.timeout_secs`** (integer) — 
- **`providers.moonshot`** (object) — Moonshot AI / Kimi (https://platform.moonshot.ai). OpenAI-compatible. Models: kimi-k2-0905-preview, kimi-thinking-preview, etc.
- **`providers.moonshot.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.moonshot.api_key`** (string) — 
- **`providers.moonshot.base_url`** (string) — 
- **`providers.moonshot.default_model`** (string) — 
- **`providers.moonshot.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.moonshot.timeout_secs`** (integer) — 
- **`providers.groq`** (object) — Groq (https://console.groq.com) — fast hosted inference for Llama, Mixtral, DeepSeek, and others. OpenAI-compatible.
- **`providers.groq.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.groq.api_key`** (string) — 
- **`providers.groq.base_url`** (string) — 
- **`providers.groq.default_model`** (string) — 
- **`providers.groq.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.groq.timeout_secs`** (integer) — 
- **`providers.xai`** (object) — xAI Grok (https://x.ai/api). OpenAI-compatible. Models: grok-4, grok-3-fast, etc.
- **`providers.xai.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.xai.api_key`** (string) — 
- **`providers.xai.base_url`** (string) — 
- **`providers.xai.default_model`** (string) — 
- **`providers.xai.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.xai.timeout_secs`** (integer) — 
- **`providers.openai_compat`** (object) — Generic OpenAI-compatible gateway. Use this for any provider not already in the list above — Together, Fireworks, Perplexity, Mistral La Plateforme, DeepInfra, Azure OpenAI, vLLM-self-hosted, LocalAI, etc. Pick a `name` slug used in logs and ProviderId.
- **`providers.openai_compat.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.openai_compat.name`** (string) — Slug used in logs + ProviderId. Empty = disabled.
- **`providers.openai_compat.api_key`** (string) — 
- **`providers.openai_compat.base_url`** (string) — 
- **`providers.openai_compat.default_model`** (string) — 
- **`providers.openai_compat.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.openai_compat.timeout_secs`** (integer) — 
- **`providers.openai_compat.auth_style`** (string; one of: `bearer`, `azure`, `none`) — Which auth header to send: 'bearer' (default, OpenAI-style Authorization), 'azure' (api-key header used by Azure OpenAI), or 'none' (no auth header — for unsecured local endpoints).
- **`providers.anthropic`** (object) — Anthropic Claude (https://console.anthropic.com). Native /v1/messages API — not OpenAI-compatible. Required header: anthropic-version (pinned by MIRA). Set api_key to enable.
- **`providers.anthropic.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.anthropic.api_key`** (string) — Anthropic API key. Obtain at https://console.anthropic.com/settings/keys.
- **`providers.anthropic.base_url`** (string) — Override only for Anthropic-compatible proxies (e.g. Vercel AI Gateway, LiteLLM). Default https://api.anthropic.com.
- **`providers.anthropic.default_model`** (string) — Examples: claude-sonnet-4-5, claude-haiku-4-5, claude-opus-4-1.
- **`providers.anthropic.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.anthropic.timeout_secs`** (integer) — 
- **`providers.gemini`** (object) — Google Gemini (https://aistudio.google.com). Native :generateContent API — not OpenAI-compatible. Uses x-goog-api-key header. Set api_key to enable.
- **`providers.gemini.enabled`** (boolean) — When false, skip registration. Default true.
- **`providers.gemini.api_key`** (string) — Google API key. Obtain at https://aistudio.google.com/apikey.
- **`providers.gemini.base_url`** (string) — Override only for Gemini-compatible proxies. Default https://generativelanguage.googleapis.com (the AI Studio API, not Vertex).
- **`providers.gemini.default_model`** (string) — Examples: gemini-2.5-pro (top), gemini-2.5-flash (fast/cheap), gemini-2.5-flash-lite.
- **`providers.gemini.available_models`** (array) — Models the admin has added for this provider — shown in the chat-page dropdown and anywhere a model can be picked. Empty = treat default_model as the sole entry. Populated via the catalog's Add button on /providers.
- **`providers.gemini.timeout_secs`** (integer) — 

## cli

_Settings for the simple reedline CLI mode (invoked with `--simple` flag)._

- **`cli.colored_output`** (boolean) — Enable ANSI colour codes in CLI output. Disable if your terminal does not support ANSI colours.
- **`cli.streaming`** (boolean) — Stream AI response tokens as they arrive. When false, the full response is buffered before being displayed.
- **`cli.prompt`** (string) — Input prompt string displayed before each user message.

## tui

_Rich terminal UI (TUI) appearance and layout defaults. All settings can be changed at runtime._

- **`tui.theme`** (string; one of: `mira-dark`, `mira-light`, `dracula`, `gruvbox`, `nord`) — Colour theme. Switch at runtime with /theme <name> or F5. Available themes: mira-dark, mira-light, dracula, gruvbox, nord.
- **`tui.layout`** (string; one of: `simple`, `standard`, `right-full`, `left-full`, `right-only`, `left-only`) — Initial layout mode. Switch at runtime with /layout <mode> or F6. Modes: simple (chat+input only), standard (+status bar), right-full/left-full (sidebar+status bar), right-only/left-only (sidebar, no status bar).
- **`tui.show_timestamps`** (boolean) — Display timestamps next to messages in the chat view.
- **`tui.show_token_count`** (boolean) — Display the running token estimate in the status bar.
- **`tui.mode`** (string; one of: `auto`, `local`, `server`) — TUI backend selection. 'auto' picks server mode when `server.enabled` and reachable, else local. 'local' always talks to AgentCore directly. 'server' requires a running MIRA server; errors if unreachable.
- **`tui.server_url`** (string) — Base URL of the MIRA HTTP server for server-mode TUI. Used when mode=server or mode=auto with `server.enabled`=true.
- **`tui.auto_token_path`** (string) — Path to the local bearer token the server mints at startup for same-host TUI use. Supports '~'. Ignored when MIRA_TOKEN env is set.
- **`tui.resume_last`** (boolean) — When true, on startup the TUI loads the tail of the most recent tui conversation and continues it instead of starting empty.

## server

_MIRA HTTP server settings (invoked with `--server` flag). Exposes an API for Telegram, Signal, and other webhook-based integrations._

- **`server.enabled`** (boolean) — Whether the server starts automatically when MIRA launches in server mode.
- **`server.host`** (string) — IP address to bind. Use '0.0.0.0' to accept external connections, '127.0.0.1' for localhost only.
- **`server.port`** (integer) — TCP port to listen on.
- **`server.tls_cert_path`** (string) — Path to a TLS certificate file in PEM format. Set to null to use plain HTTP (not recommended for public servers).
- **`server.tls_key_path`** (string) — Path to the TLS private key file in PEM format. Required when tls_cert_path is set.
- **`server.auth_token`** (string) — Bearer token for API authentication. Callers must include 'Authorization: Bearer <token>' in every request. Set to null to disable authentication (not recommended for public-facing servers).
- **`server.request_timeout_secs`** (integer) — Timeout (seconds) applied to the server's OUTBOUND HTTP client (the reqwest client the server builds). Note: this is not an inbound per-request/408 limit.
- **`server.readiness_cache_ttl_secs`** (integer) — Short TTL (seconds) the `/readyz` readiness probe caches its provider-reachability verdict for. The verdict is refreshed out of band (serve-stale-while-revalidate), so however many load balancers probe in a tight loop, at most one provider round-trip is in flight per window and no probe blocks on the provider. 0 disables caching (every probe checks the provider inline). Default 30.
- **`server.display_name`** (string) — Human-readable label for this instance (e.g. 'Tarek's MIRA'), shown by the mobile app and returned in the device-pairing payload and /api/status. Null falls back to the hostname.
- **`server.public_base_url`** (string) — Canonical public base URL (scheme+host[+port]) the outside world — phones, pairing QR codes — should use to reach this instance. Null derives it from the incoming request. Set this when behind a reverse proxy.
- **`server.remote_url`** (string) — Externally-reachable base URL for REMOTE access away from the LAN — e.g. a Tailscale MagicDNS name (https://mira.my-tailnet.ts.net) or a Cloudflare Tunnel / DDNS hostname. Distinct from public_base_url (the LAN/current address): this is the 'away' endpoint embedded as remote_url in the pairing QR so the mobile app can auto-select it when the LAN address is unreachable. Null falls back to Tailscale auto-detection, then omits the field. Must be an absolute http/https URL when set. Also settable via the MIRA_REMOTE_URL env var.
- **`server.update_check`** (object) — Passive check against an upstream Releases API for a newer MIRA build, surfaced in Settings and as an admin banner. ON by default: it's a single lightweight request that only COMPARES versions — it never downloads or installs anything (upgrading is always an explicit action). Set enabled=false to stop MIRA contacting the release host entirely.
- **`server.update_check.enabled`** (boolean) — Whether to check the source URL for new releases. Default true (check-only; installing is always a deliberate 'Upgrade now' / `mira upgrade` action).
- **`server.update_check.source_url`** (string) — Releases API URL to check. Must be a PUBLIC releases API: the checker sends no repository token, so a private GitHub/GitLab repo returns 404 (GitLab: 'Project Not Found') and the check fails. Defaults to the public MIRA GitHub project (Vexillon-ai/MIRA); forks should override with their own public GitHub / GitLab releases endpoint. The configured source is shown (credential-stripped) on the admin Updates card. Empty string disables (same as enabled=false).
- **`server.update_check.frequency`** (string; one of: `daily`, `weekly`, `monthly`) — How often the server refreshes its cached check result. The Settings 'Check now' button always forces an immediate refresh regardless of this. Default 'daily'.
- **`server.web_apps`** (object) — Serve web apps/games that MIRA's coding agent builds (a completed task's output/index.html) at an isolated per-app origin, so 'open the game you built' returns a real clickable link instead of the model confabulating a browser-open it cannot perform. Each app is served at http://<task_id>.<host_suffix>:<port>/ — a distinct browser origin from the MIRA app itself, so a model-built app cannot read MIRA's session token or call its authenticated API.
- **`server.web_apps.enabled`** (boolean) — Master switch for serving built web apps. Default true.
- **`server.web_apps.mode`** (string; one of: `subdomain`, `port`, `both`) — How built apps are exposed — a security/reachability trade-off the deployer picks (the server can't auto-detect how a browser reaches it, nor fall back at runtime). 'subdomain': served at http://<task_id>.<host_suffix>:<port>/ — a distinct origin per app (isolates cookies AND localStorage), no extra port; works when the browser resolves the suffix to MIRA's box (same machine, or WSL via localhost) but NOT from a phone (localhost is the phone). 'port': a separate listener (web_apps.port) at http://<host>:<apps_port>/a/<task_id>/ — reachable over any host incl. a LAN / WSL-gateway IP / the mobile app, with weaker isolation (all apps share one origin; port is not a cookie boundary on the same host). 'both' (default): serve via both — the subdomain link for a same-box browser, the port path (/a/<id>/) for LAN / mobile clients that build the URL against their own base host; default so web apps are reachable from the mobile app out of the box.
- **`server.web_apps.host_suffix`** (string) — Host suffix for the per-app subdomain origin (subdomain/both mode; app served at http://<task_id>.<host_suffix>:<port>/). Default 'localhost' — resolved to loopback natively by every major browser (RFC 6761), giving origin isolation with no extra port. Only works when the browser reaches MIRA's box via that name (same machine, or WSL via localhost).
- **`server.web_apps.port`** (integer) — Listener port for 'port'/'both' mode. 0 (default) means `server.port` + 1.
- **`server.web_apps.advertised_host`** (string) — Host clients use to reach the 'port'-mode listener, used only to build the returned URL (e.g. a LAN or WSL-gateway IP like '198.51.100.10'). Null derives it from `server.public_base_url`, then `server.host` (when concrete), then 'localhost'.

## channels

_Messaging channel integrations. Each channel must also be enabled individually._

- **`channels.signal`** (object) — Signal messenger integration via signal-cli (https://github.com/AsamK/signal-cli). Requires signal-cli to be installed and the phone number to be registered.
- **`channels.signal.enabled`** (boolean) — Enable the Signal channel. Requires a configured phone_number and signal-cli on PATH.
- **`channels.signal.phone_number`** (string) — E.164-format phone number registered with Signal (e.g. '+15551234567').
- **`channels.signal.rest_port`** (integer) — Port used by the signal-cli REST API daemon.
- **`channels.signal.cli_binary`** (string) — Name or full path to the signal-cli binary. Use 'signal-cli' if it is on your PATH.
- **`channels.signal.data_dir`** (string) — Directory where signal-cli stores its account data and keys. Supports ~ expansion.
- **`channels.signal.hmac_key`** (string) — HMAC-SHA256 secret key used to verify the X-Signal-Signature header on incoming webhook requests. Configure the same key in signal-cli. Set to null to disable signature verification (not recommended for public servers).
- **`channels.telegram`** (object) — Telegram bot integration via the Bot API (https://core.telegram.org/bots). Create a bot with @BotFather to get a token.
- **`channels.telegram.enabled`** (boolean) — Enable the Telegram channel. Requires a bot_token.
- **`channels.telegram.bot_token`** (string) — Telegram Bot API token from @BotFather.
- **`channels.telegram.polling`** (boolean) — Use long-polling to receive updates instead of a webhook. Recommended for local development.
- **`channels.telegram.secret_token`** (string) — DEPRECATED (0.152.x) — secret-token verification is now per-bot on each ChannelAccount row, configured from the Channels page in the web UI. This field is accepted on existing configs so they keep loading; serde drops it on the next save. Value here is never read.
- **`channels.telegram.allow_insecure_webhook`** (boolean) — Fail-closed inbound webhook control. When an account has no per-account webhook secret configured, its inbound webhook is REJECTED unless this is true. Default false (secure). Set true only if you cannot register a secret and accept that anyone who learns the account id could POST forged updates.
- **`channels.discord`** (object) — Discord channel global config. Per-bot credentials live on each channel_accounts row (each MIRA user registers their own Discord application). This block only holds the MIRA-wide kill switch that the gateway connection + outbound dispatchers honour at request time.
- **`channels.discord.enabled`** (boolean) — Enable the Discord channel. When false, no gateway connections are opened and outbound dispatchers short-circuit. Per-row enabled flags still apply on top.
- **`channels.matrix`** (object) — Matrix channel global config. Per-account credentials (homeserver URL + access token) live on each channel_accounts row. This block only holds the MIRA-wide kill switch that the /sync long-poll loop + outbound dispatchers honour at request time.
- **`channels.matrix.enabled`** (boolean) — Enable the Matrix channel. When false, no /sync loops are started and outbound dispatchers short-circuit. Per-row enabled flags still apply on top.
- **`channels.whatsapp`** (object) — WhatsApp channel global config (Meta WhatsApp Business Cloud API). Per-account credentials (phone_number_id + tokens) live on each channel_accounts row. This block only holds the MIRA-wide kill switch that the webhook handler + outbound dispatchers honour at request time.
- **`channels.whatsapp.enabled`** (boolean) — Enable the WhatsApp channel. When false, inbound webhooks are dropped (200, no processing) and outbound dispatchers short-circuit. Per-row enabled flags still apply on top.
- **`channels.whatsapp.allow_insecure_webhook`** (boolean) — Fail-closed inbound webhook control. When an account has no per-account webhook secret configured, its inbound webhook is REJECTED unless this is true. Default false (secure). Set true only if you cannot register a secret and accept that anyone who learns the account id could POST forged updates.
- **`channels.slack`** (object) — Slack channel global config (Events API). Per-account credentials (bot token + signing secret) live on each channel_accounts row. This block only holds the MIRA-wide kill switch that the webhook handler + outbound dispatchers honour at request time.
- **`channels.slack.enabled`** (boolean) — Enable the Slack channel. When false, inbound webhooks are dropped (200, no processing) and outbound dispatchers short-circuit. Per-row enabled flags still apply on top.
- **`channels.external`** (object) — External plugin channels via the Channel Provider Protocol (CPP). Per-account config (provider kind, send_url, HMAC secrets) lives on each channel_accounts row. This block only holds the MIRA-wide kill switch that the /webhook/external/{id} handler + outbound dispatchers honour.
- **`channels.external.enabled`** (boolean) — Enable CPP external channels. When false, inbound webhooks are dropped (200, no processing) and outbound dispatchers short-circuit. Per-row enabled flags still apply on top.

## logging

_Log output configuration. Logs are written to a file — the terminal remains clean for UI output._

- **`logging.level`** (string; one of: `trace`, `debug`, `info`, `warn`, `error`) — Minimum log level to record. Recommended: 'info' for normal use, 'debug' or 'trace' for troubleshooting. Levels: trace < debug < info < warn < error.
- **`logging.format`** (string; one of: `compact`, `pretty`, `json`) — Log line format. 'compact' is human-readable single-line. 'pretty' is multi-line with colour. 'json' is machine-parseable for log aggregation (Loki, Datadog, etc.).
- **`logging.file`** (string) — Path to the log file. Supports ~ expansion. Parent directory is created automatically if absent.
- **`logging.max_file_size_mb`** (integer) — Maximum log file size in megabytes before the file is rotated.
- **`logging.max_files`** (integer) — Number of rotated log files to retain (including the active log file).

## memory

_Persistent memory system and vector search settings._

- **`memory.embedding`** (object) — Embedding model configuration. Embeddings convert text to vectors for semantic (meaning-based) memory search.
- **`memory.embedding.provider`** (string; one of: `internal`, `ollama`, `lmstudio`, `openai`, `openrouter`) — Embedding provider. 'internal' uses the built-in fastembed engine — it downloads the model automatically on first use, no server required. Other providers require a running local or remote server.
- **`memory.embedding.model`** (string) — Embedding model name. internal: 'BGE-small-en-v1.5' (default, ~24 MB) or 'all-MiniLM-L6-v2'. ollama: 'nomic-embed-text'. lmstudio: model name as shown in the UI. openai: 'text-embedding-3-small'.
- **`memory.embedding.provider_url`** (string) — Base URL of the embedding server. Required for 'ollama' and 'lmstudio'. Ignored for 'internal'. For 'openai' use null to default to https://api.openai.com/v1.
- **`memory.embedding.model_cache_dir`** (string) — Directory where the 'internal' provider caches downloaded model files. Supports ~ expansion.
- **`memory.embedding.api_key`** (string) — API key for the embedding provider. Required for 'openai' and 'openrouter'. Not used for 'internal', 'ollama', or 'lmstudio'.
- **`memory.embedding_dim`** (integer) — Dimensionality of embedding vectors. Must match the chosen model exactly. BGE-small-en-v1.5 and all-MiniLM-L6-v2 → 384. BGE-base-en-v1.5 → 768. text-embedding-3-small → 1536.
- **`memory.embedding_cache_size`** (integer) — Maximum number of embedding vectors to keep in the in-memory LRU cache. Higher values reduce repeated embedding requests at the cost of RAM.
- **`memory.similarity_threshold`** (number) — Minimum cosine similarity score (0.0–1.0) required for a memory to appear in semantic search results. Higher values return fewer but more precise matches. Start at 0.6 and tune as needed.
- **`memory.context_top_k`** (integer) — How many memories the per-turn context hook retrieves and injects into the prompt. Higher helps aggregation / 'how many' / multi-session questions at the cost of a larger prompt. Default 15.
- **`memory.indexer`** (object) — Background transcript indexer. Embeds historical chat messages into message_vectors so semantic recall can reach past conversations. Disabling stops new inserts but leaves existing vectors intact.
- **`memory.indexer.enabled`** (boolean) — Enable the background indexer. Defaults to true. Set to false to disable transcript semantic indexing entirely.
- **`memory.indexer.interval_secs`** (integer) — Seconds between idle polls for new messages. Only applies when the previous batch found zero rows; busy passes run back-to-back.
- **`memory.indexer.batch_size`** (integer) — Maximum messages embedded per pass. Higher values backfill faster on first run at the cost of longer embedding-provider stalls.
- **`memory.indexer.skip_roles`** (array) — Message roles the indexer skips. Defaults to ['tool', 'system'] since those messages aren't conversationally meaningful.
- **`memory.auto_extract`** (object) — Post-turn memory auto-extraction. 'off' disables proactive writes, 'heuristic' uses the bundled regex extractor (default), and 'llm' runs a structured extraction pass through the model with confidence gating and category allow-listing. 'llm_channels' enables the LLM extractor per-channel independently of 'mode'.
- **`memory.auto_extract.mode`** (string; one of: `off`, `heuristic`, `llm`) — Default extraction mode for all channels. 'off' — no auto-extraction anywhere. 'heuristic' — bundled regex extractor runs after every turn (default). 'llm' — LLM extractor runs on every channel post-turn. Per-channel overrides live in 'llm_channels'.
- **`memory.auto_extract.min_confidence`** (string; one of: `low`, `medium`, `high`) — Minimum confidence tier required to persist an LLM-extracted candidate. Only applied when the LLM extractor runs.
- **`memory.auto_extract.allowed_categories`** (array) — Memory categories eligible for LLM extraction. 'relationship' is off by default because it can involve third parties who haven't consented.
- **`memory.auto_extract.llm_channels`** (array) — Channels that use the richer LLM extractor regardless of 'mode' (unless mode='off'). Channel ids: 'web', 'telegram', 'signal', 'discord', 'slack', 'matrix', 'whatsapp', 'email'. Empty (default) means 'mode' alone decides. Example: ['telegram'] runs the LLM extractor for Telegram while everything else stays heuristic.
- **`memory.rollup`** (object) — Daily memory rollup. Background job that consolidates each user's previous UTC day of conversation into one summary memory, tagged 'rollup' and 'rollup:YYYY-MM-DD' for provenance. Off by default — costs one extra LLM call per active user per day, so users should opt in.
- **`memory.rollup.enabled`** (boolean) — Enable the rollup job. Defaults to false. When true, a background poller consolidates each active user's previous UTC day into a summary memory.
- **`memory.rollup.interval_secs`** (integer) — Seconds between polls. The loop wakes this often and runs rollups for any user whose target-day summary is still missing. One hour is a sensible default — the work is idempotent so repeats cost one DB check.
- **`memory.rollup.day_lag_days`** (integer) — How many UTC days back to summarise. 1 = yesterday (default, safe — today is still happening). Set to 0 only if you want mid-day partial summaries.
- **`memory.rollup.max_messages`** (integer) — Hard cap on messages fed to one summarizer call. Oldest-first truncation keeps the prompt bounded on heavy days.
- **`memory.rollup.max_chars_per_message`** (integer) — Per-message character cap before concatenation. Long pastes rarely help a day summary; truncating keeps the prompt short.
- **`memory.graph`** (object) — Temporal knowledge-graph memory. Additive and off by default. When enabled, the post-turn extractor also writes typed, timestamped triples to kg_entities/kg_edges so aggregation/counting questions resolve against exact set membership instead of fuzzy top-k retrieval.
- **`memory.graph.enabled`** (boolean) — Master switch. Off (default) = flat memory only, no graph extraction or retrieval.
- **`memory.recency`** (object) — Recency tuning for semantic recall. Retrieval blends similarity with an age-based freshness boost so recently-formed memories can surface ahead of older but frequently-reinforced ones: score' = (1-weight)*similarity + weight*2^(-age_days/half_life_days).
- **`memory.recency.weight`** (number) — Weight of the recency term in [0.0, 1.0]. 0.0 = pure similarity (pre-0.244 behaviour); higher favours fresher memories. Default 0.25.
- **`memory.recency.half_life_days`** (number) — Half-life in days: a memory this old contributes half the recency boost of a brand-new one. Larger = recency decays more slowly. Default 30.
- **`memory.consolidation`** (object) — Sleep-like consolidation passes over the knowledge graph. Phased nightly clean-up that resolves contradictions, dedups entities, and scores importance — all deterministic and MIRA-side. Each phase independently togglable so the ones that don't fit your use case can be turned off without disabling the others. All off by default.
- **`memory.consolidation.contradictions_enabled`** (boolean) — Phase C — resolve single-valued-predicate contradictions (works_at, lives_in, married_to, etc.): when multiple live edges exist for the same (subject, predicate), keep the newest and close the older edges' valid_to. Pure SQL + a curated predicate list. Off by default. The LongMemEval benchmark can't validate this work — its haystacks are activity-heavy and state-poor — ship and observe in production usage.
- **`memory.consolidation.entity_dedup_enabled`** (boolean) — Phase A — merge near-duplicate entities within the same entity_type via strict-token-subset + size-ratio rule (e.g. 'navy blazer' / 'navy blue blazer'). Re-points edges to the winner (more-edges, tiebreak: older), rolls loser name into winner aliases, marks loser superseded. Pure SQL, no LLM. Off by default. Runs per active user in the nightly `memory.rollup` tick AFTER contradictions_enabled (so dedup's edge-count tiebreak sees post-resolution counts). Production-driven: LongMemEval haystacks aren't long enough to grow the entity sprawl this consolidates.
- **`memory.consolidation.entity_dedup_ratio`** (number) — Size-ratio threshold for the Phase A merge rule (smaller / larger token counts). 0.6 (default) catches {navy, blazer} subset of {navy, blue, blazer} (2/3 = 0.67) while rejecting {plant} subset of {peace, lily, plant} (1/3 = 0.33). Raise toward 1.0 for stricter merges (only near-identical names), lower toward 0.4 for more aggressive merging (higher risk of false positives).
- **`memory.consolidation.importance_enabled`** (boolean) — Phase D — score every live edge nightly as ln(1 + access_count) × exp(-age_days / half_life). Retrieval already orders by importance DESC (no-op until non-zero scores exist), so enabling this biases context toward frequently-reinforced + recent facts. Access tracking on retrieval is always-on and free — only the scoring pass is gated. Off by default. Runs LAST in the nightly tick so it scores the post-dedup, post-contradiction edge set.
- **`memory.consolidation.importance_half_life_days`** (number) — Half-life for the Phase D decay term, in days. After this many days of no reinforcement, an edge's score decays to ~50% of its un-decayed strength. 30 default (month-scale, matches typical personal context drift). Range 1–365.

## session

_Conversation session lifecycle settings._

- **`session.cleanup_interval_secs`** (integer) — Interval in seconds between background sweeps that remove expired sessions.
- **`session.timeout_secs`** (integer) — Seconds of inactivity after which a session is marked as expired and eligible for cleanup.
- **`session.max_turns`** (integer) — Maximum number of conversation turns (one user message + one assistant reply = one turn) to retain per session. Older turns are dropped when this limit is reached.

## companion

_Companion-mode (proactive check-in) tuning. Companion is otherwise configured per-user; this holds global scheduler knobs._

- **`companion.max_unanswered_checkins`** (integer) — Pause proactive check-ins after this many go unanswered in a row; the counter resets on any user message, so check-ins resume when the user replies. 0 disables the cap (check-ins then bounded only by min-gap, daily cap, and quiet hours). Default 3.
- **`companion.max_per_day`** (integer) — Maximum proactive check-ins per user-local day (hard ceiling, separate from the unanswered cap). Default 6.
- **`companion.min_gap_minutes`** (integer) — Minimum minutes between consecutive proactive check-ins (frequency floor). Default 90.

## automations

_Automations subsystem — per-user quotas, agent-creation gating, and channel rate limits for scheduled prompts, webhooks, and event subscriptions._

- **`automations.quota_per_user`** (object) — Hard caps on how many automation rows a single user may own. Enforced at create-time.
- **`automations.quota_per_user.schedules`** (integer) — Maximum scheduled-prompt rows per user. Default: 50.
- **`automations.quota_per_user.webhooks`** (integer) — Maximum registered webhook rows per user. Default: 20.
- **`automations.quota_per_user.event_subscriptions`** (integer) — Maximum event-subscription rows per user. Default: 50.
- **`automations.agent_creates_pending`** (boolean) — When true, agent-authored automations land in 'pending_approval' instead of 'active'. The user approves or rejects from the UI. Default: true.
- **`automations.agent_rationale_required`** (boolean) — When true, agent-authored automations must include a non-empty rationale string. Default: true.
- **`automations.max_chain_depth`** (integer) — Hard cap on how many activations may chain together before the dispatcher refuses. Guards against runaway event loops. Default: 5.
- **`automations.max_action_secs`** (integer) — Wall-clock ceiling in seconds on a single automation action before it is aborted and recorded as a failure. Default 300. A prompt action can raise its own ceiling with max_action_secs on the action; this is the fallback for actions that don't set one. Raise it for long multi-cycle research automations that need roughly 15 minutes of search, fetch and write per run. Actions run off the tick loop, so a long one does not delay other schedules.
- **`automations.channel_rate_limits`** (object) — Per-channel cap on channel_message actions per minute, scoped to the row's owning user. Keys are channel ids ('web', 'signal', 'telegram', 'email'); the special key '*' is the fallback. A value of 0 disables the limit for that channel.
- **`automations.watchdog`** (object) — Log/audit watchdog. Tails the configured log file every interval_secs, fires a 'watchdog.alert' event for matched lines. Off by default; opt in by flipping enabled and setting notify_user_id.
- **`automations.watchdog.enabled`** (boolean) — Master switch. Default false.
- **`automations.watchdog.interval_secs`** (integer) — How often the heartbeat ticks. Default 60.
- **`automations.watchdog.severity_threshold`** (string; one of: `WARN`, `ERROR`) — Lowest level treated as alert. Default WARN.
- **`automations.watchdog.dedup_ttl_secs`** (integer) — Per-fingerprint suppression window. Default 600.
- **`automations.watchdog.rate_limit_per_min`** (integer) — Hard global cap on alerts/min. 0 disables. Default 10.
- **`automations.watchdog.storm_threshold`** (integer) — W4 storm pause — emit count from one source within storm_window_secs that triggers a per-source pause. 0 disables. Default 30.
- **`automations.watchdog.storm_window_secs`** (integer) — W4 storm pause — sliding window for the threshold. Default 300 (5 min).
- **`automations.watchdog.storm_cooldown_secs`** (integer) — W4 storm pause — how long a source stays paused after a storm trip. Default 900 (15 min).
- **`automations.watchdog.ignore_patterns`** (array) — Regexes that, when matched, skip the line before fingerprinting.
- **`automations.watchdog.notify_user_id`** (string) — Recipient user UUID for the auto-seeded ChannelMessage subscription. Null = events fire but no channel routing is auto-created.
- **`automations.watchdog.channel`** (string; one of: `web`, `signal`, `telegram`, `email`) — Channel for the auto-seeded subscription. Default web.
- **`automations.watchdog.log_file`** (string) — Override the tailed log path. Null falls back to `logging.file`.

## agent

_Agent reasoning and tool-calling behaviour._

- **`agent.max_tool_rounds`** (integer) — Maximum tool-call/observe cycles per user message before the agent gives up and returns its last response. Prevents runaway loops.
- **`agent.tool_mode`** (string; one of: `auto`, `openai`, `react`, `disabled`) — Tool-calling protocol. 'auto' tries OpenAI structured tool_calls first and falls back to ReAct text parsing. 'openai' enforces structured format only. 'react' enforces text parsing only. 'disabled' disables all tool use.
- **`agent.system_prompt_file`** (string) — Path to an `agent.md` persona file. If empty or the file is absent the built-in default MIRA prompt is used. Supports ~ expansion.
- **`agent.disable_reasoning`** (boolean) — Suppress model 'thinking' by appending the /no_think directive to the system prompt. Turn on when the active model is a reasoning model (e.g. the qwen3 family) — otherwise it can burn the per-round token budget on chain-of-thought before acting, stalling tool loops. The web chat can override this per-conversation. Default false.
- **`agent.playful_easter_eggs`** (boolean) — Playful 'easter eggs' personality layer. When on, MIRA recognises famous pop-culture references and playful prompts (mirror-mirror, 'open the pod bay doors', 'meaning of life', magic-8-ball 'should I…', 'marco', 'I wish…', etc.) and plays along — improvised, in the user's own personality/tone and scaled by their playfulness setting — without hijacking a genuine request. LLM-driven, no canned strings. Default true; set false to disable the delight layer instance-wide.
- **`agent.max_context_turns`** (integer) — Maximum conversation turns injected into the model context per request (used when context_length_tokens is 0). Older turns are silently omitted (they remain in the session store). Tune to fit your model's context window.
- **`agent.context_length_tokens`** (integer) — Token-aware context budgeting: the model's context window in tokens. 0 (default) keeps the legacy fixed max_context_turns window (no behaviour change). When set (e.g. 128000), MIRA fills the window by token budget instead — carrying far more history when it fits — while reserving room for the response (`agent.max_response_tokens`) and a safety margin (`agent.context_safety_margin_tokens`). Set to your primary model's real context length.
- **`agent.context_safety_margin_tokens`** (integer) — Tokens held back from the context budget as headroom (only when context_length_tokens > 0), guarding against token-estimate drift so a packed prompt doesn't overflow the model. Default 2048.
- **`agent.local_effective_context_tokens`** (integer) — Effective context window (tokens) to budget to when the active provider is LOCAL and context_length_tokens is 0 (unset). Local quantized models on consumer runtimes reliably attend to only a few thousand tokens despite advertising 128K, so budgeting to the advertised window packs history where the model can't recall it. This conservative default turns token budgeting (and therefore compaction) ON for local providers with a small window, keeping the prompt in the reliable-recall zone. An explicit context_length_tokens > 0 overrides this (cloud models with a real large window); 0 here restores the legacy fixed max_context_turns window for local providers. Default 8192.
- **`agent.prompt_cache_enabled`** (boolean) — Prompt caching: when true, keep the system-prompt PREFIX byte-stable turn-to-turn by moving per-turn retrieved context (memory + wiki) out of the system prompt and folding it into the current user message. A stable prefix lets cloud providers (Anthropic/OpenAI/Gemini) and local backends' KV cache reuse it — roughly 90% cheaper/faster input on cloud, a free speedup locally. Default false (unchanged prompt shape).
- **`agent.compaction`** (object) — Auto-compaction. When token budgeting (context_length_tokens > 0) is on and the oldest turns overflow the window, compact them into a rolling anchored summary instead of dropping them. Inert unless token budgeting is enabled, so defaults preserve current behaviour.
- **`agent.compaction.enabled`** (boolean) — Auto-compaction master switch. When token budgeting (context_length_tokens > 0) is on and the oldest turns overflow the window, compact them into a rolling anchored summary instead of dropping them. Inert unless token budgeting is enabled. Default true.
- **`agent.compaction.keep_last_turns`** (integer) — How many of the most recent turns (1 turn = user + assistant) are kept verbatim and never summarized. Default 6.
- **`agent.compaction.summary_model`** (string) — Model used to produce the summary. Empty = use the cheap classifier provider when configured, else the primary model. A named model is reserved for a future per-model resolver; today a non-empty value behaves the same as empty.
- **`agent.compaction.max_summary_tokens`** (integer) — Soft cap on the rolling summary's size in tokens, so the compacted block can't grow unbounded. Default 1024.
- **`agent.degeneracy_guard`** (object) — Degenerate-output guard: detects a model stuck emitting pathologically repetitive text (a wedged local model once produced ~8k tokens of '/'), aborts that generation early, and treats it as a failed turn + provider error so the garbage is neither shown to the user nor fanned into the memory/wiki extractors. Applies to every LLM call. Conservative defaults never trip on normal prose, code, JSON, long tables, or base64 blobs.
- **`agent.degeneracy_guard.enabled`** (boolean) — Degenerate-output guard: detects a model stuck emitting pathologically repetitive text (a wedged local model once produced ~8k tokens of '/'), aborts that generation early, and treats it as a failed turn + provider error so the garbage is neither shown nor fed to the memory/wiki extractors. Applies to every LLM call. Default true; turn off (or widen the thresholds) for a legitimate request to emit a long repeated pattern.
- **`agent.degeneracy_guard.min_chars`** (integer) — Minimum generated length (chars) before the guard can trip, so short legitimate repeats / small ASCII art survive. Default 400.
- **`agent.degeneracy_guard.window_chars`** (integer) — Size (chars) of the trailing window inspected for degeneracy. Default 512.
- **`agent.degeneracy_guard.min_distinct_chars`** (integer) — Char-collapse threshold: trip if the window has this many or fewer DISTINCT non-whitespace characters (with real substance). Default 1 (pure single-char runs like '////').
- **`agent.degeneracy_guard.min_window_tokens`** (integer) — Minimum whitespace-separated tokens in the window before the token-collapse check applies. Default 24.
- **`agent.degeneracy_guard.min_distinct_tokens`** (integer) — Token-collapse threshold: with at least min_window_tokens tokens, trip if this many or fewer are DISTINCT (a repeated word/phrase). Default 2.
- **`agent.avatar`** (string) — Assistant avatar. Encoded as 'preset:<key>' for bundled icons, or 'upload:<ext>' when a file exists at {data_dir}/avatars/agent.{ext}. Null = built-in MIRA logo.
- **`agent.avatar_updated_at`** (integer) — Unix-ms timestamp of the last avatar change. Used as a cache-buster on the web client.
- **`agent.llm_aliases`** (object) — Map of logical alias name -> (provider, model) tuple. Skill manifests pick a model by alias ('coding', 'fast', 'primary', ...) so site admins can swap providers globally without editing every Skill.
- **`agent.max_tool_round_tokens`** (integer) — Per-call token cap applied to non-streaming tool-loop rounds (where the model emits a structured tool call rather than prose). Tight on purpose: reasoning-distilled fine-tunes will happily loop on duplicated tool-call XML when given enough rope. Default 2048.
- **`agent.max_response_tokens`** (integer) — Per-call token cap applied to the streaming final-answer path. Set large enough that long markdown plans (or models that prepend reasoning before answering) don't truncate mid-sentence. Default 16384.
- **`agent.tool_selection`** (object) — Just-in-Time Tools — adaptive per-turn tool selection. When mode='adaptive', each turn carries only the tools it plausibly needs (core set + semantic top-K of the message + conversation-sticky tools) plus a find_tools meta-tool the model can call to load anything else on demand, instead of sending every enabled tool on every request. Default mode='all' preserves current behaviour.
- **`agent.tool_selection.mode`** (string; one of: `all`, `adaptive`) — 'all' (send every enabled tool — current behaviour) or 'adaptive' (semantic top-K + core set + stickiness + find_tools).
- **`agent.tool_selection.core_tools`** (array) — Tools always included even when unmatched. Supports trailing-* globs (e.g. 'memory_*'). Keeps flow-critical/baseline tools present.
- **`agent.tool_selection.top_k`** (integer) — Max number of semantically-matched tools to add per turn.
- **`agent.tool_selection.min_similarity`** (number) — Minimum cosine similarity (0.0-1.0) for a tool to be included by semantic match.
- **`agent.tool_selection.stickiness_turns`** (integer) — Tools used earlier in a conversation stay active for this many subsequent turns.
- **`agent.tool_selection.expose_find_tools`** (boolean) — Expose the find_tools meta-tool so the model can pull in any tool on demand (progressive disclosure). Recommended on.
- **`agent.tools`** (object) — Per-tool enable/disable switches. All tools are disabled by default for security.
- **`agent.tools.shell`** (object) — Shell command execution tool. Disabled by default. Enable only on trusted deployments.
- **`agent.tools.shell.enabled`** (boolean) — 
- **`agent.tools.filesystem`** (object) — File read/write tool. Disabled by default.
- **`agent.tools.filesystem.enabled`** (boolean) — 
- **`agent.tools.web_fetch`** (object) — Tier 2 web_fetch tool — retrieves a URL and returns readability-extracted text. Subject to the `security.http` SSRF and rate-limit policy.
- **`agent.tools.web_fetch.enabled`** (boolean) — 
- **`agent.tools.web_fetch.max_body_bytes`** (integer) — 
- **`agent.tools.web_fetch.max_text_chars`** (integer) — 
- **`agent.tools.web_fetch.timeout_secs`** (integer) — 
- **`agent.tools.web_fetch.max_redirects`** (integer) — 
- **`agent.tools.url_preview`** (object) — Tier 2 url_preview tool — pulls <title>, description and OpenGraph tags. Subject to `security.http`.
- **`agent.tools.url_preview.enabled`** (boolean) — 
- **`agent.tools.url_preview.max_body_bytes`** (integer) — 
- **`agent.tools.web_search`** (object) — Tier 2 web_search tool — supports DDG HTML scraping (no key), Brave Search API (key required), and a self-hosted SearXNG instance (URL required). Backends are tried in the order 'default' then 'failover' until one succeeds.
- **`agent.tools.web_search.enabled`** (boolean) — 
- **`agent.tools.web_search.default`** (string; one of: `ddg`, `brave`, `searxng`) — 
- **`agent.tools.web_search.failover`** (array) — 
- **`agent.tools.web_search.top_k`** (integer) — 
- **`agent.tools.web_search.brave`** (object) — 
- **`agent.tools.web_search.brave.api_key`** (string) — 
- **`agent.tools.web_search.searxng`** (object) — 
- **`agent.tools.web_search.searxng.url`** (string) — 
- **`agent.detail`** (object) — Agent detail-view page defaults. Controls the /agents/{id} live view: poll vs SSE, poll interval, and whether to prettify adapter stdout.
- **`agent.detail.view_mode`** (string; one of: `poll`, `sse`) — How the detail page receives updates. 'poll' = periodic GETs (simpler, default). 'sse' = streaming connection (sub-second updates on long-running agents).
- **`agent.detail.poll_interval_ms`** (integer) — Frontend poll interval in milliseconds. Only consulted when view_mode is 'poll'. Reasonable range 500–5000.
- **`agent.detail.prettify_output`** (boolean) — When true, the detail page JSON-prettifies stdout lines it recognises (claudecode stream-json, opencode events). False = render raw output.
- **`agent.show_thinking`** (boolean) — Render a 'Thinking' rollup on each assistant message in the chat UI containing the agent's tool calls, results, model reasoning blocks, and wiki context fetched for the turn. Server still collects + persists the events when false, so flipping back to true doesn't lose past activity.
- **`agent.session_budget_usd`** (number) — Shared USD budget across a root agent's whole multi-agent tree. When the combined LLM spend of all agents under a session exceeds this, work is cut off with a 'session_budget_exceeded' fault. Raise it for long research runs (this cap is the usual cause of a lengthy run failing). Default 5.0.
- **`agent.default_task_budget_usd`** (number) — Per-task USD budget assigned to a worker spawned via spawn_background_task when the caller omits one. Default 2.0.
- **`agent.max_task_budget_usd`** (number) — Hard ceiling a single spawned task's USD budget is clamped to. Default 10.0.
- **`agent.reasoning`** (object) — Reasoning-model auto-routing (roadmap #13). When enabled, hard turns are routed to a stronger reasoning provider instead of the default.
- **`agent.reasoning.enabled`** (boolean) — Master switch for reasoning auto-routing. Off by default.
- **`agent.reasoning.provider`** (string) — Provider id (as in providers.*) to route hard turns to; its model should be your strong reasoning model. Empty disables routing.
- **`agent.reasoning.min_chars`** (integer) — Input length (chars) at/above which a turn counts as a routing signal.
- **`agent.reasoning.effort`** (string; one of: `low`, `medium`, `high`) — Reasoning effort for the routed-to provider on hard turns. Sent as OpenAI reasoning_effort; mapped to an Anthropic thinking budget.
- **`agent.reasoning.classifier_provider`** (string) — Provider id for the cheap classifier consulted on ambiguous turns (hybrid fallback). Should be a small/fast model. Empty uses the default provider.

## guardian

_MIRA-Guardian — the built-in, code-defined system watchdog agent. Its identity (system prompt + tool allowlist) is immutable and lives in the binary; this only controls whether it runs and at what authority._

- **`guardian.mode`** (string; one of: `off`, `monitor`, `active`) — 'off' — disabled (default; opt-in). 'monitor' — observe and alert only, no actions. 'active' — monitor plus gated/isolation remediation actions. Identity is fixed regardless; only authority changes.
- **`guardian.watch_interval_secs`** (integer) — How often (seconds) the proactive watch loop checks the latest health snapshot and, on a new non-green state, fires a Guardian alert. Only active when mode != 'off'. Default 900 (15 min); minimum 60.
- **`guardian.action_approver_ids`** (array) — User ids permitted to approve/decline the Guardian's pending (system) actions in addition to admins. Lets an owner delegate approval of household fixes to a trusted adult without granting full admin. Empty (default) = admins only. Admins can always approve regardless.
- **`guardian.isolation_dry_run`** (boolean) — Isolation autonomy dry-run. When true (default), on detecting it cannot reach you (a configured channel's delivery failed) the Guardian only logs + audits what it would do — it does not execute. Set false to permit real autonomous remediation under isolation. Only relevant in 'active' mode.
- **`guardian.isolation_grace_secs`** (integer) — Grace period (seconds) after a failed approval delivery before the Guardian may act autonomously — a window for any web-side decision. Default 180 (3 min). Only relevant when isolation_dry_run = false.
- **`guardian.isolation_autonomy_kinds`** (array) — Staged autonomy opt-in: which bounded action kinds the Guardian may execute on its own under isolation once isolation_dry_run = false. A per-kind allowlist so an operator grants the harmless 'rerun_audit' first and the more-impactful 'restart_bridge' only when comfortable. Always intersected with the hard code ceiling (config can never widen autonomy beyond rerun_audit/restart_bridge); any eligible kind not listed falls back to observe-only. Default: just 'rerun_audit'.
- **`guardian.provision_model`** (string) — Ollama-registry model the provisioning flow pulls + binds the Guardian to when no local provider is configured, so a fresh install can run the Guardian without manual LLM setup. Default 'qwen2.5:3b-instruct'.
- **`guardian.routine_provider`** (string) — Tiered model: the local provider (lmstudio/ollama) for the light always-on routine tier used on low-severity ticks. Empty/absent = fall back to the 'guardian' llm-alias, then the primary provider. Still subject to the fail-closed local-only check (cloud refused).
- **`guardian.routine_model`** (string) — Tiered model: the model id on 'routine_provider' for the light routine tier. Empty/absent = the provider's/alias's default model.
- **`guardian.triage_provider`** (string) — Tiered model: the local provider (lmstudio/ollama) for the stronger triage tier, reached only when a detector goes Red. Empty/absent = fall back to the 'guardian' llm-alias, then the primary provider. Still subject to the fail-closed local-only check (cloud refused).
- **`guardian.triage_model`** (string) — Tiered model: the model id on 'triage_provider' for the stronger triage tier. Empty/absent = the provider's/alias's default model.
- **`guardian.process`** (object) — Out-of-process liveness sentinel (mira guardian-watch): a separate supervised process that probes MIRA's /health and raises a direct web-push alarm if MIRA goes down. Off by default.
- **`guardian.process.enabled`** (boolean) — Master switch for the out-of-process liveness sentinel (mira guardian-watch), a separate supervised process that probes MIRA's /health and raises a direct web-push alarm if MIRA goes down. Off by default; the operator enables + supervises it.
- **`guardian.process.probe_interval_secs`** (integer) — How often (seconds) the sentinel probes MIRA's liveness. Default 30; minimum 5.
- **`guardian.process.down_after_failures`** (integer) — Consecutive failed probes before declaring MIRA down + alarming. Default 3, so a normal restart doesn't alarm.
- **`guardian.process.probe_url`** (string) — Explicit liveness URL to probe. Empty/absent = derive http://127.0.0.1:<`server.port`>/health. Override for a non-default bind / reverse-proxy.
- **`guardian.process.notify_user_id`** (string) — User id whose registered push devices receive the 'MIRA is down' alarm. Empty/absent = no push target (sentinel still logs + audits). Set to the household admin so the phone buzzes.
- **`guardian.process.owns_watch`** (boolean) — When true, the out-of-process sentinel owns health watch + triage: it also triages non-green health while MIRA is up (surfacing through MIRA), and MIRA's co-resident watch loop stands down so the two don't double-alert. Default false (co-resident loop still owns health triage; sentinel watches liveness only). Requires enabled=true.
- **`guardian.process.log_file`** (string) — Separate log file for the out-of-process sentinel. Empty/absent = share MIRA's main log file (`logging.file`) so both processes' lines land together (the default). Set an explicit path to keep the sentinel's logs in their own file. '~' is expanded.

## security

_HTTP security policy applied by the middleware layer (rate limiting, CORS, IP blocking). Authentication tokens are configured under '`server.auth_token`'._

- **`security.rate_limit_rpm`** (integer) — Maximum requests per minute per IP address. Set to 0 to disable rate limiting.
- **`security.cors_allowed_origins`** (array) — CORS allowed origins. Example: ['https://my-app.example.com']. Empty array = deny all cross-origin requests.
- **`security.blocked_ips`** (array) — IP addresses permanently blocked from all endpoints. Checked before rate limiting.
- **`security.jwt_secret`** (string) — HS256 secret for signing/verifying JWT access tokens. Auto-generated on first run if omitted.
- **`security.session_days`** (integer) — Refresh token lifetime in days. Default: 7.
- **`security.http`** (object) — Outbound HTTP policy applied by Tier 2 web tools (SSRF guard, rate limits, denylist/allowlist). Independent from the inbound-traffic policy above.
- **`security.http.denylist`** (array) — Domains the policy layer refuses to reach. Exact match OR suffix at a dot boundary (e.g. 'example.com' also blocks 'api.example.com').
- **`security.http.allowlist`** (array) — When allowlist_only=true, only these domains may be reached.
- **`security.http.allowlist_only`** (boolean) — Paranoid mode: block everything not in the allowlist. Default false.
- **`security.http.searxng_exception`** (string) — Single 'host:port' bypass for a user-operated SearXNG on a private network. Only the private-IP block is relaxed; scheme, size, timeout and rate limits still apply.
- **`security.http.rate`** (object) — Per-user token-bucket limits for Tier 2 HTTP traffic.
- **`security.http.rate.user_per_min`** (integer) — 
- **`security.http.rate.user_per_hour`** (integer) — 
- **`security.http.rate.user_per_domain_per_min`** (integer) — 
- **`security.http.rate.search_per_min`** (integer) — 

## proxy

_nginx reverse proxy configuration. When enabled, MIRA generates an nginx.conf, binds its HTTP server to 127.0.0.1 only, and manages nginx as a subprocess._

- **`proxy.enabled`** (boolean) — Enable nginx reverse proxy management. Requires nginx to be installed on the system.
- **`proxy.nginx_binary`** (string) — Full path to the nginx binary. Use '/usr/sbin/nginx' on most Linux distributions.
- **`proxy.config_path`** (string) — Where MIRA writes the generated nginx.conf. Supports ~ expansion. Parent directory is created automatically.
- **`proxy.pid_path`** (string) — nginx PID file path. Used to detect running nginx and send reload signals. Supports ~ expansion.
- **`proxy.worker_processes`** (string) — nginx worker_processes directive. 'auto' lets nginx choose based on CPU cores.
- **`proxy.websocket_support`** (boolean) — Add nginx config for WebSocket proxying at /api/v1/stream. Required for the browser streaming client.
- **`proxy.tls`** (object) — TLS (HTTPS) configuration. nginx handles TLS termination; MIRA's internal HTTP server always uses plain HTTP.
- **`proxy.tls.enabled`** (boolean) — Enable TLS. Requires cert_path and key_path to be valid PEM files.
- **`proxy.tls.cert_path`** (string) — Path to the TLS certificate file in PEM format. Supports ~ expansion.
- **`proxy.tls.key_path`** (string) — Path to the TLS private key file in PEM format. Supports ~ expansion.
- **`proxy.tls.listen_port`** (integer) — HTTPS port nginx listens on.

## wiki

_Per-user wiki — markdown knowledge base companion to the structured memory DB. Stores narrative pages on disk under {data_dir}/wikis/users/<id>/._

- **`wiki.enabled`** (boolean) — Enable wiki context injection and auto-extraction. When false, the wiki directory is not created and no context is injected.
- **`wiki.auto_extract`** (object) — How the post-turn extractor that derives wiki pages from conversations behaves.
- **`wiki.auto_extract.mode`** (string; one of: `review`, `auto`, `off`) — 'review' (default, ChatGPT-lessons mitigation: ops land as pending until the user approves), 'auto' (apply immediately), or 'off' (extractor disabled).
- **`wiki.auto_extract.min_confidence`** (number) — Minimum extractor confidence in [0.0, 1.0]. Candidates below this threshold are dropped.
- **`wiki.auto_extract.max_ops_per_turn`** (integer) — Maximum number of wiki ops emitted per conversation turn. Keeps the wiki from getting noisy on busy days.
- **`wiki.auto_extract.auto_apply_above`** (number) — In mode='review', ops with extractor confidence at or above this threshold [0.0, 1.0] are applied immediately instead of waiting in the review queue; lower-confidence ops still land as pending. Defaults to 0.7 so confident extractions apply while uncertain ones still queue. Set to null to require review for every extracted op (the old behaviour; configs that already wrote null keep it). Ignored when mode is 'auto' or 'off'.
- **`wiki.agent_tools`** (object) — Controls the model-callable `wiki` skill (search/read are always on; writes are gated by write_mode).
- **`wiki.agent_tools.enabled`** (boolean) — When false, no wiki tools are registered — the model has no direct way to read or write the wiki (it still reads from context-injection).
- **`wiki.agent_tools.write_mode`** (string; one of: `review`, `auto`, `off`) — 'review' (default — agent writes land as pending until approved), 'auto' (writes apply immediately), or 'off' (write tools not registered; reads only).
- **`wiki.git`** (object) — Git-backed durability for the wiki. When enabled, each wiki is initialised as a git repo and every applied op can auto-commit.
- **`wiki.git.enabled`** (boolean) — Init <wiki_root>/.git on first startup and track changes.
- **`wiki.git.auto_commit`** (boolean) — Commit after every successful applied op. Push/pull stay manual.
- **`wiki.mcp`** (object) — Model Context Protocol server. When enabled, `mira wiki mcp-serve` exposes the user's wiki pages as MCP resources over stdio.
- **`wiki.mcp.enabled`** (boolean) — Enable the wiki MCP server.

## artifacts

_Where subagent-spawned task deliverables land. Each task gets a per-skill subdir under root_dir, named with a slug + task_id._

- **`artifacts.root_dir`** (string) — Filesystem path where task artifact directories are created. Supports ~ expansion. Default '~/mira-artifacts'.

## calendar

_Calendar integration. MIRA-native storage is always available; external sync is opt-in._

- **`calendar.enabled`** (boolean) — Enable MIRA-native calendar storage and agent tools.
- **`calendar.sync_provider`** (string; one of: `none`, `caldav`, `google`, `outlook`) — External source to mirror into the native calendar. 'none' keeps MIRA self-contained.
- **`calendar.sync_interval_mins`** (integer) — How often the sync engine polls the external source, in minutes. Floor is 5.
- **`calendar.google`** (object) — Google Calendar OAuth client settings. Only used when sync_provider = 'google'.
- **`calendar.google.client_id`** (string) — Google OAuth client id.
- **`calendar.google.client_secret`** (string) — Google OAuth client secret.
- **`calendar.google.redirect_uri`** (string) — OAuth redirect URI, must match the Google console.
- **`calendar.google.scopes`** (string) — Space- or comma-separated OAuth scopes.
- **`calendar.outlook`** (object) — Microsoft Graph OAuth client settings. Only used when sync_provider = 'outlook'.
- **`calendar.outlook.client_id`** (string) — Microsoft / Azure app client id.
- **`calendar.outlook.client_secret`** (string) — Microsoft / Azure app client secret.
- **`calendar.outlook.redirect_uri`** (string) — OAuth redirect URI, must match the Azure app registration.
- **`calendar.outlook.scopes`** (string) — Space- or comma-separated OAuth scopes.

## sandbox

_Tier 4 sandboxed code execution. Disabled by default. Requires a prebaked rootfs (see `mira sandbox install python`)._

- **`sandbox.enabled`** (boolean) — Master switch. When false, no Tier 4 tool is registered regardless of per-tool toggles.
- **`sandbox.seccomp_mode`** (string; one of: `denylist`, `allowlist`) — Which seccomp filter the backend installs. `allowlist` (default) permits only the syscalls a Python interpreter needs; `denylist` blocks a fixed set of escape primitives.
- **`sandbox.code_run`** (object) — Settings for the `code_run` agent tool.
- **`sandbox.code_run.enabled`** (boolean) — Enable the `code_run` tool. Both this and `sandbox.enabled` must be true.
- **`sandbox.code_run.allowed_languages`** (array) — Languages the tool will accept. Iteration B ships `python` only.
- **`sandbox.code_run.max_wall_clock_seconds`** (integer) — Hard ceiling on per-call wall-clock seconds.
- **`sandbox.code_run.max_memory_mb`** (integer) — Per-call memory cap (RLIMIT_AS) in megabytes.
- **`sandbox.python`** (object) — Python rootfs settings.
- **`sandbox.python.rootfs_path`** (string) — Override path for the python pivot root. Empty = use the default under <data_dir>/sandbox/rootfs/.
- **`sandbox.backend`** (string; one of: ``, `auto`, `namespace`, `wasm`, `pyodide`) — Code-execution backend: '' or 'auto' (namespace on Linux when a rootfs is installed, else the cross-platform WASM backend), 'namespace' (Linux seccomp/namespaces only), 'wasm' (WASM/WASI on all platforms incl. Windows/macOS), or 'pyodide' (scientific Python on Node as the primary backend).
- **`sandbox.wasm`** (object) — WASM/WASI sandbox settings (cross-platform code_run).
- **`sandbox.wasm.python_path`** (string) — Override path for the WASI CPython module. Empty = use the managed copy under <data_dir>/deps/wasm/.
- **`sandbox.pyodide`** (object) — Pyodide-on-Node scientific Python backend (numpy/pandas/matplotlib). When enabled, calls whose code imports a scientific package route here; plain scripts stay on the primary namespace/WASM backend.
- **`sandbox.pyodide.enabled`** (boolean) — Enable the Pyodide scientific backend. Off by default. First enable triggers a background download of the Pyodide distribution (~6 MB) plus the pre-warm wheels; available after the next restart.
- **`sandbox.pyodide.prewarm`** (array) — Packages to pre-warm into the on-disk wheel cache at provision time (offline-fast first run). Empty = the default trio: numpy, pandas, matplotlib.

## tts

_Text-to-Speech subsystem._

- **`tts.enabled`** (boolean) — Master switch for the TTS subsystem.
- **`tts.default_backend`** (string; one of: `internal`, `kokoro`, `chatterbox`, `openai`, `openai_compat`, `elevenlabs`, `cartesia`) — Backend used when no per-channel route or per-request override applies.
- **`tts.default_voice`** (string) — Voice id used when the request does not specify one. Empty = backend default.
- **`tts.default_speed`** (number) — Speech rate. 1.0 = natural; supported band 0.5..=2.0.
- **`tts.default_format`** (string; one of: `wav`, `mp3`, `ogg-opus`) — Encoder hint.
- **`tts.streaming`** (boolean) — Sentence-chunked streaming for chat. When true (default) the /api/tts/speak/stream endpoint emits one audio chunk per sentence so playback starts as soon as the first sentence is synthesised; when false it returns the full buffer as a single chunk (fewer synth calls, higher time-to-first-audio).
- **`tts.max_chars_per_request`** (integer) — Safety cap on a single TTS call.
- **`tts.request_timeout_secs`** (integer) — Per-request timeout in seconds.
- **`tts.cache`** (object) — Two-layer (LRU memory + disk) audio cache.
- **`tts.cache.enabled`** (boolean) — Enable caching of synthesised audio.
- **`tts.cache.max_disk_mb`** (integer) — Disk-cache cap in megabytes.
- **`tts.cache.ttl_days`** (integer) — Cache entries older than this are swept on startup.
- **`tts.internal`** (object) — Out-of-the-box local engine (Piper subprocess + voice auto-download).
- **`tts.internal.engine`** (string; one of: `piper`, `espeak`, `kokoro`) — Internal engine.
- **`tts.internal.voices_dir`** (string) — Override for <data_dir>/tts/voices. Empty = derive.
- **`tts.internal.default_voice`** (string) — Default Piper voice id.
- **`tts.internal.auto_download_voices`** (boolean) — Whether the backend may fetch voice files on first use.
- **`tts.internal.binary_path`** (string) — Override for the Piper executable. Empty = derive.
- **`tts.internal.volume`** (number) — Web playback gain. 1.0 = unaltered, 2.0 = doubled.
- **`tts.kokoro`** (object) — K1 (Q2 #10) — native in-process Kokoro-82M backend (any-tts / Candle). Active only in a build with the 'kokoro' feature. American/British English.
- **`tts.kokoro.enabled`** (boolean) — Register the Kokoro backend. Defaults off; the model loads lazily on first use.
- **`tts.kokoro.model_path`** (string) — Override for <data_dir>/tts/kokoro/Kokoro-82M. Empty = derive.
- **`tts.kokoro.default_voice`** (string) — Kokoro preset voice id (e.g. af_heart, am_michael, bf_emma).
- **`tts.kokoro.auto_download`** (boolean) — Pull missing weights from HuggingFace on first use.
- **`tts.kokoro.device`** (string; one of: `auto`, `cpu`, `cuda`, `metal`) — Compute device. CUDA/Metal require the matching any-tts GPU build feature; otherwise they degrade to CPU.
- **`tts.chatterbox`** (object) — K3 (Q2 #10) — Chatterbox AMD Vulkan TTS server integration. OpenAI-compatible server, fast on AMD Radeon via Vulkan.
- **`tts.chatterbox.enabled`** (boolean) — Register the `chatterbox` TTS backend (client → http://127.0.0.1:{port}/v1).
- **`tts.chatterbox.port`** (integer) — Local port the Chatterbox server listens on.
- **`tts.chatterbox.default_voice`** (string) — Default Chatterbox preset voice (e.g. Adrian).
- **`tts.chatterbox.supervise`** (boolean) — Have MIRA spawn + health-check + restart the server process. Same-host only.
- **`tts.chatterbox.binary_path`** (string) — Path to the Chatterbox server executable. Required when supervise = true.
- **`tts.chatterbox.extra_args`** (array) — Extra args passed to the server on spawn (model paths, `--config`, …).
- **`tts.openai`** (object) — OpenAI cloud TTS (api.openai.com).
- **`tts.openai.api_key`** (string) — API key. null = fall back to `providers.openai.api_key`, then OPENAI_API_KEY env.
- **`tts.openai.base_url`** (string) — OpenAI-compatible base URL.
- **`tts.openai.model`** (string) — Model id.
- **`tts.openai.default_voice`** (string) — Default voice id (alloy, echo, fable, onyx, nova, shimmer, …).
- **`tts.openai.volume`** (number) — Web playback gain. 1.0 = unaltered, 2.0 = doubled.
- **`tts.openai_compat`** (object) — Self-hosted OpenAI-compatible TTS (Piper daemon, OpenedAI-Speech, LiteLLM, …).
- **`tts.openai_compat.url`** (string) — Server base URL.
- **`tts.openai_compat.api_key`** (string) — Optional bearer token.
- **`tts.openai_compat.model`** (string) — Model id forwarded to the server.
- **`tts.openai_compat.default_voice`** (string) — Default voice id.
- **`tts.openai_compat.volume`** (number) — Web playback gain. 1.0 = unaltered, 2.0 = doubled.
- **`tts.elevenlabs`** (object) — ElevenLabs cloud TTS (WebSocket).
- **`tts.elevenlabs.api_key`** (string) — ElevenLabs API key.
- **`tts.elevenlabs.model`** (string) — Model id (eleven_turbo_v2_5, eleven_flash_v2_5, …).
- **`tts.elevenlabs.default_voice_id`** (string) — Default voice id.
- **`tts.elevenlabs.volume`** (number) — Web playback gain. 1.0 = unaltered, 2.0 = doubled.
- **`tts.cartesia`** (object) — Cartesia Sonic cloud TTS (WebSocket, sub-100 ms first-byte).
- **`tts.cartesia.api_key`** (string) — Cartesia API key.
- **`tts.cartesia.model`** (string) — Model id (sonic-english, …).
- **`tts.cartesia.default_voice_id`** (string) — Default voice id.
- **`tts.cartesia.volume`** (number) — Web playback gain. 1.0 = unaltered, 2.0 = doubled.
- **`tts.routing`** (object) — Per-channel backend pinning. Each value is a backend id or 'internal'.
- **`tts.routing.web`** (string) — Backend used for the web chat 🔊 button.
- **`tts.routing.tui`** (string) — Backend used for /speak in the terminal UI.
- **`tts.routing.telegram`** (string) — Backend used for Telegram voice notes.
- **`tts.routing.signal`** (string) — Backend used for Signal voice messages.
- **`tts.routing.mobile`** (string) — Backend used for the native mobile app's voice playback.
- **`tts.voice_prefs`** (object) — Server-default per-channel voice prefs (response policy + voice id). Each user can override per-channel from their profile. Keys are channel ids (web, tui, telegram, signal, mobile, or any plugin-registered channel). Missing channels fall back to 'never' / no voice id.

## stt

_Speech-to-Text subsystem. Mirrors the TTS layout: pluggable backends behind a routing service, with a default internal whisper.cpp engine and optional OpenAI / OpenAI-compatible cloud or self-hosted backends._

- **`stt.enabled`** (boolean) — Master switch for the STT subsystem.
- **`stt.default_backend`** (string; one of: `internal`, `openai`, `openai_compat`) — Backend used when no per-channel route or per-request override applies.
- **`stt.default_language`** (string) — BCP-47 language hint (e.g. 'en', 'de'). Empty = let the backend auto-detect.
- **`stt.max_audio_seconds`** (integer) — Hard cap on a single transcription's audio length.
- **`stt.request_timeout_secs`** (integer) — Per-request timeout against the backend.
- **`stt.internal`** (object) — Out-of-the-box local engine (whisper.cpp via the whisper-rs FFI).
- **`stt.internal.model`** (string) — Whisper.cpp ggml model id ('tiny.en', 'base.en', 'small.en', 'tiny', 'base', 'small', 'medium', 'medium.en', 'large-v3').
- **`stt.internal.models_dir`** (string) — Override for <data_dir>/stt/models. Empty = derive.
- **`stt.internal.auto_download_model`** (boolean) — Auto-fetch the model file from huggingface.co on first use.
- **`stt.internal.threads`** (integer) — Inference threads. 0 = use num_cpus.
- **`stt.internal.use_gpu`** (boolean) — Run the encoder on GPU when whisper-rs was built with a GPU feature.
- **`stt.openai`** (object) — OpenAI cloud STT (/v1/audio/transcriptions).
- **`stt.openai.api_key`** (string) — API key. null = fall back to `providers.openai.api_key`, then OPENAI_API_KEY env.
- **`stt.openai.base_url`** (string) — OpenAI-compatible base URL.
- **`stt.openai.model`** (string) — Model id (typically 'whisper-1').
- **`stt.openai_compat`** (object) — Self-hosted OpenAI-compatible STT (whisper.cpp HTTP server, faster-whisper-server, OpenedAI-Speech, …).
- **`stt.openai_compat.url`** (string) — Server base URL (must include /v1).
- **`stt.openai_compat.api_key`** (string) — Optional bearer token.
- **`stt.openai_compat.model`** (string) — Model id forwarded to the server.
- **`stt.routing`** (object) — Per-channel backend pinning for inbound voice ingest.
- **`stt.routing.web`** (string) — Backend used for the web chat record button.
- **`stt.routing.tui`** (string) — Backend used for the TUI (future).
- **`stt.routing.telegram`** (string) — Backend used for Telegram voice notes.
- **`stt.routing.signal`** (string) — Backend used for Signal voice messages.

## mcp

_MCP host registry. External Model Context Protocol servers MIRA connects to at startup; each server's tools are exposed under mcp__<server_name>__<tool_name>._

- **`mcp.servers`** (array) — Per-server entries. Empty list disables MCP entirely.

## email_oauth

_OAuth client config for Gmail + Outlook email accounts (Q2 #8 E4). Operator brings their own Google Cloud and Azure OAuth apps; MIRA only needs the public client_ids and a publicly-reachable callback URL. PKCE flow — no client_secret stored._

- **`email_oauth.public_base_url`** (string) — Origin (scheme + host + port) MIRA serves on, as reachable from the user's browser. Used to build the OAuth redirect URI; must match the value registered at the provider exactly. Defaults to http://127.0.0.1:<`server.port`> at runtime when empty.
- **`email_oauth.google_client_id`** (string) — Google Cloud OAuth client_id (Desktop app type) for the Gmail integration. Empty disables 'Connect Gmail' in the UI.
- **`email_oauth.microsoft_client_id`** (string) — Microsoft Entra ID OAuth client_id (Public client) for the Outlook / Microsoft 365 integration. Empty disables 'Connect Outlook' in the UI.

## auth

_Authentication config. Today only SSO via OpenID Connect (Q2 #11). Local username/password always works regardless._

- **`auth.oidc`** (object) — SSO via OpenID Connect. Generic + discovery-driven (one shape covers Google / Microsoft Entra / Keycloak / Authentik / Okta). Off by default; when enabled with a configured provider, the web login page shows a 'Sign in with …' button. Changing providers requires a restart.
- **`auth.oidc.enabled`** (boolean) — Master switch. Even with providers listed, OIDC is inert unless true.
- **`auth.oidc.public_base_url`** (string) — Origin (scheme + host + port) MIRA is reachable on from the user's browser. Used to build the redirect URI (<base>/api/auth/oidc/callback), which must match the value registered at the IdP exactly. Empty defaults to http://127.0.0.1:<`server.port`>.
- **`auth.oidc.providers`** (array) — Configured identity providers. Each renders a login button.
- **`auth.signup`** (object) — Self-service onboarding. Admin invite links always work; this governs open (un-invited) signup. Invite-only by default.
- **`auth.signup.enabled`** (boolean) — Allow anyone to create an account from the signup page without an invite. Off by default.
- **`auth.signup.require_approval`** (boolean) — Open-signup accounts land pending until an admin approves them. On by default.
- **`auth.signup.default_role`** (string) — Role granted to open-signup accounts: 'user' (default) or 'admin'.
- **`auth.signup.allowed_domains`** (array) — When set, open signup only accepts these email domains (lowercase, no '@'). Empty = any domain.
- **`auth.ldap`** (object) — LDAP / Active Directory auth. Tried as a fallback when local password auth fails, so local accounts (incl. the bootstrap admin) keep working and a directory outage never locks everyone out. Search-then-bind. Off by default; provider change needs a restart.
- **`auth.ldap.enabled`** (boolean) — Master switch.
- **`auth.ldap.url`** (string) — Directory URL, e.g. ldap://dc.example.com:389 or ldaps://dc.example.com:636.
- **`auth.ldap.starttls`** (boolean) — Upgrade a plaintext ldap:// connection to TLS via STARTTLS before binding. Ignored for ldaps://.
- **`auth.ldap.bind_dn`** (string) — Service-account DN used to search for the user before binding as them. Empty = anonymous search.
- **`auth.ldap.bind_password`** (string) — Password for bind_dn.
- **`auth.ldap.user_base_dn`** (string) — Base DN to search under, e.g. ou=people,dc=example,dc=com.
- **`auth.ldap.user_filter`** (string) — User search filter; {username} is substituted (LDAP-escaped). Default (uid={username}); AD usually (sAMAccountName={username}).
- **`auth.ldap.attr_email`** (string) — Attribute carrying the user's email. Default 'mail'.
- **`auth.ldap.attr_display_name`** (string) — Attribute carrying the display name. Default 'cn'.
- **`auth.ldap.required_group`** (string) — If set, the user must be a member of this group DN (checked via memberOf). Empty = no requirement.
- **`auth.ldap.auto_provision`** (boolean) — Create a MIRA account on first successful LDAP login for a user with no existing match. Off by default.
- **`auth.ldap.allowed_domains`** (array) — When auto-provisioning, only allow these email domains. Empty = any.
- **`auth.ldap.default_role`** (string) — Role for auto-provisioned users: 'user' (default) or 'admin'.

## system_email

_System email account (Q2 #8 E5). Application-initiated outbound — distinct from per-user email_accounts. Disabled by default; configure when a feature needs MIRA to send mail as itself (password reset, admin alerts, waitlist confirmations)._

- **`system_email.enabled`** (boolean) — 
- **`system_email.from_address`** (string) — From: address (e.g. mira@example.com).
- **`system_email.from_name`** (string) — Display name in the From header. Defaults to 'MIRA' when empty.
- **`system_email.smtp_host`** (string) — 
- **`system_email.smtp_port`** (integer) — 
- **`system_email.smtp_use_tls`** (boolean) — When true, port 465 uses implicit TLS, anything else uses STARTTLS. Plaintext is rejected.
- **`system_email.smtp_username`** (string) — 
- **`system_email.smtp_password`** (string) — 

## backup

_Backup behaviour. The on-demand download/upload (Q1.5 since 0.148.1) is always available regardless; this section only gates the scheduled nightly snapshot loop and its retention/rotation._

- **`backup.scheduled_enabled`** (boolean) — Run a scheduled snapshot loop in the background. Off by default. When on, writes <data_dir>/backups/mira-backup-…tar.gz every scheduled_interval_secs and keeps scheduled_retention_count most-recent files. Toggling this requires a service restart.
- **`backup.scheduled_interval_secs`** (integer) — Interval between scheduled snapshots, in seconds. Default 86400 = once a day. Minimum 60 (anything lower is treated as 60).
- **`backup.scheduled_retention_count`** (integer) — How many scheduled snapshots to retain on disk. Older snapshots are pruned after each write. 0 = keep all (use with care). Default 7.

## notifications

_Proactive-notification transports. Web Push (VAPID) is always available; this section adds opt-in Firebase Cloud Messaging for the native mobile app. Off by default._

- **`notifications.fcm`** (object) — Firebase Cloud Messaging (HTTP v1). Off by default. When enabled, the notification dispatcher also delivers proactive events to registered FCM device tokens.
- **`notifications.fcm.enabled`** (boolean) — Master switch for Firebase Cloud Messaging (push to the native mobile app). false → behaves exactly as before (web push only).
- **`notifications.fcm.project_id`** (string) — Firebase project id (the project_id field in the service-account JSON). Required when enabled.
- **`notifications.fcm.service_account_json_path`** (string) — Filesystem path to the Google service-account JSON used to mint OAuth2 access tokens. Required when enabled. The file is a secret — keep it readable only by the MIRA process user.

## weather

_Built-in weather tool. Defaults to keyless Open-Meteo (global, no API key, includes geocoding). An admin can switch to a keyed provider for an alternative source._

- **`weather.provider`** (string; one of: `open_meteo`, `openweathermap`) — Weather data source for the built-in weather tool. 'open_meteo' (default) is free, global, and keyless (includes geocoding). 'openweathermap' requires `weather.api_key`.
- **`weather.api_key`** (string) — API key for keyed providers (openweathermap). Secret — redacted on config read. Null for keyless Open-Meteo.
- **`weather.units`** (string; one of: `metric`, `imperial`) — 'metric' (°C, mm, km/h — default) or 'imperial' (°F, in, mph).

## image

_Image-generation backends for the image_generate tool. The OpenAI backend uses `providers.openai`; local Automatic1111 / ComfyUI are configured here._

- **`image.default_backend`** (string) — Default backend. '' or 'auto' = first enabled (local preferred). One of: openai | automatic1111 | comfyui.
- **`image.openai`** (object) — OpenAI Images backend. Key + endpoint come from `providers.openai`; this picks the model.
- **`image.openai.default_model`** (string) — OpenAI image model, e.g. dall-e-3 or gpt-image-1. Key + endpoint come from `providers.openai`.
- **`image.automatic1111`** (object) — Local Automatic1111 / Stable Diffusion WebUI backend (/sdapi/v1/txt2img).
- **`image.automatic1111.enabled`** (boolean) — Enable the local Automatic1111 / SD WebUI backend (needs the server launched with `--api --listen`).
- **`image.automatic1111.base_url`** (string) — WebUI base URL, e.g. http://127.0.0.1:7860 (or http://windows-host:7860 from WSL).
- **`image.automatic1111.model`** (string) — Optional checkpoint to switch to per call (override_settings.sd_model_checkpoint). Empty = leave the WebUI's current model.
- **`image.automatic1111.steps`** (integer) — Sampling steps.
- **`image.automatic1111.sampler`** (string) — Sampler name, e.g. 'Euler a'.
- **`image.automatic1111.width`** (integer) — Default image width.
- **`image.automatic1111.height`** (integer) — Default image height.
- **`image.automatic1111.cfg_scale`** (number) — CFG scale (prompt adherence).
- **`image.automatic1111.negative_prompt`** (string) — Default negative prompt when a call doesn't pass one.
- **`image.comfyui`** (object) — Local ComfyUI backend (POST /prompt → poll /history → fetch /view).
- **`image.comfyui.enabled`** (boolean) — Enable the ComfyUI backend.
- **`image.comfyui.base_url`** (string) — ComfyUI base URL, e.g. http://127.0.0.1:8188.
- **`image.comfyui.workflow_json`** (string) — Optional ComfyUI API-format workflow with placeholder tokens ({{prompt}} {{negative}} {{seed}} {{width}} {{height}} {{steps}} {{cfg}} {{ckpt}}). Empty = built-in default SD txt2img workflow.
- **`image.comfyui.model`** (string) — Checkpoint filename for the default workflow's {{ckpt}}. Empty = auto-pick the first available.
- **`image.comfyui.steps`** (integer) — Sampling steps.
- **`image.comfyui.width`** (integer) — Default image width.
- **`image.comfyui.height`** (integer) — Default image height.
- **`image.comfyui.cfg_scale`** (number) — CFG scale.
- **`image.comfyui.negative_prompt`** (string) — Default negative prompt.

## video

_Video-generation backends for the video_generate tool: OpenAI Videos / Sora, local ComfyUI (a video workflow), or local WAN2GP._

- **`video.default_backend`** (string) — Default backend. '' / 'auto' = first enabled (local preferred). One of: openai | comfyui | wan2gp.
- **`video.openai`** (object) — OpenAI Videos (Sora) settings. Key + endpoint from `providers.openai`.
- **`video.openai.default_model`** (string) — Default video model, e.g. sora-2 or sora-2-pro.
- **`video.openai.default_size`** (string) — Default frame size WIDTHxHEIGHT (larger sizes need sora-2-pro).
- **`video.openai.default_seconds`** (integer) — Default clip length in seconds.
- **`video.comfyui`** (object) — Local ComfyUI video backend. Requires a video workflow (no universal default).
- **`video.comfyui.enabled`** (boolean) — Enable the ComfyUI video backend.
- **`video.comfyui.base_url`** (string) — ComfyUI base URL, e.g. http://127.0.0.1:8188.
- **`video.comfyui.workflow_json`** (string) — REQUIRED video workflow (API format) with tokens {{prompt}} {{negative}} {{seed}} {{width}} {{height}} {{frames}} {{fps}} {{steps}} {{cfg}} {{ckpt}}.
- **`video.comfyui.model`** (string) — Checkpoint/model for {{ckpt}} (workflow-dependent).
- **`video.comfyui.steps`** (integer) — Sampling steps.
- **`video.comfyui.width`** (integer) — Default width.
- **`video.comfyui.height`** (integer) — Default height.
- **`video.comfyui.fps`** (integer) — Frames per second; {{frames}} = seconds × fps.
- **`video.comfyui.cfg_scale`** (number) — CFG scale.
- **`video.comfyui.negative_prompt`** (string) — Default negative prompt.
- **`video.wan2gp`** (object) — Local WAN2GP (Gradio) video backend.
- **`video.wan2gp.enabled`** (boolean) — Enable the WAN2GP backend.
- **`video.wan2gp.base_url`** (string) — Gradio app base URL, e.g. http://127.0.0.1:7862.
- **`video.wan2gp.api_name`** (string) — Gradio API endpoint name to call (e.g. /generate_video), from the app's /config.
