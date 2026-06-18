// SPDX-License-Identifier: AGPL-3.0-or-later

//! Skill manifest signing + verification (slice A7).
//!
//! What gets signed: a canonical JSON serialisation of the `SkillManifest`
//! with the `verification` block stripped (you can't sign over the
//! signature itself). Canonicalisation sorts every JSON object's keys so
//! verifier and signer agree on byte order regardless of the order
//! `serde` happened to emit fields in.
//!
//! Encoding conventions in the manifest:
//! - `signature   = "ed25519:<base64-no-pad>"`
//! - `publisher_key = "fingerprint:<hex sha256>"` (looked up against the
//!   trust store; the actual public key bytes never travel in the
//!   manifest — that lives in the trust store, where admins manage it)
//!
//! Why fingerprints, not embedded public keys? It forces every published
//! key to pass through the trust store before it can verify anything.
//! Otherwise a malicious publisher could ship a manifest that says
//! "trust me — here's my key" and the verifier would have no anchor.

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::skills::manifest::{SkillManifest, Verification};
use crate::skills::trust::{fingerprint_of, TrustStore};

/// Outcome of running `verify_manifest`. The reason field is human-
/// readable and surfaced in the UI when a Skill is shown as unverified.
#[derive(Debug, Clone)]
pub struct VerificationOutcome {
    pub verified:        bool,
    /// `Some(label)` when the publisher key was found in the trust
    /// store, even if the signature itself didn't validate.
    pub publisher_label: Option<String>,
    /// Why verification failed. None when verified == true.
    pub reason:          Option<String>,
}

impl VerificationOutcome {
    fn fail(reason: impl Into<String>) -> Self {
        Self { verified: false, publisher_label: None, reason: Some(reason.into()) }
    }
}

pub fn verify_manifest(manifest: &SkillManifest, trust_store: &TrustStore) -> VerificationOutcome {
    let Some(verification) = manifest.verification.as_ref() else {
        return VerificationOutcome::fail("no [verification] block in manifest");
    };

    let canonical = match canonical_bytes(manifest) {
        Ok(b)  => b,
        Err(e) => return VerificationOutcome::fail(format!("canonicalisation failed: {e}")),
    };
    verify_signature_block(&canonical, verification, trust_store)
}

/// Verify a detached `Verification` block against the `canonical` bytes it
/// was signed over, using the trust store. Shared by skill-manifest and
/// plugin-package verification so the crypto path is identical, not parallel.
pub fn verify_signature_block(
    canonical:    &[u8],
    verification: &Verification,
    trust_store:  &TrustStore,
) -> VerificationOutcome {
    // Decode the publisher key fingerprint reference.
    let fp = match verification.publisher_key.strip_prefix("fingerprint:") {
        Some(rest) => rest.trim().to_lowercase(),
        None       => return VerificationOutcome::fail(
            "publisher_key must be in `fingerprint:<hex>` form",
        ),
    };

    let (label, public_key) = match trust_store.lookup(&fp) {
        Some((l, k)) => (l.to_string(), *k),
        None         => return VerificationOutcome::fail(format!(
            "publisher fingerprint {fp} is not in the trust store",
        )),
    };

    // Decode the signature.
    let sig_b64 = match verification.signature.strip_prefix("ed25519:") {
        Some(rest) => rest.trim(),
        None       => return VerificationOutcome { verified: false, publisher_label: Some(label),
                          reason: Some("signature must be in `ed25519:<base64>` form".into()) },
    };
    let sig_bytes = match base64::engine::general_purpose::STANDARD.decode(sig_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(sig_b64))
    {
        Ok(b) => b,
        Err(e) => return VerificationOutcome { verified: false, publisher_label: Some(label),
                      reason: Some(format!("signature is not valid base64: {e}")) },
    };
    let sig_arr: [u8; 64] = match sig_bytes.as_slice().try_into() {
        Ok(a)  => a,
        Err(_) => return VerificationOutcome { verified: false, publisher_label: Some(label),
                     reason: Some(format!(
                         "signature must decode to 64 bytes, got {}", sig_bytes.len(),
                     )) },
    };
    let sig = Signature::from_bytes(&sig_arr);

    match public_key.verify(canonical, &sig) {
        Ok(())  => VerificationOutcome { verified: true, publisher_label: Some(label), reason: None },
        Err(_)  => VerificationOutcome { verified: false, publisher_label: Some(label),
                       reason: Some("signature does not match the manifest contents".into()) },
    }
}

/// Sign a manifest and return the populated `Verification` block. The
/// returned struct can be embedded into the manifest before serialising
/// (or `apply_signature` does it for you).
pub fn sign_manifest(manifest: &SkillManifest, signing_key: &SigningKey) -> Verification {
    let canonical = canonical_bytes(manifest).expect("canonicalisation must not fail in sign path");
    let sig: Signature = signing_key.sign(&canonical);
    let pk = signing_key.verifying_key();

    Verification {
        signature:     format!("ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())),
        publisher_key: format!("fingerprint:{}", fingerprint_of(&pk)),
        signed_at:     chrono::Utc::now().to_rfc3339(),
    }
}

/// Convenience: sign a manifest in-place. Returns the public key whose
/// fingerprint is now embedded — the caller pushes it through
/// `TrustStore::add(label, b64)` to make the signature meaningful.
pub fn apply_signature(manifest: &mut SkillManifest, signing_key: &SigningKey) -> VerifyingKey {
    manifest.verification = Some(sign_manifest(manifest, signing_key));
    signing_key.verifying_key()
}

// ── canonicalisation ──────────────────────────────────────────────────

/// Canonical JSON bytes the signature is computed over.
///
/// Steps:
///   1. Clone the manifest, drop the `verification` block.
///   2. Serialise to `serde_json::Value` (JSON keys mirror Rust field
///      names because we don't use `#[serde(rename = ...)]`).
///   3. Recursively re-emit with sorted object keys so signer and
///      verifier agree on byte order regardless of serde's emit order.
pub fn canonical_bytes(manifest: &SkillManifest) -> Result<Vec<u8>, String> {
    let mut clone = manifest.clone();
    clone.verification = None;
    canonical_json(&clone)
}

/// Canonical JSON bytes for any serialisable value: serialise to a JSON
/// value, then re-emit with every object's keys sorted so signer and verifier
/// agree on byte order. Shared by skill-manifest and plugin-package signing.
pub fn canonical_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let val = serde_json::to_value(value).map_err(|e| e.to_string())?;
    let sorted = sort_keys(val);
    serde_json::to_vec(&sorted).map_err(|e| e.to_string())
}

fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            // BTreeMap → sorted insertion order when re-collected.
            let sorted: BTreeMap<String, Value> = map.into_iter()
                .map(|(k, v)| (k, sort_keys(v)))
                .collect();
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (k, v) in sorted { out.insert(k, v); }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    use crate::skills::manifest::{Permissions, SkillManifest, SkillMeta};

    fn minimal_manifest() -> SkillManifest {
        SkillManifest {
            skill: SkillMeta {
                id: "com.example.signing".into(),
                version: semver::Version::parse("1.0.0").unwrap(),
                display_name: "Signing Test".into(),
                description: "Used by signing tests".into(),
                authors: vec!["Test <t@example.com>".into()],
                license: Some("MIT".into()),
                mira_min: None,
                system: false,
            },
            permissions: Permissions::default(),
            tools:        Default::default(),
            dependencies: Default::default(),
            verification: None,
        }
    }

    fn trust_store_with(label: &str, key: &VerifyingKey) -> TrustStore {
        let mut store = TrustStore::empty();
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.as_bytes());
        store.add(label, &b64).unwrap();
        store
    }

    #[test]
    fn unsigned_manifest_is_unverified() {
        let store = TrustStore::empty();
        let outcome = verify_manifest(&minimal_manifest(), &store);
        assert!(!outcome.verified);
        assert!(outcome.reason.unwrap().contains("no [verification] block"));
    }

    #[test]
    fn signed_with_trusted_key_verifies() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key();
        let store = trust_store_with("Test Publisher", &pk);

        let mut m = minimal_manifest();
        apply_signature(&mut m, &sk);

        let outcome = verify_manifest(&m, &store);
        assert!(outcome.verified, "verified must be true; reason = {:?}", outcome.reason);
        assert_eq!(outcome.publisher_label.as_deref(), Some("Test Publisher"));
        assert!(outcome.reason.is_none());
    }

    #[test]
    fn signed_with_untrusted_key_is_unverified() {
        let sk = SigningKey::generate(&mut OsRng);
        let store = TrustStore::empty(); // empty trust store

        let mut m = minimal_manifest();
        apply_signature(&mut m, &sk);

        let outcome = verify_manifest(&m, &store);
        assert!(!outcome.verified);
        assert!(outcome.reason.unwrap().contains("not in the trust store"));
    }

    #[test]
    fn tampered_manifest_fails_verification() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key();
        let store = trust_store_with("Test", &pk);

        let mut m = minimal_manifest();
        apply_signature(&mut m, &sk);

        // Mutate something *after* signing — verification must fail.
        m.skill.description = "tampered after signing".into();

        let outcome = verify_manifest(&m, &store);
        assert!(!outcome.verified);
        assert!(outcome.reason.unwrap().contains("does not match"));
    }

    #[test]
    fn canonical_bytes_are_stable_across_field_reorderings() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key();
        let store = trust_store_with("Test", &pk);

        // Sign one way…
        let mut m1 = minimal_manifest();
        m1.permissions.network_egress = vec![
            "https://b.example".into(),
            "https://a.example".into(),
        ];
        m1.permissions.filesystem = vec!["read:/tmp".into()];
        apply_signature(&mut m1, &sk);

        // Round-trip through JSON to shuffle internal ordering, then verify.
        let s = serde_json::to_string(&m1).unwrap();
        let m2: SkillManifest = serde_json::from_str(&s).unwrap();

        let outcome = verify_manifest(&m2, &store);
        assert!(outcome.verified, "round-tripped manifest must still verify; reason = {:?}", outcome.reason);
    }

    #[test]
    fn malformed_signature_string_fails_cleanly() {
        let pk = SigningKey::generate(&mut OsRng).verifying_key();
        let store = trust_store_with("Test", &pk);

        let mut m = minimal_manifest();
        m.verification = Some(Verification {
            signature:     "bare-no-prefix".into(),
            publisher_key: format!("fingerprint:{}", fingerprint_of(&pk)),
            signed_at:     "now".into(),
        });
        let outcome = verify_manifest(&m, &store);
        assert!(!outcome.verified);
        assert!(outcome.reason.unwrap().contains("ed25519:"));
    }
}
