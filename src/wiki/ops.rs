// SPDX-License-Identifier: AGPL-3.0-or-later

// src/wiki/ops.rs
//! Mutation operations on the wiki.
//!
//! Every change — whether from a user click, an agent tool call, or the
//! post-turn extractor — is expressed as a [`WikiOp`] inside a
//! [`WikiOpEnvelope`]. The envelope carries an op id, scope, status,
//! and provenance so each change is auditable and reversible.
//!
//! The applier (`applier.rs`) is the only thing that actually touches
//! the filesystem; everywhere else builds and submits ops.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::wiki::frontmatter::PageFrontmatter;
use crate::wiki::paths::WikiPath;

/// Which wiki this op targets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum WikiScope {
    /// Per-user wiki scoped by user id.
    User(String),
    /// The shared system wiki (admin-only writes).
    System,
}

impl WikiScope {
    pub fn as_str(&self) -> &str {
        match self {
            WikiScope::User(_) => "user",
            WikiScope::System => "system",
        }
    }

    pub fn user_id(&self) -> Option<&str> {
        match self {
            WikiScope::User(id) => Some(id),
            WikiScope::System => None,
        }
    }
}

/// Categorisation for `log.md` entries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogKind {
    Ingest,
    Promote,
    Supersede,
    Lint,
    Note,
}

impl LogKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogKind::Ingest    => "ingest",
            LogKind::Promote   => "promote",
            LogKind::Supersede => "supersede",
            LogKind::Lint      => "lint",
            LogKind::Note      => "note",
        }
    }
}

/// Where this op came from. Stored in the audit row so we can answer
/// "why does the wiki know X?".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    /// `turn`, `user_ui`, `tool`, `import`, `migration`, etc.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// The acting principal — user id for UI writes, agent name for
    /// agent tool calls, "extractor" for auto-extraction.
    pub actor: String,
}

impl Provenance {
    pub fn user_ui(user_id: &str) -> Self {
        Self {
            source: "user_ui".into(),
            turn_id: None,
            conversation_id: None,
            actor: user_id.to_string(),
        }
    }

    pub fn from_turn(actor: &str, turn_id: &str, conversation_id: &str) -> Self {
        Self {
            source: "turn".into(),
            turn_id: Some(turn_id.to_string()),
            conversation_id: Some(conversation_id.to_string()),
            actor: actor.to_string(),
        }
    }

    pub fn migration() -> Self {
        Self {
            source: "migration".into(),
            turn_id: None,
            conversation_id: None,
            actor: "system".into(),
        }
    }
}

/// Lifecycle state for an op in the audit table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpStatus {
    /// Submitted but not yet applied; awaits review or auto-apply.
    Pending,
    /// Successfully applied to the filesystem.
    Applied,
    /// Rejected by a reviewer; never applied.
    Rejected,
    /// Apply attempted and failed; see `failure`.
    Failed,
}

impl OpStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            OpStatus::Pending  => "pending",
            OpStatus::Applied  => "applied",
            OpStatus::Rejected => "rejected",
            OpStatus::Failed   => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending"  => Some(OpStatus::Pending),
            "applied"  => Some(OpStatus::Applied),
            "rejected" => Some(OpStatus::Rejected),
            "failed"   => Some(OpStatus::Failed),
            _ => None,
        }
    }
}

/// The actual mutation. Untagged inputs from the agent tool layer go
/// through `WikiOp::parse_loose` if they don't carry the discriminator;
/// internal callers build these variants directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WikiOp {
    /// Create or replace an entire page.
    WritePage {
        path: WikiPath,
        frontmatter: PageFrontmatter,
        body: String,
    },
    /// Replace a single `## Heading` section.
    UpdateSection {
        path: WikiPath,
        section: String,
        body: String,
    },
    /// Append text under a `## Heading`, preserving the prior content.
    AppendSection {
        path: WikiPath,
        section: String,
        body: String,
    },
    /// Append a timestamped entry to `log.md`.
    LogEntry {
        kind: LogKind,
        summary: String,
        #[serde(default)]
        page_refs: Vec<WikiPath>,
    },
    /// Mark a page as superseded by setting `valid_to` (and optionally
    /// pointing to a replacement page).
    Supersede {
        path: WikiPath,
        reason: String,
        #[serde(default)]
        replacement: Option<WikiPath>,
    },
    /// Promote an atomic memory fact into a wiki page section.
    PromoteFact {
        memory_id: String,
        target: WikiPath,
        section: String,
    },
    /// Archive a page (writer policy enforced).
    DeletePage { path: WikiPath },
}

impl WikiOp {
    /// One-word kind label, used in the audit table for filtering.
    pub fn kind(&self) -> &'static str {
        match self {
            WikiOp::WritePage     {..} => "write_page",
            WikiOp::UpdateSection {..} => "update_section",
            WikiOp::AppendSection {..} => "append_section",
            WikiOp::LogEntry      {..} => "log_entry",
            WikiOp::Supersede     {..} => "supersede",
            WikiOp::PromoteFact   {..} => "promote_fact",
            WikiOp::DeletePage    {..} => "delete_page",
        }
    }

    /// The wiki path this op affects (for audit indexing). `LogEntry`
    /// returns `log.md`.
    pub fn target_path(&self) -> WikiPath {
        match self {
            WikiOp::WritePage     { path, .. }    => path.clone(),
            WikiOp::UpdateSection { path, .. }    => path.clone(),
            WikiOp::AppendSection { path, .. }    => path.clone(),
            WikiOp::LogEntry      { .. }          => WikiPath::from_trusted("log.md".into()),
            WikiOp::Supersede     { path, .. }    => path.clone(),
            WikiOp::PromoteFact   { target, .. }  => target.clone(),
            WikiOp::DeletePage    { path }        => path.clone(),
        }
    }
}

/// The wrapped form actually stored in the audit table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiOpEnvelope {
    pub op_id: String,
    pub scope: WikiScope,
    pub op: WikiOp,
    pub status: OpStatus,
    pub provenance: Provenance,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Extractor confidence [0.0, 1.0] for ops proposed by the post-turn
    /// extractor. `None` for direct UI/tool writes (no model judgement).
    /// Drives confidence-tiered auto-apply and the "approve all ≥ X"
    /// bulk review action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

impl WikiOpEnvelope {
    pub fn new(scope: WikiScope, op: WikiOp, provenance: Provenance) -> Self {
        Self {
            op_id: Uuid::now_v7().to_string(),
            scope,
            op,
            status: OpStatus::Pending,
            provenance,
            created_at: Utc::now(),
            applied_at: None,
            reviewed_at: None,
            reviewed_by: None,
            failure: None,
            confidence: None,
        }
    }

    /// Builder: attach the extractor's confidence to a freshly-built envelope.
    pub fn with_confidence(mut self, confidence: Option<f32>) -> Self {
        self.confidence = confidence;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_starts_pending() {
        let op = WikiOp::LogEntry {
            kind: LogKind::Note,
            summary: "hello".into(),
            page_refs: vec![],
        };
        let env = WikiOpEnvelope::new(WikiScope::User("u1".into()), op, Provenance::user_ui("u1"));
        assert_eq!(env.status, OpStatus::Pending);
        assert!(env.applied_at.is_none());
        assert_eq!(env.op.kind(), "log_entry");
    }

    #[test]
    fn target_path_for_log_is_log_md() {
        let op = WikiOp::LogEntry { kind: LogKind::Note, summary: "x".into(), page_refs: vec![] };
        assert_eq!(op.target_path().as_str(), "log.md");
    }

    #[test]
    fn op_round_trips_json() {
        let op = WikiOp::WritePage {
            path: WikiPath::parse("pages/foo.md").unwrap(),
            frontmatter: PageFrontmatter::default(),
            body: "# Foo\n".into(),
        };
        let json = serde_json::to_string(&op).unwrap();
        let back: WikiOp = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind(), "write_page");
    }
}
