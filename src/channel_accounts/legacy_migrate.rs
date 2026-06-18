// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel_accounts/legacy_migrate.rs
//! One-shot migration from the single-account `[channels.signal]` /
//! `[channels.telegram]` blocks in `config.toml` to per-user
//! `channel_accounts` rows.
//!
//! Runs at startup, after the store is opened and the admin user is known.
//! Only seeds when the store is empty — existing deployments that already
//! use the per-user API are a no-op.

use std::sync::Arc;

use tracing::{info, warn};

use crate::channel_accounts::{
    ChannelAccountStore, ChannelKind, NewChannelAccount,
    SignalAccountConfig, TelegramAccountConfig,
};
use crate::config::MiraConfig;
use crate::history::HistoryStore;
use crate::MiraError;

/// Seed `channel_accounts` from the legacy TOML blocks, then re-stamp any
/// pre-existing conversations that were stored under the generic `"local-user"`
/// id. Safe to call on every boot — guarded by a row-count check.
pub fn migrate_if_empty(
    store:     &ChannelAccountStore,
    history:   Option<&Arc<HistoryStore>>,
    config:    &MiraConfig,
    admin_id:  &str,
) -> Result<(), MiraError> {
    if store.count_all()? > 0 {
        return Ok(());
    }

    let mut seeded = 0usize;

    // ── Signal ────────────────────────────────────────────────────────────────
    let sig = &config.channels.signal;
    if sig.enabled {
        match &sig.phone_number {
            Some(phone) if !phone.is_empty() => {
                let cfg = SignalAccountConfig {
                    phone_number: phone.clone(),
                    rest_port:    Some(sig.rest_port),
                    cli_binary:   sig.cli_binary.clone(),
                    data_dir:     sig.data_dir.clone(),
                    hmac_key:     sig.hmac_key.clone(),
                };
                let config_json = serde_json::to_string(&cfg)
                    .map_err(|e| MiraError::ConfigError(e.to_string()))?;
                store.create(NewChannelAccount {
                    user_id:       admin_id.to_owned(),
                    channel:       ChannelKind::Signal,
                    account_label: "default".to_owned(),
                    external_id:   Some(phone.clone()),
                    config_json,
                    enabled:       true,
                    routing_mode:  Default::default(),
                })?;
                info!("Migrated legacy Signal config → channel_account (phone={})", phone);
                seeded += 1;
            }
            _ => warn!("Signal enabled in config but no phone_number — skipping legacy migration"),
        }
    }

    // ── Telegram ──────────────────────────────────────────────────────────────
    let tg = &config.channels.telegram;
    if tg.enabled {
        match &tg.bot_token {
            Some(tok) if !tok.is_empty() => {
                let cfg = TelegramAccountConfig {
                    bot_token:         tok.clone(),
                    mode:              if tg.polling { "polling".to_owned() } else { "webhook".to_owned() },
                    // Per-account secret_token replaced the legacy
                    // global field. The admin can set one via the
                    // Channels page after migration; leaving None here
                    // means inbound webhooks for this account are
                    // accepted without header verification (matches
                    // legacy behaviour when the field was empty).
                    secret_token:      None,
                    poll_timeout_secs: 30,
                };
                let config_json = serde_json::to_string(&cfg)
                    .map_err(|e| MiraError::ConfigError(e.to_string()))?;
                store.create(NewChannelAccount {
                    user_id:       admin_id.to_owned(),
                    channel:       ChannelKind::Telegram,
                    account_label: "default".to_owned(),
                    external_id:   None,
                    config_json,
                    enabled:       true,
                    routing_mode:  Default::default(),
                })?;
                info!("Migrated legacy Telegram config → channel_account");
                seeded += 1;
            }
            _ => warn!("Telegram enabled in config but no bot_token — skipping legacy migration"),
        }
    }

    if seeded == 0 {
        info!("No legacy Signal/Telegram config to migrate");
        return Ok(());
    }

    // ── Conversation re-stamping ──────────────────────────────────────────────
    // Any pre-existing conversations were stamped with `"local-user"` by the
    // old TUI backend or the old channel webhook handlers. Re-stamp them onto
    // the real admin id so they surface in the admin's sidebar and not
    // nowhere (list_visible_conversations filters by exact user_id match).
    if let Some(hist) = history {
        for ch in ["signal", "telegram"] {
            match hist.reassign_channel_conversations("local-user", admin_id, ch) {
                Ok(0) => {}
                Ok(n) => info!("Re-stamped {} legacy {} conversation(s) onto admin user", n, ch),
                Err(e) => warn!("Could not re-stamp {} conversations: {}", ch, e),
            }
        }
    }

    Ok(())
}
