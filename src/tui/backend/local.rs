// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/backend/local.rs
//! In-process backend: drives `AgentCore` directly and persists to the
//! shared `HistoryStore`. Matches  behavior exactly — the refactor
//! introduced in  moves this logic behind the `TuiBackend` trait
//! without changing any semantics.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use mira::agent::{AgentCore, TurnContext, stream::StreamEvent};
use mira::config::MiraConfig;
use mira::history::{HistoryStore, MessageRole, NewConversation, NewMessage};
use mira::memory::Category;
use mira::providers::openrouter::OpenRouterProvider;

use super::{
    CatalogModel, CatalogSnapshot, MemoryEntry, ResumedConversation, ResumedRole,
    ToolExecOutcome, ToolInfo, TuiBackend, TurnHandle,
};

pub struct LocalBackend {
    core:       Arc<AgentCore>,
    history:    Option<Arc<HistoryStore>>,
    session_id: String,
    // MIRA user id that owns every conversation this backend writes.
    // Resolved at startup to the first active admin — shell access on the
    // host implies admin, so TUI/CLI traffic is owner-stamped the same as
    // any other channel. Falls back to `"local-user"` when auth is disabled.
    user_id:    String,
    // Held so OpenRouter catalog calls can read API key + cache settings
    // without round-tripping through extensions.
    config:     Arc<MiraConfig>,
}

impl LocalBackend {
    pub fn new(
        core:       Arc<AgentCore>,
        history:    Option<Arc<HistoryStore>>,
        session_id: String,
        user_id:    String,
        config:     Arc<MiraConfig>,
    ) -> Self {
        Self { core, history, session_id, user_id, config }
    }
}

#[async_trait]
impl TuiBackend for LocalBackend {
    async fn health_check(&self) -> bool {
        self.core.health_check().await
    }

    async fn tool_count(&self) -> usize {
        self.core.tools.list_tools().len()
    }

    async fn memory_count(&self) -> usize {
        self.core.memory.count().unwrap_or(0) as usize
    }

    async fn send_message(
        &self,
        conv_id:  Option<String>,
        msg:      String,
        model:    String,
        provider: String,
    ) -> Result<TurnHandle, String> {
        // Persist user message up-front — matches a crash mid-stream
        // still leaves the user's prompt in history, which is what they'd
        // expect when reopening the web UI.
        let conv_id = if let Some(ref hist) = self.history {
            let id = match conv_id {
                Some(id) => id,
                None => {
                    let title = truncate_title(&msg);
                    hist.create_conversation(NewConversation {
                        user_id:          self.user_id.clone(),
                        channel:          "tui".to_owned(),
                        title:            Some(title),
                        model:            Some(model.clone()),
                        provider:         Some(provider.clone()),
                        external_user_id: None,
                        mode:             None,
                    })
                    .map_err(|e| format!("create_conversation: {}", e))?
                    .id
                }
            };
            let _ = hist.add_message(NewMessage {
                conversation_id: id.clone(),
                role:            MessageRole::User,
                content:         msg.clone(),
                content_type:    "text".to_owned(),
                token_count:     None,
                model:           None,
                tool_calls:      None,
                metadata:        None,
            });
            Some(id)
        } else {
            conv_id
        };

        // Trusted identity injection — see server/handlers/signal.rs for
        // the rationale. Without this, automations.* and other user-scoped
        // tools refuse to run with "missing caller identity".
        let mut inject = serde_json::Map::new();
        inject.insert(
            "_user_id".to_string(),
            serde_json::Value::String(self.user_id.clone()),
        );
        if let Some(cid) = conv_id.as_ref() {
            inject.insert(
                "_conversation_id".to_string(),
                serde_json::Value::String(cid.clone()),
            );
        }
        let turn_ctx = TurnContext { inject_tool_args: inject, ..TurnContext::default() };

        // Kick off the provider stream and forward StreamEvents to a fresh
        // channel the caller can consume without holding a lock on us.
        let mut rx = self
            .core
            .process_with_context(
                &self.session_id, &self.user_id, "tui", &msg,
                None, turn_ctx,
            )
            .await
            .map_err(|e| e.to_string())?;

        let (out_tx, out_rx) = mpsc::unbounded_channel::<StreamEvent>();
        let history         = self.history.clone();
        let conv_id_clone   = conv_id.clone();
        let model_for_persist = model.clone();

        tokio::spawn(async move {
            let mut full = String::new();
            while let Some(ev) = rx.recv().await {
                if let StreamEvent::Token(ref t) = ev {
                    full.push_str(t);
                }
                let is_done = matches!(ev, StreamEvent::Done { .. });
                if is_done {
                    if let (Some(hist), Some(cid)) = (history.as_ref(), conv_id_clone.as_ref()) {
                        if !full.is_empty() {
                            let _ = hist.add_message(NewMessage {
                                conversation_id: cid.clone(),
                                role:            MessageRole::Assistant,
                                content:         full.clone(),
                                content_type:    "text".to_owned(),
                                token_count:     None,
                                model:           Some(model_for_persist.clone()),
                                tool_calls:      None,
                                metadata:        None,
                            });
                        }
                        let _ = hist.touch_conversation(cid);
                    }
                }
                let _ = out_tx.send(ev);
                if is_done {
                    break;
                }
            }
        });

        Ok(TurnHandle { conv_id, rx: out_rx })
    }

    async fn list_memories(&self, limit: usize) -> Result<Vec<MemoryEntry>, String> {
        let items = self.core.memory.list_all(limit, 0);
        Ok(items.into_iter().map(|m| MemoryEntry {
            id:       m.id,
            content:  m.content,
            category: m.category.to_string(),
        }).collect())
    }

    async fn search_memories(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, String> {
        let items = self.core.memory.search(query);
        Ok(items.into_iter().take(limit).map(|m| MemoryEntry {
            id:       m.id,
            content:  m.content,
            category: m.category.to_string(),
        }).collect())
    }

    async fn store_memory(&self, content: String) -> Result<u64, String> {
        // Auto-categorize based on content; same heuristic the /api/memory
        // POST path uses when the client doesn't specify a category.
        self.core.memory.store(content, Category::Fact, Vec::new())
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_memory(&self, id: u64) -> Result<bool, String> {
        self.core.memory.delete(id).await.map_err(|e| e.to_string())
    }

    async fn list_tools_detailed(&self) -> Result<Vec<ToolInfo>, String> {
        let mut infos: Vec<ToolInfo> = self
            .core
            .tools
            .list_tools()
            .into_iter()
            .filter_map(|name| {
                self.core.tools.get(&name).map(|t| ToolInfo {
                    name:        t.name().to_owned(),
                    description: t.description().to_owned(),
                })
            })
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(infos)
    }

    async fn run_tool(
        &self,
        name: String,
        args: serde_json::Value,
    ) -> Result<ToolExecOutcome, String> {
        match self.core.tools.execute(&name, args).await {
            Ok(r) => Ok(ToolExecOutcome {
                success: r.success,
                output:  r.output,
                error:   r.error,
            }),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn fetch_openrouter_catalog(&self, force: bool) -> Result<CatalogSnapshot, String> {
        let or = &self.config.providers.openrouter;
        // No key → still allow a fetch: OpenRouter's `/models` is publicly
        // listable. We just won't be able to filter by key-specific access.
        let api_key = or.api_key.clone().unwrap_or_default();
        let provider = OpenRouterProvider::new(api_key, or.default_model.clone());
        let data_dir = self.config.data_dir_path();
        let cat = provider
            .catalog(&data_dir, force, or.catalog_refresh_hours)
            .await
            .map_err(|e| e.to_string())?;
        Ok(CatalogSnapshot {
            fetched_at: cat.fetched_at,
            models: cat.models.into_iter().map(|m| CatalogModel {
                id:               m.id,
                name:             m.name,
                context_length:   m.context_length,
                modality:         m.modality,
                price_prompt:     m.pricing.prompt,
                price_completion: m.pricing.completion,
                price_request:    m.pricing.request,
            }).collect(),
        })
    }

    async fn fetch_last_tui_conversation(&self, limit: usize) -> Option<ResumedConversation> {
        let hist = self.history.as_ref()?;

        // Same user id LocalBackend uses when creating rows in send_message.
        let convs = hist.list_conversations(&self.user_id, Some("tui"), 1, 0).ok()?;
        let conv  = convs.into_iter().next()?;

        let msgs = hist.get_messages(&conv.id, limit as i64, None).ok()?;
        let messages = msgs
            .into_iter()
            .map(|m| {
                let role = match m.role {
                    MessageRole::User      => ResumedRole::User,
                    MessageRole::Assistant => ResumedRole::Assistant,
                    MessageRole::System
                    | MessageRole::Tool    => ResumedRole::System,
                };
                (role, m.content)
            })
            .collect();

        Some(ResumedConversation { conv_id: conv.id, messages })
    }
}

fn truncate_title(msg: &str) -> String {
    let trimmed = msg.trim();
    let first_line = trimmed.lines().next().unwrap_or(trimmed);
    if first_line.chars().count() <= 80 {
        first_line.to_owned()
    } else {
        let cut: String = first_line.chars().take(77).collect();
        format!("{}...", cut)
    }
}
