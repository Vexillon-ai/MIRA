// SPDX-License-Identifier: AGPL-3.0-or-later

//! Skill secrets vault — encrypted store for env vars subprocess
//! adapters need (e.g. `ANTHROPIC_API_KEY` for `com.mira.claudecode`).
//!
//! ## Threat model
//!
//! Protect against an attacker who reads `skill_secrets.db` off disk
//! (backup tape, accidental support bundle, host snapshot). Master
//! key lives in a sibling file (`master.key`, mode 0600) — keeping
//! the two separated means you have to grab BOTH to recover values.
//! NOT designed to defeat a privileged attacker who has root on the
//! same host as MIRA: an attacker who can read the master key and
//! the DB has, by definition, won. Encrypt-at-rest is a defence in
//! depth measure, not a substitute for filesystem permissions.
//!
//! ## Cipher
//!
//! AES-256-GCM with a fresh 12-byte random nonce per record. AES-GCM
//! catastrophically loses confidentiality on nonce reuse, so the
//! random space (2^96) has to be wide enough that birthday-paradox
//! collisions are negligible at our scale (≪ 2^32 records). For a
//! per-host secrets store with O(100) entries this is comfortably
//! safe; if we ever shipped a multi-tenant cloud version with
//! O(billions) of secrets we'd switch to XChaCha20-Poly1305 for the
//! 192-bit nonce.
//!
//! ## Scope
//!
//! `(scope, scope_id, skill_id, key)` is the primary key.
//! - `scope = "system"` + `scope_id = ""` → host-wide value (e.g. an
//!   admin-set `ANTHROPIC_API_KEY` for every user's coding tasks).
//! - `scope = "user"` + `scope_id = <user_id>` → per-user override.
//!
//! Lookup goes user-first, system-fallback (see [`SecretsStore::env_vars_for`])
//! so a user can always shadow the system default with their own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng, Payload},
    Aes256Gcm, Key, Nonce,
};
use rand::RngCore;
use rusqlite::{params, Connection};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("master key file invalid: expected 32 hex bytes, got {0}")]
    BadMasterKey(String),
    #[error("encryption failed (this is a bug — report it)")]
    Encrypt,
    #[error("decryption failed — ciphertext tampered, master key changed, or DB corrupt")]
    Decrypt,
    #[error("value is not valid UTF-8 — corrupt secret")]
    NonUtf8Value,
    #[error("key '{0}' contains characters disallowed for env-var names")]
    InvalidKeyName(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Host-wide. `scope_id` is the empty string. Set by an admin via
    /// the admin UI / CLI; available to every user's tasks under the
    /// matching skill.
    System,
    /// Per-user. `scope_id` is the user's UUID. Shadows a system
    /// value of the same key.
    User,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::System => "system",
            Scope::User   => "user",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "system" => Some(Scope::System),
            "user"   => Some(Scope::User),
            _        => None,
        }
    }
}

/// One record listed by [`SecretsStore::list`]. Never carries a
/// decrypted value — listing is metadata-only.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SecretEntry {
    pub scope:      Scope,
    pub scope_id:   String,
    pub skill_id:   String,
    pub key:        String,
    pub updated_at: i64,
}

/// File-backed vault. Open once at startup; share via `Arc`.
pub struct SecretsStore {
    conn:   Mutex<Connection>,
    cipher: Aes256Gcm,
}

impl SecretsStore {
    /// Open the DB at `db_path`, loading or creating the master key
    /// at `key_path`. The master key file is created with 0600 perms
    /// when missing. If the key already exists but is malformed
    /// we refuse to start: silently regenerating would lose access
    /// to every previously-stored secret.
    pub fn open(db_path: &Path, key_path: &Path) -> Result<Self, SecretsError> {
        let key = load_or_create_master_key(key_path)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS skill_secrets (
                scope      TEXT NOT NULL,
                scope_id   TEXT NOT NULL,
                skill_id   TEXT NOT NULL,
                key        TEXT NOT NULL,
                nonce      BLOB NOT NULL,
                ciphertext BLOB NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (scope, scope_id, skill_id, key)
            ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS idx_skill_secrets_lookup
               ON skill_secrets(skill_id, scope, scope_id);",
        )?;
        debug!("SecretsStore opened at {:?}", db_path);
        Ok(Self { conn: Mutex::new(conn), cipher })
    }

    /// Set or overwrite a secret. The value is bound to the
    /// `(scope, scope_id, skill_id, key)` tuple as AAD so a swap
    /// attack (copying ciphertext from one row into another) fails
    /// closed at decrypt time.
    pub fn set(
        &self,
        scope:    Scope,
        scope_id: &str,
        skill_id: &str,
        key:      &str,
        value:    &str,
    ) -> Result<(), SecretsError> {
        validate_env_key(key)?;
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let aad = associated_data(scope, scope_id, skill_id, key);
        let ciphertext = self.cipher.encrypt(
            nonce,
            Payload { msg: value.as_bytes(), aad: aad.as_bytes() },
        ).map_err(|_| SecretsError::Encrypt)?;
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock().expect("secrets conn");
        conn.execute(
            "INSERT INTO skill_secrets (scope, scope_id, skill_id, key, nonce, ciphertext, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(scope, scope_id, skill_id, key) DO UPDATE SET
               nonce = excluded.nonce,
               ciphertext = excluded.ciphertext,
               updated_at = excluded.updated_at",
            params![scope.as_str(), scope_id, skill_id, key, &nonce_bytes[..], &ciphertext, now],
        )?;
        info!(
            "skill secret set: scope={} scope_id={} skill={} key={} (value redacted)",
            scope.as_str(), redact_id(scope_id), skill_id, key,
        );
        Ok(())
    }

    /// Read a single secret.
    pub fn get(
        &self,
        scope:    Scope,
        scope_id: &str,
        skill_id: &str,
        key:      &str,
    ) -> Result<Option<String>, SecretsError> {
        let conn = self.conn.lock().expect("secrets conn");
        let mut stmt = conn.prepare(
            "SELECT nonce, ciphertext FROM skill_secrets
              WHERE scope = ?1 AND scope_id = ?2 AND skill_id = ?3 AND key = ?4",
        )?;
        let row = stmt.query_row(
            params![scope.as_str(), scope_id, skill_id, key],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
        );
        let (nonce_bytes, ciphertext) = match row {
            Ok(t)                                     => t,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e)                                    => return Err(e.into()),
        };
        if nonce_bytes.len() != 12 {
            return Err(SecretsError::Decrypt);
        }
        let nonce = Nonce::from_slice(&nonce_bytes);
        let aad = associated_data(scope, scope_id, skill_id, key);
        let plaintext = self.cipher.decrypt(
            nonce,
            Payload { msg: &ciphertext, aad: aad.as_bytes() },
        ).map_err(|_| SecretsError::Decrypt)?;
        let s = String::from_utf8(plaintext).map_err(|_| SecretsError::NonUtf8Value)?;
        Ok(Some(s))
    }

    /// Distinct skill ids across the entire vault. Used by the
    /// 0.108.0 `skills.dangling_secrets_count` health detector to
    /// compare against the installed-skill set.
    pub fn list_distinct_skill_ids(&self) -> Result<Vec<String>, SecretsError> {
        let conn = self.conn.lock().expect("secrets conn");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT skill_id FROM skill_secrets ORDER BY skill_id",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(SecretsError::from)
    }

    /// Delete every secret for `skill_id` across all (scope, scope_id)
    /// combinations. Used by the dangling-secrets sweep auto-action
    /// when a skill is no longer installed. Returns rows deleted.
    pub fn purge_skill(&self, skill_id: &str) -> Result<usize, SecretsError> {
        let conn = self.conn.lock().expect("secrets conn");
        let n = conn.execute(
            "DELETE FROM skill_secrets WHERE skill_id = ?1",
            params![skill_id],
        )?;
        if n > 0 {
            info!("secrets purge: removed {n} row(s) for skill {skill_id}");
        }
        Ok(n)
    }

    /// List the keys (NOT values) registered under one
    /// `(scope, scope_id, skill_id)`. Use this to populate the UI.
    pub fn list(
        &self,
        scope:    Scope,
        scope_id: &str,
        skill_id: &str,
    ) -> Result<Vec<SecretEntry>, SecretsError> {
        let conn = self.conn.lock().expect("secrets conn");
        let mut stmt = conn.prepare(
            "SELECT scope, scope_id, skill_id, key, updated_at
               FROM skill_secrets
              WHERE scope = ?1 AND scope_id = ?2 AND skill_id = ?3
              ORDER BY key",
        )?;
        let rows = stmt.query_map(
            params![scope.as_str(), scope_id, skill_id],
            |r| Ok(SecretEntry {
                scope:      Scope::from_str(&r.get::<_, String>(0)?).unwrap_or(Scope::System),
                scope_id:   r.get(1)?,
                skill_id:   r.get(2)?,
                key:        r.get(3)?,
                updated_at: r.get(4)?,
            }),
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(SecretsError::from)
    }

    /// Remove one secret. Returns true if a row was deleted.
    pub fn delete(
        &self,
        scope:    Scope,
        scope_id: &str,
        skill_id: &str,
        key:      &str,
    ) -> Result<bool, SecretsError> {
        let conn = self.conn.lock().expect("secrets conn");
        let n = conn.execute(
            "DELETE FROM skill_secrets
              WHERE scope = ?1 AND scope_id = ?2 AND skill_id = ?3 AND key = ?4",
            params![scope.as_str(), scope_id, skill_id, key],
        )?;
        if n > 0 {
            info!(
                "skill secret deleted: scope={} scope_id={} skill={} key={}",
                scope.as_str(), redact_id(scope_id), skill_id, key,
            );
        }
        Ok(n > 0)
    }

    /// Resolve the env-var map a subprocess adapter should use for a
    /// given `(user_id, skill_id)`. System secrets are loaded first;
    /// user secrets shadow them on key collision. Decrypt failures
    /// are logged and the offending key skipped — better to start
    /// the subprocess with a partial env than refuse to launch.
    pub fn env_vars_for(
        &self,
        user_id:  Option<&str>,
        skill_id: &str,
    ) -> HashMap<String, String> {
        let mut env = HashMap::new();
        for entry in self.list(Scope::System, "", skill_id).unwrap_or_default() {
            if let Some(v) = self.try_get_logged(Scope::System, "", skill_id, &entry.key) {
                env.insert(entry.key, v);
            }
        }
        if let Some(uid) = user_id {
            for entry in self.list(Scope::User, uid, skill_id).unwrap_or_default() {
                if let Some(v) = self.try_get_logged(Scope::User, uid, skill_id, &entry.key) {
                    env.insert(entry.key, v);
                }
            }
        }
        env
    }

    /// Move every row from `old_skill_id` to `new_skill_id`, re-encrypting
    /// each value because the row's AAD includes `skill_id` (so the old
    /// ciphertext won't decrypt under the new identity). One-shot use
    /// case: 0.93.0 renamed `com.mira.coding` → `com.mira.claudecode`.
    /// Returns the number of rows migrated. Idempotent — running twice
    /// just reports 0 on the second run.
    pub fn rename_skill(
        &self,
        old_skill_id: &str,
        new_skill_id: &str,
    ) -> Result<usize, SecretsError> {
        if old_skill_id == new_skill_id {
            return Ok(0);
        }
        // Snapshot the rows we need to rewrite. `list(...)` returns
        // metadata only; we still need to call `get(...)` for plaintext.
        // Scoped block so the prepared statement and connection guard
        // both drop before we re-enter via `set` / `get`, which take
        // their own lock.
        let rows: Vec<(Scope, String, String)> = {
            let conn = self.lock()?;
            let mut stmt = conn.prepare(
                "SELECT scope, scope_id, key
                   FROM skill_secrets
                  WHERE skill_id = ?1",
            )?;
            stmt.query_map(params![old_skill_id], |r| {
                Ok((
                    Scope::from_str(&r.get::<_, String>(0)?).unwrap_or(Scope::System),
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?
        };

        if rows.is_empty() {
            return Ok(0);
        }

        let mut migrated = 0usize;
        for (scope, scope_id, key) in rows {
            let plaintext = match self.get(scope, &scope_id, old_skill_id, &key)? {
                Some(p) => p,
                None    => continue, // raced with a delete; skip.
            };
            self.set(scope, &scope_id, new_skill_id, &key, &plaintext)?;
            self.delete(scope, &scope_id, old_skill_id, &key)?;
            migrated += 1;
        }
        if migrated > 0 {
            info!(
                "skill secrets migrated: {} → {} ({} row(s))",
                old_skill_id, new_skill_id, migrated,
            );
        }
        Ok(migrated)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SecretsError> {
        Ok(self.conn.lock().expect("secrets conn"))
    }

    fn try_get_logged(
        &self,
        scope:    Scope,
        scope_id: &str,
        skill_id: &str,
        key:      &str,
    ) -> Option<String> {
        match self.get(scope, scope_id, skill_id, key) {
            Ok(opt) => opt,
            Err(e) => {
                warn!(
                    "skill secret decrypt failed (skipped): scope={} skill={} key={}: {e}",
                    scope.as_str(), skill_id, key,
                );
                None
            }
        }
    }
}

/// Build the AAD string that binds a ciphertext to its row identity.
/// Format is unambiguous (`\0`-separated, no escape needed because
/// the components are constrained: scope is one of two literals,
/// scope_id is a UUID or empty, skill_id and key are validated).
fn associated_data(scope: Scope, scope_id: &str, skill_id: &str, key: &str) -> String {
    format!("{}\0{}\0{}\0{}", scope.as_str(), scope_id, skill_id, key)
}

/// Env-var keys must be the conservative POSIX-y subset: ASCII
/// alphanumerics + underscore, leading char alpha or underscore.
/// Rejects sneaky names like `LD_PRELOAD\nFOO=evil` that would
/// inject extra env on a naive `KEY=value` round-trip.
fn validate_env_key(key: &str) -> Result<(), SecretsError> {
    if key.is_empty() || key.len() > 128 {
        return Err(SecretsError::InvalidKeyName(key.to_string()));
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(SecretsError::InvalidKeyName(key.to_string()));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(SecretsError::InvalidKeyName(key.to_string()));
        }
    }
    Ok(())
}

/// Prefix-only id in logs so a leaked log file doesn't fingerprint
/// every user. UUIDs are guessable by prefix length anyway, so
/// 8 chars is enough to disambiguate during debugging.
fn redact_id(id: &str) -> String {
    if id.is_empty() { return "(system)".into(); }
    let head: String = id.chars().take(8).collect();
    format!("{head}…")
}

/// Load the 32-byte master key from `path`, or create a new random
/// one if missing. The file is hex-encoded so it's diff-able and
/// safe to copy/paste during recovery. Created with mode 0600 on
/// Unix; on other platforms we rely on the user-home dir's perms.
/// Load (or create, 0600) the instance master key — shared by every encrypted
/// store on this host (skill secrets, calendar CalDAV creds, …) so there's one
/// key to back up. Reused by [`crate::calendar::store`].
pub(crate) fn load_or_create_master_key(path: &Path) -> Result<[u8; 32], SecretsError> {
    if path.exists() {
        let s = std::fs::read_to_string(path)?;
        let trimmed = s.trim();
        let bytes = hex::decode(trimmed)
            .map_err(|_| SecretsError::BadMasterKey(format!("not hex ({} chars)", trimmed.len())))?;
        if bytes.len() != 32 {
            return Err(SecretsError::BadMasterKey(format!("{} bytes", bytes.len())));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        debug!("master key loaded from {:?}", path);
        return Ok(out);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    write_master_key(path, &key)?;
    info!("master key generated at {:?} (0600). Back this file up — losing it discards the secrets store.", path);
    Ok(key)
}

#[cfg(unix)]
fn write_master_key(path: &Path, key: &[u8; 32]) -> Result<(), SecretsError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    writeln!(f, "{}", hex::encode(key))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_master_key(path: &Path, key: &[u8; 32]) -> Result<(), SecretsError> {
    std::fs::write(path, hex::encode(key))?;
    Ok(())
}

#[allow(dead_code)] // public surface for v1.0+ ops
pub fn default_paths(data_dir: &Path) -> (PathBuf, PathBuf) {
    (
        data_dir.join("skill_secrets.db"),
        data_dir.join("master.key"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (SecretsStore, tempfile::TempDir) {
        let d = tempdir().unwrap();
        let s = SecretsStore::open(
            &d.path().join("secrets.db"),
            &d.path().join("master.key"),
        ).unwrap();
        (s, d)
    }

    #[test]
    fn round_trip_system_scope() {
        let (s, _d) = store();
        s.set(Scope::System, "", "com.mira.claudecode", "ANTHROPIC_API_KEY", "sk-abc-123").unwrap();
        assert_eq!(
            s.get(Scope::System, "", "com.mira.claudecode", "ANTHROPIC_API_KEY").unwrap(),
            Some("sk-abc-123".into())
        );
    }

    #[test]
    fn round_trip_user_scope() {
        let (s, _d) = store();
        s.set(Scope::User, "alice", "com.mira.claudecode", "ANTHROPIC_API_KEY", "alice-key").unwrap();
        s.set(Scope::User, "bob",   "com.mira.claudecode", "ANTHROPIC_API_KEY", "bob-key").unwrap();
        assert_eq!(s.get(Scope::User, "alice", "com.mira.claudecode", "ANTHROPIC_API_KEY").unwrap().as_deref(), Some("alice-key"));
        assert_eq!(s.get(Scope::User, "bob",   "com.mira.claudecode", "ANTHROPIC_API_KEY").unwrap().as_deref(), Some("bob-key"));
    }

    #[test]
    fn user_secret_shadows_system() {
        let (s, _d) = store();
        s.set(Scope::System, "", "com.mira.claudecode", "ANTHROPIC_API_KEY", "system-key").unwrap();
        s.set(Scope::User, "alice", "com.mira.claudecode", "ANTHROPIC_API_KEY", "alice-key").unwrap();
        let env = s.env_vars_for(Some("alice"), "com.mira.claudecode");
        assert_eq!(env.get("ANTHROPIC_API_KEY").map(String::as_str), Some("alice-key"));
        let env_no_user = s.env_vars_for(None, "com.mira.claudecode");
        assert_eq!(env_no_user.get("ANTHROPIC_API_KEY").map(String::as_str), Some("system-key"));
    }

    #[test]
    fn missing_secret_returns_none() {
        let (s, _d) = store();
        assert!(s.get(Scope::User, "alice", "com.mira.claudecode", "MISSING").unwrap().is_none());
        assert!(s.env_vars_for(Some("alice"), "com.mira.claudecode").is_empty());
    }

    #[test]
    fn list_returns_metadata_only() {
        let (s, _d) = store();
        s.set(Scope::User, "alice", "com.mira.claudecode", "K1", "v1").unwrap();
        s.set(Scope::User, "alice", "com.mira.claudecode", "K2", "v2").unwrap();
        let entries = s.list(Scope::User, "alice", "com.mira.claudecode").unwrap();
        let keys: Vec<_> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["K1", "K2"]);
    }

    #[test]
    fn delete_removes_secret() {
        let (s, _d) = store();
        s.set(Scope::User, "alice", "com.mira.claudecode", "K1", "v").unwrap();
        assert!(s.delete(Scope::User, "alice", "com.mira.claudecode", "K1").unwrap());
        assert!(s.get(Scope::User, "alice", "com.mira.claudecode", "K1").unwrap().is_none());
        // Idempotent.
        assert!(!s.delete(Scope::User, "alice", "com.mira.claudecode", "K1").unwrap());
    }

    #[test]
    fn env_var_key_validation() {
        let (s, _d) = store();
        for bad in ["", "1FOO", "FOO BAR", "FOO=BAR", "FOO\nBAR", "✨", &"X".repeat(129)] {
            assert!(s.set(Scope::System, "", "com.x", bad, "v").is_err(), "must reject {bad:?}");
        }
        for good in ["FOO", "_FOO", "FOO_BAR", "anthropic_api_key", "X1Y2"] {
            s.set(Scope::System, "", "com.x", good, "v").expect("must accept");
        }
    }

    #[test]
    fn rename_skill_migrates_rows_with_re_encryption() {
        // The 0.93.0 com.mira.coding → com.mira.claudecode migration.
        // Plain UPDATE-skill_id wouldn't decrypt because the AAD binds
        // ciphertext to (scope, scope_id, skill_id, key); rename_skill
        // must decrypt → re-encrypt under the new identity.
        let (s, _d) = store();
        s.set(Scope::System, "", "com.mira.coding", "ANTHROPIC_API_KEY", "sk-sys").unwrap();
        s.set(Scope::User, "alice", "com.mira.coding", "ANTHROPIC_API_KEY", "alice-key").unwrap();
        s.set(Scope::User, "bob",   "com.mira.coding", "ANTHROPIC_BASE_URL", "https://x").unwrap();

        let n = s.rename_skill("com.mira.coding", "com.mira.claudecode").unwrap();
        assert_eq!(n, 3);

        // Old rows are gone.
        assert!(s.get(Scope::System, "", "com.mira.coding", "ANTHROPIC_API_KEY").unwrap().is_none());
        assert!(s.list(Scope::User, "alice", "com.mira.coding").unwrap().is_empty());

        // New rows decrypt to the original plaintext.
        assert_eq!(
            s.get(Scope::System, "", "com.mira.claudecode", "ANTHROPIC_API_KEY").unwrap().as_deref(),
            Some("sk-sys"),
        );
        assert_eq!(
            s.get(Scope::User, "alice", "com.mira.claudecode", "ANTHROPIC_API_KEY").unwrap().as_deref(),
            Some("alice-key"),
        );
        assert_eq!(
            s.get(Scope::User, "bob", "com.mira.claudecode", "ANTHROPIC_BASE_URL").unwrap().as_deref(),
            Some("https://x"),
        );

        // Idempotent — running again with no old rows is a no-op.
        let n2 = s.rename_skill("com.mira.coding", "com.mira.claudecode").unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn ciphertext_swap_between_rows_fails_closed() {
        // A confused-deputy attack: an attacker with DB write but no
        // master key copies the ciphertext from row A onto row B,
        // hoping B's reader will decrypt to A's value. The AAD binds
        // the ciphertext to (scope, scope_id, skill_id, key) so this
        // fails at the auth-tag check, not silently.
        let (s, d) = store();
        s.set(Scope::System, "", "com.x", "K", "value-x").unwrap();
        s.set(Scope::System, "", "com.y", "K", "value-y").unwrap();
        // Forge by direct DB write — copy x's ciphertext+nonce onto y's row.
        let conn = Connection::open(d.path().join("secrets.db")).unwrap();
        conn.execute(
            "UPDATE skill_secrets SET ciphertext = (
                SELECT ciphertext FROM skill_secrets WHERE skill_id='com.x' AND key='K'
             ), nonce = (
                SELECT nonce FROM skill_secrets WHERE skill_id='com.x' AND key='K'
             ) WHERE skill_id='com.y' AND key='K'",
            [],
        ).unwrap();
        assert!(matches!(s.get(Scope::System, "", "com.y", "K"), Err(SecretsError::Decrypt)));
        assert_eq!(s.get(Scope::System, "", "com.x", "K").unwrap().as_deref(), Some("value-x"));
    }

    #[test]
    fn master_key_persists_across_reopen() {
        let d = tempdir().unwrap();
        let db = d.path().join("secrets.db");
        let mk = d.path().join("master.key");
        let s1 = SecretsStore::open(&db, &mk).unwrap();
        s1.set(Scope::System, "", "com.x", "K", "v").unwrap();
        drop(s1);
        let s2 = SecretsStore::open(&db, &mk).unwrap();
        assert_eq!(s2.get(Scope::System, "", "com.x", "K").unwrap().as_deref(), Some("v"));
    }

    #[test]
    fn corrupt_master_key_refuses_to_open() {
        let d = tempdir().unwrap();
        let mk = d.path().join("master.key");
        std::fs::write(&mk, "not-hex").unwrap();
        let r = SecretsStore::open(&d.path().join("secrets.db"), &mk);
        assert!(matches!(r, Err(SecretsError::BadMasterKey(_))));
    }
}
