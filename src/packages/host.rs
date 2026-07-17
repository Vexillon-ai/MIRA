// SPDX-License-Identifier: AGPL-3.0-or-later

//! The real [`WizardHost`] (, slice 5b) — the store-backed side-effect
//! surface the `setup_guide` engine drives a `cpp_provider` install through.
//!
//! It wraps the channel-account store + the encrypted secret vault, mints CPP
//! secrets, runs reachability probes, and exposes MIRA's base URL. It is
//! **synchronous** (the engine is sync) so the install handler runs it under
//! `tokio::task::spawn_blocking`; the channel-manager hot-reload (async) happens
//! in the handler *after* finalize, exactly like the MCP install path.
//!
//! v1 is connection-only: `set_setting` is refused (not used by the worked
//! `nextcloud-talk` flow), and `command_exit`/`roundtrip` probes report skipped
//! rather than executing shell / a full inbound round-trip.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rand::RngCore;

use crate::channel_accounts::{
    ChannelAccountStore, ChannelKind, ExternalAccountConfig, NewChannelAccount, RoutingMode,
};
use crate::skills::secrets::{Scope, SecretsStore};

use super::engine::{CreateAccountReq, CreatedAccount, ProbeOutcome, ResolvedProbe, WizardHost};
use super::wizard::Encoding;

// Store-backed wizard host. Construct per-install (stores are cheap to open).
pub struct LiveHost {
    channels: ChannelAccountStore,
    secrets: SecretsStore,
    base_url: String,
    http: reqwest::blocking::Client,
}

impl LiveHost {
    pub fn new(channels: ChannelAccountStore, secrets: SecretsStore, base_url: String) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self { channels, secrets, base_url, http }
    }

    // 32-default random bytes in the requested encoding.
    fn random(bytes: usize, encoding: Encoding) -> String {
        let mut raw = vec![0u8; bytes.clamp(1, 4096)];
        rand::thread_rng().fill_bytes(&mut raw);
        match encoding {
            Encoding::Hex => hex::encode(raw),
            Encoding::Base64 => base64::engine::general_purpose::STANDARD.encode(raw),
        }
    }
}

impl WizardHost for LiveHost {
    fn mint_secret(&self, bytes: usize, encoding: Encoding) -> Result<String, String> {
        Ok(Self::random(bytes, encoding))
    }

    fn create_channel_account(
        &self,
        admin_id: &str,
        req: CreateAccountReq,
    ) -> Result<CreatedAccount, String> {
        // Mint the two CPP HMAC secrets here (same scheme as the manual
        // create-account path), so MIRA owns them — no manual copy, no drift.
        let cfg = ExternalAccountConfig {
            provider_kind: req.provider_kind,
            send_url: req.send_url,
            inbound_secret: Self::random(32, Encoding::Hex),
            outbound_secret: Self::random(32, Encoding::Hex),
            mention_only: req.mention_only,
            supports_voice: req.supports_voice,
        };
        let config_json =
            serde_json::to_string(&cfg).map_err(|e| format!("encode external config: {e}"))?;
        let row = self
            .channels
            .create(NewChannelAccount {
                user_id: admin_id.to_string(),
                channel: ChannelKind::External,
                account_label: req.account_label,
                external_id: None,
                config_json,
                enabled: true,
                routing_mode: RoutingMode::default(),
            })
            .map_err(|e| format!("create channel account: {e}"))?;
        Ok(CreatedAccount {
            account_id: row.id,
            inbound_secret: cfg.inbound_secret,
            outbound_secret: cfg.outbound_secret,
            send_url: cfg.send_url,
        })
    }

    fn set_setting(&self, key: &str, _value: &serde_json::Value) -> Result<(), String> {
        Err(format!(
            "mira.set_setting (key {key:?}) is not supported in v1 — set it via the relevant \
             settings page"
        ))
    }

    fn store_secret(&self, package_id: &str, key: &str, value: &str) -> Result<(), String> {
        self.secrets
            .set(Scope::System, "", package_id, key, value)
            .map_err(|e| format!("store secret {key:?}: {e}"))
    }

    fn get_secret(&self, package_id: &str, key: &str) -> Option<String> {
        self.secrets.get(Scope::System, "", package_id, key).ok().flatten()
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn run_probe(&self, probe: &ResolvedProbe) -> ProbeOutcome {
        match probe {
            ResolvedProbe::Http { url, method, expect_status } => {
                let m = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
                    .unwrap_or(reqwest::Method::GET);
                match self.http.request(m, url).send() {
                    Ok(resp) => {
                        let got = resp.status().as_u16();
                        match expect_status {
                            Some(want) if *want == got => ProbeOutcome::Pass,
                            Some(want) => ProbeOutcome::Fail(format!(
                                "expected HTTP {want}, got {got}"
                            )),
                            // No expectation → any HTTP response means reachable.
                            None => ProbeOutcome::Pass,
                        }
                    }
                    Err(e) => ProbeOutcome::Fail(format!("request to {url} failed: {e}")),
                }
            }
            ResolvedProbe::Tcp { host, port } => {
                use std::net::ToSocketAddrs;
                let addr = format!("{host}:{port}");
                match addr.to_socket_addrs() {
                    Ok(mut addrs) => match addrs.next() {
                        Some(sa) => match std::net::TcpStream::connect_timeout(
                            &sa,
                            Duration::from_secs(5),
                        ) {
                            Ok(_) => ProbeOutcome::Pass,
                            Err(e) => ProbeOutcome::Fail(format!("connect {addr}: {e}")),
                        },
                        None => ProbeOutcome::Fail(format!("no address resolved for {addr}")),
                    },
                    Err(e) => ProbeOutcome::Fail(format!("resolve {addr}: {e}")),
                }
            }
            ResolvedProbe::CommandExit { command, expect } => match self.run_command(command, "") {
                Ok(r) if r.code == *expect => ProbeOutcome::Pass,
                Ok(r) => ProbeOutcome::Fail(format!(
                    "`{command}` exited {} (expected {expect}){}",
                    r.code,
                    if r.stderr.trim().is_empty() { String::new() } else { format!(": {}", r.stderr.trim()) }
                )),
                Err(e) => ProbeOutcome::Fail(e),
            },
            ResolvedProbe::Roundtrip { kind, url, account_id, inbound_secret } => {
                match kind.as_str() {
                    "cpp" => self.roundtrip_cpp(url, account_id, inbound_secret),
                    "mcp" => self.roundtrip_mcp(url),
                    other => ProbeOutcome::Skipped(format!(
                        "roundtrip kind {other:?} isn't supported (cpp | mcp)"
                    )),
                }
            }
        }
    }

    fn write_service(&self, spec: super::service::ServiceSpec) -> Result<String, String> {
        super::service::install_and_start(&spec)
    }

    fn run_command(&self, command: &str, cwd: &str) -> Result<super::engine::CommandResult, String> {
        let (shell, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
        let mut c = std::process::Command::new(shell);
        c.arg(flag).arg(command);
        // Only set the working dir if it actually exists — spawning with a
        // missing cwd fails with a confusing ENOENT on the *command*.
        if !cwd.is_empty() && std::path::Path::new(cwd).is_dir() {
            c.current_dir(cwd);
        }
        let out = c.output().map_err(|e| format!("spawn `{command}`: {e}"))?;
        Ok(super::engine::CommandResult {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

impl LiveHost {
    // The `roundtrip {kind:"cpp"}` probe: sign a health-check inbound with the
    // account's `inbound_secret` and POST it to MIRA's own webhook. A `200`
    // proves the account is live *and* the minted secret verifies end-to-end —
    // the exact path a real provider's inbound takes. It uses a non-`message`
    // event type, so MIRA validates + acks without running an agent turn.
    fn roundtrip_cpp(&self, url: &str, account_id: &str, inbound_secret: &str) -> ProbeOutcome {
        if account_id.is_empty() || inbound_secret.is_empty() {
            return ProbeOutcome::Skipped(
                "no CPP channel account in this install to round-trip".into(),
            );
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
            .to_string();
        let body = serde_json::json!({
            "cpp_version": "1",
            "type": "mira.healthcheck",
            "conversation_id": "__mira_healthcheck__",
            "sender_id": "__mira_healthcheck__",
        })
        .to_string();
        let sig = crate::external::api::sign(inbound_secret, &ts, body.as_bytes());
        match self
            .http
            .post(url)
            .header("content-type", "application/json")
            .header("x-mira-cpp-timestamp", &ts)
            .header("x-mira-cpp-signature", sig)
            .body(body)
            .send()
        {
            Ok(resp) => match resp.status().as_u16() {
                200 => ProbeOutcome::Pass,
                401 => ProbeOutcome::Fail(
                    "MIRA rejected the signature — the stored inbound_secret doesn't match".into(),
                ),
                404 => ProbeOutcome::Fail(
                    "MIRA doesn't know this account yet (404) — it isn't live".into(),
                ),
                other => ProbeOutcome::Fail(format!("unexpected HTTP {other} from the webhook")),
            },
            Err(e) => ProbeOutcome::Fail(format!("couldn't reach MIRA's webhook at {url}: {e}")),
        }
    }

    // The `roundtrip {kind:"mcp"}` probe: an MCP `initialize` JSON-RPC call over
    // HTTP, expecting a JSON-RPC result back — proving the server is reachable
    // and speaks MCP. (stdio-transport MCP can't be probed this way → skipped.)
    fn roundtrip_mcp(&self, url: &str) -> ProbeOutcome {
        if url.is_empty() {
            return ProbeOutcome::Skipped(
                "mcp roundtrip needs a `url` (a stdio-transport MCP server can't be probed)".into(),
            );
        }
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "mira-verify", "version": "1" },
            },
        })
        .to_string();
        match self
            .http
            .post(url)
            // Streamable-HTTP servers may answer with JSON or an SSE stream.
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(init)
            .send()
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if !(200..300).contains(&status) {
                    return ProbeOutcome::Fail(format!("MCP initialize returned HTTP {status}"));
                }
                let body = resp.text().unwrap_or_default();
                if jsonrpc_has_result(&body) {
                    ProbeOutcome::Pass
                } else {
                    ProbeOutcome::Fail(
                        "MCP server didn't return a JSON-RPC result to initialize".into(),
                    )
                }
            }
            Err(e) => ProbeOutcome::Fail(format!("couldn't reach the MCP server at {url}: {e}")),
        }
    }
}

// Does `body` carry a JSON-RPC `result`? Accepts a raw JSON object or an SSE
// stream (`data: {…}` lines), since MCP streamable-HTTP may use either.
fn jsonrpc_has_result(body: &str) -> bool {
    let raw = std::iter::once(body.trim());
    let sse = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:").map(str::trim));
    for candidate in raw.chain(sse) {
        if candidate.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
            if v.get("result").is_some() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    // Stand up real stores against a temp auth.db (with the users FK target).
    fn live_host(dir: &std::path::Path) -> (LiveHost, ChannelAccountStore) {
        let db = dir.join("auth.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE users(id TEXT PRIMARY KEY); INSERT INTO users(id) VALUES('admin-1');",
            )
            .unwrap();
        }
        let channels = ChannelAccountStore::open(&db).unwrap();
        let channels2 = ChannelAccountStore::open(&db).unwrap();
        let secrets = SecretsStore::open(&dir.join("secrets.db"), &dir.join("master.key")).unwrap();
        (
            LiveHost::new(channels, secrets, "https://mira.example.com".into()),
            channels2,
        )
    }

    #[test]
    fn create_channel_account_persists_external_row_with_minted_secrets() {
        let d = tempfile::tempdir().unwrap();
        let (host, reader) = live_host(d.path());
        let created = host
            .create_channel_account(
                "admin-1",
                CreateAccountReq {
                    provider_kind: "nctalk".into(),
                    account_label: "com.x.talk (nctalk)".into(),
                    send_url: "https://nc.example.com/cpp".into(),
                    mention_only: false,
                    supports_voice: true,
                },
            )
            .unwrap();
        assert!(!created.account_id.is_empty());
        assert_eq!(created.inbound_secret.len(), 64); // 32 bytes hex
        assert_eq!(created.outbound_secret.len(), 64);

        // The row really landed, with the External config + minted secrets.
        let row = reader.get(&created.account_id).unwrap().unwrap();
        let cfg = row.external_config().unwrap();
        assert_eq!(cfg.provider_kind, "nctalk");
        assert_eq!(cfg.send_url, "https://nc.example.com/cpp");
        assert!(cfg.supports_voice);
        assert_eq!(cfg.inbound_secret, created.inbound_secret);
        assert!(row.enabled);
    }

    #[test]
    fn store_secret_roundtrips_through_the_vault() {
        let d = tempfile::tempdir().unwrap();
        let (host, _r) = live_host(d.path());
        host.store_secret("com.x.talk", "TALK_BOT_SECRET", "deadbeef").unwrap();
        let got = host
            .secrets
            .get(Scope::System, "", "com.x.talk", "TALK_BOT_SECRET")
            .unwrap();
        assert_eq!(got.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn mint_secret_encodings_and_set_setting_refused() {
        let d = tempfile::tempdir().unwrap();
        let (host, _r) = live_host(d.path());
        assert_eq!(host.mint_secret(16, Encoding::Hex).unwrap().len(), 32);
        // base64 of 30 bytes (no pad) is 40 chars.
        assert_eq!(host.mint_secret(30, Encoding::Base64).unwrap().len(), 40);
        assert!(host.set_setting("supports_voice", &serde_json::Value::Bool(true)).is_err());
    }

    #[test]
    fn jsonrpc_result_detected_in_raw_json_and_sse() {
        assert!(jsonrpc_has_result(r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{}}}"#));
        assert!(jsonrpc_has_result("event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n"));
        assert!(!jsonrpc_has_result(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601}}"#));
        assert!(!jsonrpc_has_result("not json at all"));
    }

    #[test]
    #[cfg(not(windows))]
    fn run_command_captures_exit_and_stdout_and_command_exit_probe() {
        let d = tempfile::tempdir().unwrap();
        let (host, _r) = live_host(d.path());
        let ok = host.run_command("echo hello", "").unwrap();
        assert_eq!(ok.code, 0);
        assert_eq!(ok.stdout.trim(), "hello");
        let bad = host.run_command("exit 3", "").unwrap();
        assert_eq!(bad.code, 3);
        // command_exit probe: pass when the exit matches, fail otherwise.
        assert!(matches!(
            host.run_probe(&ResolvedProbe::CommandExit { command: "true".into(), expect: 0 }),
            ProbeOutcome::Pass
        ));
        assert!(matches!(
            host.run_probe(&ResolvedProbe::CommandExit { command: "false".into(), expect: 0 }),
            ProbeOutcome::Fail(_)
        ));
    }

    #[test]
    fn tcp_probe_passes_on_open_port_fails_on_closed() {
        let d = tempfile::tempdir().unwrap();
        let (host, _r) = live_host(d.path());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(matches!(
            host.run_probe(&ResolvedProbe::Tcp { host: "127.0.0.1".into(), port }),
            ProbeOutcome::Pass
        ));
        drop(listener);
        // A port nobody is listening on → fail. Deriving a "closed" port is
        // inherently racy under the parallel test runner: the OS can immediately
        // hand a just-freed ephemeral port to another test. So retry with fresh
        // ports until one genuinely probes closed — the race would have to recur
        // every iteration to fail, which is astronomically unlikely.
        let mut fail_seen = false;
        for _ in 0..20 {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            if matches!(
                host.run_probe(&ResolvedProbe::Tcp { host: "127.0.0.1".into(), port: p }),
                ProbeOutcome::Fail(_)
            ) {
                fail_seen = true;
                break;
            }
        }
        assert!(fail_seen, "a freshly-closed port should probe as Fail");
        // A roundtrip with no resolved account reports skipped, not fail.
        assert!(matches!(
            host.run_probe(&ResolvedProbe::Roundtrip {
                kind: "cpp".into(),
                url: String::new(),
                account_id: String::new(),
                inbound_secret: String::new(),
            }),
            ProbeOutcome::Skipped(_)
        ));
        // An mcp roundtrip with no url (stdio transport) is skipped.
        assert!(matches!(
            host.run_probe(&ResolvedProbe::Roundtrip {
                kind: "mcp".into(),
                url: String::new(),
                account_id: String::new(),
                inbound_secret: String::new(),
            }),
            ProbeOutcome::Skipped(_)
        ));
    }
}
