// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/signal_cli/sse_listener.rs
//! Subscribes to signal-cli's SSE event stream at /api/v1/events and routes
//! incoming Signal messages through AgentCore, replying via JSON-RPC send.
//!
//! signal-cli 0.14.x emits SSE events without a space after the colon:
//!   event:receive
//!   data:{"envelope":{"sourceNumber":"+...","dataMessage":{"message":"..."},...}}

use std::sync::Arc;
use std::time::Duration;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::agent::{AgentCore, TurnContext};
use crate::auth::LocalAuthService;
use crate::history::HistoryStore;
use crate::providers::signal_cli::SignalCliClient;
use crate::stt::types::{AudioInputFormat, TranscribeRequest};
use crate::stt::SttService;
use crate::tts::TtsService;
use crate::tts::types::{AudioCodec, OutputFormat};
use crate::voice::{parse_user_prefs, resolve_voice, ResolvedVoice, ResponsePolicy};

// ── Deserialization types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SseEvent {
    envelope: Option<Envelope>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    source_number: Option<String>,
    source:        Option<String>,
    data_message:  Option<DataMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataMessage {
    message: Option<String>,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

/// signal-cli attachment metadata. Voice notes set `voice_note: true` and
/// carry an `id` we can pull from `/api/v1/attachments/{id}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attachment {
    id:           Option<String>,
    content_type: Option<String>,
    #[serde(default)]
    voice_note:   bool,
}

impl Attachment {
    /// Heuristic: treat any `audio/*` attachment as a voice note even when
    /// signal-cli omitted the `voiceNote: true` flag. Some signal-cli builds
    /// strip that flag in their JSON-RPC output, leaving only the content
    /// type to identify the payload as voice. Filtering by `audio/*` keeps
    /// the door closed for images and documents.
    fn is_voice_like(&self) -> bool {
        if self.voice_note { return true; }
        match self.content_type.as_deref() {
            Some(ct) => ct.to_ascii_lowercase().starts_with("audio/"),
            None     => false,
        }
    }
}

// ── Parsed result ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct ParsedVoiceNote {
    pub(crate) attachment_id: String,
    pub(crate) content_type:  String,
}

pub(crate) struct ParsedMessage {
    pub(crate) sender:     String,
    /// Caption / typed text. Voice-only notes leave this empty so the
    /// listener can substitute the transcript verbatim.
    pub(crate) text:       String,
    /// Set when the inbound message includes a voice-note attachment that
    /// the listener should download and transcribe before dispatching.
    pub(crate) voice_note: Option<ParsedVoiceNote>,
}

/// Extract sender and message text from a signal-cli SSE event JSON payload.
/// Returns `None` for typing indicators, receipts, sync events, and messages
/// with neither text nor a voice-note attachment.
pub(crate) fn parse_signal_event(json: &str) -> Option<ParsedMessage> {
    let event: SseEvent = serde_json::from_str(json)
        .map_err(|e| warn!("SSE parse error: {} — raw: {}", e, &json[..json.len().min(300)]))
        .ok()?;

    let envelope  = event.envelope?;
    let data_msg  = envelope.data_message?; // typing / receipt / sync → skip
    let sender    = envelope.source_number.or(envelope.source)?;

    let text = data_msg.message
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_default();

    let attachments = data_msg.attachments;
    let voice_note = attachments.iter()
        .find(|a| a.is_voice_like() && a.id.is_some())
        .cloned()
        .map(|a| ParsedVoiceNote {
            attachment_id: a.id.unwrap_or_default(),
            content_type:  a.content_type.unwrap_or_default(),
        });

    if text.is_empty() && voice_note.is_none() {
        // Silently dropping a message that arrived with attachments would
        // hide the "I sent a voice note and saw nothing" failure mode from
        // the operator. Surface it as a warn so the cause is visible at
        // INFO log level.
        if !attachments.is_empty() {
            let summary: Vec<String> = attachments.iter().map(|a| format!(
                "{{ct={:?}, voiceNote={}, id={}}}",
                a.content_type.as_deref().unwrap_or(""),
                a.voice_note,
                if a.id.is_some() { "yes" } else { "no" },
            )).collect();
            warn!(
                "Signal event from {} dropped — no text and no voice-note-like \
                 attachment ({} attachments inspected): [{}]",
                sender,
                attachments.len(),
                summary.join(", "),
            );
        }
        return None;
    }

    Some(ParsedMessage { sender, text, voice_note })
}

// ── Listener ──────────────────────────────────────────────────────────────────

pub struct SignalSseListener {
    port:          u16,
    phone_number:  String,
    /// signal-cli's `--config` directory. Voice-note attachments are read
    /// from `<data_dir>/attachments/<id>` directly off disk because signal-
    /// cli's native HTTP daemon (the JSON-RPC server we point at) does not
    /// expose an attachment-fetch endpoint — only `/api/v1/rpc` and
    /// `/api/v1/events`. Hitting any other path returns 404 "No context
    /// found for request" from `com.sun.net.httpserver`.
    data_dir:      String,
    agent_core:    Arc<AgentCore>,
    history:       Option<Arc<HistoryStore>>,
    /// Owning MIRA user. Stamped onto every inbound conversation so each
    /// Signal account's traffic surfaces only in its owner's sidebar.
    owner_user_id: String,
    /// Looks up the MIRA user UUID from the inbound sender's phone via
    /// `users.phone`, so memory and profile context follow the user across
    /// channels. `None` falls back to owner-attribution (1:1 bots).
    auth:          Option<Arc<LocalAuthService>>,
    /// Speech-to-text service used to transcribe inbound voice notes. When
    /// `None` (STT disabled at gateway start) voice-note-only messages are
    /// dropped with a warning rather than silently ignored.
    stt:           Option<SttService>,
    /// Text-to-speech service used to synthesise outbound voice notes when
    /// `tts.routing.signal` pins a backend. `None` disables voice replies
    /// regardless of routing config.
    tts:           Option<TtsService>,
}

impl SignalSseListener {
    pub fn new(
        port:          u16,
        phone_number:  String,
        data_dir:      String,
        agent_core:    Arc<AgentCore>,
        history:       Option<Arc<HistoryStore>>,
        owner_user_id: String,
        auth:          Option<Arc<LocalAuthService>>,
        stt:           Option<SttService>,
        tts:           Option<TtsService>,
    ) -> Self {
        Self { port, phone_number, data_dir, agent_core, history, owner_user_id, auth, stt, tts }
    }

    /// Map an inbound Signal sender (E.164 phone) to a MIRA user UUID.
    /// See [`Self::auth`] doc for the lookup chain.
    fn resolve_sender_user_id(&self, sender: &str) -> String {
        if let Some(ref auth) = self.auth {
            match auth.find_by_phone(sender) {
                Ok(Some(u)) => {
                    debug!(
                        "Signal sender {} → MIRA user {} ({}) via users.phone",
                        sender, u.id, u.username,
                    );
                    return u.id;
                }
                Ok(None) => {
                    debug!(
                        "Signal sender {} not claimed by any users.phone — \
                         falling back to bot owner {}",
                        sender, self.owner_user_id,
                    );
                }
                Err(e) => warn!("users.phone lookup failed for {}: {}", sender, e),
            }
        }
        // Owner fallback works for the typical 1:1 bot deployment.
        if !self.owner_user_id.is_empty() {
            return self.owner_user_id.clone();
        }
        sender.to_owned()
    }

    /// Connect to the SSE stream and process events until the task is cancelled.
    /// Reconnects automatically on connection drop.
    pub async fn run(self) {
        let url = format!("http://127.0.0.1:{}/api/v1/events", self.port);
        info!("Signal SSE listener starting at {}", url);

        loop {
            match self.connect_and_stream(&url).await {
                Ok(())  => info!("Signal SSE stream ended — reconnecting"),
                Err(e)  => warn!("Signal SSE error: {} — reconnecting in 5s", e),
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    }

    async fn connect_and_stream(&self, url: &str) -> Result<(), String> {
        // 0.112.1 — SSE streams are designed to stay open for hours
        // while idle. reqwest's `.timeout()` is the OVERALL request
        // deadline (including body read), so a 5-minute timeout was
        // killing healthy idle connections every 5 min and producing
        // the "error decoding response body" warning on reconnect.
        // Use `.connect_timeout()` instead to bound only the initial
        // TCP/HTTP handshake; real failures (signal-cli dying, network
        // partition) still surface, and the slice-3b
        // `channel.signal.daemon_alive` detector catches silent
        // process death at the OS layer.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;

        let response = client
            .get(url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let mut byte_stream = response.bytes_stream();
        let mut buf = String::new();

        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // SSE events are delimited by a blank line (\n\n)
            while let Some(pos) = buf.find("\n\n") {
                let event_block = buf[..pos].to_string();
                buf = buf[pos + 2..].to_string();

                for line in event_block.lines() {
                    debug!("SSE line: {}", &line[..line.len().min(200)]);
                    // signal-cli omits the space: `data:{...}` not `data: {...}`
                    if let Some(json) = line.strip_prefix("data:").map(str::trim_start) {
                        self.handle_event(json).await;
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_event(&self, json: &str) {
        let msg = match parse_signal_event(json) {
            Some(m) => m,
            None    => return,
        };
        let inbound_was_voice = msg.voice_note.is_some();

        // Voice-note ingest: if the inbound message carries a voice-note
        // attachment, fetch the audio bytes from signal-cli and transcribe
        // them. The transcript replaces (or augments) the text content; we
        // auto-send the agent's reply without confirming, matching the
        // channel UX rule that voice notes flow end-to-end.
        let mut effective_text = msg.text.clone();
        if let Some(vn) = msg.voice_note.as_ref() {
            match self.transcribe_voice_note(vn).await {
                Ok(transcript) => {
                    info!(
                        "Signal voice note from {} transcribed ({} chars)",
                        msg.sender, transcript.len(),
                    );
                    effective_text = if effective_text.is_empty() {
                        transcript
                    } else {
                        format!("{}\n\n[voice]: {}", effective_text, transcript)
                    };
                }
                Err(e) => {
                    warn!(
                        "Signal voice note from {} failed to transcribe: {} \
                         — falling back to text content",
                        msg.sender, e,
                    );
                    if effective_text.is_empty() {
                        // Nothing left to send — don't bother the agent.
                        return;
                    }
                }
            }
        }

        if effective_text.is_empty() {
            // Defensive — should not happen since parse_signal_event filters
            // empty/no-voice messages, but keeps the agent loop safe.
            return;
        }

        info!(
            "Signal message from {}: {}",
            msg.sender,
            &effective_text[..effective_text.len().min(80)],
        );

        // Start the "MIRA is typing…" indicator immediately so the sender sees
        // activity in their Signal app while the agent is generating. The loop
        // re-sends every 10s (Signal expires the bubble at ~15s) and stops
        // the moment `cancel_tx` fires right before we send the reply.
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        tokio::spawn(run_typing_loop(
            self.port,
            self.phone_number.clone(),
            msg.sender.clone(),
            cancel_rx,
        ));

        // Resolve the inbound phone to a MIRA user UUID so the agent's
        // memory and profile context lock onto the right person. Order:
        //   1. `users.phone` match (cross-channel identity — preferred);
        //   2. fall back to the bot's owner (1:1 bots where the only
        //      person who'd ever message is the owner themselves);
        //   3. final fallback to the raw sender phone (anonymous turn —
        //      memory simply scoped per phone for the session).
        let resolved_user_id = self.resolve_sender_user_id(&msg.sender);

        // Session scoped to owner+sender so two owners chatting with the
        // same external contact don't share context.
        let session_id = format!("signal-{}-{}", self.owner_user_id, msg.sender);

        // Trusted identity injection so user-tier tools (recall_history,
        // automations.*, etc.) can scope to the right MIRA user. Without
        // this the agent reasons fine but every automations tool call
        // fails with "automations tool called without caller identity"
        // — the webhook handler at server::handlers::signal does the same
        // wiring; the SSE path was missing it.
        let mut inject = serde_json::Map::new();
        inject.insert(
            "_user_id".to_string(),
            serde_json::Value::String(resolved_user_id.clone()),
        );
        let mut turn_ctx = TurnContext { inject_tool_args: inject, ..TurnContext::default() };

        // Resolve the persisted thread up-front so the agent can rehydrate this
        // conversation's context on a cache miss (restart / idle eviction); the
        // record-turn below reuses the same id. The owner (MIRA user) owns the
        // conversation row, and `external_user_id` is the Signal contact.
        let history_conv = self.history.as_ref().and_then(|hist| {
            hist.find_or_create_external_conversation(
                &self.owner_user_id, "signal", &msg.sender,
                Some(&truncate_title(&effective_text)),
            ).map_err(|e| warn!("find_or_create_external_conversation failed: {}", e)).ok()
        });
        if let Some(ref conv) = history_conv {
            turn_ctx.conversation_id = Some(conv.id.clone());
        }

        let rx = match self.agent_core
            .process_with_context(
                &session_id, &resolved_user_id, "signal", &effective_text,
                None, turn_ctx,
            )
            .await
        {
            Ok(rx)  => rx,
            Err(e)  => {
                let _ = cancel_tx.send(());
                warn!("AgentCore error for {}: {}", msg.sender, e);
                return;
            }
        };

        let (response_text, _) = AgentCore::collect_response(rx).await;

        // Stop typing before dispatching so clients don't briefly show both.
        let _ = cancel_tx.send(());

        // Record turn in history. The owner (MIRA user) owns the
        // conversation row, and `external_user_id` is the Signal contact
        // — per-sender dedup falls out of the triple key.
        if let (Some(ref hist), Some(ref conv)) = (self.history.as_ref(), history_conv.as_ref()) {
            if let Err(e) = hist.record_turn(&conv.id, &effective_text, &response_text, None, None) {
                warn!("Failed to record Signal turn in history: {}", e);
            }
        }

        let client = SignalCliClient::new(self.port, self.phone_number.clone());
        self.dispatch_signal_reply(&client, &msg.sender, &response_text, inbound_was_voice).await;
    }

    /// Dispatch the assistant's reply to the Signal recipient. Voice
    /// replies are gated by the layered prefs resolver (bot owner's
    /// `users.voice_prefs.signal` over server defaults over the built-in
    /// `Never` fallback). When the policy permits voice we try the
    /// voice-note path; on any failure (synth error, codec mismatch,
    /// tempfile write, RPC error) we fall back to plain text. Per the
    /// channel UX rule the transcript ALWAYS travels with the audio
    /// (signal-cli's `message` field), so the reader can scan without
    /// playing the clip.
    async fn dispatch_signal_reply(
        &self,
        client:            &SignalCliClient,
        recipient:         &str,
        response_text:     &str,
        inbound_was_voice: bool,
    ) {
        let resolved = self.resolve_voice_for_owner("signal");
        let want_voice = match resolved.policy {
            ResponsePolicy::Always       => true,
            ResponsePolicy::OnVoiceInput => inbound_was_voice,
            ResponsePolicy::Never        => false,
        };
        debug!(
            "Signal reply policy: {} (inbound_was_voice={}) → want_voice={}",
            resolved.policy.as_str(), inbound_was_voice, want_voice,
        );

        if want_voice {
            if let Some(buf) = synth_signal_voice(
                self.tts.as_ref(),
                response_text,
                resolved.voice_id.as_deref(),
            ).await {
                // Write OGG/Opus bytes to a tempfile signal-cli can read.
                // The file name extension matters — `.oga`/`.ogg` is what
                // Signal clients accept as a voice note. The file is
                // dropped (and deleted) when this scope ends, after the
                // RPC has returned.
                match write_voice_tempfile(&buf.bytes) {
                    Ok(tmp) => {
                        let path = tmp.path().to_string_lossy().to_string();
                        match client.send_with_attachments(
                            vec![recipient.to_string()],
                            response_text,
                            &[path],
                        ).await {
                            Ok(())  => {
                                info!("Signal voice reply sent to {}", recipient);
                                return;
                            }
                            Err(e) => warn!(
                                "Signal send_with_attachments to {} failed: {} \
                                 — falling back to text",
                                recipient, e,
                            ),
                        }
                    }
                    Err(e) => warn!(
                        "Failed to write Signal voice tempfile: {} — falling back to text",
                        e,
                    ),
                }
            }
        }
        match client.send(vec![recipient.to_string()], response_text).await {
            Ok(())  => info!("Signal reply sent to {}", recipient),
            Err(e)  => warn!("Failed to send reply to {}: {}", recipient, e),
        }
    }

    /// Look up the bot owner's voice prefs and merge over server defaults.
    /// The owner — not the inbound sender — is the MIRA user the bot
    /// belongs to, so "this is how my agent should sound" tracks the
    /// right person. Falls back gracefully when auth lookups fail.
    fn resolve_voice_for_owner(&self, channel: &str) -> ResolvedVoice {
        let server_defaults = self.tts.as_ref()
            .map(|t| t.voice_prefs_defaults())
            .unwrap_or_default();
        let user_prefs = self.auth.as_ref()
            .and_then(|a| a.get_user(&self.owner_user_id).ok().flatten())
            .map(|u| parse_user_prefs(u.voice_prefs.as_deref()))
            .unwrap_or_default();
        resolve_voice(channel, Some(&user_prefs), &server_defaults)
    }

    /// Pull the voice-note bytes from signal-cli and run them through the
    /// configured STT service. Returns the transcript verbatim — the caller
    /// is responsible for context-tagging it before handing it to AgentCore.
    async fn transcribe_voice_note(&self, vn: &ParsedVoiceNote) -> Result<String, String> {
        let stt = self.stt.as_ref()
            .ok_or_else(|| "STT not configured at gateway start".to_string())?;

        // signal-cli's native daemon does not expose an attachment HTTP
        // endpoint — read the bytes straight off disk. Try the per-account
        // path first (MIRA's `per_account_data_dir` appends the account UUID
        // when it spawns a fresh daemon); fall back to the parent path so
        // we still find attachments when MIRA is talking to a daemon that
        // was started with `--config <shared_root>` (no account suffix).
        let attachment_id = &vn.attachment_id;
        let candidates: [std::path::PathBuf; 2] = [
            std::path::Path::new(&self.data_dir).join("attachments").join(attachment_id),
            std::path::Path::new(&self.data_dir)
                .parent()
                .unwrap_or_else(|| std::path::Path::new(&self.data_dir))
                .join("attachments")
                .join(attachment_id),
        ];
        let bytes = {
            let mut found = None;
            let mut last_err = None;
            for path in &candidates {
                debug!("Reading Signal attachment from disk: {}", path.display());
                match tokio::fs::read(path).await {
                    Ok(b)  => { found = Some(b); break; }
                    Err(e) => last_err = Some(format!("{}: {}", path.display(), e)),
                }
            }
            found.ok_or_else(|| format!(
                "read_attachment({}) — none of the candidate paths existed: {}",
                attachment_id,
                last_err.unwrap_or_else(|| "no candidates".into()),
            ))?
        };

        // The SSE event carries the attachment-level Content-Type; Signal
        // clients are reliable here so we trust it directly without sniffing
        // the file bytes.
        let mime = vn.content_type.as_str();
        let format = AudioInputFormat::from_mime(mime);

        let req = TranscribeRequest {
            audio_bytes: bytes,
            format,
            language:    None,
        };
        let transcript = stt
            .transcribe(req, None, Some("signal"))
            .await
            .map_err(|e| e.to_string())?;
        Ok(transcript.text)
    }
}

/// Synthesise an outbound voice note for the Signal channel. Returns `None`
/// (with a warning) when TTS is unavailable or the resolved backend can't
/// produce OGG/Opus — Signal voice notes require Opus, and transcoding
/// WAV→Opus inline would mean pulling in libopus, so we choose the
/// fallback-to-text path instead.
///
/// We deliberately do NOT short-circuit when `tts.routing.signal` is empty:
/// if the user's policy is `Always` (or `OnVoiceInput` and the inbound was
/// voice) we honour that by falling through to `tts.default_backend`. The
/// `routing.signal` field is a *pin* used to override the default, not a
/// gate on whether voice replies happen at all.
pub(crate) async fn synth_signal_voice(
    tts:               Option<&TtsService>,
    text:              &str,
    voice_id_override: Option<&str>,
) -> Option<crate::tts::types::AudioBuffer> {
    let Some(tts) = tts else {
        warn!("tts: signal voice reply skipped — TTS service unavailable, falling back to text");
        return None;
    };
    if !tts.enabled() {
        warn!("tts: signal voice reply skipped — `tts.enabled = false`, falling back to text");
        return None;
    }

    // `backend = None` lets the service apply per-channel routing → default.
    // Request WAV, not OggOpus: we transcode to OGG/Opus locally below, and
    // OpenAI-compatible servers that only support wav/pcm (e.g. self-hosted
    // Chatterbox/kokoro) reject `opus` with HTTP 400 → robotic piper fallback.
    let buf = match tts.speak(text, voice_id_override, None, Some(OutputFormat::Wav), None, Some("signal")).await {
        Ok(buf) => buf,
        Err(e) => {
            warn!("tts: synth for signal failed: {} — falling back to text", e);
            return None;
        }
    };
    if matches!(buf.codec, AudioCodec::OggOpus) {
        return Some(buf);
    }
    // Backend gave us WAV/PCM (Piper, eSpeak). Transcode to OGG/Opus so the
    // attachment is something Signal will actually play.
    let original = buf.codec.clone();
    match crate::tts::encoder::ensure_ogg_opus(buf) {
        Ok(transcoded) => Some(transcoded),
        Err(e) => {
            warn!(
                "tts: signal voice reply skipped — transcode {:?} → OGG/Opus failed: {}",
                original, e,
            );
            None
        }
    }
}

/// Persist `bytes` to a `.oga` tempfile so signal-cli can read it from disk.
/// Returns the `NamedTempFile` so the caller controls its lifetime — drop
/// after the RPC returns to remove the file.
pub(crate) fn write_voice_tempfile(bytes: &[u8]) -> std::io::Result<tempfile::NamedTempFile> {
    use std::io::Write;
    let mut tmp = tempfile::Builder::new()
        .prefix("mira-signal-voice-")
        .suffix(".oga")
        .tempfile()?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    Ok(tmp)
}

/// Produce a sidebar-friendly conversation title from the first inbound
/// message. Matches the 60-char cap with a trailing ellipsis used by the
/// web chat handler so titles look consistent across channels.
fn truncate_title(text: &str) -> String {
    let t = text.trim();
    let first_line = t.lines().next().unwrap_or(t);
    let char_count = first_line.chars().count();
    if char_count <= 60 {
        first_line.to_owned()
    } else {
        let cut: String = first_line.chars().take(57).collect();
        format!("{}…", cut)
    }
}

/// Keep the "MIRA is typing…" indicator alive for one turn.
///
/// The first `sendTyping` call is logged at `warn!` if it fails so the
/// operator sees daemon/RPC misconfiguration; subsequent failures drop to
/// `debug!` to avoid spam during a single slow turn.
async fn run_typing_loop(
    signal_port:   u16,
    phone_number:  String,
    recipient:     String,
    mut cancel_rx: oneshot::Receiver<()>,
) {
    let client = SignalCliClient::new(signal_port, phone_number);
    let mut first = true;

    loop {
        match client.send_typing(vec![recipient.clone()], false).await {
            Ok(())  if first => {
                info!("Signal typing indicator started for {}", recipient);
                first = false;
            }
            Ok(())  => {}
            Err(e) if first => {
                warn!("Signal typing indicator failed (first send): {} — subsequent \
                       failures will be logged at debug", e);
                first = false;
            }
            Err(e) => debug!("Signal typing indicator refresh failed: {}", e),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = &mut cancel_rx => break,
        }
    }

    // Final stop so clients clear the bubble right away.
    if let Err(e) = client.send_typing(vec![recipient], true).await {
        debug!("Signal typing stop failed: {}", e);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Real-world payload shape from signal-cli 0.14.x (trimmed)
    fn data_message_json(sender: &str, text: &str) -> String {
        format!(
            r#"{{"envelope":{{"source":"{sender}","sourceNumber":"{sender}","sourceName":"Test","sourceDevice":1,"timestamp":1776437901815,"serverReceivedTimestamp":1776437902073,"serverDeliveredTimestamp":1776437902074,"dataMessage":{{"timestamp":1776437901815,"message":"{text}","expiresInSeconds":0,"viewOnce":false}}}}}}"#
        )
    }

    #[test]
    fn test_parse_data_message() {
        let json = data_message_json("+61421938567", "Hello MIRA");
        let msg = parse_signal_event(&json).unwrap();
        assert_eq!(msg.sender, "+61421938567");
        assert_eq!(msg.text, "Hello MIRA");
    }

    #[test]
    fn test_parse_typing_indicator_ignored() {
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","typingMessage":{"action":"STARTED","timestamp":1776437897301}}}"#;
        assert!(parse_signal_event(json).is_none());
    }

    #[test]
    fn test_parse_receipt_ignored() {
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","receiptMessage":{"when":1776437902000,"isDelivery":true,"isRead":false,"timestamps":[1776437901815]}}}"#;
        assert!(parse_signal_event(json).is_none());
    }

    #[test]
    fn test_parse_sync_message_ignored() {
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","syncMessage":{}}}"#;
        assert!(parse_signal_event(json).is_none());
    }

    #[test]
    fn test_parse_empty_text_ignored() {
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","dataMessage":{"timestamp":123,"message":""}}}"#;
        assert!(parse_signal_event(json).is_none());
    }

    #[test]
    fn test_parse_null_text_ignored() {
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","dataMessage":{"timestamp":123}}}"#;
        assert!(parse_signal_event(json).is_none());
    }

    #[test]
    fn test_parse_no_envelope_ignored() {
        assert!(parse_signal_event(r#"{"someOtherField":"value"}"#).is_none());
        assert!(parse_signal_event("{}").is_none());
    }

    #[test]
    fn test_parse_falls_back_to_source_when_no_source_number() {
        // source_number absent, only source present
        let json = r#"{"envelope":{"source":"+61421938567","dataMessage":{"timestamp":1,"message":"hi"}}}"#;
        let msg = parse_signal_event(json).unwrap();
        assert_eq!(msg.sender, "+61421938567");
        assert_eq!(msg.text, "hi");
    }

    #[test]
    fn test_parse_prefers_source_number_over_source() {
        let json = r#"{"envelope":{"source":"+11111111111","sourceNumber":"+22222222222","dataMessage":{"timestamp":1,"message":"hi"}}}"#;
        let msg = parse_signal_event(json).unwrap();
        assert_eq!(msg.sender, "+22222222222");
    }

    #[test]
    fn test_sse_data_prefix_no_space() {
        // signal-cli sends `data:{...}` without a space — verify strip logic
        let line = data_message_json("+61421938567", "no-space test");
        let prefixed = format!("data:{}", line);
        let json = prefixed.strip_prefix("data:").map(str::trim_start).unwrap();
        let msg = parse_signal_event(json).unwrap();
        assert_eq!(msg.text, "no-space test");
    }

    #[test]
    fn test_sse_data_prefix_with_space_also_works() {
        // trim_start handles optional space gracefully
        let line = data_message_json("+61421938567", "space test");
        let prefixed = format!("data: {}", line);
        let json = prefixed.strip_prefix("data:").map(str::trim_start).unwrap();
        let msg = parse_signal_event(json).unwrap();
        assert_eq!(msg.text, "space test");
    }

    #[test]
    fn test_parse_voice_note_only_message() {
        // signal-cli payload for a voice-only voice note (no caption text).
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","dataMessage":{"timestamp":1,"attachments":[{"contentType":"audio/aac","filename":null,"id":"abc-123","size":12345,"voiceNote":true}]}}}"#;
        let msg = parse_signal_event(json).expect("should parse");
        assert_eq!(msg.sender, "+61421938567");
        assert!(msg.text.is_empty());
        let vn = msg.voice_note.expect("voice_note set");
        assert_eq!(vn.attachment_id, "abc-123");
        assert_eq!(vn.content_type, "audio/aac");
    }

    #[test]
    fn test_parse_voice_note_with_caption() {
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","dataMessage":{"timestamp":1,"message":"check this","attachments":[{"contentType":"audio/ogg","id":"xyz-9","size":1024,"voiceNote":true}]}}}"#;
        let msg = parse_signal_event(json).expect("should parse");
        assert_eq!(msg.text, "check this");
        let vn = msg.voice_note.expect("voice_note set");
        assert_eq!(vn.attachment_id, "xyz-9");
        assert_eq!(vn.content_type, "audio/ogg");
    }

    #[test]
    fn test_parse_non_voice_attachment_ignored() {
        // An image attachment without voiceNote=true must NOT be returned
        // as a voice note. Since there's also no text, parse returns None.
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","dataMessage":{"timestamp":1,"attachments":[{"contentType":"image/jpeg","id":"img-1","size":50000,"voiceNote":false}]}}}"#;
        assert!(parse_signal_event(json).is_none());
    }

    #[test]
    fn test_parse_audio_attachment_without_voice_note_flag_is_treated_as_voice() {
        // Some signal-cli builds drop the voiceNote flag on the JSON-RPC
        // event even though the inbound attachment was a voice memo. The
        // parser falls back to the audio/* content-type heuristic so the
        // listener still transcribes it instead of silently dropping the
        // event.
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","dataMessage":{"timestamp":1,"attachments":[{"contentType":"audio/aac","id":"vn-noflag","size":2048,"voiceNote":false}]}}}"#;
        let msg = parse_signal_event(json).expect("should parse via audio/* heuristic");
        let vn = msg.voice_note.expect("voice_note set");
        assert_eq!(vn.attachment_id, "vn-noflag");
        assert_eq!(vn.content_type, "audio/aac");
    }

    #[test]
    fn test_parse_voice_note_alongside_image_picks_voice_note() {
        let json = r#"{"envelope":{"source":"+61421938567","sourceNumber":"+61421938567","dataMessage":{"timestamp":1,"attachments":[{"contentType":"image/jpeg","id":"img-1","voiceNote":false},{"contentType":"audio/aac","id":"vn-2","voiceNote":true}]}}}"#;
        let msg = parse_signal_event(json).expect("should parse");
        let vn = msg.voice_note.expect("voice_note set");
        assert_eq!(vn.attachment_id, "vn-2");
    }
}
