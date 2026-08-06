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
pub fn init<W>(level: &str, format: &str, writer: W)
where
    W: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = build_filter(level);
    let (filter_layer, handle) = reload::Layer::new(filter);

    // G2: honor `logging.format` — `compact` (default), `pretty` (human), or
    // `json` (structured, for Loki/Datadog/etc.). Each formatter is a distinct
    // type, so the whole registry is initialised inside the match arm. Only one
    // arm runs, so moving `filter_layer` + `writer` in each is fine.
    let base = || fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(false);
    match format.trim().to_ascii_lowercase().as_str() {
        "json" => tracing_subscriber::registry()
            .with(filter_layer)
            .with(base().json().with_writer(writer))
            .init(),
        "pretty" => tracing_subscriber::registry()
            .with(filter_layer)
            .with(base().pretty().with_writer(writer))
            .init(),
        _ => tracing_subscriber::registry()
            .with(filter_layer)
            .with(base().compact().with_writer(writer))
            .init(),
    }

    let _ = RELOAD.set(handle);
    *current_cell().write().unwrap() = level.to_string();
}

/// Set up file logging end-to-end: create the log directory, install a
/// non-blocking, **size-rotating** file appender at `log_path`, and call
/// [`init`] at `level`. The non-blocking guard is leaked so it lives for the
/// whole process. Call exactly once per process.
///
/// Both entry points use this so logs land in the same place and the
/// `/api/logs/stream` endpoint always has a file to tail: the console
/// `--server`/TUI path (via `main`) and the Windows **service** path
/// (`install::windows::service_main`). The service entry previously skipped
/// logging setup entirely, so no log file was ever written and the web UI Logs
/// page hung on "connecting to log stream".
///
/// `max_file_size_mb` / `max_files` are honored via [`crate::log_rotate`]:
/// the active file keeps the fixed `log_path` name (so the tail endpoint still
/// follows a stable path), and archives roll to `mira.log.1`, `.2`, … up to
/// `max_files` total. `max_file_size_mb == 0` disables rotation (a single
/// unbounded file). The `/api/logs/stream` tail re-opens the path and detects
/// the size shrink at each roll, so it follows the fresh file automatically.
pub fn init_to_file(
    level:            &str,
    format:           &str,
    log_path:         &std::path::Path,
    max_file_size_mb: u32,
    max_files:        u32,
) {
    match crate::log_rotate::RotatingWriter::new(log_path, max_file_size_mb, max_files) {
        Ok(writer) => {
            let (non_blocking, guard) = tracing_appender::non_blocking(writer);
            // Guard must live for the entire process lifetime.
            Box::leak(Box::new(guard));
            init(level, format, non_blocking);
        }
        Err(e) => {
            // Opening the log file failed (bad path / permissions). Fall back to
            // stderr so the process still starts and logs somewhere, rather than
            // panicking or running silent.
            eprintln!("log: cannot open {log_path:?} for writing ({e}); logging to stderr");
            let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stderr());
            Box::leak(Box::new(guard));
            init(level, format, non_blocking);
        }
    }
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
