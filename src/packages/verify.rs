// SPDX-License-Identifier: AGPL-3.0-or-later

//! Package verification → trust level (see design-docs/plugin-packages.md, "Trust
//! model" + "Signing").
//!
//! Reuses the skills crypto path verbatim: the same ed25519 + canonical-JSON
//! `verify_signature_block`, the same `TrustStore` of publisher fingerprints.
//! The only package-specific bit is mapping the low-level `VerificationOutcome`
//! onto a `TrustLevel` the install policy gates on.

use serde::Serialize;

use crate::skills::manifest::Verification;
use crate::skills::signing::{canonical_json, verify_signature_block};
use crate::skills::trust::{fingerprint_of, TrustStore};

use super::manifest::PackageManifest;

// The trust level MIRA assigns a package at verify time. Capability gating
// (later phases) keys off this. See the spec's trust-level table.
// // `official` / `verified-publisher` distinction and `blacklisted` are future
// (a trust-store tag + a revocation feed);  collapses to these four.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum TrustLevel {
    // Valid signature from a publisher key in the trust store.
    Verified { publisher: String },
    // A signature is present, the key is trusted, but the signature does not
    // match the package contents (tampered or corrupt).
    Invalid { reason: String },
    // A signature is present but the publisher key isn't in the trust store,
    // so MIRA can't verify it — add the key to trust this publisher (the TOFU
    // pin point).
    Untrusted { reason: String },
    // No signature block at all.
    Unsigned,
}

impl TrustLevel {
    // True only for a valid signature from a trusted publisher.
    pub fn is_verified(&self) -> bool {
        matches!(self, TrustLevel::Verified { .. })
    }

    // Short, stable label for storage / display.
    pub fn label(&self) -> &'static str {
        match self {
            TrustLevel::Verified { .. } => "verified",
            TrustLevel::Invalid { .. } => "invalid",
            TrustLevel::Untrusted { .. } => "untrusted",
            TrustLevel::Unsigned => "unsigned",
        }
    }
}

// Canonical bytes a package is signed over: the manifest with the
// `verification` block stripped, emitted as sorted-key JSON. Mirrors the
// skill-manifest canonicalisation exactly.
pub fn canonical_bytes(manifest: &PackageManifest) -> Result<Vec<u8>, String> {
    let mut clone = manifest.clone();
    clone.verification = None;
    canonical_json(&clone)
}

// Verify a package manifest against the trust store and classify its trust.
pub fn verify_package(manifest: &PackageManifest, trust_store: &TrustStore) -> TrustLevel {
    let Some(verification) = manifest.verification.as_ref() else {
        return TrustLevel::Unsigned;
    };
    let canonical = match canonical_bytes(manifest) {
        Ok(b) => b,
        Err(e) => return TrustLevel::Invalid { reason: format!("canonicalisation failed: {e}") },
    };
    let outcome = verify_signature_block(&canonical, verification, trust_store);
    if outcome.verified {
        TrustLevel::Verified { publisher: outcome.publisher_label.unwrap_or_default() }
    } else if outcome.publisher_label.is_some() {
        // Key is in the trust store but the signature didn't validate.
        TrustLevel::Invalid {
            reason: outcome.reason.unwrap_or_else(|| "signature does not match".into()),
        }
    } else {
        // Publisher key not in the trust store, or a malformed block.
        TrustLevel::Untrusted {
            reason: outcome.reason.unwrap_or_else(|| "publisher key not trusted".into()),
        }
    }
}

// Sign a package manifest, returning the detached `Verification` block to
// embed. Used by the packaging CLI and the tests. Reuses the skills
// fingerprint + `Verification` format.
pub fn sign_package(
    manifest: &PackageManifest,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<Verification, String> {
    use base64::Engine;
    use ed25519_dalek::Signer;
    let canonical = canonical_bytes(manifest)?;
    let sig = signing_key.sign(&canonical);
    let pk = signing_key.verifying_key();
    Ok(Verification {
        signature: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
        ),
        publisher_key: format!("fingerprint:{}", fingerprint_of(&pk)),
        signed_at: chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    use crate::packages::manifest::PackageManifest;

    fn sample() -> PackageManifest {
        PackageManifest::parse_json(
            r#"{
                "format": "1",
                "id": "com.example.nextcloud-talk",
                "name": "Nextcloud Talk",
                "version": "1.0.0",
                "components": [ { "type": "cpp_provider" } ]
            }"#,
        )
        .unwrap()
    }

    fn trust_with(label: &str, sk: &SigningKey) -> TrustStore {
        let mut store = TrustStore::empty();
        let b64 = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().as_bytes());
        store.add(label, &b64).unwrap();
        store
    }

    #[test]
    fn unsigned_package_is_unsigned() {
        let level = verify_package(&sample(), &TrustStore::empty());
        assert_eq!(level, TrustLevel::Unsigned);
        assert!(!level.is_verified());
    }

    #[test]
    fn signed_by_trusted_key_is_verified() {
        let sk = SigningKey::generate(&mut OsRng);
        let store = trust_with("Example", &sk);
        let mut m = sample();
        m.verification = Some(sign_package(&m, &sk).unwrap());
        let level = verify_package(&m, &store);
        assert_eq!(level, TrustLevel::Verified { publisher: "Example".into() });
        assert!(level.is_verified());
    }

    #[test]
    fn signed_by_unknown_key_is_untrusted() {
        let sk = SigningKey::generate(&mut OsRng);
        let mut m = sample();
        m.verification = Some(sign_package(&m, &sk).unwrap());
        let level = verify_package(&m, &TrustStore::empty()); // key not in store
        assert!(matches!(level, TrustLevel::Untrusted { .. }));
        assert!(!level.is_verified());
    }

    #[test]
    fn tampered_after_signing_is_invalid() {
        let sk = SigningKey::generate(&mut OsRng);
        let store = trust_with("Example", &sk);
        let mut m = sample();
        m.verification = Some(sign_package(&m, &sk).unwrap());
        m.name = "Evil Talk".into(); // mutate after signing
        let level = verify_package(&m, &store);
        assert!(matches!(level, TrustLevel::Invalid { .. }));
    }

    #[test]
    fn verification_survives_json_roundtrip() {
        let sk = SigningKey::generate(&mut OsRng);
        let store = trust_with("Example", &sk);
        let mut m = sample();
        m.verification = Some(sign_package(&m, &sk).unwrap());
        let s = serde_json::to_string(&m).unwrap();
        let m2: PackageManifest = serde_json::from_str(&s).unwrap();
        assert!(verify_package(&m2, &store).is_verified());
    }
}
