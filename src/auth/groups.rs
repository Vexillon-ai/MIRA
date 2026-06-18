// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/groups.rs
//! Groups — admin-created containers used by the memory system for shared
//! visibility. A user can belong to many groups; a group has many users.
//!
//! Tables live in `auth.db` alongside users so the FKs cascade on user delete.
//! The types and methods here are added as an `impl AuthDb` extension in the
//! same module so callers can reach them through the normal `AuthDb` handle.

use rusqlite::params;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::models::{AuthDb, User, row_to_user};
use crate::MiraError;

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
    pub created_by:  String,
    pub created_at:  i64,
    pub updated_at:  i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewGroup {
    pub name:        String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateGroup {
    pub name:        Option<String>,
    pub description: Option<String>,
}

// ── AuthDb impls ──────────────────────────────────────────────────────────────

impl AuthDb {
    // ---- Groups CRUD --------------------------------------------------------

    pub fn create_group(&self, new: NewGroup, created_by: &str) -> Result<Group, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let trimmed = new.name.trim().to_string();
        if trimmed.is_empty() {
            return Err(MiraError::AuthError("Group name is required".into()));
        }

        let conn = self.conn.lock().unwrap();

        // Case-insensitive duplicate check — mirrors the username policy.
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM groups WHERE LOWER(name) = LOWER(?1)",
            params![trimmed],
            |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if exists > 0 {
            return Err(MiraError::AuthError(format!("Group name already taken: {}", trimmed)));
        }

        conn.execute(
            "INSERT INTO groups (id, name, description, created_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, trimmed, new.description, created_by, now],
        ).map_err(|e| MiraError::DatabaseError(format!("create_group: {}", e)))?;

        Ok(Group {
            id,
            name:        trimmed,
            description: new.description,
            created_by:  created_by.to_owned(),
            created_at:  now,
            updated_at:  now,
        })
    }

    pub fn list_groups(&self) -> Result<Vec<Group>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_by, created_at, updated_at
             FROM groups ORDER BY LOWER(name) ASC",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let rows = stmt.query_map([], row_to_group)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }

    pub fn get_group(&self, id: &str) -> Result<Option<Group>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_by, created_at, updated_at
             FROM groups WHERE id = ?1",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        match stmt.query_row(params![id], row_to_group) {
            Ok(g)                                       => Ok(Some(g)),
            Err(rusqlite::Error::QueryReturnedNoRows)   => Ok(None),
            Err(e)                                      => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn update_group(&self, id: &str, up: UpdateGroup) -> Result<Group, MiraError> {
        let now = Self::now_ms();

        // Pull existing first so we can merge partial updates without a second
        // round-trip; also needed to return the updated row.
        let current = self.get_group(id)?
            .ok_or_else(|| MiraError::NotFound(format!("Group not found: {}", id)))?;

        let new_name = up.name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or(current.name.clone());
        let new_desc = up.description.or(current.description.clone());

        let conn = self.conn.lock().unwrap();
        // Dup-check only if name actually changed.
        if new_name.to_lowercase() != current.name.to_lowercase() {
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM groups WHERE LOWER(name) = LOWER(?1) AND id != ?2",
                params![new_name, id],
                |r| r.get(0),
            ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
            if exists > 0 {
                return Err(MiraError::AuthError(format!("Group name already taken: {}", new_name)));
            }
        }

        conn.execute(
            "UPDATE groups SET name = ?1, description = ?2, updated_at = ?3 WHERE id = ?4",
            params![new_name, new_desc, now, id],
        ).map_err(|e| MiraError::DatabaseError(format!("update_group: {}", e)))?;

        Ok(Group {
            id:          id.to_owned(),
            name:        new_name,
            description: new_desc,
            created_by:  current.created_by,
            created_at:  current.created_at,
            updated_at:  now,
        })
    }

    pub fn delete_group(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute("DELETE FROM groups WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("Group not found: {}", id)));
        }
        Ok(())
    }

    // ---- Membership ---------------------------------------------------------

    pub fn add_member(&self, group_id: &str, user_id: &str, added_by: &str) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        // INSERT OR IGNORE so re-adding is a no-op rather than an error.
        conn.execute(
            "INSERT OR IGNORE INTO group_members (group_id, user_id, added_by, added_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![group_id, user_id, added_by, now],
        ).map_err(|e| MiraError::DatabaseError(format!("add_member: {}", e)))?;
        Ok(())
    }

    pub fn remove_member(&self, group_id: &str, user_id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM group_members WHERE group_id = ?1 AND user_id = ?2",
            params![group_id, user_id],
        ).map_err(|e| MiraError::DatabaseError(format!("remove_member: {}", e)))?;
        Ok(())
    }

    /// All users in a group, ordered by username.
    pub fn list_members(&self, group_id: &str) -> Result<Vec<User>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT u.id, u.username, u.display_name, u.email, u.role, u.is_active,
                    u.created_at, u.updated_at, u.last_login
             FROM users u
             INNER JOIN group_members gm ON gm.user_id = u.id
             WHERE gm.group_id = ?1
             ORDER BY LOWER(u.username) ASC",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let rows = stmt.query_map(params![group_id], row_to_user)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }

    /// All groups a user belongs to, ordered by group name.
    pub fn list_user_groups(&self, user_id: &str) -> Result<Vec<Group>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT g.id, g.name, g.description, g.created_by, g.created_at, g.updated_at
             FROM groups g
             INNER JOIN group_members gm ON gm.group_id = g.id
             WHERE gm.user_id = ?1
             ORDER BY LOWER(g.name) ASC",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let rows = stmt.query_map(params![user_id], row_to_group)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }

    /// Returns just the group ids a user belongs to — the shape callers need
    /// for the memory visibility chokepoint.
    pub fn list_user_group_ids(&self, user_id: &str) -> Result<Vec<String>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT group_id FROM group_members WHERE user_id = ?1",
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let rows = stmt.query_map(params![user_id], |r| r.get::<_, String>(0))
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }

    pub fn is_member(&self, group_id: &str, user_id: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM group_members WHERE group_id = ?1 AND user_id = ?2",
            params![group_id, user_id],
            |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n > 0)
    }
}

// ── Row helper ────────────────────────────────────────────────────────────────

fn row_to_group(row: &rusqlite::Row<'_>) -> rusqlite::Result<Group> {
    Ok(Group {
        id:          row.get(0)?,
        name:        row.get(1)?,
        description: row.get(2)?,
        created_by:  row.get(3)?,
        created_at:  row.get(4)?,
        updated_at:  row.get(5)?,
    })
}
