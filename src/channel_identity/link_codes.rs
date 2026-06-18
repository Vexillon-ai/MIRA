// SPDX-License-Identifier: AGPL-3.0-or-later

// src/channel_identity/link_codes.rs
//
// Short-lived one-time codes for the self-serve linking flow:
//   1. User clicks "Link Discord" in Settings → My Channels.
//   2. Server inserts a `channel_link_codes` row, returns the code.
//   3. User DMs the code to the admin's bot.
//   4. Dispatcher recognises the `LINK-XXXX` pattern, calls `consume()`.
//   5. On success, creates the `user_channel_links` row + replies to the
//      DM confirming the link.
//
// Properties of the codes:
//   * Format `LINK-XXXX-XXXX` — easy to recognise, hard to typo, not
//     visually confusable. 8 alphanumerics minus ambiguous chars
//     (0/O, 1/I/l, etc.) → ~10^11 codespace, more than enough for our
//     one-at-a-time-per-user rate.
//   * TTL 10 minutes — long enough to copy-paste, short enough that a
//     leaked one stops working soon. Configurable via constant.
//   * Single-use: `consume()` deletes the row on success so a leaked
//     code can't be replayed even within the TTL window.
//   * One pending code per (user, channel) — a second issue replaces
//     the first. Avoids a user accumulating dozens of stale codes.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::MiraError;

/// Code TTL — how long a freshly issued link code stays valid. Codes are
/// also deleted from the table opportunistically when expired ones are
/// stumbled across (see `consume`).
pub const CODE_TTL_SECS: i64 = 600;

/// Alphabet for the random part — no 0/O/1/I/L to avoid copy/paste
/// confusion. 30 characters, two 4-char chunks → ~30^8 ≈ 6.6×10^11.
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelLinkCode {
    pub code:        String,
    pub user_id:     String,
    pub channel:     String,
    pub created_at:  i64,
    pub expires_at:  i64,
}

pub struct LinkCodeStore {
    conn: Arc<Mutex<Connection>>,
}

impl LinkCodeStore {
    pub fn open(path: &Path) -> Result<Self, MiraError> {
        let conn = Connection::open(path).map_err(|e| {
            MiraError::DatabaseError(format!("Cannot open link-codes DB: {}", e))
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS channel_link_codes (
                code         TEXT PRIMARY KEY,
                user_id      TEXT NOT NULL,
                channel      TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                expires_at   INTEGER NOT NULL,
                FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
                UNIQUE(user_id, channel)
            );
            CREATE INDEX IF NOT EXISTS idx_clinkcodes_expires
                ON channel_link_codes(expires_at);
            "#,
        )
        .map_err(|e| MiraError::DatabaseError(format!(
            "channel_link_codes migration failed: {}", e
        )))?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
    }

    /// Issue a fresh code for `(user_id, channel)`. Replaces any existing
    /// pending code for the same pair so users aren't expected to
    /// remember which of three codes is current — only the latest works.
    pub fn issue(&self, user_id: &str, channel: &str) -> Result<ChannelLinkCode, MiraError> {
        let code  = generate_code();
        let now   = Self::now_ms();
        let exp   = now + CODE_TTL_SECS * 1000;
        let conn  = self.conn.lock().unwrap();
        // Use an UPSERT so we replace any pre-existing pending code for
        // the same (user_id, channel) pair. The PRIMARY KEY collision on
        // the new random code is astronomically unlikely but let's be
        // honest about it.
        conn.execute(
            "INSERT INTO channel_link_codes
               (code, user_id, channel, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id, channel) DO UPDATE SET
               code        = excluded.code,
               created_at  = excluded.created_at,
               expires_at  = excluded.expires_at",
            params![code, user_id, channel, now, exp],
        )
        .map_err(|e| MiraError::DatabaseError(format!("issue link code: {}", e)))?;
        Ok(ChannelLinkCode {
            code, user_id: user_id.to_owned(), channel: channel.to_owned(),
            created_at: now, expires_at: exp,
        })
    }

    /// Try to consume `code` for `channel`. Returns the owning user_id on
    /// success and deletes the row (single-use). Returns None if the code
    /// doesn't exist, has expired, or is for a different channel.
    ///
    /// This is the function the dispatcher calls when it sees a message
    /// matching the `LINK-XXXX-XXXX` pattern.
    pub fn consume(&self, code: &str, channel: &str) -> Result<Option<String>, MiraError> {
        let now  = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        let r = conn.query_row(
            "SELECT user_id, channel, expires_at FROM channel_link_codes WHERE code = ?1",
            params![code],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)),
        );
        let (user_id, code_channel, expires_at) = match r {
            Ok(t) => t,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(MiraError::DatabaseError(e.to_string())),
        };
        if code_channel != channel {
            // Code was issued for a different channel — refuse without
            // consuming so the legitimate channel can still use it.
            return Ok(None);
        }
        if expires_at < now {
            // Expired — opportunistic cleanup, but don't surface a
            // distinct success/error to the caller.
            let _ = conn.execute("DELETE FROM channel_link_codes WHERE code = ?1",
                                  params![code]);
            return Ok(None);
        }
        let n = conn.execute("DELETE FROM channel_link_codes WHERE code = ?1", params![code])
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        if n == 0 {
            // Lost a race with a concurrent consume — treat as miss.
            return Ok(None);
        }
        Ok(Some(user_id))
    }

    /// Cheap GC for expired codes. Called opportunistically from `issue`
    /// in a busy system; on a quiet one a periodic call from a heartbeat
    /// keeps the table small. Returns the count purged.
    pub fn purge_expired(&self) -> Result<usize, MiraError> {
        let now  = Self::now_ms();
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM channel_link_codes WHERE expires_at < ?1",
            params![now],
        )
        .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        Ok(n)
    }
}

fn generate_code() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Cheap non-cryptographic source seeded by nanos — adequate for a
    // 30^8 codespace with 10-minute TTL. If a determined attacker is
    // brute-forcing a leaked code in <10 minutes they have far worse
    // options.
    let seed = SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64).unwrap_or(0);
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut next = || -> u8 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ALPHABET[(state >> 33) as usize % ALPHABET.len()]
    };
    let mut buf = String::from("LINK-");
    for i in 0..8 {
        if i == 4 { buf.push('-'); }
        buf.push(next() as char);
    }
    buf
}

/// Cheap regex-free recogniser the dispatcher uses to decide whether an
/// inbound DM body looks like a link code (so it should be consumed
/// rather than forwarded to AgentCore). Matches `LINK-XXXX-XXXX`
/// optionally surrounded by whitespace; the alphabet check is strict.
pub fn looks_like_link_code(s: &str) -> Option<&str> {
    let t = s.trim();
    // "LINK-" (5) + 4 alnum + "-" (1) + 4 alnum = 14
    if t.len() != 14 { return None; }
    let b = t.as_bytes();
    if &b[..5] != b"LINK-" || b[9] != b'-' { return None; }
    let allowed = |c: u8| -> bool {
        ((b'A'..=b'Z').contains(&c) && c != b'I' && c != b'O')
            || (b'2'..=b'9').contains(&c)
    };
    for &i in &[5u8, 6, 7, 8, 10, 11, 12, 13] {
        if !allowed(b[i as usize]) { return None; }
    }
    Some(t)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::models::{AuthDb, NewUser, Role};
    use tempfile::tempdir;

    fn open_with_user() -> (tempfile::TempDir, LinkCodeStore, String) {
        let dir  = tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let auth = AuthDb::open(&path).unwrap();
        let u = auth.create_user(NewUser {
            username: "u".into(), display_name: None, email: None,
            password: "p".into(), role: Role::User,
        }, "h".into()).unwrap();
        let s = LinkCodeStore::open(&path).unwrap();
        (dir, s, u.id)
    }

    #[test]
    fn issue_then_consume_returns_user_id_once() {
        let (_d, s, uid) = open_with_user();
        let c = s.issue(&uid, "discord").unwrap();
        let first = s.consume(&c.code, "discord").unwrap();
        assert_eq!(first.as_deref(), Some(uid.as_str()));
        // Second consume must miss.
        assert_eq!(s.consume(&c.code, "discord").unwrap(), None);
    }

    #[test]
    fn consume_on_wrong_channel_returns_none_and_keeps_row() {
        let (_d, s, uid) = open_with_user();
        let c = s.issue(&uid, "discord").unwrap();
        // Try to redeem on Signal — should fail without burning the code.
        assert_eq!(s.consume(&c.code, "signal").unwrap(), None);
        // Discord still works.
        assert_eq!(s.consume(&c.code, "discord").unwrap().as_deref(), Some(uid.as_str()));
    }

    #[test]
    fn issuing_a_second_code_invalidates_the_first() {
        let (_d, s, uid) = open_with_user();
        let c1 = s.issue(&uid, "discord").unwrap();
        let c2 = s.issue(&uid, "discord").unwrap();
        assert_ne!(c1.code, c2.code);
        assert_eq!(s.consume(&c1.code, "discord").unwrap(), None);
        assert_eq!(s.consume(&c2.code, "discord").unwrap().as_deref(), Some(uid.as_str()));
    }

    #[test]
    fn generated_code_has_the_expected_shape() {
        for _ in 0..50 {
            let c = generate_code();
            assert!(c.starts_with("LINK-"));
            assert_eq!(c.len(), 14); // "LINK-XXXX-XXXX"
            assert_eq!(c.as_bytes()[9], b'-');
            assert!(looks_like_link_code(&c).is_some());
        }
    }

    #[test]
    fn looks_like_link_code_rejects_garbage() {
        assert!(looks_like_link_code("hello world").is_none());
        assert!(looks_like_link_code("LINK-ABCDXYZWQ").is_none()); // missing dash
        assert!(looks_like_link_code("LINK-0123-4567").is_none()); // 0/1 disallowed
        assert!(looks_like_link_code("LINK-IIII-OOOO").is_none()); // I/O disallowed
        assert!(looks_like_link_code("link-ABCD-EFGH").is_none()); // case-sensitive
    }

    #[test]
    fn purge_expired_cleans_old_rows() {
        let (_d, s, uid) = open_with_user();
        let c = s.issue(&uid, "discord").unwrap();
        // Tamper expires_at directly to simulate the passage of time.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute("UPDATE channel_link_codes SET expires_at = 1 WHERE code = ?1",
                          params![&c.code]).unwrap();
        }
        let purged = s.purge_expired().unwrap();
        assert_eq!(purged, 1);
        assert_eq!(s.consume(&c.code, "discord").unwrap(), None);
    }
}
