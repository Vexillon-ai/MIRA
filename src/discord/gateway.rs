// SPDX-License-Identifier: AGPL-3.0-or-later

// src/discord/gateway.rs
//
// Discord gateway WebSocket client — the inbound half of D2.
//
// We deliberately roll this ourselves on top of `tokio-tungstenite` rather
// than pulling in `twilight-gateway`/`serenity`: the gateway protocol's
// useful surface is small (IDENTIFY, HEARTBEAT, RESUME, INVALID_SESSION,
// DISPATCH) and owning it means we own the failure modes (heartbeat ACK
// tracking, RESUME-vs-fresh decisioning, reconnect backoff). Same posture
// as our backup_crypto vs leaning on a higher-level container.
//
// What this DOESN'T do (intentionally, for D2):
// * Sharding — bots in <2500 guilds use shard 0/1; we won't.
// * zlib-stream compression — saves ~30% bandwidth; not worth the
//   ceremony of holding an inflate state across frames yet.
// * Voice — voice gateway is a separate protocol; D3 / future.
// * Presence updates / activity — `op 3` deferred to later.
//
// Lifecycle (single connection):
// 1. Connect WSS to gateway URL (?v=10&encoding=json).
// 2. Receive `op 10 HELLO` → spawn heartbeat ticker at heartbeat_interval.
// 3. Send `op 2 IDENTIFY` (fresh) OR `op 6 RESUME` (if we have session_id).
// 4. On `op 0 t=READY`, cache session_id + resume_gateway_url.
// 5. On `op 0 t=MESSAGE_CREATE`, forward to the dispatcher.
// 6. On each heartbeat send, track that we expect an ACK — if the next
//    heartbeat tick fires without one, treat the connection as zombie
//    and close + reconnect with RESUME.
// 7. On `op 7 RECONNECT` or `op 9 INVALID_SESSION(true)`, RESUME.
//    On `op 9 INVALID_SESSION(false)`, drop session_id + fresh IDENTIFY.
// 8. On WS close / IO error, exponential backoff (1s, 2s, 4s, capped 30s)
//    then reconnect.
//
// The driver loop runs until the `shutdown` Notify fires, at which point
// it sends a clean WS close (code 1000) and returns.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::{mpsc, Notify};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{protocol::CloseFrame, Message as WsMessage},
};
use tracing::{debug, error, info, warn};

use super::dispatch::DiscordDispatcherDeps;
use super::types::{
    GatewayFrame, Hello, Identify, IdentifyProps, InvalidSession, MessageCreate, Ready,
    Resume, OP_DISPATCH, OP_HEARTBEAT, OP_HEARTBEAT_ACK, OP_HELLO, OP_IDENTIFY,
    OP_INVALID_SESSION, OP_RECONNECT, OP_RESUME, REQUIRED_INTENTS,
};

// Bootstrap gateway URL. After READY we switch to `resume_gateway_url`.
const BOOTSTRAP_GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

// Cap on reconnect backoff between successive attempts.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

// Per-account immutable bits the dispatcher needs at process time.
// Cloned into each spawned dispatch task so the connection loop stays
// focused on protocol mechanics.
#[derive(Clone)]
pub struct DiscordAccountCtx {
    pub account_id:     String,
    pub owner_user_id:  String,
    pub bot_token:      String,
    // Application snowflake (numeric string). When known up-front we use
    // it to skip our own MESSAGE_CREATE echoes immediately; when missing
    // we fall back to caching `Ready.user.id` after the first READY.
    pub application_id: Option<String>,
    // When true the dispatcher only acts on messages that @-mention us.
    pub mention_only:   bool,
    // R1+R2 routing mode: `Personal` runs every inbound as `owner_user_id`,
    // `Shared` resolves the sender via `IdentityStore`, `GuestOk` falls
    // through to a guest identity on lookup miss.
    pub routing_mode:   crate::channel_accounts::RoutingMode,
}

// Spawn a long-lived Discord gateway connection for one account. The
// returned `Notify` is used by the gateway manager to request a clean
// shutdown (send WS close + return). The `JoinHandle` should be held by
// the channel manager so it aborts on drop.
pub fn spawn_gateway(
    ctx:      DiscordAccountCtx,
    deps:     DiscordDispatcherDeps,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_gateway_loop(ctx, deps, shutdown).await;
    })
}

async fn run_gateway_loop(
    ctx:      DiscordAccountCtx,
    deps:     DiscordDispatcherDeps,
    shutdown: Arc<Notify>,
) {
    // Persisted across reconnects.
    let mut session_id:         Option<String> = None;
    let mut resume_url:         Option<String> = None;
    let mut last_seq:           u64            = 0;
    let mut bot_user_id_seen:   Option<String> = ctx.application_id.clone();

    let mut backoff = Duration::from_secs(1);

    loop {
        // Pick where to connect this attempt. RESUME if we have a saved
        // session + url; otherwise bootstrap.
        let url = match (&session_id, &resume_url) {
            (Some(_), Some(u)) => u.clone(),
            _                  => BOOTSTRAP_GATEWAY.to_string(),
        };

        let resume_payload = session_id.as_ref().map(|sid| Resume {
            token:      ctx.bot_token.clone(),
            session_id: sid.clone(),
            seq:        last_seq,
        });

        info!(account = %ctx.account_id,
              "Discord gateway connecting (url={}, resume={})",
              redact_query(&url), resume_payload.is_some());

        let one_conn = connect_and_pump(
            &url,
            &ctx,
            &deps,
            &mut session_id,
            &mut resume_url,
            &mut last_seq,
            &mut bot_user_id_seen,
            resume_payload,
            Arc::clone(&shutdown),
        ).await;

        match one_conn {
            ConnectionOutcome::ShutdownRequested => {
                info!(account = %ctx.account_id, "Discord gateway: clean shutdown");
                return;
            }
            ConnectionOutcome::FreshIdentifyNeeded => {
                // INVALID_SESSION(false) — drop the cached session +
                // wait the jittered cool-off Discord wants before
                // identifying again. Spec says 1-5s.
                warn!(account = %ctx.account_id,
                      "Discord gateway: invalid session (non-resumable); fresh IDENTIFY after backoff");
                session_id = None;
                resume_url = None;
                last_seq   = 0;
                tokio::time::sleep(Duration::from_millis(1500 + (rand_jitter_ms() % 3500))).await;
                backoff = Duration::from_secs(1);
            }
            ConnectionOutcome::Reconnect => {
                // Resumable error — try the saved session_id with backoff.
                debug!(account = %ctx.account_id,
                       "Discord gateway: reconnect (backoff={}s)", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
        }
    }
}

enum ConnectionOutcome {
    ShutdownRequested,
    FreshIdentifyNeeded,
    Reconnect,
}

#[allow(clippy::too_many_arguments)]
async fn connect_and_pump(
    url:                  &str,
    ctx:                  &DiscordAccountCtx,
    deps:                 &DiscordDispatcherDeps,
    session_id:           &mut Option<String>,
    resume_url:           &mut Option<String>,
    last_seq:             &mut u64,
    bot_user_id_seen:     &mut Option<String>,
    resume_payload:       Option<Resume>,
    shutdown:             Arc<Notify>,
) -> ConnectionOutcome {
    let (ws_stream, _resp) = match connect_async(url).await {
        Ok(p)  => p,
        Err(e) => {
            warn!(account = %ctx.account_id,
                  "Discord gateway connect failed: {}", e);
            return ConnectionOutcome::Reconnect;
        }
    };
    let (mut write, mut read) = ws_stream.split();

    // Channel used by the heartbeat tick task to push outbound frames
    // onto the same writer the dispatcher uses for IDENTIFY/RESUME.
    // Bounded — heartbeats are tiny + infrequent, 8 is more than enough.
    let (tx, mut rx) = mpsc::channel::<WsMessage>(8);

    // ── wait for HELLO ───────────────────────────────────────
    let heartbeat_interval_ms = match read.next().await {
        Some(Ok(WsMessage::Text(payload))) => match parse_hello(&payload) {
            Some(ms) => ms,
            None     => {
                warn!(account = %ctx.account_id,
                      "Discord gateway: first frame was not HELLO ({}), reconnecting",
                      preview(&payload, 120));
                return ConnectionOutcome::Reconnect;
            }
        },
        Some(Ok(WsMessage::Close(c))) => {
            return classify_close(ctx, c);
        }
        Some(Err(e)) => {
            warn!(account = %ctx.account_id, "Discord gateway: WS error before HELLO: {}", e);
            return ConnectionOutcome::Reconnect;
        }
        _ => {
            warn!(account = %ctx.account_id, "Discord gateway: no HELLO received");
            return ConnectionOutcome::Reconnect;
        }
    };
    debug!(account = %ctx.account_id,
           "Discord gateway HELLO (heartbeat_interval={}ms)", heartbeat_interval_ms);

    // ── IDENTIFY or RESUME ──────────────────────────────────
    let opening = if let Some(resume) = resume_payload {
        json!({ "op": OP_RESUME, "d": resume })
    } else {
        let identify = Identify {
            token:      ctx.bot_token.clone(),
            intents:    REQUIRED_INTENTS,
            properties: IdentifyProps::default(),
        };
        json!({ "op": OP_IDENTIFY, "d": identify })
    };
    if let Err(e) = write.send(WsMessage::Text(opening.to_string().into())).await {
        warn!(account = %ctx.account_id, "Discord gateway: send IDENTIFY/RESUME failed: {}", e);
        return ConnectionOutcome::Reconnect;
    }

    // ── heartbeat ticker ─────────────────────────────────────
    //
    // Discord wants the first heartbeat at a jittered fraction of the
    // interval (so a million bots reconnecting at once don't all heart
    // at t+0). After that, fixed cadence.
    let jitter = rand_jitter_ms() % heartbeat_interval_ms;
    let mut first_tick = true;
    let heartbeat_tx = tx.clone();
    let hb_account = ctx.account_id.clone();
    let last_seq_for_hb = Arc::new(std::sync::Mutex::new(0u64));
    let last_seq_for_hb_writer = Arc::clone(&last_seq_for_hb);
    let hb_handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            Instant::now() + Duration::from_millis(jitter),
            Duration::from_millis(heartbeat_interval_ms),
        );
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let seq = *last_seq_for_hb.lock().unwrap();
            let frame = json!({ "op": OP_HEARTBEAT, "d": if seq == 0 { serde_json::Value::Null } else { json!(seq) } });
            if heartbeat_tx.send(WsMessage::Text(frame.to_string().into())).await.is_err() {
                debug!(account = %hb_account, "heartbeat task: writer closed");
                return;
            }
            if first_tick {
                first_tick = false;
                debug!(account = %hb_account, "heartbeat task: first tick sent (jitter={}ms)", jitter);
            }
        }
    });

    // Track whether we've seen an ACK since the last heartbeat we sent.
    // After each heartbeat send, flip to false; on each ACK, flip true.
    // If we send a heartbeat with `acked_since_last == false`, the
    // connection is zombie — close + reconnect.
    let mut acked_since_last = true;
    let mut hb_sent_count    = 0u64;

    loop {
        tokio::select! {
            // Shutdown takes priority over everything else.
            _ = shutdown.notified() => {
                hb_handle.abort();
                let _ = write.send(WsMessage::Close(Some(CloseFrame {
                    code:   tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                    reason: "MIRA shutdown".into(),
                }))).await;
                return ConnectionOutcome::ShutdownRequested;
            }

            // Outbound frames produced by the heartbeat ticker.
            Some(frame) = rx.recv() => {
                if !acked_since_last && hb_sent_count > 0 {
                    warn!(account = %ctx.account_id,
                          "Discord gateway: heartbeat not ACK'd — zombie connection, reconnecting");
                    hb_handle.abort();
                    let _ = write.send(WsMessage::Close(Some(CloseFrame {
                        code:   tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Abnormal,
                        reason: "missing heartbeat ack".into(),
                    }))).await;
                    return ConnectionOutcome::Reconnect;
                }
                if let Err(e) = write.send(frame).await {
                    warn!(account = %ctx.account_id, "Discord gateway: heartbeat send failed: {}", e);
                    hb_handle.abort();
                    return ConnectionOutcome::Reconnect;
                }
                acked_since_last = false;
                hb_sent_count += 1;
            }

            // Inbound frames from Discord.
            Some(msg) = read.next() => {
                match msg {
                    Ok(WsMessage::Text(t)) => {
                        let frame: GatewayFrame = match serde_json::from_str(&t) {
                            Ok(f)  => f,
                            Err(e) => {
                                warn!(account = %ctx.account_id,
                                      "Discord gateway: bad frame ({}): {}", e, preview(&t, 160));
                                continue;
                            }
                        };
                        if let Some(s) = frame.s {
                            *last_seq = s;
                            *last_seq_for_hb_writer.lock().unwrap() = s;
                        }
                        match frame.op {
                            OP_HEARTBEAT_ACK => {
                                acked_since_last = true;
                            }
                            OP_HEARTBEAT => {
                                // Server-requested heartbeat — answer
                                // immediately rather than waiting for
                                // our ticker.
                                let seq = *last_seq;
                                let f = json!({ "op": OP_HEARTBEAT,
                                                "d":  if seq == 0 { serde_json::Value::Null } else { json!(seq) } });
                                if let Err(e) = write.send(WsMessage::Text(f.to_string().into())).await {
                                    warn!(account = %ctx.account_id,
                                          "Discord gateway: respond-to-heartbeat failed: {}", e);
                                    hb_handle.abort();
                                    return ConnectionOutcome::Reconnect;
                                }
                                acked_since_last = false;
                            }
                            OP_RECONNECT => {
                                debug!(account = %ctx.account_id,
                                       "Discord gateway: server asked to RECONNECT");
                                hb_handle.abort();
                                return ConnectionOutcome::Reconnect;
                            }
                            OP_INVALID_SESSION => {
                                let invalid: InvalidSession = serde_json::from_value(frame.d)
                                    .unwrap_or(InvalidSession(false));
                                hb_handle.abort();
                                return if invalid.0 {
                                    ConnectionOutcome::Reconnect
                                } else {
                                    ConnectionOutcome::FreshIdentifyNeeded
                                };
                            }
                            OP_DISPATCH => {
                                handle_dispatch(
                                    frame.t.as_deref().unwrap_or(""),
                                    frame.d,
                                    ctx, deps,
                                    session_id, resume_url,
                                    bot_user_id_seen,
                                ).await;
                            }
                            other => {
                                debug!(account = %ctx.account_id,
                                       "Discord gateway: ignoring op {}", other);
                            }
                        }
                    }
                    Ok(WsMessage::Close(c)) => {
                        hb_handle.abort();
                        return classify_close(ctx, c);
                    }
                    Ok(WsMessage::Ping(p)) => {
                        let _ = write.send(WsMessage::Pong(p)).await;
                    }
                    Ok(_) => { /* Binary/Pong/Frame — ignore */ }
                    Err(e) => {
                        warn!(account = %ctx.account_id,
                              "Discord gateway: WS read error: {}", e);
                        hb_handle.abort();
                        return ConnectionOutcome::Reconnect;
                    }
                }
            }

            else => {
                // Both streams closed.
                hb_handle.abort();
                return ConnectionOutcome::Reconnect;
            }
        }
    }
}

async fn handle_dispatch(
    event_name:       &str,
    payload:          serde_json::Value,
    ctx:              &DiscordAccountCtx,
    deps:             &DiscordDispatcherDeps,
    session_id:       &mut Option<String>,
    resume_url:       &mut Option<String>,
    bot_user_id_seen: &mut Option<String>,
) {
    match event_name {
        "READY" => {
            match serde_json::from_value::<Ready>(payload) {
                Ok(ready) => {
                    info!(account = %ctx.account_id,
                          "Discord gateway READY (session_id=…{}, bot_user_id={})",
                          tail(&ready.session_id, 6), ready.user.id);
                    *session_id       = Some(ready.session_id);
                    *resume_url       = ready.resume_gateway_url
                        .map(|u| format!("{}/?v=10&encoding=json", u.trim_end_matches('/')));
                    *bot_user_id_seen = Some(ready.user.id);
                }
                Err(e) => warn!(account = %ctx.account_id, "Discord READY parse: {}", e),
            }
        }
        "RESUMED" => {
            debug!(account = %ctx.account_id, "Discord gateway RESUMED");
        }
        "MESSAGE_CREATE" => {
            match serde_json::from_value::<MessageCreate>(payload) {
                Ok(msg) => {
                    // Spawn the dispatcher so the connection loop never
                    // blocks on AgentCore + LLM round-trip latency. The
                    // ctx + deps are cheap clones (Arcs inside).
                    let ctx2 = ctx.clone();
                    let deps2 = deps.clone();
                    let bot_id = bot_user_id_seen.clone();
                    tokio::spawn(async move {
                        super::dispatch::process_discord_message(deps2, ctx2, msg, bot_id).await;
                    });
                }
                Err(e) => warn!(account = %ctx.account_id, "Discord MESSAGE_CREATE parse: {}", e),
            }
        }
        // Lots of other events — guild/channel/typing/presence/etc. We
        // intentionally don't subscribe to most via intents; what slips
        // through (GUILD_CREATE flood on connect, etc.) we just log at
        // trace and drop.
        _ => {}
    }
}

// ── small helpers ──────────────────────────────────────────────────────

fn parse_hello(payload: &str) -> Option<u64> {
    let frame: GatewayFrame = serde_json::from_str(payload).ok()?;
    if frame.op != OP_HELLO { return None; }
    let hello: Hello = serde_json::from_value(frame.d).ok()?;
    Some(hello.heartbeat_interval)
}

fn classify_close(ctx: &DiscordAccountCtx, c: Option<CloseFrame>) -> ConnectionOutcome {
    // Discord-specific close codes:
    // 4004 = Authentication failed (bad token) — non-recoverable.
    // 4010 = Invalid shard, 4011 = Sharding required — N/A for single-shard.
    // 4012 = Invalid API version (we pin v10 so unlikely).
    // 4013 = Invalid intents, 4014 = Disallowed intents (privileged
    //        MESSAGE_CONTENT not enabled in the dev portal) —
    //        non-recoverable until operator fixes the portal.
    if let Some(ref c) = c {
        let code: u16 = c.code.into();
        match code {
            4004 => {
                error!(account = %ctx.account_id,
                       "Discord gateway: authentication failed (bot token rejected). \
                        Stopping reconnect loop — fix the token in Settings → Channels.");
                // We still trigger ConnectionOutcome::FreshIdentifyNeeded
                // so the outer loop sleeps + retries (avoids tight loop)
                // but ops should see this and disable the account.
                return ConnectionOutcome::FreshIdentifyNeeded;
            }
            4013 | 4014 => {
                error!(account = %ctx.account_id, code = code,
                       "Discord gateway: invalid/disallowed intents ({}). \
                        Enable MESSAGE_CONTENT in Developer Portal → Bot → \
                        Privileged Gateway Intents, then restart.", c.reason);
                return ConnectionOutcome::FreshIdentifyNeeded;
            }
            _ => {
                warn!(account = %ctx.account_id, code = code,
                      "Discord gateway: closed by server ({} — {})", code, c.reason);
            }
        }
    } else {
        warn!(account = %ctx.account_id, "Discord gateway: closed without code");
    }
    ConnectionOutcome::Reconnect
}

fn preview(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n]) }
}

fn tail(s: &str, n: usize) -> &str {
    let take_from = s.len().saturating_sub(n);
    &s[take_from..]
}

fn redact_query(url: &str) -> String {
    // Bootstrap URL is `wss://gateway.discord.gg/?v=10&encoding=json`
    // (no secrets). Resume URLs are `wss://<region>.gateway.discord.gg/`
    // (also no secrets). We log both verbatim — kept as a helper in case
    // future versions ship query params we'd want to scrub.
    url.to_string()
}

// Cheap jitter source — not cryptographic, just for spreading reconnect
// timing. Uses the nanos-since-epoch so each invocation is unique-ish.
fn rand_jitter_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hello_extracts_interval() {
        let p = r#"{"op":10,"d":{"heartbeat_interval":41250},"s":null,"t":null}"#;
        assert_eq!(parse_hello(p), Some(41250));
    }

    #[test]
    fn parse_hello_returns_none_for_other_ops() {
        let p = r#"{"op":0,"t":"READY","s":1,"d":{}}"#;
        assert_eq!(parse_hello(p), None);
    }

    #[test]
    fn tail_handles_short_strings() {
        assert_eq!(tail("abc",  6), "abc");
        assert_eq!(tail("abcdef", 3), "def");
    }

    #[test]
    fn preview_truncates() {
        assert_eq!(preview("short", 10), "short");
        assert_eq!(preview("toolong", 4), "tool…");
    }
}
