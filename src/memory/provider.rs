// SPDX-License-Identifier: AGPL-3.0-or-later

//! The pluggable memory-provider seam (Part B).
//!
//! One trait — [`MemoryProvider`] — that the built-in SQLite store, a plain
//! files store, an MCP memory server, or a remote service all implement, so
//! MIRA can switch or combine memory backends with no code change. The shapes
//! here mirror the external contract in `docs/reference/memory-provider-spec.md`
//! (SPI v0.1); keep the two in lockstep.
//!
//! Only the handful of methods the turn hooks need live on the trait. The
//! concrete [`MemorySystem`](crate::memory::MemorySystem) keeps its full surface
//! for its own HTTP handlers, tools and the rollup; [`builtin`] is a thin adapter
//! over it.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod builtin;
pub mod conformance;
pub mod files;
pub mod http;
pub mod mcp;
pub mod set;

/// Who spoke the content a fact was distilled from (ties to the `source_speaker`
/// column, A.1). External providers that don't track it report `External`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker { User, Assistant, System, Rollup, External }

impl Speaker {
    pub fn as_str(&self) -> &'static str {
        match self { Self::User => "user", Self::Assistant => "assistant",
            Self::System => "system", Self::Rollup => "rollup", Self::External => "external" }
    }
    /// Parse a stored `source_speaker` string; unknown values map to `User`
    /// (the column default) rather than erroring.
    pub fn parse(s: &str) -> Self {
        match s { "assistant" => Self::Assistant, "system" => Self::System,
            "rollup" => Self::Rollup, "external" => Self::External, _ => Self::User }
    }
}

/// How much a provider attests a fact — tiers how MIRA presents it (A.7 / spec
/// §5). The three SPI trust levels are `Verified` (the source attests it →
/// "Established"), `Reported` (found/corroborated but not attested) and
/// `Unverified` (present but unconfirmed); the latter two both render as "Found
/// in a source" (attributed, never asserted). `Unknown` is a non-SPI fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust { Verified, Reported, Unverified, Unknown }

impl Default for Trust { fn default() -> Self { Self::Unverified } }

/// Capability flags a provider advertises (spec §5). MIRA never calls a verb a
/// provider didn't advertise, and the two earned claims (`can_assert_complete`,
/// and the per-fact `Verified` trust) are honoured only when declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCapabilities {
    pub read: bool,
    pub write: bool,
    pub forget: bool,
    pub update: bool,
    pub supersede: bool,
    pub ingest_conversation: bool,
    pub temporal: bool,
    pub graph: bool,
    pub citations: bool,
    /// May a result claim it is the COMPLETE set for the query? (A.5 → earned.)
    pub can_assert_complete: bool,
    /// Does the provider expose a `catalog` surface — WHAT IS HELD (counts per
    /// store/tier, a paged subject/record list, one subject's served facts with
    /// tier labels) rather than a read of it? Drives the per-user memory browser
    /// and the `memory_catalog` tool; only an HTTP source advertises it.
    pub catalog: bool,
}

impl MemoryCapabilities {
    /// Nothing advertised — the floor a provider builds up from.
    pub fn none() -> Self {
        Self { read: false, write: false, forget: false, update: false, supersede: false,
            ingest_conversation: false, temporal: false, graph: false, citations: false,
            can_assert_complete: false, catalog: false }
    }
    /// A read-only recall provider (the minimum useful shape).
    pub fn read_only() -> Self { Self { read: true, ..Self::none() } }
}

/// Where a provider lives, for the fail-closed locality gate (spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryLocality { Loopback, Lan, Remote }

/// Liveness/readiness. `Warming` is treated as unavailable-this-turn by MIRA
/// (never a false "no memory") while it keeps polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryHealth {
    Ready,
    Warming { detail: String },
    Unavailable { reason: String },
}

/// Scope keys carried on every data call. `user_id` is required and is the
/// isolation boundary. `group_ids` is a MIRA-internal visibility extension the
/// built-in store uses (external providers ignore it); it is not part of the
/// external SPI's required scope keys.
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub user_id: String,
    /// The account's username + email, alongside `user_id` (the UUID). A provider
    /// can be configured to send one of these three as its identity key (e.g. an
    /// external service keyed on the username rather than MIRA's UUID). `user_id`
    /// remains the isolation boundary; these are purely alternative identity keys.
    pub username: Option<String>,
    pub email: Option<String>,
    pub agent_id: Option<String>,
    pub thread_id: Option<String>,
    pub namespace: Option<String>,
    /// SEARCH only: the model that will consume these facts (spec §3/§5).
    pub consumer_model: Option<String>,
    pub query: String,
    pub k: usize,
    /// MIRA-internal: groups whose memories this user may see (builtin store).
    pub group_ids: Vec<String>,
}

impl MemoryQuery {
    pub fn new(user_id: impl Into<String>, query: impl Into<String>, k: usize) -> Self {
        Self { user_id: user_id.into(), username: None, email: None, agent_id: None,
            thread_id: None, namespace: None,
            consumer_model: None, query: query.into(), k, group_ids: Vec::new() }
    }
}

/// One recalled fact (spec §4). `source_id` is the provider id; `provider_meta`
/// is opaque and passed through untouched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<DateTime<Utc>>,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    pub speaker: Speaker,
    pub trust: Trust,
    /// Honoured only when the provider's `can_assert_complete` is set.
    #[serde(default)]
    pub complete_set: bool,
    #[serde(default)]
    pub provider_meta: serde_json::Value,
}

/// The outcome of a `retrieve` — three distinct statuses (spec §4). `Absent`
/// (healthy, nothing for this query) and `Unavailable` (store down) must never
/// be conflated with each other or with an error.
#[derive(Debug, Clone)]
pub enum MemoryRetrieval {
    Facts(Vec<MemoryFact>),
    Absent { reason: Option<String> },
    /// The source HAS related material but deliberately did not return it for
    /// this query (e.g. a near/ambiguous match: "be more specific"). Distinct
    /// from `Absent` — the model must NOT read it as "you have no such memory";
    /// the reason is surfaced so it can refine or ask.
    Withheld { reason: Option<String> },
    Unavailable { reason: String },
}

/// A completed turn handed to `ingest` (spec: `ingest_turn`).
#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub user_id: String,
    pub agent_id: Option<String>,
    pub thread_id: Option<String>,
    pub channel: String,
    pub user_msg: String,
    pub assistant_msg: String,
    pub conversation_id: Option<String>,
}

/// What to forget.
#[derive(Debug, Clone)]
pub enum ForgetSelector {
    /// A single provider-stable item, with the AUTHENTICATED caller's user id.
    /// Providers MUST authorize against `caller_user_id` and never trust any user
    /// identity embedded in `item` (that would let a caller delete another user's
    /// data by crafting the id — reachable via prompt injection).
    Id {
        caller_user_id: String,
        item: String,
        /// The caller's username/email, when known, so an HTTP provider keyed on
        /// `username`/`email` (its `user_id_field`) can forget on the right
        /// identity instead of the raw UUID. `None` → the service is keyed on id.
        caller_username: Option<String>,
        caller_email: Option<String>,
    },
    /// Everything for a user (the "forget-everything" path).
    AllForUser(String),
}

/// Provider error. Distinct from [`MemoryRetrieval::Unavailable`], which is an
/// expected, surfaced state: an `Err` is an unexpected failure. MIRA treats both
/// as "memory unavailable this turn" at the render layer, never as "no memory".
#[derive(Debug, Clone)]
pub enum MemoryError {
    Unavailable(String),
    InvalidRequest(String),
    Backend(String),
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(s)    => write!(f, "memory unavailable: {s}"),
            Self::InvalidRequest(s) => write!(f, "invalid memory request: {s}"),
            Self::Backend(s)        => write!(f, "memory backend error: {s}"),
        }
    }
}
impl std::error::Error for MemoryError {}
impl From<crate::MiraError> for MemoryError {
    fn from(e: crate::MiraError) -> Self { Self::Backend(e.to_string()) }
}

/// Request for a provider's `catalog` surface — WHAT IS HELD for one user, not a
/// read of it. The identity fields mirror [`MemoryQuery`] so the same
/// username/email/UUID wire selection applies; `prefix` and `contains` are
/// mutually exclusive filters and `subject`, when set, asks for that one
/// subject's served facts (with tier labels). Always **owner-scoped**: a caller
/// only ever sees its own.
#[derive(Debug, Clone)]
pub struct CatalogParams {
    pub user_id: String,
    pub username: Option<String>,
    pub email: Option<String>,
    pub subject: Option<String>,
    pub prefix: Option<String>,
    pub contains: Option<String>,
    pub page: u32,
    pub per_page: u32,
}

impl CatalogParams {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self { user_id: user_id.into(), username: None, email: None, subject: None,
            prefix: None, contains: None, page: 1, per_page: 50 }
    }
}

/// A provider's catalog response. `catalog` is the provider's own payload passed
/// through untouched (MIRA does not model its per-store/per-tier nesting — the
/// browser UI renders it), so a provider can evolve its shape without a MIRA
/// change. `state` is the SPI state (`served`/`absent`/…).
#[derive(Debug, Clone)]
pub struct CatalogResult {
    pub state: String,
    pub catalog: serde_json::Value,
}

/// The pluggable memory backend. See the module docs + the SPI spec.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Stable provider id: `"builtin"` | `"files"` | `"mcp:…"` | `"ext-http"`.
    fn id(&self) -> &str;
    fn capabilities(&self) -> MemoryCapabilities;
    fn locality(&self) -> MemoryLocality;
    async fn health(&self) -> MemoryHealth;
    async fn retrieve(&self, q: &MemoryQuery) -> Result<MemoryRetrieval, MemoryError>;
    /// No-op (returns 0) for a read-only provider; MIRA won't call it when
    /// `capabilities().write` is false.
    async fn ingest(&self, turn: &TurnRecord) -> Result<usize, MemoryError>;
    async fn forget(&self, sel: &ForgetSelector) -> Result<usize, MemoryError>;
    /// Enumerate WHAT IS HELD for the asking user (owner-scoped). Default: not
    /// supported — only a provider advertising `capabilities().catalog`
    /// implements it. MIRA won't call it when `capabilities().catalog` is false.
    async fn catalog(&self, _p: &CatalogParams) -> Result<CatalogResult, MemoryError> {
        Err(MemoryError::InvalidRequest("catalog not supported".into()))
    }

    /// One-shot capability discovery, run once at boot. A provider that can
    /// declare its own capabilities (e.g. an HTTP service's `GET /capabilities`)
    /// overrides this to fetch and cache them, so [`Self::capabilities`] reflects
    /// what the service actually supports instead of an optimistic default.
    /// Default: no-op (the static capabilities are already accurate).
    async fn probe(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_builders() {
        assert!(MemoryCapabilities::read_only().read);
        assert!(!MemoryCapabilities::read_only().write);
        assert!(!MemoryCapabilities::none().read);
    }

    #[test]
    fn speaker_round_trips_with_user_default() {
        for s in ["user", "assistant", "system", "rollup", "external"] {
            assert_eq!(Speaker::parse(s).as_str(), s);
        }
        assert_eq!(Speaker::parse("nonsense"), Speaker::User);
    }

    #[test]
    fn trust_defaults_unverified() {
        assert_eq!(Trust::default(), Trust::Unverified);
    }
}
