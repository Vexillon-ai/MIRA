// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/tokens.rs
//! JWT access tokens + refresh token helpers.

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::models::User;
use crate::MiraError;

// ── Claims ────────────────────────────────────────────────────────────────────

/// JWT claims payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Subject: user id.
    pub sub:  String,
    /// Role string.
    pub role: String,
    /// Expiry (Unix seconds).
    pub exp:  i64,
    /// Issued-at (Unix seconds).
    pub iat:  i64,
}

// ── TokenPair ─────────────────────────────────────────────────────────────────

/// A short-lived access token + long-lived raw refresh token.
#[derive(Debug, Clone)]
pub struct TokenPair {
    /// Signed JWT (HS256, 15-minute lifetime).
    pub access_token:  String,
    /// Raw 32-byte refresh token (hex-encoded, 64 chars). Store its hash only.
    pub refresh_token: String,
}

// ── Functions ─────────────────────────────────────────────────────────────────

pub fn issue_token_pair(user: &User, secret: &str) -> Result<TokenPair, MiraError> {
    let now = unix_now_secs();
    let exp = now + 900; // 15 minutes

    let claims = Claims {
        sub:  user.id.clone(),
        role: user.role.as_str().to_owned(),
        exp,
        iat: now,
    };

    let access_token = encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| MiraError::AuthError(format!("JWT signing failed: {}", e)))?;

    // 32 random bytes as hex string.
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let refresh_token = hex::encode(raw);

    Ok(TokenPair { access_token, refresh_token })
}

/// Issue a signed access token with an arbitrary TTL. Used for the local
/// bearer token the server writes at startup for same-host TUI use
/// (see `Gateway::mint_local_token`). Does NOT allocate a refresh token —
/// callers just want a single long-lived JWT, and refresh rotation would
/// complicate the TUI client.
pub fn issue_long_lived_access_token(
    user:     &User,
    secret:   &str,
    ttl_secs: i64,
) -> Result<String, MiraError> {
    let now    = unix_now_secs();
    let claims = Claims {
        sub:  user.id.clone(),
        role: user.role.as_str().to_owned(),
        exp:  now + ttl_secs,
        iat:  now,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| MiraError::AuthError(format!("JWT signing failed: {}", e)))
}

pub fn verify_access_token(token: &str, secret: &str) -> Result<Claims, MiraError> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;

    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|e| MiraError::AuthError(format!("JWT verification failed: {}", e)))?;

    Ok(data.claims)
}

/// SHA-256 hex digest of the raw refresh token string.
pub fn hash_refresh_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
