// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/filesystem.rs

//! Filesystem access tools
//! 
//! Provides file read and write capabilities with safety restrictions.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, warn};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::MiraError;

// File read tool
pub struct FileReadTool {
    allowed_dirs: Vec<PathBuf>,
    // when set + the call's args carry an `_agent_id`,
    // every read consults the engine with a `FilesystemAccess` event
    // (mode = "read"). Deny short-circuits to a tool-failure with the
    // `policy/<rule>` prefix. None = no engine gating (legacy / dev).
    policy_engine: Option<Arc<dyn crate::policy::PolicyEngine>>,
}

impl FileReadTool {
    pub fn new(allowed_dirs: Option<Vec<String>>) -> Self {
        // If no dirs specified, allow all (for testing/development)
        let dirs = allowed_dirs.unwrap_or_else(|| vec![]);
        let allowed_dirs: Vec<PathBuf> = dirs.into_iter().map(PathBuf::from).collect();

        Self { allowed_dirs, policy_engine: None }
    }

    // attach a [`crate::policy::PolicyEngine`]. Same opt-in
    // posture as the other  integration points.
    pub fn with_policy_engine(
        mut self, engine: Arc<dyn crate::policy::PolicyEngine>,
    ) -> Self {
        self.policy_engine = Some(engine);
        self
    }
    
    // Check if path is within allowed directories
    fn is_allowed(&self, path: &Path) -> Result<bool, MiraError> {
        // If no specific allowed dirs set, allow everything but warn loudly.
        if self.allowed_dirs.is_empty() {
            warn!(
                "FileReadTool has no directory restrictions — any readable path is accessible. \
                 Pass allowed_dirs to FileReadTool::new() to restrict access."
            );
            return Ok(true);
        }

        let canonical = fs::canonicalize(path)
            .map_err(|e| MiraError::ToolError(
                format!("Cannot resolve path {}: {}", path.display(), e)
            ))?;

        for allowed in &self.allowed_dirs {
            let allowed_canonical = fs::canonicalize(allowed)
                .map_err(|e| MiraError::ToolError(
                    format!("Cannot resolve allowed dir {}: {}", allowed.display(), e)
                ))?;

            if canonical.starts_with(&allowed_canonical) {
                return Ok(true);
            }
        }

        warn!("FileReadTool blocked access to path outside allowed directories: {}", path.display());
        Ok(false)
    }
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }
    
    fn description(&self) -> &str {
        "Read the contents of a file. Returns the file content as text."
    }

    fn tier(&self) -> Tier { Tier::Filesystem }
    
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                }
            },
            "required": ["path"]
        })
    }
    
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError(
                "Missing 'path' argument".to_string()
            ))?;

        let path_buf = PathBuf::from(path);

        // admin policy gate runs before the per-tool
        // is_allowed check. Admin rules get the first say AND don't
        // have to wait on canonicalize for paths that don't exist
        // yet (the engine's payload is the path string verbatim).
        if let Some(reason) = consult_filesystem_policy(
            self.policy_engine.as_ref(), &args, &path_buf, "read",
        ).await {
            return Ok(ToolResult::failure(reason));
        }

        // Security check
        if !self.is_allowed(&path_buf)? {
            return Ok(ToolResult::failure(
                format!("Access denied: {} is not in allowed directories", path)
            ));
        }

        debug!("Reading file: {}", path);

        let content = fs::read_to_string(&path_buf)
            .map_err(|e| MiraError::ToolError(
                format!("Failed to read {}: {}", path, e)
            ))?;

        Ok(ToolResult::success(content))
    }
}

// File write tool
pub struct FileWriteTool {
    allowed_dirs: Vec<PathBuf>,
    // same posture as `FileReadTool::policy_engine`.
    policy_engine: Option<Arc<dyn crate::policy::PolicyEngine>>,
}

impl FileWriteTool {
    pub fn new(allowed_dirs: Option<Vec<String>>) -> Self {
        // If no dirs specified, allow all (for testing/development)
        let dirs = allowed_dirs.unwrap_or_else(|| vec![]);
        let allowed_dirs: Vec<PathBuf> = dirs.into_iter().map(PathBuf::from).collect();

        Self { allowed_dirs, policy_engine: None }
    }

    // attach a policy engine. Same opt-in posture as the
    // other  integration points; no engine = no gating.
    pub fn with_policy_engine(
        mut self, engine: Arc<dyn crate::policy::PolicyEngine>,
    ) -> Self {
        self.policy_engine = Some(engine);
        self
    }
    
    // Check if path is within allowed directories
    fn is_allowed(&self, path: &Path) -> Result<bool, MiraError> {
        // If no specific allowed dirs set, allow everything but warn loudly.
        if self.allowed_dirs.is_empty() {
            warn!(
                "FileWriteTool has no directory restrictions — any writable path is accessible. \
                 Pass allowed_dirs to FileWriteTool::new() to restrict access."
            );
            return Ok(true);
        }

        let parent = path.parent()
            .ok_or_else(|| MiraError::ToolError(
                "Invalid path: no parent directory".to_string()
            ))?;

        let canonical_parent = fs::canonicalize(parent)
            .map_err(|e| MiraError::ToolError(
                format!("Cannot resolve parent {}: {}", parent.display(), e)
            ))?;

        for allowed in &self.allowed_dirs {
            let allowed_canonical = fs::canonicalize(allowed)
                .map_err(|e| MiraError::ToolError(
                    format!("Cannot resolve allowed dir {}: {}", allowed.display(), e)
                ))?;

            if canonical_parent.starts_with(&allowed_canonical) {
                return Ok(true);
            }
        }

        warn!("FileWriteTool blocked write to path outside allowed directories: {}", path.display());
        Ok(false)
    }
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }
    
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does."
    }

    fn tier(&self) -> Tier { Tier::Filesystem }
    
    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path where to write the file"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }
    
    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let path = args.get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError(
                "Missing 'path' argument".to_string()
            ))?;

        let content = args.get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError(
                "Missing 'content' argument".to_string()
            ))?;

        let path_buf = PathBuf::from(path);

        // admin policy gate runs before is_allowed for
        // the same reason as FileReadTool: admin rules first, no
        // canonicalize for paths whose parent doesn't exist yet.
        if let Some(reason) = consult_filesystem_policy(
            self.policy_engine.as_ref(), &args, &path_buf, "write",
        ).await {
            return Ok(ToolResult::failure(reason));
        }

        // Security check
        if !self.is_allowed(&path_buf)? {
            return Ok(ToolResult::failure(
                format!("Access denied: {} is not in allowed directories", path)
            ));
        }

        debug!("Writing to file: {} ({} bytes)", path, content.len());
        
        // Create parent directories if needed
        if let Some(parent) = path_buf.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| MiraError::ToolError(
                    format!("Failed to create directory {}: {}", parent.display(), e)
                ))?;
        }
        
        fs::write(&path_buf, content)
            .map_err(|e| MiraError::ToolError(
                format!("Failed to write {}: {}", path, e)
            ))?;
        
        Ok(ToolResult::success(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path
        )))
    }
}

// ─── shared policy gate ─────────────────────────────────

// Build a `FilesystemAccess` event from the call args + path + mode
// and consult the engine. Returns `Some(reason)` to deny (caller
// surfaces it as `ToolResult::failure`) or `None` to allow. Three
// early-outs in cheap-first order:
// 1. No engine wired → None.
// 2. `_agent_id` absent in args → None (no agent context = no
// per-Skill rules can match).
// 3. `_agent_id` present but unparseable UUID → None (fail-open,
// same reasoning as the tool registry's per-call gate).
// // Mode strings ("read", "write") match what the
// `FilesystemAllowlistRule` (slice D2) expects on the engine side
// keep these in sync when extending to "list" / "create" / etc.
async fn consult_filesystem_policy(
    engine: Option<&Arc<dyn crate::policy::PolicyEngine>>,
    args:   &ToolArgs,
    path:   &Path,
    mode:   &str,
) -> Option<String> {
    let engine = engine?;
    let agent_id = args.get("_agent_id")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(crate::agent::instance::AgentId)?;
    let skill_id = args.get("_skill_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let event = crate::policy::PolicyEvent::FilesystemAccess {
        agent_id,
        skill_id,
        path: path.to_path_buf(),
        mode: mode.to_owned(),
    };
    match engine.evaluate(&event).await {
        crate::policy::PolicyDecision::Allow => None,
        crate::policy::PolicyDecision::Deny { rule, reason } => {
            Some(format!("policy/{rule} denied: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    #[tokio::test]
    async fn test_file_read() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "Hello World").unwrap();
        
        let tool = FileReadTool::new(Some(vec![temp_dir.path().to_string_lossy().to_string()]));
        let args = json!({"path": file_path.to_string_lossy()});
        
        let result = tool.execute(args).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "Hello World");
    }
    
    #[tokio::test]
    async fn test_file_write() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("output.txt");

        let tool = FileWriteTool::new(Some(vec![temp_dir.path().to_string_lossy().to_string()]));
        let args = json!({
            "path": file_path.to_string_lossy(),
            "content": "Test content"
        });

        let result = tool.execute(args).await.unwrap();
        assert!(result.success);
        assert!(fs::read_to_string(&file_path).unwrap() == "Test content");
    }

    // ── policy engine integration ─────────────────────────

    use crate::agent::instance::AgentId;
    use crate::policy::{AllowAllEngine, DenyAllEngine, PolicyDecision, PolicyEngine, PolicyEvent};
    use std::sync::Mutex as StdMutex;

    // Engine that records every event + replies via a closure.
    struct RecordingEngine {
        seen:   StdMutex<Vec<PolicyEvent>>,
        decide: Box<dyn Fn(&PolicyEvent) -> PolicyDecision + Send + Sync>,
    }
    #[async_trait]
    impl PolicyEngine for RecordingEngine {
        async fn evaluate(&self, event: &PolicyEvent) -> PolicyDecision {
            self.seen.lock().unwrap().push(event.clone());
            (self.decide)(event)
        }
    }

    fn write_a_file(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let p = dir.path().join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[tokio::test]
    async fn read_engine_consult_skipped_when_args_lack_agent_id() {
        let dir = TempDir::new().unwrap();
        let path = write_a_file(&dir, "x.txt", "data");
        // Engine that would deny if asked.
        let tool = FileReadTool::new(Some(vec![dir.path().to_string_lossy().into()]))
            .with_policy_engine(Arc::new(DenyAllEngine::new("would fire")));
        let r = tool.execute(json!({"path": path.to_string_lossy()})).await.unwrap();
        assert!(r.success, "missing _agent_id should skip engine consult");
        assert_eq!(r.output, "data");
    }

    #[tokio::test]
    async fn read_engine_deny_short_circuits_to_failure_with_rule_in_message() {
        let dir = TempDir::new().unwrap();
        let path = write_a_file(&dir, "x.txt", "data");
        let tool = FileReadTool::new(Some(vec![dir.path().to_string_lossy().into()]))
            .with_policy_engine(Arc::new(DenyAllEngine::new("nope")));
        let r = tool.execute(json!({
            "path":      path.to_string_lossy(),
            "_agent_id": AgentId::new().to_string(),
        })).await.unwrap();
        assert!(!r.success);
        let err = r.error.unwrap_or_default();
        assert!(err.contains("policy/test/deny-all"), "got: {err}");
        assert!(err.contains("nope"),                 "got: {err}");
    }

    #[tokio::test]
    async fn read_emits_filesystem_access_event_with_correct_mode_and_skill() {
        let dir = TempDir::new().unwrap();
        let path = write_a_file(&dir, "x.txt", "data");
        let engine = Arc::new(RecordingEngine {
            seen:   StdMutex::new(Vec::new()),
            decide: Box::new(|_| PolicyDecision::Allow),
        });
        let tool = FileReadTool::new(Some(vec![dir.path().to_string_lossy().into()]))
            .with_policy_engine(engine.clone());
        let agent = AgentId::new();
        let _ = tool.execute(json!({
            "path":      path.to_string_lossy(),
            "_agent_id": agent.to_string(),
            "_skill_id": "com.example.fs",
        })).await.unwrap();

        let seen = engine.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        match &seen[0] {
            PolicyEvent::FilesystemAccess { agent_id, skill_id, path: p, mode } => {
                assert_eq!(*agent_id, agent);
                assert_eq!(skill_id.as_deref(), Some("com.example.fs"));
                assert_eq!(p, &path);
                assert_eq!(mode, "read");
            }
            other => panic!("expected FilesystemAccess, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_allow_engine_lets_existing_pipeline_run() {
        let dir = TempDir::new().unwrap();
        let path = write_a_file(&dir, "x.txt", "data");
        let tool = FileReadTool::new(Some(vec![dir.path().to_string_lossy().into()]))
            .with_policy_engine(Arc::new(AllowAllEngine));
        let r = tool.execute(json!({
            "path":      path.to_string_lossy(),
            "_agent_id": AgentId::new().to_string(),
        })).await.unwrap();
        assert!(r.success);
        assert_eq!(r.output, "data");
    }

    #[tokio::test]
    async fn read_invalid_agent_id_uuid_treated_as_missing() {
        // Same fail-open posture as 1.2 / 1.3.
        let dir = TempDir::new().unwrap();
        let path = write_a_file(&dir, "x.txt", "data");
        let tool = FileReadTool::new(Some(vec![dir.path().to_string_lossy().into()]))
            .with_policy_engine(Arc::new(DenyAllEngine::new("would fire")));
        let r = tool.execute(json!({
            "path":      path.to_string_lossy(),
            "_agent_id": "not-a-uuid",
        })).await.unwrap();
        assert!(r.success, "unparseable _agent_id should skip the engine");
    }

    #[tokio::test]
    async fn write_engine_deny_blocks_write_and_reports_rule() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("blocked.txt");
        let tool = FileWriteTool::new(Some(vec![dir.path().to_string_lossy().into()]))
            .with_policy_engine(Arc::new(DenyAllEngine::new("write blocked")));
        let r = tool.execute(json!({
            "path":      path.to_string_lossy(),
            "content":   "should never land",
            "_agent_id": AgentId::new().to_string(),
        })).await.unwrap();
        assert!(!r.success);
        let err = r.error.unwrap_or_default();
        assert!(err.contains("policy/test/deny-all"), "got: {err}");
        // Critical: file MUST NOT exist on disk after a denied write.
        assert!(!path.exists(), "file was written despite policy denial");
    }

    #[tokio::test]
    async fn write_emits_filesystem_access_event_with_write_mode() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ok.txt");
        let engine = Arc::new(RecordingEngine {
            seen:   StdMutex::new(Vec::new()),
            decide: Box::new(|_| PolicyDecision::Allow),
        });
        let tool = FileWriteTool::new(Some(vec![dir.path().to_string_lossy().into()]))
            .with_policy_engine(engine.clone());
        let _ = tool.execute(json!({
            "path":      path.to_string_lossy(),
            "content":   "hello",
            "_agent_id": AgentId::new().to_string(),
            "_skill_id": "com.example.fs",
        })).await.unwrap();

        match &engine.seen.lock().unwrap()[0] {
            PolicyEvent::FilesystemAccess { mode, .. } => assert_eq!(mode, "write"),
            other => panic!("expected FilesystemAccess, got {other:?}"),
        }
        // Sanity: write actually happened on Allow.
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[tokio::test]
    async fn engine_consult_runs_before_is_allowed_check() {
        // Critical ordering test: a path OUTSIDE allowed_dirs that
        // doesn't exist would normally fail at `is_allowed` because
        // canonicalize fails on missing files. With the engine
        // consult running first, we should see a `policy/...` deny
        // (not the canonicalize error). This proves the engine
        // gate gets first say.
        let allowed = TempDir::new().unwrap();
        let other   = TempDir::new().unwrap();
        let bogus   = other.path().join("does-not-exist.txt");

        let tool = FileReadTool::new(Some(vec![allowed.path().to_string_lossy().into()]))
            .with_policy_engine(Arc::new(DenyAllEngine::new("admin block")));
        let r = tool.execute(json!({
            "path":      bogus.to_string_lossy(),
            "_agent_id": AgentId::new().to_string(),
        })).await.unwrap();
        assert!(!r.success);
        let err = r.error.unwrap_or_default();
        assert!(err.contains("policy/test/deny-all"),
            "engine should fire before is_allowed; got: {err}");
        assert!(!err.to_lowercase().contains("cannot resolve"),
            "is_allowed leaked through: {err}");
    }
}
