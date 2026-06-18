// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/wiki.rs
//! Model-callable wiki tools (Slice D).
//!
//! Exposes the per-user wiki to the agent as a skill with five tools:
//!
//! - `wiki_search` — query against page paths / titles / tags / body.
//! - `wiki_read` — fetch a single page's frontmatter + body.
//! - `wiki_append_section` — append text under a `## Heading` on an
//!   existing page.
//! - `wiki_write_page` — create or replace a page.
//! - `wiki_log_entry` — append a timestamped entry to `log.md`.
//!
//! Identity scoping mirrors `recall_history` / `memory_supersede`: the
//! chat handler injects a trusted `_user_id` into every tool call so each
//! tool resolves the correct per-user wiki via the shared
//! [`WikiRegistry`].
//!
//! Writes flow through the same audit + applier pipeline used by the
//! UI and the auto-extractor. The `write_mode` carried on each write
//! tool decides whether the op lands as `pending` (review queue, the
//! default) or is applied immediately. The ChatGPT-memory lessons
//! mitigation is honoured: by default the wiki is never mutated without
//! the user's explicit approval.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::debug;

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::wiki::{
    frontmatter, LogKind, PageFrontmatter, Provenance, WikiOp, WikiPath, WikiRegistry,
    WikiSystem,
};
use crate::MiraError;

/// Max chars per page returned by `wiki_read`. The model can re-read with
/// a narrower `section` filter (TODO Slice D+) if it needs more.
const PAGE_PREVIEW_CHARS: usize = 4_000;

/// Max chars of body returned per `wiki_search` hit. Designed to fit ~3-5
/// hits in a single response under the model's working budget.
const SEARCH_PREVIEW_CHARS: usize = 280;

/// Default / cap for `top_k` in `wiki_search`.
const SEARCH_DEFAULT_TOP_K: usize = 8;
const SEARCH_MAX_TOP_K:     usize = 25;

/// How a wiki write tool routes its op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteMode { Review, Auto, Off }

impl WriteMode {
    /// Parse the config string. Unknown values fall through to the safer
    /// `Review` mode rather than silently failing.
    fn from_str(s: &str) -> Self {
        match s {
            "auto"  => WriteMode::Auto,
            "off"   => WriteMode::Off,
            _       => WriteMode::Review,
        }
    }
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Pull the trusted `_user_id` out of injected tool args. Tools that
/// can't resolve a caller refuse to run rather than touching the wrong
/// user's wiki.
fn require_user_id(args: &ToolArgs, tool: &str) -> Result<String, ToolResult> {
    args.get("_user_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .ok_or_else(|| ToolResult::failure(format!(
            "{tool} called without _user_id (chat handler must inject)"
        )))
}

/// Resolve the per-user wiki, mapping wiki errors to a tool failure.
fn open_user_wiki(reg: &WikiRegistry, user_id: &str, tool: &str)
    -> Result<Arc<WikiSystem>, ToolResult>
{
    reg.for_user(user_id).map_err(|e| {
        ToolResult::failure(format!("{tool}: failed to open user wiki: {e}"))
    })
}

/// Parse the `path` arg into a [`WikiPath`] or return a tool failure.
fn parse_path(args: &ToolArgs, tool: &str) -> Result<WikiPath, ToolResult> {
    let raw = args.get("path").and_then(|v| v.as_str()).unwrap_or("").trim();
    if raw.is_empty() {
        return Err(ToolResult::failure(format!("{tool}: `path` is required")));
    }
    WikiPath::parse(raw)
        .map_err(|e| ToolResult::failure(format!("{tool}: invalid path '{raw}': {e}")))
}

/// Char-safe truncate (won't split a multi-byte UTF-8 sequence).
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars { return s.to_string(); }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Build a turn-scoped provenance. Tool args may carry `_conversation_id`
/// (chat handler injects it) but never `_turn_id`, so we mint one per
/// call — the audit row tracks the act of the model invoking the tool.
fn provenance_for_call(args: &ToolArgs, actor: &str) -> Provenance {
    let conv_id = args.get("_conversation_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let turn_id = uuid::Uuid::now_v7().to_string();
    Provenance::from_turn(actor, &turn_id, conv_id)
}

/// Route a write op through the wiki audit pipeline according to
/// `mode`. Returns the op id; the JSON payload the tool returns
/// communicates the status so the model can tell the user whether
/// it landed or is queued.
fn submit_write(
    wiki: &WikiSystem,
    op: WikiOp,
    prov: Provenance,
    mode: WriteMode,
    tool: &str,
) -> Result<(String, &'static str), ToolResult> {
    match mode {
        WriteMode::Off => Err(ToolResult::failure(format!(
            "{tool}: wiki writes are disabled in this deployment \
             (wiki.agent_tools.write_mode = \"off\")"
        ))),
        WriteMode::Auto => match wiki.submit_and_apply(op, prov) {
            Ok(id) => Ok((id, "applied")),
            Err(e) => Err(ToolResult::failure(format!("{tool}: apply failed: {e}"))),
        },
        WriteMode::Review => match wiki.submit_op(op, prov) {
            Ok(id) => Ok((id, "pending")),
            Err(e) => Err(ToolResult::failure(format!("{tool}: submit failed: {e}"))),
        },
    }
}

// ── wiki_search ──────────────────────────────────────────────────────────────

/// Substring search over page path, title, tags, and body. Pages are
/// scored by total token-hit count across those fields; results sorted
/// by score then path. Designed for "the model wants to find a page it
/// vaguely remembers" rather than "the user wants full-text search."
pub struct WikiSearchTool {
    registry: Arc<WikiRegistry>,
}

impl WikiSearchTool {
    pub fn new(registry: Arc<WikiRegistry>) -> Self { Self { registry } }
}

#[async_trait]
impl Tool for WikiSearchTool {
    fn name(&self) -> &str { "wiki_search" }

    fn description(&self) -> &str {
        "Search the user's wiki by keyword. Returns matching pages with \
         path, title, and a short body preview. Use this to find a page \
         when you don't already know its exact path — e.g. 'pong project', \
         'cooking recipes'. The wiki is the user's long-term knowledge \
         base; pages are markdown files they (or you, with their \
         approval) have written. To read a full page, follow up with \
         `wiki_read`."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description":
                        "Keywords or short phrase to match. Tokens of length \
                         ≥3 are matched as substrings against page path, \
                         title, tags, and body. Empty query is allowed when \
                         `path_prefix` is set."
                },
                "path_prefix": {
                    "type": "string",
                    "description":
                        "Optional prefix to restrict results to (e.g. \
                         'pages/projects/'). Useful when you know the rough \
                         area of the wiki you want to browse."
                },
                "top_k": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": SEARCH_MAX_TOP_K as i64,
                    "description":
                        "Maximum hits to return (default 8, hard-capped at 25)."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        let wiki = match open_user_wiki(&self.registry, &user_id, self.name()) {
            Ok(w) => w, Err(r) => return Ok(r),
        };

        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
        let prefix = args.get("path_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if query.is_empty() && prefix.is_empty() {
            return Ok(ToolResult::failure(
                "wiki_search: provide `query` or `path_prefix` (or both)"
            ));
        }
        let top_k = args.get("top_k")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(SEARCH_DEFAULT_TOP_K)
            .clamp(1, SEARCH_MAX_TOP_K);

        let tokens: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
            .filter(|w| w.len() >= 3)
            .map(|w| w.to_owned())
            .collect();

        let store = wiki.store();
        let pages = match store.list_pages() {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(
                format!("wiki_search: list_pages failed: {e}")
            )),
        };

        let mut scored: Vec<(usize, WikiPath, Option<String>, Vec<String>, String)> = Vec::new();
        for path in pages {
            if !prefix.is_empty() && !path.as_str().starts_with(prefix) { continue; }
            let page = match store.read_page(&path) {
                Ok(p) => p,
                Err(_) => continue, // skip unreadable
            };
            let title = page.frontmatter.title.clone().unwrap_or_default();
            let tags  = page.frontmatter.tags.clone();
            let body  = page.body.clone();

            let score = if tokens.is_empty() {
                // Prefix-only search — surface every page that matched the
                // prefix at score 1 so the result list reflects the filter.
                1
            } else {
                let path_l  = path.as_str().to_lowercase();
                let title_l = title.to_lowercase();
                let tags_l: String = tags.iter().map(|t| t.to_lowercase()).collect::<Vec<_>>().join(" ");
                let body_l  = body.to_lowercase();
                tokens.iter().map(|t| {
                    // Path/title/tag hits are worth more than body hits so
                    // a query like "pong" doesn't get drowned by any page
                    // that happens to mention the word in its body.
                    let p = if path_l.contains(t)  { 3 } else { 0 };
                    let ti = if title_l.contains(t) { 3 } else { 0 };
                    let tg = if tags_l.contains(t)  { 2 } else { 0 };
                    let b = if body_l.contains(t)  { 1 } else { 0 };
                    p + ti + tg + b
                }).sum()
            };
            if score == 0 { continue; }
            scored.push((
                score, path,
                Some(title).filter(|s| !s.is_empty()),
                tags, body,
            ));
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
        scored.truncate(top_k);

        let hits: Vec<Value> = scored.into_iter().map(|(score, path, title, tags, body)| {
            let preview = truncate_chars(body.trim(), SEARCH_PREVIEW_CHARS);
            json!({
                "path": path.as_str(),
                "title": title,
                "tags": tags,
                "score": score,
                "preview": preview,
            })
        }).collect();

        debug!("wiki_search: user={} query={:?} prefix={:?} hits={}",
               user_id, query, prefix, hits.len());

        Ok(ToolResult::success(json!({ "hits": hits }).to_string()))
    }
}

// ── wiki_read ────────────────────────────────────────────────────────────────

/// Read a single page by path. Returns frontmatter (title, writer, tags,
/// confidence, valid_from/to) and the body, char-safe-truncated to
/// [`PAGE_PREVIEW_CHARS`].
pub struct WikiReadTool {
    registry: Arc<WikiRegistry>,
}

impl WikiReadTool {
    pub fn new(registry: Arc<WikiRegistry>) -> Self { Self { registry } }
}

#[async_trait]
impl Tool for WikiReadTool {
    fn name(&self) -> &str { "wiki_read" }

    fn description(&self) -> &str {
        "Read a single page from the user's wiki by exact path. Returns \
         the page's frontmatter (title, writer policy, tags, etc.) and \
         body. Use this after `wiki_search` to fetch the full content of \
         a page you want to use. Bodies longer than ~4000 chars are \
         truncated at the tail; if a page is critical and got cut off, \
         tell the user rather than guessing the missing content."
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
                        "Wiki-relative path of the page, e.g. \
                         'pages/projects/pong.md', 'profile.md', \
                         'index.md'. Must end in `.md` and not contain \
                         `..`."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        let wiki = match open_user_wiki(&self.registry, &user_id, self.name()) {
            Ok(w) => w, Err(r) => return Ok(r),
        };
        let path = match parse_path(&args, self.name()) {
            Ok(p) => p, Err(r) => return Ok(r),
        };

        let store = wiki.store();
        let page = match store.try_read_page(&path) {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(ToolResult::failure(
                format!("wiki_read: page not found: {}", path.as_str())
            )),
            Err(e) => return Ok(ToolResult::failure(
                format!("wiki_read: read failed: {e}")
            )),
        };

        let body = truncate_chars(page.body.trim(), PAGE_PREVIEW_CHARS);
        let fm = &page.frontmatter;
        let payload = json!({
            "path": path.as_str(),
            "title": fm.title,
            "writer": fm.writer.as_str(),
            "tags": fm.tags,
            "confidence": fm.confidence,
            "valid_from": fm.valid_from.map(|d| d.to_string()),
            "valid_to":   fm.valid_to.map(|d| d.to_string()),
            "body": body,
            "truncated": page.body.chars().count() > PAGE_PREVIEW_CHARS,
        });
        Ok(ToolResult::success(payload.to_string()))
    }
}

// ── wiki_append_section ──────────────────────────────────────────────────────

/// Append text under a `## Heading` on an existing page. Lighter-weight
/// than `wiki_write_page` — preserves prior content and is the preferred
/// way for the model to add a fact to an existing page.
pub struct WikiAppendSectionTool {
    registry: Arc<WikiRegistry>,
    mode: WriteMode,
}

impl WikiAppendSectionTool {
    pub fn new(registry: Arc<WikiRegistry>, write_mode: &str) -> Self {
        Self { registry, mode: WriteMode::from_str(write_mode) }
    }
}

#[async_trait]
impl Tool for WikiAppendSectionTool {
    fn name(&self) -> &str { "wiki_append_section" }

    fn description(&self) -> &str {
        "Append text under a `## Heading` on an existing wiki page. If \
         the heading doesn't exist, it will be created at the end of the \
         page. Use this to add a single fact or note to an existing page \
         (e.g. a new bullet under '## Decisions' on a project page) — \
         it preserves the prior content. Prefer this over `wiki_write_page` \
         when adding to a page, since whole-page rewrites are more \
         disruptive. In `review` mode (default) the change lands as \
         pending until the user approves it."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "section", "body"],
            "properties": {
                "path":    { "type": "string",
                             "description": "Wiki-relative page path, e.g. 'pages/projects/pong.md'." },
                "section": { "type": "string",
                             "description": "The `## Heading` text (without the `## ` prefix) to append under." },
                "body":    { "type": "string",
                             "description": "Markdown text to append. Keep it concise — one or two bullets is ideal." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        let wiki = match open_user_wiki(&self.registry, &user_id, self.name()) {
            Ok(w) => w, Err(r) => return Ok(r),
        };
        let path = match parse_path(&args, self.name()) {
            Ok(p) => p, Err(r) => return Ok(r),
        };
        let section = args.get("section").and_then(|v| v.as_str()).unwrap_or("").trim();
        let body    = args.get("body")   .and_then(|v| v.as_str()).unwrap_or("").trim();
        if section.is_empty() {
            return Ok(ToolResult::failure("wiki_append_section: `section` is required"));
        }
        if body.is_empty() {
            return Ok(ToolResult::failure("wiki_append_section: `body` is required"));
        }

        let prov = provenance_for_call(&args, "agent");
        let op = WikiOp::AppendSection {
            path: path.clone(),
            section: section.to_owned(),
            body: body.to_owned(),
        };
        let (op_id, status) = match submit_write(&wiki, op, prov, self.mode, self.name()) {
            Ok(v) => v, Err(r) => return Ok(r),
        };
        Ok(ToolResult::success(json!({
            "op_id": op_id, "status": status, "path": path.as_str(), "section": section,
        }).to_string()))
    }
}

// ── wiki_write_page ──────────────────────────────────────────────────────────

/// Create or replace a wiki page. Provides a title, optional tags, and
/// a body. The writer policy on the resulting frontmatter is `agent`
/// since the model is the author of record.
pub struct WikiWritePageTool {
    registry: Arc<WikiRegistry>,
    mode: WriteMode,
}

impl WikiWritePageTool {
    pub fn new(registry: Arc<WikiRegistry>, write_mode: &str) -> Self {
        Self { registry, mode: WriteMode::from_str(write_mode) }
    }
}

#[async_trait]
impl Tool for WikiWritePageTool {
    fn name(&self) -> &str { "wiki_write_page" }

    fn description(&self) -> &str {
        "Create a new page or replace an existing one in the user's \
         wiki. Use this when you need to write a substantial chunk of \
         narrative content the user will want to keep — e.g. a project \
         summary, a recipe, a set of design notes. For incremental \
         additions to an existing page, prefer `wiki_append_section`. In \
         `review` mode (default) the change lands as pending until the \
         user approves it."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "title", "body"],
            "properties": {
                "path":  { "type": "string",
                           "description": "Wiki-relative page path. Conventionally under 'pages/<topic>/'." },
                "title": { "type": "string",
                           "description": "Short human-readable page title (appears in `index.md` and frontmatter)." },
                "body":  { "type": "string",
                           "description": "Full markdown body of the page (without YAML frontmatter — the tool adds that)." },
                "tags":  { "type": "array",
                           "items": { "type": "string" },
                           "description": "Optional tags. Keep them short ('project', 'recipe', 'in-progress')." }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        let wiki = match open_user_wiki(&self.registry, &user_id, self.name()) {
            Ok(w) => w, Err(r) => return Ok(r),
        };
        let path  = match parse_path(&args, self.name()) {
            Ok(p) => p, Err(r) => return Ok(r),
        };
        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
        let body  = args.get("body") .and_then(|v| v.as_str()).unwrap_or("").trim();
        if title.is_empty() {
            return Ok(ToolResult::failure("wiki_write_page: `title` is required"));
        }
        if body.is_empty() {
            return Ok(ToolResult::failure("wiki_write_page: `body` is required"));
        }
        let tags: Vec<String> = args.get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|t| t.as_str().map(|s| s.to_owned())).collect())
            .unwrap_or_default();

        // Refuse to touch the four special navigation files via this tool —
        // those have specific layouts the rest of the wiki relies on.
        // The user can edit them by hand or the system can rewrite them
        // programmatically (e.g. index updates after a successful apply).
        if path.is_special() {
            return Ok(ToolResult::failure(format!(
                "wiki_write_page: refusing to overwrite special navigation file '{}'; \
                 use `wiki_append_section` to add to it, or edit by hand",
                path.as_str()
            )));
        }

        let mut fm = PageFrontmatter::default();
        fm.title = Some(title.to_owned());
        fm.tags  = tags;
        fm.writer = frontmatter::Writer::Agent;

        let prov = provenance_for_call(&args, "agent");
        let op = WikiOp::WritePage { path: path.clone(), frontmatter: fm, body: body.to_owned() };
        let (op_id, status) = match submit_write(&wiki, op, prov, self.mode, self.name()) {
            Ok(v) => v, Err(r) => return Ok(r),
        };
        Ok(ToolResult::success(json!({
            "op_id": op_id, "status": status, "path": path.as_str(), "title": title,
        }).to_string()))
    }
}

// ── wiki_log_entry ───────────────────────────────────────────────────────────

/// Append a categorized one-line entry to `log.md`. Cheap; safe by
/// design (free-form text into an append-only file).
pub struct WikiLogEntryTool {
    registry: Arc<WikiRegistry>,
    mode: WriteMode,
}

impl WikiLogEntryTool {
    pub fn new(registry: Arc<WikiRegistry>, write_mode: &str) -> Self {
        Self { registry, mode: WriteMode::from_str(write_mode) }
    }
}

#[async_trait]
impl Tool for WikiLogEntryTool {
    fn name(&self) -> &str { "wiki_log_entry" }

    fn description(&self) -> &str {
        "Append a one-line timestamped entry to `log.md` — the wiki's \
         event timeline. Use this for transient signals that don't \
         deserve their own page: 'user paused work on the Pong project', \
         'imported recipe collection from notebook'. Heavier facts that \
         the user will want to come back to should go into a page via \
         `wiki_append_section` or `wiki_write_page` instead."
    }

    fn tier(&self) -> Tier { Tier::Pure }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["kind", "summary"],
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["ingest", "promote", "supersede", "lint", "note"],
                    "description": "Category of event. Use 'note' when unsure."
                },
                "summary": {
                    "type": "string",
                    "description": "One-line description of the event. Aim for under 120 characters."
                },
                "page_refs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of wiki paths the entry references."
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let user_id = match require_user_id(&args, self.name()) {
            Ok(s) => s, Err(r) => return Ok(r),
        };
        let wiki = match open_user_wiki(&self.registry, &user_id, self.name()) {
            Ok(w) => w, Err(r) => return Ok(r),
        };
        let kind_s = args.get("kind").and_then(|v| v.as_str()).unwrap_or("note");
        let kind = match kind_s {
            "ingest"    => LogKind::Ingest,
            "promote"   => LogKind::Promote,
            "supersede" => LogKind::Supersede,
            "lint"      => LogKind::Lint,
            "note"      => LogKind::Note,
            other       => return Ok(ToolResult::failure(format!(
                "wiki_log_entry: unknown kind '{other}' (use ingest|promote|supersede|lint|note)"
            ))),
        };
        let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("").trim();
        if summary.is_empty() {
            return Ok(ToolResult::failure("wiki_log_entry: `summary` is required"));
        }
        let mut refs: Vec<WikiPath> = Vec::new();
        if let Some(arr) = args.get("page_refs").and_then(|v| v.as_array()) {
            for v in arr {
                let s = v.as_str().unwrap_or("").trim();
                if s.is_empty() { continue; }
                match WikiPath::parse(s) {
                    Ok(p) => refs.push(p),
                    Err(e) => return Ok(ToolResult::failure(format!(
                        "wiki_log_entry: invalid page_ref '{s}': {e}"
                    ))),
                }
            }
        }

        let prov = provenance_for_call(&args, "agent");
        let op = WikiOp::LogEntry { kind, summary: summary.to_owned(), page_refs: refs };
        let (op_id, status) = match submit_write(&wiki, op, prov, self.mode, self.name()) {
            Ok(v) => v, Err(r) => return Ok(r),
        };
        Ok(ToolResult::success(json!({
            "op_id": op_id, "status": status, "kind": kind.as_str(), "summary": summary,
        }).to_string()))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::{PageFrontmatter, Provenance, WikiOp, WikiPath, WikiSystem};
    use serde_json::json;
    use tempfile::tempdir;

    /// Build a temp wiki preloaded with a couple of pages so each test
    /// has stable content to query. The registry shares the same
    /// data_dir so tools resolve it on the first call.
    fn seeded_registry() -> (tempfile::TempDir, Arc<WikiRegistry>) {
        let dir = tempdir().unwrap();
        let wiki = WikiSystem::for_user(dir.path(), "u1").unwrap();
        let mut fm_pong = PageFrontmatter::default();
        fm_pong.title = Some("Pong project".into());
        fm_pong.tags  = vec!["project".into(), "game".into()];
        wiki.submit_and_apply(WikiOp::WritePage {
            path: WikiPath::parse("pages/projects/pong.md").unwrap(),
            frontmatter: fm_pong,
            body: "# Pong\n\n## Decisions\n- chose rust\n\n## Open questions\n- ai opponent?\n".into(),
        }, Provenance::user_ui("u1")).unwrap();

        let mut fm_rec = PageFrontmatter::default();
        fm_rec.title = Some("Cooking recipes".into());
        fm_rec.tags  = vec!["recipes".into(), "cooking".into()];
        wiki.submit_and_apply(WikiOp::WritePage {
            path: WikiPath::parse("pages/recipes/index.md").unwrap(),
            frontmatter: fm_rec,
            body: "# Recipes\n\n## Favourites\n- Vegan curry\n".into(),
        }, Provenance::user_ui("u1")).unwrap();

        let reg = Arc::new(WikiRegistry::new(dir.path().to_path_buf()));
        (dir, reg)
    }

    // ── wiki_search ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn search_hits_by_title_and_path() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiSearchTool::new(reg);
        let r = tool.execute(json!({"_user_id": "u1", "query": "pong"})).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        let hits = v["hits"].as_array().unwrap();
        assert!(hits.iter().any(|h| h["path"] == "pages/projects/pong.md"));
        assert!(hits.iter().all(|h| h["score"].as_u64().unwrap() > 0));
    }

    #[tokio::test]
    async fn search_requires_user_id() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiSearchTool::new(reg);
        let r = tool.execute(json!({"query": "pong"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("_user_id"));
    }

    #[tokio::test]
    async fn search_path_prefix_filters() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiSearchTool::new(reg);
        // No query, only prefix.
        let r = tool.execute(json!({
            "_user_id": "u1", "query": "", "path_prefix": "pages/recipes/",
        })).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        let hits = v["hits"].as_array().unwrap();
        assert!(!hits.is_empty());
        for h in hits {
            assert!(h["path"].as_str().unwrap().starts_with("pages/recipes/"));
        }
    }

    #[tokio::test]
    async fn search_empty_query_and_no_prefix_fails() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiSearchTool::new(reg);
        let r = tool.execute(json!({"_user_id": "u1", "query": ""})).await.unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn search_ranks_path_over_body() {
        let (_dir, reg) = seeded_registry();
        // Add a noise page that mentions "pong" only in its body.
        let wiki = reg.for_user("u1").unwrap();
        let mut fm = PageFrontmatter::default();
        fm.title = Some("Noise".into());
        wiki.submit_and_apply(WikiOp::WritePage {
            path: WikiPath::parse("pages/noise.md").unwrap(),
            frontmatter: fm,
            body: "# Misc\nThis page mentions pong once.\n".into(),
        }, Provenance::user_ui("u1")).unwrap();

        let tool = WikiSearchTool::new(reg);
        let r = tool.execute(json!({"_user_id": "u1", "query": "pong"})).await.unwrap();
        let v: Value = serde_json::from_str(&r.output).unwrap();
        let hits = v["hits"].as_array().unwrap();
        // The dedicated pong page (path/title hits) should out-score the noise page.
        assert!(hits[0]["path"] == "pages/projects/pong.md");
    }

    // ── wiki_read ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_returns_frontmatter_and_body() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiReadTool::new(reg);
        let r = tool.execute(json!({
            "_user_id": "u1", "path": "pages/projects/pong.md",
        })).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["title"], "Pong project");
        assert_eq!(v["writer"], "both");
        assert!(v["body"].as_str().unwrap().contains("Decisions"));
        assert_eq!(v["truncated"], false);
    }

    #[tokio::test]
    async fn read_missing_page_returns_failure() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiReadTool::new(reg);
        let r = tool.execute(json!({
            "_user_id": "u1", "path": "pages/does-not-exist.md",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn read_rejects_path_traversal() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiReadTool::new(reg);
        let r = tool.execute(json!({
            "_user_id": "u1", "path": "../../etc/passwd",
        })).await.unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn read_truncates_oversize_body() {
        let (_dir, reg) = seeded_registry();
        let wiki = reg.for_user("u1").unwrap();
        let mut fm = PageFrontmatter::default();
        fm.title = Some("Big".into());
        wiki.submit_and_apply(WikiOp::WritePage {
            path: WikiPath::parse("pages/big.md").unwrap(),
            frontmatter: fm,
            body: "x".repeat(PAGE_PREVIEW_CHARS + 500),
        }, Provenance::user_ui("u1")).unwrap();

        let tool = WikiReadTool::new(reg);
        let r = tool.execute(json!({"_user_id": "u1", "path": "pages/big.md"}))
            .await.unwrap();
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["truncated"], true);
        assert!(v["body"].as_str().unwrap().chars().count() <= PAGE_PREVIEW_CHARS + 1);
    }

    // ── wiki_append_section ──────────────────────────────────────────────────

    #[tokio::test]
    async fn append_in_review_mode_lands_pending() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiAppendSectionTool::new(Arc::clone(&reg), "review");
        let r = tool.execute(json!({
            "_user_id": "u1",
            "_conversation_id": "conv-1",
            "path": "pages/projects/pong.md",
            "section": "Decisions",
            "body": "- chose tokio for async",
        })).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["status"], "pending");
        assert!(v["op_id"].as_str().unwrap().len() > 0);

        // Page on disk unchanged — review queue holds it.
        let wiki = reg.for_user("u1").unwrap();
        let body = std::fs::read_to_string(
            wiki.root().join("pages/projects/pong.md")
        ).unwrap();
        assert!(!body.contains("tokio"));
        let pending = wiki.list_pending_ops().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op.kind(), "append_section");
    }

    #[tokio::test]
    async fn append_in_auto_mode_applies_immediately() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiAppendSectionTool::new(Arc::clone(&reg), "auto");
        let r = tool.execute(json!({
            "_user_id": "u1", "_conversation_id": "conv-1",
            "path": "pages/projects/pong.md",
            "section": "Open questions",
            "body": "- multiplayer?",
        })).await.unwrap();
        assert!(r.success);
        let v: Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["status"], "applied");

        // Page now contains the new content.
        let wiki = reg.for_user("u1").unwrap();
        let body = std::fs::read_to_string(
            wiki.root().join("pages/projects/pong.md")
        ).unwrap();
        assert!(body.contains("multiplayer?"));
    }

    #[tokio::test]
    async fn append_in_off_mode_refuses() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiAppendSectionTool::new(reg, "off");
        let r = tool.execute(json!({
            "_user_id": "u1", "path": "pages/projects/pong.md",
            "section": "X", "body": "y",
        })).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("disabled"));
    }

    #[tokio::test]
    async fn append_requires_section_and_body() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiAppendSectionTool::new(reg, "auto");
        let r = tool.execute(json!({
            "_user_id": "u1", "path": "pages/projects/pong.md", "section": "X", "body": "",
        })).await.unwrap();
        assert!(!r.success);
    }

    // ── wiki_write_page ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_page_creates_new_page_in_auto_mode() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiWritePageTool::new(Arc::clone(&reg), "auto");
        let r = tool.execute(json!({
            "_user_id": "u1", "_conversation_id": "conv-1",
            "path": "pages/projects/new.md",
            "title": "New project",
            "body": "# New\n\nStuff goes here.\n",
            "tags": ["project", "experimental"],
        })).await.unwrap();
        assert!(r.success, "got: {:?}", r.error);

        let wiki = reg.for_user("u1").unwrap();
        let page = wiki.store().read_page(&WikiPath::parse("pages/projects/new.md").unwrap()).unwrap();
        assert_eq!(page.frontmatter.title.as_deref(), Some("New project"));
        // Writer policy is `agent` because the model authored it.
        assert_eq!(page.frontmatter.writer.as_str(), "agent");
        assert!(page.frontmatter.tags.contains(&"project".to_string()));
        assert!(page.body.contains("Stuff goes here"));
    }

    #[tokio::test]
    async fn write_page_refuses_special_files() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiWritePageTool::new(reg, "auto");
        for special in ["profile.md", "index.md", "log.md", "SCHEMA.md"] {
            let r = tool.execute(json!({
                "_user_id": "u1", "path": special, "title": "x", "body": "y",
            })).await.unwrap();
            assert!(!r.success, "expected refusal for {special}");
            assert!(r.error.unwrap().contains("special"));
        }
    }

    #[tokio::test]
    async fn write_page_review_mode_does_not_touch_disk() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiWritePageTool::new(Arc::clone(&reg), "review");
        let r = tool.execute(json!({
            "_user_id": "u1", "path": "pages/pending.md",
            "title": "Pending", "body": "body\n",
        })).await.unwrap();
        assert!(r.success);
        let wiki = reg.for_user("u1").unwrap();
        assert!(!wiki.root().join("pages/pending.md").exists());
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 1);
    }

    // ── wiki_log_entry ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn log_entry_auto_mode_appends_to_log_md() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiLogEntryTool::new(Arc::clone(&reg), "auto");
        let r = tool.execute(json!({
            "_user_id": "u1", "_conversation_id": "conv-1",
            "kind": "note",
            "summary": "User paused work on Pong",
        })).await.unwrap();
        assert!(r.success, "got: {:?}", r.error);

        let wiki = reg.for_user("u1").unwrap();
        let log = std::fs::read_to_string(wiki.root().join("log.md")).unwrap();
        assert!(log.contains("paused work on Pong"));
    }

    #[tokio::test]
    async fn log_entry_rejects_unknown_kind() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiLogEntryTool::new(reg, "auto");
        let r = tool.execute(json!({
            "_user_id": "u1", "kind": "garbage", "summary": "x",
        })).await.unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn log_entry_review_mode_keeps_log_untouched() {
        let (_dir, reg) = seeded_registry();
        let tool = WikiLogEntryTool::new(Arc::clone(&reg), "review");
        let wiki = reg.for_user("u1").unwrap();
        let before = std::fs::read_to_string(wiki.root().join("log.md")).unwrap();

        let r = tool.execute(json!({
            "_user_id": "u1", "kind": "note", "summary": "should be pending",
        })).await.unwrap();
        assert!(r.success);

        // log.md unchanged; one pending op.
        let after = std::fs::read_to_string(wiki.root().join("log.md")).unwrap();
        assert_eq!(before, after);
        assert_eq!(wiki.list_pending_ops().unwrap().len(), 1);
    }
}
