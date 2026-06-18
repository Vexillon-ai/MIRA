// SPDX-License-Identifier: AGPL-3.0-or-later

//! `mira deps` — managed native deps (ONNX Runtime today, signal-cli
//! / JRE in v1.1+).
//!
//! Closes the "fresh tarball install fails until you `apt install
//! libonnxruntime`" gap by giving MIRA its own pinned + verified
//! fetcher for upstream binaries. Same model as `nvm`, `pyenv`,
//! `rustup`: download from upstream → verify → extract to
//! `~/.mira/deps/<name>/` → at runtime, MIRA sets the right env vars
//! to point dynamic loaders at the bundled libs first.
//!
//! Manifest lives at `deps/manifest.toml` and is embedded at compile
//! time via `include_str!` — the binary always knows what versions it
//! expects, no out-of-band manifest fetch at runtime.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const MANIFEST_TOML: &str = include_str!("../../deps/manifest.toml");

/// Top-level manifest shape. One `deps.<name>` block per dep.
#[derive(Debug, Deserialize)]
pub struct DepsManifest {
    pub deps: std::collections::BTreeMap<String, DepEntry>,
}

#[derive(Debug, Deserialize)]
pub struct DepEntry {
    pub version:      String,
    pub description:  String,
    pub required_for: String,
    /// Per-platform variants keyed by `<os>-<arch>` (e.g. `linux-x86_64`).
    /// Captured as a flat map so unknown platform keys don't break
    /// parsing — the lookup at runtime just returns None and the user
    /// sees a clear "no pin for your platform" error.
    #[serde(flatten)]
    pub platforms:    std::collections::BTreeMap<String, PlatformEntry>,
}

#[derive(Debug, Deserialize)]
pub struct PlatformEntry {
    pub url:      String,
    pub sha256:   String,
    pub lib_path: String,
}

impl DepsManifest {
    /// Parse the embedded manifest. Errors mean a malformed
    /// `deps/manifest.toml` made it into a release build, which is
    /// caught at test time by `embedded_manifest_parses_at_compile_time`.
    pub fn load() -> Result<Self, Box<dyn Error>> {
        let m: DepsManifest = toml::from_str(MANIFEST_TOML)
            .map_err(|e| format!("parse embedded deps manifest: {e}"))?;
        Ok(m)
    }
}

/// Where a dep gets extracted on disk. `~/.mira/deps/<name>/`.
pub fn dep_install_dir(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let home = dirs::home_dir().ok_or("could not determine home dir")?;
    Ok(home.join(".mira").join("deps").join(name))
}

/// Current platform key used to look up a `PlatformEntry`. Maps
/// `std::env::consts::{OS,ARCH}` to the `<os>-<arch>` strings used in
/// the manifest. Keep this in sync with the manifest's per-platform
/// subsections when adding new targets.
pub fn current_platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

// ─── User commands ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum DepsCommand {
    Install { force: bool },
    Verify,
    List,
}

pub fn run(cmd: DepsCommand) -> Result<(), Box<dyn Error>> {
    let manifest = DepsManifest::load()?;
    match cmd {
        DepsCommand::Install { force } => install_all(&manifest, force),
        DepsCommand::Verify             => verify_all(&manifest),
        DepsCommand::List               => list_all(&manifest),
    }
}

fn install_all(manifest: &DepsManifest, force: bool) -> Result<(), Box<dyn Error>> {
    let plat = current_platform_key();
    println!("Installing deps for platform: {plat}");
    let mut installed = 0;
    let mut skipped   = 0;
    let mut errors    = Vec::new();

    for (name, dep) in &manifest.deps {
        let Some(p) = dep.platforms.get(&plat) else {
            println!("  - {name} {}: skipped (no pin for {plat} in manifest)",
                dep.version);
            skipped += 1;
            continue;
        };
        match install_one(name, dep, p, force) {
            Ok(true)  => { println!("  ✓ {name} {} installed", dep.version); installed += 1; }
            Ok(false) => { println!("  · {name} {} already present (sha matches)", dep.version); skipped += 1; }
            Err(e)    => { println!("  ✗ {name} {}: {e}", dep.version); errors.push(name.clone()); }
        }
    }

    println!();
    println!("{installed} installed, {skipped} skipped, {} failed", errors.len());
    if errors.is_empty() { Ok(()) }
    else                 { Err(format!("install failed for: {}", errors.join(", ")).into()) }
}

/// Install one dep × one platform. Returns Ok(true) when bytes were
/// fetched + extracted; Ok(false) when the lib file already exists
/// AND its parent dir's hash anchor matches (we record the source
/// sha256 in a sidecar).
fn install_one(
    name: &str,
    _dep: &DepEntry,
    plat: &PlatformEntry,
    force: bool,
) -> Result<bool, Box<dyn Error>> {
    let install_dir = dep_install_dir(name)?;
    let lib_path    = install_dir.join(&plat.lib_path);
    let sha_marker  = install_dir.join(".sha256");

    // Already installed AND matches the manifest sha → no-op.
    if !force && lib_path.exists() && sha_marker.exists() {
        let recorded = fs::read_to_string(&sha_marker).unwrap_or_default();
        if recorded.trim() == plat.sha256 {
            return Ok(false);
        }
    }

    // Wipe any prior install so we don't mix old + new file trees.
    if install_dir.exists() {
        fs::remove_dir_all(&install_dir)
            .map_err(|e| format!("remove old install dir {}: {e}", install_dir.display()))?;
    }
    fs::create_dir_all(&install_dir)?;

    // Download to a tempfile inside install_dir (same FS so the
    // extract step doesn't cross-mount).
    let tmpfile = install_dir.join(".incoming.tar.gz");
    println!("    fetching {} …", plat.url);
    download(&plat.url, &tmpfile)?;

    // Verify sha256 BEFORE extracting — refuses to touch disk if
    // the bytes don't match.
    let actual = sha256_of_file(&tmpfile)?;
    if actual != plat.sha256 {
        let _ = fs::remove_file(&tmpfile);
        return Err(format!(
            "sha256 mismatch for {name} ({}): expected {}, got {actual}. \
             Refusing to install — possible MITM or upstream change.",
            plat.url, plat.sha256,
        ).into());
    }

    println!("    extracting …");
    extract_tarball(&tmpfile, &install_dir)?;
    fs::remove_file(&tmpfile).ok();

    // Sanity-check the lib_path actually exists post-extract — guards
    // against a manifest that says one path but the tarball has another.
    if !lib_path.exists() {
        return Err(format!(
            "post-extract sanity check: expected {} to exist but it doesn't. \
             Manifest's lib_path may be wrong for this version.",
            lib_path.display(),
        ).into());
    }

    // Write the sha marker so subsequent runs can short-circuit.
    fs::write(&sha_marker, &plat.sha256)?;
    Ok(true)
}

fn verify_all(manifest: &DepsManifest) -> Result<(), Box<dyn Error>> {
    let plat = current_platform_key();
    let mut missing = Vec::new();

    for (name, dep) in &manifest.deps {
        let Some(p) = dep.platforms.get(&plat) else {
            println!("  - {name}: no manifest pin for {plat} (skipping)");
            continue;
        };
        let lib = dep_install_dir(name)?.join(&p.lib_path);
        if lib.exists() {
            println!("  ✓ {name} {} → {}", dep.version, lib.display());
        } else {
            println!("  ✗ {name} {} MISSING (expected {})", dep.version, lib.display());
            missing.push(name.clone());
        }
    }

    if missing.is_empty() {
        println!();
        println!("All deps present.");
        Ok(())
    } else {
        Err(format!(
            "missing deps: {}. Run `mira deps install`.",
            missing.join(", ")
        ).into())
    }
}

fn list_all(manifest: &DepsManifest) -> Result<(), Box<dyn Error>> {
    let plat = current_platform_key();
    println!("Platform: {plat}");
    println!();
    for (name, dep) in &manifest.deps {
        println!("  {name} {}", dep.version);
        println!("    {}", dep.description);
        println!("    Required for: {}", dep.required_for);
        match dep.platforms.get(&plat) {
            Some(p) => {
                let installed = dep_install_dir(name)
                    .map(|d| d.join(&p.lib_path).exists())
                    .unwrap_or(false);
                println!("    Status: {}", if installed { "installed" } else { "not installed" });
                println!("    URL:    {}", p.url);
            }
            None => println!("    Status: NO PIN for {plat} — manifest doesn't support your platform"),
        }
        println!();
    }
    Ok(())
}

// ─── Runtime helper used by main.rs at startup ──────────────────────

/// True when fastembed's underlying ONNX Runtime can be located on
/// this host. Checked before initialising the `internal` embedding
/// provider — `panic = "abort"` in our release profile means a missed
/// dlopen aborts the whole process, so we have to know up-front
/// whether the lib is reachable.
///
/// Resolution order (matches `maybe_apply_runtime_env` + ort's own
/// loader):
///   1. `ORT_DYLIB_PATH` env var → that file must exist
///   2. `~/.mira/deps/onnxruntime/<lib_path>` from the embedded
///      manifest, when a pin exists for the current platform
///
/// Returns false on missing manifest, missing platform pin, or
/// missing lib. We deliberately do NOT probe system loader paths —
/// success there can't be predicted without dlopen, and dlopen on
/// `libonnxruntime.so` already aborts under panic=abort if it pulls
/// in incompatible deps. The two paths above are the ones MIRA is
/// in a position to provision.
pub fn is_onnxruntime_available() -> bool {
    if let Some(p) = std::env::var_os("ORT_DYLIB_PATH") {
        return Path::new(&p).is_file();
    }
    let Ok(manifest) = DepsManifest::load() else { return false; };
    let plat = current_platform_key();
    let Some(dep)  = manifest.deps.get("onnxruntime") else { return false; };
    let Some(p)    = dep.platforms.get(&plat)         else { return false; };
    let Ok(install_dir) = dep_install_dir("onnxruntime") else { return false; };
    install_dir.join(&p.lib_path).is_file()
}

/// Install one named dep on demand from the HTTP API. Public wrapper
/// around `install_one` so the deps handler doesn't need to reach
/// into private helpers. Returns `Ok(was_fetched)` — false means the
/// lib was already present and matches the manifest sha.
pub fn install_named(name: &str, force: bool) -> Result<bool, Box<dyn Error>> {
    let manifest = DepsManifest::load()?;
    let plat = current_platform_key();
    let dep = manifest.deps.get(name)
        .ok_or_else(|| format!("unknown dep '{name}' (not in embedded manifest)"))?;
    let p = dep.platforms.get(&plat)
        .ok_or_else(|| format!("no manifest pin for {name} on {plat}"))?;
    install_one(name, dep, p, force)
}

/// Snapshot of every managed dep's install state. Used by the admin
/// UI to render the deps page and decide whether the embedding-
/// provider save needs an install-and-retry dance.
#[derive(Debug, serde::Serialize)]
pub struct DepStatus {
    pub name:         String,
    pub version:      String,
    pub description:  String,
    pub required_for: String,
    pub installed:    bool,
    pub lib_path:     Option<String>,
    pub platform:     String,
    pub supported:    bool,
}

pub fn list_status() -> Result<Vec<DepStatus>, Box<dyn Error>> {
    let manifest = DepsManifest::load()?;
    let plat = current_platform_key();
    let mut out = Vec::new();
    for (name, dep) in &manifest.deps {
        let (installed, lib_path, supported) = match dep.platforms.get(&plat) {
            Some(p) => {
                let lib = dep_install_dir(name).ok().map(|d| d.join(&p.lib_path));
                let installed = lib.as_ref().is_some_and(|l| l.is_file());
                (installed, lib.map(|l| l.display().to_string()), true)
            }
            None => (false, None, false),
        };
        out.push(DepStatus {
            name:         name.clone(),
            version:      dep.version.clone(),
            description:  dep.description.clone(),
            required_for: dep.required_for.clone(),
            installed,
            lib_path,
            platform:     plat.clone(),
            supported,
        });
    }
    Ok(out)
}

/// If a managed ONNX Runtime exists in `~/.mira/deps/onnxruntime/`,
/// set `ORT_DYLIB_PATH` to its location so fastembed loads the
/// bundled lib instead of the system one. Idempotent + non-clobbering:
/// existing `ORT_DYLIB_PATH` (set by the user / by `mira install --
/// macos`) takes precedence.
///
/// Called from `main.rs` BEFORE any subsystem touches embeddings, so
/// the env var is in scope when fastembed dlopens.
pub fn maybe_apply_runtime_env() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return; // user / installer already set it
    }
    let Ok(manifest) = DepsManifest::load() else { return; };
    let plat = current_platform_key();
    let Some(dep) = manifest.deps.get("onnxruntime") else { return; };
    let Some(p)   = dep.platforms.get(&plat)         else { return; };
    let Ok(install_dir) = dep_install_dir("onnxruntime") else { return; };
    let lib = install_dir.join(&p.lib_path);
    if lib.exists() {
        // SAFETY: setting env at startup, before any subsystem reads
        // it. Single-threaded at this point.
        unsafe { std::env::set_var("ORT_DYLIB_PATH", &lib); }
    }
}

// ─── Pure helpers ─────────────────────────────────────────────────────

fn download(url: &str, dest: &Path) -> Result<(), Box<dyn Error>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let mut resp = client.get(url).send()?;
    if !resp.status().is_success() {
        return Err(format!("download {url}: {}", resp.status()).into());
    }
    let mut file = fs::File::create(dest)?;
    std::io::copy(&mut resp, &mut file)?;
    file.sync_all()?;
    Ok(())
}

fn sha256_of_file(path: &Path) -> Result<String, Box<dyn Error>> {
    use sha2::{Digest, Sha256};
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_tarball(tarball: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    let f = fs::File::open(tarball)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_manifest_parses_at_compile_time() {
        let m = DepsManifest::load().expect("manifest parses");
        assert!(!m.deps.is_empty(), "manifest must declare at least one dep");
    }

    #[test]
    fn manifest_includes_onnxruntime_for_linux_x86_64() {
        let m = DepsManifest::load().unwrap();
        let dep = m.deps.get("onnxruntime").expect("onnxruntime must be in manifest");
        let plat = dep.platforms.get("linux-x86_64")
            .expect("linux-x86_64 pin must be present");
        assert!(plat.url.starts_with("https://"), "url must be https");
        assert_eq!(plat.sha256.len(), 64, "sha256 must be 64-char hex");
        assert!(plat.lib_path.contains("libonnxruntime"), "lib_path must point at the actual lib");
    }

    #[test]
    fn current_platform_key_format_matches_manifest_keys() {
        // The format `<os>-<arch>` is the contract between the
        // runtime and the manifest. If this fails, the manifest's
        // platform subsections need renaming.
        let key = current_platform_key();
        assert!(key.contains('-'));
        let parts: Vec<&str> = key.splitn(2, '-').collect();
        assert_eq!(parts.len(), 2);
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
    }

    #[test]
    fn dep_install_dir_resolves_under_home() {
        let p = dep_install_dir("onnxruntime").expect("home dir resolves");
        // Normalize separators so the assert holds on Windows (backslashes) too.
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(s.contains(".mira/deps/onnxruntime"),
            "expected ~/.mira/deps/onnxruntime, got {s}");
    }

    #[test]
    fn maybe_apply_runtime_env_skips_when_already_set() {
        // We can't reliably test the "set var" branch without a real
        // installed dep on disk, but the early-out for an
        // already-set env var is verifiable.
        // SAFETY: single-threaded test; rustc 2024 made env mutators
        // unsafe to flag the cross-thread risk that doesn't apply here.
        unsafe { std::env::set_var("ORT_DYLIB_PATH", "/sentinel/should-not-clobber"); }
        maybe_apply_runtime_env();
        assert_eq!(
            std::env::var("ORT_DYLIB_PATH").unwrap(),
            "/sentinel/should-not-clobber"
        );
        unsafe { std::env::remove_var("ORT_DYLIB_PATH"); }
    }
}
