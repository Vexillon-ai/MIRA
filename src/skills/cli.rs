// SPDX-License-Identifier: AGPL-3.0-or-later

//! `mira skill ...` CLI commands (slice A8). Backstops the local-dev
//! workflow before publishers can ship signed Skills:
//!
//! ```text
//! mira skill init    com.example.foo  [--out DIR]
//! mira skill validate <dir>
//! mira skill keygen  [--out PATH]
//! mira skill sign    <dir> --key PATH
//! mira skill package <dir>             [--out FILE]
//! ```
//!
//! Each command exits 0 on success, non-zero on failure; the message on
//! stderr is intended for humans typing in a terminal, not for parsing.

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use ed25519_dalek::{SigningKey, VerifyingKey};
use flate2::write::GzEncoder;
use flate2::Compression;
use rand::rngs::OsRng;

use crate::skills::manifest::SkillManifest;
use crate::skills::signing::{apply_signature, sign_manifest};
use crate::skills::trust::fingerprint_of;

// ─── Public entry point ─────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SkillCliCommand {
    Init     { id: String,    out: Option<PathBuf> },
    Validate { path: PathBuf },
    Keygen   { out: Option<PathBuf> },
    Sign     { path: PathBuf, key: PathBuf },
    Package  { path: PathBuf, out: Option<PathBuf> },
}

pub fn run(cmd: SkillCliCommand) -> Result<(), Box<dyn Error>> {
    match cmd {
        SkillCliCommand::Init     { id, out }       => init(&id, out.as_deref()),
        SkillCliCommand::Validate { path }          => validate(&path),
        SkillCliCommand::Keygen   { out }           => keygen(out.as_deref()),
        SkillCliCommand::Sign     { path, key }     => sign(&path, &key),
        SkillCliCommand::Package  { path, out }     => package(&path, out.as_deref()),
    }
}

// ─── init ──────────────────────────────────────────────────────────────────

fn init(id: &str, out: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir()?;
    let target = out.unwrap_or(cwd.as_path()).join(id);
    if target.exists() {
        return Err(format!(
            "{} already exists — pick a different id or delete it first",
            target.display(),
        ).into());
    }
    fs::create_dir_all(&target)?;
    let manifest_path = target.join("skill.toml");
    fs::write(&manifest_path, starter_manifest(id))?;

    println!("✓ Created {}", target.display());
    println!("  ├── skill.toml");
    println!();
    println!("Next steps:");
    println!("  • Edit {} — fill in description, permissions, tools.", manifest_path.display());
    println!("  • mira skill validate {}", target.display());
    println!("  • mira skill keygen        # if you don't have a publisher key yet");
    println!("  • mira skill sign {} --key <secret-key-file>", target.display());
    println!("  • mira skill package {}    # produces a .miraskill tarball", target.display());
    Ok(())
}

fn starter_manifest(id: &str) -> String {
    format!(
        r#"# Manifest for {id}
# Format reference: design-docs/skills-and-agents.md §"Manifest format"

[skill]
id            = "{id}"
version       = "0.1.0"
display_name  = "TODO: friendly name"
description   = "TODO: one-sentence summary the agent uses to decide when to call this Skill."
authors       = ["You <you@example.com>"]
license       = "MIT"

# Declare the narrowest set of permissions the Skill needs. The user (or
# admin) sees this list at install time and either approves or rejects.
[permissions]
network_egress                   = []     # e.g. ["https://api.example.com"]
filesystem                       = []     # e.g. ["read+write:~/Documents/myskill/"]
subprocess                       = false
secrets                          = []     # env-var names the Skill may read
llm_providers                    = ["primary"]
max_llm_spend_per_invocation_usd = 0.50

# Tools exposed to the agent. Three kinds:
#   builtin   — wraps an existing built-in tool by name (web_fetch, math_eval, …)
#   prompt    — applies a prompt template through the LLM
#   executable — runs a binary/script shipped with the Skill (sandboxed)
[tools]
# example_clock = {{ kind = "builtin", impl = "now" }}

# Filled in by `mira skill sign` — leave commented out for unsigned dev.
# [verification]
# signature     = "ed25519:..."
# publisher_key = "fingerprint:..."
# signed_at     = "..."
"#,
    )
}

// ─── validate ──────────────────────────────────────────────────────────────

fn validate(path: &Path) -> Result<(), Box<dyn Error>> {
    let manifest_path = path.join("skill.toml");
    let toml = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("can't read {}: {e}", manifest_path.display()))?;
    let manifest = SkillManifest::parse(&toml).map_err(|e| format!("parse error: {e}"))?;

    if let Err(errs) = manifest.validate() {
        eprintln!("✗ Validation failed for {}:", manifest_path.display());
        for e in errs { eprintln!("  • {e}"); }
        return Err("manifest validation failed".into());
    }

    // Check the directory name matches the manifest id (catches a common
    // packaging mistake before it reaches install).
    let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if dir_name != manifest.skill.id {
        return Err(format!(
            "directory name {dir_name:?} doesn't match manifest id {:?} — \
             rename the directory or update the manifest",
            manifest.skill.id,
        ).into());
    }

    // Verify executable tool paths actually exist on disk.
    for rel in manifest.executable_paths() {
        if !path.join(rel).is_file() {
            return Err(format!(
                "tool references missing executable {rel:?} (relative to {})",
                path.display(),
            ).into());
        }
    }

    println!("✓ {} parses and validates", manifest_path.display());
    println!("  id          {}", manifest.skill.id);
    println!("  version     {}", manifest.skill.version);
    println!("  tools       {}", manifest.tools.len());
    println!("  permissions {}", permission_summary(&manifest));
    if manifest.verification.is_some() {
        println!("  signature   present (use `mira skill verify` against a trust store to confirm)");
    } else {
        println!("  signature   absent (run `mira skill sign` before publishing)");
    }
    Ok(())
}

fn permission_summary(m: &SkillManifest) -> String {
    let mut bits = Vec::new();
    let p = &m.permissions;
    if !p.network_egress.is_empty() { bits.push(format!("network={}", p.network_egress.len())); }
    if !p.filesystem.is_empty()     { bits.push(format!("fs={}", p.filesystem.len())); }
    if p.subprocess                 { bits.push("subprocess".into()); }
    if !p.secrets.is_empty()        { bits.push(format!("secrets={}", p.secrets.len())); }
    if !p.llm_providers.is_empty()  { bits.push(format!("llm={}", p.llm_providers.len())); }
    if bits.is_empty() { "(none)".into() } else { bits.join(", ") }
}

// ─── keygen ────────────────────────────────────────────────────────────────

fn keygen(out: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let signing = SigningKey::generate(&mut OsRng);
    let pk = signing.verifying_key();
    let fp = fingerprint_of(&pk);

    let path = out.map(|p| p.to_path_buf()).unwrap_or_else(|| default_key_path(&fp));
    if path.exists() {
        return Err(format!(
            "{} already exists — refusing to overwrite an existing key",
            path.display(),
        ).into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_key_file(&path, &signing, &pk, &fp)?;

    println!("✓ Wrote secret key to {}", path.display());
    println!("  Mode 0600 — back this file up; losing it means losing the ability to sign new Skills under this key.");
    println!();
    println!("Publisher fingerprint: {fp}");
    println!("Public key (base64):   {}",
        base64::engine::general_purpose::STANDARD.encode(pk.as_bytes()));
    println!();
    println!("Add to MIRA's trust store (admin) so signed Skills under this key are recognised:");
    println!("  • web UI: Skills → Trust Store → Add (paste label + the public key above)");
    println!("  • or: POST /api/skills/trust-store {{label, public_key}}");
    Ok(())
}

fn default_key_path(fingerprint: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".mira/keys").join(format!("{}.ed25519", &fingerprint[..16]))
}

const KEY_FILE_MAGIC: &str = "MIRA-ED25519-SK-V1";

fn write_key_file(path: &Path, sk: &SigningKey, pk: &VerifyingKey, fingerprint: &str)
-> std::io::Result<()> {
    let secret_b64 = base64::engine::general_purpose::STANDARD.encode(sk.to_bytes());
    let public_b64 = base64::engine::general_purpose::STANDARD.encode(pk.as_bytes());
    let body = format!(
        "{KEY_FILE_MAGIC}\n\
         # Generated {ts}\n\
         # Treat this file like an SSH private key — chmod 600, don't commit, back up.\n\
         secret:      {secret_b64}\n\
         public:      {public_b64}\n\
         fingerprint: {fingerprint}\n",
        ts = chrono::Utc::now().to_rfc3339(),
    );
    // Open with restrictive mode where the platform supports it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true).create_new(true).mode(0o600)
            .open(path)?;
        f.write_all(body.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true).create_new(true)
            .open(path)?;
        f.write_all(body.as_bytes())?;
    }
    Ok(())
}

fn read_key_file(path: &Path) -> Result<SigningKey, Box<dyn Error>> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("can't read key file {}: {e}", path.display()))?;
    let mut lines = text.lines();
    match lines.next() {
        Some(line) if line.trim() == KEY_FILE_MAGIC => {}
        _ => return Err(format!(
            "{} is not a MIRA signing key (expected first line: {KEY_FILE_MAGIC})",
            path.display(),
        ).into()),
    }
    let secret_b64 = lines
        .find_map(|l| l.strip_prefix("secret:").map(|s| s.trim().to_string()))
        .ok_or_else(|| format!("{} missing `secret:` line", path.display()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&secret_b64)
        .map_err(|e| format!("`secret:` line is not valid base64: {e}"))?;
    let arr: [u8; 32] = bytes.as_slice().try_into()
        .map_err(|_| format!("`secret:` must decode to 32 bytes, got {}", bytes.len()))?;
    Ok(SigningKey::from_bytes(&arr))
}

// ─── sign ──────────────────────────────────────────────────────────────────

fn sign(path: &Path, key_path: &Path) -> Result<(), Box<dyn Error>> {
    let signing = read_key_file(key_path)?;
    let pk = signing.verifying_key();
    let fp = fingerprint_of(&pk);

    let manifest_path = path.join("skill.toml");
    let original = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("can't read {}: {e}", manifest_path.display()))?;
    let mut manifest = SkillManifest::parse(&original)
        .map_err(|e| format!("parse error: {e}"))?;

    // Strip any prior verification block from the in-memory manifest so we
    // sign a clean canonical form. (The on-disk doc is rewritten below.)
    manifest.verification = None;
    let verification = sign_manifest(&manifest, &signing);
    apply_signature(&mut manifest, &signing); // sets the same Verification on manifest

    // Rewrite skill.toml: parse with toml_edit so user comments are
    // preserved, replace the [verification] table.
    let mut doc: toml_edit::DocumentMut = original.parse()
        .map_err(|e| format!("toml_edit re-parse: {e}"))?;
    let mut tbl = toml_edit::Table::new();
    tbl["signature"]     = toml_edit::value(verification.signature.clone());
    tbl["publisher_key"] = toml_edit::value(verification.publisher_key.clone());
    tbl["signed_at"]     = toml_edit::value(verification.signed_at.clone());
    doc["verification"]  = toml_edit::Item::Table(tbl);
    fs::write(&manifest_path, doc.to_string())?;

    println!("✓ Signed {}", manifest_path.display());
    println!("  Publisher fingerprint: {fp}");
    println!("  Signed at:             {}", verification.signed_at);
    println!();
    println!("For MIRA to mark this Skill as Verified, the trust store must contain");
    println!("the fingerprint above. Admins add it via the web UI or:");
    println!("  POST /api/skills/trust-store {{label, public_key}}");
    Ok(())
}

// ─── package ───────────────────────────────────────────────────────────────

fn package(path: &Path, out: Option<&Path>) -> Result<(), Box<dyn Error>> {
    // Re-validate before packaging so we never ship a broken archive.
    let manifest_path = path.join("skill.toml");
    let toml = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("can't read {}: {e}", manifest_path.display()))?;
    let manifest = SkillManifest::parse(&toml).map_err(|e| format!("parse error: {e}"))?;
    if let Err(errs) = manifest.validate() {
        let joined = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
        return Err(format!("manifest validation failed: {joined}").into());
    }
    let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if dir_name != manifest.skill.id {
        return Err(format!(
            "directory name {dir_name:?} doesn't match manifest id {:?}",
            manifest.skill.id,
        ).into());
    }

    let out_path = match out {
        Some(p) => p.to_path_buf(),
        None    => PathBuf::from(format!(
            "{}-{}.miraskill",
            manifest.skill.id, manifest.skill.version,
        )),
    };
    if out_path.exists() {
        return Err(format!(
            "{} already exists — pass --out to choose a different path",
            out_path.display(),
        ).into());
    }

    let f = File::create(&out_path)
        .map_err(|e| format!("can't create {}: {e}", out_path.display()))?;
    let gz = GzEncoder::new(f, Compression::default());
    let mut tar = tar::Builder::new(gz);
    // Append with the skill id as the top-level directory — install
    // expects exactly that.
    tar.append_dir_all(&manifest.skill.id, path)
        .map_err(|e| format!("tar build: {e}"))?;
    tar.finish().map_err(|e| format!("tar finish: {e}"))?;

    let size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    println!("✓ Wrote {} ({} kB)", out_path.display(), (size + 512) / 1024);
    println!();
    println!("Install with:");
    println!("  • web UI: Skills → Install Skill (admin) → upload this file");
    println!("  • or: curl -F archive=@{} -X POST .../api/skills/install", out_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_then_validate_then_package_roundtrips() {
        let dir = TempDir::new().unwrap();
        init("com.example.tdd", Some(dir.path())).unwrap();

        let skill_dir = dir.path().join("com.example.tdd");
        // The starter manifest has TODO display_name — we override before
        // running validate so it passes the non-empty check (the starter
        // is intentionally TODO so users edit; we patch it for the test).
        let toml = fs::read_to_string(skill_dir.join("skill.toml")).unwrap();
        let toml = toml.replace("TODO: friendly name", "TDD Skill")
                       .replace("TODO: one-sentence summary the agent uses to decide when to call this Skill.",
                                "Test skill");
        fs::write(skill_dir.join("skill.toml"), toml).unwrap();

        validate(&skill_dir).unwrap();

        let archive = dir.path().join("test.miraskill");
        package(&skill_dir, Some(&archive)).unwrap();
        assert!(archive.exists(), "archive should exist");
        assert!(fs::metadata(&archive).unwrap().len() > 0, "archive shouldn't be empty");
    }

    #[test]
    fn keygen_then_sign_round_trips_through_verify() {
        use crate::skills::signing::verify_manifest;
        use crate::skills::trust::{TrustStore};

        let dir = TempDir::new().unwrap();

        // Generate a key
        let key_path = dir.path().join("test.ed25519");
        keygen(Some(&key_path)).unwrap();

        // Scaffold + edit a skill so it parses cleanly
        init("com.example.signed", Some(dir.path())).unwrap();
        let skill_dir = dir.path().join("com.example.signed");
        let toml = fs::read_to_string(skill_dir.join("skill.toml")).unwrap()
            .replace("TODO: friendly name", "Signed Skill")
            .replace("TODO: one-sentence summary the agent uses to decide when to call this Skill.",
                     "x");
        fs::write(skill_dir.join("skill.toml"), toml).unwrap();

        // Sign it
        sign(&skill_dir, &key_path).unwrap();

        // Reload + verify against a trust store containing our public key
        let signed_text = fs::read_to_string(skill_dir.join("skill.toml")).unwrap();
        let manifest = SkillManifest::parse(&signed_text).unwrap();
        assert!(manifest.verification.is_some(), "verification block must be present");

        let signing = read_key_file(&key_path).unwrap();
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().as_bytes());
        let mut store = TrustStore::empty();
        store.add("Test", &pk_b64).unwrap();

        let outcome = verify_manifest(&manifest, &store);
        assert!(outcome.verified, "round-trip must verify; reason = {:?}", outcome.reason);
    }

    #[test]
    fn read_key_file_rejects_garbage() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad");
        fs::write(&path, "not-a-mira-key").unwrap();
        let err = read_key_file(&path).unwrap_err();
        assert!(err.to_string().contains("MIRA-ED25519-SK-V1"));
    }

    #[test]
    fn keygen_refuses_to_overwrite_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.ed25519");
        fs::write(&path, "anything").unwrap();
        let err = keygen(Some(&path)).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }
}
