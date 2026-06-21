// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel_accounts/models.rs
//! Data types for per-user channel accounts.

use serde::{Deserialize, Serialize};

use crate::MiraError;

// ── ChannelKind ───────────────────────────────────────────────────────────────

/// Which messaging channel an account belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Signal,
    Telegram,
    /// Discord — per-user bot (each MIRA user registers their own Discord
    /// application + bot token, invites it to their servers / starts DMs
    /// with it). Inbound via persistent WebSocket gateway connection;
    /// outbound via REST. See `src/channels/discord/`.
    Discord,
    /// Matrix — per-user (or shared) bot against a homeserver. Inbound via
    /// HTTP long-poll on the Client-Server `/sync` endpoint; outbound via
    /// REST. The account carries a homeserver URL + access token. See
    /// `src/matrix/`.
    Matrix,
    /// WhatsApp — via the Meta WhatsApp Business Cloud API. Inbound via a
    /// webhook Meta POSTs to `/webhook/whatsapp/{account_id}`; outbound via
    /// the Graph API. The account carries a phone_number_id + access token
    /// + app secret + verify token. See `src/whatsapp/`.
    WhatsApp,
    /// Slack — via the Events API. Inbound via a webhook Slack POSTs to
    /// `/webhook/slack/{account_id}`; outbound via the Web API. The account
    /// carries a bot token + signing secret. See `src/slack/`.
    Slack,
    /// External — a Channel Provider Protocol (CPP) plugin channel. An
    /// external provider process owns the native transport; MIRA proxies
    /// inbound (`/webhook/external/{id}`) + outbound (to the provider's
    /// send_url) over signed HTTP. The `channel` string is
    /// `external:<provider_kind>`. See `src/external/` + the CPP spec.
    External,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Signal   => "signal",
            ChannelKind::Telegram => "telegram",
            ChannelKind::Discord  => "discord",
            ChannelKind::Matrix   => "matrix",
            ChannelKind::WhatsApp => "whatsapp",
            ChannelKind::Slack    => "slack",
            ChannelKind::External => "external",
        }
    }
}

impl std::str::FromStr for ChannelKind {
    type Err = MiraError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "signal"   => Ok(ChannelKind::Signal),
            "telegram" => Ok(ChannelKind::Telegram),
            "discord"  => Ok(ChannelKind::Discord),
            "matrix"   => Ok(ChannelKind::Matrix),
            "whatsapp" => Ok(ChannelKind::WhatsApp),
            "slack"    => Ok(ChannelKind::Slack),
            "external" => Ok(ChannelKind::External),
            other      => Err(MiraError::ConfigError(format!(
                "Unknown channel kind: {}", other
            ))),
        }
    }
}

// ── Per-channel config blobs ──────────────────────────────────────────────────

/// Signal-cli per-account daemon settings. Persisted as JSON in `config_json`
/// and rehydrated when the gateway starts. `rest_port` is auto-assigned by the
/// `ChannelManager` on first launch and saved back so the daemon binds to the
/// same port on subsequent restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalAccountConfig {
    pub phone_number: String,
    /// Port assigned by the runtime; `None` until first start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest_port:    Option<u16>,
    #[serde(default = "default_signal_binary")]
    pub cli_binary:   String,
    #[serde(default = "default_signal_data_dir")]
    pub data_dir:     String,
    /// HMAC-SHA256 key for the legacy `/webhook/signal` path. `None` =
    /// signature verification disabled (warn at startup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac_key:     Option<String>,
}

fn default_signal_binary()  -> String { "signal-cli".to_string() }
fn default_signal_data_dir() -> String {
    // Match the legacy default in `config::SignalConfig`. Each daemon needs
    // its own data dir, so `ChannelManager` will append the account id when
    // launching multiple Signal accounts on the same host.
    "~/.local/share/signal-cli".to_string()
}

/// Telegram per-account bot settings. Persisted as JSON in `config_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramAccountConfig {
    pub bot_token:    String,
    /// `polling` (default) or `webhook`. Polling spawns a long-poll task per
    /// account (`getUpdates`) and works anywhere — behind NAT, on localhost, no
    /// public URL needed — so it's the right default for self-hosted installs.
    /// Webhook mode receives via the shared `/webhook/telegram/{account_id}`
    /// endpoint and requires a public HTTPS URL Telegram can reach (production
    /// deployments behind a reverse proxy).
    #[serde(default = "default_tg_mode")]
    pub mode:         String,
    /// Value sent by Telegram in the `X-Telegram-Bot-Api-Secret-Token` header.
    /// Only meaningful in `webhook` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_token: Option<String>,
    /// Long-poll hold time in seconds passed to Telegram's `getUpdates`
    /// (only meaningful in `polling` mode). Default 30 — Telegram caps
    /// at 50. Lower values shorten the worst-case poll-loop teardown
    /// time on shutdown but cost more requests when idle.
    #[serde(default = "default_poll_timeout_secs")]
    pub poll_timeout_secs: u64,
}

fn default_tg_mode() -> String { "polling".to_string() }
fn default_poll_timeout_secs() -> u64 { 30 }

/// Discord per-account bot settings. Persisted as JSON in `config_json`.
///
/// `bot_token` is the secret from Discord Developer Portal → Application
/// → Bot → "Reset Token". `application_id` is the Application ID from
/// "General Information" — we use it to skip the bot's own MESSAGE_CREATE
/// events (Discord echoes them back over the gateway).
///
/// `mention_only`: when true, MIRA only responds when the bot is @-mentioned
/// in the message content (recommended for shared servers; default for DMs).
/// When false, MIRA responds to every message in every channel the bot can
/// see — appropriate for personal-server deployments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordAccountConfig {
    pub bot_token:      String,
    /// Discord application snowflake (numeric string). Optional — when
    /// missing we still de-dup against `author.bot` but won't suppress
    /// the bot's own messages on first-message turns before READY fires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    /// If true, only respond when the bot is mentioned in the message text.
    /// Default false (respond to everything the bot can see).
    #[serde(default)]
    pub mention_only:   bool,
}

/// Matrix per-account settings. The bot authenticates with a long-lived
/// `access_token` against `homeserver` (e.g. "https://matrix.org"). Inbound
/// is an HTTP `/sync` long-poll; outbound is a REST PUT. No application id
/// needed — `/whoami` resolves the bot's own MXID at connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixAccountConfig {
    /// Homeserver base URL, e.g. `https://matrix.org` or a self-hosted
    /// `https://matrix.example.com`. The client API path is appended by
    /// the transport.
    pub homeserver:   String,
    /// Long-lived access token (Element → Settings → Help & About →
    /// Advanced → Access Token, or one minted via /login).
    pub access_token: String,
    /// If true, only respond to messages that mention the bot's MXID or
    /// localpart. Recommended for shared/group rooms. Default false.
    #[serde(default)]
    pub mention_only: bool,
}

/// WhatsApp per-account settings (Meta WhatsApp Business Cloud API).
/// Inbound arrives via a webhook Meta POSTs to
/// `/webhook/whatsapp/{account_id}`; outbound goes to the Graph API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhatsAppAccountConfig {
    /// Cloud API phone-number id (from the Meta app's WhatsApp → API
    /// Setup page). Messages are sent *from* this id.
    pub phone_number_id: String,
    /// Permanent (system-user) access token with `whatsapp_business_messaging`.
    pub access_token:    String,
    /// App secret used to verify the `X-Hub-Signature-256` header on
    /// inbound webhooks. None disables verification (not recommended;
    /// warned at startup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_secret:      Option<String>,
    /// Token MIRA echoes back during the GET subscription handshake. You
    /// set the same value in the Meta webhook config.
    pub verify_token:    String,
    /// If true, only respond to messages containing the word "mira"
    /// (useful in WhatsApp group chats). Default false.
    #[serde(default)]
    pub mention_only:    bool,
}

/// External (CPP) per-account settings. `inbound_secret` + `outbound_secret`
/// are generated by MIRA on account creation (the operator copies them into
/// the provider). See `design-docs/channel-provider-protocol.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalAccountConfig {
    /// Provider slug, e.g. `nctalk`. Namespaces the `external:<kind>`
    /// channel string + identity links.
    pub provider_kind:   String,
    /// Where MIRA POSTs outbound replies (the provider's CPP endpoint).
    pub send_url:        String,
    /// HMAC key the provider signs inbound webhooks with. Auto-generated.
    #[serde(default)]
    pub inbound_secret:  String,
    /// HMAC key MIRA signs outbound calls with. Auto-generated.
    #[serde(default)]
    pub outbound_secret: String,
    /// Only respond to messages containing "mira" when true. Default false.
    #[serde(default)]
    pub mention_only:    bool,
    /// Whether the provider can play synthesized audio (so MIRA should
    /// offer voice for this `external:<kind>` channel + include audio on
    /// outbound CPP calls). The provider decides this — MIRA can't know if
    /// e.g. Nextcloud Talk supports voice. Default false (text-only).
    #[serde(default)]
    pub supports_voice:  bool,
}

/// Slack per-account settings (Events API). Inbound arrives via a webhook
/// Slack POSTs to `/webhook/slack/{account_id}`; outbound goes to the Web
/// API `chat.postMessage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackAccountConfig {
    /// Bot User OAuth token (`xoxb-…`) — needs at least `chat:write`.
    pub bot_token:      String,
    /// App signing secret (Basic Information → App Credentials). Used to
    /// verify the `X-Slack-Signature` on inbound events.
    pub signing_secret: String,
    /// If true, only respond to messages containing the word "mira"
    /// (useful in busy Slack channels). Default false.
    #[serde(default)]
    pub mention_only:   bool,
}

// ── Routing mode (R1+R2) ──────────────────────────────────────────────────────

/// How an inbound message picks the MIRA user the agent runs as.
///
/// * `Personal` — every inbound runs as the bot owner. The default; matches
///   the "one bot per MIRA user" model Signal/Telegram/Discord all shipped
///   with originally. Safe and simple; doesn't scale to multi-tenant.
///
/// * `Shared` — every inbound looks up `(channel, sender_external_id)` in
///   the `user_channel_links` table and runs as that MIRA user. Senders
///   not in the link table get a one-line "you need to link first; use
///   this code: LINK-XXXX" reply. Lets one admin-managed bot serve many
///   MIRA users.
///
/// * `GuestOk` — same lookup, but unmapped senders fall through to a
///   shared "guest" agent identity instead of being told to link. Useful
///   for public help-desk bots that should respond to anyone but with a
///   limited capability set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    Personal,
    Shared,
    GuestOk,
}

impl RoutingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RoutingMode::Personal => "personal",
            RoutingMode::Shared   => "shared",
            RoutingMode::GuestOk  => "guest_ok",
        }
    }
}

impl Default for RoutingMode {
    fn default() -> Self { RoutingMode::Personal }
}

impl std::str::FromStr for RoutingMode {
    type Err = MiraError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "personal" => Ok(RoutingMode::Personal),
            "shared"   => Ok(RoutingMode::Shared),
            "guest_ok" => Ok(RoutingMode::GuestOk),
            other      => Err(MiraError::ConfigError(format!(
                "Unknown routing_mode: {}", other
            ))),
        }
    }
}

// ── Stored row ────────────────────────────────────────────────────────────────

/// A single channel account row as persisted in `auth.db`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelAccount {
    pub id:            String,
    pub user_id:       String,
    pub channel:       ChannelKind,
    pub account_label: String,
    /// External identifier (signal phone, telegram bot username). Nullable
    /// because we may not know it until first contact, but the unique
    /// constraint prevents two accounts from claiming the same id once set.
    pub external_id:   Option<String>,
    /// Channel-specific config blob, deserialised by callers based on
    /// `channel`.
    pub config_json:   String,
    pub enabled:       bool,
    /// How inbound messages pick the MIRA user. Defaults to `Personal` for
    /// every row created before R1+R2 shipped (set by the migration's
    /// `DEFAULT 'personal'` clause) so existing single-user bots keep their
    /// trust model. Admins can flip a bot to `Shared` in the Channels UI.
    #[serde(default)]
    pub routing_mode:  RoutingMode,
    pub created_at:    i64,
    pub updated_at:    i64,
}

impl ChannelAccount {
    /// Decode `config_json` into the Signal-specific config. Returns an error
    /// if `channel` is not `Signal` or the JSON is malformed.
    pub fn signal_config(&self) -> Result<SignalAccountConfig, MiraError> {
        if self.channel != ChannelKind::Signal {
            return Err(MiraError::ConfigError(format!(
                "Account {} is not a Signal account", self.id
            )));
        }
        serde_json::from_str(&self.config_json).map_err(|e| {
            MiraError::ConfigError(format!("Bad signal config for {}: {}", self.id, e))
        })
    }

    pub fn telegram_config(&self) -> Result<TelegramAccountConfig, MiraError> {
        if self.channel != ChannelKind::Telegram {
            return Err(MiraError::ConfigError(format!(
                "Account {} is not a Telegram account", self.id
            )));
        }
        serde_json::from_str(&self.config_json).map_err(|e| {
            MiraError::ConfigError(format!("Bad telegram config for {}: {}", self.id, e))
        })
    }

    pub fn discord_config(&self) -> Result<DiscordAccountConfig, MiraError> {
        if self.channel != ChannelKind::Discord {
            return Err(MiraError::ConfigError(format!(
                "Account {} is not a Discord account", self.id
            )));
        }
        serde_json::from_str(&self.config_json).map_err(|e| {
            MiraError::ConfigError(format!("Bad discord config for {}: {}", self.id, e))
        })
    }

    pub fn matrix_config(&self) -> Result<MatrixAccountConfig, MiraError> {
        if self.channel != ChannelKind::Matrix {
            return Err(MiraError::ConfigError(format!(
                "Account {} is not a Matrix account", self.id
            )));
        }
        serde_json::from_str(&self.config_json).map_err(|e| {
            MiraError::ConfigError(format!("Bad matrix config for {}: {}", self.id, e))
        })
    }

    pub fn whatsapp_config(&self) -> Result<WhatsAppAccountConfig, MiraError> {
        if self.channel != ChannelKind::WhatsApp {
            return Err(MiraError::ConfigError(format!(
                "Account {} is not a WhatsApp account", self.id
            )));
        }
        serde_json::from_str(&self.config_json).map_err(|e| {
            MiraError::ConfigError(format!("Bad whatsapp config for {}: {}", self.id, e))
        })
    }

    pub fn slack_config(&self) -> Result<SlackAccountConfig, MiraError> {
        if self.channel != ChannelKind::Slack {
            return Err(MiraError::ConfigError(format!(
                "Account {} is not a Slack account", self.id
            )));
        }
        serde_json::from_str(&self.config_json).map_err(|e| {
            MiraError::ConfigError(format!("Bad slack config for {}: {}", self.id, e))
        })
    }

    pub fn external_config(&self) -> Result<ExternalAccountConfig, MiraError> {
        if self.channel != ChannelKind::External {
            return Err(MiraError::ConfigError(format!(
                "Account {} is not an External account", self.id
            )));
        }
        serde_json::from_str(&self.config_json).map_err(|e| {
            MiraError::ConfigError(format!("Bad external config for {}: {}", self.id, e))
        })
    }
}

// ── Create / update DTOs ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NewChannelAccount {
    pub user_id:       String,
    pub channel:       ChannelKind,
    pub account_label: String,
    pub external_id:   Option<String>,
    pub config_json:   String,
    pub enabled:       bool,
    /// Defaults to `Personal` when the request omits the field; the
    /// handler maps it through before reaching the store.
    pub routing_mode:  RoutingMode,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateChannelAccount {
    pub account_label: Option<String>,
    pub external_id:   Option<Option<String>>,
    pub config_json:   Option<String>,
    pub enabled:       Option<bool>,
    pub routing_mode:  Option<RoutingMode>,
}
