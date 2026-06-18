// SPDX-License-Identifier: AGPL-3.0-or-later

//! Optional passphrase encryption for backup archives.
//!
//! AES-256-GCM payload + argon2id key derivation. Reuses the `aes-gcm`
//! and `argon2` crates already in the tree (skill-secrets vault uses
//! AES-GCM; auth uses argon2 for passwords), so no new dep weight.
//!
//! ## File format (all little-endian where it matters)
//!
//! ```text
//! ┌──────────┬──────────┬──────────┬──────────────────────┐
//! │ magic 8B │ nonce12B │ salt 16B │ ciphertext+tag (var) │
//! └──────────┴──────────┴──────────┴──────────────────────┘
//! ```
//!
//! `magic` = ASCII `"MIRABK01"` — version-tagged so the format can be
//! evolved without breaking detection. `nonce` is per-archive random
//! (AES-GCM requires unique nonces per key — fresh key per archive
//! makes that trivially safe). `salt` feeds argon2id. The aes-gcm
//! crate appends the 16-byte auth tag to ciphertext automatically.
//!
//! ## Argon2id params
//!
//! Defaults from OWASP (m=19 MiB, t=2, p=1). One-shot at backup /
//! restore time, ~50–200 ms on modern hardware — fine for a manual
//! operation, prohibitive for an online attacker.

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use argon2::{Argon2, Algorithm, Version, Params};
use rand::RngCore;

/// 8-byte magic prefix identifying a MIRA-encrypted backup. Read the
/// first 8 bytes of any candidate file to decide whether decryption
/// is needed — plain `.tar.gz` starts with the gzip magic `1f 8b`.
pub const ENCRYPTED_MAGIC: &[u8; 8] = b"MIRABK01";

/// Length of the per-archive nonce (AES-GCM standard).
const NONCE_LEN: usize = 12;
/// Length of the argon2id salt.
const SALT_LEN: usize  = 16;
/// Header overhead before ciphertext: magic + nonce + salt.
pub const HEADER_LEN: usize = ENCRYPTED_MAGIC.len() + NONCE_LEN + SALT_LEN;

/// Returns true when the bytes start with the MIRA-encrypted magic.
/// Bytewise check — no allocation, no false positives on gzip magic.
pub fn is_encrypted(bytes: &[u8]) -> bool {
    bytes.len() >= ENCRYPTED_MAGIC.len()
        && &bytes[..ENCRYPTED_MAGIC.len()] == ENCRYPTED_MAGIC
}

/// Derive a 32-byte AES-256 key from `passphrase` + `salt` via argon2id.
fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let argon = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::default(),
    );
    let mut key = [0u8; 32];
    argon.hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| format!("argon2id KDF failed: {e}"))?;
    Ok(key)
}

/// Encrypt `plaintext` with `passphrase`. Returns the framed archive:
/// magic ‖ nonce ‖ salt ‖ aes-gcm(plaintext)‖tag. Random nonce + salt
/// per call so the same passphrase produces a different ciphertext
/// each time (proper IND-CPA).
pub fn encrypt(plaintext: &[u8], passphrase: &str) -> Result<Vec<u8>, String> {
    if passphrase.is_empty() {
        return Err("passphrase must not be empty".into());
    }
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce_bytes);
    let key_bytes = derive_key(passphrase, &salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext)
        .map_err(|e| format!("aes-gcm encrypt failed: {e}"))?;
    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(ENCRYPTED_MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&salt);
    out.extend(ciphertext);
    Ok(out)
}

/// Decrypt a framed archive produced by `encrypt`. Returns the
/// plaintext on success; descriptive error on wrong passphrase
/// (authentication tag mismatch surfaces as a generic "decryption
/// failed" — caller should treat that as user error, not corruption).
pub fn decrypt(framed: &[u8], passphrase: &str) -> Result<Vec<u8>, String> {
    if framed.len() < HEADER_LEN + 16 {
        return Err("encrypted payload truncated or not a MIRA-encrypted backup".into());
    }
    if !is_encrypted(framed) {
        return Err("not a MIRA-encrypted backup (missing magic header)".into());
    }
    let nonce_bytes = &framed[ENCRYPTED_MAGIC.len()..ENCRYPTED_MAGIC.len() + NONCE_LEN];
    let salt        = &framed[ENCRYPTED_MAGIC.len() + NONCE_LEN..HEADER_LEN];
    let ciphertext  = &framed[HEADER_LEN..];
    let key_bytes = derive_key(passphrase, salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ciphertext)
        .map_err(|_| "decryption failed — wrong passphrase or corrupted archive".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_recovers_plaintext() {
        let pt = b"hello MIRA backup contents go here, including binary \x00\xff data";
        let ct = encrypt(pt, "correct horse battery staple").unwrap();
        assert!(is_encrypted(&ct));
        let pt2 = decrypt(&ct, "correct horse battery staple").unwrap();
        assert_eq!(pt, pt2.as_slice());
    }

    #[test]
    fn wrong_passphrase_fails_cleanly() {
        let ct = encrypt(b"secret", "right").unwrap();
        let err = decrypt(&ct, "wrong").unwrap_err();
        assert!(err.contains("decryption failed"), "got: {err}");
    }

    #[test]
    fn same_plaintext_produces_different_ciphertexts() {
        let ct1 = encrypt(b"x", "pw").unwrap();
        let ct2 = encrypt(b"x", "pw").unwrap();
        assert_ne!(ct1, ct2, "nonce + salt must randomise each call");
    }

    #[test]
    fn empty_passphrase_refused() {
        assert!(encrypt(b"x", "").is_err());
    }

    #[test]
    fn is_encrypted_doesnt_falsely_match_gzip_magic() {
        // Real gzip stream starts with 1f 8b
        let gzip = [0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(!is_encrypted(&gzip));
    }

    #[test]
    fn truncated_payload_rejected() {
        let err = decrypt(b"MIRABK01too-short", "x").unwrap_err();
        assert!(err.contains("truncated") || err.contains("Missing"), "got: {err}");
    }
}
