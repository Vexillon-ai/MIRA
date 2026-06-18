// SPDX-License-Identifier: AGPL-3.0-or-later

// src/log_filter.rs
//! Runtime-reloadable log filter.
//!
//! `init` installs the global tracing subscriber with a `reload::Layer` so the
//! `EnvFilter` directives can be swapped at runtime via `set_level`. The
//! current effective level is tracked separately so `current_level` can answer
//! the API without parsing the EnvFilter back out.

use std::sync::OnceLock;
use std::sync::RwLock;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::reload;
use tracing_subscriber::Registry;

type ReloadHandle = reload::Handle<EnvFilter, Registry>;

static RELOAD: OnceLock<ReloadHandle> = OnceLock::new();
static CURRENT: OnceLock<RwLock<String>> = OnceLock::new();

/// Levels accepted by the runtime toggle API.
pub const LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

fn current_cell() -> &'static RwLock<String> {
    CURRENT.get_or_init(|| RwLock::new("info".to_string()))
}

fn build_filter(level: &str) -> EnvFilter {
    // Mirror the directives historically applied at startup:
    //   mira=<level>, tokio=warn, plus anything from the RUST_LOG env var.
    let primary = format!("mira={}", level);
    EnvFilter::from_default_env()
        .add_directive(primary.parse().unwrap_or_else(|_| "mira=info".parse().unwrap()))
        .add_directive("tokio=warn".parse().unwrap())
}

/// Install the subscriber. Must be called exactly once at process start.
///
/// `writer` receives every formatted log line. `level` seeds the `mira=...`
/// directive; `RUST_LOG` is still honoured.
pub fn init<W>(level: &str, writer: W)
where
    W: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = build_filter(level);
    let (filter_layer, handle) = reload::Layer::new(filter);

    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(false)
        .compact()
        .with_writer(writer);

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();

    let _ = RELOAD.set(handle);
    *current_cell().write().unwrap() = level.to_string();
}

/// Returns the level last applied via `init` or `set_level`.
pub fn current_level() -> String {
    current_cell().read().unwrap().clone()
}

/// Swap the active filter to the given level. Returns Err if the level is not
/// in `LEVELS` or the subscriber was never installed.
pub fn set_level(level: &str) -> Result<(), String> {
    let normalised = level.to_ascii_lowercase();
    if !LEVELS.contains(&normalised.as_str()) {
        return Err(format!(
            "invalid log level '{}', expected one of: {}",
            level,
            LEVELS.join(", "),
        ));
    }
    let handle = RELOAD.get().ok_or_else(|| "log subscriber not initialised".to_string())?;
    let new_filter = build_filter(&normalised);
    handle.reload(new_filter).map_err(|e| e.to_string())?;
    *current_cell().write().unwrap() = normalised;
    Ok(())
}
