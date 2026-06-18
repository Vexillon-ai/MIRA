// SPDX-License-Identifier: AGPL-3.0-or-later

//! Trust store for Skill publishers (slice A7).
//!
//! On disk: a single JSON file at `<data_dir>/skills/trust_store.json`
//! listing the public keys MIRA accepts as trusted publishers. The
//! Verified badge in the UI lights up when a Skill manifest's signature
//! checks against one of these keys.
//!
//! Schema (small enough that we don't bother with versioning yet):
//!
//! ```json
//! {
//!   "entries": [
//!     {
//!       "fingerprint": "<hex sha256 of the public key, lowercase, no separators>",
//!       "label":       "MIRA Team",
//!       "public_key":  "<base64 standard, no padding stripped — 32 raw bytes>",
//!       "added_at":    1777660000000
//!     }
//!   ]
//! }
//! ```
//!
//! We key by fingerprint rather than label: labels are user-editable
//! presentation, fingerprints are content-addressable identity.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::MiraError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustEntry {
    /// Hex SHA-256 of the 32-byte public key. Lowercase, 64 chars.
    pub fingerprint: String,
    pub label:       String,
    /// Base64 (standard, no-pad) of the 32-byte ed25519 public key.
    pub public_key:  String,
    /// When this key was added (unix ms). Audit only.
    pub added_at:    i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrustStoreFile {
    #[serde(default)]
    pub entries: Vec<TrustEntry>,
}

/// In-memory view: keyed by fingerprint for O(1) lookup, plus the
/// canonical entry list for serialisation.
#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    /// fingerprint → (label, parsed key)
    by_fingerprint: BTreeMap<String, (String, VerifyingKey)>,
    raw:            Vec<TrustEntry>,
    /// File the store was loaded from, so `save_self` doesn't need a
    /// caller-supplied path.
    path:           Option<PathBuf>,
}

impl TrustStore {
    pub fn empty() -> Self { Self::default() }

    /// Conventional location for the trust store, derived from the
    /// configured Skills directory.
    pub fn default_path(skills_dir: &Path) -> PathBuf {
        skills_dir.join("trust_store.json")
    }

    /// Load from `path` if it exists; otherwise return an empty store
    /// stamped with `path` so a subsequent `save_self` knows where to
    /// write. Missing-file is *not* an error — fresh installs start
    /// with no trusted publishers.
    pub fn load(path: &Path) -> Result<Self, MiraError> {
        let mut store = if path.exists() {
            let bytes = fs::read(path).map_err(|e| MiraError::DatabaseError(
                format!("read trust_store {}: {e}", path.display()),
            ))?;
            let file: TrustStoreFile = serde_json::from_slice(&bytes)
                .map_err(|e| MiraError::DatabaseError(format!("parse trust_store: {e}")))?;
            Self::from_file(file)?
        } else {
            Self::empty()
        };
        store.path = Some(path.to_path_buf());
        Ok(store)
    }

    fn from_file(file: TrustStoreFile) -> Result<Self, MiraError> {
        let mut store = Self::empty();
        for entry in file.entries {
            store.insert_entry(entry)?;
        }
        Ok(store)
    }

    fn insert_entry(&mut self, entry: TrustEntry) -> Result<(), MiraError> {
        let pk = parse_public_key_b64(&entry.public_key)
            .map_err(|e| MiraError::DatabaseError(format!(
                "trust entry {:?}: {e}", entry.fingerprint,
            )))?;
        // Cross-check fingerprint matches the key.
        let computed = fingerprint_of(&pk);
        if computed != entry.fingerprint.to_lowercase() {
            return Err(MiraError::DatabaseError(format!(
                "trust entry {:?} fingerprint mismatch (key fingerprints to {computed})",
                entry.fingerprint,
            )));
        }
        self.by_fingerprint.insert(entry.fingerprint.clone(), (entry.label.clone(), pk));
        self.raw.push(entry);
        Ok(())
    }

    /// Add a new trusted publisher key. The fingerprint is derived from
    /// the key — the caller doesn't supply it.
    pub fn add(&mut self, label: impl Into<String>, public_key_b64: &str) -> Result<TrustEntry, MiraError> {
        let pk = parse_public_key_b64(public_key_b64)
            .map_err(|e| MiraError::DatabaseError(format!("invalid public key: {e}")))?;
        let fp = fingerprint_of(&pk);
        if self.by_fingerprint.contains_key(&fp) {
            return Err(MiraError::DatabaseError(format!(
                "key {fp} is already in the trust store",
            )));
        }
        let entry = TrustEntry {
            fingerprint: fp.clone(),
            label:       label.into(),
            public_key:  public_key_b64.to_string(),
            added_at:    chrono::Utc::now().timestamp_millis(),
        };
        self.by_fingerprint.insert(fp, (entry.label.clone(), pk));
        self.raw.push(entry.clone());
        Ok(entry)
    }

    /// Remove an entry by fingerprint. Returns true when the entry existed.
    pub fn remove(&mut self, fingerprint: &str) -> bool {
        let fp = fingerprint.to_lowercase();
        let removed_map = self.by_fingerprint.remove(&fp).is_some();
        let before = self.raw.len();
        self.raw.retain(|e| e.fingerprint != fp);
        removed_map || self.raw.len() != before
    }

    pub fn lookup(&self, fingerprint: &str) -> Option<(&str, &VerifyingKey)> {
        self.by_fingerprint
            .get(&fingerprint.to_lowercase())
            .map(|(label, pk)| (label.as_str(), pk))
    }

    pub fn iter(&self) -> impl Iterator<Item = &TrustEntry> { self.raw.iter() }
    pub fn len(&self) -> usize { self.raw.len() }
    pub fn is_empty(&self) -> bool { self.raw.is_empty() }

    /// Persist to the path the store was loaded from. Atomic via temp +
    /// rename so a crash mid-write never leaves an empty file.
    pub fn save_self(&self) -> Result<(), MiraError> {
        let Some(path) = self.path.as_ref() else {
            return Err(MiraError::DatabaseError("trust store has no path to save to".into()));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| MiraError::DatabaseError(format!(
                "create trust store dir {}: {e}", parent.display(),
            )))?;
        }
        let file = TrustStoreFile { entries: self.raw.clone() };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|e| MiraError::DatabaseError(e.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &json).map_err(|e| MiraError::DatabaseError(format!(
            "write {}: {e}", tmp.display(),
        )))?;
        fs::rename(&tmp, path).map_err(|e| MiraError::DatabaseError(format!(
            "rename {} -> {}: {e}", tmp.display(), path.display(),
        )))?;
        Ok(())
    }
}

// ── helpers ───────────────────────────────────────────────────────────

/// Decode a base64 (standard, no-pad-tolerant) string into a
/// `VerifyingKey`. Errors carry context useful in API responses.
pub fn parse_public_key_b64(s: &str) -> Result<VerifyingKey, String> {
    // Accept padded or unpadded standard base64 — humans and tools
    // disagree about which to use.
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s.trim()))
        .map_err(|e| format!("invalid base64: {e}"))?;
    let arr: [u8; 32] = bytes.as_slice()
        .try_into()
        .map_err(|_| format!("expected 32 raw key bytes, got {}", bytes.len()))?;
    VerifyingKey::from_bytes(&arr)
        .map_err(|e| format!("not a valid ed25519 public key: {e}"))
}

/// Hex SHA-256 of the public key. Used as the fingerprint — short
/// enough for UI display, content-addressable for lookup.
pub fn fingerprint_of(pk: &VerifyingKey) -> String {
    let mut h = Sha256::new();
    h.update(pk.as_bytes());
    let digest = h.finalize();
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey};
    use rand::rngs::OsRng;

    fn keypair() -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key();
        (sk, pk)
    }

    #[test]
    fn add_lookup_remove() {
        let (_sk, pk) = keypair();
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(pk.as_bytes());

        let mut store = TrustStore::empty();
        let entry = store.add("Test Publisher", &pk_b64).unwrap();
        assert!(store.lookup(&entry.fingerprint).is_some());
        assert_eq!(store.len(), 1);

        assert!(store.remove(&entry.fingerprint));
        assert!(store.lookup(&entry.fingerprint).is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn add_rejects_duplicate_fingerprint() {
        let (_sk, pk) = keypair();
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(pk.as_bytes());

        let mut store = TrustStore::empty();
        store.add("first", &pk_b64).unwrap();
        let err = store.add("second copy of same key", &pk_b64).unwrap_err();
        assert!(err.to_string().contains("already in the trust store"));
    }

    #[test]
    fn add_rejects_garbage_key() {
        let mut store = TrustStore::empty();
        let err = store.add("garbage", "not-base64-at-all").unwrap_err();
        assert!(err.to_string().contains("invalid base64"));
    }

    #[test]
    fn fingerprint_is_stable() {
        let (_sk, pk) = keypair();
        let fp1 = fingerprint_of(&pk);
        let fp2 = fingerprint_of(&pk);
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 64);
        assert!(fp1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("trust_store.json");

        let (_sk, pk) = keypair();
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(pk.as_bytes());

        let mut store = TrustStore::load(&path).unwrap();
        store.add("Roundtrip Publisher", &pk_b64).unwrap();
        store.save_self().unwrap();

        let loaded = TrustStore::load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.iter().next().unwrap().label, "Roundtrip Publisher");
    }

    #[test]
    fn load_rejects_fingerprint_mismatch() {
        // Hand-craft a JSON file where fingerprint doesn't match the key.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("trust_store.json");
        let (_sk, pk) = keypair();
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(pk.as_bytes());
        let bad = serde_json::json!({
            "entries": [
                { "fingerprint": "0".repeat(64), "label": "x", "public_key": pk_b64, "added_at": 0 },
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();

        let err = TrustStore::load(&path).unwrap_err();
        assert!(err.to_string().contains("fingerprint mismatch"));
    }
}
