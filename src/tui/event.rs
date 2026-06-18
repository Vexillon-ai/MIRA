// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/event.rs
use std::sync::Arc;

use crossterm::event::KeyEvent;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, Duration};
use crossterm::event::EventStream;
use futures::StreamExt;

use mira::types::TokenUsage;

use crate::tui::backend::CatalogSnapshot;

/// All events the TUI main loop handles.
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    /// Terminal resize — ratatui handles the redraw internally, the payload
    /// is unused but kept so downstream handlers can inspect dimensions.
    #[allow(dead_code)]
    Resize(u16, u16),
    Tick,
    Token(String),
    StreamDone,
    /// Stream finished with usage statistics from the provider. Used to
    /// drive the per-turn cost footer; emitted after `StreamDone` so the
    /// existing flush path doesn't change.
    StreamUsage(TokenUsage),
    StreamError(String),
    /// Background OpenRouter catalog fetch finished. The receiver replaces
    /// `state.openrouter_catalog`. `Err` carries a message to surface as a
    /// system note.
    OpenRouterCatalog(Result<Arc<CatalogSnapshot>, String>),
    /// Backend has created or adopted a conversation id for the in-flight
    /// turn. Emitted before the first token so the UI can update state.
    ConversationUpdated(String),
    MemoryCount(usize),
    HealthStatus(bool),
    /// Push a system-role message into the chat from a background task
    /// (e.g. `/reconnect` probing backend health).
    SystemMessage(String),
}

/// Spawn the background tasks that produce events and return (sender, receiver).
/// The sender clone is passed to AI streaming tasks.
pub fn spawn_event_tasks() -> (UnboundedSender<AppEvent>, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    spawn_tick_task(tx.clone());
    spawn_key_task(tx.clone());
    (tx, rx)
}

/// Sends AppEvent::Tick at 20 fps (every 50 ms).
fn spawn_tick_task(tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_millis(50));
        loop {
            ticker.tick().await;
            if tx.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
}

/// Reads crossterm events and forwards them as AppEvent::Key / AppEvent::Resize.
fn spawn_key_task(tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(ev)) = stream.next().await {
            let app_ev = match ev {
                crossterm::event::Event::Key(k)       => AppEvent::Key(k),
                crossterm::event::Event::Resize(w, h) => AppEvent::Resize(w, h),
                _ => continue,
            };
            if tx.send(app_ev).is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tick_event_arrives() {
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
        tx.send(AppEvent::Tick).unwrap();
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, AppEvent::Tick));
    }

    #[tokio::test]
    async fn test_token_event() {
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
        tx.send(AppEvent::Token("hello".to_string())).unwrap();
        match rx.recv().await.unwrap() {
            AppEvent::Token(s) => assert_eq!(s, "hello"),
            _ => panic!("wrong event"),
        }
    }
}
