// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/pdf.rs
//! PDF text extraction (Tier 1 — pure, workspace-jailed).
//!
//! Extracts text from a PDF the user has placed in their MIRA workspace at
//! `$MIRA_DATA/workspaces/{user_id}/`. The tool **does not** accept arbitrary
//! filesystem paths — any attempt to escape the workspace via `..` or an
//! absolute path is rejected. This keeps the tool in the Pure tier: it only
//! reads MIRA-owned files, the same way `recall_history` only reads the
//! MIRA-owned history DB.
//!
//! Output caps (belt + braces against giant PDFs):
//! - default `max_pages` = 20, hard cap = 100
//! - extracted text truncated to `MAX_TEXT_CHARS` chars with `…` marker

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::fs;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::task;
use tracing::debug;

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::MiraError;

const DEFAULT_MAX_PAGES: u32 = 20;
const HARD_MAX_PAGES:    u32 = 100;
const MAX_TEXT_CHARS:    usize = 80_000;

pub struct PdfExtractTool {
    data_dir: Arc<PathBuf>,
}

impl PdfExtractTool {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir: Arc::new(data_dir) }
    }

    fn workspace_root(&self, user_id: &str) -> PathBuf {
        self.data_dir.join("workspaces").join(user_id)
    }
}

#[async_trait]
impl Tool for PdfExtractTool {
    fn name(&self) -> &str { "pdf_extract" }

    fn description(&self) -> &str {
        "Extract plain text from a PDF file the user has placed in their MIRA \
         workspace. `path` is a *relative* path inside the workspace — absolute \
         paths and '..' escapes are rejected. Returns the extracted text, capped \
         at roughly 80K characters. Use when the user asks about a document \
         they've uploaded or references a .pdf by name."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description":
                        "Relative path to the PDF inside the user's workspace \
                         (e.g. 'report.pdf' or 'notes/spec.pdf'). Must not be \
                         absolute and must not contain '..' segments."
                },
                "max_pages": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": HARD_MAX_PAGES as i64,
                    "description":
                        "Max pages to extract (default 20, hard cap 100). \
                         Pages beyond this are skipped silently."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = args.get("_user_id").and_then(|v| v.as_str())
            .ok_or_else(|| MiraError::ToolError(
                "pdf_extract called without _user_id (chat handler must inject)".to_string()
            ))?
            .to_owned();

        let rel_path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
        if rel_path.is_empty() {
            return Ok(ToolResult::failure("pdf_extract: `path` is required"));
        }

        let max_pages = args.get("max_pages").and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(DEFAULT_MAX_PAGES)
            .clamp(1, HARD_MAX_PAGES);

        let ws_root = self.workspace_root(&user_id);

        // Ensure workspace exists so canonicalize succeeds. Cheap no-op when
        // already present; on first access it seeds an empty directory.
        if let Err(e) = fs::create_dir_all(&ws_root) {
            return Ok(ToolResult::failure(
                format!("pdf_extract: cannot create workspace {}: {}", ws_root.display(), e),
            ));
        }

        let resolved = match resolve_inside_workspace(&ws_root, rel_path) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(format!("pdf_extract: {}", e))),
        };

        debug!("pdf_extract: user={} path={} max_pages={}", user_id, resolved.display(), max_pages);

        // lopdf is synchronous and CPU-bound — offload to a blocking thread so
        // the async runtime isn't stalled on a big document.
        let resolved_clone = resolved.clone();
        let res = task::spawn_blocking(move || extract_pdf_text(&resolved_clone, max_pages))
            .await
            .map_err(|e| MiraError::ToolError(format!("pdf_extract: task join: {}", e)))?;

        match res {
            Ok((text, pages_read, pages_total)) => {
                let (final_text, truncated) = cap_text(text, MAX_TEXT_CHARS);
                let body = json!({
                    "path":          rel_path,
                    "pages_read":    pages_read,
                    "pages_total":   pages_total,
                    "truncated":     truncated,
                    "text":          final_text,
                });
                Ok(ToolResult::success(body.to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("pdf_extract: {}", e))),
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Resolve `rel` against `ws_root` and verify the result is inside the
/// workspace. Rejects absolute paths and `..` segments up-front, then belt-
/// and-braces with a canonical prefix check (catches symlinks pointing out).
fn resolve_inside_workspace(ws_root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(format!("path must be relative to the workspace, got: {}", rel));
    }
    for component in rel_path.components() {
        use std::path::Component;
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => return Err("'..' is not allowed in path".to_string()),
            Component::RootDir | Component::Prefix(_) =>
                return Err("absolute / rooted paths are not allowed".to_string()),
        }
    }

    let joined = ws_root.join(rel_path);
    let canonical = fs::canonicalize(&joined)
        .map_err(|e| format!("cannot resolve {}: {}", joined.display(), e))?;
    let ws_canonical = fs::canonicalize(ws_root)
        .map_err(|e| format!("cannot resolve workspace {}: {}", ws_root.display(), e))?;

    if !canonical.starts_with(&ws_canonical) {
        return Err("path escapes workspace".to_string());
    }
    Ok(canonical)
}

/// Load the PDF and extract text from up to `max_pages` pages.
/// Returns `(text, pages_read, pages_total)`.
fn extract_pdf_text(path: &Path, max_pages: u32) -> Result<(String, u32, u32), String> {
    let doc = lopdf::Document::load(path)
        .map_err(|e| format!("failed to load PDF: {}", e))?;
    let pages = doc.get_pages();
    let pages_total = pages.len() as u32;

    let page_numbers: Vec<u32> = pages.keys().copied().take(max_pages as usize).collect();
    let pages_read = page_numbers.len() as u32;

    let text = doc.extract_text(&page_numbers)
        .map_err(|e| format!("failed to extract text: {}", e))?;

    Ok((text, pages_read, pages_total))
}

/// Char-safe cap. Returns `(maybe-truncated text, truncated?)`.
fn cap_text(s: String, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s, false);
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    (out, true)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_tool() -> (TempDir, PdfExtractTool, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let tool = PdfExtractTool::new(tmp.path().to_path_buf());
        let ws = tmp.path().join("workspaces").join("user-1");
        fs::create_dir_all(&ws).unwrap();
        (tmp, tool, ws)
    }

    #[test]
    fn cap_text_truncates_and_marks() {
        let s = "a".repeat(10);
        let (out, truncated) = cap_text(s, 5);
        assert!(truncated);
        assert_eq!(out.chars().count(), 6); // 5 + ellipsis
        assert!(out.ends_with('…'));

        let (out, truncated) = cap_text("short".into(), 50);
        assert!(!truncated);
        assert_eq!(out, "short");
    }

    #[test]
    fn resolve_rejects_absolute() {
        let tmp = TempDir::new().unwrap();
        let err = resolve_inside_workspace(tmp.path(), "/etc/passwd").unwrap_err();
        assert!(err.contains("absolute") || err.contains("relative"));
    }

    #[test]
    fn resolve_rejects_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let err = resolve_inside_workspace(tmp.path(), "../escape.pdf").unwrap_err();
        assert!(err.contains(".."));
    }

    #[test]
    fn resolve_allows_subdir() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("notes");
        fs::create_dir_all(&sub).unwrap();
        let f = sub.join("a.pdf");
        fs::write(&f, b"x").unwrap();
        let got = resolve_inside_workspace(tmp.path(), "notes/a.pdf").unwrap();
        assert!(got.ends_with("notes/a.pdf"));
    }

    #[tokio::test]
    async fn missing_user_id_is_rejected() {
        let (_tmp, tool, _ws) = setup_tool();
        // Missing _user_id is a hard error — matches recall_history semantics.
        let err = tool.execute(json!({"path": "x.pdf"})).await.unwrap_err();
        assert!(format!("{}", err).contains("_user_id"));
    }

    #[tokio::test]
    async fn empty_path_is_rejected() {
        let (_tmp, tool, _ws) = setup_tool();
        let out = tool.execute(json!({"_user_id": "user-1", "path": "  "})).await.unwrap();
        assert!(!out.success);
    }

    #[tokio::test]
    async fn absolute_path_blocked() {
        let (_tmp, tool, _ws) = setup_tool();
        let out = tool.execute(json!({
            "_user_id": "user-1",
            "path": "/etc/passwd"
        })).await.unwrap();
        assert!(!out.success);
    }

    #[tokio::test]
    async fn parent_dir_blocked() {
        let (_tmp, tool, _ws) = setup_tool();
        let out = tool.execute(json!({
            "_user_id": "user-1",
            "path": "../../../etc/passwd"
        })).await.unwrap();
        assert!(!out.success);
    }

    #[tokio::test]
    async fn nonexistent_file_returns_failure_not_error() {
        let (_tmp, tool, _ws) = setup_tool();
        let out = tool.execute(json!({
            "_user_id": "user-1",
            "path": "nope.pdf"
        })).await.unwrap();
        assert!(!out.success);
    }

    #[tokio::test]
    async fn non_pdf_file_fails_cleanly() {
        let (_tmp, tool, ws) = setup_tool();
        fs::write(ws.join("fake.pdf"), b"not a real pdf").unwrap();
        let out = tool.execute(json!({
            "_user_id": "user-1",
            "path": "fake.pdf"
        })).await.unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap().contains("pdf_extract"));
    }
}
