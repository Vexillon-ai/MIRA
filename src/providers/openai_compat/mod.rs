// SPDX-License-Identifier: AGPL-3.0-or-later

// src/providers/openai_compat/mod.rs

//! Generic OpenAI-compatible LLM provider client.
//!
//! Every major hosted LLM gateway except Anthropic and Google now serves
//! the OpenAI `/v1/chat/completions` shape: OpenAI itself, DeepSeek,
//! Moonshot AI (Kimi), Groq, Together AI, Fireworks, xAI Grok,
//! Mistral La Plateforme, Perplexity, DeepInfra, Azure OpenAI, plus
//! every reasonable self-hosted runtime (vLLM, llama.cpp server, TGI,
//! LocalAI). This module is the one client that talks to all of them.
//!
//! The thin wrappers in this directory's siblings (`OpenAiProvider`,
//! `DeepSeekProvider`, `KimiProvider`, `GroqProvider`, `XaiProvider`,
//! plus the generic `OpenAiCompatProvider` for unknown gateways) just
//! pre-fill the `OpenAiCompatConfig` with the right base URL and let
//! the shared client do the work.

mod client;

pub use client::{
    AuthHeader,
    ExtraHeader,
    OpenAiCompatClient,
    OpenAiCompatConfig,
};
