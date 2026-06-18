// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/models.rs
//! User, Role, NewUser structs + SQLite CRUD via AuthDb.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::MiraError;

// ── SELECT column list — keep in sync with row_to_user below. ────────────────
pub(crate) const USER_COLS: &str = "id, username, display_name, email, role, is_active, \
                         created_at, updated_at, last_login, phone, preferred_contact, \
                         avatar, voice_prefs";

// ── Role ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User  => "user",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = MiraError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "admin" => Ok(Role::Admin),
            "user"  => Ok(Role::User),
            other   => Err(MiraError::AuthError(format!("Unknown role: {}", other))),
        }
    }
}

// ── User ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id:           String,
    pub username:     String,
    pub display_name: Option<String>,
    pub email:        Option<String>,
    pub role:         Role,
    pub is_active:    bool,
    pub created_at:   i64,
    pub updated_at:   i64,
    pub last_login:   Option<i64>,
    /// Contact phone number (E.164 preferred). Populated for the future
    /// proactive-messaging module; no other part of the system reads this yet.
    pub phone:        Option<String>,
    /// Channel key the agent should use for proactive outreach
    /// ("signal", "telegram", or None = don't contact proactively).
    pub preferred_contact: Option<String>,
    /// Avatar selection. Encoded as `"preset:<key>"` for bundled icons, or
    /// `"upload:<ext>"` when a file exists at `{data_dir}/avatars/{id}.{ext}`.
    pub avatar:            Option<String>,
    /// Per-user voice preferences keyed by channel id. Stored as a JSON
    /// blob (`HashMap<String, ChannelVoicePrefs>`) so plugin channels can
    /// register new keys without a schema migration. `None` or an empty
    /// object means "inherit server defaults for every channel."
    pub voice_prefs:       Option<String>,
}

// ── NewUser ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NewUser {
    pub username:     String,
    pub display_name: Option<String>,
    pub email:        Option<String>,
    pub password:     String,
    pub role:         Role,
}

// ── StoredRefreshToken ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StoredRefreshToken {
    pub token_hash: String,
    pub user_id:    String,
    pub expires_at: i64,
    pub revoked:    bool,
}

// ── AuthDb ────────────────────────────────────────────────────────────────────

/// Wraps a single SQLite connection and handles all auth tables.
pub struct AuthDb {
    pub(super) conn: Arc<Mutex<Connection>>,
}

impl AuthDb {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MiraError::DatabaseError(format!("Cannot create auth DB dir: {}", e))
            })?;
        }

        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open auth DB: {}", e))
        })?;

        // Enable WAL for better concurrent read performance.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id            TEXT PRIMARY KEY,
                username      TEXT UNIQUE NOT NULL,
                display_name  TEXT,
                email         TEXT,
                password_hash TEXT NOT NULL,
                role          TEXT NOT NULL DEFAULT 'user',
                is_active     INTEGER NOT NULL DEFAULT 1,
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL,
                last_login    INTEGER
            );

            CREATE TABLE IF NOT EXISTS refresh_tokens (
                token_hash    TEXT PRIMARY KEY,
                user_id       TEXT NOT NULL,
                expires_at    INTEGER NOT NULL,
                created_at    INTEGER NOT NULL,
                revoked       INTEGER NOT NULL DEFAULT 0,
                user_agent    TEXT,
                ip_address    TEXT,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
            );

            -- Groups — admin-created containers for shared memory visibility.
            -- A user can belong to many groups; a group has many users.
            CREATE TABLE IF NOT EXISTS groups (
                id          TEXT PRIMARY KEY,
                name        TEXT UNIQUE NOT NULL,
                description TEXT,
                created_by  TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL,
                FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE RESTRICT
            );

            CREATE TABLE IF NOT EXISTS group_members (
                group_id  TEXT NOT NULL,
                user_id   TEXT NOT NULL,
                added_by  TEXT NOT NULL,
                added_at  INTEGER NOT NULL,
                PRIMARY KEY (group_id, user_id),
                FOREIGN KEY (group_id) REFERENCES groups(id) ON DELETE CASCADE,
                FOREIGN KEY (user_id)  REFERENCES users(id)  ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_group_members_user ON group_members(user_id);

            -- Capability RBAC — a user's optional direct capability profile.
            -- Group profiles live on `groups.capabilities_json` (added below);
            -- a user's effective profile is the merge of this row + every
            -- group they belong to. See src/auth/capabilities.rs.
            CREATE TABLE IF NOT EXISTS user_capabilities (
                user_id           TEXT PRIMARY KEY,
                capabilities_json TEXT NOT NULL,
                updated_at        INTEGER NOT NULL,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
            );

            -- SSO / OIDC — stable external-identity binding. A returning SSO
            -- user is matched by (issuer, subject) regardless of email change.
            -- See src/auth/identities.rs + src/auth/oidc.rs.
            CREATE TABLE IF NOT EXISTS user_identities (
                issuer      TEXT NOT NULL,
                subject     TEXT NOT NULL,
                user_id     TEXT NOT NULL,
                provider_id TEXT,
                created_at  INTEGER NOT NULL,
                PRIMARY KEY (issuer, subject),
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_user_identities_user ON user_identities(user_id);

            -- Self-service onboarding (Q2 #11) — admin-minted invite tokens.
            -- The raw token is shown once at creation; only its SHA-256 hash is
            -- stored. A redemption creates an active account with `role`.
            CREATE TABLE IF NOT EXISTS invites (
                id          TEXT PRIMARY KEY,
                token_hash  TEXT UNIQUE NOT NULL,
                created_by  TEXT NOT NULL,
                role        TEXT NOT NULL DEFAULT 'user',
                email_hint  TEXT,
                max_uses    INTEGER NOT NULL DEFAULT 1,
                used_count  INTEGER NOT NULL DEFAULT 0,
                expires_at  INTEGER,
                revoked     INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );

            -- Extended per-user profile. Populated lazily during onboarding.
            -- One row per user, created on first write. Separate from `users`
            -- because these fields are (a) all optional, (b) edited by the
            -- user themselves via onboarding, and (c) not needed on every
            -- auth check.
            CREATE TABLE IF NOT EXISTS user_profile (
                user_id              TEXT PRIMARY KEY,
                full_name            TEXT,
                preferred_name       TEXT,
                nickname             TEXT,
                pronouns             TEXT,
                birth_date           TEXT,
                height_cm            INTEGER,
                weight_kg            INTEGER,
                eye_color            TEXT,
                hair_color           TEXT,
                timezone             TEXT,
                locale               TEXT,
                contact_hours_start  INTEGER,
                contact_hours_end    INTEGER,
                agent_name           TEXT,
                onboarded_at         INTEGER,
                onboarding_progress  TEXT,
                created_at           INTEGER NOT NULL,
                updated_at           INTEGER NOT NULL,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
            );

            -- 0.106.0 — failed login attempts. Populated by `local::login`
            -- on every Unauthorized return. Powers the
            -- `auth.failed_logins_*` health detectors and the temp-IP-ban
            -- auto-action. `username` is whatever the client supplied —
            -- including for unknown users (so brute-force attempts that
            -- enumerate usernames still leave a trace).
            CREATE TABLE IF NOT EXISTS auth_failed_logins (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                ip            TEXT,
                username      TEXT,
                reason        TEXT NOT NULL,
                attempted_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_failed_logins_ts
                ON auth_failed_logins(attempted_at DESC);
            CREATE INDEX IF NOT EXISTS idx_failed_logins_ip
                ON auth_failed_logins(ip, attempted_at DESC);

            -- 0.106.0 — temporary IP bans. Inserted by the health-audit
            -- auto-action when an IP exceeds the failed-login threshold.
            -- `banned_until` is unix-seconds; the security IpBanLayer
            -- short-circuits requests from rows where now < banned_until.
            CREATE TABLE IF NOT EXISTS auth_ip_bans (
                ip            TEXT PRIMARY KEY,
                banned_at     INTEGER NOT NULL,
                banned_until  INTEGER NOT NULL,
                reason        TEXT
            );
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!("Auth DB migration failed: {}", e)))?;

        // Additive columns. ALTER TABLE is idempotent via error-swallow
        // since SQLite has no IF NOT EXISTS on ADD COLUMN.
        for sql in [
            "ALTER TABLE users ADD COLUMN phone TEXT",
            "ALTER TABLE users ADD COLUMN preferred_contact TEXT",
            "ALTER TABLE users ADD COLUMN avatar TEXT",
            // Per-user voice preferences — a JSON blob keyed by channel id
            // so plugin channels can register new entries without a
            // schema bump. The previous per-column shape (voice_id /
            // voice_speed / auto_speak) was a single global override that
            // didn't survive once we needed per-channel response policies.
            "ALTER TABLE users ADD COLUMN voice_prefs TEXT",
            // Capability RBAC — a group's optional capability profile (JSON;
            // see src/auth/capabilities.rs). NULL = the group restricts nothing.
            "ALTER TABLE groups ADD COLUMN capabilities_json TEXT",
            // Self-service onboarding — pending-approval gate. 1 = approved
            // (default, so every existing + admin-created user stays usable);
            // open-signup accounts awaiting admin approval are 0.
            "ALTER TABLE users ADD COLUMN approved INTEGER NOT NULL DEFAULT 1",
            // Drop the now-superseded global override columns. SQLite 3.35+
            // supports DROP COLUMN; on older builds the statements simply
            // fail and we leave the columns hanging — they're harmless dead
            // weight since nothing reads them.
            "ALTER TABLE users DROP COLUMN voice_id",
            "ALTER TABLE users DROP COLUMN voice_speed",
            "ALTER TABLE users DROP COLUMN auto_speak",
        ] {
            let _ = conn.execute(sql, []);
        }

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub(super) fn now_ms() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    // ── User CRUD ─────────────────────────────────────────────────────────────

    pub fn create_user(&self, new: NewUser, hash: String) -> Result<User, MiraError> {
        let id   = Uuid::new_v4().to_string();
        let now  = Self::now_ms();
        let role = new.role.as_str().to_owned();

        let conn = self.conn.lock().unwrap();

        // Reject any case-variant of an existing username. The UNIQUE index on
        // `users.username` is exact-match, so without this a user could create
        // "Alice" alongside an existing "alice" and collide at login.
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users WHERE LOWER(username) = LOWER(?1)",
            params![new.username],
            |r| r.get(0),
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if exists > 0 {
            return Err(MiraError::AuthError(format!(
                "Username already taken: {}", new.username
            )));
        }

        conn.execute(
            "INSERT INTO users (id, username, display_name, email, password_hash, role, is_active, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)",
            params![
                id, new.username, new.display_name, new.email,
                hash, role, now,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("create_user: {}", e)))?;

        Ok(User {
            id,
            username:          new.username,
            display_name:      new.display_name,
            email:             new.email,
            role:              new.role,
            is_active:         true,
            created_at:        now,
            updated_at:        now,
            last_login:        None,
            phone:             None,
            preferred_contact: None,
            avatar:            None,
            voice_prefs:       None,
        })
    }

    pub fn find_by_username(&self, username: &str) -> Result<Option<User>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {} FROM users WHERE LOWER(username) = LOWER(?1)", USER_COLS);
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let result = stmt.query_row(params![username], row_to_user);
        match result {
            Ok(u)                               => Ok(Some(u)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                              => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn find_by_id(&self, id: &str) -> Result<Option<User>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {} FROM users WHERE id = ?1", USER_COLS);
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let result = stmt.query_row(params![id], row_to_user);
        match result {
            Ok(u)                               => Ok(Some(u)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                              => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// Find a user by email (case-insensitive). Email is not unique, so when
    /// several rows share it we return the most-recently-active one (NULL
    /// `last_login` sorts last under SQLite's DESC). Used for SSO account
    /// linking. `None`/empty email never matches.
    pub fn find_by_email(&self, email: &str) -> Result<Option<User>, MiraError> {
        let email = email.trim();
        if email.is_empty() {
            return Ok(None);
        }
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM users WHERE LOWER(email) = LOWER(?1) AND is_active = 1 \
             ORDER BY last_login DESC, created_at DESC LIMIT 1",
            USER_COLS
        );
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        match stmt.query_row(params![email], row_to_user) {
            Ok(u)                                     => Ok(Some(u)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                                    => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// Lookup a user by their personal contact phone number. Used by the
    /// channel listeners (Signal today; Telegram once we add a numeric id
    /// column) to map an inbound `sender` back to the MIRA user UUID, so
    /// memory and profile context follow the user across channels.
    ///
    /// Match is exact and case-insensitive — phone numbers are stored as
    /// E.164 strings (`+61421938567`) but we lowercase to be defensive
    /// against future free-form formats. Multiple rows with the same phone
    /// shouldn't happen (sign-up enforces it), but if they do we pick the
    /// most-recently active one to avoid surprising stale matches.
    pub fn find_by_phone(&self, phone: &str) -> Result<Option<User>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "SELECT {} FROM users \
             WHERE LOWER(phone) = LOWER(?1) AND is_active = 1 \
             ORDER BY COALESCE(last_login, updated_at) DESC LIMIT 1",
            USER_COLS,
        );
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        match stmt.query_row(params![phone], row_to_user) {
            Ok(u)                                     => Ok(Some(u)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                                    => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn list_users(&self) -> Result<Vec<User>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {} FROM users ORDER BY created_at ASC", USER_COLS);
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let rows = stmt.query_map([], row_to_user)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        let mut users = Vec::new();
        for r in rows {
            users.push(r.map_err(|e| MiraError::DatabaseError(e.to_string()))?);
        }
        Ok(users)
    }

    pub fn update_user(
        &self,
        id:                &str,
        display_name:      Option<String>,
        email:             Option<String>,
        role:              Role,
        is_active:         bool,
        phone:             Option<String>,
        preferred_contact: Option<String>,
        avatar:            Option<String>,
        voice_prefs:       Option<String>,
    ) -> Result<User, MiraError> {
        let now  = Self::now_ms();
        let role_str = role.as_str().to_owned();
        let active   = is_active as i64;

        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE users
                SET display_name=?1, email=?2, role=?3, is_active=?4,
                    phone=?5, preferred_contact=?6, avatar=?7,
                    voice_prefs=?8,
                    updated_at=?9
              WHERE id=?10",
            params![
                display_name, email, role_str, active,
                phone, preferred_contact, avatar,
                voice_prefs,
                now, id,
            ],
        )
        .map_err(|e| MiraError::DatabaseError(format!("update_user: {}", e)))?;

        if rows == 0 {
            return Err(MiraError::NotFound(format!("User not found: {}", id)));
        }

        // Re-fetch.
        let sql = format!("SELECT {} FROM users WHERE id = ?1", USER_COLS);
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;

        stmt.query_row(params![id], row_to_user)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    pub fn set_avatar(&self, id: &str, avatar: Option<&str>) -> Result<User, MiraError> {
        let now  = Self::now_ms();
        let conn = self.conn.lock().unwrap();

        let rows = conn.execute(
            "UPDATE users SET avatar=?1, updated_at=?2 WHERE id=?3",
            params![avatar, now, id],
        )
        .map_err(|e| MiraError::DatabaseError(format!("set_avatar: {}", e)))?;

        if rows == 0 {
            return Err(MiraError::NotFound(format!("User not found: {}", id)));
        }

        let sql = format!("SELECT {} FROM users WHERE id = ?1", USER_COLS);
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        stmt.query_row(params![id], row_to_user)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    pub fn delete_user(&self, id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute("DELETE FROM users WHERE id = ?1", params![id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("User not found: {}", id)));
        }
        Ok(())
    }

    pub fn update_last_login(&self, id: &str) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE users SET last_login=?1 WHERE id=?2", params![now, id])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    pub fn get_password_hash(&self, id: &str) -> Result<String, MiraError> {
        let conn = self.conn.lock().unwrap();
        let result: rusqlite::Result<String> = conn.query_row(
            "SELECT password_hash FROM users WHERE id = ?1",
            params![id],
            |row| row.get(0),
        );
        match result {
            Ok(h)                               => Ok(h),
            Err(rusqlite::Error::QueryReturnedNoRows) =>
                Err(MiraError::NotFound(format!("User not found: {}", id))),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn get_password_hash_by_username(&self, username: &str) -> Result<(String, String), MiraError> {
        // Returns (id, hash) — lookup is case-insensitive so "Alice" and "alice"
        // resolve to the same row.
        let conn = self.conn.lock().unwrap();
        let result: rusqlite::Result<(String, String)> = conn.query_row(
            "SELECT id, password_hash FROM users WHERE LOWER(username) = LOWER(?1)",
            params![username],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        match result {
            Ok(pair)                            => Ok(pair),
            Err(rusqlite::Error::QueryReturnedNoRows) =>
                Err(MiraError::NotFound(format!("User not found: {}", username))),
            Err(e) => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn change_password(&self, id: &str, new_hash: String) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE users SET password_hash=?1, updated_at=?2 WHERE id=?3",
            params![new_hash, now, id],
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if rows == 0 {
            return Err(MiraError::NotFound(format!("User not found: {}", id)));
        }
        Ok(())
    }

    pub fn count_users(&self) -> Result<i64, MiraError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(count)
    }

    // ── Refresh token CRUD ────────────────────────────────────────────────────

    pub fn save_refresh_token(
        &self,
        user_id:    &str,
        token_hash: &str,
        expires_at: i64,
        user_agent: Option<&str>,
        ip:         Option<&str>,
    ) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO refresh_tokens (token_hash, user_id, expires_at, created_at, revoked, user_agent, ip_address)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![token_hash, user_id, expires_at, now, user_agent, ip],
        )
        .map_err(|e| MiraError::DatabaseError(format!("save_refresh_token: {}", e)))?;
        Ok(())
    }

    pub fn find_refresh_token(&self, token_hash: &str) -> Result<Option<StoredRefreshToken>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT token_hash, user_id, expires_at, revoked FROM refresh_tokens WHERE token_hash = ?1",
            params![token_hash],
            |row| {
                Ok(StoredRefreshToken {
                    token_hash: row.get(0)?,
                    user_id:    row.get(1)?,
                    expires_at: row.get(2)?,
                    revoked:    row.get::<_, i64>(3)? != 0,
                })
            },
        );
        match result {
            Ok(t)                               => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                              => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    pub fn revoke_refresh_token(&self, token_hash: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE refresh_tokens SET revoked=1 WHERE token_hash=?1",
            params![token_hash],
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    pub fn revoke_all_for_user(&self, user_id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE refresh_tokens SET revoked=1 WHERE user_id=?1",
            params![user_id],
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    /// Count a user's live sessions — non-revoked, unexpired refresh tokens.
    /// Drives the admin "sign out everywhere (N sessions)" affordance.
    pub fn count_active_sessions(&self, user_id: &str) -> Result<i64, MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM refresh_tokens WHERE user_id=?1 AND revoked=0 AND expires_at > ?2",
            params![user_id, now],
            |r| r.get(0),
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))
    }

    pub fn cleanup_expired(&self) -> Result<(), MiraError> {
        let now = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM refresh_tokens WHERE expires_at < ?1",
            params![now],
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(())
    }

    // ── 0.106.0: failed-login bookkeeping ──────────────────────────────

    /// Record one failed-login attempt. Best-effort — caller logs and
    /// continues on error (a failed-login bookkeeping write must never
    /// block the actual auth response).
    pub fn record_failed_login(
        &self,
        ip:       Option<&str>,
        username: Option<&str>,
        reason:   &str,
    ) -> Result<(), MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO auth_failed_logins (ip, username, reason, attempted_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![ip, username, reason, now],
        ).map_err(|e| MiraError::DatabaseError(format!("record_failed_login: {e}")))?;
        Ok(())
    }

    /// (total_failures, top_ip_count) over `since`. The detector uses
    /// the top-IP count for the per-IP threshold and the total for the
    /// global one.
    pub fn count_failed_logins_since(
        &self, since: i64,
    ) -> Result<(usize, usize, Option<String>), MiraError> {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM auth_failed_logins WHERE attempted_at >= ?1",
            params![since], |r| r.get(0),
        ).map_err(|e| MiraError::DatabaseError(format!("count_failed_logins total: {e}")))?;
        let mut stmt = conn.prepare(
            "SELECT ip, COUNT(*) c FROM auth_failed_logins
              WHERE attempted_at >= ?1 AND ip IS NOT NULL
              GROUP BY ip ORDER BY c DESC LIMIT 1",
        ).map_err(|e| MiraError::DatabaseError(format!("count_failed_logins prep: {e}")))?;
        let top: Option<(String, i64)> = stmt.query_row(params![since], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        }).ok();
        let (ip, ip_count) = match top {
            Some((i, c)) => (Some(i), c as usize),
            None         => (None, 0),
        };
        Ok((total as usize, ip_count, ip))
    }

    /// Drop failed-login rows older than `cutoff`. Caller invokes from
    /// the auto-action when a temp-ban fires, plus opportunistically on
    /// a future heartbeat. Bounded growth control.
    pub fn prune_failed_logins(&self, cutoff: i64) -> Result<usize, MiraError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM auth_failed_logins WHERE attempted_at < ?1",
            params![cutoff],
        ).map_err(|e| MiraError::DatabaseError(format!("prune_failed_logins: {e}")))?;
        Ok(n)
    }

    // ── 0.106.0: IP bans ──────────────────────────────────────────────

    /// Ban `ip` for `secs` from now. Idempotent — re-banning extends
    /// the ban if the new expiry is later than the existing one.
    pub fn ban_ip(&self, ip: &str, secs: i64, reason: &str) -> Result<i64, MiraError> {
        let now = chrono::Utc::now().timestamp();
        let until = now + secs;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO auth_ip_bans (ip, banned_at, banned_until, reason)
                  VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(ip) DO UPDATE
                SET banned_until = MAX(banned_until, excluded.banned_until),
                    reason       = COALESCE(excluded.reason, reason)",
            params![ip, now, until, reason],
        ).map_err(|e| MiraError::DatabaseError(format!("ban_ip: {e}")))?;
        Ok(until)
    }

    /// Lift any ban on `ip`. Returns whether anything was removed.
    pub fn unban_ip(&self, ip: &str) -> Result<bool, MiraError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM auth_ip_bans WHERE ip = ?1",
            params![ip],
        ).map_err(|e| MiraError::DatabaseError(format!("unban_ip: {e}")))?;
        Ok(n > 0)
    }

    /// All currently-active bans (banned_until > now). Caller is
    /// expected to refresh periodically — this isn't a real-time hot
    /// path. The middleware caches the active set via the IpBanCache
    /// layer with a short refresh interval.
    pub fn list_active_bans(&self) -> Result<Vec<(String, i64, Option<String>)>, MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ip, banned_until, reason FROM auth_ip_bans WHERE banned_until > ?1",
        ).map_err(|e| MiraError::DatabaseError(format!("list_bans prep: {e}")))?;
        let rows = stmt.query_map(params![now], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?))
        }).map_err(|e| MiraError::DatabaseError(format!("list_bans q: {e}")))?
          .collect::<rusqlite::Result<Vec<_>>>()
          .map_err(|e| MiraError::DatabaseError(format!("list_bans rows: {e}")))?;
        Ok(rows)
    }

    /// Drop expired ban rows. Cheap; called from the heartbeat after
    /// each list refresh.
    pub fn prune_expired_bans(&self) -> Result<usize, MiraError> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM auth_ip_bans WHERE banned_until <= ?1",
            params![now],
        ).map_err(|e| MiraError::DatabaseError(format!("prune_bans: {e}")))?;
        Ok(n)
    }
}

// ── UserProfile ───────────────────────────────────────────────────────────────

/// Per-user soft profile populated during onboarding. One row per user; the
/// row is created lazily on first write. All fields are optional — the
/// presence of a row does not imply onboarding completion. Check
/// `onboarded_at` for that.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserProfile {
    pub user_id:              String,
    pub full_name:            Option<String>,
    pub preferred_name:       Option<String>,
    pub nickname:             Option<String>,
    pub pronouns:             Option<String>,
    /// ISO 8601 date (YYYY-MM-DD). Age is derived at read time.
    pub birth_date:           Option<String>,
    pub height_cm:            Option<i64>,
    pub weight_kg:            Option<i64>,
    pub eye_color:            Option<String>,
    pub hair_color:           Option<String>,
    /// IANA timezone, e.g. "Australia/Sydney".
    pub timezone:             Option<String>,
    /// BCP 47 locale, e.g. "en-AU".
    pub locale:               Option<String>,
    /// Minutes from midnight in the user's timezone, 0..1439.
    pub contact_hours_start:  Option<i64>,
    pub contact_hours_end:    Option<i64>,
    /// What the user wants to call MIRA. `None` = default.
    pub agent_name:           Option<String>,
    /// Unix ms when onboarding was completed. `None` = not done.
    pub onboarded_at:         Option<i64>,
    /// Opaque JSON blob. See `ONBOARDING_PLAN.md` Appendix B for shape.
    pub onboarding_progress:  Option<String>,
    pub created_at:           i64,
    pub updated_at:           i64,
}

const PROFILE_COLS: &str = "user_id, full_name, preferred_name, nickname, pronouns, \
                            birth_date, height_cm, weight_kg, eye_color, hair_color, \
                            timezone, locale, contact_hours_start, contact_hours_end, \
                            agent_name, onboarded_at, onboarding_progress, \
                            created_at, updated_at";

impl AuthDb {
    /// Fetch the profile row for a user; `None` if it has never been written.
    pub fn get_profile(&self, user_id: &str) -> Result<Option<UserProfile>, MiraError> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("SELECT {} FROM user_profile WHERE user_id = ?1", PROFILE_COLS);
        let mut stmt = conn.prepare(&sql)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        match stmt.query_row(params![user_id], row_to_profile) {
            Ok(p)                                     => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e)                                    => Err(MiraError::DatabaseError(e.to_string())),
        }
    }

    /// Ensure a profile row exists for the user. Returns whether a row was
    /// inserted (`true`) or already existed (`false`).
    fn ensure_profile_row(conn: &Connection, user_id: &str) -> Result<bool, MiraError> {
        let now = Self::now_ms();
        let rows = conn.execute(
            "INSERT OR IGNORE INTO user_profile (user_id, created_at, updated_at)
             VALUES (?1, ?2, ?2)",
            params![user_id, now],
        ).map_err(|e| MiraError::DatabaseError(format!("ensure_profile_row: {}", e)))?;
        Ok(rows > 0)
    }

    /// Upsert a single column on the profile. `column` is trusted — callers
    /// MUST pass a known static string (one of the columns listed in
    /// `PROFILE_COLS`), never user input.
    pub fn upsert_profile_field<V: rusqlite::ToSql>(
        &self,
        user_id: &str,
        column:  &'static str,
        value:   V,
    ) -> Result<(), MiraError> {
        let now  = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        Self::ensure_profile_row(&conn, user_id)?;
        let sql = format!(
            "UPDATE user_profile SET {} = ?1, updated_at = ?2 WHERE user_id = ?3",
            column
        );
        conn.execute(&sql, params![value, now, user_id])
            .map_err(|e| MiraError::DatabaseError(format!("upsert_profile_field({}): {}", column, e)))?;
        Ok(())
    }

    /// Write the `onboarding_progress` JSON blob. Opaque to the DB layer.
    pub fn set_onboarding_progress(&self, user_id: &str, progress_json: &str) -> Result<(), MiraError> {
        self.upsert_profile_field(user_id, "onboarding_progress", progress_json)
    }

    /// Stamp `onboarded_at` with the current time. Idempotent — re-running
    /// overwrites with the latest timestamp.
    pub fn mark_onboarded(&self, user_id: &str) -> Result<(), MiraError> {
        self.upsert_profile_field(user_id, "onboarded_at", Self::now_ms())
    }

    /// Wipe all soft fields captured during onboarding, plus the progress
    /// blob and `onboarded_at`. Leaves `user_id`, `created_at`, and
    /// `updated_at` in place — the row's identity and its avatar (which is
    /// edited through its own UI) are preserved. Called by the Settings
    /// "Start fresh" button before a new onboarding conversation is
    /// created. Idempotent: safe to run against a row that has no data.
    pub fn reset_onboarding_profile(&self, user_id: &str) -> Result<(), MiraError> {
        let conn = self.conn.lock().unwrap();
        Self::ensure_profile_row(&conn, user_id)?;
        conn.execute(
            "UPDATE user_profile SET
                full_name           = NULL,
                preferred_name      = NULL,
                nickname            = NULL,
                pronouns            = NULL,
                birth_date          = NULL,
                height_cm           = NULL,
                weight_kg           = NULL,
                eye_color           = NULL,
                hair_color          = NULL,
                timezone            = NULL,
                locale              = NULL,
                contact_hours_start = NULL,
                contact_hours_end   = NULL,
                agent_name          = NULL,
                onboarded_at        = NULL,
                onboarding_progress = NULL,
                updated_at          = ?1
             WHERE user_id = ?2",
            params![Self::now_ms(), user_id],
        )
        .map_err(|e| MiraError::DatabaseError(format!("reset_onboarding_profile: {}", e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use tempfile::tempdir;

    fn open_db_with_user(user_id: &str) -> (tempfile::TempDir, AuthDb) {
        let dir  = tempdir().unwrap();
        let db   = AuthDb::open(&dir.path().join("auth.db")).unwrap();
        // FK requires a users row — insert a minimal one directly.
        let conn = db.conn.lock().unwrap();
        let now  = AuthDb::now_ms();
        conn.execute(
            "INSERT INTO users (id, username, password_hash, role, is_active, created_at, updated_at)
             VALUES (?1, ?1, 'x', 'user', 1, ?2, ?2)",
            params![user_id, now],
        ).unwrap();
        drop(conn);
        (dir, db)
    }

    #[test]
    fn migration_is_idempotent() {
        let dir  = tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let _a   = AuthDb::open(&path).unwrap();
        let _b   = AuthDb::open(&path).unwrap();
    }

    #[test]
    fn profile_is_lazy_and_upsert_creates_row() {
        let (_dir, db) = open_db_with_user("u1");
        assert!(db.get_profile("u1").unwrap().is_none());

        db.upsert_profile_field("u1", "preferred_name", "Alex").unwrap();
        let p = db.get_profile("u1").unwrap().unwrap();
        assert_eq!(p.preferred_name.as_deref(), Some("Alex"));
        assert!(p.onboarded_at.is_none());
    }

    #[test]
    fn mark_onboarded_sets_timestamp() {
        let (_dir, db) = open_db_with_user("u1");
        db.mark_onboarded("u1").unwrap();
        let p = db.get_profile("u1").unwrap().unwrap();
        assert!(p.onboarded_at.is_some());
    }

    #[test]
    fn progress_blob_round_trips() {
        let (_dir, db) = open_db_with_user("u1");
        let blob = r#"{"completed_groups":["name"],"skipped_keys":[]}"#;
        db.set_onboarding_progress("u1", blob).unwrap();
        let p = db.get_profile("u1").unwrap().unwrap();
        assert_eq!(p.onboarding_progress.as_deref(), Some(blob));
    }

    #[test]
    fn reset_onboarding_profile_clears_soft_fields() {
        let (_dir, db) = open_db_with_user("u1");
        db.upsert_profile_field("u1", "preferred_name", "Alex").unwrap();
        db.upsert_profile_field("u1", "timezone", "Australia/Sydney").unwrap();
        db.set_onboarding_progress("u1", r#"{"completed_groups":["name"]}"#).unwrap();
        db.mark_onboarded("u1").unwrap();

        db.reset_onboarding_profile("u1").unwrap();

        let p = db.get_profile("u1").unwrap().unwrap();
        assert_eq!(p.preferred_name, None);
        assert_eq!(p.timezone, None);
        assert_eq!(p.onboarding_progress, None);
        assert_eq!(p.onboarded_at, None);
    }

    #[test]
    fn reset_onboarding_profile_is_idempotent_on_empty_row() {
        let (_dir, db) = open_db_with_user("u1");
        db.reset_onboarding_profile("u1").unwrap();
        db.reset_onboarding_profile("u1").unwrap();
    }

    #[test]
    fn find_by_phone_resolves_active_user() {
        let (_dir, db) = open_db_with_user("u1");
        // Stamp a phone on the freshly-inserted u1.
        db.conn.lock().unwrap().execute(
            "UPDATE users SET phone = ?1 WHERE id = 'u1'",
            params!["+61421938567"],
        ).unwrap();

        let u = db.find_by_phone("+61421938567").unwrap().unwrap();
        assert_eq!(u.id, "u1");

        // Case-insensitive lookup still works (matters for future free-form formats).
        let upper = db.find_by_phone("+61421938567").unwrap().unwrap();
        assert_eq!(upper.id, "u1");

        // Unknown phone returns None, not an error.
        assert!(db.find_by_phone("+10000000000").unwrap().is_none());
    }

    #[test]
    fn find_by_phone_skips_disabled_users() {
        let (_dir, db) = open_db_with_user("u1");
        db.conn.lock().unwrap().execute(
            "UPDATE users SET phone = ?1, is_active = 0 WHERE id = 'u1'",
            params!["+61421938567"],
        ).unwrap();
        // Disabled users shouldn't claim inbound channel messages.
        assert!(db.find_by_phone("+61421938567").unwrap().is_none());
    }
}

fn row_to_profile(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserProfile> {
    Ok(UserProfile {
        user_id:              row.get(0)?,
        full_name:            row.get(1)?,
        preferred_name:       row.get(2)?,
        nickname:             row.get(3)?,
        pronouns:             row.get(4)?,
        birth_date:           row.get(5)?,
        height_cm:            row.get(6)?,
        weight_kg:            row.get(7)?,
        eye_color:            row.get(8)?,
        hair_color:           row.get(9)?,
        timezone:             row.get(10)?,
        locale:               row.get(11)?,
        contact_hours_start:  row.get(12)?,
        contact_hours_end:    row.get(13)?,
        agent_name:           row.get(14)?,
        onboarded_at:         row.get(15)?,
        onboarding_progress:  row.get(16)?,
        created_at:           row.get(17)?,
        updated_at:           row.get(18)?,
    })
}

// ── Row helper ─────────────────────────────────────────────────────────────────

pub(super) fn row_to_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    use std::str::FromStr;

    let role_str: String = row.get(4)?;
    let role = Role::from_str(&role_str).unwrap_or(Role::User);

    Ok(User {
        id:                row.get(0)?,
        username:          row.get(1)?,
        display_name:      row.get(2)?,
        email:             row.get(3)?,
        role,
        is_active:         row.get::<_, i64>(5)? != 0,
        created_at:        row.get(6)?,
        updated_at:        row.get(7)?,
        last_login:        row.get(8)?,
        phone:             row.get(9)?,
        preferred_contact: row.get(10)?,
        avatar:            row.get(11)?,
        voice_prefs:       row.get(12)?,
    })
}
