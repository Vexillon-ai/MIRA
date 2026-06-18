// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/identities.rs
//! External-identity binding for SSO/OIDC. A `user_identities` row maps a
//! stable `(issuer, subject)` pair — the IdP's permanent id for a person — to
//! a MIRA `user_id`, so a returning SSO user is matched even if their email or
//! display name changes at the IdP. Table created in `models.rs`.

use rusqlite::params;

use crate::auth::models::{AuthDb, User};
use crate::MiraError;

impl AuthDb {
    /// Resolve the MIRA user bound to an `(issuer, subject)` identity, if any.
    pub fn find_user_by_identity(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<User>, MiraError> {
        let user_id: Option<String> = {
            let conn = self.conn.lock().unwrap();
            match conn.query_row(
                "SELECT user_id FROM user_identities WHERE issuer = ?1 AND subject = ?2",
                params![issuer, subject],
                |r| r.get(0),
            ) {
                Ok(uid) => Some(uid),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(MiraError::DatabaseError(e.to_string())),
            }
        };
        match user_id {
            Some(uid) => self.find_by_id(&uid),
            None => Ok(None),
        }
    }

    /// Bind `(issuer, subject)` to a user. Idempotent: re-linking the same
    /// identity to the same user is a no-op; the PK prevents binding one
    /// external identity to two different users.
    pub fn link_identity(
        &self,
        issuer: &str,
        subject: &str,
        user_id: &str,
        provider_id: &str,
    ) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_identities (issuer, subject, user_id, provider_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(issuer, subject) DO NOTHING",
            params![issuer, subject, user_id, provider_id, now],
        )
        .map_err(|e| MiraError::DatabaseError(format!("link_identity: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::auth::models::AuthDb;
    use rusqlite::params;
    use tempfile::tempdir;

    fn db_with_user(uid: &str, email: &str) -> (tempfile::TempDir, AuthDb) {
        let dir = tempdir().unwrap();
        let db = AuthDb::open(&dir.path().join("auth.db")).unwrap();
        let conn = db.conn.lock().unwrap();
        let now = AuthDb::now_ms();
        conn.execute(
            "INSERT INTO users (id, username, email, password_hash, role, is_active, created_at, updated_at)
             VALUES (?1, ?1, ?2, 'x', 'user', 1, ?3, ?3)",
            params![uid, email, now],
        )
        .unwrap();
        drop(conn);
        (dir, db)
    }

    #[test]
    fn link_and_resolve_identity() {
        let (_d, db) = db_with_user("u1", "a@example.com");
        assert!(db.find_user_by_identity("https://idp", "sub-1").unwrap().is_none());

        db.link_identity("https://idp", "sub-1", "u1", "google").unwrap();
        let u = db.find_user_by_identity("https://idp", "sub-1").unwrap().unwrap();
        assert_eq!(u.id, "u1");

        // Idempotent re-link is fine.
        db.link_identity("https://idp", "sub-1", "u1", "google").unwrap();
    }

    #[test]
    fn find_by_email_matches_case_insensitively() {
        let (_d, db) = db_with_user("u2", "Bob@Example.com");
        assert_eq!(db.find_by_email("bob@example.com").unwrap().unwrap().id, "u2");
        assert!(db.find_by_email("nobody@example.com").unwrap().is_none());
        assert!(db.find_by_email("").unwrap().is_none());
    }
}
