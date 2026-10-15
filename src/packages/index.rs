// SPDX-License-Identifier: AGPL-3.0-or-later

//! Package source index — schema + fetch / verify / cache.
//!
//! A *source* publishes a signed catalog (`index.json`). MIRA fetches it through the instance
//! [`HttpPolicy`] (admin-initiated egress), verifies a **detached ed25519
//! signature** (`<index_url>.sig`) against the source's **pinned key**, caches
//! the last good copy on disk (so browse survives an offline refresh), and
//! merges all enabled sources into one browse list (dedup by id).
//!
//! Trust note: the index only aids *discovery*. The authoritative trust
//! decision is still per-artifact at install time (sha256 + manifest
//! `[verification]` + TrustStore, via [`crate::packages::verify_package`]) — a
//! verified index can never make an unsigned/untrusted artifact install
//! silently, and an *unverified* index simply surfaces its entries as such.
//!
//! The signature covers the **exact bytes** of `index.json` as served (no
//! re-canonicalisation): the signer signs the file, the client verifies the
//! downloaded bytes.

use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::{Signature, Verifier};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::PackageSource;
use crate::skills::trust::parse_public_key_b64;
use crate::tools::http_policy::HttpPolicy;

/// The signed catalog a source publishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceIndex {
    pub schema: String,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub packages: Vec<IndexPackage>,
}

/// One package in a source index (all its versions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexPackage {
    pub id: String,
    pub name: String,
    /// `skill` | `mcp_server` | `cpp_provider` | `app`.
    pub kind: String,
    pub latest: String,
    #[serde(default)]
    pub versions: Vec<IndexVersion>,
}

/// One downloadable version of a package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexVersion {
    pub version: String,
    #[serde(default)]
    pub min_mira_version: Option<String>,
    pub download_url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub publisher: Option<String>,
    /// `fingerprint:<hex>` the artifact's `[verification]` must match.
    #[serde(default)]
    pub key_fingerprint: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
}

/// One package as the merged browse list presents it — the index entry plus
/// which source it came from and whether that source's index was signature-
/// verified against its pinned key.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    #[serde(flatten)]
    pub package: IndexPackage,
    pub source: String,
    /// The source's index signature verified against its pinned key.
    pub source_verified: bool,
}

/// Outcome of refreshing one source.
#[derive(Debug, Clone, Serialize)]
pub struct SourceResult {
    pub name: String,
    pub enabled: bool,
    /// The index signature verified against the pinned key (false when no key
    /// is pinned or verification failed).
    pub verified: bool,
    /// Served fresh (false) or served from the on-disk cache after a failed
    /// refresh (true).
    pub from_cache: bool,
    /// Human-readable error when the source could not be fetched/verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub package_count: usize,
}

/// Sign `index_bytes` with `signing_key`, returning the detached signature in
/// the `ed25519:<base64>` form written to `<index_url>.sig`. The inverse of
/// [`verify_index_signature`] — used by the index-publishing tool
/// (`mira skill index-sign`) and the source repo's CI.
pub fn sign_index(index_bytes: &[u8], signing_key: &ed25519_dalek::SigningKey) -> String {
    use ed25519_dalek::Signer;
    let sig = signing_key.sign(index_bytes);
    format!("ed25519:{}", base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()))
}

/// Verify a detached ed25519 signature (`sig_text`, optionally `ed25519:`-
/// prefixed base64) over `bytes` against `pinned_key_b64`. Any decode/parse
/// failure is a verification failure (fail closed), never a panic.
pub fn verify_index_signature(bytes: &[u8], sig_text: &str, pinned_key_b64: &str) -> bool {
    let sig_b64 = sig_text.trim().strip_prefix("ed25519:").unwrap_or(sig_text.trim());
    let Ok(sig_bytes) = base64::engine::general_purpose::STANDARD.decode(sig_b64) else { return false };
    let Ok(sig_arr): Result<[u8; 64], _> = sig_bytes.as_slice().try_into() else { return false };
    let signature = Signature::from_bytes(&sig_arr);
    let Ok(vk) = parse_public_key_b64(pinned_key_b64) else { return false };
    vk.verify(bytes, &signature).is_ok()
}

/// Hex SHA-256 of `bytes` — used to key the per-source disk cache off its URL
/// and (by the install path) to match an artifact against its index entry.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("packages").join("index-cache")
}

fn cache_path(data_dir: &Path, index_url: &str) -> PathBuf {
    cache_dir(data_dir).join(format!("{}.json", sha256_hex(index_url.as_bytes())))
}

/// Load the last good cached index for a source, if any.
pub fn load_cached(data_dir: &Path, index_url: &str) -> Option<SourceIndex> {
    let bytes = std::fs::read(cache_path(data_dir, index_url)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache(data_dir: &Path, index_url: &str, bytes: &[u8]) {
    let dir = cache_dir(data_dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("package index cache: mkdir {}: {e}", dir.display());
        return;
    }
    let path = cache_path(data_dir, index_url);
    if let Err(e) = std::fs::write(&path, bytes) {
        tracing::warn!("package index cache: write {}: {e}", path.display());
    }
}

/// Fetch + verify one source's index. On a network/verify failure, falls back to
/// the on-disk cache when present (browse degrades gracefully offline). Returns
/// the parsed index, whether it was signature-verified, and whether it came from
/// cache.
pub async fn fetch_index(
    policy:   &HttpPolicy,
    source:   &PackageSource,
    data_dir: &Path,
    admin_id: &str,
) -> Result<(SourceIndex, bool, bool), String> {
    let fetched = fetch_and_verify(policy, source, data_dir, admin_id).await;
    match fetched {
        Ok((index, verified)) => Ok((index, verified, false)),
        Err(e) => match load_cached(data_dir, &source.index_url) {
            // A cached copy was verified when it was first written (we only
            // cache after a good fetch), but we can't re-attest it now → report
            // it unverified + from_cache so the UI is honest about staleness.
            Some(index) => {
                tracing::warn!("source {:?} refresh failed ({e}); using cached index", source.name);
                Ok((index, false, true))
            }
            None => Err(e),
        },
    }
}

async fn fetch_and_verify(
    policy:   &HttpPolicy,
    source:   &PackageSource,
    data_dir: &Path,
    admin_id: &str,
) -> Result<(SourceIndex, bool), String> {
    let resp = policy
        .get(&source.index_url, admin_id)
        .await
        .map_err(|e| format!("fetch index: {e}"))?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("index fetch returned HTTP {}", resp.status));
    }
    if resp.truncated {
        return Err("index is larger than the fetch limit".to_string());
    }
    let body = resp.body;

    // Verify the detached signature against the pinned key, when one is set.
    let verified = if source.pinned_key.trim().is_empty() {
        false
    } else {
        let sig_url = format!("{}.sig", source.index_url);
        let sig = policy
            .get(&sig_url, admin_id)
            .await
            .map_err(|e| format!("fetch index signature: {e}"))?;
        if !(200..300).contains(&sig.status) {
            return Err(format!("index signature fetch returned HTTP {}", sig.status));
        }
        let sig_text = String::from_utf8_lossy(&sig.body);
        if !verify_index_signature(&body, &sig_text, &source.pinned_key) {
            return Err("index signature did not verify against the pinned key".to_string());
        }
        true
    };

    let index: SourceIndex =
        serde_json::from_slice(&body).map_err(|e| format!("index is not valid JSON: {e}"))?;
    if index.schema != "1" {
        return Err(format!("unsupported index schema {:?} (expected \"1\")", index.schema));
    }

    // Only cache a successfully-parsed index.
    write_cache(data_dir, &source.index_url, &body);
    Ok((index, verified))
}

/// Refresh every enabled source and return the merged browse list + per-source
/// results. Dedup is by package id, first enabled source wins (sources are
/// consulted in config order, so a higher-priority source shadows a lower one).
pub async fn refresh_catalog(
    policy:   &HttpPolicy,
    sources:  &[PackageSource],
    data_dir: &Path,
    admin_id: &str,
) -> (Vec<CatalogEntry>, Vec<SourceResult>) {
    let mut catalog: Vec<CatalogEntry> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut results: Vec<SourceResult> = Vec::new();

    for source in sources {
        if !source.enabled {
            results.push(SourceResult {
                name: source.name.clone(), enabled: false, verified: false,
                from_cache: false, error: None, package_count: 0,
            });
            continue;
        }
        match fetch_index(policy, source, data_dir, admin_id).await {
            Ok((index, verified, from_cache)) => {
                let mut count = 0;
                for pkg in index.packages {
                    if seen.insert(pkg.id.clone()) {
                        catalog.push(CatalogEntry {
                            package: pkg, source: source.name.clone(), source_verified: verified,
                        });
                        count += 1;
                    }
                }
                results.push(SourceResult {
                    name: source.name.clone(), enabled: true, verified, from_cache,
                    error: None, package_count: count,
                });
            }
            Err(e) => results.push(SourceResult {
                name: source.name.clone(), enabled: true, verified: false,
                from_cache: false, error: Some(e), package_count: 0,
            }),
        }
    }
    (catalog, results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn sample_index() -> &'static str {
        r#"{"schema":"1","generated_at":"2026-10-15T00:00:00Z","packages":[
            {"id":"ai.vexillon.demo","name":"Demo","kind":"skill","latest":"1.0.0",
             "versions":[{"version":"1.0.0","download_url":"https://x/demo-1.0.0.miraskill",
             "sha256":"abc","publisher":"Vexillon","key_fingerprint":"fingerprint:deadbeef"}]}]}"#
    }

    #[test]
    fn parses_index_schema_1() {
        let idx: SourceIndex = serde_json::from_str(sample_index()).unwrap();
        assert_eq!(idx.schema, "1");
        assert_eq!(idx.packages.len(), 1);
        assert_eq!(idx.packages[0].versions[0].download_url, "https://x/demo-1.0.0.miraskill");
    }

    #[test]
    fn signature_round_trips_and_rejects_tampering() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(sk.verifying_key().as_bytes());
        let body = sample_index().as_bytes();
        let sig = sk.sign(body);
        let sig_b64 = format!("ed25519:{}", base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()));

        // sign_index produces exactly the signature verify accepts (round-trip).
        assert_eq!(sign_index(body, &sk), sig_b64, "sign_index matches a manual sign");
        assert!(verify_index_signature(body, &sign_index(body, &sk), &pk_b64), "sign_index output verifies");

        assert!(verify_index_signature(body, &sig_b64, &pk_b64), "valid signature verifies");
        // Tampered body → fails.
        let mut bad = body.to_vec();
        bad[0] ^= 0xff;
        assert!(!verify_index_signature(&bad, &sig_b64, &pk_b64), "tampered body rejected");
        // Wrong key → fails.
        let other = base64::engine::general_purpose::STANDARD
            .encode(SigningKey::generate(&mut OsRng).verifying_key().as_bytes());
        assert!(!verify_index_signature(body, &sig_b64, &other), "wrong key rejected");
        // Garbage signature → fails closed, no panic.
        assert!(!verify_index_signature(body, "not-base64!!", &pk_b64));
    }

    #[test]
    fn cache_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let url = "https://example/index.json";
        assert!(load_cached(tmp.path(), url).is_none());
        write_cache(tmp.path(), url, sample_index().as_bytes());
        let cached = load_cached(tmp.path(), url).expect("cached index loads");
        assert_eq!(cached.packages[0].id, "ai.vexillon.demo");
    }
}
