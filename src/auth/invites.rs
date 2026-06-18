// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/invites.rs
//! Self-service onboarding (Q2 #11) — admin-minted invite tokens + the
//! pending-approval gate for open self-signup. Tables created in `models.rs`.
//!
//! An invite's raw token is shown once at creation; only its SHA-256 hash is
//! stored (same posture as refresh tokens). Redeeming an invite creates an
//! **active, approved** account with the invite's role. Open self-signup
//! (when enabled) instead creates an account that is `approved = 0` until an
//! admin approves it — the login gate refuses unapproved accounts.

use rusqlite::params;
use serde::Serialize;
use uuid::Uuid;

use crate::auth::models::{AuthDb, User, row_to_user};
use crate::auth::tokens::hash_refresh_token;
use crate::MiraError;

/// An invite as stored (never carries the raw token).
#[derive(Debug, Clone, Serialize)]
pub struct Invite {
    pub id:         String,
    pub created_by: String,
    pub role:       String,
    pub email_hint: Option<String>,
    pub max_uses:   i64,
    pub used_count: i64,
    pub expires_at: Option<i64>,
    pub revoked:    bool,
    pub created_at: i64,
}

impl Invite {
    /// Usable right now? (not revoked, not expired, uses remaining)
    pub fn is_redeemable(&self, now_ms: i64) -> bool {
        !self.revoked
            && self.used_count < self.max_uses
            && self.expires_at.map(|e| e > now_ms).unwrap_or(true)
    }
}

fn row_to_invite(row: &rusqlite::Row<'_>) -> rusqlite::Result<Invite> {
    Ok(Invite {
        id:         row.get(0)?,
        created_by: row.get(1)?,
        role:       row.get(2)?,
        email_hint: row.get(3)?,
        max_uses:   row.get(4)?,
        used_count: row.get(5)?,
        expires_at: row.get(6)?,
        revoked:    row.get::<_, i64>(7)? != 0,
        created_at: row.get(8)?,
    })
}

const INVITE_COLS: &str =
    "id, created_by, role, email_hint, max_uses, used_count, expires_at, revoked, created_at";

impl AuthDb {
    /// Mint an invite. Returns the stored row plus the **raw token** (shown
    /// once — only its hash is persisted).
    #[allow(clippy::too_many_arguments)]
    pub fn create_invite(
        &self,
        created_by: &str,
        role:       &str,
        email_hint: Option<&str>,
        max_uses:   i64,
        expires_at: Option<i64>,
        raw_token:  &str,
    ) -> Result<Invite, MiraError> {
        let id  = Uuid::new_v4().to_string();
        let now = Self::now_ms();
        let token_hash = hash_refresh_token(raw_token);
        let max_uses = max_uses.max(1);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO invites
               (id, token_hash, created_by, role, email_hint, max_uses, used_count, expires_at, revoked, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, 0, ?8)",
            params![id, token_hash, created_by, role, email_hint, max_uses, expires_at, now],
        )
        .map_err(|e| MiraError::DatabaseError(format!("create_invite: {e}")))?;
        Ok(Invite {
            id,
            created_by: created_by.to_owned(),
            role:       role.to_owned(),
            email_hint: email_hint.map(str::to_owned),
            max_uses,
            used_count: 0,
            expires_at,
            revoked:    false,
            created_at: now,
        })
    }

    /// All invites, newest first (no raw tokens — those are unrecoverable).
    pub fn list_invites(&self) -> Result<Vec<Invite>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {INVITE_COLS} FROM invites ORDER BY created_at DESC");
        let mut stmt = conn.prepare(&sql).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map([], row_to_invite).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }

    /// Revoke (soft-delete) an invite so it can no longer be redeemed.
    pub fn revoke_invite(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("UPDATE invites SET revoked = 1 WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(format!("revoke_invite: {e}")))?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("invite not found: {id}")));
        }
        Ok(())
    }

    /// Look up an invite by raw token (read-only — used to validate before
    /// showing the signup form). `None` when no such token.
    pub fn find_invite_by_token(&self, raw_token: &str) -> Result<Option<Invite>, MiraError> {
        let token_hash = hash_refresh_token(raw_token);
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {INVITE_COLS} FROM invites WHERE token_hash = ?1");
        let mut stmt = conn.prepare(&sql).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        match stmt.query_row(params![token_hash], row_to_invite) {
            Ok(i)                                     => Ok(Some(i)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                                    => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// Atomically validate + consume one use of an invite. Returns the invite
    /// (carrying its `role`) on success. Errors if the token is unknown,
    /// revoked, expired, or exhausted — so a race can't over-redeem.
    pub fn redeem_invite(&self, raw_token: &str) -> Result<Invite, MiraError> {
        let token_hash = hash_refresh_token(raw_token);
        let now = Self::now_ms();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let invite = {
            let sql = format!("SELECT {INVITE_COLS} FROM invites WHERE token_hash = ?1");
            let mut stmt = tx.prepare(&sql).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
            match stmt.query_row(params![token_hash], row_to_invite) {
                Ok(i)                                     => i,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Err(MiraError::Unauthorized),
                Err(e)                                    => return Err(MiraError::DatabaseError(e.to_string())),
            }
        };
        if !invite.is_redeemable(now) {
            return Err(MiraError::AuthError("This invite is no longer valid.".into()));
        }
        tx.execute("UPDATE invites SET used_count = used_count + 1 WHERE id = ?1", params![invite.id])
            .map_err(|e| MiraError::DatabaseError(format!("redeem_invite: {e}")))?;
        tx.commit().map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(invite)
    }

    // ── Pending-approval gate ────────────────────────────────────────────────

    /// Is this user approved to log in? Unknown user → false.
    pub fn is_user_approved(&self, user_id: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().unwrap();
        match conn.query_row(
            "SELECT approved FROM users WHERE id = ?1",
            params![user_id],
            |r| r.get::<_, i64>(0),
        ) {
            Ok(a)                                     => Ok(a != 0),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e)                                    => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// Approve / un-approve a user.
    pub fn set_user_approved(&self, user_id: &str, approved: bool) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE users SET approved = ?1, updated_at = ?2 WHERE id = ?3",
            params![approved as i64, now, user_id],
        )
        .map_err(|e| MiraError::DatabaseError(format!("set_user_approved: {e}")))?;
        if n == 0 {
            return Err(MiraError::NotFound(format!("user not found: {user_id}")));
        }
        Ok(())
    }

    /// Users awaiting approval (`approved = 0`), newest first.
    pub fn list_pending_users(&self) -> Result<Vec<User>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM users WHERE approved = 0 ORDER BY created_at DESC",
            crate::auth::models::USER_COLS,
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let rows = stmt.query_map([], row_to_user).map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows { out.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?); }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::auth::models::AuthDb;
    use rusqlite::params;
    use tempfile::tempdir;

    fn db() -> (tempfile::TempDir, AuthDb) {
        let dir = tempdir().unwrap();
        let db = AuthDb::open(&dir.path().join("auth.db")).unwrap();
        // an admin row for created_by FK-free reference
        let conn = db.conn.lock().unwrap();
        let now = AuthDb::now_ms();
        conn.execute(
            "INSERT INTO users (id, username, password_hash, role, is_active, created_at, updated_at)
             VALUES ('admin','admin','x','admin',1,?1,?1)",
            params![now],
        ).unwrap();
        drop(conn);
        (dir, db)
    }

    #[test]
    fn invite_lifecycle() {
        let (_d, db) = db();
        let inv = db.create_invite("admin", "user", Some("a@x.com"), 1, None, "raw-token-1").unwrap();
        assert_eq!(inv.used_count, 0);

        // validate (read-only) finds it
        assert!(db.find_invite_by_token("raw-token-1").unwrap().is_some());
        assert!(db.find_invite_by_token("wrong").unwrap().is_none());

        // redeem once → consumed; second redeem fails (max_uses=1)
        let r = db.redeem_invite("raw-token-1").unwrap();
        assert_eq!(r.role, "user");
        assert!(db.redeem_invite("raw-token-1").is_err());
    }

    #[test]
    fn redeem_unknown_or_revoked_fails() {
        let (_d, db) = db();
        assert!(db.redeem_invite("nope").is_err());
        let inv = db.create_invite("admin", "user", None, 5, None, "tok").unwrap();
        db.revoke_invite(&inv.id).unwrap();
        assert!(db.redeem_invite("tok").is_err());
    }

    #[test]
    fn multi_use_invite() {
        let (_d, db) = db();
        db.create_invite("admin", "user", None, 3, None, "multi").unwrap();
        assert!(db.redeem_invite("multi").is_ok());
        assert!(db.redeem_invite("multi").is_ok());
        assert!(db.redeem_invite("multi").is_ok());
        assert!(db.redeem_invite("multi").is_err()); // exhausted
    }

    #[test]
    fn approval_gate() {
        let (_d, db) = db();
        let now = AuthDb::now_ms();
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO users (id, username, password_hash, role, is_active, approved, created_at, updated_at)
                 VALUES ('u1','u1','x','user',1,0,?1,?1)",
                params![now],
            ).unwrap();
        }
        assert!(!db.is_user_approved("u1").unwrap());
        assert_eq!(db.list_pending_users().unwrap().len(), 1);
        db.set_user_approved("u1", true).unwrap();
        assert!(db.is_user_approved("u1").unwrap());
        assert!(db.list_pending_users().unwrap().is_empty());
        // existing admin (default approved=1) is never pending
        assert!(db.is_user_approved("admin").unwrap());
    }
}
