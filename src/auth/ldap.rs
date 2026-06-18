// SPDX-License-Identifier: AGPL-3.0-or-later

// src/auth/ldap.rs
//! LDAP / Active Directory authentication (Q2 #11). Tried as a fallback when
//! local password auth fails (so the bootstrap admin + any local account keep
//! working, and a directory outage never locks everyone out).
//!
//! **Search-then-bind** (the robust, AD- and OpenLDAP-friendly pattern):
//!   1. connect (optionally STARTTLS-upgrade an `ldap://` link);
//!   2. bind as the service account and **search** for the user by filter;
//!   3. **re-bind as the user's DN** with the supplied password to verify it;
//!   4. read the email / display-name attributes and (optionally) check group
//!      membership.
//!
//! Security note: an LDAP bind with a DN and an **empty** password is an
//! "unauthenticated bind" that *succeeds* without checking anything — a
//! classic auth-bypass. We reject empty passwords before ever binding.

use std::time::Duration;

use ldap3::{LdapConnAsync, LdapConnSettings, Scope, SearchEntry};

use crate::auth::models::Role;
use crate::config::LdapConfig;
use crate::MiraError;

/// What a successful LDAP bind tells us about the user.
#[derive(Debug, Clone)]
pub struct LdapIdentity {
    pub username:     String,
    pub dn:           String,
    pub email:        Option<String>,
    pub display_name: Option<String>,
}

pub struct LdapService {
    cfg: LdapConfig,
}

impl LdapService {
    pub fn new(cfg: &LdapConfig) -> Self {
        Self { cfg: cfg.clone() }
    }

    pub fn is_enabled(&self) -> bool {
        self.cfg.enabled
            && !self.cfg.url.trim().is_empty()
            && !self.cfg.user_base_dn.trim().is_empty()
    }

    /// Stable issuer string used to bind LDAP identities in `user_identities`.
    pub fn realm(&self) -> String {
        format!("ldap:{}", self.cfg.url.trim())
    }

    /// Resolve the role for an auto-provisioned user, or an `Err(reason)` when
    /// auto-provisioning isn't permitted for this identity. Pure — testable.
    pub fn auto_provision_role(&self, email: Option<&str>) -> Result<Role, String> {
        if !self.cfg.auto_provision {
            return Err("No MIRA account is linked to this directory identity. Ask an administrator to create your account.".into());
        }
        let email = email.unwrap_or("").trim().to_lowercase();
        if email.is_empty() {
            return Err("The directory returned no email; cannot auto-provision.".into());
        }
        if !domain_allowed(&email, &self.cfg.allowed_domains) {
            return Err(format!("Email domain not permitted for auto-provisioning ({email})."));
        }
        Ok(if self.cfg.default_role.trim().eq_ignore_ascii_case("admin") {
            Role::Admin
        } else {
            Role::User
        })
    }

    /// Authenticate `username`/`password` against the directory. Returns the
    /// identity on success; `Unauthorized` when the directory rejects the
    /// credentials or the user isn't found / not in the required group.
    pub async fn authenticate(&self, username: &str, password: &str) -> Result<LdapIdentity, MiraError> {
        // Reject empty password up-front — see the unauthenticated-bind note.
        if password.is_empty() {
            return Err(MiraError::Unauthorized);
        }

        let settings = LdapConnSettings::new()
            .set_starttls(self.cfg.starttls)
            .set_conn_timeout(Duration::from_secs(10));
        let (conn, mut ldap) = LdapConnAsync::with_settings(settings, self.cfg.url.trim())
            .await
            .map_err(|e| MiraError::ProviderError(format!("ldap connect: {e}")))?;
        ldap3::drive!(conn);

        // 1. Bind as the service account (or anonymously) to run the search.
        if !self.cfg.bind_dn.trim().is_empty() {
            ldap.simple_bind(self.cfg.bind_dn.trim(), &self.cfg.bind_password)
                .await
                .map_err(|e| MiraError::ProviderError(format!("ldap service bind: {e}")))?
                .success()
                .map_err(|_| MiraError::ProviderError("ldap service-account bind rejected".into()))?;
        }

        // 2. Search for the user.
        let filter = build_filter(&self.cfg.user_filter, username);
        let attrs = vec![
            self.cfg.attr_email.as_str(),
            self.cfg.attr_display_name.as_str(),
            "memberOf",
        ];
        let (rs, _res) = ldap
            .search(self.cfg.user_base_dn.trim(), Scope::Subtree, &filter, attrs)
            .await
            .map_err(|e| MiraError::ProviderError(format!("ldap search: {e}")))?
            .success()
            .map_err(|e| MiraError::ProviderError(format!("ldap search: {e}")))?;

        // Exactly one match — 0 = unknown user, >1 = ambiguous filter.
        if rs.len() != 1 {
            let _ = ldap.unbind().await;
            return Err(MiraError::Unauthorized);
        }
        let entry = SearchEntry::construct(rs.into_iter().next().unwrap());
        let dn = entry.dn.clone();

        // Optional group requirement (memberOf, case-insensitive).
        if !self.cfg.required_group.trim().is_empty() {
            let needed = self.cfg.required_group.trim();
            let member = entry
                .attrs
                .get("memberOf")
                .map(|v| v.iter().any(|g| g.eq_ignore_ascii_case(needed)))
                .unwrap_or(false);
            if !member {
                let _ = ldap.unbind().await;
                return Err(MiraError::Unauthorized);
            }
        }

        // 3. Re-bind as the user to verify the password.
        let bind = ldap
            .simple_bind(&dn, password)
            .await
            .map_err(|e| MiraError::ProviderError(format!("ldap user bind: {e}")))?;
        let ok = bind.success().is_ok();
        let _ = ldap.unbind().await;
        if !ok {
            return Err(MiraError::Unauthorized);
        }

        let first = |k: &str| entry.attrs.get(k).and_then(|v| v.first()).cloned();
        Ok(LdapIdentity {
            username:     username.to_string(),
            dn,
            email:        first(&self.cfg.attr_email),
            display_name: first(&self.cfg.attr_display_name),
        })
    }
}

/// Substitute `{username}` in the filter with the LDAP-escaped value, so a
/// crafted username can't alter the filter structure (LDAP injection).
fn build_filter(template: &str, username: &str) -> String {
    template.replace("{username}", &ldap3::ldap_escape(username))
}

/// True if `email`'s domain is in `allowed` (empty `allowed` = any domain).
fn domain_allowed(email: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    match email.rsplit_once('@') {
        Some((_, d)) => allowed.iter().any(|a| a.trim().eq_ignore_ascii_case(d)),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(auto: bool, domains: &[&str], role: &str) -> LdapService {
        let mut cfg = LdapConfig::default();
        cfg.enabled = true;
        cfg.url = "ldap://localhost:389".into();
        cfg.user_base_dn = "ou=people,dc=example,dc=com".into();
        cfg.auto_provision = auto;
        cfg.allowed_domains = domains.iter().map(|s| s.to_string()).collect();
        cfg.default_role = role.into();
        LdapService::new(&cfg)
    }

    #[test]
    fn filter_substitution_escapes_injection() {
        assert_eq!(build_filter("(uid={username})", "alice"), "(uid=alice)");
        // Parens / asterisks that would alter the filter are escaped.
        let f = build_filter("(uid={username})", "a)(uid=*");
        assert!(f.contains("\\28") && f.contains("\\29") && f.contains("\\2a"), "got {f}");
        assert!(!f.contains("(uid=*)"));
    }

    #[test]
    fn is_enabled_requires_url_and_base() {
        let mut cfg = LdapConfig::default();
        cfg.enabled = true;
        assert!(!LdapService::new(&cfg).is_enabled()); // no url/base
        cfg.url = "ldap://x".into();
        cfg.user_base_dn = "dc=x".into();
        assert!(LdapService::new(&cfg).is_enabled());
    }

    #[test]
    fn auto_provision_gating() {
        // disabled → always Err
        assert!(svc(false, &[], "user").auto_provision_role(Some("a@x.com")).is_err());
        // enabled + any domain → role
        assert_eq!(svc(true, &[], "admin").auto_provision_role(Some("a@x.com")).unwrap(), Role::Admin);
        // domain allow-list enforced
        let s = svc(true, &["corp.com"], "user");
        assert!(s.auto_provision_role(Some("a@corp.com")).is_ok());
        assert!(s.auto_provision_role(Some("a@gmail.com")).is_err());
        // no email → Err
        assert!(svc(true, &[], "user").auto_provision_role(None).is_err());
    }

    #[test]
    fn realm_is_stable() {
        assert_eq!(svc(false, &[], "user").realm(), "ldap:ldap://localhost:389");
    }

    /// Live check against a throwaway OpenLDAP (see the test runbook in the
    /// commit). Network/Docker-dependent → ignored by default. Run with a
    /// bitnami/openldap container on :1389 seeded with alice/alicepass:
    ///   cargo test --lib auth::ldap::tests::authenticate_against_real_ldap -- --ignored
    #[tokio::test]
    #[ignore]
    async fn authenticate_against_real_ldap() {
        let mut cfg = LdapConfig::default();
        cfg.enabled = true;
        cfg.url = "ldap://localhost:1389".into();
        cfg.bind_dn = "cn=admin,dc=example,dc=org".into();
        cfg.bind_password = "adminpass".into();
        cfg.user_base_dn = "ou=users,dc=example,dc=org".into();
        cfg.user_filter = "(cn={username})".into();
        let svc = LdapService::new(&cfg);

        // Valid credentials succeed and resolve the DN.
        let ident = svc.authenticate("alice", "alicepass").await.expect("valid login");
        assert_eq!(ident.username, "alice");
        assert!(ident.dn.to_lowercase().contains("cn=alice"), "dn={}", ident.dn);

        // Wrong password, unknown user, and empty password all rejected.
        assert!(svc.authenticate("alice", "wrong").await.is_err());
        assert!(svc.authenticate("nobody", "x").await.is_err());
        assert!(svc.authenticate("alice", "").await.is_err());
    }
}
