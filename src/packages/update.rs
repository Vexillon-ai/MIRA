// SPDX-License-Identifier: AGPL-3.0-or-later

//! Update planning (, slice 1) — the pure diff + policy core that gates
//! an update (design-docs/plugin-packages.md §"Update & migration").
//!
//! An update is an admin supplying a bundle with the **same `id`** and a
//! **higher `version`** than an installed package. Before anything is touched,
//! MIRA computes an [`UpdatePlan`] from `(installed manifest, candidate
//! manifest)` and three diffs:
//!
//! 1. **Trust** — a different signing key is a *new trust decision*, not a
//!  silent update.
//! 2. **Capability** — *widened* capability needs explicit re-approval;
//!  same-or-narrower proceeds silently. This is the security boundary: a
//!  trusted v1 can't ship a malicious v2 without new capability, and new
//!  capability needs a human.
//! 3. **Config-schema** — new required fields prompt; removed fields drop;
//!  renames carry values via `config_migrations`; secrets are preserved.
//!
//! This module is pure (no I/O); the engine + handlers (later slices) act on the
//! plan.

use std::collections::{BTreeMap, BTreeSet};

use semver::{Version, VersionReq};
use serde::Serialize;

use super::manifest::{Capabilities, ComponentKind, PackageManifest};
use super::store::InstalledPackage;
use super::verify::TrustLevel;
use super::wizard::{ConfigField, FieldSource};

// Why an update is refused outright (a policy gate, before any diff review).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum UpdateBlock {
    // The candidate's `id` doesn't match the installed package.
    IdMismatch { installed: String, candidate: String },
    // Candidate version isn't strictly newer (downgrade or same — force-only).
    NotNewer { installed: String, candidate: String },
    // Candidate needs a newer MIRA than is running ("update MIRA first").
    NeedsNewerMira { required: String, running: String },
    // The candidate's signature is invalid (tampered) — never installable.
    InvalidSignature { reason: String },
}

impl std::fmt::Display for UpdateBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateBlock::IdMismatch { installed, candidate } => write!(
                f,
                "this bundle is {candidate:?}, not an update of the installed {installed:?}"
            ),
            UpdateBlock::NotNewer { installed, candidate } => write!(
                f,
                "version {candidate} is not newer than the installed {installed} — downgrades are force-only"
            ),
            UpdateBlock::NeedsNewerMira { required, running } => write!(
                f,
                "this version needs MIRA >= {required} (running {running}) — update MIRA first"
            ),
            UpdateBlock::InvalidSignature { reason } => {
                write!(f, "candidate signature is invalid: {reason}")
            }
        }
    }
}

// The capability widenings an update requests. Empty/false everywhere ⇒ the
// update is same-or-narrower and needs no capability re-approval.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CapabilityDiff {
    pub added_egress: Vec<String>,
    pub added_filesystem: Vec<String>,
    pub added_secrets: Vec<String>,
    pub gained_subprocess: bool,
    pub added_subprocess: Vec<String>,
    pub gained_listen_port: Option<u16>,
}

impl CapabilityDiff {
    // Does this update widen the capability grant (⇒ needs re-approval)?
    pub fn is_widened(&self) -> bool {
        !self.added_egress.is_empty()
            || !self.added_filesystem.is_empty()
            || !self.added_secrets.is_empty()
            || self.gained_subprocess
            || !self.added_subprocess.is_empty()
            || self.gained_listen_port.is_some()
    }
}

// The config-schema delta the update introduces.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ConfigDiff {
    // New admin-`input` fields that are required — the update must prompt for these.
    pub new_required_inputs: Vec<String>,
    // New fields that don't require a prompt (optional input, or minted/derived).
    pub new_optional: Vec<String>,
    // Keys present before but gone now (dropped — value discarded).
    pub removed: Vec<String>,
    // `old_key` → `new_key` renames that apply to the installed version.
    pub renamed: BTreeMap<String, String>,
    // Generated-secret keys flagged `rotate_on_update` — re-minted on apply.
    pub rotated: Vec<String>,
}

// The full, reviewable plan for updating an installed package to a candidate.
#[derive(Debug, Clone, Serialize)]
pub struct UpdatePlan {
    pub id: String,
    pub from_version: String,
    pub to_version: String,
    // A different (or now-absent) signing key vs. what's installed.
    pub trust_changed: bool,
    pub new_trust: String,
    pub capability: CapabilityDiff,
    pub config: ConfigDiff,
    // Gate: a widened capability needs explicit admin re-approval.
    pub needs_capability_reapproval: bool,
    // Gate: a changed signer needs a fresh trust decision.
    pub needs_trust_reapproval: bool,
}

impl UpdatePlan {
    // Both gates clear with no human decision (auto-update-eligible shape).
    pub fn is_silent(&self) -> bool {
        !self.needs_capability_reapproval
            && !self.needs_trust_reapproval
            && self.config.new_required_inputs.is_empty()
    }
}

// Aggregate capability across a manifest's components (the conservative,
// security-correct view: a new capability *anywhere* is a widening).
fn aggregate_caps(m: &PackageManifest) -> Capabilities {
    let mut agg = Capabilities::default();
    let mut egress = BTreeSet::new();
    let mut fs = BTreeSet::new();
    let mut secrets = BTreeSet::new();
    let mut subp = BTreeSet::new();
    for c in &m.components {
        egress.extend(c.capabilities.network_egress.iter().cloned());
        fs.extend(c.capabilities.filesystem.iter().cloned());
        secrets.extend(c.capabilities.secrets.iter().cloned());
        subp.extend(c.capabilities.subprocess_allowlist.iter().cloned());
        agg.subprocess |= c.capabilities.subprocess;
        if c.capabilities.listen_port.is_some() {
            agg.listen_port = c.capabilities.listen_port;
        }
    }
    agg.network_egress = egress.into_iter().collect();
    agg.filesystem = fs.into_iter().collect();
    agg.secrets = secrets.into_iter().collect();
    agg.subprocess_allowlist = subp.into_iter().collect();
    agg
}

// What `new` requests beyond `old`.
pub fn capability_diff(old: &Capabilities, new: &Capabilities) -> CapabilityDiff {
    let added = |o: &[String], n: &[String]| -> Vec<String> {
        let os: BTreeSet<&str> = o.iter().map(String::as_str).collect();
        n.iter().filter(|x| !os.contains(x.as_str())).cloned().collect()
    };
    CapabilityDiff {
        added_egress: added(&old.network_egress, &new.network_egress),
        added_filesystem: added(&old.filesystem, &new.filesystem),
        added_secrets: added(&old.secrets, &new.secrets),
        gained_subprocess: new.subprocess && !old.subprocess,
        added_subprocess: added(&old.subprocess_allowlist, &new.subprocess_allowlist),
        gained_listen_port: match (old.listen_port, new.listen_port) {
            (None, Some(p)) => Some(p),
            _ => None,
        },
    }
}

// The first `cpp_provider` component's config_schema (the wizard-bearing one),
// or an empty slice.
fn cpp_schema(m: &PackageManifest) -> &[ConfigField] {
    m.components
        .iter()
        .find(|c| c.kind == ComponentKind::CppProvider)
        .map(|c| c.config_schema.as_slice())
        .unwrap_or(&[])
}

// Renames declared by the candidate that apply to `from_version`.
fn applicable_renames(candidate: &PackageManifest, from_version: &Version) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for c in &candidate.components {
        for mig in &c.config_migrations {
            let applies = mig.from.trim().is_empty()
                || VersionReq::parse(&mig.from).map(|r| r.matches(from_version)).unwrap_or(false);
            if applies {
                for (old, new) in &mig.rename {
                    out.insert(old.clone(), new.clone());
                }
            }
        }
    }
    out
}

// Explicit drops declared by applicable migrations.
fn applicable_drops(candidate: &PackageManifest, from_version: &Version) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for c in &candidate.components {
        for mig in &c.config_migrations {
            let applies = mig.from.trim().is_empty()
                || VersionReq::parse(&mig.from).map(|r| r.matches(from_version)).unwrap_or(false);
            if applies {
                out.extend(mig.drop.iter().cloned());
            }
        }
    }
    out
}

// Compute the config-schema diff old → new, honouring renames/drops.
pub fn config_diff(
    old_schema: &[ConfigField],
    new_schema: &[ConfigField],
    candidate: &PackageManifest,
    from_version: &Version,
) -> ConfigDiff {
    let renamed = applicable_renames(candidate, from_version);
    let drops = applicable_drops(candidate, from_version);
    let old_keys: BTreeSet<&str> = old_schema.iter().map(|f| f.key.as_str()).collect();
    let new_keys: BTreeSet<&str> = new_schema.iter().map(|f| f.key.as_str()).collect();
    let rename_targets: BTreeSet<&str> = renamed.values().map(String::as_str).collect();
    let rename_sources: BTreeSet<&str> = renamed.keys().map(String::as_str).collect();

    let mut diff = ConfigDiff { renamed: renamed.clone(), ..Default::default() };

    // New fields = in new, not in old, and not the target of a rename (those
    // carry a value, they aren't "new" to prompt for).
    for f in new_schema {
        if old_keys.contains(f.key.as_str()) || rename_targets.contains(f.key.as_str()) {
            // A field flagged rotate_on_update is re-minted, surface it.
            if f.rotate_on_update && f.source == FieldSource::Generate {
                diff.rotated.push(f.key.clone());
            }
            continue;
        }
        if f.source == FieldSource::Input && f.required {
            diff.new_required_inputs.push(f.key.clone());
        } else {
            diff.new_optional.push(f.key.clone());
        }
        if f.rotate_on_update && f.source == FieldSource::Generate {
            diff.rotated.push(f.key.clone());
        }
    }

    // Removed = in old, not in new, not a rename source; plus explicit drops.
    for k in &old_keys {
        if !new_keys.contains(k) && !rename_sources.contains(k) {
            diff.removed.push((*k).to_string());
        }
    }
    for d in drops {
        if !diff.removed.contains(&d) {
            diff.removed.push(d);
        }
    }
    diff.removed.sort();
    diff
}

// Apply the candidate's applicable renames/drops to a prior install's stored
// config, producing the seed config for an update session (key renames carry
// the value; drops discard it).
pub fn apply_migrations(
    prior_config: &serde_json::Value,
    candidate: &PackageManifest,
    from_version: &Version,
) -> BTreeMap<String, serde_json::Value> {
    let renames = applicable_renames(candidate, from_version);
    let drops = applicable_drops(candidate, from_version);
    let mut out = BTreeMap::new();
    if let Some(obj) = prior_config.as_object() {
        for (k, v) in obj {
            if drops.contains(k) {
                continue;
            }
            let key = renames.get(k).cloned().unwrap_or_else(|| k.clone());
            out.insert(key, v.clone());
        }
    }
    out
}

// The publisher key fingerprint a manifest is signed with, if any.
fn signer(m: &PackageManifest) -> Option<&str> {
    m.verification.as_ref().map(|v| v.publisher_key.as_str())
}

// Policy gate: id match, strictly-newer version, `min_mira_version`, valid sig.
pub fn policy_check(
    installed: &InstalledPackage,
    candidate: &PackageManifest,
    candidate_trust: &TrustLevel,
    running_mira: &Version,
) -> Result<(), UpdateBlock> {
    if installed.id != candidate.id {
        return Err(UpdateBlock::IdMismatch {
            installed: installed.id.clone(),
            candidate: candidate.id.clone(),
        });
    }
    if let TrustLevel::Invalid { reason } = candidate_trust {
        return Err(UpdateBlock::InvalidSignature { reason: reason.clone() });
    }
    let installed_v = Version::parse(&installed.version).unwrap_or_else(|_| Version::new(0, 0, 0));
    if candidate.version <= installed_v {
        return Err(UpdateBlock::NotNewer {
            installed: installed.version.clone(),
            candidate: candidate.version.to_string(),
        });
    }
    if let Some(req) = &candidate.min_mira_version {
        if running_mira < req {
            return Err(UpdateBlock::NeedsNewerMira {
                required: req.to_string(),
                running: running_mira.to_string(),
            });
        }
    }
    Ok(())
}

// Build the full update plan. Call [`policy_check`] first (this assumes it
// passed); it parses the installed manifest for the old schema + signer.
pub fn plan_update(
    installed: &InstalledPackage,
    candidate: &PackageManifest,
    candidate_trust: &TrustLevel,
) -> UpdatePlan {
    let old_manifest: Option<PackageManifest> =
        serde_json::from_value(installed.manifest.clone()).ok();
    let from_version =
        Version::parse(&installed.version).unwrap_or_else(|_| Version::new(0, 0, 0));

    let capability = match &old_manifest {
        Some(old) => capability_diff(&aggregate_caps(old), &aggregate_caps(candidate)),
        None => capability_diff(&Capabilities::default(), &aggregate_caps(candidate)),
    };
    let config = config_diff(
        old_manifest.as_ref().map(cpp_schema).unwrap_or(&[]),
        cpp_schema(candidate),
        candidate,
        &from_version,
    );

    // A different (or newly-absent) signer is a new trust decision.
    let old_signer = old_manifest.as_ref().and_then(signer).map(str::to_string);
    let new_signer = signer(candidate).map(str::to_string);
    let trust_changed = old_signer != new_signer;

    UpdatePlan {
        id: candidate.id.clone(),
        from_version: installed.version.clone(),
        to_version: candidate.version.to_string(),
        trust_changed,
        new_trust: candidate_trust.label().to_string(),
        needs_capability_reapproval: capability.is_widened(),
        needs_trust_reapproval: trust_changed,
        capability,
        config,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(egress: &[&str], secrets: &[&str], subp: bool) -> Capabilities {
        Capabilities {
            network_egress: egress.iter().map(|s| s.to_string()).collect(),
            secrets: secrets.iter().map(|s| s.to_string()).collect(),
            subprocess: subp,
            ..Default::default()
        }
    }

    #[test]
    fn capability_diff_flags_only_widenings() {
        let old = caps(&["https://a.com"], &["K1"], false);
        // Same egress, new secret, gained subprocess → widened.
        let new = caps(&["https://a.com"], &["K1", "K2"], true);
        let d = capability_diff(&old, &new);
        assert_eq!(d.added_secrets, vec!["K2"]);
        assert!(d.gained_subprocess);
        assert!(d.added_egress.is_empty());
        assert!(d.is_widened());

        // Narrowing (dropping a secret) is not a widening.
        let narrower = caps(&["https://a.com"], &[], false);
        assert!(!capability_diff(&old, &narrower).is_widened());
    }

    fn field(key: &str, source: FieldSource, required: bool) -> ConfigField {
        ConfigField {
            key: key.into(),
            label: None,
            help: None,
            field_type: super::super::wizard::FieldType::String,
            source,
            secret: false,
            group: None,
            required,
            generate: None,
            derive: None,
            from_step: None,
            default: None,
            enum_values: vec![],
            validate: None,
            visible_when: None,
            required_when: None,
            rotate_on_update: false,
        }
    }

    fn manifest_with(schema: Vec<ConfigField>, migrations: Vec<super::super::wizard::ConfigMigration>) -> PackageManifest {
        let mut m: PackageManifest = serde_json::from_value(serde_json::json!({
            "format":"1","id":"com.x.p","name":"P","version":"2.0.0",
            "components":[{"type":"cpp_provider"}]
        }))
        .unwrap();
        m.components[0].config_schema = schema;
        m.components[0].config_migrations = migrations;
        m
    }

    #[test]
    fn config_diff_classifies_new_removed_and_renamed() {
        let old = vec![field("OLD_USER", FieldSource::Input, true), field("GONE", FieldSource::Input, false)];
        let new = vec![
            field("NEW_USER", FieldSource::Input, true),     // rename target of OLD_USER
            field("EXTRA", FieldSource::Input, true),        // genuinely new + required
            field("OPT", FieldSource::Input, false),         // new optional
        ];
        let mut rename = BTreeMap::new();
        rename.insert("OLD_USER".to_string(), "NEW_USER".to_string());
        let mig = super::super::wizard::ConfigMigration { from: "<2.0.0".into(), rename, drop: vec![] };
        let candidate = manifest_with(new.clone(), vec![mig]);

        let d = config_diff(&old, &new, &candidate, &Version::new(1, 5, 0));
        assert_eq!(d.renamed.get("OLD_USER").map(String::as_str), Some("NEW_USER"));
        assert!(d.new_required_inputs.contains(&"EXTRA".to_string()));
        assert!(d.new_optional.contains(&"OPT".to_string()));
        // NEW_USER is a rename target → not "new to prompt".
        assert!(!d.new_required_inputs.contains(&"NEW_USER".to_string()));
        // GONE removed; OLD_USER not removed (it's a rename source).
        assert!(d.removed.contains(&"GONE".to_string()));
        assert!(!d.removed.contains(&"OLD_USER".to_string()));
    }

    #[test]
    fn rename_only_applies_to_matching_from_range() {
        let old = vec![field("OLD_USER", FieldSource::Input, true)];
        let new = vec![field("NEW_USER", FieldSource::Input, true)];
        let mut rename = BTreeMap::new();
        rename.insert("OLD_USER".to_string(), "NEW_USER".to_string());
        let mig = super::super::wizard::ConfigMigration { from: "<2.0.0".into(), rename, drop: vec![] };
        let candidate = manifest_with(new.clone(), vec![mig]);
        // Updating from 2.1.0 → the <2.0.0 rename does NOT apply.
        let d = config_diff(&old, &new, &candidate, &Version::new(2, 1, 0));
        assert!(d.renamed.is_empty());
        // So NEW_USER reads as genuinely new (+ required), OLD_USER as removed.
        assert!(d.new_required_inputs.contains(&"NEW_USER".to_string()));
        assert!(d.removed.contains(&"OLD_USER".to_string()));
    }

    fn installed(version: &str, manifest: serde_json::Value) -> InstalledPackage {
        InstalledPackage {
            id: "com.x.p".into(),
            version: version.into(),
            name: "P".into(),
            trust: "verified".into(),
            installed_by: "admin".into(),
            installed_at: 0,
            updated_at: 0,
            ledger: vec![],
            manifest,
            config: serde_json::json!({}),
            state: "active".into(),
        }
    }

    #[test]
    fn policy_blocks_downgrade_id_mismatch_and_old_mira() {
        let inst = installed("2.0.0", serde_json::Value::Null);
        let mk = |id: &str, ver: &str, min: Option<&str>| -> PackageManifest {
            let mut v = serde_json::json!({"format":"1","id":id,"name":"P","version":ver,"components":[{"type":"cpp_provider"}]});
            if let Some(m) = min { v["min_mira_version"] = serde_json::json!(m); }
            serde_json::from_value(v).unwrap()
        };
        let trust = TrustLevel::Unsigned;
        let mira = Version::new(0, 223, 0);
        // Downgrade / same.
        assert!(matches!(
            policy_check(&inst, &mk("com.x.p", "2.0.0", None), &trust, &mira),
            Err(UpdateBlock::NotNewer { .. })
        ));
        // Id mismatch.
        assert!(matches!(
            policy_check(&inst, &mk("com.y.q", "3.0.0", None), &trust, &mira),
            Err(UpdateBlock::IdMismatch { .. })
        ));
        // Needs newer MIRA.
        assert!(matches!(
            policy_check(&inst, &mk("com.x.p", "3.0.0", Some("0.999.0")), &trust, &mira),
            Err(UpdateBlock::NeedsNewerMira { .. })
        ));
        // A clean newer version passes.
        assert!(policy_check(&inst, &mk("com.x.p", "2.1.0", None), &trust, &mira).is_ok());
    }

    #[test]
    fn plan_flags_trust_change_when_signer_differs() {
        let old_manifest = serde_json::json!({
            "format":"1","id":"com.x.p","name":"P","version":"1.0.0",
            "components":[{"type":"cpp_provider"}],
            "verification":{"signature":"ed25519:x","publisher_key":"fingerprint:AAA","signed_at":"t"}
        });
        let inst = installed("1.0.0", old_manifest);
        let candidate: PackageManifest = serde_json::from_value(serde_json::json!({
            "format":"1","id":"com.x.p","name":"P","version":"2.0.0",
            "components":[{"type":"cpp_provider"}],
            "verification":{"signature":"ed25519:y","publisher_key":"fingerprint:BBB","signed_at":"t"}
        }))
        .unwrap();
        let plan = plan_update(&inst, &candidate, &TrustLevel::Unsigned);
        assert!(plan.trust_changed);
        assert!(plan.needs_trust_reapproval);
        assert_eq!(plan.from_version, "1.0.0");
        assert_eq!(plan.to_version, "2.0.0");
    }
}
