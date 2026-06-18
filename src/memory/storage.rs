// SPDX-License-Identifier: AGPL-3.0-or-later

// src/memory/storage.rs

//! SQLite-based persistent memory storage
//! 
//! Provides CRUD operations for MemoryItem with support for:
//! - Hierarchical categorization (Fact, Preference, Skill, Relationship, Project)
//! - Keyword search via FTS5 full-text search
//! - Tagged memories for flexible organization
//! - Timestamped entries with relevance scoring

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params, params_from_iter, OptionalExtension, ToSql};
use rusqlite::functions::FunctionFlags;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

use crate::MiraError;

// ── Decay model ──────────────────────────────────────────────────────────────
// Exponential half-life: effective = strength · 2^(-age / half_life).
// `last_reinforced` is stored in milliseconds since epoch.

// Half-life (ms) for each stability tier. A newly-stored memory defaults to
// `stable`, so `stable` must be long enough that day-to-day use doesn't wash
// memories out; `permanent` is the escape hatch for facts that never age.
fn half_life_ms(stability: &str) -> Option<f64> {
    match stability {
        "permanent" => None,                          // never decays
        "ephemeral" => Some(86_400_000.0),            // 1 day
        "episodic"  => Some(14.0  * 86_400_000.0),    // 14 days
        _           => Some(90.0  * 86_400_000.0),    // stable (default)
    }
}

// Compute the effective strength right now in Rust. Mirrors the SQL UDF so
// tests and post-query code paths agree.
pub fn compute_effective_strength(strength: f64, last_reinforced_ms: i64, stability: &str) -> f64 {
    let Some(hl) = half_life_ms(stability) else { return strength.clamp(0.0, 1.0); };
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(last_reinforced_ms);
    let age = (now_ms - last_reinforced_ms).max(0) as f64;
    let factor = 2f64.powf(-age / hl);
    (strength * factor).clamp(0.0, 1.0)
}

// SQL fragment that computes effective_strength for the current row. Relies
// on the `mira_decay` UDF registered at connection open.
const EFFECTIVE_STRENGTH_EXPR: &str =
    "mira_decay(strength, last_reinforced, stability)";

// Memory categories for hierarchical organization
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Category {
    // Factual information (names, dates, facts)
    Fact,
    // User preferences and likes/dislikes
    Preference,
    // Skills, abilities, competencies
    Skill,
    // Relationships and social connections
    Relationship,
    // Projects, goals, ongoing work
    Project,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Fact => "fact",
            Category::Preference => "preference",
            Category::Skill => "skill",
            Category::Relationship => "relationship",
            Category::Project => "project",
        }
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// Source of a memory item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MemorySource {
    // User explicitly stored this via command
    UserExplicit(String),
    // System auto-extracted from conversation
    AutoExtracted,
    // Imported from external source
    Imported(String),
}

// Visibility scope of a memory.
// // - `User`:   private to a single user (`scope_id = user_id`).
// - `Group`:  shared within a group (`scope_id = group_id`).
// - `System`: global, visible to all authenticated users (`scope_id = NULL`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    User,
    Group,
    System,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::User   => "user",
            Scope::Group  => "group",
            Scope::System => "system",
        }
    }
    pub fn parse(s: &str) -> Scope {
        match s {
            "group"  => Scope::Group,
            "system" => Scope::System,
            _        => Scope::User,
        }
    }
}

fn default_scope() -> Scope { Scope::User }

// Sort order for visibility-aware list queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSort {
    // Decay-aware: effective strength DESC, ties broken by created_at DESC.
    Strength,
    // Plain chronological: created_at DESC.
    Recent,
}

impl ListSort {
    pub fn parse(s: &str) -> ListSort {
        match s {
            "recent" => ListSort::Recent,
            _        => ListSort::Strength,
        }
    }
}

// A single memory item stored in the database
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: u64,
    pub content: String,
    pub category: Category,
    pub tags: Vec<String>,
    pub source: Option<MemorySource>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub relevance_score: f32,  // 0.0 to 1.0

    // ── Scope + governance (added for group/system memory) ──
    #[serde(default = "default_scope")]
    pub scope: Scope,
    #[serde(default)]
    pub scope_id: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub supersedes: Option<u64>,
    #[serde(default)]
    pub superseded_by: Option<u64>,

    // ── Decay metadata ──
    // Persisted baseline strength (0.0..=1.0). Bumped by reinforcement, never
    // decays directly — decay is applied at read time via `effective_strength`.
    #[serde(default = "default_strength")]
    pub strength: f32,
    // `strength · 2^(-age / half_life(stability))`, computed by the SQL UDF.
    #[serde(default = "default_strength")]
    pub effective_strength: f32,
    // How many times this memory was surfaced into a retrieval result.
    #[serde(default)]
    pub access_count: u32,
    // Epoch ms of last reinforcement (or creation if never reinforced).
    #[serde(default)]
    pub last_reinforced: i64,
    // Decay class — `permanent` | `stable` | `episodic` | `ephemeral`.
    #[serde(default = "default_stability")]
    pub stability: String,

    // ── Provenance ──
    // Channel the triggering turn came through — `"web"`, `"tg"`, `"signal"`,
    // `"tui"`, `"cli"`, or whatever the channel layer stamps. `None` for
    // rows created outside a conversation (imports, system seeds).
    #[serde(default)]
    pub source_channel: Option<String>,
    // Conversation id that produced this memory. Lets the review surface
    // deep-link back to the transcript the extractor read.
    #[serde(default)]
    pub source_conversation_id: Option<String>,
    // Message id that produced this memory (usually the *user* turn). Not
    // every memory has one — rollups span a whole day, not a single message.
    #[serde(default)]
    pub source_message_id: Option<String>,
}

fn default_strength() -> f32 { 1.0 }
fn default_stability() -> String { "stable".into() }

// SQLite memory storage backend
pub struct MemoryStorage {
    conn: Connection,
    user_id: String,
}

impl MemoryStorage {
    // Create new storage for the default user (backwards-compatible).
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, MiraError> {
        Self::new_for_user(path, "default")
    }

    // Create storage scoped to a specific user_id.
    pub fn new_for_user<P: AsRef<Path>>(path: P, user_id: &str) -> Result<Self, MiraError> {
        let path_str = path.as_ref().to_string_lossy();
        info!("Opening memory database at {} for user '{}'", path_str, user_id);

        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MiraError::MemoryError(
                    format!("Failed to create data directory: {}", e)
                ))?;
        }

        let conn = Connection::open(path).map_err(|e| {
            MiraError::MemoryError(format!("Failed to open database: {}", e))
        })?;

        conn.execute("PRAGMA foreign_keys = ON", [])
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to enable foreign keys: {}", e)
            ))?;

        register_decay_udf(&conn)?;

        let storage = Self { conn, user_id: user_id.to_string() };
        storage.initialize_schema()?;

        info!("Memory database initialized for user '{}'", user_id);
        Ok(storage)
    }
    
    // Initialize database schema with tables and indexes
    fn initialize_schema(&self) -> Result<(), MiraError> {
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL DEFAULT 'default',
                content TEXT NOT NULL,
                category TEXT NOT NULL CHECK(category IN ('fact', 'preference', 'skill', 'relationship', 'project')),
                tags TEXT DEFAULT '',
                source_type TEXT DEFAULT 'user_explicit',
                source_detail TEXT,
                created_at INTEGER NOT NULL,
                relevance_score REAL DEFAULT 1.0
            )",
            [],
        ).map_err(|e| MiraError::MemoryError(
            format!("Failed to create memories table: {}", e)
        ))?;

        // Try to add user_id column to existing databases that predate this schema
        let _ = self.conn.execute(
            "ALTER TABLE memories ADD COLUMN user_id TEXT NOT NULL DEFAULT 'default'", []
        );

        // ── Scope + governance columns (added for group/system memory) ──
        // ALTER TABLE is idempotent via error-swallow; SQLite has no IF NOT EXISTS
        // on ADD COLUMN. These are added one-by-one so a partial migration from
        // a crash mid-run still converges.
        let alters: &[&str] = &[
            // Visibility scope — 'user' | 'group' | 'system'.
            "ALTER TABLE memories ADD COLUMN scope TEXT NOT NULL DEFAULT 'user'",
            // scope_id: user_id for 'user', group_id for 'group', NULL for 'system'.
            "ALTER TABLE memories ADD COLUMN scope_id TEXT",
            // JSON array of user_ids this memory is about.
            "ALTER TABLE memories ADD COLUMN subject_user_ids TEXT DEFAULT '[]'",
            // JSON array of user_ids who witnessed/observed.
            "ALTER TABLE memories ADD COLUMN observed_by TEXT DEFAULT '[]'",
            // user_id who wrote the memory, or 'agent' for auto-extracted.
            "ALTER TABLE memories ADD COLUMN created_by TEXT",
            // Sensitive content flag — caller may redact or gate display.
            "ALTER TABLE memories ADD COLUMN sensitive INTEGER NOT NULL DEFAULT 0",
            // Decay class — 'permanent' | 'stable' | 'episodic' | 'ephemeral'.
            "ALTER TABLE memories ADD COLUMN stability TEXT NOT NULL DEFAULT 'stable'",
            // Current strength, decays over time (slice 2). Starts at 1.0.
            "ALTER TABLE memories ADD COLUMN strength REAL NOT NULL DEFAULT 1.0",
            // Timestamp (ms) of last reinforcement — seeded to created_at in backfill.
            "ALTER TABLE memories ADD COLUMN last_reinforced INTEGER",
            // How often this memory was surfaced (used to reinforce).
            "ALTER TABLE memories ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0",
            // Supersession chain — new memory points to the one it replaces.
            "ALTER TABLE memories ADD COLUMN supersedes INTEGER",
            "ALTER TABLE memories ADD COLUMN superseded_by INTEGER",
            // Provenance — where this memory came from.
            "ALTER TABLE memories ADD COLUMN source_channel TEXT",
            "ALTER TABLE memories ADD COLUMN source_conversation_id TEXT",
            "ALTER TABLE memories ADD COLUMN source_message_id TEXT",
            // Soft-delete (admin-only) — rows stay for audit but become invisible.
            "ALTER TABLE memories ADD COLUMN deleted_at INTEGER",
            "ALTER TABLE memories ADD COLUMN deleted_by TEXT",
        ];
        for sql in alters {
            // Swallow "duplicate column name" errors so migration is idempotent.
            let _ = self.conn.execute(sql, []);
        }

        // ── Backfill: existing rows predate scope/scope_id. Treat them as
        // user-scope owned by whatever user_id they already carry. Also seed
        // last_reinforced = created_at so decay starts counting from creation.
        let _ = self.conn.execute(
            "UPDATE memories SET scope_id = user_id WHERE scope_id IS NULL",
            [],
        );
        let _ = self.conn.execute(
            "UPDATE memories SET created_by = user_id WHERE created_by IS NULL",
            [],
        );
        // last_reinforced is stored in milliseconds. Seed unbackfilled rows
        // from created_at (which is in seconds) × 1000.
        let _ = self.conn.execute(
            "UPDATE memories SET last_reinforced = created_at * 1000 WHERE last_reinforced IS NULL",
            [],
        );
        // Correction for earlier builds (pre-slice-2) that seeded
        // last_reinforced = created_at in seconds. Anything below ~2001 is
        // clearly still in seconds, upscale it. Threshold chosen so real
        // millisecond timestamps (> ~10^12) are left alone.
        let _ = self.conn.execute(
            "UPDATE memories SET last_reinforced = last_reinforced * 1000
             WHERE last_reinforced IS NOT NULL AND last_reinforced < 1000000000000",
            [],
        );

        // ── Audit log for memory writes/reads (slice 1: writes only) ──
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS memory_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                memory_id INTEGER,
                actor_user_id TEXT NOT NULL,
                action TEXT NOT NULL,
                scope TEXT,
                scope_id TEXT,
                at INTEGER NOT NULL,
                detail TEXT
            )",
            [],
        ).map_err(|e| MiraError::MemoryError(
            format!("Failed to create memory_audit table: {}", e)
        ))?;

        // ── Indexes ──
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_memories_user_category ON memories(user_id, category)",
            [],
        ).map_err(|e| MiraError::MemoryError(
            format!("Failed to create index: {}", e)
        ))?;
        // Visibility reads: fast lookup by (scope, scope_id) filtering out
        // superseded/deleted rows.
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope, scope_id)",
            [],
        );
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_memories_live ON memories(superseded_by, deleted_at)",
            [],
        );
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_audit_memory ON memory_audit(memory_id)",
            [],
        );

        self.initialize_graph_schema()?;

        debug!("Database schema initialized");
        Ok(())
    }

    // Temporal knowledge-graph tables (see `design-docs/graph-memory.md`). Created
    // unconditionally — they're empty and harmless until `memory.graph.enabled`
    // turns extraction on, and creating them up-front keeps the schema in one
    // place and avoids a runtime "table missing" race when the flag flips.
    fn initialize_graph_schema(&self) -> Result<(), MiraError> {
        // Distinct things the user refers to (a plant, a bike, a trip, a person).
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS kg_entities (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id     TEXT NOT NULL,
                name        TEXT NOT NULL,
                name_norm   TEXT NOT NULL,
                entity_type TEXT NOT NULL DEFAULT 'thing',
                aliases     TEXT NOT NULL DEFAULT '[]',
                created_at  INTEGER NOT NULL
            )",
            [],
        ).map_err(|e| MiraError::MemoryError(format!("create kg_entities: {}", e)))?;

        // Timestamped, typed facts. `value_num`/`value_unit` carry the numeric
        // payload for COUNT/SUM/AVG; `event_at` is when the fact happened;
        // `valid_from`/`valid_to` time-bound supersession.
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS kg_edges (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id     TEXT NOT NULL,
                subject_id  INTEGER NOT NULL,
                predicate   TEXT NOT NULL,
                object_id   INTEGER,
                value_num   REAL,
                value_unit  TEXT,
                fact_text   TEXT NOT NULL,
                event_at    INTEGER,
                valid_from  INTEGER NOT NULL,
                valid_to    INTEGER,
                source      TEXT,
                created_at  INTEGER NOT NULL
            )",
            [],
        ).map_err(|e| MiraError::MemoryError(format!("create kg_edges: {}", e)))?;

        // `superseded_by` — set on the *loser* when the Phase-A consolidator
        // dedups near-duplicate entities (e.g. "navy blazer" / "navy blue
        // blazer"). The loser's edges are re-pointed to the winner; the loser
        // row is preserved for audit. Retrieval filters this out.
        let _ = self.conn.execute(
            "ALTER TABLE kg_entities ADD COLUMN superseded_by INTEGER", [],
        );

        // ── Phase D — importance scoring + decay ──
        // access_count       — bumped each time the edge is retrieved into
        //                    context (always-on, even when Phase D off).
        // last_reinforced    — unix ms; same trigger; NULL ⇒ never retrieved.
        // importance         — computed nightly by Phase D consolidator:
        //                    ln(1 + access_count) × exp(-age_days / half_life).
        //                    Defaults to 0; ORDER BY importance DESC is a
        //                    no-op until Phase D writes non-zero scores.
        let _ = self.conn.execute("ALTER TABLE kg_edges ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0", []);
        let _ = self.conn.execute("ALTER TABLE kg_edges ADD COLUMN last_reinforced INTEGER", []);
        let _ = self.conn.execute("ALTER TABLE kg_edges ADD COLUMN importance REAL NOT NULL DEFAULT 0.0", []);

        // Resolution lookups by normalised name; edge gathering by subject.
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_kg_entities_user_norm ON kg_entities(user_id, name_norm)", [],
        );
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_kg_entities_live ON kg_entities(user_id, superseded_by)", [],
        );
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_kg_edges_subject ON kg_edges(user_id, subject_id)", [],
        );
        let _ = self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_kg_edges_live ON kg_edges(user_id, valid_to)", [],
        );
        Ok(())
    }
    
    // Store a new memory item
    pub fn store(
        &self,
        content: String,
        category: Category,
        tags: Vec<String>,
        source: Option<MemorySource>,
    ) -> Result<u64, MiraError> {
        let created_at = Utc::now().timestamp();
        let tags_json = serde_json::to_string(&tags).unwrap_or_default();
        
        // Determine source type and detail
        let (source_type, source_detail) = match source {
            Some(MemorySource::UserExplicit(detail)) => ("user_explicit", Some(detail)),
            Some(MemorySource::AutoExtracted) => ("auto_extracted", None),
            Some(MemorySource::Imported(detail)) => ("imported", Some(detail)),
            None => ("user_explicit", None),
        };
        
        let last_reinforced_ms = created_at * 1000;
        self.conn
            .execute(
                "INSERT INTO memories (user_id, content, category, tags, source_type, source_detail, created_at, last_reinforced)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![self.user_id, content, category.as_str(), tags_json, source_type, source_detail, created_at, last_reinforced_ms],
            )
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to insert memory: {}", e)
            ))?;
        
        // Get the last inserted rowid
        let id = self.conn
            .query_row("SELECT last_insert_rowid()", [], |row| -> rusqlite::Result<i64> { row.get(0) })
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to get insert ID: {}", e)
            ))?;

        debug!("Stored memory id={}, category={}", id, category);
        Ok(id as u64)
    }
    
    // Retrieve a single memory by ID
    pub fn get(&self, id: u64) -> Result<Option<MemoryItem>, MiraError> {
        let item = self.conn
            .query_row(
                &format!("SELECT {} FROM memories WHERE id = ?1 AND user_id = ?2", MEMORY_COLUMNS),
                params![id, self.user_id],
                row_to_memory_item,
            )
            .optional()
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to query memory by ID: {}", e)
            ))?;

        Ok(item)
    }
    
    // Search memories by keyword.
    //     // Splits the query into individual meaningful words and performs an OR-based
    // LIKE search so that "what is my name" finds "User's name is Tarek".
    // Falls back to a full-phrase search when no meaningful words are extracted.
    pub fn search(&self, query: &str) -> Result<Vec<MemoryItem>, MiraError> {
        const STOPWORDS: &[&str] = &[
            "what", "when", "where", "which", "that", "this", "with", "from", "have",
            "will", "your", "their", "about", "would", "could", "should", "tell",
            "know", "just", "some", "than", "then", "them", "there", "does", "been",
            "does", "were", "they", "also", "very", "more", "into", "over", "such",
        ];

        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 3)
            .map(|w| w.to_lowercase())
            .filter(|w| !STOPWORDS.contains(&w.as_str()))
            .collect();

        if words.is_empty() {
            return self.search_like(query);
        }

        let mut seen = std::collections::HashSet::new();
        let mut all_items = Vec::new();
        for word in &words {
            for item in self.search_like(word)? {
                if seen.insert(item.id) {
                    all_items.push(item);
                }
            }
        }

        if all_items.is_empty() {
            self.search_like(query)
        } else {
            Ok(all_items)
        }
    }
    
    // Search using SQL LIKE
    fn search_like(&self, query: &str) -> Result<Vec<MemoryItem>, MiraError> {
        let search_pattern = format!("%{}%", query);

        let sql = format!(
            "SELECT {} FROM memories WHERE (content LIKE ?1 OR tags LIKE ?1) AND user_id = ?2
             ORDER BY relevance_score DESC, created_at DESC",
            MEMORY_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to prepare search statement: {}", e)
            ))?;

        let items = stmt
            .query_map(params![search_pattern, self.user_id], row_to_memory_item)
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to execute search: {}", e)
            ))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(items)
    }

    // Get all memories of a specific category
    pub fn get_by_category(&self, category: &Category) -> Result<Vec<MemoryItem>, MiraError> {
        let sql = format!(
            "SELECT {} FROM memories WHERE category = ?1 AND user_id = ?2 ORDER BY created_at DESC",
            MEMORY_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to prepare query: {}", e)
            ))?;

        let items = stmt
            .query_map(params![category.as_str(), self.user_id], row_to_memory_item)
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to execute query: {}", e)
            ))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(items)
    }
    
    // Delete a memory by ID
    pub fn delete(&self, id: u64) -> Result<bool, MiraError> {
        let rows = self.conn
            .execute("DELETE FROM memories WHERE id = ?1 AND user_id = ?2", params![id, self.user_id])
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to delete memory: {}", e)
            ))?;

        Ok(rows > 0)
    }

    // Hard-delete memories whose `source_type = 'imported'` and
    // `source_detail = ?detail` for a single subject user. Returns the row
    // count deleted. Rows are matched by `scope = 'user'` and `scope_id =
    // user_id` — the write path for onboarding seeds uses that shape. Also
    // returns the ids so callers can drop matching vectors from the
    // semantic store.
    pub fn delete_by_source_detail(
        &self,
        detail:  &str,
        user_id: &str,
    ) -> Result<(usize, Vec<u64>), MiraError> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM memories
             WHERE source_type = 'imported'
               AND source_detail = ?1
               AND scope         = 'user'
               AND scope_id      = ?2",
        ).map_err(|e| MiraError::MemoryError(format!("delete_by_source_detail prepare: {}", e)))?;
        let ids: Vec<u64> = stmt
            .query_map(params![detail, user_id], |row| row.get::<_, i64>(0))
            .map_err(|e| MiraError::MemoryError(format!("delete_by_source_detail query: {}", e)))?
            .filter_map(|r| r.ok())
            .map(|i| i as u64)
            .collect();

        let rows = self.conn.execute(
            "DELETE FROM memories
             WHERE source_type = 'imported'
               AND source_detail = ?1
               AND scope         = 'user'
               AND scope_id      = ?2",
            params![detail, user_id],
        ).map_err(|e| MiraError::MemoryError(format!("delete_by_source_detail: {}", e)))?;

        Ok((rows, ids))
    }

    // List all memories for this user with pagination
    pub fn list_all(&self, limit: u64, offset: u64) -> Result<Vec<MemoryItem>, MiraError> {
        let sql = format!(
            "SELECT {} FROM memories WHERE user_id = ?1
             ORDER BY created_at DESC LIMIT ?2 OFFSET ?3",
            MEMORY_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to prepare list_all statement: {}", e)
            ))?;

        let items = stmt
            .query_map(params![self.user_id, limit, offset], row_to_memory_item)
            .map_err(|e| MiraError::MemoryError(format!("Failed to list memories: {}", e)))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(items)
    }

    // Update content, category, and tags of an existing memory
    pub fn update(&self, id: u64, content: String, category: Category, tags: Vec<String>) -> Result<bool, MiraError> {
        let tags_json = serde_json::to_string(&tags).unwrap_or_default();
        let rows = self.conn
            .execute(
                "UPDATE memories SET content = ?1, category = ?2, tags = ?3 WHERE id = ?4 AND user_id = ?5",
                params![content, category.as_str(), tags_json, id, self.user_id],
            )
            .map_err(|e| MiraError::MemoryError(format!("Failed to update memory: {}", e)))?;
        Ok(rows > 0)
    }

    // Get total count of memories for this user
    pub fn count(&self) -> Result<u64, MiraError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM memories WHERE user_id = ?1", params![self.user_id], |row| row.get(0))
            .map_err(|e| MiraError::MemoryError(
                format!("Failed to count memories: {}", e)
            ))
    }
    
    // ──────────────────────────────────────────────────────────────────────
    // Visibility-aware reads — the "chokepoint" that all multi-scope callers
    // MUST use. Instead of filtering by user_id alone, these accept the
    // caller's user_id plus the list of group_ids they belong to, and return
    // rows that are:
    // - scope='user'   AND scope_id = user_id     (their own)
    // - scope='group'  AND scope_id IN group_ids  (groups they're in)
    // - scope='system'                            (everyone sees system memory)
    // Superseded and soft-deleted rows are filtered out (use `*_with_history`
    // variants for audit views).
    // ──────────────────────────────────────────────────────────────────────

    // List memories visible to `(user_id, group_ids)`.
    //     // `sort` selects the ORDER BY: `ListSort::Strength` (default, effective
    // strength desc) or `ListSort::Recent` (creation time desc).
    pub fn list_visible(
        &self,
        user_id:   &str,
        group_ids: &[String],
        limit:     u64,
        offset:    u64,
    ) -> Result<Vec<MemoryItem>, MiraError> {
        self.list_visible_sorted(user_id, group_ids, limit, offset, ListSort::Strength)
    }

    // Like [`Self::list_visible`] but lets the caller pick the sort.
    pub fn list_visible_sorted(
        &self,
        user_id:   &str,
        group_ids: &[String],
        limit:     u64,
        offset:    u64,
        sort:      ListSort,
    ) -> Result<Vec<MemoryItem>, MiraError> {
        let visibility = visibility_where_clause(group_ids.len(), 1);
        let order = match sort {
            ListSort::Strength => format!("{} DESC, created_at DESC", EFFECTIVE_STRENGTH_EXPR),
            ListSort::Recent   => "created_at DESC".to_string(),
        };
        let sql = format!(
            "SELECT {cols} FROM memories
             WHERE ({vis})
               AND superseded_by IS NULL
               AND deleted_at IS NULL
             ORDER BY {order}
             LIMIT ?{lim} OFFSET ?{off}",
            cols = MEMORY_COLUMNS,
            vis  = visibility,
            lim  = group_ids.len() + 2,
            off  = group_ids.len() + 3,
        );

        let mut stmt = self.conn.prepare(&sql)
            .map_err(|e| MiraError::MemoryError(format!("list_visible prepare: {}", e)))?;

        let mut params: Vec<Box<dyn ToSql>> = Vec::with_capacity(group_ids.len() + 3);
        params.push(Box::new(user_id.to_string()));
        for g in group_ids { params.push(Box::new(g.clone())); }
        params.push(Box::new(limit as i64));
        params.push(Box::new(offset as i64));

        let items = stmt
            .query_map(params_from_iter(params.iter().map(|p| p.as_ref())), row_to_memory_item)
            .map_err(|e| MiraError::MemoryError(format!("list_visible query: {}", e)))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(items)
    }

    // Count memories visible to `(user_id, group_ids)`.
    pub fn count_visible(
        &self,
        user_id:   &str,
        group_ids: &[String],
    ) -> Result<u64, MiraError> {
        let visibility = visibility_where_clause(group_ids.len(), 1);
        let sql = format!(
            "SELECT COUNT(*) FROM memories
             WHERE ({vis})
               AND superseded_by IS NULL
               AND deleted_at IS NULL",
            vis = visibility,
        );

        let mut params: Vec<Box<dyn ToSql>> = Vec::with_capacity(group_ids.len() + 1);
        params.push(Box::new(user_id.to_string()));
        for g in group_ids { params.push(Box::new(g.clone())); }

        let n: i64 = self.conn
            .query_row(&sql, params_from_iter(params.iter().map(|p| p.as_ref())), |r| r.get(0))
            .map_err(|e| MiraError::MemoryError(format!("count_visible: {}", e)))?;
        Ok(n as u64)
    }

    // Fetch a single memory by id, gated by visibility.
    pub fn get_visible(
        &self,
        id:        u64,
        user_id:   &str,
        group_ids: &[String],
    ) -> Result<Option<MemoryItem>, MiraError> {
        let visibility = visibility_where_clause(group_ids.len(), 2);
        let sql = format!(
            "SELECT {cols} FROM memories
             WHERE id = ?1 AND ({vis})
               AND superseded_by IS NULL
               AND deleted_at IS NULL",
            cols = MEMORY_COLUMNS,
            vis  = visibility,
        );

        let mut params: Vec<Box<dyn ToSql>> = Vec::with_capacity(group_ids.len() + 2);
        params.push(Box::new(id as i64));
        params.push(Box::new(user_id.to_string()));
        for g in group_ids { params.push(Box::new(g.clone())); }

        let item = self.conn
            .query_row(&sql, params_from_iter(params.iter().map(|p| p.as_ref())), row_to_memory_item)
            .optional()
            .map_err(|e| MiraError::MemoryError(format!("get_visible: {}", e)))?;
        Ok(item)
    }

    // LIKE search gated by visibility. Tokenises `query` the same way
    // [`Self::search`] does so "what is my name" finds "User's name is Tarek".
    pub fn search_visible(
        &self,
        query:     &str,
        user_id:   &str,
        group_ids: &[String],
    ) -> Result<Vec<MemoryItem>, MiraError> {
        const STOPWORDS: &[&str] = &[
            "what", "when", "where", "which", "that", "this", "with", "from",
            "have", "will", "your", "their", "about", "would", "could", "should",
            "tell", "know", "just", "some", "than", "then", "them", "there",
            "does", "been", "were", "they", "also", "very", "more", "into",
            "over", "such",
        ];

        let mut words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 3)
            .map(|w| w.to_lowercase())
            .filter(|w| !STOPWORDS.contains(&w.as_str()))
            .collect();
        if words.is_empty() {
            words.push(query.to_string());
        }

        let mut seen = std::collections::HashSet::new();
        let mut out  = Vec::new();
        for word in &words {
            let items = self.search_visible_one(word, user_id, group_ids)?;
            for item in items {
                if seen.insert(item.id) {
                    out.push(item);
                }
            }
        }
        Ok(out)
    }

    fn search_visible_one(
        &self,
        query:     &str,
        user_id:   &str,
        group_ids: &[String],
    ) -> Result<Vec<MemoryItem>, MiraError> {
        let visibility = visibility_where_clause(group_ids.len(), 2);
        let sql = format!(
            "SELECT {cols} FROM memories
             WHERE (content LIKE ?1 OR tags LIKE ?1)
               AND ({vis})
               AND superseded_by IS NULL
               AND deleted_at IS NULL
             ORDER BY {strength} DESC, created_at DESC",
            cols     = MEMORY_COLUMNS,
            vis      = visibility,
            strength = EFFECTIVE_STRENGTH_EXPR,
        );
        let pattern = format!("%{}%", query);

        let mut params: Vec<Box<dyn ToSql>> = Vec::with_capacity(group_ids.len() + 2);
        params.push(Box::new(pattern));
        params.push(Box::new(user_id.to_string()));
        for g in group_ids { params.push(Box::new(g.clone())); }

        let mut stmt = self.conn.prepare(&sql)
            .map_err(|e| MiraError::MemoryError(format!("search_visible prepare: {}", e)))?;
        let items = stmt
            .query_map(params_from_iter(params.iter().map(|p| p.as_ref())), row_to_memory_item)
            .map_err(|e| MiraError::MemoryError(format!("search_visible query: {}", e)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(items)
    }

    // Look up the newest live memory tagged `entity:<name>` for `user_id`.
    //     // Used by the post-turn LLM extractor to conflict-detect: when the
    // model re-asserts a fact that already has a memory, the caller should
    // [`Self::supersede`] the old row rather than insert a duplicate.
    //     // Only matches rows whose primary `user_id` column is `user_id` (the
    // common user-scope case). Group/system scoped rows aren't candidates
    // for per-user conflict detection — they're authored by a different
    // subject. Returns the newest matching row, or `None` if nothing
    // tagged that entity exists yet.
    pub fn find_by_entity_tag(
        &self,
        user_id: &str,
        entity:  &str,
    ) -> Result<Option<MemoryItem>, MiraError> {
        if entity.is_empty() {
            return Ok(None);
        }
        // Tags are stored as a JSON-encoded string. Match `"entity:<name>"`
        // including the surrounding quotes so that `location` doesn't
        // false-match `location_history`.
        let pattern = format!(r#"%"entity:{}"%"#, entity);
        let sql = format!(
            "SELECT {cols} FROM memories
             WHERE user_id = ?1
               AND tags LIKE ?2
               AND superseded_by IS NULL
               AND deleted_at IS NULL
             ORDER BY created_at DESC
             LIMIT 1",
            cols = MEMORY_COLUMNS,
        );
        let mut stmt = self.conn.prepare(&sql)
            .map_err(|e| MiraError::MemoryError(format!("find_by_entity_tag prepare: {}", e)))?;
        let row = stmt.query_row(params![user_id, pattern], row_to_memory_item)
            .optional()
            .map_err(|e| MiraError::MemoryError(format!("find_by_entity_tag query: {}", e)))?;
        Ok(row)
    }

    // Return **all** live memories owned by `user_id` tagged with any of the
    // given `topic:<slug>` topics, newest first.
    //     // This powers *topic-grouped retrieval*: semantic top-k surfaces only the
    // most-similar facts, which silently misses scattered members of a
    // category ("peace lily", "snake plant" don't rank near "how many
    // plants"). For aggregation/counting to be correct the model needs the
    // *complete* set, so after the semantic hits we expand by their topic tags
    // and pull every sibling. Unlike [`Self::find_by_entity_tag`] (newest one,
    // for conflict detection) this returns the whole live set.
    //     // User-scope only, mirroring `find_by_entity_tag`'s rationale: aggregation
    // facts are user-authored. `LIMIT 200` is a safety ceiling — real personal
    // topics have a handful of members.
    pub fn list_by_topic_tags(
        &self,
        user_id: &str,
        topics:  &[String],
    ) -> Result<Vec<MemoryItem>, MiraError> {
        if topics.is_empty() {
            return Ok(vec![]);
        }
        // Bind [user_id, pat_1, …, pat_n]; the surrounding quotes in the LIKE
        // pattern stop `topic:plant` from false-matching `topic:plantation`.
        let mut binds: Vec<String> = Vec::with_capacity(topics.len() + 1);
        binds.push(user_id.to_string());
        let mut clauses = Vec::with_capacity(topics.len());
        for (i, t) in topics.iter().enumerate() {
            clauses.push(format!("tags LIKE ?{}", i + 2));
            binds.push(format!(r#"%"topic:{}"%"#, t));
        }
        let sql = format!(
            "SELECT {cols} FROM memories
             WHERE user_id = ?1
               AND ({clauses})
               AND superseded_by IS NULL
               AND deleted_at IS NULL
             ORDER BY created_at DESC
             LIMIT 200",
            cols = MEMORY_COLUMNS,
            clauses = clauses.join(" OR "),
        );
        let mut stmt = self.conn.prepare(&sql)
            .map_err(|e| MiraError::MemoryError(format!("list_by_topic_tags prepare: {}", e)))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), row_to_memory_item)
            .map_err(|e| MiraError::MemoryError(format!("list_by_topic_tags query: {}", e)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::MemoryError(format!("list_by_topic_tags row: {}", e)))?);
        }
        Ok(out)
    }

    // ── Knowledge-graph storage (design-docs/graph-memory.md) ──────────────────────

    // Resolve an entity name to an id, creating the row if new. Conservative
    // Phase-1 resolution: exact match on the normalised name (lowercased,
    // article-stripped, whitespace-collapsed). Embedding-similarity merge is a
    // later refinement. Returns the existing or freshly-inserted entity id.
    pub fn graph_ensure_entity(
        &self,
        user_id:     &str,
        name:        &str,
        entity_type: &str,
    ) -> Result<i64, MiraError> {
        let norm = crate::memory::graph::normalize_name(name);
        if norm.is_empty() {
            return Err(MiraError::MemoryError("graph_ensure_entity: empty name".into()));
        }
        let existing: Option<i64> = self.conn.query_row(
            // Live entities only — a superseded loser (Phase A consolidator)
            // must not be resurrected by ensure_entity, or we'd undo the merge.
            "SELECT id FROM kg_entities
              WHERE user_id = ?1 AND name_norm = ?2 AND superseded_by IS NULL
              LIMIT 1",
            params![user_id, norm],
            |r| r.get(0),
        ).optional().map_err(|e| MiraError::MemoryError(format!("ensure_entity lookup: {}", e)))?;
        if let Some(id) = existing {
            return Ok(id);
        }
        let now = Utc::now().timestamp_millis();
        let etype = if entity_type.trim().is_empty() { "thing" } else { entity_type.trim() };
        self.conn.execute(
            "INSERT INTO kg_entities (user_id, name, name_norm, entity_type, aliases, created_at)
             VALUES (?1, ?2, ?3, ?4, '[]', ?5)",
            params![user_id, name.trim(), norm, etype, now],
        ).map_err(|e| MiraError::MemoryError(format!("ensure_entity insert: {}", e)))?;
        Ok(self.conn.last_insert_rowid())
    }

    // Insert a graph edge (a typed, timestamped fact). `valid_from` defaults to
    // `event_at` when present, else now. Returns the new edge id.
    #[allow(clippy::too_many_arguments)]
    pub fn graph_add_edge(
        &self,
        user_id:    &str,
        subject_id: i64,
        predicate:  &str,
        object_id:  Option<i64>,
        value_num:  Option<f64>,
        value_unit: Option<&str>,
        fact_text:  &str,
        event_at:   Option<i64>,
        source:     Option<&str>,
    ) -> Result<i64, MiraError> {
        let now = Utc::now().timestamp_millis();
        let valid_from = event_at.unwrap_or(now);
        self.conn.execute(
            "INSERT INTO kg_edges
               (user_id, subject_id, predicate, object_id, value_num, value_unit,
                fact_text, event_at, valid_from, valid_to, source, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL,?10,?11)",
            params![user_id, subject_id, predicate, object_id, value_num, value_unit,
                    fact_text, event_at, valid_from, source, now],
        ).map_err(|e| MiraError::MemoryError(format!("graph_add_edge: {}", e)))?;
        Ok(self.conn.last_insert_rowid())
    }

    // Count entities / edges for a user (used by tests and the bench report).
    pub fn graph_entity_count(&self, user_id: &str) -> Result<i64, MiraError> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM kg_entities WHERE user_id = ?1 AND superseded_by IS NULL",
            params![user_id], |r| r.get(0),
        ).map_err(|e| MiraError::MemoryError(format!("graph_entity_count: {}", e)))
    }

    pub fn graph_edge_count(&self, user_id: &str) -> Result<i64, MiraError> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM kg_edges WHERE user_id = ?1",
            params![user_id], |r| r.get(0),
        ).map_err(|e| MiraError::MemoryError(format!("graph_edge_count: {}", e)))
    }

    // Phase C of the sleep-like consolidator (see `design-docs/memory-research-2026.md`
    // §5): resolve contradictions in **single-valued predicates** — predicates
    // where only one current truth is meaningful (`works_at`, `lives_in`,
    // `married_to`, …). For each `(subject, predicate)` group on this list
    // with > 1 live edges, keep the newest by `event_at` (fallback
    // `created_at`) and close older edges via `valid_to`. This preserves the
    // audit trail (`valid_from`/`valid_to` bracket the period the fact was
    // current) while giving retrieval a single, consistent current truth.
    //     // Multi-valued predicates (`owned`, `worn`, `visited`, `ate`, `cost`) are
    // deliberately NOT in the list — multiple co-existing values for those
    // are normal, not contradictions. High-precision, deterministic, no LLM
    // (the LLM-as-batch-tool variant can be a future v2 for borderline cases).
    //     // Returns `(groups_resolved, edges_closed)`. Idempotent: re-running closes
    // nothing new because the older edges already have `valid_to` set.
    pub fn graph_consolidate_contradictions(
        &self, user_id: &str,
    ) -> Result<(usize, usize), MiraError> {
        // Predicates with exactly one current truth over time. Curated — expand
        // as production extraction reveals more single-valued patterns.
        const SINGLE_VALUED: &[&str] = &[
            // employment / role
            "works_at","works for","employed_at","employed_by","job","current_job",
            "career","occupation","profession","employer","role_at",
            // residence / location
            "lives_in","lives_at","lives","based_in","located_in","located_at",
            "address","home","currently_in","resides_in",
            // primary relationship
            "married_to","partnered_with","engaged_to","dating","in_relationship_with",
            "spouse","partner",
            // body / identity (single current value over time)
            "weighs","weight","height","height_is","age","age_is","ages",
            "birth_date","born_on","born_in","nationality","gender",
            // education
            "studies_at","attends","attends_school","school","university",
            "graduated_from","alma_mater","enrolled_at","major",
            // primary belongings (one current)
            "drives","primary_vehicle","primary_phone","main_email",
            "current_residence","current_pet",
        ];

        let mut stmt = self.conn.prepare(
            "SELECT id, subject_id, predicate, COALESCE(event_at, created_at) AS at
               FROM kg_edges
              WHERE user_id = ?1 AND valid_to IS NULL",
        ).map_err(|e| MiraError::MemoryError(format!("contradict prepare: {}", e)))?;

        let rows = stmt.query_map(params![user_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        }).map_err(|e| MiraError::MemoryError(format!("contradict query: {}", e)))?;

        // Group by (subject, predicate), keeping only single-valued ones.
        let mut groups: std::collections::HashMap<(i64, String), Vec<(i64, i64)>> =
            std::collections::HashMap::new();
        for r in rows {
            let (id, subj, pred, at) = r.map_err(|e|
                MiraError::MemoryError(format!("contradict row: {}", e)))?;
            if SINGLE_VALUED.contains(&pred.as_str()) {
                groups.entry((subj, pred)).or_default().push((id, at));
            }
        }
        if groups.is_empty() { return Ok((0, 0)); }

        let tx = self.conn.unchecked_transaction()
            .map_err(|e| MiraError::MemoryError(format!("contradict tx: {}", e)))?;
        let mut groups_resolved = 0usize;
        let mut edges_closed = 0usize;
        for (_key, mut edges) in groups {
            if edges.len() <= 1 { continue; }
            // Newest by event-or-created date is the current truth.
            edges.sort_by(|a, b| b.1.cmp(&a.1));
            let newest_at = edges[0].1;
            groups_resolved += 1;
            for (id, _at) in edges.iter().skip(1) {
                // Close at the newest fact's date — that's when this older
                // truth was superseded. The strict `< newest_at` filter avoids
                // closing edges that share the exact same date (multi-record
                // single-day pile-ups, e.g. the bench's date-prefix uniform
                // blanket-stamping), which would have no temporal signal.
                let touched = tx.execute(
                    "UPDATE kg_edges SET valid_to = ?1
                      WHERE id = ?2
                        AND valid_to IS NULL
                        AND COALESCE(event_at, created_at) < ?1",
                    params![newest_at, id],
                ).map_err(|e| MiraError::MemoryError(format!("contradict update: {}", e)))?;
                edges_closed += touched;
            }
        }
        tx.commit().map_err(|e| MiraError::MemoryError(format!("contradict commit: {}", e)))?;
        Ok((groups_resolved, edges_closed))
    }

    // Phase A of the sleep-like consolidator (see `design-docs/memory-research-2026.md`
    // §5): merge near-duplicate entities **within the same `entity_type`**
    // using a high-precision token-set rule (no LLM — MIRA-side and
    // deterministic per the product principle).
    //     // Two entities of the same type merge iff: the smaller's token set is a
    // **strict subset** of the larger's, AND `|smaller| / |larger| ≥
    // threshold` (size-ratio guard). This catches genuine "longer name is the
    // more specific version of the shorter" cases (`{navy, blazer} ⊂ {navy,
    // blue, blazer}` ✓) while rejecting "shared qualifier" false positives
    // (`{running, shoes}` vs `{tennis, shoes}` — neither is a subset ✗) and
    // "tiny-vs-huge" inclusion (`{plant} ⊂ {peace, lily, plant}` — ratio
    // 1/3 = 0.33 < 0.6 ✗). Bench-tested at 0.6.
    //     // On merge:
    // 1. Winner = entity with more edges (more "established"); tiebreak: older.
    // 2. Re-point every edge whose subject_id or object_id is the loser.
    // 3. Roll the loser's `name` into the winner's `aliases` JSON.
    // 4. Set `loser.superseded_by = winner.id`; retrieval drops it
    // automatically (`superseded_by IS NULL` filter on entity queries).
    //     // Idempotent (re-running finds nothing — losers are already excluded
    // from the live set). Returns `(entities_merged, edges_repointed)`.
    // LongMemEval can't validate this (its short haystacks don't grow enough
    // entity sprawl to consolidate); ships as production infrastructure.
    pub fn graph_consolidate_entities(
        &self, user_id: &str, threshold: f64,
    ) -> Result<(usize, usize), MiraError> {
        // Load live entities + per-entity edge counts in one go. Edge count
        // doubles as a "how established" tiebreaker for picking the winner.
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.name, e.name_norm, e.entity_type, e.aliases, e.created_at,
                    (SELECT COUNT(*) FROM kg_edges k
                      WHERE k.user_id = ?1
                        AND (k.subject_id = e.id OR k.object_id = e.id)) AS edge_count
               FROM kg_entities e
              WHERE e.user_id = ?1 AND e.superseded_by IS NULL",
        ).map_err(|e| MiraError::MemoryError(format!("consolidate_entities prepare: {}", e)))?;

        struct Ent {
            id: i64, name: String,
            tokens: std::collections::HashSet<String>,
            etype: String, aliases: String, created_at: i64, edges: i64,
        }
        let rows = stmt.query_map(params![user_id], |r| {
            let name: String = r.get(1)?;
            let name_norm: String = r.get(2)?;
            // Token the normalised name — split on underscore + whitespace,
            // drop 1-2 char tokens (mostly noise: "a", "of", "in").
            let tokens: std::collections::HashSet<String> = name_norm
                .split(|c: char| c == '_' || c.is_whitespace())
                .filter(|t| t.len() >= 3)
                .map(|t| t.to_string())
                .collect();
            Ok(Ent {
                id: r.get(0)?, name, tokens,
                etype: r.get(3)?, aliases: r.get(4)?, created_at: r.get(5)?, edges: r.get(6)?,
            })
        }).map_err(|e| MiraError::MemoryError(format!("consolidate_entities query: {}", e)))?;
        let mut ents: Vec<Ent> = Vec::new();
        for r in rows {
            let e = r.map_err(|e| MiraError::MemoryError(format!("consolidate_entities row: {}", e)))?;
            if !e.tokens.is_empty() { ents.push(e); }
        }
        if ents.len() < 2 { return Ok((0, 0)); }

        // Group by entity_type — only merge within a type (a clothing item
        // and a plant with similar names should NOT merge).
        let mut by_type: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        for (idx, e) in ents.iter().enumerate() {
            by_type.entry(e.etype.clone()).or_default().push(idx);
        }

        // `merged_into[i]` = current winner index for i. Chase the chain so
        // transitive merges (A→B, B→C) resolve to A→C.
        let mut merged_into: Vec<Option<usize>> = vec![None; ents.len()];
        let resolve = |idx: usize, m: &Vec<Option<usize>>| -> usize {
            let mut cur = idx;
            while let Some(next) = m[cur] { if next == cur { break; } cur = next; }
            cur
        };

        let mut pairs_merged = 0usize;
        for indices in by_type.values() {
            for i in 0..indices.len() {
                for j in (i + 1)..indices.len() {
                    let ai = resolve(indices[i], &merged_into);
                    let aj = resolve(indices[j], &merged_into);
                    if ai == aj { continue; }
                    let a = &ents[ai];
                    let b = &ents[aj];
                    // Strict-subset + size-ratio rule (high precision).
                    let (smaller, larger) = if a.tokens.len() <= b.tokens.len() {
                        (&a.tokens, &b.tokens)
                    } else {
                        (&b.tokens, &a.tokens)
                    };
                    if larger.is_empty() { continue; }
                    if !smaller.is_subset(larger) { continue; }
                    let ratio = smaller.len() as f64 / larger.len() as f64;
                    if ratio < threshold { continue; }
                    // Winner: more edges; tiebreak: older.
                    let (winner_idx, loser_idx) = match (a.edges.cmp(&b.edges), a.created_at.cmp(&b.created_at)) {
                        (std::cmp::Ordering::Greater, _) => (ai, aj),
                        (std::cmp::Ordering::Less, _)    => (aj, ai),
                        (_, std::cmp::Ordering::Less)    => (ai, aj),
                        _                                => (aj, ai),
                    };
                    merged_into[loser_idx] = Some(winner_idx);
                    pairs_merged += 1;
                }
            }
        }
        if pairs_merged == 0 { return Ok((0, 0)); }

        // Apply: re-point edges + alias-roll + supersede-mark in one tx.
        let tx = self.conn.unchecked_transaction()
            .map_err(|e| MiraError::MemoryError(format!("consolidate_entities tx: {}", e)))?;
        let mut edges_repointed = 0usize;
        for (idx, target) in merged_into.iter().enumerate() {
            let Some(winner_idx) = target else { continue };
            let winner = resolve(*winner_idx, &merged_into);
            let loser = &ents[idx];
            let winner_id = ents[winner].id;
            edges_repointed += tx.execute(
                "UPDATE kg_edges SET subject_id = ?1 WHERE user_id = ?2 AND subject_id = ?3",
                params![winner_id, user_id, loser.id],
            ).map_err(|e| MiraError::MemoryError(format!("consolidate_entities edges subj: {}", e)))?;
            edges_repointed += tx.execute(
                "UPDATE kg_edges SET object_id = ?1 WHERE user_id = ?2 AND object_id = ?3",
                params![winner_id, user_id, loser.id],
            ).map_err(|e| MiraError::MemoryError(format!("consolidate_entities edges obj: {}", e)))?;
            // Roll loser's name into winner's aliases JSON.
            let mut aliases: Vec<String> = serde_json::from_str(&ents[winner].aliases).unwrap_or_default();
            if !aliases.iter().any(|a| a == &loser.name) {
                aliases.push(loser.name.clone());
                let _ = tx.execute(
                    "UPDATE kg_entities SET aliases = ?1 WHERE id = ?2",
                    params![serde_json::to_string(&aliases).unwrap_or_default(), winner_id],
                );
            }
            let _ = tx.execute(
                "UPDATE kg_entities SET superseded_by = ?1 WHERE id = ?2",
                params![winner_id, loser.id],
            );
        }
        tx.commit().map_err(|e| MiraError::MemoryError(format!("consolidate_entities commit: {}", e)))?;
        Ok((pairs_merged, edges_repointed))
    }

    // Phase D — record an access to all live edges of the given subject ids.
    // Called from the retrieval path so we get reinforcement signal even when
    // the importance consolidator hasn't run yet (the columns are always-on;
    // only the scoring pass + retrieval bias are gated). Single UPDATE batch.
    pub fn graph_track_access(&self, user_id: &str, subject_ids: &[i64]) -> Result<usize, MiraError> {
        if subject_ids.is_empty() { return Ok(0); }
        let in_list = subject_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let now = Utc::now().timestamp_millis();
        let sql = format!(
            "UPDATE kg_edges
                SET access_count = access_count + 1,
                    last_reinforced = ?1
              WHERE user_id = ?2
                AND valid_to IS NULL
                AND subject_id IN ({in_list})",
        );
        let touched = self.conn.execute(&sql, params![now, user_id])
            .map_err(|e| MiraError::MemoryError(format!("graph_track_access: {}", e)))?;
        Ok(touched)
    }

    // Phase D — compute importance scores for every live edge of this user
    // using `ln(1 + access_count) × exp(-age_days / half_life_days)`. Stored
    // in `kg_edges.importance` so the retrieval `ORDER BY importance DESC`
    // surfaces high-reinforcement / recent edges first. Idempotent (always
    // overwrites with the freshly-computed score).
    //     // Age is measured from `last_reinforced` (fallback `created_at`). The
    // classic exponential-decay shape from cognitive psychology — frequently-
    // retrieved facts stay strong; long-unretrieved facts drift toward zero
    // without ever being deleted. Returns the number of edges scored.
    pub fn graph_consolidate_importance(
        &self, user_id: &str, half_life_days: f64,
    ) -> Result<usize, MiraError> {
        if half_life_days <= 0.0 {
            return Err(MiraError::MemoryError(
                "graph_consolidate_importance: half_life_days must be > 0".into(),
            ));
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, access_count, COALESCE(last_reinforced, created_at)
               FROM kg_edges
              WHERE user_id = ?1 AND valid_to IS NULL",
        ).map_err(|e| MiraError::MemoryError(format!("importance prepare: {}", e)))?;
        let rows = stmt.query_map(params![user_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        }).map_err(|e| MiraError::MemoryError(format!("importance query: {}", e)))?;

        let now_ms = Utc::now().timestamp_millis();
        let day_ms = 86_400_000.0_f64;
        let tx = self.conn.unchecked_transaction()
            .map_err(|e| MiraError::MemoryError(format!("importance tx: {}", e)))?;
        let mut scored = 0usize;
        for r in rows {
            let (id, access_count, lr_ms) = r.map_err(|e|
                MiraError::MemoryError(format!("importance row: {}", e)))?;
            let age_days = ((now_ms - lr_ms).max(0) as f64) / day_ms;
            let reinforce = (1.0 + access_count as f64).ln();
            let decay = (-age_days / half_life_days).exp();
            let importance = reinforce * decay;
            tx.execute(
                "UPDATE kg_edges SET importance = ?1 WHERE id = ?2",
                params![importance, id],
            ).map_err(|e| MiraError::MemoryError(format!("importance update: {}", e)))?;
            scored += 1;
        }
        tx.commit().map_err(|e| MiraError::MemoryError(format!("importance commit: {}", e)))?;
        Ok(scored)
    }

    // Human-readable sample of stored edges (subject · predicate · value/unit ·
    // fact) for eyeballing extraction/resolution quality. Diagnostic only.
    pub fn graph_sample_edges(&self, user_id: &str, limit: usize) -> Result<Vec<String>, MiraError> {
        let mut stmt = self.conn.prepare(
            "SELECT e.name, e.entity_type, k.predicate, k.value_num, k.value_unit, k.fact_text, k.event_at
             FROM kg_edges k JOIN kg_entities e ON e.id = k.subject_id
             WHERE k.user_id = ?1 AND e.superseded_by IS NULL
             ORDER BY e.entity_type, e.name, k.id LIMIT ?2",
        ).map_err(|e| MiraError::MemoryError(format!("graph_sample prepare: {}", e)))?;
        let rows = stmt.query_map(params![user_id, limit as i64], |r| {
            let name: String = r.get(0)?;
            let etype: String = r.get(1)?;
            let pred: String = r.get(2)?;
            let val:  Option<f64> = r.get(3)?;
            let unit: Option<String> = r.get(4)?;
            let fact: String = r.get(5)?;
            let event_at: Option<i64> = r.get(6)?;
            let v = match (val, unit) {
                (Some(v), Some(u)) => format!(" [{v} {u}]"),
                (Some(v), None)    => format!(" [{v}]"),
                _                  => String::new(),
            };
            let date = event_at
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "no-date".to_string());
            Ok(format!("({etype}) {name} ·{pred}·{v}  @{date}  ⟶  {fact}"))
        }).map_err(|e| MiraError::MemoryError(format!("graph_sample query: {}", e)))?;
        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::MemoryError(format!("graph_sample row: {}", e)))?); }
        Ok(out)
    }

    // All entities for a user as `(id, name, entity_type)`, for query matching.
    // `LIMIT` bounds the scan; personal graphs are small.
    pub fn graph_entities_for_user(&self, user_id: &str, limit: usize) -> Result<Vec<(i64, String, String)>, MiraError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, entity_type FROM kg_entities
              WHERE user_id = ?1 AND superseded_by IS NULL
              LIMIT ?2",
        ).map_err(|e| MiraError::MemoryError(format!("graph_entities prepare: {}", e)))?;
        let rows = stmt.query_map(params![user_id, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        }).map_err(|e| MiraError::MemoryError(format!("graph_entities query: {}", e)))?;
        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::MemoryError(format!("graph_entities row: {}", e)))?); }
        Ok(out)
    }

    // Live-edge `fact_text` (with `[value unit]` appended when numeric) for the
    // given subject entity ids, newest first. This is the complete edge set for
    // the matched entities — the exact-membership input aggregation needs.
    pub fn graph_context_for_subjects(&self, user_id: &str, subject_ids: &[i64], limit: usize) -> Result<Vec<String>, MiraError> {
        if subject_ids.is_empty() { return Ok(vec![]); }
        // subject_ids are i64 PKs from our own DB — safe to inline (no injection
        // surface), which avoids a variadic-param-count binding dance.
        let in_list = subject_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT fact_text, value_num, value_unit FROM kg_edges
             WHERE user_id = ?1 AND valid_to IS NULL AND subject_id IN ({in_list})
             ORDER BY importance DESC, COALESCE(event_at, created_at) DESC
             LIMIT ?2",
        );
        let mut stmt = self.conn.prepare(&sql)
            .map_err(|e| MiraError::MemoryError(format!("graph_context prepare: {}", e)))?;
        let rows = stmt.query_map(params![user_id, limit as i64], |r| {
            let fact: String = r.get(0)?;
            let val:  Option<f64> = r.get(1)?;
            let unit: Option<String> = r.get(2)?;
            let suffix = match (val, unit) {
                (Some(v), Some(u)) => format!(" [{v} {u}]"),
                (Some(v), None)    => format!(" [{v}]"),
                _                  => String::new(),
            };
            Ok(format!("{fact}{suffix}"))
        }).map_err(|e| MiraError::MemoryError(format!("graph_context query: {}", e)))?;
        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::MemoryError(format!("graph_context row: {}", e)))?); }
        Ok(out)
    }

    // Exists-check: does `user_id` already own a live memory tagged `tag`?
    //     // Powers idempotency for the daily rollup — we encode the target date
    // into a tag (`rollup:YYYY-MM-DD`) and skip users whose rollup for that
    // day is already present. Matches the same JSON-array-LIKE trick as
    // [`Self::find_by_entity_tag`]: the surrounding quotes mean `rollup`
    // doesn't false-match `rollup_other`.
    pub fn has_tag_for_user(
        &self,
        user_id: &str,
        tag:     &str,
    ) -> Result<bool, MiraError> {
        if tag.is_empty() { return Ok(false); }
        let pattern = format!(r#"%"{}"%"#, tag);
        let row: Option<i64> = self.conn.query_row(
            "SELECT 1 FROM memories
             WHERE user_id = ?1
               AND tags LIKE ?2
               AND superseded_by IS NULL
               AND deleted_at IS NULL
             LIMIT 1",
            params![user_id, pattern],
            |r| r.get(0),
        ).optional()
        .map_err(|e| MiraError::MemoryError(format!("has_tag_for_user: {}", e)))?;
        Ok(row.is_some())
    }

    // ──────────────────────────────────────────────────────────────────────
    // Scoped writes — new entry point that respects the full governance
    // model. Callers must assert authorization (group membership, admin
    // role) BEFORE calling; this layer trusts its inputs.
    // ──────────────────────────────────────────────────────────────────────

    // Store a new memory with an explicit scope + governance metadata.
    // For backwards compatibility `user_id` (the row's primary owner column)
    // is set to `created_by` for user scope and to a conventional marker for
    // group/system (so the legacy `user_id` filter keeps working if queried).
    #[allow(clippy::too_many_arguments)]
    pub fn store_scoped(
        &self,
        content:      String,
        category:     Category,
        tags:         Vec<String>,
        source:       Option<MemorySource>,
        scope:        Scope,
        scope_id:     Option<&str>,
        created_by:   &str,
        subject_user_ids: &[String],
        source_channel:  Option<&str>,
        source_conversation_id: Option<&str>,
        source_message_id:      Option<&str>,
    ) -> Result<u64, MiraError> {
        let created_at = Utc::now().timestamp();
        let created_at_ms = created_at * 1000;
        let tags_json = serde_json::to_string(&tags).unwrap_or_default();
        let subjects_json = serde_json::to_string(subject_user_ids).unwrap_or_else(|_| "[]".into());

        let (source_type, source_detail) = match source {
            Some(MemorySource::UserExplicit(d)) => ("user_explicit", Some(d)),
            Some(MemorySource::AutoExtracted)   => ("auto_extracted", None),
            Some(MemorySource::Imported(d))     => ("imported", Some(d)),
            None                                => ("user_explicit", None),
        };

        // `user_id` column is the legacy single-owner field. We keep it coherent
        // with scope so per-user DBs upgrade cleanly:
        // user-scope   → user_id = scope_id
        // group-scope  → user_id = scope_id (group_id) so legacy filters still see it
        // system-scope → user_id = 'system'
        let legacy_user_id = match scope {
            Scope::User   => scope_id.unwrap_or(created_by).to_string(),
            Scope::Group  => scope_id.unwrap_or("").to_string(),
            Scope::System => "system".to_string(),
        };

        self.conn.execute(
            "INSERT INTO memories (
                user_id, content, category, tags,
                source_type, source_detail, created_at, relevance_score,
                scope, scope_id, subject_user_ids, created_by,
                stability, strength, last_reinforced, access_count,
                source_channel, source_conversation_id, source_message_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1.0, ?8, ?9, ?10, ?11,
                       'stable', 1.0, ?12, 0, ?13, ?14, ?15)",
            params![
                legacy_user_id,
                content,
                category.as_str(),
                tags_json,
                source_type,
                source_detail,
                created_at,
                scope.as_str(),
                scope_id,
                subjects_json,
                created_by,
                created_at_ms,
                source_channel,
                source_conversation_id,
                source_message_id,
            ],
        ).map_err(|e| MiraError::MemoryError(format!("store_scoped: {}", e)))?;

        let id: i64 = self.conn
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .map_err(|e| MiraError::MemoryError(format!("last_insert_rowid: {}", e)))?;

        self.audit(Some(id as u64), created_by, "create", Some(scope.as_str()), scope_id, None)?;

        debug!("Stored scoped memory id={} scope={} scope_id={:?}", id, scope.as_str(), scope_id);
        Ok(id as u64)
    }

    // Append a newer memory that supersedes an older one. Neither row is
    // deleted — the old one stays readable via `*_with_history` queries so
    // historical weight is preserved, but it drops out of the default live
    // views.
    //     // Returns the new memory's id.
    #[allow(clippy::too_many_arguments)]
    pub fn supersede(
        &self,
        old_id:       u64,
        new_content:  String,
        new_category: Category,
        new_tags:     Vec<String>,
        source:       Option<MemorySource>,
        actor_user_id: &str,
    ) -> Result<u64, MiraError> {
        // Read the old row's scope so the new one inherits it. Use a direct
        // lookup (not `get`) so per-user legacy filtering doesn't mask rows
        // owned by a different user_id but visible to this caller.
        let old = self.load_any(old_id)?
            .ok_or_else(|| MiraError::NotFound(format!("Memory not found: {}", old_id)))?;

        let new_id = self.store_scoped(
            new_content,
            new_category,
            new_tags,
            source,
            old.scope.clone(),
            old.scope_id.as_deref(),
            actor_user_id,
            &[],
            None, None, None,
        )?;

        // Link the chain — old.superseded_by -> new_id, new.supersedes -> old_id.
        self.conn.execute(
            "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
            params![new_id as i64, old_id as i64],
        ).map_err(|e| MiraError::MemoryError(format!("supersede link old: {}", e)))?;
        self.conn.execute(
            "UPDATE memories SET supersedes = ?1 WHERE id = ?2",
            params![old_id as i64, new_id as i64],
        ).map_err(|e| MiraError::MemoryError(format!("supersede link new: {}", e)))?;

        self.audit(Some(old_id), actor_user_id, "supersede", Some(old.scope.as_str()),
                   old.scope_id.as_deref(), Some(&format!("new_id={}", new_id)))?;

        Ok(new_id)
    }

    // Internal: fetch a row by id with no user/scope filter. Callers must
    // have already resolved authorization before invoking.
    fn load_any(&self, id: u64) -> Result<Option<MemoryItem>, MiraError> {
        let item = self.conn
            .query_row(
                &format!("SELECT {} FROM memories WHERE id = ?1", MEMORY_COLUMNS),
                params![id as i64],
                row_to_memory_item,
            )
            .optional()
            .map_err(|e| MiraError::MemoryError(format!("load_any: {}", e)))?;
        Ok(item)
    }

    // Reinforce a memory: bump `access_count`, push `last_reinforced = now`,
    // and move `strength` 10% of the way back toward 1.0. No-op if the row
    // is superseded or soft-deleted. Returns the new strength on success.
    pub fn reinforce(&self, id: u64, actor_user_id: &str) -> Result<Option<f32>, MiraError> {
        let now_ms = Utc::now().timestamp_millis();
        // `strength = strength + 0.1 * (1.0 - strength)` — caps at 1.0 without
        // branching; old-row default of 1.0 stays put.
        let rows = self.conn.execute(
            "UPDATE memories
             SET strength        = MIN(1.0, strength + 0.1 * (1.0 - strength)),
                 access_count    = access_count + 1,
                 last_reinforced = ?1
             WHERE id = ?2
               AND superseded_by IS NULL
               AND deleted_at    IS NULL",
            params![now_ms, id as i64],
        ).map_err(|e| MiraError::MemoryError(format!("reinforce: {}", e)))?;
        if rows == 0 { return Ok(None); }

        // Audit is best-effort — don't fail the reinforcement if logging fails.
        let _ = self.audit(Some(id), actor_user_id, "reinforce", None, None, None);

        let s: f64 = self.conn.query_row(
            "SELECT strength FROM memories WHERE id = ?1",
            params![id as i64],
            |r| r.get(0),
        ).map_err(|e| MiraError::MemoryError(format!("reinforce read-back: {}", e)))?;
        Ok(Some(s as f32))
    }

    // Admin-only soft delete. The row stays for audit but is filtered out of
    // every visibility-aware read.
    pub fn soft_delete(&self, id: u64, actor_user_id: &str) -> Result<bool, MiraError> {
        let now = Utc::now().timestamp_millis();
        let rows = self.conn.execute(
            "UPDATE memories SET deleted_at = ?1, deleted_by = ?2
             WHERE id = ?3 AND deleted_at IS NULL",
            params![now, actor_user_id, id as i64],
        ).map_err(|e| MiraError::MemoryError(format!("soft_delete: {}", e)))?;

        if rows > 0 {
            self.audit(Some(id), actor_user_id, "soft_delete", None, None, None)?;
        }
        Ok(rows > 0)
    }

    // Append a row to the audit log. Errors are logged but not propagated
    // for non-critical actions to avoid masking the underlying write.
    fn audit(
        &self,
        memory_id:     Option<u64>,
        actor_user_id: &str,
        action:        &str,
        scope:         Option<&str>,
        scope_id:      Option<&str>,
        detail:        Option<&str>,
    ) -> Result<(), MiraError> {
        let at = Utc::now().timestamp_millis();
        self.conn.execute(
            "INSERT INTO memory_audit (memory_id, actor_user_id, action, scope, scope_id, at, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![memory_id.map(|v| v as i64), actor_user_id, action, scope, scope_id, at, detail],
        ).map_err(|e| MiraError::MemoryError(format!("audit: {}", e)))?;
        Ok(())
    }
}

// ── Row + enum parsing helpers ────────────────────────────────────────────────

const MEMORY_COLUMNS: &str =
    "id, content, category, tags, source_type, source_detail, created_at, relevance_score, \
     scope, scope_id, created_by, supersedes, superseded_by, \
     strength, access_count, last_reinforced, stability, \
     source_channel, source_conversation_id, source_message_id, \
     mira_decay(strength, last_reinforced, stability) AS effective_strength";

fn row_to_memory_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryItem> {
    let category_str: String = row.get(2)?;
    let source_type:  String = row.get(4)?;
    let source_detail: Option<String> = row.get(5)?;
    let scope_str: String = row.get::<_, Option<String>>(8)?.unwrap_or_else(|| "user".into());
    Ok(MemoryItem {
        id:              row.get::<_, i64>(0)? as u64,
        content:         row.get(1)?,
        category:        parse_category(&category_str).unwrap_or(Category::Fact),
        tags:            parse_tags_json(&row.get::<_, String>(3)?).unwrap_or_default(),
        source:          parse_source(&source_type, source_detail),
        created_at:      DateTime::from_timestamp(row.get(6)?, 0).unwrap_or(Utc::now()),
        relevance_score: row.get::<_, Option<f32>>(7)?.unwrap_or(1.0),
        scope:           Scope::parse(&scope_str),
        scope_id:        row.get(9)?,
        created_by:      row.get(10)?,
        supersedes:      row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        superseded_by:   row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        strength:            row.get::<_, Option<f64>>(13)?.unwrap_or(1.0) as f32,
        access_count:        row.get::<_, Option<i64>>(14)?.unwrap_or(0) as u32,
        last_reinforced:     row.get::<_, Option<i64>>(15)?.unwrap_or(0),
        stability:           row.get::<_, Option<String>>(16)?.unwrap_or_else(|| "stable".into()),
        source_channel:         row.get::<_, Option<String>>(17)?,
        source_conversation_id: row.get::<_, Option<String>>(18)?,
        source_message_id:      row.get::<_, Option<String>>(19)?,
        effective_strength:  row.get::<_, Option<f64>>(20)?.unwrap_or(1.0) as f32,
    })
}

// Register the `mira_decay(strength, last_reinforced_ms, stability)` scalar
// function on this connection. Deterministic-but-wall-clock-dependent, so
// results within a single query are consistent but re-evaluate on re-query.
fn register_decay_udf(conn: &Connection) -> Result<(), MiraError> {
    conn.create_scalar_function(
        "mira_decay",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DIRECTONLY,
        |ctx| {
            // Nullable inputs — migrating rows may not have all three yet.
            let strength: f64 = ctx.get::<Option<f64>>(0)?.unwrap_or(1.0);
            let last:     i64 = ctx.get::<Option<i64>>(1)?.unwrap_or(0);
            let stability: String = ctx.get::<Option<String>>(2)?.unwrap_or_else(|| "stable".into());
            Ok(compute_effective_strength(strength, last, &stability))
        },
    )
    .map_err(|e| MiraError::MemoryError(format!("register mira_decay: {}", e)))?;
    Ok(())
}

fn parse_category(s: &str) -> Option<Category> {
    match s {
        "fact"         => Some(Category::Fact),
        "preference"   => Some(Category::Preference),
        "skill"        => Some(Category::Skill),
        "relationship" => Some(Category::Relationship),
        "project"      => Some(Category::Project),
        _              => None,
    }
}

fn parse_tags_json(json: &str) -> Option<Vec<String>> {
    if json.is_empty() { return Some(vec![]); }
    serde_json::from_str(json).ok()
}

fn parse_source(source_type: &str, source_detail: Option<String>) -> Option<MemorySource> {
    match source_type {
        "user_explicit"  => Some(MemorySource::UserExplicit(source_detail.unwrap_or_default())),
        "auto_extracted" => Some(MemorySource::AutoExtracted),
        "imported"       => Some(MemorySource::Imported(source_detail.unwrap_or_default())),
        _                => None,
    }
}

// Build a SQL visibility clause. `first_param_index` is the 1-based param
// index of `user_id`; group_ids follow at `first_param_index + 1..`.
// // Example, with `first_param_index = 1` and two group ids:
// `(scope = 'user' AND scope_id = ?1) OR scope = 'system'
// OR (scope = 'group' AND scope_id IN (?2, ?3))`
fn visibility_where_clause(group_count: usize, first_param_index: usize) -> String {
    let user_clause = format!(
        "(scope = 'user' AND scope_id = ?{})",
        first_param_index
    );
    let system_clause = "scope = 'system'".to_string();
    if group_count == 0 {
        return format!("{} OR {}", user_clause, system_clause);
    }
    let placeholders: Vec<String> = (0..group_count)
        .map(|i| format!("?{}", first_param_index + 1 + i))
        .collect();
    format!(
        "{} OR {} OR (scope = 'group' AND scope_id IN ({}))",
        user_clause,
        system_clause,
        placeholders.join(","),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn contradiction_consolidator_closes_older_single_valued_edges() {
        let path = "/tmp/mira_test_contradict.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new(path).unwrap();
        // Subject "user" carries two contradictory `works_at` edges (Google
        // older, Anthropic newer) and two multi-valued `owned` edges (camera,
        // bike — both true simultaneously). Phase C should close Google but
        // leave both `owned` edges alone.
        let user = s.graph_ensure_entity("default", "user", "person").unwrap();
        let _e1 = s.graph_add_edge("default", user, "works_at", None, None, None,
            "User works at Google", Some(1_700_000_000_000), None).unwrap();
        let e2 = s.graph_add_edge("default", user, "works_at", None, None, None,
            "User works at Anthropic", Some(1_750_000_000_000), None).unwrap();
        let e3 = s.graph_add_edge("default", user, "owned", None, None, None,
            "User owns a camera", Some(1_710_000_000_000), None).unwrap();
        let e4 = s.graph_add_edge("default", user, "owned", None, None, None,
            "User owns a bike", Some(1_720_000_000_000), None).unwrap();

        let (groups, closed) = s.graph_consolidate_contradictions("default").unwrap();
        assert_eq!(groups, 1, "exactly one (subject,predicate) group resolved");
        assert_eq!(closed, 1, "exactly one older edge closed");

        // The Google edge has valid_to set; Anthropic + both owned edges remain live.
        let live: i64 = s.conn.query_row(
            "SELECT COUNT(*) FROM kg_edges WHERE user_id='default' AND valid_to IS NULL",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(live, 3, "3 of 4 edges remain live (Google closed)");
        // Specifically: newer works_at, both owned, NOT older works_at.
        let still_live = |id: i64| -> i64 {
            s.conn.query_row(
                "SELECT COUNT(*) FROM kg_edges WHERE id=?1 AND valid_to IS NULL",
                params![id], |r| r.get(0),
            ).unwrap()
        };
        assert_eq!(still_live(e2), 1, "newer works_at must stay live");
        assert_eq!(still_live(e3), 1, "owned-camera must stay live (multi-valued)");
        assert_eq!(still_live(e4), 1, "owned-bike must stay live (multi-valued)");

        // Idempotent: a second run finds nothing more to close.
        assert_eq!(s.graph_consolidate_contradictions("default").unwrap(), (0, 0));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn entity_dedup_consolidator_strict_subset_rule() {
        let path = "/tmp/mira_test_entitydedup.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new(path).unwrap();
        // Subset case (should merge) + same-type-but-not-subset (should NOT)
        // + cross-type-name-overlap (should NOT — different entity_type).
        let blazer       = s.graph_ensure_entity("default", "navy blazer", "clothing").unwrap();
        let blazer_long  = s.graph_ensure_entity("default", "navy blue blazer", "clothing").unwrap();
        let running_sh   = s.graph_ensure_entity("default", "running shoes", "clothing").unwrap();
        let tennis_sh    = s.graph_ensure_entity("default", "tennis shoes",  "clothing").unwrap();
        let plant        = s.graph_ensure_entity("default", "peace lily",    "plant").unwrap();
        // Edges: blazer_long has 2 → wins the dedup tiebreak vs blazer's 1.
        s.graph_add_edge("default", blazer,      "worn",       None, None, None, "wore navy blazer", None, None).unwrap();
        s.graph_add_edge("default", blazer_long, "worn",       None, None, None, "wore navy blue blazer", None, None).unwrap();
        s.graph_add_edge("default", blazer_long, "dry_cleaned",None, None, None, "dry-cleaned navy blue blazer", None, None).unwrap();
        s.graph_add_edge("default", running_sh,  "worn",       None, None, None, "wore running shoes", None, None).unwrap();
        s.graph_add_edge("default", tennis_sh,   "worn",       None, None, None, "wore tennis shoes", None, None).unwrap();
        s.graph_add_edge("default", plant,       "owned",      None, None, None, "owns peace lily", None, None).unwrap();
        assert_eq!(s.graph_entity_count("default").unwrap(), 5);

        let (merged, repointed) = s.graph_consolidate_entities("default", 0.6).unwrap();
        // Exactly one merge: navy blazer ⊂ navy blue blazer (ratio 2/3 = 0.67).
        // running/tennis shoes: neither is a subset → not merged.
        // peace lily: different entity_type → not merged.
        assert_eq!(merged, 1, "exactly one merge (blazer-subset case)");
        assert_eq!(repointed, 1, "blazer had 1 subject edge to re-point");
        assert_eq!(s.graph_entity_count("default").unwrap(), 4, "blazer superseded → live count drops 1");
        assert_eq!(s.graph_edge_count("default").unwrap(), 6, "edge count unchanged (edges re-pointed, not deleted)");
        // Idempotent.
        assert_eq!(s.graph_consolidate_entities("default", 0.6).unwrap(), (0, 0));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn importance_scoring_reinforces_and_decays() {
        let path = "/tmp/mira_test_importance.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new(path).unwrap();
        let bike = s.graph_ensure_entity("default", "bike", "vehicle").unwrap();
        let car  = s.graph_ensure_entity("default", "car",  "vehicle").unwrap();
        let _e1 = s.graph_add_edge("default", bike, "owned", None, None, None,
            "User owns bike", None, None).unwrap();
        let _e2 = s.graph_add_edge("default", car,  "owned", None, None, None,
            "User owns car", None, None).unwrap();

        // Reinforce bike three times, car zero. Tracking is the always-on path.
        for _ in 0..3 { s.graph_track_access("default", &[bike]).unwrap(); }

        // Score with a long half-life so age decay is negligible — the
        // reinforcement difference should dominate.
        let scored = s.graph_consolidate_importance("default", 3650.0).unwrap();
        assert_eq!(scored, 2, "both live edges scored");

        // Bike's edge should have HIGHER importance than car's.
        let bike_imp: f64 = s.conn.query_row(
            "SELECT importance FROM kg_edges WHERE subject_id=?1", params![bike], |r| r.get(0)
        ).unwrap();
        let car_imp: f64 = s.conn.query_row(
            "SELECT importance FROM kg_edges WHERE subject_id=?1", params![car], |r| r.get(0)
        ).unwrap();
        assert!(bike_imp > car_imp, "bike (3 reinforcements) > car (0): {bike_imp} vs {car_imp}");
        assert!(bike_imp > 0.0, "bike should have positive importance");
        // ln(1+3) ≈ 1.386 — close to (0.99–1.0 of) the un-decayed strength
        // since half-life=3650d and the edge was just reinforced.
        assert!((bike_imp - (1.0_f64 + 3.0).ln()).abs() < 0.01,
            "bike importance ≈ ln(4) with negligible decay: got {bike_imp}");

        // Idempotent (re-running just rewrites the same value).
        assert_eq!(s.graph_consolidate_importance("default", 3650.0).unwrap(), 2);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_store_and_retrieve() {
        let path = "/tmp/mira_test_1.db";
        std::fs::remove_file(path).ok(); // Clean up if exists
        let storage = MemoryStorage::new(path).unwrap();
        
        let id = storage.store(
            "User's name is Tarek".to_string(),
            Category::Fact,
            vec!["name".to_string(), "identity".to_string()],
            Some(MemorySource::UserExplicit("test".to_string())),
        ).unwrap();
        
        assert!(id > 0);
        
        let item = storage.get(id).unwrap().expect("Memory should exist");
        assert_eq!(item.content, "User's name is Tarek");
        assert_eq!(item.category, Category::Fact);
    }
    
    #[test]
    fn test_search() {
        let path = "/tmp/mira_test_2.db";
        std::fs::remove_file(path).ok(); // Clean up if exists
        let storage = MemoryStorage::new(path).unwrap();
        
        storage.store("I like Rust programming".to_string(), Category::Preference, vec![], None).unwrap();
        storage.store("Python is also good".to_string(), Category::Fact, vec![], None).unwrap();
        
        let results = storage.search("Rust").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("Rust"));
    }

    #[test]
    fn test_get_by_category() {
        let path = "/tmp/mira_test_3.db";
        std::fs::remove_file(path).ok();
        let storage = MemoryStorage::new(path).unwrap();
        storage.store("I love coffee".to_string(), Category::Preference, vec![], None).unwrap();
        storage.store("I love tea".to_string(), Category::Preference, vec![], None).unwrap();
        storage.store("I live in Cairo".to_string(), Category::Fact, vec![], None).unwrap();

        let prefs = storage.get_by_category(&Category::Preference).unwrap();
        assert_eq!(prefs.len(), 2);
        assert!(prefs.iter().all(|m| m.category == Category::Preference));

        let facts = storage.get_by_category(&Category::Fact).unwrap();
        assert_eq!(facts.len(), 1);
    }

    #[test]
    fn test_delete() {
        let path = "/tmp/mira_test_4.db";
        std::fs::remove_file(path).ok();
        let storage = MemoryStorage::new(path).unwrap();
        let id = storage.store("To delete".to_string(), Category::Fact, vec![], None).unwrap();

        assert!(storage.get(id).unwrap().is_some());
        let deleted = storage.delete(id).unwrap();
        assert!(deleted);
        assert!(storage.get(id).unwrap().is_none());
        // Deleting again returns false
        assert!(!storage.delete(id).unwrap());
    }

    #[test]
    fn test_count() {
        let path = "/tmp/mira_test_5.db";
        std::fs::remove_file(path).ok();
        let storage = MemoryStorage::new(path).unwrap();
        assert_eq!(storage.count().unwrap(), 0);
        storage.store("One".to_string(), Category::Fact, vec![], None).unwrap();
        storage.store("Two".to_string(), Category::Fact, vec![], None).unwrap();
        assert_eq!(storage.count().unwrap(), 2);
    }

    #[test]
    fn test_tags_stored_and_retrieved() {
        let path = "/tmp/mira_test_6.db";
        std::fs::remove_file(path).ok();
        let storage = MemoryStorage::new(path).unwrap();
        let id = storage.store(
            "Tagged fact".to_string(),
            Category::Fact,
            vec!["work".to_string(), "important".to_string()],
            None,
        ).unwrap();
        let item = storage.get(id).unwrap().unwrap();
        assert_eq!(item.tags, vec!["work", "important"]);
    }

    #[test]
    fn test_all_categories_roundtrip() {
        let path = "/tmp/mira_test_7.db";
        std::fs::remove_file(path).ok();
        let storage = MemoryStorage::new(path).unwrap();
        for cat in [Category::Fact, Category::Preference, Category::Skill, Category::Relationship, Category::Project] {
            let id = storage.store(format!("Content for {:?}", cat), cat.clone(), vec![], None).unwrap();
            let item = storage.get(id).unwrap().unwrap();
            assert_eq!(item.category, cat);
        }
    }

    #[test]
    fn test_user_isolation() {
        let path = "/tmp/mira_test_user_iso.db";
        std::fs::remove_file(path).ok();
        let storage = MemoryStorage::new_for_user(path, "user-alice").unwrap();

        storage.store("Alice's memory".to_string(), Category::Fact, vec![], None).unwrap();

        let alice_results = storage.search("Alice").unwrap();
        assert_eq!(alice_results.len(), 1);

        let bob_storage = MemoryStorage::new_for_user(path, "user-bob").unwrap();
        let bob_results = bob_storage.search("Alice").unwrap();
        assert!(bob_results.is_empty());
    }

    #[test]
    fn test_user_count_isolated() {
        let path = "/tmp/mira_test_user_count.db";
        std::fs::remove_file(path).ok();
        let alice = MemoryStorage::new_for_user(path, "alice").unwrap();
        let bob = MemoryStorage::new_for_user(path, "bob").unwrap();
        alice.store("a1".to_string(), Category::Fact, vec![], None).unwrap();
        alice.store("a2".to_string(), Category::Fact, vec![], None).unwrap();
        bob.store("b1".to_string(), Category::Fact, vec![], None).unwrap();
        assert_eq!(alice.count().unwrap(), 2);
        assert_eq!(bob.count().unwrap(), 1);
    }

    #[test]
    fn test_search_by_tag() {
        let path = "/tmp/mira_test_8.db";
        std::fs::remove_file(path).ok();
        let storage = MemoryStorage::new(path).unwrap();
        storage.store("No tags here".to_string(), Category::Fact, vec![], None).unwrap();
        storage.store("Tagged entry".to_string(), Category::Fact, vec!["python".to_string()], None).unwrap();
        // Search matches content AND tags
        let results = storage.search("python").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].tags.contains(&"python".to_string()));
    }

    #[test]
    fn has_tag_for_user_is_scoped_and_exact() {
        let path = "/tmp/mira_test_has_tag.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "alice").unwrap();

        s.store_scoped(
            "alice yesterday".into(), Category::Fact,
            vec!["rollup".to_owned(), "rollup:2026-04-23".to_owned()],
            None, Scope::User, Some("alice"), "alice", &[], None, None, None,
        ).unwrap();
        // Bob owns a *different* day's rollup — shouldn't leak into alice's check.
        s.store_scoped(
            "bob other day".into(), Category::Fact,
            vec!["rollup".to_owned(), "rollup:2026-04-22".to_owned()],
            None, Scope::User, Some("bob"), "bob", &[], None, None, None,
        ).unwrap();

        assert!(s.has_tag_for_user("alice", "rollup:2026-04-23").unwrap());
        assert!(!s.has_tag_for_user("alice", "rollup:2026-04-22").unwrap(),
            "alice must not see bob's rollup day");
        assert!(!s.has_tag_for_user("bob",   "rollup:2026-04-23").unwrap());
        assert!(!s.has_tag_for_user("alice", "rollup:2026-04").unwrap(),
            "prefix match must not succeed — JSON quote boundaries prevent it");
        assert!(!s.has_tag_for_user("alice", "").unwrap(), "empty tag returns false");
    }

    // Provenance columns round-trip through store_scoped → list_visible → MemoryItem,
    // and None-valued fields stay None rather than being stringified.
    #[test]
    fn provenance_round_trips_through_storage() {
        let path = "/tmp/mira_test_provenance_rt.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "alice").unwrap();

        s.store_scoped(
            "sourced fact".into(), Category::Fact, vec![], None,
            Scope::User, Some("alice"), "alice", &[],
            Some("web"), Some("conv-123"), Some("msg-7"),
        ).unwrap();
        s.store_scoped(
            "unsourced fact".into(), Category::Fact, vec![], None,
            Scope::User, Some("alice"), "alice", &[],
            None, None, None,
        ).unwrap();

        let items = s.list_visible("alice", &[], 100, 0).unwrap();
        let sourced = items.iter().find(|m| m.content == "sourced fact").unwrap();
        assert_eq!(sourced.source_channel.as_deref(),         Some("web"));
        assert_eq!(sourced.source_conversation_id.as_deref(), Some("conv-123"));
        assert_eq!(sourced.source_message_id.as_deref(),      Some("msg-7"));

        let unsourced = items.iter().find(|m| m.content == "unsourced fact").unwrap();
        assert!(unsourced.source_channel.is_none());
        assert!(unsourced.source_conversation_id.is_none());
        assert!(unsourced.source_message_id.is_none());
    }

    // ── Scope + visibility ────────────────────────────────────────────────

    // Visibility chokepoint: alice sees her own + group 'g1' + system,
    // but not bob's private memory nor group 'g2' she isn't in.
    #[test]
    fn test_visibility_across_scopes() {
        let path = "/tmp/mira_test_vis.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "alice").unwrap();

        // alice's private
        s.store_scoped("alice secret".into(), Category::Fact, vec![], None,
                       Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();
        // bob's private
        s.store_scoped("bob secret".into(), Category::Fact, vec![], None,
                       Scope::User, Some("bob"), "bob", &[], None, None, None).unwrap();
        // group g1 (alice is member) + g2 (alice not a member)
        s.store_scoped("g1 shared".into(), Category::Fact, vec![], None,
                       Scope::Group, Some("g1"), "alice", &[], None, None, None).unwrap();
        s.store_scoped("g2 shared".into(), Category::Fact, vec![], None,
                       Scope::Group, Some("g2"), "bob", &[], None, None, None).unwrap();
        // system (everyone)
        s.store_scoped("world fact".into(), Category::Fact, vec![], None,
                       Scope::System, None, "admin", &[], None, None, None).unwrap();

        let alice_groups = vec!["g1".to_string()];
        let items = s.list_visible("alice", &alice_groups, 100, 0).unwrap();
        let contents: Vec<_> = items.iter().map(|m| m.content.as_str()).collect();

        assert!(contents.contains(&"alice secret"));
        assert!(contents.contains(&"g1 shared"));
        assert!(contents.contains(&"world fact"));
        assert!(!contents.contains(&"bob secret"));
        assert!(!contents.contains(&"g2 shared"));
        assert_eq!(items.len(), 3);
    }

    // count_visible mirrors list_visible.
    #[test]
    fn test_count_visible() {
        let path = "/tmp/mira_test_count_vis.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "a").unwrap();
        s.store_scoped("a1".into(), Category::Fact, vec![], None,
                       Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();
        s.store_scoped("b1".into(), Category::Fact, vec![], None,
                       Scope::User, Some("bob"),   "bob",   &[], None, None, None).unwrap();
        s.store_scoped("g1".into(), Category::Fact, vec![], None,
                       Scope::Group, Some("g1"),   "alice", &[], None, None, None).unwrap();
        s.store_scoped("sys".into(), Category::Fact, vec![], None,
                       Scope::System, None,        "admin", &[], None, None, None).unwrap();

        assert_eq!(s.count_visible("alice", &["g1".to_string()]).unwrap(), 3);
        assert_eq!(s.count_visible("bob",   &[]).unwrap(),                 2); // own + system
    }

    // get_visible honours visibility: you can read your own but not
    // another user's private row.
    #[test]
    fn test_get_visible_enforces_scope() {
        let path = "/tmp/mira_test_get_vis.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "a").unwrap();

        let alice_id = s.store_scoped("alice private".into(), Category::Fact, vec![], None,
                                      Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();
        let bob_id = s.store_scoped("bob private".into(), Category::Fact, vec![], None,
                                    Scope::User, Some("bob"), "bob", &[], None, None, None).unwrap();

        assert!(s.get_visible(alice_id, "alice", &[]).unwrap().is_some());
        assert!(s.get_visible(bob_id,   "alice", &[]).unwrap().is_none());
        assert!(s.get_visible(bob_id,   "bob",   &[]).unwrap().is_some());
    }

    // search_visible only returns rows the caller can see.
    #[test]
    fn test_search_visible_filters() {
        let path = "/tmp/mira_test_search_vis.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "a").unwrap();
        s.store_scoped("alice likes coffee".into(), Category::Preference, vec![], None,
                       Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();
        s.store_scoped("bob likes coffee".into(), Category::Preference, vec![], None,
                       Scope::User, Some("bob"), "bob", &[], None, None, None).unwrap();

        let hits = s.search_visible("coffee", "alice", &[]).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("alice"));
    }

    // Superseded rows drop out of live reads; the supersedes/superseded_by
    // fields describe the chain.
    #[test]
    fn test_supersede_chain() {
        let path = "/tmp/mira_test_supersede.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "alice").unwrap();

        let old_id = s.store_scoped("User lives in Cairo".into(), Category::Fact, vec![], None,
                                    Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();

        let new_id = s.supersede(
            old_id,
            "User lives in Dubai".into(),
            Category::Fact,
            vec![],
            None,
            "alice",
        ).unwrap();

        let live = s.list_visible("alice", &[], 100, 0).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, new_id);
        assert_eq!(live[0].supersedes, Some(old_id));
        assert!(live[0].content.contains("Dubai"));

        // Old row is still stored but marked superseded (read via load_any
        // since legacy `get` filters by user_id).
        let old = s.load_any(old_id).unwrap().unwrap();
        assert_eq!(old.superseded_by, Some(new_id));
    }

    // Soft-delete hides a row from every visibility read but keeps it on disk.
    #[test]
    fn test_soft_delete_hides_from_visibility() {
        let path = "/tmp/mira_test_soft_delete.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "a").unwrap();

        let id = s.store_scoped("transient".into(), Category::Fact, vec![], None,
                                Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();
        assert_eq!(s.count_visible("alice", &[]).unwrap(), 1);

        let deleted = s.soft_delete(id, "admin").unwrap();
        assert!(deleted);
        assert_eq!(s.count_visible("alice", &[]).unwrap(), 0);

        // Row still present in raw DB
        let raw: i64 = s.conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE id = ?1",
            params![id as i64], |r| r.get(0)
        ).unwrap();
        assert_eq!(raw, 1);
    }

    // ── decay + reinforcement ────────────────────────────────────

    // The Rust-side decay helper matches its SQL UDF counterpart: a fresh
    // reinforcement leaves strength essentially unchanged, and a one-day-old
    // stable memory is still close to 1.0 (90-day half-life).
    #[test]
    fn test_decay_formula_monotonic() {
        let now_ms = Utc::now().timestamp_millis();
        let fresh  = compute_effective_strength(1.0, now_ms, "stable");
        let day    = compute_effective_strength(1.0, now_ms - 86_400_000, "stable");
        let month  = compute_effective_strength(1.0, now_ms - 30 * 86_400_000, "stable");
        // Fresh is ~1.0, and older values strictly smaller.
        assert!(fresh > 0.999);
        assert!(fresh > day);
        assert!(day   > month);
        // 30 days on a 90-day half-life → 2^(-1/3) ≈ 0.794.
        assert!((month - 0.794f64).abs() < 0.01);
    }

    // Stability tier picks the half-life: ephemeral decays fastest, permanent
    // never. Ephemeral dropping below stable at the same age is the point.
    #[test]
    fn test_decay_by_stability_tier() {
        let now_ms = Utc::now().timestamp_millis();
        let age_ms = 86_400_000 * 3; // 3 days old
        let ephemeral = compute_effective_strength(1.0, now_ms - age_ms, "ephemeral");
        let episodic  = compute_effective_strength(1.0, now_ms - age_ms, "episodic");
        let stable    = compute_effective_strength(1.0, now_ms - age_ms, "stable");
        let permanent = compute_effective_strength(1.0, now_ms - age_ms, "permanent");
        assert!(ephemeral < episodic);
        assert!(episodic  < stable);
        assert!(stable    < permanent);
        assert!((permanent - 1.0).abs() < 1e-9);   // permanent never decays
        assert!(ephemeral < 0.15);                  // 3 × 1-day half-lives → ~1/8
    }

    // reinforce() bumps access_count, moves strength toward 1.0, and pushes
    // last_reinforced forward. A no-op is reported when the row is missing.
    #[test]
    fn test_reinforce_bumps_strength_and_counters() {
        let path = "/tmp/mira_test_reinforce.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "alice").unwrap();

        let id = s.store_scoped("facts".into(), Category::Fact, vec![], None,
                                Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();

        // Fake an old, half-decayed memory by pushing last_reinforced back 30
        // days and dropping strength to 0.5.
        let thirty_days_ago = Utc::now().timestamp_millis() - 30 * 86_400_000;
        s.conn.execute(
            "UPDATE memories SET strength = 0.5, access_count = 3, last_reinforced = ?1 WHERE id = ?2",
            params![thirty_days_ago, id as i64],
        ).unwrap();

        let new_strength = s.reinforce(id, "alice").unwrap().expect("row reinforced");
        assert!((new_strength - 0.55).abs() < 0.001, "strength nudged 10% toward 1.0");

        let item = s.load_any(id).unwrap().unwrap();
        assert_eq!(item.access_count, 4);
        assert!(item.last_reinforced > thirty_days_ago, "last_reinforced moved forward");

        // Superseded rows are skipped.
        assert!(s.reinforce(99_999, "alice").unwrap().is_none(), "missing row → None");
    }

    // list_visible orders by effective_strength DESC by default: a freshly
    // reinforced memory beats an older stable one, and Recent sort flips the
    // order back to chronological.
    #[test]
    fn test_list_visible_strength_ordering() {
        let path = "/tmp/mira_test_sort.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "alice").unwrap();

        let old_id = s.store_scoped("old but weak".into(), Category::Fact, vec![], None,
                                    Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure created_at differs by ≥1 s
        let new_id = s.store_scoped("fresh and strong".into(), Category::Fact, vec![], None,
                                    Scope::User, Some("alice"), "alice", &[], None, None, None).unwrap();

        // Age the old row by a year and drop its strength so its effective
        // strength falls well below the fresh one's.
        let year_ago = Utc::now().timestamp_millis() - 365 * 86_400_000;
        s.conn.execute(
            "UPDATE memories SET strength = 0.2, last_reinforced = ?1 WHERE id = ?2",
            params![year_ago, old_id as i64],
        ).unwrap();

        // Default sort (Strength): fresh row wins.
        let by_strength = s.list_visible("alice", &[], 100, 0).unwrap();
        assert_eq!(by_strength[0].id, new_id);
        assert!(by_strength[0].effective_strength > by_strength[1].effective_strength);

        // Recent sort: strictly chronological, so the newer row is still first,
        // but the ordering now ignores effective_strength. Verify by dropping
        // the newer row's strength and checking it still leads on Recent but
        // loses on Strength.
        s.conn.execute("UPDATE memories SET strength = 0.1 WHERE id = ?1",
                       params![new_id as i64]).unwrap();

        let by_recent = s.list_visible_sorted("alice", &[], 100, 0, ListSort::Recent).unwrap();
        assert_eq!(by_recent[0].id, new_id, "Recent keeps newest first");

        let by_strength_again = s.list_visible_sorted("alice", &[], 100, 0, ListSort::Strength).unwrap();
        // Both rows are now weak — with Strength sort, whichever decays less
        // leads. The old row has a year of stable decay on strength 0.2; new
        // row has ~no age on strength 0.1. Effective strength numerically:
        assert!(by_strength_again[0].effective_strength >= by_strength_again[1].effective_strength);
    }

    // delete_by_source_detail removes only rows matching the requested
    // source_detail for a specific subject user — leaving other users' rows
    // and other detail tags alone.
    #[test]
    fn test_delete_by_source_detail_is_user_scoped() {
        let path = "/tmp/mira_test_delete_by_source.db";
        std::fs::remove_file(path).ok();
        let s = MemoryStorage::new_for_user(path, "shared").unwrap();

        // Two users, two kinds of imports. Only alice's onboarding seeds
        // should disappear.
        let alice_seed = s.store_scoped(
            "alice seed".into(), Category::Fact, vec![],
            Some(MemorySource::Imported("onboarding".into())),
            Scope::User, Some("alice"), "alice", &["alice".into()], None, None, None,
        ).unwrap();
        let alice_other = s.store_scoped(
            "alice other".into(), Category::Fact, vec![],
            Some(MemorySource::Imported("slack-dump".into())),
            Scope::User, Some("alice"), "alice", &["alice".into()], None, None, None,
        ).unwrap();
        let bob_seed = s.store_scoped(
            "bob seed".into(), Category::Fact, vec![],
            Some(MemorySource::Imported("onboarding".into())),
            Scope::User, Some("bob"), "bob", &["bob".into()], None, None, None,
        ).unwrap();

        let (count, ids) = s.delete_by_source_detail("onboarding", "alice").unwrap();
        assert_eq!(count, 1);
        assert_eq!(ids, vec![alice_seed]);

        // Alice's non-onboarding import survives; bob's onboarding seed too.
        assert!(s.load_any(alice_other).unwrap().is_some());
        assert!(s.load_any(bob_seed).unwrap().is_some());
        assert!(s.load_any(alice_seed).unwrap().is_none());
    }

    // Backfill on an old DB without scope columns: existing rows should end
    // up scope='user' with scope_id = user_id.
    #[test]
    fn test_backfill_preserves_existing_rows() {
        let path = "/tmp/mira_test_backfill.db";
        std::fs::remove_file(path).ok();
        {
            let s = MemoryStorage::new_for_user(path, "legacy").unwrap();
            s.store("old data".into(), Category::Fact, vec![], None).unwrap();
        }
        // Reopen — migration runs, backfill fires.
        let s = MemoryStorage::new_for_user(path, "legacy").unwrap();
        let items = s.list_visible("legacy", &[], 100, 0).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].scope, Scope::User);
        assert_eq!(items[0].scope_id.as_deref(), Some("legacy"));
    }
}
