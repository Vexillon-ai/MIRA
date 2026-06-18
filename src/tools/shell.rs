// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/shell.rs

//! Shell execution tool with basic sandboxing
//! 
//! Allows the agent to execute shell commands with safety restrictions.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::MiraError;

/// Shell execution tool with sandboxing
pub struct ShellExecuteTool {
    timeout_secs: u64,
    blocked_patterns: Vec<String>,
}

impl ShellExecuteTool {
    pub fn new(timeout_secs: u64) -> Self {
        // Blocked command patterns for safety.
        // Note: this is a best-effort deny-list, not a full sandbox. For production
        // use consider running commands inside a container or as a restricted user.
        let blocked_patterns = vec![
            // rm variants targeting the root, home, or current directory
            "rm -rf /".to_string(),
            "rm -rf ~".to_string(),
            "rm -rf .".to_string(),
            "rm -fr /".to_string(),
            "rm -fr ~".to_string(),
            "--no-preserve-root".to_string(),
            // Fork bomb
            ":(){:|:&};:".to_string(),
            // Disk destruction
            "mkfs".to_string(),
            "dd if=/dev/zero".to_string(),
            "dd if=/dev/random".to_string(),
            "> /dev/sd".to_string(),
            // Dangerous privilege escalation
            "sudo rm".to_string(),
            "sudo dd".to_string(),
            "sudo mkfs".to_string(),
            "sudo chmod".to_string(),
            // System shutdown
            "shutdown".to_string(),
            "reboot".to_string(),
            "halt".to_string(),
            "poweroff".to_string(),
            // Dangerous permission changes
            "chmod -R 777 /".to_string(),
            "chmod 777 /".to_string(),
        ];
        
        Self {
            timeout_secs,
            blocked_patterns,
        }
    }
    
    /// Check if command contains blocked patterns
    fn is_blocked(&self, command: &str) -> bool {
        for pattern in &self.blocked_patterns {
            if command.contains(pattern) {
                warn!("Blocked dangerous command containing: {}", pattern);
                return true;
            }
        }
        false
    }
}

#[async_trait]
impl Tool for ShellExecuteTool {
    fn name(&self) -> &str {
        "shell_execute"
    }
    
    fn description(&self) -> &str {
        "Execute a shell command and return its output. Use this to run system commands, check files, etc."
    }

    fn tier(&self) -> Tier { Tier::Code }
    
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            },
            "required": ["command"]
        })
    }
    
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        // Extract command from arguments
        let command = args.get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError(
                "Missing 'command' argument".to_string()
            ))?;
        
        // Safety check: block dangerous commands
        if self.is_blocked(command) {
            return Ok(ToolResult::failure(
                format!("Command blocked for safety: {}", command)
            ));
        }
        
        debug!("Executing shell command: {}", command);
        
        // Execute with timeout using async tokio::process::Command
        let result = timeout(
            Duration::from_secs(self.timeout_secs),
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
        ).await;
        
        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                
                if output.status.success() {
                    Ok(ToolResult::success(stdout))
                } else {
                    Ok(ToolResult::failure(
                        format!("Command failed with code {}: {}", 
                               output.status.code().unwrap_or(-1),
                               stderr)
                    ))
                }
            }
            Ok(Err(e)) => Err(MiraError::ToolError(
                format!("Failed to execute command: {}", e)
            )),
            Err(_) => Err(MiraError::ToolError(
                format!("Command timed out after {} seconds", self.timeout_secs)
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_simple_command() {
        let tool = ShellExecuteTool::new(30);
        let args = json!({"command": "echo 'Hello World'"});
        
        let result = tool.execute(args).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Hello World"));
    }
    
    #[tokio::test]
    async fn test_blocked_command() {
        let tool = ShellExecuteTool::new(30);
        let args = json!({"command": "rm -rf / --no-preserve-root"});

        let result = tool.execute(args).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("blocked"));
    }

    #[tokio::test]
    async fn test_blocked_rm_rf_home() {
        let tool = ShellExecuteTool::new(30);
        let result = tool.execute(json!({"command": "rm -rf ~"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_blocked_fork_bomb() {
        let tool = ShellExecuteTool::new(30);
        let result = tool.execute(json!({"command": ":(){:|:&};:"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_blocked_mkfs() {
        let tool = ShellExecuteTool::new(30);
        let result = tool.execute(json!({"command": "mkfs.ext4 /dev/sda1"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_blocked_shutdown() {
        let tool = ShellExecuteTool::new(30);
        let result = tool.execute(json!({"command": "shutdown -h now"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_blocked_sudo_rm() {
        let tool = ShellExecuteTool::new(30);
        let result = tool.execute(json!({"command": "sudo rm -rf /var/log"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_blocked_dd_random() {
        let tool = ShellExecuteTool::new(30);
        let result = tool.execute(json!({"command": "dd if=/dev/random of=/dev/sda"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_command_with_exit_code() {
        let tool = ShellExecuteTool::new(30);
        let result = tool.execute(json!({"command": "exit 1"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_missing_command_argument() {
        let tool = ShellExecuteTool::new(30);
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, crate::MiraError::ToolError(_)));
    }

    #[tokio::test]
    async fn test_multiline_command() {
        let tool = ShellExecuteTool::new(30);
        let result = tool.execute(json!({"command": "echo line1 && echo line2"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("line1"));
        assert!(result.output.contains("line2"));
    }
}
