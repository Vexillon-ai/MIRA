// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel/cli_channel.rs

//! CLI Channel implementation for MIRA

use async_trait::async_trait;
use std::io::{self, Write};

use super::{Channel, IncomingMessage, OutgoingMessage};

/// CLI channel - wraps the interactive terminal interface
pub struct CliChannel {
    session_id: String,
}

impl CliChannel {
    pub fn new() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }
    
    /// Print the MIRA banner
    pub fn print_banner(&self) {
        println!();
        println!("{}", crate::banner::render("cli"));
        println!();
        println!("Your life's loyal partner. Always ready to assist.");
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn name(&self) -> &str {
        "cli"
    }
    
    fn display_name(&self) -> String {
        "CLI (Interactive)".to_string()
    }
    
    async fn receive(&mut self) -> Option<IncomingMessage> {
        print!("MIRA > ");
        io::stdout().flush().ok();
        
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() || input.is_empty() {
            return None;
        }
        
        // Handle exit commands
        match input.trim().to_lowercase().as_str() {
            "quit" | "exit" | ":q" => return None,
            _ => {}
        }
        
        Some(IncomingMessage {
            id: format!("cli-{}-{}", self.session_id, chrono::Utc::now().timestamp_millis()),
            sender: "user".to_string(),
            content: input.trim().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }
    
    async fn send(&self, msg: OutgoingMessage) -> Result<(), crate::MiraError> {
        println!("{}", msg.content);
        Ok(())
    }
    
    fn is_active(&self) -> bool {
        true
    }
    
    async fn shutdown(&mut self) {
        // Nothing special needed for CLI
    }
}
