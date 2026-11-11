// SPDX-License-Identifier: AGPL-3.0-or-later

//! `McpMemoryProvider` — serve memory from any MCP memory server through a
//! per-server mapping file (Part B3b).
//!
//! MCP memory servers expose tools under server-specific names (the reference
//! server's `search_nodes`, Mem0's `search_memories`, Graphiti's
//! `search_memory_facts`, …). Rather than hardcode each, MIRA maps the SPI verbs
//! to a server's tools with a mapping file (spec §8). An unmapped server is
//! rejected — no guessing.
//!
//! The actual tool call is abstracted behind [`McpToolCaller`] so the mapping
//! logic (verb → tool, arg substitution, result extraction) is unit- and
//! conformance-testable without a live server; the production caller wraps
//! `crate::mcp::client`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{
    ForgetSelector, MemoryCapabilities, MemoryError, MemoryFact, MemoryHealth, MemoryLocality,
    MemoryProvider, MemoryQuery, MemoryRetrieval, Speaker, Trust, TurnRecord,
};

/// Abstracts "call one tool on the MCP server and get its JSON result" so the
/// adapter can be driven by a real connection or a test mock.
#[async_trait]
pub trait McpToolCaller: Send + Sync {
    async fn call(&self, tool: &str, args: Value) -> Result<Value, MemoryError>;
}

/// How to read facts out of a search tool's response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResultSpec {
    /// Field holding the array of hits (top-level key). `None` = the response is
    /// itself the array.
    pub list_at: Option<String>,
    /// Per-element keys. `content_at` is required for a usable fact.
    pub content_at: Option<String>,
    pub id_at: Option<String>,
    pub score_at: Option<String>,
}

/// One verb's mapping: which tool, what args (with `$placeholders`), and (for
/// search) how to read the result.
#[derive(Debug, Clone, Deserialize)]
pub struct VerbMap {
    pub tool: String,
    #[serde(default)]
    pub args: Map<String, Value>,
    #[serde(default)]
    pub result: Option<ResultSpec>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CapsSpec {
    #[serde(default)] pub read: bool,
    #[serde(default)] pub write: bool,
    #[serde(default)] pub forget: bool,
    #[serde(default)] pub update: bool,
}

/// A parsed mapping file.
#[derive(Debug, Clone, Deserialize)]
pub struct Mapping {
    pub server: String,
    pub verbs: HashMap<String, VerbMap>,
    #[serde(default)]
    pub capabilities: CapsSpec,
}

impl Mapping {
    pub fn from_json(s: &str) -> Result<Self, MemoryError> {
        serde_json::from_str(s).map_err(|e| MemoryError::InvalidRequest(format!("bad mapping file: {e}")))
    }
}

/// Production [`McpToolCaller`] over a connected MCP host client. Wiring a
/// provider to a live server (spawn the server from `command`, hand its
/// `McpClient` here) is the integration step done when an MCP memory server is
/// actually configured; the adapter + mappings above are transport-agnostic and
/// fully exercised via the mock in tests + the conformance suite.
pub struct McpClientCaller {
    client: Arc<crate::mcp::McpClient>,
}

impl McpClientCaller {
    pub fn new(client: Arc<crate::mcp::McpClient>) -> Self { Self { client } }
}

#[async_trait]
impl McpToolCaller for McpClientCaller {
    async fn call(&self, tool: &str, args: Value) -> Result<Value, MemoryError> {
        self.client.call_tool(tool, args).await
            .map_err(|e| MemoryError::Unavailable(e.to_string()))
    }
}

pub struct McpMemoryProvider {
    id: String,
    mapping: Mapping,
    caller: Arc<dyn McpToolCaller>,
}

impl McpMemoryProvider {
    pub fn new(id: impl Into<String>, mapping: Mapping, caller: Arc<dyn McpToolCaller>) -> Self {
        Self { id: id.into(), mapping, caller }
    }

    /// Substitute `$query/$user_id/$k/$content/$id/$agent_id/$thread_id/$namespace`
    /// placeholders in a verb's arg template against the call's bindings.
    fn render_args(template: &Map<String, Value>, bind: &HashMap<&str, Value>) -> Value {
        let mut out = Map::new();
        for (k, v) in template {
            let rendered = match v {
                Value::String(s) if s.starts_with('$') => bind.get(s.as_str()).cloned().unwrap_or(Value::Null),
                other => other.clone(),
            };
            out.insert(k.clone(), rendered);
        }
        Value::Object(out)
    }
}

/// Read a (possibly nested-free, top-level) field off a JSON object.
fn field<'a>(obj: &'a Value, key: &Option<String>) -> Option<&'a Value> {
    key.as_ref().and_then(|k| obj.get(k))
}

#[async_trait]
impl MemoryProvider for McpMemoryProvider {
    fn id(&self) -> &str { &self.id }

    fn capabilities(&self) -> MemoryCapabilities {
        let c = &self.mapping.capabilities;
        MemoryCapabilities {
            read: c.read && self.mapping.verbs.contains_key("search"),
            write: c.write && self.mapping.verbs.contains_key("add"),
            forget: c.forget && self.mapping.verbs.contains_key("forget"),
            update: c.update,
            ..MemoryCapabilities::none()
        }
    }

    // MCP memory servers are local processes (stdio or loopback HTTP); the
    // locality gate on any non-loopback HTTP transport is applied at config
    // construction, not here.
    fn locality(&self) -> MemoryLocality { MemoryLocality::Loopback }

    async fn health(&self) -> MemoryHealth { MemoryHealth::Ready }

    async fn retrieve(&self, q: &MemoryQuery) -> Result<MemoryRetrieval, MemoryError> {
        if q.user_id.trim().is_empty() {
            return Err(MemoryError::InvalidRequest("user_id is required".into()));
        }
        let Some(verb) = self.mapping.verbs.get("search") else {
            return Ok(MemoryRetrieval::Absent { reason: Some("provider has no search mapping".into()) });
        };
        let bind: HashMap<&str, Value> = HashMap::from([
            ("$query", Value::String(q.query.clone())),
            ("$user_id", Value::String(q.user_id.clone())),
            ("$k", Value::Number(q.k.into())),
            ("$namespace", q.namespace.clone().map(Value::String).unwrap_or(Value::Null)),
        ]);
        let args = Self::render_args(&verb.args, &bind);
        let resp = match self.caller.call(&verb.tool, args).await {
            Ok(v) => v,
            // A tool-call failure is surfaced as Unavailable, never empty.
            Err(e) => return Ok(MemoryRetrieval::Unavailable { reason: e.to_string() }),
        };

        let spec = verb.result.clone().unwrap_or_default();
        let items: Vec<Value> = match field(&resp, &spec.list_at).or(Some(&resp)) {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        let mut facts = Vec::new();
        for it in &items {
            let content = field(it, &spec.content_at)
                .or_else(|| it.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if content.is_empty() { continue; }
            facts.push(MemoryFact {
                id: field(it, &spec.id_at).and_then(|v| v.as_str().map(String::from)),
                content,
                score: field(it, &spec.score_at).and_then(|v| v.as_f64()).map(|f| f as f32),
                recorded_at: None,
                valid_at: None,
                source_id: self.id.clone(),
                provenance: None,
                confidence: None,
                speaker: Speaker::External,
                trust: Trust::Unverified,
                complete_set: false,
                provider_meta: Value::Null,
            });
        }
        if facts.is_empty() {
            Ok(MemoryRetrieval::Absent { reason: None })
        } else {
            Ok(MemoryRetrieval::Facts(facts))
        }
    }

    async fn ingest(&self, turn: &TurnRecord) -> Result<usize, MemoryError> {
        let Some(verb) = self.mapping.verbs.get("add") else { return Ok(0) };
        let body = turn.user_msg.trim();
        if body.is_empty() { return Ok(0); }
        let bind: HashMap<&str, Value> = HashMap::from([
            ("$content", Value::String(body.to_string())),
            ("$user_id", Value::String(turn.user_id.clone())),
        ]);
        let args = Self::render_args(&verb.args, &bind);
        self.caller.call(&verb.tool, args).await?;
        Ok(1)
    }

    async fn forget(&self, sel: &ForgetSelector) -> Result<usize, MemoryError> {
        let Some(verb) = self.mapping.verbs.get("forget") else {
            return Err(MemoryError::InvalidRequest("provider has no forget mapping".into()));
        };
        let id = match sel {
            ForgetSelector::Id { item, .. } => item.clone(),
            ForgetSelector::AllForUser(_) =>
                return Err(MemoryError::InvalidRequest("MCP provider: forget-all not supported".into())),
        };
        let bind: HashMap<&str, Value> = HashMap::from([("$id", Value::String(id))]);
        let args = Self::render_args(&verb.args, &bind);
        self.caller.call(&verb.tool, args).await?;
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mock caller: records the last (tool,args) and returns a canned response
    /// per tool name.
    struct Mock {
        last: Mutex<Option<(String, Value)>>,
        responses: HashMap<String, Value>,
        fail: bool,
    }
    #[async_trait]
    impl McpToolCaller for Mock {
        async fn call(&self, tool: &str, args: Value) -> Result<Value, MemoryError> {
            *self.last.lock().unwrap() = Some((tool.to_string(), args));
            if self.fail { return Err(MemoryError::Unavailable("server down".into())); }
            Ok(self.responses.get(tool).cloned().unwrap_or(Value::Null))
        }
    }

    const MEM0: &str = r#"{
        "server": "mem0",
        "verbs": {
            "search": { "tool": "search_memories",
                "args": { "query": "$query", "user_id": "$user_id", "limit": "$k" },
                "result": { "list_at": "results", "content_at": "memory", "id_at": "id", "score_at": "score" } },
            "add":    { "tool": "add_memory", "args": { "data": "$content", "user_id": "$user_id" } },
            "forget": { "tool": "delete_memory", "args": { "memory_id": "$id" } }
        },
        "capabilities": { "read": true, "write": true, "forget": true }
    }"#;

    fn provider(responses: HashMap<String, Value>, fail: bool) -> (McpMemoryProvider, Arc<Mock>) {
        let mock = Arc::new(Mock { last: Mutex::new(None), responses, fail });
        let m = Mapping::from_json(MEM0).unwrap();
        (McpMemoryProvider::new("mcp:mem0", m, mock.clone()), mock)
    }

    #[test]
    fn capabilities_derive_from_mapping() {
        let (p, _) = provider(HashMap::new(), false);
        let c = p.capabilities();
        assert!(c.read && c.write && c.forget);
    }

    #[tokio::test]
    async fn search_substitutes_args_and_extracts_facts() {
        let resp = serde_json::json!({ "results": [
            { "memory": "User likes tea", "id": "m1", "score": 0.9 },
            { "memory": "User owns a bike", "id": "m2", "score": 0.7 }
        ]});
        let (p, mock) = provider(HashMap::from([("search_memories".into(), resp)]), false);
        let got = p.retrieve(&MemoryQuery::new("alice", "beverages", 5)).await.unwrap();
        // args were substituted with the mapped tool arg names
        let (tool, args) = mock.last.lock().unwrap().clone().unwrap();
        assert_eq!(tool, "search_memories");
        assert_eq!(args["query"], "beverages");
        assert_eq!(args["user_id"], "alice");
        assert_eq!(args["limit"], 5);
        match got {
            MemoryRetrieval::Facts(f) => {
                assert_eq!(f.len(), 2);
                assert_eq!(f[0].content, "User likes tea");
                assert_eq!(f[0].id.as_deref(), Some("m1"));
                assert_eq!(f[0].source_id, "mcp:mem0");
                assert_eq!(f[0].speaker, Speaker::External);
            }
            other => panic!("expected facts, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_results_is_absent() {
        let resp = serde_json::json!({ "results": [] });
        let (p, _) = provider(HashMap::from([("search_memories".into(), resp)]), false);
        assert!(matches!(
            p.retrieve(&MemoryQuery::new("alice", "x", 5)).await.unwrap(),
            MemoryRetrieval::Absent { .. }
        ));
    }

    #[tokio::test]
    async fn tool_failure_is_unavailable_not_empty() {
        let (p, _) = provider(HashMap::new(), true);
        assert!(matches!(
            p.retrieve(&MemoryQuery::new("alice", "x", 5)).await.unwrap(),
            MemoryRetrieval::Unavailable { .. }
        ));
    }

    #[tokio::test]
    async fn add_and_forget_substitute_and_call() {
        let (p, mock) = provider(HashMap::new(), false);
        p.ingest(&TurnRecord {
            user_id: "alice".into(), agent_id: None, thread_id: None, channel: "web".into(),
            user_msg: "I love hiking".into(), assistant_msg: String::new(), conversation_id: None,
        }).await.unwrap();
        let (tool, args) = mock.last.lock().unwrap().clone().unwrap();
        assert_eq!(tool, "add_memory");
        assert_eq!(args["data"], "I love hiking");

        p.forget(&ForgetSelector::Id { caller_user_id: "u".into(), item: "m1".into(), caller_username: None, caller_email: None }).await.unwrap();
        let (tool, args) = mock.last.lock().unwrap().clone().unwrap();
        assert_eq!(tool, "delete_memory");
        assert_eq!(args["memory_id"], "m1");
    }

    #[test]
    fn shipped_mapping_files_parse() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/config/memory-mappings");
        for name in ["mem0.json", "reference.json", "graphiti.json"] {
            let s = std::fs::read_to_string(format!("{dir}/{name}"))
                .unwrap_or_else(|e| panic!("read {name}: {e}"));
            let m = Mapping::from_json(&s).unwrap_or_else(|e| panic!("parse {name}: {e}"));
            assert!(m.verbs.contains_key("search"), "{name} must map search");
            assert!(m.capabilities.read, "{name} must advertise read");
        }
    }

    #[tokio::test]
    async fn mcp_provider_passes_read_contract() {
        // No-op caller returns empty → fresh user Absent, etc.
        let resp = serde_json::json!({ "results": [] });
        let (p, _) = provider(HashMap::from([("search_memories".into(), resp)]), false);
        super::super::conformance::run_read_contract(&p).await.expect("mcp read contract");
    }
}
