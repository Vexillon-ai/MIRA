// src/skills/instance_key.rs
//! Per-instance skill signing key.
//!
//! Every MIRA install keeps a single ed25519 signing key — a sibling of the
//! secrets `master.key` (mode 0600). The matching public key is auto-pinned in
//! the local [`TrustStore`] under the label **"this MIRA"**, and every
//! self-authored or admin-promoted skill is signed with it. Such skills then
//! verify as **"Signed by this MIRA"** in the UI: provenance by *locality* (it
//! was generated on this box) — distinct from a marketplace skill trusted by a
//! publisher's signature, and from an uploaded-untrusted skill.
//!
//! This reuses [`crate::skills::signing`] + [`crate::skills::trust`] verbatim —
//! the key lifecycle lives here, the crypto does not fork.

use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

use crate::skills::signing;
use crate::skills::trust::{fingerprint_of, TrustStore};
use crate::MiraError;

/// Trust-store label the instance key is pinned under. The web UI renders the
/// publisher label as `Signed by {label}` → "Signed by this MIRA".
pub const INSTANCE_KEY_LABEL: &str = "this MIRA";

/// First line of the on-disk key file — identical to the `mira skill keygen`
/// format so the two key stores are interchangeable for recovery tooling.
const KEY_FILE_MAGIC: &str = "MIRA-ED25519-SK-V1";

/// Location of the per-instance signing key — a sibling of `master.key` so one
/// backup of the data dir's dotfiles covers both.
pub fn default_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("instance_signing.key")
}

/// Load the per-instance signing key, generating it (0600) on first use.
pub fn load_or_create(data_dir: &Path) -> Result<SigningKey, MiraError> {
    let path = default_key_path(data_dir);
    if path.exists() {
        return read_key_file(&path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            MiraError::DatabaseError(format!("instance key dir {}: {e}", parent.display()))
        })?;
    }
    let sk = SigningKey::generate(&mut OsRng);
    write_key_file(&path, &sk)?;
    tracing::info!(
        "instance signing key generated at {:?} (0600) — signs self-authored skills as {:?}. Back it up alongside master.key.",
        path, INSTANCE_KEY_LABEL,
    );
    Ok(sk)
}

/// Pin the instance public key in the on-disk trust store under `skills_dir`
/// (idempotent — a no-op if the fingerprint is already present), so skills it
/// signs verify. Persists the store.
pub fn ensure_pinned(skills_dir: &Path, sk: &SigningKey) -> Result<(), MiraError> {
    let path = TrustStore::default_path(skills_dir);
    let mut store = TrustStore::load(&path)?;
    let pk = sk.verifying_key();
    let fp = fingerprint_of(&pk);
    if store.lookup(&fp).is_some() {
        return Ok(());
    }
    let pk_b64 = base64::engine::general_purpose::STANDARD.encode(pk.as_bytes());
    store.add(INSTANCE_KEY_LABEL, &pk_b64)?;
    store.save_self()?;
    tracing::info!("instance key pinned in trust store {:?} as {:?} ({fp})", path, INSTANCE_KEY_LABEL);
    Ok(())
}

/// Load-or-create the instance key **and** ensure it is pinned — the one call
/// startup and the authoring paths use before signing. `data_dir` locates both
/// the key file and the skills dir (hence the trust store).
pub fn ensure(data_dir: &Path) -> Result<SigningKey, MiraError> {
    let sk = load_or_create(data_dir)?;
    ensure_pinned(&crate::skills::default_skills_dir(data_dir), &sk)?;
    Ok(sk)
}

/// Sign the live skill at `skill_dir` (its `skill.toml`) with the instance key,
/// after making sure the key exists and is pinned. Used by approve / promote so
/// a self-authored or promoted skill reads "Signed by this MIRA". Best-effort
/// at the call site: a signing failure must not strand an otherwise-valid
/// skill, so callers log and continue.
pub fn sign_skill(data_dir: &Path, skill_dir: &Path) -> Result<(), MiraError> {
    let sk = ensure(data_dir)?;
    let manifest_path = skill_dir.join("skill.toml");
    signing::sign_skill_file(&manifest_path, &sk)
        .map_err(|e| MiraError::ToolError(format!("sign {}: {e}", manifest_path.display())))?;
    Ok(())
}

// ── on-disk key file (MIRA-ED25519-SK-V1) ───────────────────────────────────

fn write_key_file(path: &Path, sk: &SigningKey) -> Result<(), MiraError> {
    let pk = sk.verifying_key();
    let secret_b64 = base64::engine::general_purpose::STANDARD.encode(sk.to_bytes());
    let public_b64 = base64::engine::general_purpose::STANDARD.encode(pk.as_bytes());
    let body = format!(
        "{KEY_FILE_MAGIC}\n\
         # MIRA per-instance skill signing key. Treat it like an SSH private key —\n\
         # chmod 600, never commit, back it up alongside master.key.\n\
         # Generated {ts}\n\
         secret:      {secret_b64}\n\
         public:      {public_b64}\n\
         fingerprint: {fp}\n",
        ts = chrono::Utc::now().to_rfc3339(),
        fp = fingerprint_of(&pk),
    );
    write_restricted(path, body.as_bytes())
}

#[cfg(unix)]
fn write_restricted(path: &Path, bytes: &[u8]) -> Result<(), MiraError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| MiraError::DatabaseError(format!("create instance key {}: {e}", path.display())))?;
    f.write_all(bytes)
        .map_err(|e| MiraError::DatabaseError(format!("write instance key: {e}")))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_restricted(path: &Path, bytes: &[u8]) -> Result<(), MiraError> {
    std::fs::write(path, bytes)
        .map_err(|e| MiraError::DatabaseError(format!("write instance key {}: {e}", path.display())))
}

fn read_key_file(path: &Path) -> Result<SigningKey, MiraError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| MiraError::DatabaseError(format!("read instance key {}: {e}", path.display())))?;
    match text.lines().next() {
        Some(l) if l.trim() == KEY_FILE_MAGIC => {}
        _ => {
            return Err(MiraError::DatabaseError(format!(
                "{} is not a MIRA signing key (first line must be {KEY_FILE_MAGIC})",
                path.display(),
            )))
        }
    }
    let secret_b64 = text
        .lines()
        .find_map(|l| l.strip_prefix("secret:").map(|s| s.trim().to_string()))
        .ok_or_else(|| MiraError::DatabaseError(format!("{} missing `secret:` line", path.display())))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&secret_b64)
        .map_err(|e| MiraError::DatabaseError(format!("instance key secret not base64: {e}")))?;
    let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        MiraError::DatabaseError(format!("instance key secret must be 32 bytes, got {}", bytes.len()))
    })?;
    Ok(SigningKey::from_bytes(&arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_persist_reload_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let a = load_or_create(data_dir).unwrap();
        // Second call reads the same key back (stable identity across restarts).
        let b = load_or_create(data_dir).unwrap();
        assert_eq!(a.to_bytes(), b.to_bytes());
        assert!(default_key_path(data_dir).is_file());
    }

    #[test]
    fn ensure_pins_the_key_as_this_mira() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let sk = ensure(data_dir).unwrap();
        let skills_dir = crate::skills::default_skills_dir(data_dir);
        let store = TrustStore::load(&TrustStore::default_path(&skills_dir)).unwrap();
        let fp = fingerprint_of(&sk.verifying_key());
        let (label, _) = store.lookup(&fp).expect("instance key must be pinned");
        assert_eq!(label, INSTANCE_KEY_LABEL);
        // Idempotent: a second ensure doesn't duplicate the entry.
        ensure(data_dir).unwrap();
        let store2 = TrustStore::load(&TrustStore::default_path(&skills_dir)).unwrap();
        assert_eq!(store2.len(), 1);
    }

    #[test]
    fn signed_skill_verifies_as_this_mira() {
        use crate::skills::{load_dir_with_trust, TrustStore as TS};
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path();
        let skills_dir = crate::skills::default_skills_dir(data_dir);
        let skill_dir = skills_dir.join("local.demo");
        std::fs::create_dir_all(skill_dir.join("prompts")).unwrap();
        std::fs::write(
            skill_dir.join("skill.toml"),
            "[skill]\nid = \"local.demo\"\nversion = \"0.1.0\"\n\
             display_name = \"Demo\"\ndescription = \"when asked\"\n\n\
             [tools.guide]\nkind = \"prompt\"\ntemplate = \"prompts/main.md\"\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("prompts/main.md"), "do it").unwrap();

        sign_skill(data_dir, &skill_dir).unwrap();

        // Reload with the pinned trust store → the skill is verified as "this MIRA".
        let trust = TS::load(&TS::default_path(&skills_dir)).unwrap();
        let ver = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let reg = load_dir_with_trust(&skills_dir, &ver, Some(&trust));
        let s = reg.iter().find(|s| s.manifest.skill.id == "local.demo").expect("loaded");
        assert!(s.verified, "self-signed skill must verify against the pinned instance key");
        assert_eq!(s.publisher_label.as_deref(), Some(INSTANCE_KEY_LABEL));
    }
}
