// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/mod.rs
pub mod app;
pub mod backend;
pub mod completion;
pub mod event;
pub mod input;
pub mod layout;
pub mod markdown;
pub mod mode;
pub mod render;
pub mod theme;

use std::io;
use std::sync::Arc;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use mira::agent::stream::StreamEvent;
use mira::config::MiraConfig;
use crate::tui::app::{AppState, LastTurnCost, Role};
use crate::tui::backend::{CatalogSnapshot, TuiBackend};
use crate::tui::event::{AppEvent, spawn_event_tasks};
use crate::tui::input::{handle_key, TuiAction};
use crate::tui::layout::LayoutMode;
use crate::tui::render::render_all;
use crate::tui::theme::Theme;

// Configuration for the TUI appearance (sourced from Config::tui).
pub struct TuiUiConfig {
    pub layout:         LayoutMode,
    pub theme_name:     String,
    // `"local"` or `"server"` — drives the status-bar indicator and the
    // server-unreachable banner (banner only appears in server mode).
    pub backend_label:  String,
}

impl Default for TuiUiConfig {
    fn default() -> Self {
        Self {
            layout:         LayoutMode::Standard,
            theme_name:     "mira-dark".to_string(),
            backend_label:  "local".to_string(),
        }
    }
}

// Main TUI entry point. Sets up the terminal, runs the event loop, tears down.
pub async fn run(
    backend:   Arc<dyn TuiBackend>,
    config:    Arc<MiraConfig>,
    ui_config: TuiUiConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let term_backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(term_backend)?;

    let result = run_inner(&mut terminal, backend, config, ui_config).await;

    // Always restore terminal even on error
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_inner(
    terminal:  &mut Terminal<CrosstermBackend<io::Stdout>>,
    backend:   Arc<dyn TuiBackend>,
    config:    Arc<MiraConfig>,
    ui_config: TuiUiConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut state = AppState::new();

    // Derive display labels from config
    let provider_label = config.providers.lmstudio.url
        .contains("lmstudio")
        .then(|| "lmstudio")
        .unwrap_or("lmstudio")
        .to_string();
    let model_label    = config.providers.lmstudio.default_model.clone();

    // Apply theme and layout from config
    if let Some(t) = Theme::by_name(&ui_config.theme_name) {
        state.theme = t;
    }
    state.layout_mode    = ui_config.layout;
    state.provider_label = provider_label.clone();
    state.model_label    = model_label.clone();
    state.backend_label  = ui_config.backend_label.clone();
    state.tool_count     = backend.tool_count().await;
    state.memory_count   = backend.memory_count().await;

    // Optional resume-on-open (server mode primarily; local also works). Must
    // happen before we push the ready banner so the banner ends up last.
    if config.tui.resume_last {
        if let Some(resumed) = backend.fetch_last_tui_conversation(20).await {
            use crate::tui::backend::ResumedRole;
            for (role, content) in resumed.messages {
                let r = match role {
                    ResumedRole::User      => Role::User,
                    ResumedRole::Assistant => Role::Assistant,
                    ResumedRole::System    => Role::System,
                };
                state.push_message(r, content);
            }
            state.conv_id = Some(resumed.conv_id);
            state.push_message(
                Role::System,
                "Resumed last TUI conversation. Send a message to continue, or /clear to start fresh.".to_string(),
            );
        }
    }

    state.push_message(
        Role::System,
        format!(
            "MIRA ready \u{2014} provider: {} model: {}\nCtrl+P: palette  Tab: completions  F5: theme  F6: layout  Ctrl+C: quit",
            provider_label, model_label
        ),
    );

    let (event_tx, mut event_rx) = spawn_event_tasks();

    // Background health check via backend
    {
        let tx       = event_tx.clone();
        let backend2 = Arc::clone(&backend);
        tokio::spawn(async move {
            let ok = backend2.health_check().await;
            let _ = tx.send(AppEvent::HealthStatus(ok));
        });
    }

    // Auto-refresh OpenRouter catalog on startup. Non-blocking — fires
    // and forgets; the result lands as `OpenRouterCatalog` and surfaces
    // as a system note. Cached fetches return instantly; a cold fetch
    // takes ~1s and only delays the catalog itself, never the chat input.
    spawn_catalog_fetch(Arc::clone(&backend), event_tx.clone(), false);

    while !state.should_quit {
        terminal.draw(|f| render_all(f, &mut state))?;

        let ev = match event_rx.recv().await {
            Some(e) => e,
            None    => break,
        };

        match ev {
            AppEvent::Key(key_ev) => {
                match handle_key(&mut state, key_ev) {
                    Some(TuiAction::Quit) => state.should_quit = true,

                    Some(TuiAction::SendMessage(msg)) => {
                        state.push_message(Role::User, msg.clone());
                        state.is_streaming = true;
                        state.token_count += estimate_tokens(&msg);

                        let tx2       = event_tx.clone();
                        let backend2  = Arc::clone(&backend);
                        let conv      = state.conv_id.clone();
                        let model     = state.model_label.clone();
                        let provider  = state.provider_label.clone();
                        tokio::spawn(async move {
                            match backend2.send_message(conv, msg, model, provider).await {
                                Err(e) => { let _ = tx2.send(AppEvent::StreamError(e)); }
                                Ok(mut turn) => {
                                    if let Some(cid) = turn.conv_id {
                                        let _ = tx2.send(AppEvent::ConversationUpdated(cid));
                                    }
                                    while let Some(ev) = turn.rx.recv().await {
                                        match ev {
                                            StreamEvent::Token(tok) => { let _ = tx2.send(AppEvent::Token(tok)); }
                                            StreamEvent::Done { usage } => {
                                                let _ = tx2.send(AppEvent::StreamUsage(usage));
                                                let _ = tx2.send(AppEvent::StreamDone);
                                                break;
                                            }
                                            StreamEvent::Error(e)   => { let _ = tx2.send(AppEvent::StreamError(e)); break; }
                                            StreamEvent::Warning(w) => { let _ = tx2.send(AppEvent::StreamError(format!("Warning: {}", w))); }
                                            StreamEvent::ToolCall { name, .. } => {
                                                let _ = tx2.send(AppEvent::Token(format!("\n[tool: {}]\n", name)));
                                            }
                                            StreamEvent::ToolResult { name, output, success, .. } => {
                                                let status = if success { "ok" } else { "err" };
                                                let _ = tx2.send(AppEvent::Token(format!("[{}: {} → {}]\n", name, status, output)));
                                            }
                                            // Slice H — surface wiki context as an inline note in the TUI
                                            // so terminal users see what fed the turn too.
                                            StreamEvent::WikiContext { pages } => {
                                                if !pages.is_empty() {
                                                    let _ = tx2.send(AppEvent::Token(
                                                        format!("[wiki: {}]\n", pages.join(", "))
                                                    ));
                                                }
                                            }
                                            // reasoning models'
                                            // private chain-of-thought.
                                            // Surface as an inline note
                                            // so terminal users get a
                                            // signal too; full content
                                            // lives in the web chat's
                                            // collapsible block.
                                            StreamEvent::Reasoning(_) => {
                                                let _ = tx2.send(AppEvent::Token(
                                                    "[reasoning emitted — see web chat for full text]\n".into()
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }

                    // ── Context ────────────────────────────────────────
                    Some(TuiAction::ClearContext) => {
                        state.messages.clear();
                        state.streaming_buffer.clear();
                        state.is_streaming = false;
                        state.token_count = 0;
                        state.conv_id = None;
                        state.last_turn_cost = None;
                        state.push_message(Role::System, "Context cleared. Starting fresh.".to_string());
                    }

                    Some(TuiAction::ShowContext) => {
                        state.push_message(Role::System, format!(
                            "Context\n  Messages : {}\n  Est. tokens : ~{}\n  Provider : {} \u{2192} {}\n  Memories : {}  Tools : {}  Session : {}",
                            state.messages.len(), state.token_count,
                            state.provider_label, state.model_label,
                            state.memory_count, state.tool_count, state.session_id,
                        ));
                    }

                    Some(TuiAction::ShowHelp) => {
                        state.push_message(Role::System, help_text());
                    }

                    Some(TuiAction::ShowVersion) => {
                        state.push_message(Role::System,
                            format!("MIRA v{}  \u{2014}  Multi-tasking Intelligent Responsive Assistant",
                                env!("CARGO_PKG_VERSION")));
                    }

                    Some(TuiAction::ShowTokens) => {
                        state.push_message(Role::System,
                            format!("Token estimate: ~{}  (messages: {})",
                                state.token_count, state.messages.len()));
                    }

                    Some(TuiAction::Export(path)) => {
                        match export_conversation(&state, &path) {
                            Ok(()) => state.push_message(Role::System,
                                format!("Conversation exported to: {}", path)),
                            Err(e) => state.push_message(Role::System,
                                format!("Export failed: {}", e)),
                        }
                    }

                    // ── Provider / Model ────────────────────────────────
                    Some(TuiAction::ListProviders) => {
                        let lms_url = &config.providers.lmstudio.url;
                        let or_key  = if config.providers.openrouter.api_key.is_some() { "configured" } else { "no API key" };
                        let cur = &state.provider_label;
                        state.push_message(Role::System, format!(
                            "Available providers:\n  [0] lmstudio  \u{2014} {}\n  [1] openrouter \u{2014} openrouter.ai  ({})\n  [2] ollama     \u{2014} {}\n\nActive: {}\nUse /provider-use <name> to switch.",
                            lms_url, or_key,
                            config.providers.ollama.url,
                            cur,
                        ));
                    }

                    Some(TuiAction::SwitchProvider(name)) => {
                        let valid = ["lmstudio", "openrouter", "ollama"];
                        if valid.contains(&name.as_str()) {
                            state.provider_label = name.clone();
                            state.push_message(Role::System,
                                format!("Switched to provider: {}  (restart MIRA to reconnect)", name));
                        } else {
                            state.push_message(Role::System,
                                format!("Unknown provider: {}\nValid: lmstudio, openrouter, ollama", name));
                        }
                    }

                    Some(TuiAction::ListModels) => {
                        let lms_model = &config.providers.lmstudio.default_model;
                        let or_model  = &config.providers.openrouter.default_model;
                        state.push_message(Role::System, format!(
                            "Available models:\n  [0] LM Studio  \u{2014} {}\n  [1] OpenRouter \u{2014} {}\n  [2] Ollama     \u{2014} {}\n\nCurrent: {} / {}\nUse /model-use <0-2> to switch.",
                            lms_model, or_model, config.providers.ollama.default_model,
                            state.provider_label, state.model_label,
                        ));
                    }

                    Some(TuiAction::SwitchModel(idx)) => {
                        match idx {
                            0 => {
                                state.provider_label = "lmstudio".to_string();
                                state.model_label    = config.providers.lmstudio.default_model.clone();
                            }
                            1 => {
                                state.provider_label = "openrouter".to_string();
                                state.model_label    = config.providers.openrouter.default_model.clone();
                            }
                            2 => {
                                state.provider_label = "ollama".to_string();
                                state.model_label    = "llama3.2".to_string();
                            }
                            _ => {
                                state.push_message(Role::System,
                                    "Invalid model index. Use /model-list to see options.".to_string());
                            }
                        }
                        if idx <= 2 {
                            state.push_message(Role::System,
                                format!("Switched to: {} / {}", state.provider_label, state.model_label));
                        }
                    }

                    // ── OpenRouter catalog ────────────────────────────────
                    Some(TuiAction::OpenRouterList(filter)) => {
                        let msg = render_openrouter_list(state.openrouter_catalog.as_deref(),
                                                         filter.as_deref(), 0);
                        state.push_message(Role::System, msg);
                    }

                    Some(TuiAction::OpenRouterPage(page)) => {
                        let msg = render_openrouter_list(state.openrouter_catalog.as_deref(),
                                                         None, page);
                        state.push_message(Role::System, msg);
                    }

                    Some(TuiAction::OpenRouterInfo(id)) => {
                        let msg = match state.openrouter_catalog.as_deref().and_then(|c| c.find(&id)) {
                            Some(m) => render_openrouter_info(m),
                            None    => format!(
                                "Model `{id}` not found in catalog. Try /openrouter-refresh \
                                 if you expect it to be there.",
                            ),
                        };
                        state.push_message(Role::System, msg);
                    }

                    Some(TuiAction::OpenRouterUse(id)) => {
                        // Tighten check: accept anything in the catalog *or*
                        // anything containing a `/` (vendor/model shape) so
                        // freshly-added models work even without a refresh.
                        let known = state.openrouter_catalog.as_deref()
                            .map(|c| c.find(&id).is_some()).unwrap_or(false);
                        if !known && !id.contains('/') {
                            state.push_message(Role::System, format!(
                                "Refusing to use `{id}` — not in catalog and not in vendor/model form. \
                                 Try /openrouter-refresh or use the full id like `openai/gpt-4o`."));
                        } else {
                            state.provider_label = "openrouter".to_string();
                            state.model_label    = id.clone();
                            state.push_message(Role::System,
                                format!("Switched to: openrouter / {id}"));
                        }
                    }

                    Some(TuiAction::OpenRouterRefresh) => {
                        state.push_message(Role::System,
                            "Refreshing OpenRouter catalog…".to_string());
                        spawn_catalog_fetch(Arc::clone(&backend), event_tx.clone(), true);
                    }

                    // ── Theme / Layout ──────────────────────────────────
                    Some(TuiAction::ChangeTheme(name)) => {
                        if let Some(t) = Theme::by_name(&name) {
                            let old = state.theme.name;
                            state.theme = t;
                            state.push_message(Role::System,
                                format!("Theme: {} \u{2192} {}", old, state.theme.name));
                        } else {
                            let available = Theme::all_names().join(", ");
                            state.push_message(Role::System,
                                format!("Unknown theme '{}'\nAvailable: {}", name, available));
                        }
                    }

                    Some(TuiAction::ChangeLayout(mode)) => {
                        let old = state.layout_mode.as_str();
                        state.layout_mode = mode.clone();
                        state.push_message(Role::System,
                            format!("Layout: {} \u{2192} {}", old, state.layout_mode.as_str()));
                    }

                    // ── Memory ──────────────────────────────────────────
                    Some(TuiAction::ListMemories) => {
                        spawn_memory_list(Arc::clone(&backend), event_tx.clone(), None);
                    }

                    Some(TuiAction::StoreMemory(text)) => {
                        spawn_memory_store(Arc::clone(&backend), event_tx.clone(), text);
                    }

                    Some(TuiAction::DeleteMemory(id)) => {
                        spawn_memory_delete(Arc::clone(&backend), event_tx.clone(), id);
                    }

                    Some(TuiAction::SearchMemory(q)) => {
                        spawn_memory_list(Arc::clone(&backend), event_tx.clone(), Some(q));
                    }

                    // ── Session ─────────────────────────────────────────
                    Some(TuiAction::ShowSessionInfo) => {
                        state.push_message(Role::System, format!(
                            "Session\n  ID       : {}\n  Messages : {}\n  Tokens   : ~{}\n  Provider : {} / {}",
                            state.session_id, state.messages.len(), state.token_count,
                            state.provider_label, state.model_label,
                        ));
                    }

                    Some(TuiAction::ClearSession) => {
                        state.messages.clear();
                        state.streaming_buffer.clear();
                        state.is_streaming = false;
                        state.token_count = 0;
                        state.conv_id = None;
                        state.last_turn_cost = None;
                        state.push_message(Role::System, "Session cleared.".to_string());
                    }

                    Some(TuiAction::SessionSummary) => {
                        let n = state.messages.len();
                        if n < 4 {
                            state.push_message(Role::System,
                                format!("Not enough messages to summarise (have {}, need at least 4).", n));
                        } else {
                            state.push_message(Role::System,
                                format!("Conversation has {} messages (~{} tokens).\nAuto-summarisation runs at ~4000 tokens in streaming mode.", n, state.token_count));
                        }
                    }

                    // ── Tools ────────────────────────────────────────────
                    Some(TuiAction::ListTools) => {
                        spawn_tool_list(Arc::clone(&backend), event_tx.clone());
                    }

                    Some(TuiAction::RunTool(spec)) => {
                        match parse_tool_run_spec(&spec) {
                            Ok((name, args)) => {
                                state.push_message(Role::System,
                                    format!("Running tool '{}'…", name));
                                spawn_tool_run(Arc::clone(&backend), event_tx.clone(), name, args);
                            }
                            Err(e) => {
                                state.push_message(Role::System,
                                    format!("/tool-run: {}\nUsage: /tool-run <name> [{{\"arg\":\"val\"}}]", e));
                            }
                        }
                    }

                    // ── Signal ───────────────────────────────────────────
                    Some(TuiAction::SignalSetup) => {
                        state.push_message(Role::System,
                            "Signal setup runs interactively in --simple mode.\nRun: mira --simple\nThen type: /signal-setup".to_string());
                    }

                    Some(TuiAction::RunPaletteCommand(cmd)) => {
                        state.input = cmd;
                        state.input_cursor = state.input.len();
                    }

                    Some(TuiAction::Reconnect) => {
                        state.push_message(Role::System, "Probing backend…".to_string());
                        let tx2      = event_tx.clone();
                        let backend2 = Arc::clone(&backend);
                        tokio::spawn(async move {
                            let ok = backend2.health_check().await;
                            let _  = tx2.send(AppEvent::HealthStatus(ok));
                            let _  = tx2.send(AppEvent::SystemMessage(
                                if ok { "Backend reachable.".to_string() }
                                else  { "Backend still unreachable.".to_string() }
                            ));
                        });
                    }

                    None => {}
                }
            }

            AppEvent::Resize(_, _) => { /* ratatui handles resize automatically */ }
            AppEvent::Tick         => { /* triggers redraw */ }

            AppEvent::Token(tok) => {
                state.streaming_buffer.push_str(&tok);
                state.token_count += 1;
            }

            AppEvent::StreamDone => {
                state.flush_stream();
            }

            AppEvent::StreamUsage(usage) => {
                let cost = if state.provider_label == "openrouter" {
                    state.openrouter_catalog.as_deref()
                        .and_then(|cat| {
                            mira::providers::openrouter::cost_for(
                                &reconstruct_catalog(cat),
                                &state.model_label,
                                &usage,
                            )
                        })
                } else {
                    None // local providers have no $ cost
                };
                state.last_turn_cost = Some(LastTurnCost {
                    prompt:     usage.prompt_tokens,
                    completion: usage.completion_tokens,
                    cost_usd:   cost,
                });
            }

            AppEvent::OpenRouterCatalog(result) => {
                match result {
                    Ok(snapshot) => {
                        let n = snapshot.models.len();
                        state.openrouter_catalog = Some(snapshot);
                        state.push_message(Role::System,
                            format!("OpenRouter catalog loaded ({n} models). \
                                     Try /openrouter-list."));
                    }
                    Err(e) => {
                        state.push_message(Role::System,
                            format!("OpenRouter catalog fetch failed: {e}"));
                    }
                }
            }

            AppEvent::StreamError(e) => {
                state.is_streaming = false;
                state.streaming_buffer.clear();
                // Likely signals a broken HTTP/SSE connection in server mode;
                // drop health_ok so the banner appears and `/reconnect` has
                // something to flip back.
                if state.backend_label == "server" {
                    state.health_ok = false;
                }
                state.push_message(Role::System, format!("Error: {}", e));
            }

            AppEvent::ConversationUpdated(cid) => { state.conv_id = Some(cid); }
            AppEvent::MemoryCount(n)  => { state.memory_count = n; }
            AppEvent::HealthStatus(ok) => { state.health_ok = ok; }
            AppEvent::SystemMessage(m) => { state.push_message(Role::System, m); }
        }
    }

    Ok(())
}

fn estimate_tokens(text: &str) -> usize {
    (text.split_whitespace().count() * 4) / 3
}

fn help_text() -> String {
    "MIRA commands\n\
     \n\
     Conversation\n\
       /clear               Clear chat context\n\
       /new                 Start fresh conversation\n\
       /ctx                 Show context stats\n\
       /tokens              Show token estimate\n\
       /export [file]       Export conversation\n\
       /help                Show this help\n\
       /version             Show MIRA version\n\
       /quit                Exit (or Ctrl+C)\n\
     \n\
     Provider / Model\n\
       /provider-list       List providers\n\
       /provider-use <name> Switch provider\n\
       /model-list          List models\n\
       /model-use <0-2>     Switch model\n\
     \n\
     OpenRouter\n\
       /openrouter-list [filter]  Browse catalog (paged)\n\
       /openrouter-page <n>       Jump to page N\n\
       /openrouter-info <id>      Show one model in detail\n\
       /openrouter-use <id>       Use a model for the next turn\n\
       /openrouter-refresh        Re-fetch the catalog\n\
     \n\
     Appearance\n\
       /theme <name>        Change theme (F5 cycles)\n\
       /layout <mode>       Change layout (F6 cycles)\n\
       Layouts: simple  standard  right-full  left-full  right-only  left-only\n\
     \n\
     Memory\n\
       /memory-list         List stored memories\n\
       /memory-store <text> Store a memory\n\
       /memory-search <q>   Search memories\n\
       /memory-delete <id>  Delete a memory\n\
     \n\
     Session\n\
       /session-info        Show session info\n\
       /session-clear       Clear session\n\
       /session-summary     Summarise conversation\n\
     \n\
     Tools\n\
       /tool-list           List tools with descriptions\n\
       /tool-run <name> [json]  Run a tool (e.g. /tool-run shell_execute {\"command\":\"ls\"})\n\
     \n\
     Connection\n\
       /reconnect           Re-probe backend health (clears banner)\n\
     \n\
     Keys: Tab=complete  Up/Down=history  Ctrl+P=palette  F5=theme  F6=layout".to_string()
}

// ── Memory helpers ────────────────────────────────────────────────────────────
//
// All four /memory-* actions reach the backend asynchronously. The handlers
// delegate here so `run_inner` stays readable and we don't duplicate the
// spawn + send-system-message pattern four times.

const MEMORY_LIST_LIMIT: usize = 50;

// Format a list of memory rows for display. Empty list is handled by
// callers so they can say "No memories stored" vs "No matches for …".
fn format_memory_list(entries: &[crate::tui::backend::MemoryEntry]) -> String {
    let mut out = String::new();
    for (i, m) in entries.iter().enumerate() {
        if i > 0 { out.push('\n'); }
        // Truncate long bodies so a 20-line paste doesn't blow out the chat
        // pane; users can always hit the web UI or --simple for full text.
        let preview = if m.content.chars().count() > 120 {
            let cut: String = m.content.chars().take(117).collect();
            format!("{}...", cut)
        } else {
            m.content.clone()
        };
        out.push_str(&format!("  [{}] ({}) {}", m.id, m.category, preview));
    }
    out
}

fn spawn_memory_list(
    backend:  Arc<dyn TuiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    query:    Option<String>,
) {
    tokio::spawn(async move {
        let result = match &query {
            Some(q) => backend.search_memories(q, MEMORY_LIST_LIMIT).await,
            None    => backend.list_memories(MEMORY_LIST_LIMIT).await,
        };
        let msg = match result {
            Ok(entries) if entries.is_empty() => match &query {
                Some(q) => format!("No memories match \"{}\".", q),
                None    => "No memories stored.".to_string(),
            },
            Ok(entries) => {
                let header = match &query {
                    Some(q) => format!("Memories matching \"{}\" ({}):", q, entries.len()),
                    None    => format!("Stored memories ({}):", entries.len()),
                };
                format!("{}\n{}", header, format_memory_list(&entries))
            }
            Err(e) => format!("Memory fetch failed: {}", e),
        };
        let _ = event_tx.send(AppEvent::SystemMessage(msg));
    });
}

fn spawn_memory_store(
    backend:  Arc<dyn TuiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    content:  String,
) {
    tokio::spawn(async move {
        let msg = match backend.store_memory(content).await {
            Ok(id)  => format!("Memory stored (id {}).", id),
            Err(e)  => format!("Memory store failed: {}", e),
        };
        // Refresh count so the status bar reflects the write without waiting
        // for the next natural poll.
        let count = backend.memory_count().await;
        let _ = event_tx.send(AppEvent::MemoryCount(count));
        let _ = event_tx.send(AppEvent::SystemMessage(msg));
    });
}

fn spawn_memory_delete(
    backend:  Arc<dyn TuiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    id:       u64,
) {
    tokio::spawn(async move {
        let msg = match backend.delete_memory(id).await {
            Ok(true)  => format!("Memory {} deleted.", id),
            Ok(false) => format!("Memory {} not found.", id),
            Err(e)    => format!("Memory delete failed: {}", e),
        };
        let count = backend.memory_count().await;
        let _ = event_tx.send(AppEvent::MemoryCount(count));
        let _ = event_tx.send(AppEvent::SystemMessage(msg));
    });
}

// ── Tool helpers ──────────────────────────────────────────────────────────────
//
// Same spawn + SystemMessage pattern as the memory helpers. Tool execution
// uses a longer implicit timeout because shell/file tools can take seconds.

// Format a tool list for display. Keeps the line density high so a dozen
// tools still fit on screen without scrolling.
fn format_tool_list(entries: &[crate::tui::backend::ToolInfo]) -> String {
    let mut out = String::new();
    for (i, t) in entries.iter().enumerate() {
        if i > 0 { out.push('\n'); }
        out.push_str(&format!("  {} — {}", t.name, t.description));
    }
    out
}

fn spawn_tool_list(
    backend:  Arc<dyn TuiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let msg = match backend.list_tools_detailed().await {
            Ok(entries) if entries.is_empty() => "No tools registered.".to_string(),
            Ok(entries) => format!(
                "Registered tools ({}):\n{}",
                entries.len(),
                format_tool_list(&entries),
            ),
            Err(e) => format!("Tool list failed: {}", e),
        };
        let _ = event_tx.send(AppEvent::SystemMessage(msg));
    });
}

fn spawn_tool_run(
    backend:  Arc<dyn TuiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    name:     String,
    args:     serde_json::Value,
) {
    tokio::spawn(async move {
        let msg = match backend.run_tool(name.clone(), args).await {
            Ok(r) if r.success => {
                if r.output.is_empty() {
                    format!("Tool '{}' succeeded.", name)
                } else {
                    format!("Tool '{}' succeeded:\n{}", name, r.output)
                }
            }
            Ok(r) => {
                let err = r.error.as_deref().unwrap_or("(no error message)");
                format!("Tool '{}' failed: {}", name, err)
            }
            Err(e) => format!("Tool '{}' error: {}", name, e),
        };
        let _ = event_tx.send(AppEvent::SystemMessage(msg));
    });
}

// Split the raw `/tool-run` argument string into `(name, args_json)`.
// Accepts `<name>`, `<name> <json>`, or `<name> <raw-json-that-isn't-an-object>`.
// Returns `Err` only when the name is missing or the JSON is syntactically
// invalid — an empty args section normalises to `{}` so zero-arg tools work.
fn parse_tool_run_spec(spec: &str) -> Result<(String, serde_json::Value), String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("tool name required".to_string());
    }
    let (name, rest) = match spec.find(char::is_whitespace) {
        Some(i) => (spec[..i].to_string(), spec[i..].trim()),
        None    => (spec.to_string(), ""),
    };
    let args = if rest.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(rest)
            .map_err(|e| format!("invalid JSON args: {}", e))?
    };
    Ok((name, args))
}

// ── OpenRouter catalog helpers ────────────────────────────────────────────
//
// Page-size of 12 keeps the rendered list inside the standard chat pane on
// most terminals without scrolling — adjust if the layout changes.

const OPENROUTER_PAGE_SIZE: usize = 12;

fn spawn_catalog_fetch(
    backend:  Arc<dyn TuiBackend>,
    event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    force:    bool,
) {
    tokio::spawn(async move {
        let r = backend.fetch_openrouter_catalog(force).await
            .map(Arc::new);
        let _ = event_tx.send(AppEvent::OpenRouterCatalog(r));
    });
}

fn render_openrouter_list(
    catalog: Option<&CatalogSnapshot>,
    filter:  Option<&str>,
    page:    usize,
) -> String {
    let cat = match catalog {
        Some(c) => c,
        None    => return "OpenRouter catalog not loaded yet. \
                           Try /openrouter-refresh in a moment.".to_string(),
    };

    let needle = filter.map(|f| f.to_lowercase());
    let filtered: Vec<_> = cat.models.iter()
        .filter(|m| match needle.as_deref() {
            Some(q) => m.id.to_lowercase().contains(q)
                    || m.name.to_lowercase().contains(q),
            None    => true,
        })
        .collect();

    if filtered.is_empty() {
        return match filter {
            Some(f) => format!("No OpenRouter models match \"{f}\"."),
            None    => "OpenRouter catalog is empty.".to_string(),
        };
    }

    let total_pages = filtered.len().div_ceil(OPENROUTER_PAGE_SIZE);
    let page = page.min(total_pages.saturating_sub(1));
    let start = page * OPENROUTER_PAGE_SIZE;
    let end   = (start + OPENROUTER_PAGE_SIZE).min(filtered.len());

    let mut out = match filter {
        Some(f) => format!("OpenRouter \u{2014} {} match \"{}\" (page {}/{})\n",
                           filtered.len(), f, page + 1, total_pages),
        None    => format!("OpenRouter catalog \u{2014} {} models (page {}/{})\n",
                           filtered.len(), page + 1, total_pages),
    };
    for m in &filtered[start..end] {
        out.push_str(&format!("  {}\n    {} \u{2014} {}\n",
            m.id,
            m.name,
            format_pricing_compact(m.price_prompt, m.price_completion, m.price_request),
        ));
    }
    if total_pages > 1 {
        out.push_str(&format!("\nUse /openrouter-page <1-{}> to navigate, \
                               /openrouter-info <id> for details.", total_pages));
    } else {
        out.push_str("\nUse /openrouter-info <id> for details, /openrouter-use <id> to switch.");
    }
    out
}

fn render_openrouter_info(m: &crate::tui::backend::CatalogModel) -> String {
    let ctx = if m.context_length == 0 { "?".to_string() } else { format!("{}", m.context_length) };
    let modality = if m.modality.is_empty() { "?".to_string() } else { m.modality.clone() };
    format!(
        "OpenRouter model\n  id        : {}\n  name      : {}\n  modality  : {}\n  context   : {} tokens\n  prompt    : {} / 1K tokens\n  completion: {} / 1K tokens{}\n\nUse /openrouter-use {} to make this the active model.",
        m.id,
        m.name,
        modality,
        ctx,
        per_1k_usd(m.price_prompt),
        per_1k_usd(m.price_completion),
        if m.price_request > 0.0 { format!("\n  request   : {} / call", format_usd(m.price_request)) } else { String::new() },
        m.id,
    )
}

fn format_pricing_compact(prompt: f64, completion: f64, request: f64) -> String {
    if prompt == 0.0 && completion == 0.0 && request == 0.0 {
        return "free / unpriced".to_string();
    }
    let mut parts = vec![
        format!("in {}/1K", per_1k_usd(prompt)),
        format!("out {}/1K", per_1k_usd(completion)),
    ];
    if request > 0.0 {
        parts.push(format!("req {}", format_usd(request)));
    }
    parts.join("  ")
}

// USD-per-token → USD-per-1K-tokens, formatted with magnitude-aware decimals.
fn per_1k_usd(per_token: f64) -> String { format_usd(per_token * 1000.0) }
fn format_usd(c: f64) -> String { mira::providers::openrouter::format_usd(c) }

// Re-wrap a `CatalogSnapshot` (TUI-side compact view) back into the
// `Catalog` shape that `cost_for` expects. This avoids exposing the
// upstream `Catalog`/`Pricing` types across the TUI backend boundary.
fn reconstruct_catalog(snap: &CatalogSnapshot) -> mira::providers::openrouter::Catalog {
    use mira::providers::openrouter::{Catalog, CatalogEntry, Pricing};
    Catalog {
        fetched_at: snap.fetched_at,
        models: snap.models.iter().map(|m| CatalogEntry {
            id:             m.id.clone(),
            name:           m.name.clone(),
            context_length: m.context_length,
            modality:       m.modality.clone(),
            pricing: Pricing {
                prompt:     m.price_prompt,
                completion: m.price_completion,
                image:      0.0,
                request:    m.price_request,
            },
        }).collect(),
    }
}

fn export_conversation(state: &AppState, path: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "# MIRA Conversation Export")?;
    writeln!(f, "Session: {}  Provider: {} / {}", state.session_id, state.provider_label, state.model_label)?;
    writeln!(f)?;
    for entry in &state.messages {
        let role = match entry.role {
            crate::tui::app::Role::User      => "**You**",
            crate::tui::app::Role::Assistant => "**MIRA**",
            crate::tui::app::Role::System    => "*System*",
        };
        writeln!(f, "### {} — {}", role, &entry.timestamp[..16].replace('T', " "))?;
        writeln!(f, "{}", entry.content)?;
        writeln!(f)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::backend::ToolInfo;

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens("hello world foo bar"), 5);
    }

    // ── parse_tool_run_spec ───────────────────────────────────────────────

    #[test]
    fn parse_tool_run_spec_name_only_uses_empty_args() {
        let (name, args) = parse_tool_run_spec("shell_execute").unwrap();
        assert_eq!(name, "shell_execute");
        assert_eq!(args, serde_json::json!({}));
    }

    #[test]
    fn parse_tool_run_spec_with_json_args() {
        let (name, args) = parse_tool_run_spec(r#"shell_execute {"command":"ls"}"#).unwrap();
        assert_eq!(name, "shell_execute");
        assert_eq!(args, serde_json::json!({ "command": "ls" }));
    }

    #[test]
    fn parse_tool_run_spec_trims_whitespace() {
        let (name, args) = parse_tool_run_spec("   echo    {}   ").unwrap();
        assert_eq!(name, "echo");
        assert_eq!(args, serde_json::json!({}));
    }

    #[test]
    fn parse_tool_run_spec_empty_errors() {
        assert!(parse_tool_run_spec("").is_err());
        assert!(parse_tool_run_spec("   ").is_err());
    }

    #[test]
    fn parse_tool_run_spec_invalid_json_errors() {
        let err = parse_tool_run_spec("echo {not json").unwrap_err();
        assert!(err.contains("invalid JSON"));
    }

    // ── format_tool_list ──────────────────────────────────────────────────

    #[test]
    fn format_tool_list_renders_name_and_description() {
        let entries = vec![
            ToolInfo { name: "echo".into(),  description: "Echoes input".into() },
            ToolInfo { name: "shell".into(), description: "Run shell".into() },
        ];
        let out = format_tool_list(&entries);
        assert!(out.contains("echo — Echoes input"));
        assert!(out.contains("shell — Run shell"));
    }

    #[test]
    fn format_tool_list_empty_returns_empty_string() {
        assert!(format_tool_list(&[]).is_empty());
    }
}
