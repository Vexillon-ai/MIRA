// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/local.rs
//! LocalAuthService — password hashing, login, token lifecycle.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::{Rng, RngCore};
use tracing::{info, warn};

use crate::auth::models::{AuthDb, NewUser, Role, User, UserProfile};
use crate::auth::groups::{Group, NewGroup, UpdateGroup};
use crate::auth::oidc::sanitize_username;
use crate::auth::tokens::{hash_refresh_token, issue_long_lived_access_token, issue_token_pair, TokenPair};
use crate::MiraError;

// ── LocalAuthService ──────────────────────────────────────────────────────────

// Auth service: user management, login, JWT + refresh token lifecycle.
#[derive(Clone)]
pub struct LocalAuthService {
    db:         Arc<AuthDb>,
    jwt_secret: String,
    session_ms: i64, // refresh token lifetime in milliseconds
}

impl LocalAuthService {
    pub fn new(db_path: &Path, jwt_secret: String, session_days: u64) -> Result<Self, MiraError> {
        let db = AuthDb::open(db_path)?;
        let session_ms = (session_days * 24 * 60 * 60 * 1000) as i64;
        Ok(Self {
            db: Arc::new(db),
            jwt_secret,
            session_ms,
        })
    }

    // ── Password helpers ──────────────────────────────────────────────────────

    pub fn hash_password(password: &str) -> Result<String, MiraError> {
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| MiraError::AuthError(format!("Password hashing failed: {}", e)))?;
        Ok(hash.to_string())
    }

    pub fn verify_password(password: &str, hash: &str) -> Result<bool, MiraError> {
        let parsed = PasswordHash::new(hash)
            .map_err(|e| MiraError::AuthError(format!("Invalid password hash: {}", e)))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    }

    // Shared handle to the underlying `AuthDb`. Exposed for the
    // 0.106.0 health-audit detectors and the IpBanLayer cache, which
    // both need to read auth_failed_logins / auth_ip_bans rows
    // without going through the service surface.
    pub fn db_arc(&self) -> Arc<AuthDb> { Arc::clone(&self.db) }

    // ── Login ─────────────────────────────────────────────────────────────────

    pub async fn login(
        &self,
        username:   &str,
        password:   &str,
        user_agent: Option<&str>,
        ip:         Option<&str>,
    ) -> Result<(TokenPair, User), MiraError> {
        // Bookkeeping helper — fire-and-forget on every Unauthorized
        // path so the failed-logins detector + IP-ban auto-action have
        // data to work with. Logged-only on bookkeeping failure; the
        // user's auth response must not depend on this side-effect.
        let record_fail = |reason: &str| {
            if let Err(e) = self.db.record_failed_login(ip, Some(username), reason) {
                tracing::debug!("record_failed_login skipped: {e}");
            }
        };

        // Fetch user — map not-found to generic Unauthorized to avoid username enumeration.
        let user = match self.db.find_by_username(username)? {
            Some(u) => u,
            None    => { record_fail("unknown_user"); return Err(MiraError::Unauthorized); }
        };

        if !user.is_active {
            record_fail("user_inactive");
            return Err(MiraError::Unauthorized);
        }

        // Fetch hash separately (not included in User struct for safety).
        let (_, hash) = self.db.get_password_hash_by_username(username)?;

        if !Self::verify_password(password, &hash)? {
            record_fail("bad_password");
            return Err(MiraError::Unauthorized);
        }

        // Self-service onboarding gate — password is correct but an
        // open-signup account may still be awaiting admin approval. Surfaced
        // distinctly (PendingApproval, not Unauthorized) so the user is told
        // to wait; safe because it only fires post-password.
        if !self.db.is_user_approved(&user.id)? {
            record_fail("pending_approval");
            return Err(MiraError::PendingApproval);
        }

        let pair = self.issue_session(&user, user_agent, ip)?;
        Ok((pair, user))
    }

    // Mint a fresh access+refresh token pair for an already-authenticated
    // user and persist the refresh token. The shared tail of every login
    // path (password, and — Q2 #11 — OIDC SSO once the IdP has vouched for
    // the user). Does NOT verify credentials; callers must have already
    // established the user's identity.
    pub fn issue_session(
        &self,
        user:       &User,
        user_agent: Option<&str>,
        ip:         Option<&str>,
    ) -> Result<TokenPair, MiraError> {
        let pair = issue_token_pair(user, &self.jwt_secret)?;
        let token_hash = hash_refresh_token(&pair.refresh_token);
        let expires_at = self.refresh_expires_at();
        self.db.save_refresh_token(&user.id, &token_hash, expires_at, user_agent, ip)?;
        self.db.update_last_login(&user.id)?;
        Ok(pair)
    }

    // ── Refresh (token rotation) ──────────────────────────────────────────────

    pub async fn refresh(&self, raw_refresh_token: &str) -> Result<(TokenPair, User), MiraError> {
        let token_hash = hash_refresh_token(raw_refresh_token);

        match self.db.find_refresh_token(&token_hash)? {
            None => {
                // Token not found → possible theft; revocation is a no-op since
                // we don't know the user_id without the token. Return Unauthorized.
                warn!("Refresh token not found — possible theft attempt");
                Err(MiraError::Unauthorized)
            }
            Some(stored) => {
                if stored.revoked {
                    // Revoked token reuse — revoke everything for this user (theft detection).
                    warn!("Revoked refresh token reused for user {} — revoking all tokens", stored.user_id);
                    self.db.revoke_all_for_user(&stored.user_id)?;
                    return Err(MiraError::Unauthorized);
                }

                let now_ms = unix_now_ms();
                if stored.expires_at < now_ms {
                    return Err(MiraError::AuthError("Refresh token expired".into()));
                }

                // Revoke old token.
                self.db.revoke_refresh_token(&token_hash)?;

                let user = self
                    .db
                    .find_by_id(&stored.user_id)?
                    .ok_or(MiraError::Unauthorized)?;

                if !user.is_active {
                    return Err(MiraError::Unauthorized);
                }

                let pair = issue_token_pair(&user, &self.jwt_secret)?;
                let new_hash    = hash_refresh_token(&pair.refresh_token);
                let expires_at  = self.refresh_expires_at();
                self.db.save_refresh_token(&user.id, &new_hash, expires_at, None, None)?;

                Ok((pair, user))
            }
        }
    }

    // ── Logout ────────────────────────────────────────────────────────────────

    pub fn logout(&self, raw_refresh_token: &str) -> Result<(), MiraError> {
        let token_hash = hash_refresh_token(raw_refresh_token);
        self.db.revoke_refresh_token(&token_hash)
    }

    // ── JWT verification ──────────────────────────────────────────────────────

    pub fn verify_token(&self, token: &str) -> Result<crate::auth::tokens::Claims, MiraError> {
        crate::auth::tokens::verify_access_token(token, &self.jwt_secret)
    }

    // ── User management ───────────────────────────────────────────────────────

    pub fn get_user(&self, user_id: &str) -> Result<Option<User>, MiraError> {
        self.db.find_by_id(user_id)
    }

    // Resolve a phone number to a MIRA user. Channel listeners use this to
    // stamp inbound Signal messages with the right user UUID so memory and
    // profile context follow the user across channels.
    pub fn find_by_phone(&self, phone: &str) -> Result<Option<User>, MiraError> {
        self.db.find_by_phone(phone)
    }

    pub fn list_users(&self) -> Result<Vec<User>, MiraError> {
        self.db.list_users()
    }

    pub fn create_user(&self, req: NewUser) -> Result<User, MiraError> {
        let hash = Self::hash_password(&req.password)?;
        self.db.create_user(req, hash)
    }

    // ── Device pairing (QR mobile onboarding, 0.282.0) ────────────────────────

    /// Start a pairing: mint a random single-use secret, store only its
    /// SHA-256 hash, and return `(pairing_id, raw_secret, expires_at_ms)`.
    /// The raw secret is returned to the caller exactly once (embedded in
    /// the QR code) and never persisted or logged.
    pub fn start_device_pairing(
        &self,
        user_id:     &str,
        device_name: Option<&str>,
        ttl_secs:    i64,
    ) -> Result<(String, String, i64), MiraError> {
        let pairing_id = uuid::Uuid::new_v4().to_string();
        let mut raw = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut raw);
        let secret      = hex::encode(raw);
        let secret_hash = hash_refresh_token(&secret);
        let expires_at  = chrono::Utc::now().timestamp_millis() + ttl_secs * 1000;
        self.db.create_device_pairing(&pairing_id, &secret_hash, user_id, device_name, expires_at)?;
        Ok((pairing_id, secret, expires_at))
    }

    /// Claim a pairing with the raw secret. On `PairingClaim::Ok`, the
    /// caller mints a token pair via [`issue_session`].
    pub fn claim_device_pairing(
        &self,
        pairing_id: &str,
        secret:     &str,
    ) -> Result<crate::auth::models::PairingClaim, MiraError> {
        let secret_hash = hash_refresh_token(secret);
        self.db.claim_device_pairing(pairing_id, &secret_hash)
    }

    /// Status of a pairing the caller started (for the web UI to poll).
    pub fn device_pairing_status(
        &self,
        pairing_id: &str,
        owner_id:   &str,
    ) -> Result<Option<crate::auth::models::DevicePairingStatus>, MiraError> {
        self.db.device_pairing_status(pairing_id, owner_id)
    }

    // ── SSO / OIDC (Q2 #11) ──────────────────────────────────────────────────
    pub fn find_by_email(&self, email: &str) -> Result<Option<User>, MiraError> {
        self.db.find_by_email(email)
    }
    pub fn find_by_username(&self, username: &str) -> Result<Option<User>, MiraError> {
        self.db.find_by_username(username)
    }
    pub fn find_user_by_identity(&self, issuer: &str, subject: &str) -> Result<Option<User>, MiraError> {
        self.db.find_user_by_identity(issuer, subject)
    }
    pub fn link_identity(&self, issuer: &str, subject: &str, user_id: &str, provider_id: &str) -> Result<(), MiraError> {
        self.db.link_identity(issuer, subject, user_id, provider_id)
    }

    // Create an account for an SSO-authenticated user who has no existing
    // match. There is no usable password — we store a hash of a random
    // secret so the local password path can never succeed for this account
    // (they sign in only via the IdP). The preferred username is made unique
    // by appending a numeric suffix on collision.
    pub fn create_sso_user(
        &self,
        preferred_username: &str,
        email:        Option<&str>,
        display_name: Option<&str>,
        role:         Role,
    ) -> Result<User, MiraError> {
        let unusable: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(48)
            .map(char::from)
            .collect();
        let hash = Self::hash_password(&unusable)?;

        // Find a free username: base, base2, base3, …
        let base = sanitize_username(preferred_username);
        let mut candidate = base.clone();
        for n in 2..=1000 {
            if self.db.find_by_username(&candidate)?.is_none() {
                let new = NewUser {
                    username:     candidate.clone(),
                    display_name: display_name.map(str::to_owned),
                    email:        email.map(str::to_owned),
                    password:     unusable.clone(), // unused; we pass `hash` directly
                    role:         role.clone(),
                };
                return self.db.create_user(new, hash);
            }
            candidate = format!("{base}{n}");
        }
        Err(MiraError::AuthError("could not allocate a unique username for SSO user".into()))
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
        self.db.update_user(
            id, display_name, email, role, is_active,
            phone, preferred_contact, avatar,
            voice_prefs,
        )
    }

    pub fn set_avatar(&self, id: &str, avatar: Option<&str>) -> Result<User, MiraError> {
        self.db.set_avatar(id, avatar)
    }

    // ── Onboarding / profile pass-throughs ───────────────────────────────────

    pub fn get_profile(&self, user_id: &str) -> Result<Option<UserProfile>, MiraError> {
        self.db.get_profile(user_id)
    }

    pub fn upsert_profile_field<V: rusqlite::ToSql>(
        &self,
        user_id: &str,
        column:  &'static str,
        value:   V,
    ) -> Result<(), MiraError> {
        self.db.upsert_profile_field(user_id, column, value)
    }

    pub fn set_onboarding_progress(&self, user_id: &str, progress_json: &str) -> Result<(), MiraError> {
        self.db.set_onboarding_progress(user_id, progress_json)
    }

    pub fn mark_onboarded(&self, user_id: &str) -> Result<(), MiraError> {
        self.db.mark_onboarded(user_id)
    }

    pub fn reset_onboarding_profile(&self, user_id: &str) -> Result<(), MiraError> {
        self.db.reset_onboarding_profile(user_id)
    }

    pub fn delete_user(&self, id: &str) -> Result<(), MiraError> {
        self.db.delete_user(id)
    }

    // ── Groups ────────────────────────────────────────────────────────────────
    // Thin forwarders to AuthDb so handlers can work through LocalAuthService
    // without a second Extension.

    pub fn create_group(&self, new: NewGroup, created_by: &str) -> Result<Group, MiraError> {
        self.db.create_group(new, created_by)
    }
    pub fn list_groups(&self) -> Result<Vec<Group>, MiraError> { self.db.list_groups() }
    pub fn get_group(&self, id: &str) -> Result<Option<Group>, MiraError> { self.db.get_group(id) }
    pub fn update_group(&self, id: &str, up: UpdateGroup) -> Result<Group, MiraError> {
        self.db.update_group(id, up)
    }
    pub fn delete_group(&self, id: &str) -> Result<(), MiraError> { self.db.delete_group(id) }

    pub fn add_group_member(&self, group_id: &str, user_id: &str, added_by: &str) -> Result<(), MiraError> {
        self.db.add_member(group_id, user_id, added_by)
    }
    pub fn remove_group_member(&self, group_id: &str, user_id: &str) -> Result<(), MiraError> {
        self.db.remove_member(group_id, user_id)
    }
    pub fn list_group_members(&self, group_id: &str) -> Result<Vec<User>, MiraError> {
        self.db.list_members(group_id)
    }
    pub fn list_user_groups(&self, user_id: &str) -> Result<Vec<Group>, MiraError> {
        self.db.list_user_groups(user_id)
    }
    pub fn list_user_group_ids(&self, user_id: &str) -> Result<Vec<String>, MiraError> {
        self.db.list_user_group_ids(user_id)
    }
    pub fn is_group_member(&self, group_id: &str, user_id: &str) -> Result<bool, MiraError> {
        self.db.is_member(group_id, user_id)
    }

    // ── Capability RBAC ──────────────────────────────────────────────────────
    pub fn get_group_capabilities(
        &self,
        group_id: &str,
    ) -> Result<Option<crate::auth::CapabilityProfile>, MiraError> {
        self.db.get_group_capabilities(group_id)
    }
    pub fn set_group_capabilities(
        &self,
        group_id: &str,
        profile: Option<&crate::auth::CapabilityProfile>,
    ) -> Result<(), MiraError> {
        self.db.set_group_capabilities(group_id, profile)
    }
    pub fn get_user_capabilities(
        &self,
        user_id: &str,
    ) -> Result<Option<crate::auth::CapabilityProfile>, MiraError> {
        self.db.get_user_capabilities(user_id)
    }
    pub fn set_user_capabilities(
        &self,
        user_id: &str,
        profile: Option<&crate::auth::CapabilityProfile>,
    ) -> Result<(), MiraError> {
        self.db.set_user_capabilities(user_id, profile)
    }
    pub fn effective_capabilities(
        &self,
        user_id: &str,
        role: &crate::auth::Role,
    ) -> Result<crate::auth::CapabilityProfile, MiraError> {
        self.db.effective_capabilities(user_id, role)
    }

    pub fn change_password(&self, user_id: &str, new_password: &str) -> Result<(), MiraError> {
        let hash = Self::hash_password(new_password)?;
        self.db.change_password(user_id, hash)
    }

    // ── Self-service onboarding (Q2 #11) ─────────────────────────────────────

    // Mint an invite. Returns the stored record + the raw token (shown once).
    pub fn create_invite(
        &self,
        created_by: &str,
        role:       &str,
        email_hint: Option<&str>,
        max_uses:   i64,
        expires_at: Option<i64>,
    ) -> Result<(crate::auth::invites::Invite, String), MiraError> {
        let raw: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();
        let inv = self.db.create_invite(created_by, role, email_hint, max_uses, expires_at, &raw)?;
        Ok((inv, raw))
    }

    pub fn list_invites(&self) -> Result<Vec<crate::auth::invites::Invite>, MiraError> {
        self.db.list_invites()
    }
    pub fn revoke_invite(&self, id: &str) -> Result<(), MiraError> {
        self.db.revoke_invite(id)
    }
    pub fn find_invite_by_token(&self, token: &str) -> Result<Option<crate::auth::invites::Invite>, MiraError> {
        self.db.find_invite_by_token(token)
    }
    pub fn redeem_invite(&self, token: &str) -> Result<crate::auth::invites::Invite, MiraError> {
        self.db.redeem_invite(token)
    }

    // Create a user, optionally in the pending (`approved = false`) state.
    pub fn create_user_with_approval(&self, req: NewUser, approved: bool) -> Result<User, MiraError> {
        let user = self.create_user(req)?;
        if !approved {
            self.db.set_user_approved(&user.id, false)?;
        }
        Ok(user)
    }

    pub fn list_pending_users(&self) -> Result<Vec<User>, MiraError> {
        self.db.list_pending_users()
    }

    // Admin "sign out everywhere" — revoke all of a user's refresh tokens.
    // Returns how many live sessions were revoked. Access tokens are short
    // (15 min) JWTs, so the user is fully locked out within that window.
    pub fn revoke_all_sessions(&self, user_id: &str) -> Result<i64, MiraError> {
        let n = self.db.count_active_sessions(user_id)?;
        self.db.revoke_all_for_user(user_id)?;
        Ok(n)
    }
    pub fn count_active_sessions(&self, user_id: &str) -> Result<i64, MiraError> {
        self.db.count_active_sessions(user_id)
    }
    pub fn set_user_approved(&self, user_id: &str, approved: bool) -> Result<(), MiraError> {
        self.db.set_user_approved(user_id, approved)
    }

    // If no users exist, create an admin user with a random 12-char password.
    // Returns `Some(password)` if the account was created, `None` otherwise.
    pub fn ensure_admin_exists(&self) -> Result<Option<String>, MiraError> {
        if self.db.count_users()? > 0 {
            return Ok(None);
        }

        let password: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(12)
            .map(char::from)
            .collect();

        let hash = Self::hash_password(&password)?;
        self.db.create_user(
            NewUser {
                username:     "admin".to_string(),
                display_name: Some("Administrator".to_string()),
                email:        None,
                password:     password.clone(), // stored hashed via create_user, but we pass hash directly
                role:         Role::Admin,
            },
            hash,
        )?;

        info!("Created default admin user — change this password immediately");
        Ok(Some(password))
    }

    // Mint a long-lived JWT for the first active admin user. Used by the
    // server at startup to write a bearer token to disk for same-host TUI
    // use. Returns `None` if no admin account exists.
    pub fn issue_local_admin_token(&self, ttl_secs: i64) -> Result<Option<String>, MiraError> {
        let users = self.db.list_users()?;
        let admin = users
            .into_iter()
            .find(|u| matches!(u.role, Role::Admin) && u.is_active);

        let Some(admin) = admin else { return Ok(None); };
        let token = issue_long_lived_access_token(&admin, &self.jwt_secret, ttl_secs)?;
        Ok(Some(token))
    }

    // User id of the first active admin. TUI/CLI call this at startup so
    // local conversations stamp against the real admin row (and not the
    // legacy hardcoded `"local-user"` string) — needed for per-user
    // visibility, since shell access on this host implies admin.
    pub fn current_admin_user_id(&self) -> Result<Option<String>, MiraError> {
        let users = self.db.list_users()?;
        Ok(users
            .into_iter()
            .find(|u| matches!(u.role, Role::Admin) && u.is_active)
            .map(|u| u.id))
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn refresh_expires_at(&self) -> i64 {
        unix_now_ms() + self.session_ms
    }
}

fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
