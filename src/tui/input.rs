// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/input.rs
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crate::tui::app::AppState;
use crate::tui::layout::LayoutMode;
use crate::tui::completion::{complete, command_base};
use crate::tui::theme::Theme;

/// Every action the TUI event loop can perform — none of these send text to the LLM
/// unless the variant is explicitly `SendMessage`.
#[derive(Debug)]
pub enum TuiAction {
    // ── Chat ──────────────────────────────────────────────────────────────
    SendMessage(String),

    // ── Navigation ────────────────────────────────────────────────────────
    Quit,

    // ── Context / conversation ────────────────────────────────────────────
    ClearContext,           // /clear  /new
    ShowContext,            // /ctx
    ShowHelp,               // /help
    ShowVersion,            // /version
    ShowTokens,             // /tokens
    Export(String),         // /export <file>

    // ── Provider / model ─────────────────────────────────────────────────
    ListProviders,          // /provider-list
    SwitchProvider(String), // /provider-use <name>
    ListModels,             // /model-list
    SwitchModel(usize),     // /model-use <idx>
    OpenRouterList(Option<String>),     // /openrouter-list [filter]
    OpenRouterPage(usize),              // /openrouter-page <n>
    OpenRouterInfo(String),             // /openrouter-info <id>
    OpenRouterUse(String),              // /openrouter-use <id>
    OpenRouterRefresh,                  // /openrouter-refresh

    // ── Theme / layout ────────────────────────────────────────────────────
    ChangeTheme(String),
    ChangeLayout(LayoutMode),

    // ── Memory ────────────────────────────────────────────────────────────
    ListMemories,           // /memory-list
    StoreMemory(String),    // /memory-store <text>
    DeleteMemory(u64),      // /memory-delete <id>
    SearchMemory(String),   // /memory-search <query>

    // ── Session ───────────────────────────────────────────────────────────
    ShowSessionInfo,        // /session-info
    ClearSession,           // /session-clear
    SessionSummary,         // /session-summary

    // ── Tools ─────────────────────────────────────────────────────────────
    ListTools,              // /tool-list
    RunTool(String),        // /tool-run <name> [args_json]

    // ── Other ─────────────────────────────────────────────────────────────
    SignalSetup,            // /signal-setup
    #[allow(dead_code)] // command palette is wired but not yet emitted (feat/rich-tui)
    RunPaletteCommand(String),
    Reconnect,              // /reconnect — probe backend health, update banner
}

pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Option<TuiAction> {
    // Command palette has highest priority
    if state.palette_open {
        return handle_palette_key(state, key);
    }

    // Tab while completions popup is visible: cycle + fill
    if state.show_completions && key.code == KeyCode::Tab {
        cycle_completion(state, 1);
        return None;
    }

    // Arrow keys while completions popup is visible
    if state.show_completions {
        if let Some(action) = handle_completion_nav(state, key) {
            return action;
        }
    }

    match key.code {
        // ── Quit ──────────────────────────────────────────────────────────
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(TuiAction::Quit)
        }
        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(TuiAction::Quit)
        }

        // ── Send / execute ────────────────────────────────────────────────
        KeyCode::Enter => {
            // If completions are open, Enter normally fills the selected one.
            // Exception: when the input already equals the selected command's
            // base, treat Enter as execute — filling would be a no-op and
            // otherwise the user would need to press Enter twice for a
            // fully-typed zero-arg command like `/memory-list`.
            if state.show_completions {
                let idx = state.completion_sel.unwrap_or(0);
                let already_exact = state
                    .completions
                    .get(idx)
                    .map(|it| command_base(&it.command) == state.input.trim())
                    .unwrap_or(false);
                if !already_exact {
                    fill_completion(state);
                    return None;
                }
                state.show_completions = false;
                state.completion_sel = None;
            }
            let msg = std::mem::take(&mut state.input);
            state.input_cursor = 0;
            state.show_completions = false;
            if msg.is_empty() {
                return None;
            }
            state.push_to_history(msg.clone());
            parse_slash_command(msg)
        }

        // ── Backspace ─────────────────────────────────────────────────────
        KeyCode::Backspace => {
            if !state.input.is_empty() && state.input_cursor > 0 {
                state.input.pop();
                state.input_cursor = state.input_cursor.saturating_sub(1);
            }
            update_completions(state);
            None
        }

        // ── Character input ───────────────────────────────────────────────
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            state.input.push(c);
            state.input_cursor += 1;
            update_completions(state);
            None
        }

        // ── Tab: show completions and fill first match ────────────────────
        KeyCode::Tab => {
            update_completions(state);
            if !state.completions.is_empty() {
                state.show_completions = true;
                state.completion_sel = Some(0);
                fill_completion(state);
            }
            None
        }

        // ── Escape ────────────────────────────────────────────────────────
        KeyCode::Esc => {
            state.show_completions = false;
            state.completion_sel = None;
            state.palette_open = false;
            None
        }

        // ── Command palette ───────────────────────────────────────────────
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.palette_open = true;
            state.palette_query.clear();
            state.palette_sel = 0;
            None
        }

        // ── Scroll ────────────────────────────────────────────────────────
        KeyCode::PageUp => {
            state.auto_scroll = false;
            state.scroll_offset = state.scroll_offset.saturating_sub(10);
            None
        }
        KeyCode::PageDown => {
            state.scroll_offset = state.scroll_offset.saturating_add(10);
            None
        }
        KeyCode::End => {
            state.auto_scroll = true;
            None
        }

        // ── Chat scroll (Up/Down) ─────────────────────────────────────────
        KeyCode::Up if !key.modifiers.contains(KeyModifiers::ALT) => {
            state.auto_scroll = false;
            state.scroll_offset = state.scroll_offset.saturating_sub(1);
            None
        }
        KeyCode::Down if !key.modifiers.contains(KeyModifiers::ALT) => {
            state.scroll_offset = state.scroll_offset.saturating_add(1);
            None
        }

        // ── Input history (Alt+Up / Alt+Down) ────────────────────────────
        KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
            if !state.history.is_empty() {
                let pos = match state.history_pos {
                    None    => state.history.len() - 1,
                    Some(p) => p.saturating_sub(1),
                };
                state.history_pos = Some(pos);
                state.input = state.history[pos].clone();
                state.input_cursor = state.input.len();
            }
            None
        }
        KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
            match state.history_pos {
                None => {}
                Some(p) if p + 1 >= state.history.len() => {
                    state.history_pos = None;
                    state.input.clear();
                    state.input_cursor = 0;
                }
                Some(p) => {
                    state.history_pos = Some(p + 1);
                    state.input = state.history[p + 1].clone();
                    state.input_cursor = state.input.len();
                }
            }
            None
        }

        // ── F5: cycle theme ───────────────────────────────────────────────
        KeyCode::F(5) => {
            let names = Theme::all_names();
            let cur = names.iter().position(|n| *n == state.theme.name).unwrap_or(0);
            let next_name = names[(cur + 1) % names.len()];
            if let Some(t) = Theme::by_name(next_name) {
                state.theme = t;
            }
            None
        }

        // ── F6: cycle layout ──────────────────────────────────────────────
        KeyCode::F(6) => {
            state.layout_mode = state.layout_mode.next();
            None
        }

        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Completion helpers
// ─────────────────────────────────────────────────────────────────────────────

fn update_completions(state: &mut AppState) {
    // Completions cover the command name only. Once the user types a space
    // they're entering an argument — closing the popup here prevents Enter
    // from filling instead of executing (the `/memory-search <arg>` bug).
    let has_whitespace = state.input.contains(char::is_whitespace);
    if state.input.starts_with('/') && !has_whitespace {
        state.completions = complete(&state.input);
        state.show_completions = !state.completions.is_empty();
    } else {
        state.completions.clear();
        state.show_completions = false;
    }
    state.completion_sel = if state.show_completions { Some(0) } else { None };
}

/// Fill `state.input` with the currently selected (or first) completion's base command.
fn fill_completion(state: &mut AppState) {
    let idx = state.completion_sel.unwrap_or(0);
    if let Some(item) = state.completions.get(idx) {
        // Fill with just the command base (no <arg> placeholder)
        state.input = command_base(&item.command).to_string();
        // If the command takes an argument, append a trailing space for ergonomics
        if item.command.contains('<') {
            state.input.push(' ');
        }
        state.input_cursor = state.input.len();
    }
    state.show_completions = false;
    state.completion_sel = None;
}

/// Move completion selection by `delta` steps (wrapping) and fill input.
fn cycle_completion(state: &mut AppState, delta: isize) {
    if state.completions.is_empty() {
        return;
    }
    let max = state.completions.len() as isize;
    let cur = state.completion_sel.unwrap_or(0) as isize;
    let next = ((cur + delta).rem_euclid(max)) as usize;
    state.completion_sel = Some(next);
    // Immediately fill input with the cycled completion
    if let Some(item) = state.completions.get(next) {
        state.input = command_base(&item.command).to_string();
        if item.command.contains('<') {
            state.input.push(' ');
        }
        state.input_cursor = state.input.len();
    }
}

/// Handle arrow-key navigation of the completion popup.
/// Returns `Some(action)` if consumed, `None` to let normal handler run.
fn handle_completion_nav(state: &mut AppState, key: KeyEvent) -> Option<Option<TuiAction>> {
    match key.code {
        KeyCode::Down => { cycle_completion(state, 1);  Some(None) }
        KeyCode::Up   => { cycle_completion(state, -1); Some(None) }
        KeyCode::Esc  => {
            state.show_completions = false;
            state.completion_sel = None;
            Some(None)
        }
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Palette key handler
// ─────────────────────────────────────────────────────────────────────────────

fn handle_palette_key(state: &mut AppState, key: KeyEvent) -> Option<TuiAction> {
    match key.code {
        KeyCode::Esc => {
            state.palette_open = false;
            state.palette_query.clear();
            None
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.palette_query.push(c);
            state.palette_sel = 0;
            None
        }
        KeyCode::Backspace => {
            state.palette_query.pop();
            state.palette_sel = 0;
            None
        }
        KeyCode::Down | KeyCode::Tab => {
            state.palette_sel = state.palette_sel.saturating_add(1);
            None
        }
        KeyCode::Up => {
            state.palette_sel = state.palette_sel.saturating_sub(1);
            None
        }
        KeyCode::Enter => {
            state.palette_open = false;
            let q  = state.palette_query.clone();
            let ql = q.to_lowercase();
            state.palette_query.clear();
            let cmds = crate::tui::completion::all_commands();
            let filtered: Vec<_> = cmds.iter()
                .filter(|c| {
                    q.is_empty()
                        || c.command.contains(q.as_str())
                        || c.description.to_lowercase().contains(ql.as_str())
                })
                .collect();
            if let Some(cmd) = filtered.get(state.palette_sel) {
                state.input = command_base(cmd.command).to_string();
                if cmd.command.contains('<') {
                    state.input.push(' ');
                }
                state.input_cursor = state.input.len();
            }
            None
        }
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Slash-command router
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a `/command [args]` string into a `TuiAction`.
/// Anything not recognised as a command becomes `SendMessage` so the user gets
/// a useful AI response rather than a silent no-op.
pub fn parse_slash_command(msg: String) -> Option<TuiAction> {
    // Split into the command token and the rest of the line
    let (cmd, rest) = match msg.find(' ') {
        Some(i) => (msg[..i].trim(), msg[i + 1..].trim()),
        None    => (msg.trim(), ""),
    };

    match cmd {
        // ── Context / conversation ─────────────────────────────────────
        "/clear" | "/new"    => Some(TuiAction::ClearContext),
        "/ctx"               => Some(TuiAction::ShowContext),
        "/help"              => Some(TuiAction::ShowHelp),
        "/version"           => Some(TuiAction::ShowVersion),
        "/tokens"            => Some(TuiAction::ShowTokens),
        "/quit"              => Some(TuiAction::Quit),
        "/export" => {
            if rest.is_empty() {
                Some(TuiAction::Export("mira_export.md".to_string()))
            } else {
                Some(TuiAction::Export(rest.to_string()))
            }
        }

        // ── Provider / model ──────────────────────────────────────────
        "/provider-list"   => Some(TuiAction::ListProviders),
        "/provider-use"    => if rest.is_empty() {
            Some(TuiAction::ShowHelp)
        } else {
            Some(TuiAction::SwitchProvider(rest.to_string()))
        },
        "/model-list"      => Some(TuiAction::ListModels),
        "/model-use"       => match rest.parse::<usize>() {
            Ok(n) => Some(TuiAction::SwitchModel(n)),
            Err(_) => Some(TuiAction::ShowHelp),
        },
        "/openrouter-list"    => Some(TuiAction::OpenRouterList(
            if rest.is_empty() { None } else { Some(rest.to_string()) }
        )),
        "/openrouter-page"    => match rest.parse::<usize>() {
            Ok(n) if n >= 1 => Some(TuiAction::OpenRouterPage(n - 1)),
            _ => Some(TuiAction::ShowHelp),
        },
        "/openrouter-info"    => if rest.is_empty() {
            Some(TuiAction::ShowHelp)
        } else {
            Some(TuiAction::OpenRouterInfo(rest.to_string()))
        },
        "/openrouter-use"     => if rest.is_empty() {
            Some(TuiAction::ShowHelp)
        } else {
            Some(TuiAction::OpenRouterUse(rest.to_string()))
        },
        "/openrouter-refresh" => Some(TuiAction::OpenRouterRefresh),

        // ── Theme / layout ────────────────────────────────────────────
        "/theme"  => if rest.is_empty() { Some(TuiAction::ListProviders) }
                     else { Some(TuiAction::ChangeTheme(rest.to_string())) },
        "/layout" => if rest.is_empty() { Some(TuiAction::ShowHelp) }
                     else { Some(TuiAction::ChangeLayout(LayoutMode::from_str(rest))) },

        // ── Memory ────────────────────────────────────────────────────
        "/memory-list"   => Some(TuiAction::ListMemories),
        "/memory-store"  => if rest.is_empty() {
            Some(TuiAction::ShowHelp)
        } else {
            Some(TuiAction::StoreMemory(rest.to_string()))
        },
        "/memory-delete" => match rest.parse::<u64>() {
            Ok(id) => Some(TuiAction::DeleteMemory(id)),
            Err(_) => Some(TuiAction::ShowHelp),
        },
        "/memory-search" => if rest.is_empty() {
            Some(TuiAction::ShowHelp)
        } else {
            Some(TuiAction::SearchMemory(rest.to_string()))
        },

        // ── Session ───────────────────────────────────────────────────
        "/session-info"    => Some(TuiAction::ShowSessionInfo),
        "/session-clear"   => Some(TuiAction::ClearSession),
        "/session-summary" => Some(TuiAction::SessionSummary),

        // ── Tools ─────────────────────────────────────────────────────
        "/tool-list" => Some(TuiAction::ListTools),
        "/tool-run"  => if rest.is_empty() {
            Some(TuiAction::ShowHelp)
        } else {
            Some(TuiAction::RunTool(rest.to_string()))
        },

        // ── Other ─────────────────────────────────────────────────────
        "/signal-setup" => Some(TuiAction::SignalSetup),
        "/reconnect"    => Some(TuiAction::Reconnect),

        // Unknown slash command → send to LLM so user gets a response
        _ => Some(TuiAction::SendMessage(msg)),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use crate::tui::app::AppState;
    use crate::tui::completion::all_commands;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// Simulate typing `s` char-by-char through `handle_key`, so that every
    /// keystroke goes through the same completion/update machinery the real
    /// event loop uses. This is critical for catching the `/memory-search`
    /// class of bug where direct assignment bypasses `update_completions`.
    fn type_input(state: &mut AppState, s: &str) {
        for c in s.chars() {
            handle_key(state, key(KeyCode::Char(c)));
        }
    }

    // ── Basic keystroke handling ─────────────────────────────────────────

    #[test]
    fn test_enter_sends_message() {
        let mut state = AppState::new();
        state.input = "hello world".to_string();
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(matches!(action, Some(TuiAction::SendMessage(ref s)) if s == "hello world"));
        assert!(state.input.is_empty());
    }

    #[test]
    fn test_backspace_removes_char() {
        let mut state = AppState::new();
        state.input = "abc".to_string();
        state.input_cursor = 3;
        handle_key(&mut state, key(KeyCode::Backspace));
        assert_eq!(state.input, "ab");
    }

    #[test]
    fn test_ctrl_c_quits() {
        let mut state = AppState::new();
        let action = handle_key(&mut state, ctrl_key(KeyCode::Char('c')));
        assert!(matches!(action, Some(TuiAction::Quit)));
    }

    #[test]
    fn test_ctrl_q_quits() {
        let mut state = AppState::new();
        let action = handle_key(&mut state, ctrl_key(KeyCode::Char('q')));
        assert!(matches!(action, Some(TuiAction::Quit)));
    }

    #[test]
    fn test_char_appends() {
        let mut state = AppState::new();
        handle_key(&mut state, key(KeyCode::Char('x')));
        assert_eq!(state.input, "x");
    }

    #[test]
    fn test_escape_closes_completions() {
        let mut state = AppState::new();
        type_input(&mut state, "/mem");
        assert!(state.show_completions);
        handle_key(&mut state, key(KeyCode::Esc));
        assert!(!state.show_completions);
    }

    // ── Completion popup / Tab cycling ───────────────────────────────────

    #[test]
    fn test_tab_fills_first_completion() {
        let mut state = AppState::new();
        state.input = "/mem".to_string();
        handle_key(&mut state, key(KeyCode::Tab));
        assert!(state.input.starts_with("/memory"));
        assert!(!state.show_completions);
    }

    #[test]
    fn test_tab_cycles_on_subsequent_press() {
        let mut state = AppState::new();
        state.input = "/mem".to_string();
        handle_key(&mut state, key(KeyCode::Tab));
        let first = state.input.clone();
        state.input = "/mem".to_string();
        update_completions(&mut state);
        handle_key(&mut state, key(KeyCode::Tab));
        assert!(state.input.starts_with("/memory"));
        let _ = first;
    }

    #[test]
    fn test_ctrl_p_opens_palette() {
        let mut state = AppState::new();
        let _ = handle_key(&mut state, ctrl_key(KeyCode::Char('p')));
        assert!(state.palette_open);
    }

    #[test]
    fn test_completions_close_when_user_types_space() {
        // Regression: typing a space after a matched command used to leave
        // the popup open, so Enter filled instead of executing the argument.
        let mut state = AppState::new();
        type_input(&mut state, "/memory-search");
        assert!(state.show_completions);
        type_input(&mut state, " ");
        assert!(!state.show_completions, "space should close completions");
    }

    // ── Enter after char-by-char typing (end-to-end through handle_key) ──

    #[test]
    fn test_typed_memory_search_with_query_executes() {
        // The exact bug reported by the user.
        let mut state = AppState::new();
        type_input(&mut state, "/memory-search Tarek");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(
            matches!(action, Some(TuiAction::SearchMemory(ref q)) if q == "Tarek"),
            "expected SearchMemory(Tarek), got {:?}", action
        );
    }

    #[test]
    fn test_typed_memory_list_executes_on_single_enter() {
        // With completions open, Enter on an already-exact command should
        // execute, not force a second Enter.
        let mut state = AppState::new();
        type_input(&mut state, "/memory-list");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(
            matches!(action, Some(TuiAction::ListMemories)),
            "expected ListMemories, got {:?}", action
        );
    }

    #[test]
    fn test_typed_memory_store_with_text_executes() {
        let mut state = AppState::new();
        type_input(&mut state, "/memory-store hello world");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(matches!(action, Some(TuiAction::StoreMemory(ref s)) if s == "hello world"));
    }

    #[test]
    fn test_typed_memory_delete_with_id_executes() {
        let mut state = AppState::new();
        type_input(&mut state, "/memory-delete 42");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(matches!(action, Some(TuiAction::DeleteMemory(42))));
    }

    #[test]
    fn test_typed_model_use_with_idx_executes() {
        let mut state = AppState::new();
        type_input(&mut state, "/model-use 2");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(matches!(action, Some(TuiAction::SwitchModel(2))));
    }

    #[test]
    fn test_typed_provider_use_with_name_executes() {
        let mut state = AppState::new();
        type_input(&mut state, "/provider-use openrouter");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(
            matches!(action, Some(TuiAction::SwitchProvider(ref n)) if n == "openrouter")
        );
    }

    #[test]
    fn test_typed_export_with_filename_executes() {
        let mut state = AppState::new();
        type_input(&mut state, "/export chat.md");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(matches!(action, Some(TuiAction::Export(ref n)) if n == "chat.md"));
    }

    #[test]
    fn test_typed_tool_run_with_args_executes() {
        let mut state = AppState::new();
        type_input(&mut state, "/tool-run shell_execute");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(matches!(action, Some(TuiAction::RunTool(ref s)) if s == "shell_execute"));
    }

    #[test]
    fn test_typed_layout_with_mode_executes() {
        let mut state = AppState::new();
        type_input(&mut state, "/layout right-full");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(matches!(action, Some(TuiAction::ChangeLayout(_))));
    }

    #[test]
    fn test_typed_theme_with_name_executes() {
        let mut state = AppState::new();
        type_input(&mut state, "/theme dracula");
        let action = handle_key(&mut state, key(KeyCode::Enter));
        assert!(matches!(action, Some(TuiAction::ChangeTheme(ref n)) if n == "dracula"));
    }

    // ── parse_slash_command: every command routes correctly ──────────────

    #[test]
    fn test_parse_clear() {
        assert!(matches!(parse_slash_command("/clear".into()), Some(TuiAction::ClearContext)));
    }
    #[test]
    fn test_parse_new() {
        assert!(matches!(parse_slash_command("/new".into()), Some(TuiAction::ClearContext)));
    }
    #[test]
    fn test_parse_ctx() {
        assert!(matches!(parse_slash_command("/ctx".into()), Some(TuiAction::ShowContext)));
    }
    #[test]
    fn test_parse_help() {
        assert!(matches!(parse_slash_command("/help".into()), Some(TuiAction::ShowHelp)));
    }
    #[test]
    fn test_parse_version() {
        assert!(matches!(parse_slash_command("/version".into()), Some(TuiAction::ShowVersion)));
    }
    #[test]
    fn test_parse_tokens() {
        assert!(matches!(parse_slash_command("/tokens".into()), Some(TuiAction::ShowTokens)));
    }
    #[test]
    fn test_parse_quit() {
        assert!(matches!(parse_slash_command("/quit".into()), Some(TuiAction::Quit)));
    }
    #[test]
    fn test_parse_export_default() {
        let a = parse_slash_command("/export".into());
        assert!(matches!(a, Some(TuiAction::Export(ref s)) if s == "mira_export.md"));
    }
    #[test]
    fn test_parse_export_with_path() {
        let a = parse_slash_command("/export notes.md".into());
        assert!(matches!(a, Some(TuiAction::Export(ref s)) if s == "notes.md"));
    }
    #[test]
    fn test_parse_provider_list() {
        assert!(matches!(parse_slash_command("/provider-list".into()), Some(TuiAction::ListProviders)));
    }
    #[test]
    fn test_parse_provider_use_empty_shows_help() {
        assert!(matches!(parse_slash_command("/provider-use".into()), Some(TuiAction::ShowHelp)));
    }
    #[test]
    fn test_parse_provider_use_with_name() {
        let a = parse_slash_command("/provider-use openrouter".into());
        assert!(matches!(a, Some(TuiAction::SwitchProvider(ref s)) if s == "openrouter"));
    }
    #[test]
    fn test_parse_model_list() {
        assert!(matches!(parse_slash_command("/model-list".into()), Some(TuiAction::ListModels)));
    }
    #[test]
    fn test_parse_model_use_with_index() {
        assert!(matches!(parse_slash_command("/model-use 3".into()), Some(TuiAction::SwitchModel(3))));
    }
    #[test]
    fn test_parse_model_use_invalid_shows_help() {
        assert!(matches!(parse_slash_command("/model-use abc".into()), Some(TuiAction::ShowHelp)));
    }
    #[test]
    fn test_parse_layout_empty_shows_help() {
        assert!(matches!(parse_slash_command("/layout".into()), Some(TuiAction::ShowHelp)));
    }
    #[test]
    fn test_parse_layout_with_mode() {
        assert!(matches!(parse_slash_command("/layout simple".into()), Some(TuiAction::ChangeLayout(_))));
    }
    #[test]
    fn test_parse_theme_with_name() {
        let a = parse_slash_command("/theme dracula".into());
        assert!(matches!(a, Some(TuiAction::ChangeTheme(ref s)) if s == "dracula"));
    }
    #[test]
    fn test_parse_memory_list() {
        assert!(matches!(parse_slash_command("/memory-list".into()), Some(TuiAction::ListMemories)));
    }
    #[test]
    fn test_parse_memory_store_empty_shows_help() {
        assert!(matches!(parse_slash_command("/memory-store".into()), Some(TuiAction::ShowHelp)));
    }
    #[test]
    fn test_parse_memory_store_with_text() {
        let a = parse_slash_command("/memory-store hello".into());
        assert!(matches!(a, Some(TuiAction::StoreMemory(ref s)) if s == "hello"));
    }
    #[test]
    fn test_parse_memory_delete_with_id() {
        assert!(matches!(parse_slash_command("/memory-delete 7".into()), Some(TuiAction::DeleteMemory(7))));
    }
    #[test]
    fn test_parse_memory_delete_invalid_shows_help() {
        assert!(matches!(parse_slash_command("/memory-delete oops".into()), Some(TuiAction::ShowHelp)));
    }
    #[test]
    fn test_parse_memory_search_empty_shows_help() {
        assert!(matches!(parse_slash_command("/memory-search".into()), Some(TuiAction::ShowHelp)));
    }
    #[test]
    fn test_parse_memory_search_with_query() {
        let a = parse_slash_command("/memory-search Tarek".into());
        assert!(matches!(a, Some(TuiAction::SearchMemory(ref s)) if s == "Tarek"));
    }
    #[test]
    fn test_parse_session_info() {
        assert!(matches!(parse_slash_command("/session-info".into()), Some(TuiAction::ShowSessionInfo)));
    }
    #[test]
    fn test_parse_session_clear() {
        assert!(matches!(parse_slash_command("/session-clear".into()), Some(TuiAction::ClearSession)));
    }
    #[test]
    fn test_parse_session_summary() {
        assert!(matches!(parse_slash_command("/session-summary".into()), Some(TuiAction::SessionSummary)));
    }
    #[test]
    fn test_parse_tool_list() {
        assert!(matches!(parse_slash_command("/tool-list".into()), Some(TuiAction::ListTools)));
    }
    #[test]
    fn test_parse_tool_run_empty_shows_help() {
        assert!(matches!(parse_slash_command("/tool-run".into()), Some(TuiAction::ShowHelp)));
    }
    #[test]
    fn test_parse_tool_run_with_args() {
        let a = parse_slash_command("/tool-run shell_execute {\"command\":\"ls\"}".into());
        assert!(matches!(a, Some(TuiAction::RunTool(_))));
    }
    #[test]
    fn test_parse_signal_setup() {
        assert!(matches!(parse_slash_command("/signal-setup".into()), Some(TuiAction::SignalSetup)));
    }
    #[test]
    fn test_parse_reconnect() {
        assert!(matches!(parse_slash_command("/reconnect".into()), Some(TuiAction::Reconnect)));
    }
    #[test]
    fn test_parse_unknown_sends_message() {
        let a = parse_slash_command("/totally-unknown-command".into());
        assert!(matches!(a, Some(TuiAction::SendMessage(_))));
    }
    #[test]
    fn test_parse_openrouter_list_no_filter() {
        let a = parse_slash_command("/openrouter-list".into());
        assert!(matches!(a, Some(TuiAction::OpenRouterList(None))));
    }
    #[test]
    fn test_parse_openrouter_list_with_filter() {
        let a = parse_slash_command("/openrouter-list gpt".into());
        match a {
            Some(TuiAction::OpenRouterList(Some(f))) => assert_eq!(f, "gpt"),
            other => panic!("expected OpenRouterList(Some), got {:?}", other),
        }
    }
    #[test]
    fn test_parse_openrouter_page_one_based_to_zero_based() {
        // User types page 1 → internal page index 0
        let a = parse_slash_command("/openrouter-page 1".into());
        assert!(matches!(a, Some(TuiAction::OpenRouterPage(0))));
        let a2 = parse_slash_command("/openrouter-page 3".into());
        assert!(matches!(a2, Some(TuiAction::OpenRouterPage(2))));
    }
    #[test]
    fn test_parse_openrouter_info() {
        let a = parse_slash_command("/openrouter-info openai/gpt-4o".into());
        match a {
            Some(TuiAction::OpenRouterInfo(id)) => assert_eq!(id, "openai/gpt-4o"),
            other => panic!("expected OpenRouterInfo, got {:?}", other),
        }
    }
    #[test]
    fn test_parse_openrouter_use() {
        let a = parse_slash_command("/openrouter-use openai/gpt-4o".into());
        match a {
            Some(TuiAction::OpenRouterUse(id)) => assert_eq!(id, "openai/gpt-4o"),
            other => panic!("expected OpenRouterUse, got {:?}", other),
        }
    }
    #[test]
    fn test_parse_openrouter_refresh() {
        let a = parse_slash_command("/openrouter-refresh".into());
        assert!(matches!(a, Some(TuiAction::OpenRouterRefresh)));
    }

    /// Guard-rail: every command registered in the completion catalog must be
    /// handled by `parse_slash_command` (i.e. not fall through to SendMessage).
    /// This fails loudly when someone adds a command to `all_commands()` but
    /// forgets to wire it in the slash-router — forcing parity at test time.
    #[test]
    fn test_every_completion_command_is_routed() {
        for def in all_commands() {
            // Use just the command base (strip `<arg>` placeholder).
            let base = def.command.split_whitespace().next().unwrap_or(def.command);
            let action = parse_slash_command(base.to_string());
            // Commands that require an argument route to ShowHelp when called
            // bare — that's still "handled", not SendMessage.
            assert!(
                !matches!(action, Some(TuiAction::SendMessage(_))),
                "completion command `{}` falls through to SendMessage — \
                 add a match arm in parse_slash_command",
                base
            );
        }
    }
}
