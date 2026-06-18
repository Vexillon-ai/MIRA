// SPDX-License-Identifier: AGPL-3.0-or-later

// src/error.rs

use thiserror::Error;
use crate::types::ProviderId;

// Core error type for MIRA operations
#[derive(Error, Debug)]
pub enum MiraError {
    #[error("Configuration error: {0}")]
    ConfigError(String),
    
    #[error("Provider error: {0}")]
    ProviderError(String),
    
    #[error("Memory operation failed: {0}")]
    MemoryError(String),
    
    #[error("Tool execution failed: {0}")]
    ToolError(String),
    
    #[error("Model returned invalid response: {0}")]
    InvalidResponse(String),
    
    #[error("Max iterations reached without completion")]
    MaxIterationsReached,
    
    #[error("All providers unavailable")]
    AllProvidersUnavailable,
    
    #[error("Provider not found: {0}")]
    ProviderNotFound(ProviderId),
    
    #[error("Unsafe command blocked: {0}")]
    UnsafeCommand(String),
    
    #[error("Command failed '{0}': {1}")]
    CommandFailed(String, String),
    
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    
    #[error("Request error: {0}")]
    RequestError(#[from] reqwest::Error),
    
    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Security error: {0}")]
    SecurityError(String),

    #[error("Proxy error: {0}")]
    ProxyError(String),

    #[error("Tool round limit reached after {0} rounds without a final answer")]
    ToolRoundLimitReached(usize),

    #[error("Channel error: {0}")]
    ChannelError(String),

    #[error("Rate limit exceeded")]
    RateLimitExceeded,

    #[error("Unauthorized")]
    Unauthorized,

    #[error("Server error: {0}")]
    ServerError(String),

    #[error("Auth error: {0}")]
    AuthError(String),

    #[error("History error: {0}")]
    HistoryError(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Forbidden")]
    Forbidden,

    // Credentials were correct but the account is not yet approved (Q2 #11
    // self-service onboarding). Distinct from Unauthorized so the login
    // handler can tell the user to wait for an admin rather than implying
    // bad credentials. Only ever returned *after* a correct password, so it
    // leaks no information a signed-up user doesn't already know.
    #[error("Account awaiting approval")]
    PendingApproval,

    // the policy engine denied an action. Carries the
    // rule id (stable, used by the audit log to group identical
    // denials) and the human-readable reason (surfaced to the
    // model / user). Distinct from `ProviderError` so callers can
    // react differently — most should propagate, but a fall-through
    // path could downgrade to a polite "I can't do that" message.
    #[error("policy/{rule} denied: {reason}")]
    PolicyDenied { rule: String, reason: String },
}

// Result type alias for MIRA operations
pub type Result<T> = std::result::Result<T, MiraError>;
