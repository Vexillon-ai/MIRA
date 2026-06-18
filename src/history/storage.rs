// SPDX-License-Identifier: AGPL-3.0-or-later

// src/history/storage.rs
//! SQLite-backed conversation history store.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use uuid::Uuid;

use crate::history::models::{
    ChannelStats, Conversation, HistoryStats, Message, MessageRole,
    NewConversation, NewMessage,
};
use crate::MiraError;

// ── HistoryStore ──────────────────────────────────────────────────────────────

pub struct HistoryStore {
    conn: Arc<Mutex<Connection>>,
}

impl HistoryStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create history DB dir: {}", e))
            })?;
        }

        let conn = Connection::open(path)
            .map_err(|e| MiraError::DatabaseError(format!("Cannot open history DB: {}", e)))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        // Step 1: create tables (no indexes on columns that might be missing
        // on upgraded DBs — those go in step 3 after the ALTER runs).
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS conversations (
                id               TEXT PRIMARY KEY,
                user_id          TEXT NOT NULL,
                channel          TEXT NOT NULL,
                title            TEXT,
                model            TEXT,
                provider         TEXT,
                created_at       INTEGER NOT NULL,
                updated_at       INTEGER NOT NULL,
                external_user_id TEXT,
                mode             TEXT NOT NULL DEFAULT 'chat',
                skip_wiki        INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_conv_user ON conversations(user_id, updated_at DESC);

            CREATE TABLE IF NOT EXISTS messages (
                id               TEXT PRIMARY KEY,
                conversation_id  TEXT NOT NULL,
                role             TEXT NOT NULL,
                content          TEXT NOT NULL,
                content_type     TEXT NOT NULL DEFAULT 'text',
                token_count      INTEGER,
                model            TEXT,
                tool_calls       TEXT,
                created_at       INTEGER NOT NULL,
                metadata         TEXT,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_msg_conv ON messages(conversation_id, created_at ASC);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("History DB migration failed: {}", e)))?;

        // Step 2: idempotent column adds for DBs created by older builds. SQLite
        // returns "duplicate column name" when the column already exists; we
        // swallow only that specific error so a real failure still surfaces.
        let add_column = |sql: &str| -> Result<(), MiraError> {
            match conn.execute(sql, []) {
                Ok(_)  => Ok(()),
                Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
                    if msg.contains("duplicate column") => Ok(()),
                Err(e) => Err(MiraError::DatabaseError(format!(
                    "History DB column add failed: {}", e
                ))),
            }
        };
        add_column("ALTER TABLE conversations ADD COLUMN external_user_id TEXT")?;
        add_column("ALTER TABLE conversations ADD COLUMN mode TEXT NOT NULL DEFAULT 'chat'")?;
        // Slice H — per-conversation toggle: when 1, the chat handler
        // sets `TurnContext.skip_wiki_hooks = true` so this thread
        // doesn't see wiki context injection. Default 0 = wiki on.
        add_column("ALTER TABLE conversations ADD COLUMN skip_wiki INTEGER NOT NULL DEFAULT 0")?;

        // Step 3: indexes that reference columns added in step 2. Safe on both
        // fresh and upgraded databases now that the column is guaranteed.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_conv_external
                ON conversations(user_id, channel, external_user_id);"
        )
        .map_err(|e| MiraError::DatabaseError(format!("History DB index creation failed: {}", e)))?;

        // Step 4: message vector index table. Stores one embedding per
        // indexable message so the `recall_history` tool can do semantic
        // search across the transcript. `user_id` is denormalised from
        // `conversations.user_id` at insert time for single-table scoping
        // on read. Cascade deletes keep it in sync when messages or whole
        // conversations are removed.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS message_vectors (
                message_id      TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                user_id         TEXT NOT NULL,
                role            TEXT NOT NULL,
                created_at      INTEGER NOT NULL,
                dim             INTEGER NOT NULL,
                model           TEXT NOT NULL,
                vector          BLOB NOT NULL,
                FOREIGN KEY (message_id)      REFERENCES messages(id)      ON DELETE CASCADE,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_msgvec_user_date
                ON message_vectors(user_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_msgvec_conv
                ON message_vectors(conversation_id);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!(
            "History DB message_vectors migration failed: {}", e
        )))?;

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    // ── Conversations ─────────────────────────────────────────────────────────

    pub fn create_conversation(&self, req: NewConversation) -> Result<Conversation, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = now_ms();

        let mode = req.mode.clone().unwrap_or_else(|| "chat".to_owned());

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO conversations
                (id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
            params![
                id, req.user_id, req.channel, req.title, req.model, req.provider,
                now, req.external_user_id, mode,
            ],
        )
        .map_err(|e| MiraError::HistoryError(format!("create_conversation: {}", e)))?;

        Ok(Conversation {
            id,
            user_id:          req.user_id,
            channel:          req.channel,
            title:            req.title,
            model:            req.model,
            provider:         req.provider,
            created_at:       now,
            updated_at:       now,
            external_user_id: req.external_user_id,
            mode,
            skip_wiki:        false,
        })
    }

    /// Lookup-or-create for inbound bridges. Keys on
    /// `(user_id = owner, channel, external_user_id = sender)` so each
    /// external sender gets their own thread under the owning user. Used by
    /// the Signal SSE listener and Telegram webhook so multiple senders to
    /// the same account don't collapse into one giant conversation.
    ///
    /// `default_title` is only applied when a new row is created.
    pub fn find_or_create_external_conversation(
        &self,
        owner_user_id: &str,
        channel:       &str,
        sender:        &str,
        default_title: Option<&str>,
    ) -> Result<Conversation, MiraError> {
        // Most-recent thread for this sender under this owner/channel.
        {
            let conn = self.conn.lock().unwrap();
            let lookup = conn.query_row(
                "SELECT id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode, skip_wiki
                 FROM conversations
                 WHERE user_id = ?1 AND channel = ?2 AND external_user_id = ?3
                 ORDER BY updated_at DESC LIMIT 1",
                params![owner_user_id, channel, sender],
                row_to_conversation,
            );
            match lookup {
                Ok(c) => return Ok(c),
                Err(rusqlite::Error::QueryReturnedNoRows) => {}
                Err(e) => return Err(MiraError::HistoryError(e.to_string())),
            }
        }

        // No existing thread — create one. Drop the lock above before the
        // create call, which re-acquires it internally.
        self.create_conversation(NewConversation {
            user_id:          owner_user_id.to_owned(),
            channel:          channel.to_owned(),
            title:            default_title.map(str::to_owned),
            model:            None,
            provider:         None,
            external_user_id: Some(sender.to_owned()),
            mode:             None,
        })
    }

    pub fn get_conversation(&self, id: &str) -> Result<Option<Conversation>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode, skip_wiki
             FROM conversations WHERE id = ?1",
            params![id],
            row_to_conversation,
        );
        match result {
            Ok(c)                               => Ok(Some(c)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                              => Err(MiraError::HistoryError(e.to_string())),
        }
    }

    /// List all conversations across all users (admin-only use).
    /// Regular users should use [`Self::list_conversations`] which filters by user_id.
    pub fn list_all_conversations(
        &self,
        channel: Option<&str>,
        limit:   i64,
        offset:  i64,
    ) -> Result<Vec<Conversation>, MiraError> {
        let conn = self.conn.lock().unwrap();

        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(ch) = channel {
            (
                "SELECT id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode, skip_wiki
                 FROM conversations WHERE channel = ?1
                 ORDER BY updated_at DESC LIMIT ?2 OFFSET ?3".to_owned(),
                vec![
                    Box::new(ch.to_owned()),
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
        } else {
            (
                "SELECT id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode, skip_wiki
                 FROM conversations
                 ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2".to_owned(),
                vec![
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
        };

        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())), row_to_conversation)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut convs = Vec::new();
        for r in rows {
            convs.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        Ok(convs)
    }

    pub fn list_conversations(
        &self,
        user_id: &str,
        channel: Option<&str>,
        limit:   i64,
        offset:  i64,
    ) -> Result<Vec<Conversation>, MiraError> {
        let conn = self.conn.lock().unwrap();

        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(ch) = channel {
            (
                "SELECT id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode, skip_wiki
                 FROM conversations WHERE user_id = ?1 AND channel = ?2
                 ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4".to_owned(),
                vec![
                    Box::new(user_id.to_owned()),
                    Box::new(ch.to_owned()),
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
        } else {
            (
                "SELECT id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode, skip_wiki
                 FROM conversations WHERE user_id = ?1
                 ORDER BY updated_at DESC LIMIT ?2 OFFSET ?3".to_owned(),
                vec![
                    Box::new(user_id.to_owned()),
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
        };

        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())), row_to_conversation)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut convs = Vec::new();
        for r in rows {
            convs.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        Ok(convs)
    }

    /// List conversations a non-admin web user should see in their sidebar:
    /// strictly their own (`user_id = ?`). Inbound-bridge traffic
    /// (Signal/Telegram) is per-user now — each account is owned by the
    /// user who configured it, so the bridge writes conversations with
    /// that user's id as `user_id` and the normal ownership filter
    /// applies without a special case.
    pub fn list_visible_conversations(
        &self,
        user_id: &str,
        channel: Option<&str>,
        limit:   i64,
        offset:  i64,
    ) -> Result<Vec<Conversation>, MiraError> {
        let conn = self.conn.lock().unwrap();

        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(ch) = channel {
            (
                "SELECT id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode, skip_wiki
                 FROM conversations
                 WHERE channel = ?1 AND user_id = ?2
                 ORDER BY updated_at DESC LIMIT ?3 OFFSET ?4".to_owned(),
                vec![
                    Box::new(ch.to_owned()),
                    Box::new(user_id.to_owned()),
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
        } else {
            (
                "SELECT id, user_id, channel, title, model, provider, created_at, updated_at, external_user_id, mode, skip_wiki
                 FROM conversations
                 WHERE user_id = ?1
                 ORDER BY updated_at DESC LIMIT ?2 OFFSET ?3".to_owned(),
                vec![
                    Box::new(user_id.to_owned()),
                    Box::new(limit),
                    Box::new(offset),
                ],
            )
        };

        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())), row_to_conversation)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut convs = Vec::new();
        for r in rows {
            convs.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        Ok(convs)
    }

    pub fn update_conversation_title(&self, id: &str, title: &str) -> Result<(), MiraError> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE conversations SET title=?1, updated_at=?2 WHERE id=?3",
            params![title, now, id],
        )
        .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("Conversation not found: {}", id)));
        }
        Ok(())
    }

    /// Slice H — per-conversation wiki toggle. When `skip = true`, the
    /// chat handler will set `TurnContext.skip_wiki_hooks` on every
    /// subsequent turn in this thread.
    pub fn update_conversation_skip_wiki(&self, id: &str, skip: bool) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE conversations SET skip_wiki=?1 WHERE id=?2",
            params![if skip { 1i64 } else { 0i64 }, id],
        ).map_err(|e| MiraError::HistoryError(e.to_string()))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("Conversation not found: {}", id)));
        }
        Ok(())
    }

    pub fn touch_conversation(&self, id: &str) -> Result<(), MiraError> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET updated_at=?1 WHERE id=?2",
            params![now, id],
        )
        .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        Ok(())
    }

    pub fn delete_conversation(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute("DELETE FROM conversations WHERE id = ?1", params![id])
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("Conversation not found: {}", id)));
        }
        Ok(())
    }

    /// One-shot re-stamp of legacy conversations onto a real user id. Used by
    /// the channel-accounts migrator on first run: any row whose `user_id`
    /// equals `from` on the given `channel` gets rewritten to `to`. Returns
    /// the number of rows affected.
    pub fn reassign_channel_conversations(
        &self,
        from_user_id: &str,
        to_user_id:   &str,
        channel:      &str,
    ) -> Result<usize, MiraError> {
        if from_user_id == to_user_id {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE conversations SET user_id=?1 WHERE user_id=?2 AND channel=?3",
            params![to_user_id, from_user_id, channel],
        )
        .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        Ok(n)
    }

    // ── Messages ──────────────────────────────────────────────────────────────

    pub fn add_message(&self, req: NewMessage) -> Result<Message, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = now_ms();
        let role_str = req.role.as_str().to_owned();

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content, content_type, token_count, model, tool_calls, created_at, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id, req.conversation_id, role_str, req.content, req.content_type,
                req.token_count, req.model, req.tool_calls, now, req.metadata,
            ],
        )
        .map_err(|e| MiraError::HistoryError(format!("add_message: {}", e)))?;

        Ok(Message {
            id,
            conversation_id: req.conversation_id,
            role:            req.role,
            content:         req.content,
            content_type:    req.content_type,
            token_count:     req.token_count,
            model:           req.model,
            tool_calls:      req.tool_calls,
            created_at:      now,
            metadata:        req.metadata,
        })
    }

    pub fn get_messages(
        &self,
        conversation_id: &str,
        limit:           i64,
        before_id:       Option<&str>,
    ) -> Result<Vec<Message>, MiraError> {
        let conn = self.conn.lock().unwrap();

        let (sql, p): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(bid) = before_id {
            // Cursor-based pagination: get messages older than `before_id`.
            (
                "SELECT id, conversation_id, role, content, content_type, token_count, model, tool_calls, created_at, metadata
                 FROM messages
                 WHERE conversation_id = ?1 AND created_at < (SELECT created_at FROM messages WHERE id = ?2)
                 ORDER BY created_at ASC LIMIT ?3".to_owned(),
                vec![
                    Box::new(conversation_id.to_owned()),
                    Box::new(bid.to_owned()),
                    Box::new(limit),
                ],
            )
        } else {
            (
                "SELECT id, conversation_id, role, content, content_type, token_count, model, tool_calls, created_at, metadata
                 FROM messages WHERE conversation_id = ?1
                 ORDER BY created_at ASC LIMIT ?2".to_owned(),
                vec![
                    Box::new(conversation_id.to_owned()),
                    Box::new(limit),
                ],
            )
        };

        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params_from_iter(p.iter().map(|x| x.as_ref())), row_to_message)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut msgs = Vec::new();
        for r in rows {
            msgs.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        Ok(msgs)
    }

    /// Most-recent `limit` messages for a conversation, returned
    /// oldest→newest (chronological replay order).
    ///
    /// Unlike [`Self::get_messages`] — which applies `LIMIT` to an ASC scan
    /// and so returns the *oldest* N — this takes the **tail** of the
    /// conversation. That is what context rehydration needs: when the
    /// in-memory session cache has been wiped (process restart or idle
    /// eviction) we seed it from the persisted history, and the agent only
    /// keeps the most recent turns.
    pub fn get_recent_messages(
        &self,
        conversation_id: &str,
        limit:           i64,
    ) -> Result<Vec<Message>, MiraError> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, role, content, content_type, token_count, model, tool_calls, created_at, metadata
             FROM messages WHERE conversation_id = ?1
             ORDER BY created_at DESC LIMIT ?2",
        ).map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let rows = stmt
            .query_map(params![conversation_id, limit], row_to_message)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut msgs = Vec::new();
        for r in rows {
            msgs.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        msgs.reverse(); // DESC fetch → chronological (oldest first) for replay
        Ok(msgs)
    }

    pub fn delete_message(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute("DELETE FROM messages WHERE id = ?1", params![id])
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("Message not found: {}", id)));
        }
        Ok(())
    }

    // ── Message vectors ───────────────────────────────────────────────────────

    /// Row returned by [`Self::fetch_unindexed_messages`]. The indexer uses
    /// this minimal projection — full message fields aren't needed at
    /// embedding time and the extra columns would just bloat each batch.
    pub fn fetch_unindexed_messages(
        &self,
        batch_size: i64,
        skip_roles: &[String],
    ) -> Result<Vec<UnindexedMessage>, MiraError> {
        let conn = self.conn.lock().unwrap();

        // Build a parameterised `role NOT IN (?, ?, …)` clause only when
        // `skip_roles` is non-empty. An empty IN-list would be a SQL syntax
        // error, so we branch on the shape instead.
        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::ToSql>>) = if skip_roles.is_empty() {
            (
                "SELECT m.id, m.conversation_id, c.user_id, m.role, m.content, m.created_at
                 FROM messages m
                 INNER JOIN conversations c ON c.id = m.conversation_id
                 LEFT JOIN message_vectors v ON v.message_id = m.id
                 WHERE v.message_id IS NULL
                   AND m.content_type = 'text'
                   AND LENGTH(m.content) > 0
                 ORDER BY m.created_at ASC
                 LIMIT ?1".to_owned(),
                vec![Box::new(batch_size)],
            )
        } else {
            let placeholders: Vec<String> = (0..skip_roles.len())
                .map(|i| format!("?{}", i + 1))
                .collect();
            let limit_idx = skip_roles.len() + 1;
            let sql = format!(
                "SELECT m.id, m.conversation_id, c.user_id, m.role, m.content, m.created_at
                 FROM messages m
                 INNER JOIN conversations c ON c.id = m.conversation_id
                 LEFT JOIN message_vectors v ON v.message_id = m.id
                 WHERE v.message_id IS NULL
                   AND m.content_type = 'text'
                   AND LENGTH(m.content) > 0
                   AND m.role NOT IN ({})
                 ORDER BY m.created_at ASC
                 LIMIT ?{}",
                placeholders.join(","),
                limit_idx,
            );
            let mut p: Vec<Box<dyn rusqlite::ToSql>> = skip_roles
                .iter()
                .map(|r| Box::new(r.clone()) as Box<dyn rusqlite::ToSql>)
                .collect();
            p.push(Box::new(batch_size));
            (sql, p)
        };

        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())),
                |r| Ok(UnindexedMessage {
                    message_id:      r.get(0)?,
                    conversation_id: r.get(1)?,
                    user_id:         r.get(2)?,
                    role:            r.get(3)?,
                    content:         r.get(4)?,
                    created_at:      r.get(5)?,
                }),
            )
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        Ok(out)
    }

    /// Insert or replace a vector row. Uses INSERT OR REPLACE so a retry
    /// after an interrupted batch doesn't fail on PK conflict. The embedding
    /// `model` is stored so the indexer can re-embed if it ever changes.
    pub fn insert_message_vector(
        &self,
        row: &MessageVectorRow<'_>,
    ) -> Result<(), MiraError> {
        if row.vector.len() != row.dim {
            return Err(MiraError::HistoryError(format!(
                "insert_message_vector: vector length {} != declared dim {}",
                row.vector.len(), row.dim,
            )));
        }
        let blob = vec_to_blob(row.vector);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO message_vectors
                (message_id, conversation_id, user_id, role, created_at, dim, model, vector)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.message_id, row.conversation_id, row.user_id, row.role,
                row.created_at, row.dim as i64, row.model, blob,
            ],
        )
        .map_err(|e| MiraError::HistoryError(format!("insert_message_vector: {}", e)))?;
        Ok(())
    }

    /// Count indexed vs. indexable messages (status endpoint helper).
    pub fn message_vector_counts(&self) -> Result<(i64, i64), MiraError> {
        let conn = self.conn.lock().unwrap();
        let indexed: i64 = conn
            .query_row("SELECT COUNT(*) FROM message_vectors", [], |r| r.get(0))
            .unwrap_or(0);
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE content_type = 'text' AND LENGTH(content) > 0",
                [], |r| r.get(0),
            )
            .unwrap_or(0);
        Ok((indexed, total))
    }

    /// Brute-force semantic search across the user's indexed transcript.
    ///
    /// Loads every vector scoped to `user_id` (optionally windowed by
    /// `created_at`) into memory, computes cosine similarity against
    /// `query_vec`, and returns the top-K message IDs + scores. Optional
    /// date bounds are epoch-ms; pass `None` for open-ended.
    ///
    /// Scoping is strict: only rows owned by `user_id` are searched. The
    /// tool that exposes this surface is expected to be called on behalf
    /// of the authenticated caller, so there's no cross-user spill.
    pub fn search_message_vectors(
        &self,
        query_vec: &[f32],
        user_id:   &str,
        top_k:     usize,
        since_ms:  Option<i64>,
        until_ms:  Option<i64>,
    ) -> Result<Vec<MessageVectorHit>, MiraError> {
        let conn = self.conn.lock().unwrap();

        // Build a flexible WHERE clause depending on which bounds are set.
        let mut sql = String::from(
            "SELECT message_id, conversation_id, role, created_at, dim, vector
             FROM message_vectors
             WHERE user_id = ?1"
        );
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(user_id.to_owned())];
        if let Some(s) = since_ms {
            sql.push_str(&format!(" AND created_at >= ?{}", p.len() + 1));
            p.push(Box::new(s));
        }
        if let Some(u) = until_ms {
            sql.push_str(&format!(" AND created_at <= ?{}", p.len() + 1));
            p.push(Box::new(u));
        }

        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(p.iter().map(|b| b.as_ref())),
            |r| {
                let dim: i64 = r.get(4)?;
                let blob: Vec<u8> = r.get(5)?;
                Ok((
                    r.get::<_, String>(0)?,            // message_id
                    r.get::<_, String>(1)?,            // conversation_id
                    r.get::<_, String>(2)?,            // role
                    r.get::<_, i64>(3)?,               // created_at
                    dim as usize,
                    blob,
                ))
            },
        ).map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut scored: Vec<MessageVectorHit> = Vec::new();
        for r in rows {
            let (mid, cid, role, ts, dim, blob) =
                r.map_err(|e| MiraError::HistoryError(e.to_string()))?;
            if dim != query_vec.len() { continue; } // model changed — skip silently
            let v = blob_to_vec(&blob);
            let score = cosine_similarity(query_vec, &v);
            scored.push(MessageVectorHit {
                message_id:      mid,
                conversation_id: cid,
                role,
                created_at:      ts,
                score,
            });
        }
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        Ok(scored)
    }

    /// Fetch messages by ID (for hydrating a search result). Returns rows in
    /// the order supplied; missing IDs are silently dropped.
    pub fn get_messages_by_ids(
        &self,
        ids: &[String],
    ) -> Result<Vec<Message>, MiraError> {
        if ids.is_empty() { return Ok(Vec::new()); }
        let conn = self.conn.lock().unwrap();
        let placeholders: Vec<String> = (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let sql = format!(
            "SELECT id, conversation_id, role, content, content_type, token_count, model, tool_calls, created_at, metadata
             FROM messages WHERE id IN ({})",
            placeholders.join(","),
        );
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        let params: Vec<Box<dyn rusqlite::ToSql>> = ids.iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            row_to_message,
        ).map_err(|e| MiraError::HistoryError(e.to_string()))?;

        // Preserve caller-supplied order by looking each up after loading.
        let mut loaded: std::collections::HashMap<String, Message> =
            std::collections::HashMap::new();
        for r in rows {
            let m = r.map_err(|e| MiraError::HistoryError(e.to_string()))?;
            loaded.insert(m.id.clone(), m);
        }
        Ok(ids.iter().filter_map(|id| loaded.remove(id)).collect())
    }

    /// Distinct user IDs that have at least one message in `[start_ms, end_ms)`.
    ///
    /// Powers the daily rollup job: we only need to consolidate days where a
    /// given user actually spoke, so enumerate them instead of iterating over
    /// every registered account.
    pub fn distinct_users_with_messages_between(
        &self,
        start_ms: i64,
        end_ms:   i64,
    ) -> Result<Vec<String>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT c.user_id
             FROM messages m
             INNER JOIN conversations c ON c.id = m.conversation_id
             WHERE m.created_at >= ?1 AND m.created_at < ?2
             ORDER BY c.user_id ASC",
        ).map_err(|e| MiraError::HistoryError(e.to_string()))?;
        let rows = stmt.query_map(params![start_ms, end_ms], |r| r.get::<_, String>(0))
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        Ok(out)
    }

    /// Messages for `user_id` in the half-open window `[start_ms, end_ms)`,
    /// optionally filtered to specific roles. Ordered chronologically so the
    /// caller can feed them straight into a summarizer prompt.
    pub fn user_messages_between(
        &self,
        user_id:       &str,
        start_ms:      i64,
        end_ms:        i64,
        allowed_roles: &[&str],
        limit:         i64,
    ) -> Result<Vec<Message>, MiraError> {
        let conn = self.conn.lock().unwrap();

        let (sql, p): (String, Vec<Box<dyn rusqlite::ToSql>>) = if allowed_roles.is_empty() {
            (
                "SELECT m.id, m.conversation_id, m.role, m.content, m.content_type,
                        m.token_count, m.model, m.tool_calls, m.created_at, m.metadata
                 FROM messages m
                 INNER JOIN conversations c ON c.id = m.conversation_id
                 WHERE c.user_id = ?1
                   AND m.created_at >= ?2 AND m.created_at < ?3
                   AND m.content_type = 'text'
                 ORDER BY m.created_at ASC
                 LIMIT ?4".to_owned(),
                vec![
                    Box::new(user_id.to_owned()),
                    Box::new(start_ms),
                    Box::new(end_ms),
                    Box::new(limit),
                ],
            )
        } else {
            let placeholders: Vec<String> = (0..allowed_roles.len())
                .map(|i| format!("?{}", i + 4))
                .collect();
            let limit_idx = allowed_roles.len() + 4;
            let sql = format!(
                "SELECT m.id, m.conversation_id, m.role, m.content, m.content_type,
                        m.token_count, m.model, m.tool_calls, m.created_at, m.metadata
                 FROM messages m
                 INNER JOIN conversations c ON c.id = m.conversation_id
                 WHERE c.user_id = ?1
                   AND m.created_at >= ?2 AND m.created_at < ?3
                   AND m.content_type = 'text'
                   AND m.role IN ({})
                 ORDER BY m.created_at ASC
                 LIMIT ?{}",
                placeholders.join(","),
                limit_idx,
            );
            let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(user_id.to_owned()),
                Box::new(start_ms),
                Box::new(end_ms),
            ];
            for role in allowed_roles {
                p.push(Box::new((*role).to_owned()));
            }
            p.push(Box::new(limit));
            (sql, p)
        };

        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(p.iter().map(|x| x.as_ref())),
            row_to_message,
        ).map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        Ok(out)
    }

    /// Convenience: append user + assistant messages and touch the conversation.
    pub fn record_turn(
        &self,
        conv_id:           &str,
        user_content:      &str,
        assistant_content: &str,
        model:             Option<&str>,
        token_count:       Option<i32>,
    ) -> Result<(), MiraError> {
        self.add_message(NewMessage {
            conversation_id: conv_id.to_owned(),
            role:            MessageRole::User,
            content:         user_content.to_owned(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        None,
        })?;

        self.add_message(NewMessage {
            conversation_id: conv_id.to_owned(),
            role:            MessageRole::Assistant,
            content:         assistant_content.to_owned(),
            content_type:    "text".to_owned(),
            token_count,
            model:           model.map(str::to_owned),
            tool_calls:      None,
            metadata:        None,
        })?;

        self.touch_conversation(conv_id)?;
        Ok(())
    }

    /// Aggregate history stats for the conversations page.
    ///
    /// Pass `None` for `user_id` to get an admin-scope view (all rows); pass
    /// `Some(uid)` for a regular user — visibility matches
    /// [`Self::list_visible_conversations`] (strictly their own rows).
    ///
    /// Tokens are approximate: `token_count` when set, else
    /// `LENGTH(content) / 4` as a rough fallback.
    pub fn history_stats(&self, user_id: Option<&str>) -> Result<HistoryStats, MiraError> {
        let conn = self.conn.lock().unwrap();

        // Visibility filter — always appended with AND so callers can safely
        // combine it with their own WHERE clauses. We rely on positional
        // params (`?1`) so every query binds `user_id` the same way.
        let vis: &str = match user_id {
            Some(_) => "AND c.user_id = ?1",
            None    => "",
        };
        // `?1` is reused across every occurrence of the visibility filter,
        // so we only ever bind `user_id` once per query.
        let uid_params: Vec<Box<dyn rusqlite::ToSql>> = match user_id {
            Some(uid) => vec![Box::new(uid.to_owned())],
            None      => Vec::new(),
        };

        // ── Query 1: totals + role breakdown + date range ─────────────────
        let sql_totals = format!(
            "SELECT
                (SELECT COUNT(*) FROM conversations c WHERE 1=1 {vis})                              AS total_convs,
                COUNT(m.id)                                                                          AS total_msgs,
                COALESCE(SUM(CASE WHEN m.role = 'user'      THEN 1 ELSE 0 END), 0)                   AS user_msgs,
                COALESCE(SUM(CASE WHEN m.role = 'assistant' THEN 1 ELSE 0 END), 0)                   AS asst_msgs,
                COALESCE(SUM(CASE WHEN m.role = 'tool'      THEN 1 ELSE 0 END), 0)                   AS tool_msgs,
                COALESCE(SUM(COALESCE(m.token_count, CAST(LENGTH(m.content)/4 AS INTEGER))), 0)      AS tokens,
                MIN(m.created_at)                                                                     AS first_at,
                MAX(m.created_at)                                                                     AS last_at
             FROM messages m
             INNER JOIN conversations c ON c.id = m.conversation_id
             WHERE 1=1 {vis}",
            vis = vis,
        );

        let (total_convs, total_msgs, user_msgs, asst_msgs, tool_msgs, tokens, first_at, last_at):
            (i64, i64, i64, i64, i64, i64, Option<i64>, Option<i64>) =
            conn.query_row(
                &sql_totals,
                rusqlite::params_from_iter(uid_params.iter().map(|p| p.as_ref())),
                |r| Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?,
                    r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?,
                )),
            )
            .map_err(|e| MiraError::HistoryError(format!("stats totals: {}", e)))?;

        // ── Query 2: per-channel breakdown ────────────────────────────────
        let sql_channels = format!(
            "SELECT
                c.channel,
                COUNT(DISTINCT c.id)                                                               AS convs,
                COUNT(m.id)                                                                         AS msgs,
                COALESCE(SUM(COALESCE(m.token_count, CAST(LENGTH(m.content)/4 AS INTEGER))), 0)     AS tokens
             FROM conversations c
             LEFT JOIN messages m ON m.conversation_id = c.id
             WHERE 1=1 {vis}
             GROUP BY c.channel
             ORDER BY msgs DESC, convs DESC",
            vis = vis,
        );

        let mut stmt = conn.prepare(&sql_channels)
            .map_err(|e| MiraError::HistoryError(e.to_string()))?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(uid_params.iter().map(|p| p.as_ref())),
            |r| Ok(ChannelStats {
                channel:       r.get(0)?,
                conversations: r.get(1)?,
                messages:      r.get(2)?,
                tokens:        r.get(3)?,
            }),
        ).map_err(|e| MiraError::HistoryError(e.to_string()))?;

        let mut per_channel = Vec::new();
        for r in rows {
            per_channel.push(r.map_err(|e| MiraError::HistoryError(e.to_string()))?);
        }
        drop(stmt);

        // ── Query 3: top model (most assistant/tool messages) ─────────────
        let sql_model = format!(
            "SELECT m.model, COUNT(*) AS n
             FROM messages m
             INNER JOIN conversations c ON c.id = m.conversation_id
             WHERE m.model IS NOT NULL AND m.model != '' {vis}
             GROUP BY m.model
             ORDER BY n DESC
             LIMIT 1",
            vis = vis,
        );

        let top_model: Option<String> = conn
            .query_row(
                &sql_model,
                rusqlite::params_from_iter(uid_params.iter().map(|p| p.as_ref())),
                |r| r.get::<_, String>(0),
            )
            .ok();

        Ok(HistoryStats {
            total_conversations: total_convs,
            total_messages:      total_msgs,
            user_messages:       user_msgs,
            assistant_messages:  asst_msgs,
            tool_messages:       tool_msgs,
            estimated_tokens:    tokens,
            per_channel,
            top_model,
            first_message_at:    first_at,
            last_message_at:     last_at,
        })
    }

    /// Return (conversation_count, message_count) for the status endpoint.
    pub fn stats(&self) -> Result<(Option<usize>, Option<usize>), MiraError> {
        let conn = self.conn.lock().unwrap();
        let convs: usize = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap_or(0);
        let msgs: usize = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap_or(0);
        Ok((Some(convs), Some(msgs)))
    }
}

// ── Message-vector types ──────────────────────────────────────────────────────

/// Projection returned by [`HistoryStore::fetch_unindexed_messages`].
#[derive(Debug, Clone)]
pub struct UnindexedMessage {
    pub message_id:      String,
    pub conversation_id: String,
    pub user_id:         String,
    pub role:            String,
    pub content:         String,
    pub created_at:      i64,
}

/// Parameters for [`HistoryStore::insert_message_vector`]. Borrowed so the
/// caller can reuse buffers across a batch without cloning.
pub struct MessageVectorRow<'a> {
    pub message_id:      &'a str,
    pub conversation_id: &'a str,
    pub user_id:         &'a str,
    pub role:            &'a str,
    pub created_at:      i64,
    pub dim:             usize,
    pub model:           &'a str,
    pub vector:          &'a [f32],
}

/// Result row from [`HistoryStore::search_message_vectors`]. Content is not
/// inlined — the caller fetches full messages through
/// [`HistoryStore::get_messages_by_ids`] once it's picked the set to show.
#[derive(Debug, Clone)]
pub struct MessageVectorHit {
    pub message_id:      String,
    pub conversation_id: String,
    pub role:            String,
    pub created_at:      i64,
    pub score:           f32,
}

// ── Blob + math helpers ───────────────────────────────────────────────────────

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let mut dot = 0.0f32;
    let mut na  = 0.0f32;
    let mut nb  = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na  += a[i] * a[i];
        nb  += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

// ── Row helpers ───────────────────────────────────────────────────────────────

fn row_to_conversation(row: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    // skip_wiki is optional in the SELECT — most existing queries pull
    // 10 columns; the ones that need the toggle (get / list) pull 11.
    // Treat InvalidColumnIndex as "column not requested" and default
    // to false. Any other error propagates.
    let skip_wiki = match row.get::<_, i64>(10) {
        Ok(v) => v != 0,
        Err(rusqlite::Error::InvalidColumnIndex(_)) => false,
        Err(e) => return Err(e),
    };
    Ok(Conversation {
        id:               row.get(0)?,
        user_id:          row.get(1)?,
        channel:          row.get(2)?,
        title:            row.get(3)?,
        model:            row.get(4)?,
        provider:         row.get(5)?,
        created_at:       row.get(6)?,
        updated_at:       row.get(7)?,
        external_user_id: row.get(8)?,
        mode:             row.get::<_, Option<String>>(9)?.unwrap_or_else(|| "chat".to_owned()),
        skip_wiki,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    use std::str::FromStr;
    let role_str: String = row.get(2)?;
    let role = MessageRole::from_str(&role_str).unwrap_or(MessageRole::User);
    Ok(Message {
        id:              row.get(0)?,
        conversation_id: row.get(1)?,
        role,
        content:         row.get(3)?,
        content_type:    row.get(4)?,
        token_count:     row.get(5)?,
        model:           row.get(6)?,
        tool_calls:      row.get(7)?,
        created_at:      row.get(8)?,
        metadata:        row.get(9)?,
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn seed(store: &HistoryStore, user_id: &str, channel: &str) -> String {
        store
            .create_conversation(NewConversation {
                user_id:          user_id.to_owned(),
                channel:          channel.to_owned(),
                title:            None,
                model:            None,
                provider:         None,
                external_user_id: None,
                mode:             None,
            })
            .expect("seed conversation")
            .id
    }

    #[test]
    fn migration_is_idempotent() {
        let dir  = tempdir().unwrap();
        let path = dir.path().join("history.db");
        // Opening twice must not error — the second open re-runs the ALTERs
        // and must treat "duplicate column" as a no-op.
        let _first  = HistoryStore::open(&path).unwrap();
        let _second = HistoryStore::open(&path).unwrap();
    }

    #[test]
    fn conversation_mode_defaults_to_chat_and_round_trips_onboarding() {
        let dir   = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("h.db")).unwrap();

        let default_id = seed(&store, "u", "web");
        let default_c  = store.get_conversation(&default_id).unwrap().unwrap();
        assert_eq!(default_c.mode, "chat");

        let onboarding = store
            .create_conversation(NewConversation {
                user_id: "u".to_owned(),
                channel: "web".to_owned(),
                title:   None,
                model:   None,
                provider: None,
                external_user_id: None,
                mode:    Some("onboarding".to_owned()),
            })
            .unwrap();
        assert_eq!(onboarding.mode, "onboarding");

        let refetched = store.get_conversation(&onboarding.id).unwrap().unwrap();
        assert_eq!(refetched.mode, "onboarding");
    }

    #[test]
    fn list_visible_is_strictly_scoped_to_user() {
        let dir   = tempdir().unwrap();
        let path  = dir.path().join("history.db");
        let store = HistoryStore::open(&path).unwrap();

        // Alice owns: a web chat, a signal account, a telegram account.
        // Bob owns: an unrelated web chat plus his own signal account.
        let alice_web      = seed(&store, "web-alice", "web");
        let alice_signal   = seed(&store, "web-alice", "signal");
        let alice_telegram = seed(&store, "web-alice", "telegram");
        let bob_web        = seed(&store, "web-bob",   "web");
        let bob_signal     = seed(&store, "web-bob",   "signal");

        let visible: Vec<String> = store
            .list_visible_conversations("web-alice", None, 50, 0)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();

        assert!(visible.contains(&alice_web));
        assert!(visible.contains(&alice_signal));
        assert!(visible.contains(&alice_telegram));
        assert!(!visible.contains(&bob_web),    "bob's web chat must stay hidden");
        assert!(!visible.contains(&bob_signal), "bob's signal account must stay hidden");
    }

    #[test]
    fn history_stats_counts_messages_and_tokens_per_visibility() {
        let dir   = tempdir().unwrap();
        let path  = dir.path().join("history.db");
        let store = HistoryStore::open(&path).unwrap();

        // Alice owns a web chat AND her own signal account; bob is unrelated.
        let alice_web    = seed(&store, "web-alice", "web");
        let alice_signal = seed(&store, "web-alice", "signal");
        let bob_web      = seed(&store, "web-bob",   "web");

        store.record_turn(&alice_web,    "hi",  "hello back", Some("gpt-4"), Some(12)).unwrap();
        store.record_turn(&alice_web,    "ok?", "yes",        Some("gpt-4"), Some(3)).unwrap();
        store.record_turn(&alice_signal, "yo",  "yo reply",   None,          None).unwrap();
        // bob's conversation should never show up in alice's stats.
        store.record_turn(&bob_web,      "hidden", "hidden reply", Some("other"), Some(100)).unwrap();

        let stats = store.history_stats(Some("web-alice")).unwrap();

        // 2 alice conversations (web + signal). Bob's row is hidden.
        assert_eq!(stats.total_conversations, 2);
        assert_eq!(stats.total_messages,      6);  // 3 turns × 2 rows
        assert_eq!(stats.user_messages,       3);
        assert_eq!(stats.assistant_messages,  3);
        assert!(stats.estimated_tokens > 0, "expected a positive token estimate");
        assert_eq!(stats.top_model.as_deref(), Some("gpt-4"));

        // Channels: web + signal visible (both alice-owned), no bob row.
        let channels: Vec<&str> = stats.per_channel.iter().map(|c| c.channel.as_str()).collect();
        assert!(channels.contains(&"web"));
        assert!(channels.contains(&"signal"));
    }

    #[test]
    fn history_stats_admin_scope_sees_everything() {
        let dir   = tempdir().unwrap();
        let path  = dir.path().join("history.db");
        let store = HistoryStore::open(&path).unwrap();

        let alice = seed(&store, "web-alice", "web");
        let bob   = seed(&store, "web-bob",   "web");
        store.record_turn(&alice, "a", "A", None, None).unwrap();
        store.record_turn(&bob,   "b", "B", None, None).unwrap();

        let stats = store.history_stats(None).unwrap();
        assert_eq!(stats.total_conversations, 2);
        assert_eq!(stats.total_messages,      4);
    }

    #[test]
    fn find_or_create_external_threads_dedup_per_sender() {
        let dir   = tempdir().unwrap();
        let path  = dir.path().join("history.db");
        let store = HistoryStore::open(&path).unwrap();

        // Owner is the MIRA web user who configured this Signal account;
        // two external senders write in. Each should land in its own thread.
        let sender_a = "+15551111111";
        let sender_b = "+15552222222";

        let a1 = store.find_or_create_external_conversation(
            "web-alice", "signal", sender_a, Some("Hi from A"),
        ).unwrap();
        let a2 = store.find_or_create_external_conversation(
            "web-alice", "signal", sender_a, Some("ignored title"),
        ).unwrap();
        let b1 = store.find_or_create_external_conversation(
            "web-alice", "signal", sender_b, Some("Hi from B"),
        ).unwrap();

        // Same sender → same conversation, default_title only used at create.
        assert_eq!(a1.id, a2.id);
        assert_eq!(a1.title.as_deref(), Some("Hi from A"));
        // Different sender → distinct conversation under the same owner.
        assert_ne!(a1.id, b1.id);
        assert_eq!(a1.user_id, "web-alice");
        assert_eq!(b1.user_id, "web-alice");
        assert_eq!(a1.external_user_id.as_deref(), Some(sender_a));
        assert_eq!(b1.external_user_id.as_deref(), Some(sender_b));

        // A different owner (bob) talking to the same sender gets a fresh
        // thread — no cross-owner leakage.
        let bob_a = store.find_or_create_external_conversation(
            "web-bob", "signal", sender_a, Some("Bob's view"),
        ).unwrap();
        assert_ne!(bob_a.id, a1.id);
        assert_eq!(bob_a.user_id, "web-bob");
    }

    #[test]
    fn list_visible_channel_filter_still_respects_visibility() {
        let dir   = tempdir().unwrap();
        let path  = dir.path().join("history.db");
        let store = HistoryStore::open(&path).unwrap();

        // Alice owns her own signal account; bob owns one too.
        let alice_signal = seed(&store, "web-alice", "signal");
        let _bob_signal  = seed(&store, "web-bob",   "signal");
        let _alice_web   = seed(&store, "web-alice", "web");

        let only_signal = store
            .list_visible_conversations("web-alice", Some("signal"), 50, 0)
            .unwrap();

        assert_eq!(only_signal.len(), 1);
        assert_eq!(only_signal[0].id, alice_signal);
    }

    // ── Message vector tests ──────────────────────────────────────────────

    fn add(
        store: &HistoryStore,
        conv:  &str,
        role:  MessageRole,
        text:  &str,
    ) -> String {
        store.add_message(NewMessage {
            conversation_id: conv.to_owned(),
            role,
            content:         text.to_owned(),
            content_type:    "text".to_owned(),
            token_count:     None,
            model:           None,
            tool_calls:      None,
            metadata:        None,
        }).unwrap().id
    }

    #[test]
    fn fetch_unindexed_respects_skip_roles_and_excludes_indexed() {
        let dir   = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("h.db")).unwrap();
        let conv  = seed(&store, "u", "web");

        let u1 = add(&store, &conv, MessageRole::User,      "what time is it");
        let a1 = add(&store, &conv, MessageRole::Assistant, "around 3pm");
        let _t = add(&store, &conv, MessageRole::Tool,      "{\"tool_result\": 1}");

        // No skips, no indexed rows — all three unstaged (but tool should get
        // picked up too in this branch).
        let all = store.fetch_unindexed_messages(10, &[]).unwrap();
        assert_eq!(all.len(), 3);

        // With 'tool' skipped, only user + assistant come back.
        let skipped = store
            .fetch_unindexed_messages(10, &["tool".to_owned()])
            .unwrap();
        assert_eq!(skipped.len(), 2);
        assert!(skipped.iter().any(|m| m.message_id == u1));
        assert!(skipped.iter().any(|m| m.message_id == a1));

        // Insert a vector for u1; it must drop out of the next fetch.
        let v = vec![0.5f32; 4];
        store.insert_message_vector(&MessageVectorRow {
            message_id:      &u1,
            conversation_id: &conv,
            user_id:         "u",
            role:            "user",
            created_at:      1,
            dim:             4,
            model:           "test",
            vector:          &v,
        }).unwrap();
        let after = store
            .fetch_unindexed_messages(10, &["tool".to_owned()])
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].message_id, a1);
    }

    #[test]
    fn search_message_vectors_scopes_to_user_and_respects_date_window() {
        let dir   = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("h.db")).unwrap();

        let alice_conv = seed(&store, "alice", "web");
        let bob_conv   = seed(&store, "bob",   "web");
        let am = add(&store, &alice_conv, MessageRole::User, "alice says hi");
        let bm = add(&store, &bob_conv,   MessageRole::User, "bob says hi");

        // Alice's message at t=1000, bob's at t=9999.
        let unit = vec![1.0f32, 0.0, 0.0, 0.0];
        store.insert_message_vector(&MessageVectorRow {
            message_id: &am, conversation_id: &alice_conv, user_id: "alice",
            role: "user", created_at: 1000, dim: 4, model: "m", vector: &unit,
        }).unwrap();
        store.insert_message_vector(&MessageVectorRow {
            message_id: &bm, conversation_id: &bob_conv, user_id: "bob",
            role: "user", created_at: 9999, dim: 4, model: "m", vector: &unit,
        }).unwrap();

        // Alice search — only her row is ever considered.
        let hits = store
            .search_message_vectors(&unit, "alice", 5, None, None)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, am);
        assert!((hits[0].score - 1.0).abs() < 1e-5);

        // Date window excludes alice's row (since her ts is 1000).
        let narrow = store
            .search_message_vectors(&unit, "alice", 5, Some(2000), None)
            .unwrap();
        assert!(narrow.is_empty());

        // Wrong dim = no match (search skips mismatches).
        let wrong_dim = vec![1.0, 0.0, 0.0]; // 3 not 4
        let none = store
            .search_message_vectors(&wrong_dim, "alice", 5, None, None)
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn insert_message_vector_rejects_dim_mismatch() {
        let dir   = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("h.db")).unwrap();
        let conv  = seed(&store, "u", "web");
        let mid   = add(&store, &conv, MessageRole::User, "x");

        let v = vec![0.1f32, 0.2, 0.3]; // len 3
        let err = store.insert_message_vector(&MessageVectorRow {
            message_id: &mid, conversation_id: &conv, user_id: "u",
            role: "user", created_at: 1, dim: 5, model: "m", vector: &v,
        });
        assert!(err.is_err(), "dim mismatch should be rejected");
    }

    #[test]
    fn get_messages_by_ids_preserves_caller_order() {
        let dir   = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("h.db")).unwrap();
        let conv  = seed(&store, "u", "web");
        let a     = add(&store, &conv, MessageRole::User,      "first");
        let b     = add(&store, &conv, MessageRole::Assistant, "second");
        let c     = add(&store, &conv, MessageRole::User,      "third");

        let got = store.get_messages_by_ids(
            &[c.clone(), a.clone(), b.clone(), "nope".to_owned()]
        ).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].id, c);
        assert_eq!(got[1].id, a);
        assert_eq!(got[2].id, b);
    }

    #[test]
    fn user_messages_between_filters_by_user_range_and_roles() {
        let dir   = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("h.db")).unwrap();

        let alice_conv = seed(&store, "alice", "web");
        let bob_conv   = seed(&store, "bob",   "web");
        let _a_user     = add(&store, &alice_conv, MessageRole::User,      "a");
        let _a_asst     = add(&store, &alice_conv, MessageRole::Assistant, "a-reply");
        let _a_tool     = add(&store, &alice_conv, MessageRole::Tool,      "{\"t\":1}");
        let _b_user     = add(&store, &bob_conv,   MessageRole::User,      "b");

        // Alice gets only her non-tool messages.
        let alice_msgs = store.user_messages_between(
            "alice", 0, i64::MAX,
            &["user", "assistant"],
            100,
        ).unwrap();
        assert_eq!(alice_msgs.len(), 2);
        assert!(alice_msgs.iter().all(|m| m.role != MessageRole::Tool));

        // Bob only sees his own rows — no cross-user spill.
        let bob_msgs = store.user_messages_between(
            "bob", 0, i64::MAX, &["user", "assistant"], 100,
        ).unwrap();
        assert_eq!(bob_msgs.len(), 1);
        assert_eq!(bob_msgs[0].content, "b");

        // Empty roles → no role filter → tool messages show up too.
        let alice_all = store.user_messages_between(
            "alice", 0, i64::MAX, &[], 100,
        ).unwrap();
        assert_eq!(alice_all.len(), 3);
    }

    #[test]
    fn distinct_users_with_messages_between_deduplicates_and_scopes_by_time() {
        let dir   = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("h.db")).unwrap();

        let alice_conv = seed(&store, "alice", "web");
        let bob_conv   = seed(&store, "bob",   "web");
        // Both users add messages within range, alice adds two to test dedup.
        add(&store, &alice_conv, MessageRole::User, "one");
        add(&store, &alice_conv, MessageRole::User, "two");
        add(&store, &bob_conv,   MessageRole::User, "three");

        let users = store.distinct_users_with_messages_between(0, i64::MAX).unwrap();
        assert_eq!(users.len(), 2, "each user appears once even with multiple msgs");
        assert!(users.contains(&"alice".to_owned()));
        assert!(users.contains(&"bob".to_owned()));

        // Empty window → nothing.
        let none = store.distinct_users_with_messages_between(i64::MAX - 1, i64::MAX).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn message_vectors_cascade_delete_with_messages() {
        let dir   = tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("h.db")).unwrap();
        let conv  = seed(&store, "u", "web");
        let mid   = add(&store, &conv, MessageRole::User, "x");

        let v = vec![0.1f32, 0.2, 0.3, 0.4];
        store.insert_message_vector(&MessageVectorRow {
            message_id: &mid, conversation_id: &conv, user_id: "u",
            role: "user", created_at: 1, dim: 4, model: "m", vector: &v,
        }).unwrap();
        let (indexed, _total) = store.message_vector_counts().unwrap();
        assert_eq!(indexed, 1);

        store.delete_message(&mid).unwrap();
        let (indexed2, _) = store.message_vector_counts().unwrap();
        assert_eq!(indexed2, 0, "FK cascade should drop the vector row");
    }
}
