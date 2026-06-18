// SPDX-License-Identifier: AGPL-3.0-or-later

//! Skills that ship with MIRA itself (slice A9).
//!
//! The `bundled-skills/` directory at the repo root is included into the
//! binary via `include_dir!`. On boot, [`extract_missing`] writes any
//! bundled Skill that doesn't yet exist on disk into the user's
//! `<data_dir>/skills/`. After that the bundled set behaves like any
//! other installed Skill — the user can disable, uninstall, edit, etc.
//!
//! User experience model:
//!
//! - **First boot**: every bundled Skill extracts.
//! - **Later boots**: bundled Skills already on disk are left alone, so
//!   user edits survive.
//! - **User uninstalls a bundled Skill**: a marker file under
//!   `<skills_dir>/.bundled-uninstalled/<id>` records the choice so
//!   we don't re-extract on the next boot. (Disable is the more common
//!   choice — uninstall is for "really gone".)
//! - **MIRA upgrade adds a new bundled Skill**: it lands automatically
//!   because there's no on-disk dir and no uninstall marker.

use std::fs;
use std::path::{Path, PathBuf};

use include_dir::{include_dir, Dir};

/// Compile-time embed of `bundled-skills/`. The path is resolved relative
/// to `CARGO_MANIFEST_DIR` so the macro works regardless of where cargo
/// is invoked from.
static BUNDLED: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/bundled-skills");

/// Where uninstall markers live. One empty file per bundled Skill the
/// user explicitly removed.
fn marker_dir(skills_dir: &Path) -> PathBuf {
    skills_dir.join(".bundled-uninstalled")
}

fn marker_path(skills_dir: &Path, id: &str) -> PathBuf {
    marker_dir(skills_dir).join(id)
}

/// Iterate the IDs of every Skill bundled with this MIRA build.
pub fn ids() -> impl Iterator<Item = &'static str> {
    BUNDLED.dirs().filter_map(|d| d.path().file_name()?.to_str())
}

/// True if `id` is one of the Skills bundled with this MIRA build.
pub fn is_bundled(id: &str) -> bool {
    ids().any(|bundled_id| bundled_id == id)
}

/// Record that a user has uninstalled bundled Skill `id`. After this,
/// future boots won't re-extract it. Idempotent.
pub fn mark_uninstalled(skills_dir: &Path, id: &str) -> std::io::Result<()> {
    fs::create_dir_all(marker_dir(skills_dir))?;
    fs::write(marker_path(skills_dir, id), b"")
}

/// Forget the uninstall marker so the next boot will re-extract this
/// bundled Skill. Currently unused by the codebase — exposed for tests
/// and a possible "Re-install bundled" UI button.
pub fn clear_uninstall_marker(skills_dir: &Path, id: &str) -> std::io::Result<()> {
    let p = marker_path(skills_dir, id);
    match fs::remove_file(&p) {
        Ok(())                                             => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e)                                             => Err(e),
    }
}

/// Park a stale bundled-skill directory at `<skills_dir>/.bundled-uninstalled/<old_id>`
/// so the next boot's `extract_or_refresh` doesn't see it as a regular
/// installed skill, but the contents survive for forensic recovery.
/// Used by one-shot rename migrations (e.g. 0.93.0 `com.mira.coding`
/// → `com.mira.claudecode`). Idempotent: a no-op when `<old_id>` isn't
/// present, or already parked.
pub fn park_stale_skill(skills_dir: &Path, old_id: &str) -> std::io::Result<bool> {
    let live = skills_dir.join(old_id);
    if !live.exists() {
        return Ok(false);
    }
    fs::create_dir_all(marker_dir(skills_dir))?;
    let parked = marker_dir(skills_dir).join(old_id);
    if parked.exists() {
        // Already parked from a prior boot. Just remove the live copy
        // so it doesn't keep showing up as an installed skill.
        fs::remove_dir_all(&live)?;
        return Ok(true);
    }
    fs::rename(&live, &parked)?;
    Ok(true)
}

/// Extract every bundled Skill that doesn't have either a directory
/// already at `<skills_dir>/<id>` or an uninstall marker. Returns the
/// list of Skill IDs that were extracted (for logging).
pub fn extract_missing(skills_dir: &Path) -> std::io::Result<Vec<String>> {
    fs::create_dir_all(skills_dir)?;
    let mut extracted = Vec::new();
    for top in BUNDLED.dirs() {
        let Some(id) = top.path().file_name().and_then(|n| n.to_str()) else { continue };
        let dest = skills_dir.join(id);
        if dest.exists()                       { continue; }
        if marker_path(skills_dir, id).exists() { continue; }
        write_dir_recursive(top, &dest)?;
        extracted.push(id.to_string());
    }
    extracted.sort();
    Ok(extracted)
}

/// What happened to one bundled skill during a refresh pass. Returned
/// from `extract_or_refresh` so callers (boot logging, CLI output) can
/// distinguish "we did nothing", "wrote a new install", and
/// "overwrote an old version".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// On-disk dir absent and no uninstall marker — installed fresh.
    Extracted,
    /// On-disk version was older than the bundled version — overwrote.
    Refreshed { from: String, to: String },
    /// On-disk version is the bundled version (or newer / unparseable
    /// and `force` wasn't passed); nothing to do.
    UpToDate,
    /// Either the on-disk dir was a user-edited skill we couldn't read
    /// a manifest from, or the user uninstalled it. Skipped.
    Skipped { reason: String },
    /// Forced overwrite regardless of version (CLI `--force`). Returns
    /// the old version string when readable, "?" otherwise.
    Forced { from: String, to: String },
}

/// Extract bundled skills that aren't on disk AND refresh ones whose
/// on-disk manifest is older than the bundled version. Pass `force =
/// true` to overwrite every bundled skill regardless of version (the
/// CLI's `mira skill refresh-bundled --force` path). Uninstall markers
/// always block both behaviours — the user explicitly opted out.
///
/// Comparisons use semver. A bundled or on-disk version that fails to
/// parse means we can't tell which is newer; treated as up-to-date so
/// we don't trample a user-edited dev install with custom versioning.
/// One per bundled skill — what the binary thinks vs what's on disk.
/// Used by the 0.108.0 `skills.bundled_drift` health detector.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BundledDrift {
    pub skill_id:    String,
    pub bundled_ver: Option<String>,
    pub on_disk_ver: Option<String>,
    /// True when the bundled version is parseable, the on-disk version
    /// is parseable, and bundled > on_disk. Means a refresh would
    /// upgrade the on-disk copy. Other states (missing, unparseable,
    /// equal, on_disk newer) are reported as `false` — the detector
    /// only flags the actionable case.
    pub drift:       bool,
}

/// Scan every bundled skill, comparing against its on-disk counterpart
/// in `skills_dir`. Doesn't write anything; pure read.
pub fn check_drift(skills_dir: &Path) -> Vec<BundledDrift> {
    let mut out = Vec::new();
    for top in BUNDLED.dirs() {
        let Some(id) = top.path().file_name().and_then(|n| n.to_str()) else { continue };
        let dest = skills_dir.join(id);
        if marker_path(skills_dir, id).exists() {
            // Skill was explicitly uninstalled — refresh would skip it
            // anyway, so it doesn't count as drift.
            continue;
        }
        let bundled_ver = manifest_version_from_bundled(top);
        let on_disk_ver = if dest.exists() { manifest_version_from_disk(&dest) } else { None };
        let drift = match (&bundled_ver, &on_disk_ver) {
            (Some(b), Some(d)) => match (semver::Version::parse(b), semver::Version::parse(d)) {
                (Ok(bv), Ok(dv)) => bv > dv,
                _ => false,
            },
            // Missing on-disk = drift (refresh would extract).
            (Some(_), None) => dest.exists() == false,
            _ => false,
        };
        out.push(BundledDrift { skill_id: id.to_string(), bundled_ver, on_disk_ver, drift });
    }
    out.sort_by(|a, b| a.skill_id.cmp(&b.skill_id));
    out
}

pub fn extract_or_refresh(
    skills_dir: &Path,
    force:      bool,
) -> std::io::Result<Vec<(String, RefreshOutcome)>> {
    fs::create_dir_all(skills_dir)?;
    let mut out = Vec::new();
    for top in BUNDLED.dirs() {
        let Some(id) = top.path().file_name().and_then(|n| n.to_str()) else { continue };
        let dest = skills_dir.join(id);
        if marker_path(skills_dir, id).exists() {
            out.push((id.to_string(), RefreshOutcome::Skipped {
                reason: "uninstall marker present".into(),
            }));
            continue;
        }
        if !dest.exists() {
            write_dir_recursive(top, &dest)?;
            out.push((id.to_string(), RefreshOutcome::Extracted));
            continue;
        }
        // Compare versions. The bundled manifest is statically
        // available via include_dir; the on-disk one we read.
        let bundled_ver = manifest_version_from_bundled(top);
        let on_disk_ver = manifest_version_from_disk(&dest);
        let outcome = match (force, &bundled_ver, &on_disk_ver) {
            (true, Some(b), v) => {
                overwrite(top, &dest)?;
                RefreshOutcome::Forced {
                    from: v.clone().unwrap_or_else(|| "?".into()),
                    to:   b.clone(),
                }
            }
            (true, None, v) => {
                overwrite(top, &dest)?;
                RefreshOutcome::Forced {
                    from: v.clone().unwrap_or_else(|| "?".into()),
                    to:   "?".into(),
                }
            }
            (false, Some(b), Some(d)) => {
                match (semver::Version::parse(b), semver::Version::parse(d)) {
                    (Ok(bv), Ok(dv)) if bv > dv => {
                        overwrite(top, &dest)?;
                        RefreshOutcome::Refreshed {
                            from: d.clone(),
                            to:   b.clone(),
                        }
                    }
                    _ => RefreshOutcome::UpToDate,
                }
            }
            (false, _, _) => RefreshOutcome::Skipped {
                reason: "missing or unparseable manifest version".into(),
            },
        };
        out.push((id.to_string(), outcome));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn overwrite(src: &Dir<'_>, dest: &Path) -> std::io::Result<()> {
    // Remove on-disk copy first to avoid leaving stale files behind
    // when the bundled tree no longer contains them. User uninstall
    // markers / per-user prefs live elsewhere, so they survive.
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    write_dir_recursive(src, dest)
}

fn manifest_version_from_bundled(top: &Dir<'_>) -> Option<String> {
    let manifest_path = top.path().join("skill.toml");
    let file = top.get_file(&manifest_path)?;
    let s = std::str::from_utf8(file.contents()).ok()?;
    parse_skill_version_from_toml(s)
}

fn manifest_version_from_disk(dir: &Path) -> Option<String> {
    let s = fs::read_to_string(dir.join("skill.toml")).ok()?;
    parse_skill_version_from_toml(&s)
}

/// Pull `[skill] version = "..."` out of a manifest body without
/// running the full parser/validator. We deliberately don't reuse
/// `SkillManifest::parse` here so a manifest with stricter
/// validation errors (rare but possible mid-version) doesn't make
/// the boot-time refresh fall over.
fn parse_skill_version_from_toml(body: &str) -> Option<String> {
    let val: toml::Value = toml::from_str(body).ok()?;
    val.get("skill")?.get("version")?.as_str().map(String::from)
}

fn write_dir_recursive(src: &Dir<'_>, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for file in src.files() {
        let rel = file.path().strip_prefix(src.path())
            .expect("file path is always under src.path()");
        let abs = dest.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, file.contents())?;
    }
    for sub in src.dirs() {
        let rel = sub.path().strip_prefix(src.path())
            .expect("subdir is always under src.path()");
        write_dir_recursive(sub, &dest.join(rel))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use semver::Version;
    use tempfile::TempDir;

    #[test]
    fn at_least_seven_skills_are_bundled() {
        // Sanity check that the include_dir actually picked up the
        // repo's bundled-skills/ directory. Bumping this floor is a
        // good smoke test that the asset path resolved.
        assert!(ids().count() >= 7, "expected ≥7 bundled Skills, got {}", ids().count());
    }

    #[test]
    fn every_bundled_manifest_validates() {
        // If we ship a malformed bundled Skill, every fresh MIRA install
        // would surface it as a load error — embarrassing. Run our own
        // manifest validator against each at test time so that never ships.
        let dir = TempDir::new().unwrap();
        let extracted = extract_missing(dir.path()).unwrap();
        assert!(!extracted.is_empty());

        let mira_version = Version::parse("99.0.0").unwrap();
        let registry = crate::skills::loader::load_dir(dir.path(), &mira_version);
        assert!(registry.errors.is_empty(),
            "bundled skills failed to load: {:#?}", registry.errors);
        // Same number loaded as extracted (no silent skipping).
        assert_eq!(registry.loaded.len(), extracted.len());
    }

    #[test]
    fn extract_is_idempotent_when_dir_already_exists() {
        let dir = TempDir::new().unwrap();
        let first  = extract_missing(dir.path()).unwrap();
        let second = extract_missing(dir.path()).unwrap();
        assert!(!first.is_empty());
        assert!(second.is_empty(), "second extract should not re-write existing skills");
    }

    #[test]
    fn uninstall_marker_blocks_re_extraction() {
        let dir = TempDir::new().unwrap();
        let extracted = extract_missing(dir.path()).unwrap();
        let id = extracted.first().expect("at least one bundled skill").clone();

        // Simulate user uninstalling this bundled skill.
        fs::remove_dir_all(dir.path().join(&id)).unwrap();
        mark_uninstalled(dir.path(), &id).unwrap();

        let again = extract_missing(dir.path()).unwrap();
        assert!(!again.contains(&id), "should not re-extract {id} after uninstall marker");

        // Clearing the marker re-enables extraction.
        clear_uninstall_marker(dir.path(), &id).unwrap();
        let third = extract_missing(dir.path()).unwrap();
        assert!(third.contains(&id));
    }

    #[test]
    fn user_skills_are_left_alone() {
        let dir = TempDir::new().unwrap();
        let user_skill = dir.path().join("com.user.custom");
        fs::create_dir_all(&user_skill).unwrap();
        fs::write(user_skill.join("skill.toml"), "user-content").unwrap();

        extract_missing(dir.path()).unwrap();

        // User's skill.toml content is unchanged.
        let after = fs::read_to_string(user_skill.join("skill.toml")).unwrap();
        assert_eq!(after, "user-content");
    }

    #[test]
    fn is_bundled_recognises_shipped_ids() {
        assert!(is_bundled("com.mira.research"));
        assert!(is_bundled("com.mira.calculator"));
        assert!(!is_bundled("com.mira.does-not-exist"));
        assert!(!is_bundled("com.user.custom"));
    }

    /// Refresh path leaves up-to-date skills alone and reports
    /// `Refreshed` only for ones with a stale on-disk version.
    #[test]
    fn refresh_overwrites_only_stale_versions() {
        let dir = TempDir::new().unwrap();
        // First-boot extract — every bundled skill lands fresh.
        let report1 = extract_or_refresh(dir.path(), false).unwrap();
        assert!(report1.iter().all(|(_, o)| matches!(o, RefreshOutcome::Extracted)));

        // Second pass without changes — every skill is up-to-date.
        let report2 = extract_or_refresh(dir.path(), false).unwrap();
        assert!(
            report2.iter().all(|(_, o)| matches!(o, RefreshOutcome::UpToDate)),
            "second-pass report: {:?}", report2,
        );

        // Pick any bundled skill and downgrade its on-disk manifest.
        // The third pass must surface it as Refreshed.
        let target = report1.first().expect("≥1 bundled skill").0.clone();
        let target_dir = dir.path().join(&target);
        let toml_path = target_dir.join("skill.toml");
        let original = fs::read_to_string(&toml_path).unwrap();
        let downgraded = original.replacen(
            // every bundled skill currently uses the bare-line form;
            // this regex-free replace is fine for the test.
            "version       = \"",
            "version       = \"0.0.1-old-",
            1,
        );
        // If the manifest doesn't have that exact prefix, replace the
        // first quoted version literal by hand.
        let downgraded = if downgraded == original {
            original.replacen(
                &format!("version = \""),
                &format!("version = \"0.0.1-old-"),
                1,
            )
        } else { downgraded };
        fs::write(&toml_path, &downgraded).unwrap();

        let report3 = extract_or_refresh(dir.path(), false).unwrap();
        let our = report3.iter().find(|(id, _)| id == &target).unwrap();
        assert!(
            matches!(&our.1, RefreshOutcome::Refreshed { .. }),
            "expected Refreshed for {target}, got {:?}", our.1,
        );
        // Manifest is back to the bundled content.
        let restored = fs::read_to_string(&toml_path).unwrap();
        assert_eq!(restored, original);
    }

    /// `--force` overwrites even when versions match.
    #[test]
    fn force_refreshes_up_to_date_skills() {
        let dir = TempDir::new().unwrap();
        extract_or_refresh(dir.path(), false).unwrap();

        // Edit one manifest — without `force`, refresh would leave it.
        let target = ids().next().unwrap().to_string();
        let toml_path = dir.path().join(&target).join("skill.toml");
        let original = fs::read_to_string(&toml_path).unwrap();
        fs::write(&toml_path, "user-edited!").unwrap();

        let report = extract_or_refresh(dir.path(), true).unwrap();
        let our = report.iter().find(|(id, _)| id == &target).unwrap();
        assert!(matches!(&our.1, RefreshOutcome::Forced { .. }));
        let restored = fs::read_to_string(&toml_path).unwrap();
        assert_eq!(restored, original, "force must overwrite user edits");
    }

    /// Refresh respects uninstall markers — an uninstalled bundled
    /// skill stays uninstalled even with `--force`.
    #[test]
    fn refresh_respects_uninstall_marker() {
        let dir = TempDir::new().unwrap();
        extract_or_refresh(dir.path(), false).unwrap();
        let target = ids().next().unwrap().to_string();
        fs::remove_dir_all(dir.path().join(&target)).unwrap();
        mark_uninstalled(dir.path(), &target).unwrap();

        let report = extract_or_refresh(dir.path(), true).unwrap();
        let our = report.iter().find(|(id, _)| id == &target).unwrap();
        assert!(
            matches!(&our.1, RefreshOutcome::Skipped { .. }),
            "uninstall marker must block force refresh, got {:?}", our.1,
        );
        assert!(!dir.path().join(&target).exists());
    }
}
