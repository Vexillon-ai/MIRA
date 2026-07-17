// SPDX-License-Identifier: AGPL-3.0-or-later

// src/lib.rs

//! MIRA - Multi-tasking Intelligent Responsive Assistant
//! 
//! "Your life's loyal partner. Always ready to assist."
//! 
//! A personal AI agent framework designed to be safe, efficient, extensible,
//! intelligent, and transparent.

pub mod agent;
pub mod artifacts;
pub mod auth;
pub mod automations;
pub mod banner;
pub mod bench;
pub mod calendar;
pub mod channel;
pub mod channel_accounts;
pub mod channel_identity;
pub mod companion;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod gateway;
pub mod guardian_sentinel;
pub mod hardware;
pub mod health;
pub mod discord;
pub mod history;
pub mod image;
pub mod install;
pub mod setup;
pub mod email;
pub mod log_filter;
pub mod matrix;
pub mod whatsapp;
pub mod slack;
pub mod external;
pub mod mcp;
pub mod memory;
pub mod notifications;
pub mod onboarding;
pub mod packages;
pub mod policy;
// Linux-only: a Unix-socket privileged daemon (SO_PEERCRED auth) that wires
// per-process network-namespace egress via /proc + nftables. None of that is
// portable to Windows/macOS, so gate the whole module — non-Linux targets must
// still compile. Its only in-tree callers (main.rs helper subcommands,
// packages::launcher::request_egress) are gated to match.
#[cfg(target_os = "linux")]
pub mod privhelper;
pub mod providers;
pub mod proxy;
pub mod remote_access;
pub mod sandbox;
pub mod security;
pub mod server;
pub mod skills;
pub mod session;   // Session persistence
pub mod summarizer; // Context summarization
pub mod system_prompt;
pub mod stt;
pub mod task_artifacts;
pub mod tools;
pub mod tts;
pub mod types;
pub mod video;
pub mod voice;
pub mod waitlist;
pub mod web;
pub mod wiki;
pub mod wsl_net;   // WSL host-URL misrouting detection + one-click fix

// Re-export commonly used types
pub use error::{MiraError, Result};
pub use agent::{Agent, AgentCore, AgentBudget, AgentId, AgentRegistry, AgentStatus, SimpleAgent, StreamEvent, ToolMode};
pub use channel::{Channel, IncomingMessage, OutgoingMessage};
pub use config::{
    MiraConfig, Config,
    ProvidersConfig, OllamaConfig, LmStudioConfig, OpenRouterConfig,
    CliConfig, TuiConfig, ServerConfig, ChannelsConfig,
    SignalConfig, TelegramConfig, LoggingConfig, MemoryConfig,
    EmbeddingConfig, SessionConfig,
    TtsConfig, TtsCacheConfig, TtsInternalConfig, TtsOpenaiConfig,
    TtsOpenaiCompatConfig, TtsElevenlabsConfig, TtsCartesiaConfig, TtsRoutingConfig,
    AgentConfig, ToolsConfig, ToolToggle,
    WikiConfig, WikiAutoExtractConfig, WikiAgentToolsConfig, WikiGitConfig, WikiMcpConfig,
    SecurityPolicyConfig,
    ProxyConfig, TlsConfig,
    expand_path, default_config_path, default_config_dir,
};
pub use gateway::*;
pub use memory::{MemorySystem, MemoryItem, Category, MemorySource, HeuristicExtractor};
pub use security::SecurityConfig;
pub use memory::semantic::{EmbeddingProvider, LmStudioEmbeddingProvider};
pub use memory::fastembed_provider::FastEmbedProvider;
pub use providers::*;
pub use server::MiraServer;
pub use session::{SessionStore, SessionData};
pub use summarizer::{ContextSummarizer, ConversationSummary};
pub use tools::{Tool, ToolRegistry, ToolResult};
pub use types::*;
