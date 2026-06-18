// SPDX-License-Identifier: AGPL-3.0-or-later

//! Scan a `skills/` directory, parse each Skill's `skill.toml`, validate
//! it, and produce a registry of what loaded successfully + diagnostics
//! for what didn't.
//!
//! Verification (A7) happens here too when a `TrustStore` is supplied:
//! `LoadedSkill.verified` reflects whether the manifest's signature
//! validated against a trusted publisher key. Loaders without a trust
//! store treat every Skill as unverified.

use std::fs;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::skills::manifest::{ManifestError, SkillManifest, executable_resolves};
use crate::skills::signing::verify_manifest;
use crate::skills::trust::TrustStore;

/// A successfully-loaded Skill plus where it came from.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub manifest: SkillManifest,
    pub root_dir: PathBuf,
    /// Whether the manifest carried a `[verification]` block. Signed but
    /// not verified == manifest *claims* to be signed but the signature
    /// didn't validate against any trusted publisher (slice A7).
    pub signed:   bool,
    /// True iff the signature was checked and matched a key in the
    /// trust store at load time. Always false for loaders that didn't
    /// pass a trust store, and for unsigned manifests.
    pub verified: bool,
    /// Label of the publisher key the signature checked against. Only
    /// `Some` when verified, or when the publisher key was found in the
    /// trust store but the signature itself didn't match (helps users
    /// debug "wrong file signed" cases).
    pub publisher_label: Option<String>,
    /// Why verification failed. None when verified, or when no
    /// signature claim was present.
    pub verification_error: Option<String>,
    /// System skill: a built-in capability that ships in the binary. True
    /// when the manifest declares `system = true` OR the skill is one of the
    /// bundled set (so all bundled skills are system without per-file flags).
    /// System skills are non-removable and trusted-by-construction (verified).
    pub system: bool,
}

/// What couldn't be loaded and why. Surfaced to the UI so users can debug
/// a Skill that "isn't appearing" without having to read logs.
#[derive(Debug)]
pub struct LoadError {
    pub path:  PathBuf,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct SkillRegistry {
    pub loaded: Vec<LoadedSkill>,
    pub errors: Vec<LoadError>,
}

impl SkillRegistry {
    /// Look up a loaded Skill by its reverse-DNS id.
    pub fn get(&self, id: &str) -> Option<&LoadedSkill> {
        self.loaded.iter().find(|s| s.manifest.skill.id == id)
    }

    /// Iterate over all successfully-loaded Skills.
    pub fn iter(&self) -> impl Iterator<Item = &LoadedSkill> {
        self.loaded.iter()
    }
}

/// Scan `skills_dir` for subdirectories containing a `skill.toml`. The
/// directory name *must* equal the manifest's `skill.id` — keeps the
/// filesystem layout greppable and prevents two Skills with different
/// IDs colliding under the same path.
///
/// Missing `skills_dir` is *not* an error — fresh installs have no
/// Skills yet. Returns an empty registry.
///
/// `verified` on every loaded Skill is `false` here — call
/// `load_dir_with_trust` when you have a trust store and want
/// signature checks to actually populate the verified flag.
pub fn load_dir(skills_dir: &Path, mira_version: &Version) -> SkillRegistry {
    load_dir_with_trust(skills_dir, mira_version, None)
}

/// Same as `load_dir`, but runs ed25519 verification against
/// `trust_store` if supplied. Verification is best-effort — a Skill
/// whose signature fails to validate still loads, but with
/// `verified = false` and an error string in `verification_error`.
/// Refusing to load unsigned/unverified Skills is a policy decision
/// for the caller (the install flow's "uncertified allowed" toggle).
pub fn load_dir_with_trust(
    skills_dir: &Path,
    mira_version: &Version,
    trust_store: Option<&TrustStore>,
) -> SkillRegistry {
    let mut registry = SkillRegistry::default();

    let entries = match fs::read_dir(skills_dir) {
        Ok(e) => e,
        Err(_) => return registry,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Hidden directories (e.g. `.bundled-uninstalled/` markers, editor
        // scratch dirs, dotfile metadata) are never skills. Skipping them
        // here keeps them out of the Skills page error list.
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        match load_one(&path, mira_version, trust_store) {
            Ok(skill) => registry.loaded.push(skill),
            Err(e) => registry.errors.push(LoadError {
                path: path.clone(),
                error: e,
            }),
        }
    }

    // Stable, predictable order so the UI doesn't shuffle on each scan.
    registry.loaded.sort_by(|a, b| a.manifest.skill.id.cmp(&b.manifest.skill.id));
    registry.errors.sort_by(|a, b| a.path.cmp(&b.path));

    registry
}

/// Load one Skill directory. Errors are stringified because callers want
/// to display them; the structured types stay internal.
fn load_one(
    skill_root: &Path,
    mira_version: &Version,
    trust_store: Option<&TrustStore>,
) -> Result<LoadedSkill, String> {
    let manifest_path = skill_root.join("skill.toml");
    if !manifest_path.is_file() {
        return Err(format!("missing {}", manifest_path.display()));
    }

    let toml_text = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("could not read {}: {e}", manifest_path.display()))?;

    let manifest = SkillManifest::parse(&toml_text)
        .map_err(|e| match e {
            ManifestError::Parse(de) => format!("parse error: {de}"),
            other                    => other.to_string(),
        })?;

    // Validate the static rules first — they're cheap and catch most
    // typo-class issues at install time.
    if let Err(errs) = manifest.validate() {
        let joined = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
        return Err(joined);
    }

    // The directory name must match the manifest id. Avoids two Skills
    // claiming the same id from different paths, and makes filesystem
    // grep predictable.
    let dir_name = skill_root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if dir_name != manifest.skill.id {
        return Err(format!(
            "directory name {dir_name:?} doesn't match manifest id {:?}; \
             rename the directory or update the manifest",
            manifest.skill.id,
        ));
    }

    // Skip Skills that need a newer MIRA. Better to refuse cleanly than
    // crash later when a missing API is invoked.
    if let Some(req) = &manifest.skill.mira_min {
        if mira_version < req {
            return Err(ManifestError::MiraVersionTooOld {
                required: req.clone(),
                running:  mira_version.clone(),
            }.to_string());
        }
    }

    // Cross-check that any executable tool paths actually exist relative
    // to the Skill root. Don't *execute* them — just verify presence.
    for rel in manifest.executable_paths() {
        if !executable_resolves(skill_root, rel) {
            return Err(ManifestError::MissingExecutable(rel.to_string()).to_string());
        }
    }

    // A skill is "system" when it declares `system = true` or it's one of the
    // bundled-in-the-binary set. Bundled skills ship inside the (trusted)
    // binary, so they're trusted-by-construction — no signature needed.
    let system = manifest.skill.system
        || crate::skills::bundled::is_bundled(&manifest.skill.id);

    let signed = manifest.verification.is_some();
    let (verified, publisher_label, verification_error) = if system {
        // The binary is the trust anchor for its own bundled capabilities.
        (true, Some("MIRA (built-in)".to_string()), None)
    } else {
        match (signed, trust_store) {
            (true, Some(store)) => {
                let outcome = verify_manifest(&manifest, store);
                (outcome.verified, outcome.publisher_label, outcome.reason)
            }
            // Manifest claims a signature but no trust store was supplied —
            // not the loader's place to refuse, just record that we didn't check.
            (true, None) => (false, None, Some("trust store not configured".into())),
            // Unsigned manifests skip verification entirely.
            (false, _)   => (false, None, None),
        }
    };

    Ok(LoadedSkill {
        manifest,
        root_dir: skill_root.to_path_buf(),
        signed,
        verified,
        publisher_label,
        verification_error,
        system,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn current_version() -> Version {
        Version::parse("99.0.0").unwrap() // newer than any plausible mira_min
    }

    fn write_skill(root: &Path, id: &str, body: &str) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("skill.toml"), body).unwrap();
    }

    fn minimal_manifest(id: &str) -> String {
        format!(
            r#"
[skill]
id = "{id}"
version = "1.0.0"
display_name = "Test Skill"
description = "x"
"#
        )
    }

    #[test]
    fn missing_dir_returns_empty_registry() {
        let dir = TempDir::new().unwrap();
        let registry = load_dir(&dir.path().join("nonexistent"), &current_version());
        assert!(registry.loaded.is_empty());
        assert!(registry.errors.is_empty());
    }

    #[test]
    fn loads_a_single_valid_skill() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "com.example.solo", &minimal_manifest("com.example.solo"));

        let registry = load_dir(dir.path(), &current_version());
        assert_eq!(registry.loaded.len(), 1);
        assert_eq!(registry.errors.len(), 0);
        assert_eq!(registry.loaded[0].manifest.skill.id, "com.example.solo");
        assert!(!registry.loaded[0].signed);
    }

    #[test]
    fn loads_multiple_and_sorts_them() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "com.example.beta",  &minimal_manifest("com.example.beta"));
        write_skill(dir.path(), "com.example.alpha", &minimal_manifest("com.example.alpha"));

        let registry = load_dir(dir.path(), &current_version());
        assert_eq!(registry.loaded.len(), 2);
        assert_eq!(registry.loaded[0].manifest.skill.id, "com.example.alpha");
        assert_eq!(registry.loaded[1].manifest.skill.id, "com.example.beta");
    }

    #[test]
    fn invalid_skill_lands_in_errors_not_loaded() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "com.example.good", &minimal_manifest("com.example.good"));
        write_skill(dir.path(), "com.example.bad",
            r#"
[skill]
id = "BadId"
version = "1.0.0"
display_name = "Bad"
description = "x"
"#);

        let registry = load_dir(dir.path(), &current_version());
        assert_eq!(registry.loaded.len(), 1);
        assert_eq!(registry.errors.len(), 1);
        assert!(registry.errors[0].error.contains("not a valid reverse-DNS"));
    }

    #[test]
    fn directory_name_must_match_manifest_id() {
        let dir = TempDir::new().unwrap();
        // Directory says one thing, manifest says another.
        write_skill(dir.path(), "com.example.from-dir",
            &minimal_manifest("com.example.from-manifest"));

        let registry = load_dir(dir.path(), &current_version());
        assert_eq!(registry.loaded.len(), 0);
        assert_eq!(registry.errors.len(), 1);
        assert!(registry.errors[0].error.contains("doesn't match manifest id"));
    }

    #[test]
    fn refuses_skills_requiring_newer_mira() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "com.example.future",
            r#"
[skill]
id = "com.example.future"
version = "1.0.0"
display_name = "Future"
description = "needs a newer MIRA"
mira_min = "100.0.0"
"#);

        let registry = load_dir(dir.path(), &Version::parse("0.58.0").unwrap());
        assert_eq!(registry.loaded.len(), 0);
        assert_eq!(registry.errors.len(), 1);
        assert!(registry.errors[0].error.contains("requires MIRA"));
    }

    #[test]
    fn hidden_dot_dirs_are_silently_skipped() {
        // The bundled-skill machinery parks uninstall markers under
        // `.bundled-uninstalled/`. The loader must not surface those as
        // "missing skill.toml" errors on the Skills page.
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".bundled-uninstalled")).unwrap();
        fs::create_dir_all(dir.path().join(".bundled-uninstalled/com.mira.coding")).unwrap();
        write_skill(dir.path(), "com.example.real", &minimal_manifest("com.example.real"));

        let registry = load_dir(dir.path(), &current_version());
        assert_eq!(registry.loaded.len(), 1);
        assert!(registry.errors.is_empty(), "got errors: {:?}", registry.errors);
    }

    #[test]
    fn skips_subdirs_without_a_manifest() {
        let dir = TempDir::new().unwrap();
        // A stray directory with no skill.toml — common when users are
        // copying/extracting tarballs.
        fs::create_dir_all(dir.path().join("not-a-skill")).unwrap();
        write_skill(dir.path(), "com.example.real", &minimal_manifest("com.example.real"));

        let registry = load_dir(dir.path(), &current_version());
        assert_eq!(registry.loaded.len(), 1);
        // The stray dir is recorded as an error so the user can see it.
        assert!(registry.errors.iter().any(|e| e.error.contains("missing")));
    }

    #[test]
    fn missing_executable_path_rejects_skill() {
        let dir = TempDir::new().unwrap();
        let id = "com.example.exe";
        write_skill(dir.path(), id, &format!(
            r#"
[skill]
id = "{id}"
version = "1.0.0"
display_name = "Exe"
description = "x"

[tools]
runner = {{ kind = "executable", path = "tools/missing.py" }}
"#));

        let registry = load_dir(dir.path(), &current_version());
        assert_eq!(registry.loaded.len(), 0);
        assert_eq!(registry.errors.len(), 1);
        assert!(registry.errors[0].error.contains("missing executable"));
    }

    #[test]
    fn signed_flag_reflects_verification_block_presence() {
        let dir = TempDir::new().unwrap();
        let id = "com.example.signed";
        write_skill(dir.path(), id, &format!(
            r#"
[skill]
id = "{id}"
version = "1.0.0"
display_name = "Signed"
description = "x"

[verification]
signature = "ed25519:abc"
publisher_key = "fingerprint:def"
signed_at = "2026-05-15T12:00:00Z"
"#));

        let registry = load_dir(dir.path(), &current_version());
        assert_eq!(registry.loaded.len(), 1);
        assert!(registry.loaded[0].signed);
    }
}
