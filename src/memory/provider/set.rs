// SPDX-License-Identifier: AGPL-3.0-or-later

//! `ProviderSet` — the configured memory providers for a running instance
//! (Part B2d), built once from `memory.providers`.
//!
//! Splits providers into the **read** list (fanned by the recall router) and the
//! **harness writers** (providers that get `ingest` called post-turn). The
//! built-in store is special: its write path stays the turn's LLM extraction
//! (alias-routed, A.8), so `builtin` is never a harness writer here — that would
//! double-extract. A locality gate refuses a non-loopback provider unless it
//! opted in. Empty config = built-in read only, i.e. exactly today's behaviour.

use std::sync::Arc;

use super::{
    builtin::BuiltinMemoryProvider, files::FilesMemoryProvider,
    http::{HttpMemoryProvider, ReqwestHttpCaller},
    MemoryLocality, MemoryProvider,
};
use crate::config::MiraConfig;
use crate::memory::MemorySystem;

/// Locality of an HTTP endpoint for the fail-closed gate.
fn http_locality(base_url: &str) -> MemoryLocality {
    if crate::agent::guardian::url_is_loopback(base_url) { MemoryLocality::Loopback }
    else { MemoryLocality::Remote }
}

/// A configured provider that did NOT load — refused non-loopback, a missing
/// key/base_url, a TLS-init error, or the not-yet-wired MCP transport. Surfaced
/// in the admin status panel (L7): these are exactly the cases an operator opens
/// the panel to diagnose, and they're invisible if we only list what loaded.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ProviderBuildFailure {
    pub id: String,
    pub transport: String,
    pub reason: String,
}

pub struct ProviderSet {
    /// Read providers in priority order (first = highest), fed to the router.
    pub read: Vec<Arc<dyn MemoryProvider>>,
    /// Non-builtin write providers in harness/both mode — `ingest`ed post-turn.
    pub harness_writers: Vec<Arc<dyn MemoryProvider>>,
    /// Configured providers that failed to load, with why (admin status panel).
    pub build_failures: Vec<ProviderBuildFailure>,
}

impl ProviderSet {
    /// Build from config. Unknown/not-yet-implemented transports (http, mcp) are
    /// skipped with a warning (built in later phases); a non-loopback provider is
    /// refused unless `allow_non_loopback` is set.
    pub fn build(config: &MiraConfig, memory: Arc<MemorySystem>) -> Self {
        let cfgs = &config.memory.providers;
        if cfgs.is_empty() {
            return Self {
                read: vec![Arc::new(BuiltinMemoryProvider::new(memory))],
                harness_writers: Vec::new(),
                build_failures: Vec::new(),
            };
        }

        let mut read: Vec<Arc<dyn MemoryProvider>> = Vec::new();
        let mut harness_writers: Vec<Arc<dyn MemoryProvider>> = Vec::new();
        let mut build_failures: Vec<ProviderBuildFailure> = Vec::new();

        for c in cfgs {
            if !c.enabled { continue; }
            let is_builtin = c.id == "builtin";
            let transport = c.transport.as_deref().unwrap_or(
                if is_builtin || c.root.is_some() { "in_process" } else { "" });
            let mut fail = |reason: String| {
                tracing::warn!("memory provider '{}' ({}): {}", c.id, transport, reason);
                build_failures.push(ProviderBuildFailure {
                    id: c.id.clone(), transport: transport.to_string(), reason,
                });
            };
            let provider: Option<Arc<dyn MemoryProvider>> = if is_builtin {
                Some(Arc::new(BuiltinMemoryProvider::new(Arc::clone(&memory))))
            } else if transport == "in_process" {
                match &c.root {
                    Some(root) => Some(Arc::new(FilesMemoryProvider::new(
                        c.id.clone(), crate::config::expand_path(root)))),
                    None => { fail("missing `root` directory — skipped".into()); None }
                }
            } else if transport == "http" {
                // HTTP memory service (any SPI-compliant endpoint).
                match &c.base_url {
                    Some(base_url) => match ReqwestHttpCaller::new(base_url, c.api_key_file.as_deref(), c.tls_ca_file.as_deref()) {
                        Ok(caller) => Some(Arc::new(HttpMemoryProvider::new(
                            c.id.clone(), Arc::new(caller), c.read_only, http_locality(base_url),
                            super::http::UserIdField::parse(c.user_id_field.as_deref())))),
                        Err(e) => { fail(format!("init failed (key/TLS): {e} — skipped")); None }
                    },
                    None => { fail("missing `base_url` — skipped".into()); None }
                }
            } else {
                // mcp transport: the adapter + mappings exist, but attaching a
                // live McpClient needs an async spawn/handshake that this sync
                // builder can't do — wired when the MCP host lifecycle lands.
                fail(format!("transport '{transport}' is not yet wired — skipped"));
                None
            };
            let Some(p) = provider else { continue };

            // Fail-closed locality gate: refuse non-loopback unless opted in.
            if p.locality() != MemoryLocality::Loopback && !c.allow_non_loopback {
                fail("non-loopback endpoint refused — set allow_non_loopback to opt in".into());
                continue;
            }

            let caps = p.capabilities();
            if caps.read {
                read.push(Arc::clone(&p));
            }
            // Harness writers: a writable, non-read-only, NON-builtin provider in
            // harness/both mode. Builtin's write is the turn's LLM extraction, so
            // it's excluded here to avoid double-extraction.
            let mode = c.mode.as_deref().unwrap_or("harness");
            if caps.write && !c.read_only && !is_builtin && matches!(mode, "harness" | "both") {
                harness_writers.push(p);
            }
            tracing::info!("memory provider '{}' enabled (read={}, write={})", c.id, caps.read, caps.write && !c.read_only);
        }

        // The built-in store is ALWAYS a reader unless the user explicitly turned
        // it off with a `{ "id": "builtin", "enabled": false }` entry. A populated
        // `providers` list that simply OMITS builtin (or lists only an external
        // source) must not silently drop built-in recall — the UI promises the
        // built-in store "is always present".
        let builtin_disabled = cfgs.iter().any(|c| c.id == "builtin" && !c.enabled);
        let has_builtin = read.iter().any(|p| p.id() == "builtin");
        if !has_builtin && !builtin_disabled {
            read.insert(0, Arc::new(BuiltinMemoryProvider::new(Arc::clone(&memory))));
        }
        // Final safety net: never leave recall with zero readers.
        if read.is_empty() {
            read.push(Arc::new(BuiltinMemoryProvider::new(memory)));
        }
        Self { read, harness_writers, build_failures }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MemoryProviderConfig, MiraConfig};
    use tempfile::tempdir;

    fn mem() -> Arc<MemorySystem> {
        let dir = tempdir().unwrap();
        let path = dir.path().join("m.db");
        std::mem::forget(dir);
        Arc::new(MemorySystem::new(path).unwrap())
    }

    #[test]
    fn empty_config_is_builtin_read_only_no_harness_writers() {
        let set = ProviderSet::build(&MiraConfig::default(), mem());
        assert_eq!(set.read.len(), 1);
        assert_eq!(set.read[0].id(), "builtin");
        assert!(set.harness_writers.is_empty(), "builtin writes via the legacy path, not harness ingest");
    }

    #[test]
    fn files_provider_joins_read_and_harness_writers() {
        let dir = tempdir().unwrap();
        let mut cfg = MiraConfig::default();
        cfg.memory.providers = vec![
            MemoryProviderConfig {
                id: "builtin".into(), enabled: true, transport: None, mode: None, read_only: false, allow_non_loopback: false,
                base_url: None, api_key_file: None, root: None, command: None, mapping: None, tls_ca_file: None, user_id_field: None,
            },
            MemoryProviderConfig {
                id: "files".into(), enabled: true, transport: Some("in_process".into()),
                mode: Some("both".into()), read_only: false,
                allow_non_loopback: false, base_url: None, api_key_file: None,
                root: Some(dir.path().to_string_lossy().into_owned()),
                command: None, mapping: None, tls_ca_file: None, user_id_field: None,
            },
        ];
        let set = ProviderSet::build(&cfg, mem());
        let read_ids: Vec<&str> = set.read.iter().map(|p| p.id()).collect();
        assert_eq!(read_ids, vec!["builtin", "files"]);
        // files is a harness writer; builtin is NOT (legacy path).
        let writer_ids: Vec<&str> = set.harness_writers.iter().map(|p| p.id()).collect();
        assert_eq!(writer_ids, vec!["files"]);
    }

    fn http(id: &str, base_url: &str, read_only: bool, allow_non_loopback: bool) -> MemoryProviderConfig {
        MemoryProviderConfig {
            id: id.into(), enabled: true, transport: Some("http".into()), mode: None, read_only, allow_non_loopback,
            base_url: Some(base_url.into()), api_key_file: None, root: None, command: None, mapping: None, tls_ca_file: None, user_id_field: None,
        }
    }

    #[test]
    fn builtin_is_kept_even_when_providers_omit_it() {
        let mut cfg = MiraConfig::default();
        cfg.memory.providers = vec![
            MemoryProviderConfig {
                id: "files".into(), enabled: false, transport: Some("in_process".into()),
                mode: None, read_only: false, allow_non_loopback: false,
                base_url: None, api_key_file: None, root: Some("/tmp/x".into()),
                command: None, mapping: None, tls_ca_file: None, user_id_field: None,
            },
            http("ext-http", "http://127.0.0.1:9/v1", true, false),
        ];
        // files disabled → skipped; the loopback read-only http provider builds.
        // builtin is NOT listed, but must still be a reader (prepended) so
        // built-in recall is never silently dropped.
        let set = ProviderSet::build(&cfg, mem());
        assert_eq!(set.read.iter().map(|p| p.id()).collect::<Vec<_>>(), vec!["builtin", "ext-http"]);
        assert!(set.harness_writers.is_empty());
    }

    #[test]
    fn builtin_can_be_explicitly_disabled() {
        let mut cfg = MiraConfig::default();
        cfg.memory.providers = vec![
            MemoryProviderConfig {
                id: "builtin".into(), enabled: false, transport: None, mode: None, read_only: false, allow_non_loopback: false,
                base_url: None, api_key_file: None, root: None, command: None, mapping: None,
                tls_ca_file: None, user_id_field: None,
            },
            http("ext-http", "http://127.0.0.1:9/v1", true, false),
        ];
        let set = ProviderSet::build(&cfg, mem());
        // Explicit disable is honoured: only the external reader.
        assert_eq!(set.read.iter().map(|p| p.id()).collect::<Vec<_>>(), vec!["ext-http"]);
    }

    #[test]
    fn non_loopback_http_is_refused_without_opt_in() {
        let mut cfg = MiraConfig::default();
        cfg.memory.providers = vec![http("remote", "https://memory.example.com/v1", true, false)];
        // Refused (fail-closed) → no reader → builtin safety fallback.
        let set = ProviderSet::build(&cfg, mem());
        assert_eq!(set.read.iter().map(|p| p.id()).collect::<Vec<_>>(), vec!["builtin"]);

        // With the explicit opt-in, it builds — alongside the always-present builtin.
        cfg.memory.providers = vec![http("remote", "https://memory.example.com/v1", true, true)];
        let set2 = ProviderSet::build(&cfg, mem());
        assert_eq!(set2.read.iter().map(|p| p.id()).collect::<Vec<_>>(), vec!["builtin", "remote"]);
    }
}
