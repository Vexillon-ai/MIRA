// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/local/ollama.rs

//! Ollama local LLM provider implementation

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, warn};

use crate::providers::ModelProvider;
use crate::types::{ChatMessage, GenerationOptions, GenerationResponse, ProviderId, TokenUsage};

/// Ollama provider implementation
pub struct OllamaProvider {
    client: Client,
    url: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct OllamaRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    #[serde(flatten)]
    options: GenerationOptions,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct OllamaStreamResponse {
    #[serde(default)]
    message: Option<OllamaMessage>,
    #[serde(default)]
    eval_count: u32,
    #[serde(default)]
    prompt_eval_count: u32,
}

impl OllamaProvider {
    /// Create a new Ollama provider
    pub fn new(url: String, model: String) -> Self {
        let client = ClientBuilder::new()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("Failed to create HTTP client");
        
        Self { client, url, model }
    }
    
    pub fn model_name(&self) -> &str { &self.model }

    /// Get the full API URL
    fn api_url(&self) -> String {
        format!("{}/api/chat", self.url.trim_end_matches('/'))
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }
    
    async fn generate(
        &self,
        messages: &[ChatMessage],
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, crate::MiraError> {
        debug!("Generating with Ollama model '{}'", self.model);
        
        // Convert messages to Ollama format
        let ollama_messages: Vec<OllamaMessage> = messages
            .iter()
            .map(|m| OllamaMessage {
                role: match m.role {
                    crate::types::MessageRole::System => "system".to_string(),
                    crate::types::MessageRole::User => "user".to_string(),
                    crate::types::MessageRole::Assistant => "assistant".to_string(),
                    crate::types::MessageRole::Tool => "tool".to_string(),
                },
                content: m.content.clone(),
            })
            .collect();
        
        let request = OllamaRequest {
            model: self.model.clone(),
            messages: ollama_messages,
            stream: true,  // Use streaming for better UX
            options: options.clone(),
        };
        
        let url = self.api_url();
        debug!("Sending request to {}", url);
        
        let response = self.client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::MiraError::ProviderError(
                format!("Failed to connect to Ollama: {}", e)
            ))?;
        
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(crate::MiraError::ProviderError(
                format!("Ollama returned {}: {}", status, body)
            ));
        }
        
        // Stream and collect the response
        let mut content = String::new();
        let mut eval_count: u32 = 0;
        let mut prompt_eval_count: u32 = 0;
        
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| crate::MiraError::ProviderError(
                format!("Stream error: {}", e)
            ))?;
            
            // Parse JSON lines
            let line = String::from_utf8_lossy(&chunk);
            for json_line in line.lines() {
                if json_line.is_empty() { continue; }
                
                if let Ok(stream_resp) = serde_json::from_str::<OllamaStreamResponse>(json_line) {
                    if let Some(msg) = stream_resp.message {
                        content.push_str(&msg.content);
                    }
                    eval_count += stream_resp.eval_count;
                    prompt_eval_count += stream_resp.prompt_eval_count;
                }
            }
        }
        
        debug!("Generation complete, {} tokens", eval_count);
        
        Ok(GenerationResponse {
            content,
            tool_calls: None,
            reasoning: None,
            usage: TokenUsage {
                prompt_tokens: prompt_eval_count,
                completion_tokens: eval_count,
                total_tokens: prompt_eval_count + eval_count,
            },
            provider_id: ProviderId::Local(self.model.clone()),
            model_name: self.model.clone(),
            fallback: None,
            })
    }
    
    async fn health_check(&self) -> bool {
        let url = format!("{}/api/tags", self.url.trim_end_matches('/'));
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!("Ollama health check failed: {}", e);
                false
            }
        }
    }
}
