// SPDX-License-Identifier: AGPL-3.0-or-later

// src/agent/mod.rs
//! Agent module — shared reasoning engine and legacy single-turn agent.
//!
//! # Primary types
//! - [`AgentCore`] is the shared service used by the Gateway, Server,
//!   and TUI. It holds providers, memory, tools, and sessions, and
//!   processes turns via a streaming event channel.
//! - [`Agent`] (slice B1) is a single instance of the multi-agent
//!   hierarchy: identity, parent ref, status, budget, conversation
//!   history. The root MIRA agent and every future worker are both
//!   `Agent`s — the difference is in the fields, not the type.
//! - [`AgentRegistry`] indexes live `Agent`s for tree traversal,
//!   interrupt propagation (B5), and the agents UI (B7).
//!
//! # Legacy type
//! [`SimpleAgent`] is a lightweight per-request wrapper kept for the
//! `mira --simple` CLI mode. New code should use `Agent` + `AgentCore`.

pub mod adapter;
pub mod audit;
pub mod claude_code;
pub mod core;
pub mod definitions;
pub mod guardian;
pub mod guardian_actions;
pub mod hermes;
pub mod instance;
pub mod context_budget;
pub mod memory_hook;
pub mod named_agent;
pub mod opencode;
pub mod tokens;
pub mod orchestrator;
pub mod workflow;
pub mod routing;
pub mod wiki_hook;
pub mod protocol;
pub mod research;
pub mod resolver;
pub mod run_logs;
pub mod verify;
pub mod stream;
pub mod supervisor;
pub mod tool_loop;
pub mod tool_select;
pub mod transport;

pub use core::{AgentCore, DEFAULT_SYSTEM_PROMPT, TurnContext};
pub use instance::{Agent, AgentBudget, AgentId, AgentRegistry, AgentStatus};
pub use definitions::{AgentDefinition, AgentDefinitionStore, NewAgentDefinition};
pub use audit::{AuditError, AuditEvent, AuditFilter, AuditRecord, AuditStore};
pub use adapter::{
    AdapterConfig, AssignmentChannel, OutputFormat, SubprocessAdapter,
};
pub use claude_code::{ClaudeCodeAdapter, ClaudeCodeConfig};
pub use opencode::{OpenCodeAdapter, OpenCodeConfig};
pub use hermes::{HermesAdapter, HermesConfig};
pub use research::{
    FetchedDoc, HttpPolicyFetcher, ResearchAdapter, ResearchConfig,
    ResearchFetcher, StubFetcher,
};
pub use resolver::{ChainedResolver, MiraSkillResolver};
pub use named_agent::{
    handle_from_skill_id, skill_id_for_handle, NamedAgentExecutor, NamedAgentResolver,
};
pub use orchestrator::Orchestrator;
pub use workflow::{
    NewWorkflowDefinition, RunStatus, StepRun, WorkflowDefinition, WorkflowRun, WorkflowStep,
    WorkflowStore,
};
pub use verify::{
    NoOpVerifier, SubprocessVerifier, VerificationContext, Verifier,
    VerifyingAdapter,
};
pub use protocol::{
    AgentStateSnapshot, Envelope, EnvelopeId, Event as AgentEvent, InterruptReason,
    Request as AgentRequest, Response as AgentResponse,
};
pub use stream::StreamEvent;
pub use supervisor::{
    NullExecutorResolver, SkillExecutorResolver, SpawnChildError, Supervisor,
    WorkerAssignment, WorkerComplete, WorkerContext, WorkerFailure, WorkerHandle,
    WorkerOutcome, WorkerTask, MAX_RECURSION_DEPTH,
};
pub use tool_loop::{ToolMode, parse_react_action};
pub use transport::{AgentChannel, ChannelError, EventSender, Incoming as ChannelIncoming};

use crate::types::{ConversationContext, GenerationOptions};
use crate::providers::ModelProvider;
use tracing::{debug, info, warn};

/// Maximum iterations before giving up
const MAX_ITERATIONS: usize = 10;

/// Agent state machine wrapper around a provider
pub struct SimpleAgent {
    /// Current conversation context (persists across turns)
    pub context: ConversationContext,
    
    /// Generation options
    options: GenerationOptions,
    
    /// Maximum iterations per user message
    max_iterations: usize,
}

impl SimpleAgent {
    /// Create a new agent with the given system prompt
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            context: ConversationContext::new(system_prompt),
            options: GenerationOptions::default(),
            max_iterations: MAX_ITERATIONS,
        }
    }
    
    /// Set generation options
    pub fn with_options(mut self, options: GenerationOptions) -> Self {
        self.options = options;
        self
    }
    
    /// Process a user message through the agent loop
    /// Returns the assistant's response
    pub async fn process(
        &mut self,
        provider: &dyn ModelProvider,
        user_message: impl Into<String>,
    ) -> Result<String, crate::MiraError> {
        let user_content = user_message.into();
        
        // Add user message to context
        self.context.add_user_message(&user_content);
        debug!("User: {}", user_content);
        
        // Run the reasoning loop
        let response = self.reasoning_loop(provider).await?;
        
        // Add assistant response to context
        self.context.add_assistant_message(&response);
        debug!("Assistant: {}", response);
        
        Ok(response)
    }
    
    /// The core reasoning loop - generates response with potential retries
    async fn reasoning_loop(
        &self,
        provider: &dyn ModelProvider,
    ) -> Result<String, crate::MiraError> {
        let mut iteration = 0;
        
        loop {
            iteration += 1;
            
            if iteration > self.max_iterations {
                warn!("Max iterations ({}) reached", self.max_iterations);
                return Err(crate::MiraError::MaxIterationsReached);
            }
            
            debug!("Iteration {} of {}", iteration, self.max_iterations);
            
            // Get current messages from context
            let messages = self.context.messages_vec();
            
            // Generate response
            match provider.generate(&messages, &self.options).await {
                Ok(response) => {
                    if response.content.is_empty() {
                        warn!("Empty response from model, retrying...");
                        continue;  // Retry on empty response
                    }
                    return Ok(response.content);
                }
                Err(e) => {
                    warn!("Generation failed (attempt {}): {}", iteration, e);
                    if iteration >= self.max_iterations {
                        return Err(e);
                    }
                    // Continue to retry
                }
            }
        }
    }
    
    /// Get the current context's message history length
    pub fn message_count(&self) -> usize {
        self.context.messages.len()
    }
    
    /// Clear conversation history (keep system prompt)
    pub fn reset(&mut self) {
        self.context = ConversationContext::new(self.context.system_prompt.clone());
        info!("Conversation context reset");
    }
    
    /// Estimate current token usage
    pub fn token_estimate(&self) -> usize {
        self.context.token_count_estimate()
    }
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]
    use super::*;
    use async_trait::async_trait;
    use crate::types::{ChatMessage, GenerationOptions, GenerationResponse, TokenUsage, ProviderId};
    use crate::providers::ModelProvider;

    struct MockSuccessProvider(String);

    #[async_trait]
    impl ModelProvider for MockSuccessProvider {
        fn name(&self) -> &str { "mock_success" }
        async fn generate(&self, _msgs: &[ChatMessage], _opts: &GenerationOptions) -> Result<GenerationResponse, crate::MiraError> {
            Ok(GenerationResponse {
                content: self.0.clone(),
                tool_calls: None,
                reasoning: None,
                usage: TokenUsage::default(),
                provider_id: ProviderId::Local("mock".to_string()),
                model_name: "mock".to_string(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    struct MockEmptyThenFull { calls: std::sync::Mutex<u32>, response: String }
    impl MockEmptyThenFull {
        fn new(r: &str) -> Self { Self { calls: std::sync::Mutex::new(0), response: r.to_string() } }
    }

    #[async_trait]
    impl ModelProvider for MockEmptyThenFull {
        fn name(&self) -> &str { "mock_empty_then_full" }
        async fn generate(&self, _msgs: &[ChatMessage], _opts: &GenerationOptions) -> Result<GenerationResponse, crate::MiraError> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            let content = if *c == 1 { String::new() } else { self.response.clone() };
            Ok(GenerationResponse {
                content,
                tool_calls: None,
                reasoning: None,
                usage: TokenUsage::default(),
                provider_id: ProviderId::Local("mock".to_string()),
                model_name: "mock".to_string(),
                fallback: None,
            })
        }
        async fn health_check(&self) -> bool { true }
    }

    #[test]
    fn test_simple_agent_creation() {
        let agent = SimpleAgent::new("You are a helpful assistant.");
        // ConversationContext::new adds one system message
        assert_eq!(agent.message_count(), 1);
        assert_eq!(agent.context.system_prompt, "You are a helpful assistant.");
    }

    #[test]
    fn test_agent_token_estimate() {
        let agent = SimpleAgent::new("System prompt of known length.");
        // token_estimate = total char count / 4
        let expected = "System prompt of known length.".len() / 4;
        assert_eq!(agent.token_estimate(), expected);
    }

    #[test]
    fn test_agent_reset() {
        let mut agent = SimpleAgent::new("System.");
        agent.context.add_user_message("Hello");
        agent.context.add_assistant_message("Hi there");
        assert_eq!(agent.message_count(), 3);
        agent.reset();
        // After reset: only system message remains
        assert_eq!(agent.message_count(), 1);
        assert_eq!(agent.context.system_prompt, "System.");
    }

    #[tokio::test]
    async fn test_agent_process() {
        let provider = MockSuccessProvider("Hello from mock!".to_string());
        let mut agent = SimpleAgent::new("System.");
        let result = agent.process(&provider, "Tell me something").await.unwrap();
        assert_eq!(result, "Hello from mock!");
        // System + user + assistant = 3 messages
        assert_eq!(agent.message_count(), 3);
    }

    #[tokio::test]
    async fn test_agent_process_retries_on_empty_response() {
        let provider = MockEmptyThenFull::new("Non-empty response");
        let mut agent = SimpleAgent::new("System.");
        let result = agent.process(&provider, "Hello").await.unwrap();
        assert_eq!(result, "Non-empty response");
        // Verify it was called at least twice (retry logic)
        assert!(*provider.calls.lock().unwrap() >= 2);
    }

    #[test]
    fn test_agent_with_options() {
        let opts = GenerationOptions { temperature: 0.1, max_tokens: Some(100), ..Default::default() };
        let agent = SimpleAgent::new("System.").with_options(opts.clone());
        assert_eq!(agent.options.temperature, 0.1);
        assert_eq!(agent.options.max_tokens, Some(100));
    }
}
