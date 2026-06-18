// SPDX-License-Identifier: AGPL-3.0-or-later

// src/email/imap_poll.rs
//! Per-account IMAP poller (slice E1+E3, chunk 2).
//!
//! Connects to the configured IMAP server, authenticates, and polls
//! `INBOX` every `POLL_INTERVAL`. Anything with `UID > last_uid_seen`
//! is fetched, RFC822 bytes are logged (chunk 2 stops here — chunks
//! 3+4 add the MIME pipeline + agent dispatch), and the watermark is
//! advanced so we don't re-process on next tick.
//!
//! Failures are caught and logged at WARN; the loop sleeps and
//! retries rather than tearing down the task. That gives the rest of
//! the gateway resilience against a single misconfigured account
//! flapping its connection.
//!
//! IMAP IDLE (push-style real-time delivery) is supported by
//! `async-imap` but we're sticking with poll-mode here for v1 — the
//! IDLE state machine adds complexity (keepalives every <29min, two
//! sockets, etc.) that's worth the latency win only when the demand
//! materialises.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tracing::{debug, info, warn};

use crate::agent::AgentCore;
use crate::email::audit::{EmailAuditStore, NewAuditEntry};
use crate::email::dispatch::dispatch_inbound;
use crate::email::parser::{parse_email, ParseSettings};
use crate::email::quarantine::{EmailQuarantineStore, NewQuarantineEntry};
use crate::email::rate::InMemoryRateLimiter;
use crate::email::security::{evaluate, InboundHeaders, Verdict};
use crate::email::smtp::ReplyLoopCache;
use crate::email::store::{EmailAccountRow, EmailAccountStore};
use crate::email::oauth;
use crate::history::HistoryStore;
use crate::web::LiveConfig;

/// SASL XOAUTH2 authenticator for async-imap. The handshake is a
/// single-round affair — the server sends an empty challenge after
/// we issue `AUTHENTICATE XOAUTH2`, we send the formatted
/// `user=…\x01auth=Bearer <token>\x01\x01` string, server replies
/// with OK. We never see a second challenge; if the server rejects
/// the token it sends a NO+continue with error JSON and we have to
/// reply with empty bytes to abort. Implemented as one-shot — the
/// first call returns the auth string; any subsequent call returns
/// empty bytes to satisfy the abort path.
struct Xoauth2Authenticator {
    user:  String,
    token: String,
    sent:  bool,
}

impl async_imap::Authenticator for Xoauth2Authenticator {
    type Response = Vec<u8>;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        if self.sent {
            // Abort path — server's continuation after a NO with a
            // base64-JSON error blob. Empty reply tells the server
            // we're done.
            return Vec::new();
        }
        self.sent = true;
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token).into_bytes()
    }
}

/// How often the poller checks for new mail. 60s is the same default
/// as most desktop clients; tunable per-account in a later chunk if
/// it matters.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Connect timeout for both the TCP and TLS halves. IMAP servers are
/// usually fast; 30s is the "obviously dead" threshold.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-account live status, shared between the poller task and the
/// `/api/email/status` handler. The poller mutates; the handler
/// reads. Cheap clones because everything inside is `String` / scalar.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmailPollerStatus {
    pub account_id:        String,
    pub owner_user_id:     String,
    pub address:           String,
    /// "idle" (just started, no poll yet) | "polling" | "ok" | "error"
    pub state:             String,
    /// Most-recent error message, sticky until the next successful poll.
    pub last_error:        Option<String>,
    /// Unix-ms timestamps. Both None until the first cycle completes.
    pub last_polled_at:    Option<i64>,
    pub last_received_at:  Option<i64>,
    /// Number of messages handled across the lifetime of the task.
    pub total_received:    u64,
}

impl EmailPollerStatus {
    fn new(row: &EmailAccountRow) -> Self {
        Self {
            account_id:       row.id.clone(),
            owner_user_id:    row.user_id.clone(),
            address:          row.address.clone(),
            state:            "idle".into(),
            last_error:       None,
            last_polled_at:   None,
            last_received_at: None,
            total_received:   0,
        }
    }
}

/// Snapshot view for the HTTP layer.
pub type SharedStatus = Arc<RwLock<EmailPollerStatus>>;

/// Spawns one poller task. Caller keeps the returned status handle
/// to expose runtime state via `/api/email/status`; dropping the
/// handle does NOT stop the task — abort via the `JoinHandle` if
/// shutdown is needed.
pub fn spawn_poller(
    row:        EmailAccountRow,
    store:      Arc<EmailAccountStore>,
    history:    Arc<HistoryStore>,
    agent:      Arc<AgentCore>,
    quarantine: Arc<EmailQuarantineStore>,
    audit:      Arc<EmailAuditStore>,
    rate:       Arc<InMemoryRateLimiter>,
    loop_cache: Arc<ReplyLoopCache>,
    live_cfg:   Arc<LiveConfig>,
) -> (SharedStatus, tokio::task::JoinHandle<()>) {
    let status = Arc::new(RwLock::new(EmailPollerStatus::new(&row)));
    let task_status = Arc::clone(&status);
    let task_row    = row.clone();
    let task_store  = store;

    let handle = tokio::spawn(async move {
        run_poll_loop(
            task_row, task_store, history, agent,
            quarantine, audit, rate, loop_cache, live_cfg, task_status,
        ).await;
    });

    (status, handle)
}

/// The poll loop. Reconnects between cycles — IMAP sessions are
/// cheap and many servers drop long-idle connections anyway. A
/// future optimisation is to hold a long-lived session and only
/// reconnect on failure, but that needs IDLE keepalive logic to do
/// safely.
async fn run_poll_loop(
    row:        EmailAccountRow,
    store:      Arc<EmailAccountStore>,
    history:    Arc<HistoryStore>,
    agent:      Arc<AgentCore>,
    quarantine: Arc<EmailQuarantineStore>,
    audit:      Arc<EmailAuditStore>,
    rate:       Arc<InMemoryRateLimiter>,
    loop_cache: Arc<ReplyLoopCache>,
    live_cfg:   Arc<LiveConfig>,
    status:     SharedStatus,
) {
    info!("email: poller starting for '{}' (account {})", row.address, row.id);
    loop {
        {
            let mut s = status.write().await;
            s.state = "polling".into();
        }

        match poll_once(&row, &store, &history, &agent, &quarantine, &audit, &rate, &loop_cache, &live_cfg).await {
            Ok(n_new) => {
                let now = ms_now();
                let mut s = status.write().await;
                s.state          = "ok".into();
                s.last_error     = None;
                s.last_polled_at = Some(now);
                if n_new > 0 {
                    s.last_received_at = Some(now);
                    s.total_received  += n_new as u64;
                }
                if n_new > 0 {
                    info!("email '{}': {} new message{}", row.address, n_new,
                          if n_new == 1 { "" } else { "s" });
                }
            }
            Err(e) => {
                warn!("email '{}': poll failed: {e}", row.address);
                let mut s = status.write().await;
                s.state       = "error".into();
                s.last_error  = Some(e.to_string());
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One poll cycle: connect, login, select INBOX, fetch anything
/// newer than the watermark, advance watermark, logout. Returns the
/// count of new messages handled this cycle (zero is the steady
/// state).
async fn poll_once(
    row:        &EmailAccountRow,
    store:      &Arc<EmailAccountStore>,
    history:    &Arc<HistoryStore>,
    agent:      &Arc<AgentCore>,
    quarantine: &Arc<EmailQuarantineStore>,
    audit:      &Arc<EmailAuditStore>,
    rate:       &Arc<InMemoryRateLimiter>,
    loop_cache: &Arc<ReplyLoopCache>,
    live_cfg:   &Arc<LiveConfig>,
) -> Result<usize, PollError> {
    // OAuth accounts fall back to the provider's known IMAP host
    // when the row didn't carry an explicit `imap_host` — lets the
    // operator skip those fields when wiring up Gmail / Outlook.
    let oauth_defaults = oauth::OAuthProvider::parse(&row.auth_mode)
        .map(|p| p.imap_defaults());
    let host = row.imap_host.as_deref()
        .or(oauth_defaults.map(|(h, _)| h))
        .ok_or_else(|| PollError::Config("imap_host missing".into()))?;
    let port = row.imap_port
        .or(oauth_defaults.map(|(_, p)| p))
        .unwrap_or(if row.imap_use_tls { 993 } else { 143 });
    let username = row.imap_username.as_deref()
        .or(Some(row.address.as_str())) // fall back to the From address; Gmail/Outlook accept that
        .ok_or_else(|| PollError::Config("imap_username missing".into()))?;

    if !row.imap_use_tls {
        // STARTTLS / plaintext IMAP is rare enough in 2026 that
        // refusing it surfaces operator confusion faster than
        // silently shipping creds over the wire. If anyone needs it
        // they can speak up; we'll add the STARTTLS handshake then.
        return Err(PollError::Config(
            "imap_use_tls=false not supported in chunk 2 — use port 993 + TLS".into()
        ));
    }

    // ── TCP + TLS ──────────────────────────────────────────────────
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((host, port)),
    )
    .await
    .map_err(|_| PollError::Connect("tcp connect timeout".into()))?
    .map_err(|e| PollError::Connect(format!("tcp: {e}")))?;

    let tls_stream = tls_wrap(tcp, host).await?;

    // ── IMAP greeting + login ──────────────────────────────────────
    // async-imap with `runtime-tokio` feature speaks tokio's
    // AsyncRead/AsyncWrite directly — no compat wrapper needed.
    let mut client = async_imap::Client::new(tls_stream);
    let _greeting = client.read_response().await
        .map_err(|e| PollError::Protocol(format!("greeting: {e}")))?
        .ok_or_else(|| PollError::Protocol("server closed before greeting".into()))?;

    // Auth branch — password (PLAIN LOGIN) or OAuth XOAUTH2.
    // OAuth path refreshes the access token if it's near expiry +
    // re-loads the row after persisting so the in-memory copy
    // matches what was just written.
    let mut session = match row.auth_mode.as_str() {
        "password" => {
            let password = row.imap_password.as_deref()
                .ok_or_else(|| PollError::Config("imap_password missing".into()))?;
            client.login(username, password).await
                .map_err(|(e, _client)| PollError::Auth(format!("login: {e}")))?
        }
        mode if mode.starts_with("oauth_") => {
            let live = live_cfg.get().await;
            let access = oauth::refresh_if_needed(&live.email_oauth, store.as_ref(), &row.id).await
                .map_err(|e| PollError::Auth(format!("oauth refresh: {e}")))?;
            let auth = Xoauth2Authenticator {
                user:  username.to_string(),
                token: access,
                sent:  false,
            };
            client.authenticate("XOAUTH2", auth).await
                .map_err(|(e, _client)| PollError::Auth(format!("xoauth2: {e}")))?
        }
        other => return Err(PollError::Config(format!(
            "unknown auth_mode {other:?} on account {}", row.id
        ))),
    };

    // ── Select INBOX ───────────────────────────────────────────────
    let _mbox = session.select("INBOX").await
        .map_err(|e| PollError::Protocol(format!("select INBOX: {e}")))?;

    // ── Fetch UIDs > watermark ─────────────────────────────────────
    // `UID FETCH <low>:* (UID)` returns every message with UID ≥ low,
    // including the highest UID (the server's current top). We use
    // low = last_uid_seen + 1 so we never re-fetch the same message.
    let low = (row.last_uid_seen as u32).saturating_add(1);
    let query = format!("{low}:*");
    let mut new_uids: Vec<u32> = Vec::new();
    {
        let fetches = session.uid_fetch(&query, "(UID)").await
            .map_err(|e| PollError::Protocol(format!("uid_fetch list: {e}")))?;
        tokio::pin!(fetches);
        while let Some(item) = fetches.next().await {
            let f = item.map_err(|e| PollError::Protocol(format!("uid_fetch stream: {e}")))?;
            if let Some(uid) = f.uid {
                // `low:*` matches the highest UID even when nothing
                // newer exists, so filter for strictly-greater here.
                if uid >= low {
                    new_uids.push(uid);
                }
            }
        }
    }

    if new_uids.is_empty() {
        // Steady state. Drop the session politely.
        let _ = session.logout().await;
        return Ok(0);
    }

    // ── Fetch bodies for new UIDs. Chunk 3 runs each body through
    // ── the MIME parser + security pipeline; chunk 4 will route
    // ── Accepts to the agent + chunk 5 puts Quarantines into the
    // ── queue. For chunk 3 we log the verdict and drop, so the
    // ── pipeline is exercised end-to-end on real mail without
    // ── plumbing into the agent yet.
    new_uids.sort_unstable();
    let uid_set = new_uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
    let security = row.security();
    let parse_settings = ParseSettings {
        accept_html:          security.accept_html.unwrap_or(true),
        accept_inline_images: security.accept_inline_images.unwrap_or(false),
        accept_attachments:   security.accept_attachments.unwrap_or(false),
    };
    let effective_max_size_kb = security.max_message_size_kb.unwrap_or(1024);
    let effective_rate_per_sender  = security.inbound_rate_per_sender_per_hour.unwrap_or(10);
    let effective_rate_per_account = security.inbound_rate_per_account_per_day.unwrap_or(100);
    let (handled, highest_uid) = {
        let fetches = session.uid_fetch(&uid_set, "RFC822").await
            .map_err(|e| PollError::Protocol(format!("uid_fetch bodies: {e}")))?;
        tokio::pin!(fetches);
        let mut handled = 0usize;
        let mut highest_uid: u32 = row.last_uid_seen as u32;
        while let Some(item) = fetches.next().await {
            let f = item.map_err(|e| PollError::Protocol(format!("uid_fetch body stream: {e}")))?;
            let uid = f.uid.unwrap_or(0);
            let body = match f.body() {
                Some(b) => b,
                None => {
                    warn!("email '{}': uid={uid} has no body", row.address);
                    handled += 1;
                    if uid > highest_uid { highest_uid = uid; }
                    continue;
                }
            };
            let raw_size = body.len();

            // Parse headers separately first — the security pipeline
            // wants Auto-Submitted/Precedence/Authentication-Results
            // even when body extraction later fails.
            let header_msg = mail_parser::MessageParser::default().parse_headers(body);
            let headers = header_msg.as_ref()
                .map(InboundHeaders::from_message)
                .unwrap_or_default();

            let parsed = match parse_email(body, parse_settings) {
                Some(p) => p,
                None => {
                    warn!("email '{}': uid={uid} failed to parse", row.address);
                    handled += 1;
                    if uid > highest_uid { highest_uid = uid; }
                    continue;
                }
            };

            let verdict = evaluate(
                &parsed, raw_size, &headers, &security,
                effective_max_size_kb,
                effective_rate_per_sender,
                effective_rate_per_account,
                &row.id, rate.as_ref(),
            );

            // Action tag drives both the operator-facing log and the
            // audit row. Set inside each arm so the audit insert can
            // be a single call below.
            let (action, audit_reason) = match &verdict {
                Verdict::Accept { reason } => {
                    info!("email '{}': uid={uid} from={} subject={:?} → ACCEPT ({reason})",
                          row.address, parsed.sender_address, parsed.subject);
                    let action = match dispatch_inbound(
                        &parsed, row, history, agent,
                        Some(audit), Some(loop_cache),
                        Some(store), Some(live_cfg),
                    ).await {
                        Ok(conv_id) => {
                            debug!("email '{}': uid={uid} → conv={}", row.address, conv_id);
                            "accepted"
                        }
                        Err(e) => {
                            warn!("email '{}': uid={uid} dispatch failed ({}): {e}",
                                  row.address, e.as_tag());
                            "dispatch_failed"
                        }
                    };
                    (action, Some(reason.clone()))
                }
                Verdict::Quarantine { reason } => {
                    info!("email '{}': uid={uid} from={} subject={:?} → QUARANTINE ({reason})",
                          row.address, parsed.sender_address, parsed.subject);
                    let preview: String = parsed.text_body.chars().take(500).collect();
                    let new = NewQuarantineEntry {
                        account_id:  row.id.clone(),
                        sender:      parsed.sender_address.clone(),
                        subject:     parsed.subject.clone(),
                        preview,
                        message_id:  parsed.message_id.clone(),
                        reason:      reason.clone(),
                        uid:         uid as i64,
                        raw_body:    body.to_vec(),
                    };
                    if let Err(e) = quarantine.put(new) {
                        warn!("email '{}': uid={uid} quarantine put failed: {e}", row.address);
                    }
                    ("quarantined", Some(reason.clone()))
                }
                Verdict::Drop { reason } => {
                    info!("email '{}': uid={uid} from={} subject={:?} → DROP ({reason})",
                          row.address, parsed.sender_address, parsed.subject);
                    ("dropped", Some(reason.clone()))
                }
            };

            // Audit row — fire-and-forget; a failed audit insert is
            // logged but doesn't abort the poll cycle.
            if let Err(e) = audit.record(NewAuditEntry {
                account_id:     row.id.clone(),
                direction:      "inbound".into(),
                sender:         parsed.sender_address.clone(),
                recipient:      row.address.clone(),
                subject:        parsed.subject.clone(),
                action:         action.into(),
                reason:         audit_reason,
                body:           body.to_vec(),
                attached_count: parsed.dropped_attachments.len(),
            }) {
                warn!("email '{}': uid={uid} audit record failed: {e}", row.address);
            }

            debug!("email '{}': uid={uid} body_bytes={raw_size}", row.address);
            handled += 1;
            if uid > highest_uid { highest_uid = uid; }
        }
        (handled, highest_uid)
    };

    // Persist the new watermark so a restart doesn't re-process.
    // Done after the body fetch so a mid-fetch crash leaves us
    // re-fetching, which is safe (still drops the message).
    store.advance_uid(&row.id, highest_uid as i64)
        .map_err(|e| PollError::Persist(format!("advance_uid: {e}")))?;

    let _ = session.logout().await;
    Ok(handled)
}

async fn tls_wrap(
    tcp:  TcpStream,
    host: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, PollError> {
    // Use the bundled webpki-roots — no system-trust dependency,
    // matches reqwest's default in MIRA.
    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let dns_name = ServerName::try_from(host.to_string())
        .map_err(|e| PollError::Tls(format!("server name {host:?}: {e}")))?;

    let tls = tokio::time::timeout(
        CONNECT_TIMEOUT,
        connector.connect(dns_name, tcp),
    )
    .await
    .map_err(|_| PollError::Tls("tls handshake timeout".into()))?
    .map_err(|e| PollError::Tls(format!("handshake: {e}")))?;

    Ok(tls)
}

fn ms_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Internal error type — kept private to this module so the
/// distinctions only matter to the loop's logging. The status
/// snapshot just records `last_error: String`.
#[derive(Debug, thiserror::Error)]
enum PollError {
    #[error("config: {0}")]
    Config(String),
    #[error("connect: {0}")]
    Connect(String),
    #[error("tls: {0}")]
    Tls(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("persist: {0}")]
    Persist(String),
}
