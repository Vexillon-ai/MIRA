// SPDX-License-Identifier: AGPL-3.0-or-later

//! `mira upgrade --binary` — download, verify, swap, restart.
//!
//! Source-based upgrade (the original `mira upgrade`) needs a git
//! checkout + cargo toolchain on the target machine. That's the right
//! call for dev installs but not for end users who got MIRA via a
//! prebuilt tarball. Binary upgrade closes that loop:
//!
//!   1. Resolve the target version (CLI flag, or "latest" via the
//!      releases API of the configured provider — GitHub by default,
//!      GitLab when selected)
//!   2. Download the matching tarball + .minisig from that provider's
//!      release assets
//!   3. Verify the signature against the public key embedded into
//!      this binary at compile time (`include_str!` from
//!      `verification/release-pubkey.minisign`)
//!   4. Extract, find the `mira` binary inside, atomically swap it
//!      with the running binary (via rename to a sibling tempfile,
//!      then rename onto the destination)
//!   5. Restart via the existing supervisor backend
//!
//! Failures at any stage leave the old binary on disk and the
//! service running — the swap only happens after verify passes and
//! the new binary is fully on disk in the same directory.

use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use minisign_verify::{PublicKey, Signature};

/// Embedded verification key. Same value committed at
/// `verification/release-pubkey.minisign` — including it in the binary
/// means a freshly-installed MIRA can verify its own upgrades without
/// any out-of-band trust bootstrap.
const RELEASE_PUBKEY: &str = include_str!("../../verification/release-pubkey.minisign");

/// Which forge hosts the release artifacts. GitHub and GitLab have
/// different API shapes and asset-URL layouts, so the upgrade plumbing
/// branches on this rather than just swapping a hostname.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseProvider {
    /// GitHub Releases: `api.github.com/repos/<org>/<repo>/releases` for
    /// the version list, `<repo>/releases/download/v<version>/<file>` for
    /// assets. This is the public release source.
    GitHub,
    /// GitLab generic package registry: `<base>/releases` for the version
    /// list, `<base>/packages/generic/mira/<version>/<file>` for assets.
    /// Used by internal builds against the private GitLab.
    GitLab,
}

impl ReleaseProvider {
    /// Parse a provider name (case-insensitive) from config / env.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "github" | "gh" => Some(Self::GitHub),
            "gitlab" | "gl" => Some(Self::GitLab),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
        }
    }
}

/// Provider used when neither the option nor `MIRA_RELEASE_PROVIDER`
/// selects one. Public releases live on GitHub.
const DEFAULT_RELEASE_PROVIDER: ReleaseProvider = ReleaseProvider::GitHub;

/// Default release base for the GitHub provider — the public repo.
/// Overridable via `MIRA_RELEASE_BASE_URL`. Forks / mirrors point this at
/// their own repo. An internal GitLab build selects the other backend
/// with `MIRA_RELEASE_PROVIDER=gitlab` + `MIRA_RELEASE_BASE_URL=<api base>`
/// (e.g. `https://gitlab.example.com/api/v4/projects/<id>`).
const DEFAULT_RELEASE_BASE_URL: &str = "https://github.com/Vexillon-ai/MIRA";

/// Target triple this binary was built for. Determines which tarball
/// suffix we download. v1 ships only one target; later targets get
/// added to the CI pipeline + this match.
const BUILD_TARGET: &str = "x86_64-unknown-linux-gnu";

pub struct BinaryUpgradeOptions {
    /// Specific version to upgrade to (e.g. `"0.84.0"` or `"v0.84.0"`).
    /// `None` = whatever the provider's API reports as the latest release.
    pub version: Option<String>,
    /// Skip the post-swap supervisor restart. The new binary is on
    /// disk; admin can restart manually later.
    pub no_restart: bool,
    /// Bypass the "already on this version, nothing to do" short-
    /// circuit. Useful for repair (binary corrupted on disk) and for
    /// testing the download/verify/swap pipeline without bumping
    /// versions. The actual swap still proceeds atomically.
    pub force: bool,
    /// Which forge to pull releases from. `None` falls back to
    /// `MIRA_RELEASE_PROVIDER`, then [`DEFAULT_RELEASE_PROVIDER`].
    pub provider: Option<ReleaseProvider>,
    /// Override the release base URL. `None` falls back to
    /// `MIRA_RELEASE_BASE_URL`, then [`DEFAULT_RELEASE_BASE_URL`] (GitHub).
    /// Required for the GitLab provider. Useful for forks, mirrors, or a
    /// staging environment.
    pub release_base_url: Option<String>,
    /// Access token for the release host. Optional for public GitHub;
    /// required for private projects / forks. `None` reads
    /// `$MIRA_RELEASE_TOKEN`.
    pub token: Option<String>,
}

pub fn run_binary_upgrade(opts: BinaryUpgradeOptions) -> Result<(), Box<dyn Error>> {
    let provider = opts.provider
        .or_else(|| std::env::var("MIRA_RELEASE_PROVIDER").ok()
            .as_deref()
            .and_then(ReleaseProvider::parse))
        .unwrap_or(DEFAULT_RELEASE_PROVIDER);
    let explicit_base = opts.release_base_url
        .or_else(|| std::env::var("MIRA_RELEASE_BASE_URL").ok());
    let base = match (provider, explicit_base) {
        (_, Some(b)) => b,
        (ReleaseProvider::GitHub, None) => DEFAULT_RELEASE_BASE_URL.to_string(),
        (ReleaseProvider::GitLab, None) => return Err(
            "MIRA_RELEASE_PROVIDER=gitlab requires a base URL — set MIRA_RELEASE_BASE_URL to the \
             GitLab project API base, e.g. https://gitlab.example.com/api/v4/projects/<id>".into(),
        ),
    };
    let token = opts.token
        .or_else(|| std::env::var("MIRA_RELEASE_TOKEN").ok());

    println!("Current version:  {}", env!("CARGO_PKG_VERSION"));
    println!("Build target:     {BUILD_TARGET}");
    println!("Release source:   {base} [{}]", provider.label());

    // 1. Resolve target version.
    let version = match opts.version.as_deref() {
        Some(v) => normalise_version(v),
        None    => fetch_latest_version(provider, &base, token.as_deref())?,
    };
    println!("Upgrading to:     {version}");
    if version == env!("CARGO_PKG_VERSION") && !opts.force {
        println!();
        println!("Already on {version}. Nothing to do.");
        println!("(Pass --force to re-install the same version — useful for repair or for testing the upgrade pipeline.)");
        return Ok(());
    }
    if opts.force && version == env!("CARGO_PKG_VERSION") {
        println!("(--force: re-installing same version to repair / test the pipeline)");
    }

    // 2. Download tarball + signature into a temp dir.
    let tmpdir = tempfile::Builder::new().prefix("mira-upgrade-").tempdir()?;
    let tarball_name = format!("mira-{version}-{BUILD_TARGET}.tar.gz");
    let sig_name     = format!("{tarball_name}.minisig");
    let tarball_url  = asset_url(provider, &base, &version, &tarball_name);
    let sig_url      = asset_url(provider, &base, &version, &sig_name);

    let tarball_path = tmpdir.path().join(&tarball_name);
    let sig_path     = tmpdir.path().join(&sig_name);
    println!();
    println!("Downloading {tarball_name}…");
    download(provider, &tarball_url, &tarball_path, token.as_deref())?;
    println!("Downloading {sig_name}…");
    download(provider, &sig_url, &sig_path, token.as_deref())?;

    // 3. Verify signature with embedded public key.
    println!();
    println!("Verifying signature…");
    verify_signature(&tarball_path, &sig_path)?;
    println!("✓ signature verified");

    // 4. Extract, find new binary.
    println!();
    println!("Extracting…");
    let extract_dir = tmpdir.path().join("extracted");
    fs::create_dir_all(&extract_dir)?;
    extract_tarball(&tarball_path, &extract_dir)?;

    let new_binary = locate_extracted_binary(&extract_dir, &version)?;
    println!("New binary:       {}", new_binary.display());

    // 5. Atomically swap with the running binary.
    let current_binary = std::env::current_exe()?;
    println!("Swapping with:    {}", current_binary.display());
    atomic_swap(&new_binary, &current_binary)?;
    println!("✓ binary replaced");

    if opts.no_restart {
        println!();
        println!("Skipping restart per --no-restart. Run `mira restart` when ready.");
        return Ok(());
    }

    // 6. Restart via existing supervisor backend.
    let unit_installed = crate::install::supervisor_unit_path()
        .map(|p| p.exists())
        .unwrap_or(false);
    if !unit_installed {
        println!();
        println!("No service unit installed — restart MIRA manually to pick up the new build.");
        return Ok(());
    }
    println!();
    println!("Restarting service…");
    crate::install::run_restart()?;
    println!();
    println!("✓ upgrade complete — running version {version}");
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────

/// Strip a leading `v` from a version string. The CLI accepts both
/// `v0.84.0` (matches release tag) and `0.84.0` (matches semver).
fn normalise_version(v: &str) -> String {
    v.trim_start_matches('v').to_string()
}

/// Releases-list API URL for the provider. GitHub derives the API host
/// from the repo base; GitLab uses the project API base directly. Both
/// return a newest-first JSON array of releases.
fn releases_api_url(provider: ReleaseProvider, base: &str) -> String {
    match provider {
        ReleaseProvider::GitHub => {
            let api = base.replace("https://github.com/", "https://api.github.com/repos/");
            // `/releases` (not `/releases/latest`) so a pre-release / beta
            // is still returned as the newest entry.
            format!("{api}/releases?per_page=1")
        }
        ReleaseProvider::GitLab => {
            format!("{base}/releases?per_page=1&order_by=released_at&sort=desc")
        }
    }
}

/// Download URL for a release asset. GitHub addresses assets by the git
/// tag (`v<version>`); GitLab by the generic-package-registry path.
fn asset_url(provider: ReleaseProvider, base: &str, version: &str, file: &str) -> String {
    match provider {
        ReleaseProvider::GitHub => format!("{base}/releases/download/v{version}/{file}"),
        ReleaseProvider::GitLab => format!("{base}/packages/generic/mira/{version}/{file}"),
    }
}

/// Apply provider-appropriate auth + headers. GitHub requires a
/// User-Agent (it rejects API calls without one) and uses
/// `Authorization: Bearer`; GitLab uses the `PRIVATE-TOKEN` header.
fn apply_auth(
    req: reqwest::blocking::RequestBuilder,
    provider: ReleaseProvider,
    token: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match provider {
        ReleaseProvider::GitHub => {
            let req = req
                .header("User-Agent", concat!("mira/", env!("CARGO_PKG_VERSION")))
                .header("Accept", "application/vnd.github+json");
            match token {
                Some(t) => req.header("Authorization", format!("Bearer {t}")),
                None    => req,
            }
        }
        ReleaseProvider::GitLab => match token {
            Some(t) => req.header("PRIVATE-TOKEN", t),
            None    => req,
        },
    }
}

/// Hit the provider's Releases API and pull out the most recent tag.
/// Both GitHub and GitLab return a newest-first JSON array whose first
/// element carries `tag_name`. Returns the version with the `v` stripped.
fn fetch_latest_version(provider: ReleaseProvider, base: &str, token: Option<&str>) -> Result<String, Box<dyn Error>> {
    let url = releases_api_url(provider, base);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = apply_auth(client.get(&url), provider, token)
        .send()?
        .error_for_status()?;
    let body: serde_json::Value = resp.json()?;
    let arr = body.as_array()
        .ok_or("unexpected releases API response — expected a JSON array")?;
    let first = arr.first()
        .ok_or("no releases found — the project hasn't tagged anything yet")?;
    let tag = first.get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("release missing tag_name field")?;
    Ok(normalise_version(tag))
}

/// Download a single URL to a file. Streams to disk so multi-MB
/// tarballs don't sit in memory.
fn download(provider: ReleaseProvider, url: &str, dest: &Path, token: Option<&str>) -> Result<(), Box<dyn Error>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let mut resp = apply_auth(client.get(url), provider, token).send()?;
    if !resp.status().is_success() {
        return Err(format!(
            "download failed: {} for {url}\n\
             (private source? set MIRA_RELEASE_TOKEN to an access token with read access)",
            resp.status(),
        ).into());
    }
    let mut file = fs::File::create(dest)?;
    std::io::copy(&mut resp, &mut file)?;
    file.sync_all()?;
    Ok(())
}

/// Verify `tarball` against `sig` using the embedded public key. The
/// minisign-verify crate handles the format details (genesis comment,
/// trusted comment, signed-comment chain). On success returns Ok(());
/// on failure returns a clear error mentioning what went wrong.
fn verify_signature(tarball: &Path, sig: &Path) -> Result<(), Box<dyn Error>> {
    let pubkey = PublicKey::decode(RELEASE_PUBKEY.trim())
        .map_err(|e| format!("embedded public key is malformed: {e}"))?;
    let sig_text = fs::read_to_string(sig)
        .map_err(|e| format!("read signature {}: {e}", sig.display()))?;
    let signature = Signature::decode(&sig_text)
        .map_err(|e| format!("signature {} is malformed: {e}", sig.display()))?;
    let bytes = fs::read(tarball)
        .map_err(|e| format!("read tarball {}: {e}", tarball.display()))?;
    pubkey.verify(&bytes, &signature, false)
        .map_err(|e| format!(
            "signature does NOT match — refusing to install. \
             ({e}) Possible causes: the tarball was tampered with in transit, \
             or you're upgrading from a fork that uses a different signing key."
        ))?;
    Ok(())
}

/// Extract a `.tar.gz` into `dest`. Lightweight wrapper that surfaces
/// the I/O error on a single line for nicer CLI output.
fn extract_tarball(tarball: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    let f = fs::File::open(tarball)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(dest)?;
    Ok(())
}

/// Find the `mira` binary inside an extracted tarball. The release
/// layout is `mira-<version>-<target>/mira`.
fn locate_extracted_binary(dir: &Path, version: &str) -> Result<PathBuf, Box<dyn Error>> {
    let pkg_dir = dir.join(format!("mira-{version}-{BUILD_TARGET}"));
    let bin = pkg_dir.join("mira");
    if !bin.is_file() {
        return Err(format!(
            "expected binary at {} but didn't find it. \
             Tarball layout may have changed in this release.",
            bin.display(),
        ).into());
    }
    Ok(bin)
}

/// Atomically replace `dest` with `src`. POSIX semantics: rename onto
/// the same filesystem is atomic. We:
///   1. Copy `src` to a sibling tempfile next to `dest` (same filesystem
///      so the next rename is atomic).
///   2. Set the executable bit.
///   3. `fs::rename(tempfile, dest)` — atomic from any reader's view.
///
/// If anything fails before step 3, `dest` is unchanged. After step 3
/// the running process keeps using its open inode (Linux semantics) so
/// it doesn't crash; the new binary takes effect on next exec.
fn atomic_swap(src: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    let parent = dest.parent()
        .ok_or_else(|| format!("destination has no parent dir: {}", dest.display()))?;
    let temp_name = format!(
        ".{}.upgrade-{}",
        dest.file_name().and_then(|s| s.to_str()).unwrap_or("mira"),
        std::process::id(),
    );
    let temp_path = parent.join(temp_name);

    // Stream src → temp_path with the right perms.
    let src_bytes = fs::read(src)
        .map_err(|e| format!("read new binary {}: {e}", src.display()))?;
    {
        let mut f = fs::File::create(&temp_path)
            .map_err(|e| format!("create {}: {e}", temp_path.display()))?;
        f.write_all(&src_bytes)?;
        f.sync_all()?;
    }
    // Set executable bit (rwxr-xr-x).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755))?;
    }
    // The atomic flip.
    fs::rename(&temp_path, dest)
        .map_err(|e| format!("atomic swap {} → {}: {e}", temp_path.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_pubkey_parses_at_compile_time() {
        // The pubkey was committed in slice 7. If this fails, the
        // file shape changed (or the include path broke).
        PublicKey::decode(RELEASE_PUBKEY.trim()).expect("embedded pubkey decodes");
    }

    #[test]
    fn normalise_version_strips_leading_v() {
        assert_eq!(normalise_version("v0.83.0"), "0.83.0");
        assert_eq!(normalise_version("0.83.0"),  "0.83.0");
        assert_eq!(normalise_version(""),        "");
    }

    #[test]
    fn provider_parses_case_insensitively() {
        assert_eq!(ReleaseProvider::parse("GitHub"), Some(ReleaseProvider::GitHub));
        assert_eq!(ReleaseProvider::parse(" gitlab "), Some(ReleaseProvider::GitLab));
        assert_eq!(ReleaseProvider::parse("gh"), Some(ReleaseProvider::GitHub));
        assert_eq!(ReleaseProvider::parse("bitbucket"), None);
    }

    #[test]
    fn asset_and_api_urls_match_each_provider_layout() {
        let gh = "https://github.com/Vexillon-ai/MIRA";
        assert_eq!(
            asset_url(ReleaseProvider::GitHub, gh, "0.272.0", "mira-0.272.0-x.tar.gz"),
            "https://github.com/Vexillon-ai/MIRA/releases/download/v0.272.0/mira-0.272.0-x.tar.gz",
        );
        assert_eq!(
            releases_api_url(ReleaseProvider::GitHub, gh),
            "https://api.github.com/repos/Vexillon-ai/MIRA/releases?per_page=1",
        );
        let gl = "https://gitlab.example.com/api/v4/projects/3";
        assert_eq!(
            asset_url(ReleaseProvider::GitLab, gl, "0.272.0", "mira-0.272.0-x.tar.gz"),
            "https://gitlab.example.com/api/v4/projects/3/packages/generic/mira/0.272.0/mira-0.272.0-x.tar.gz",
        );
        assert!(releases_api_url(ReleaseProvider::GitLab, gl).starts_with(
            "https://gitlab.example.com/api/v4/projects/3/releases?per_page=1"));
    }

    #[test]
    fn atomic_swap_replaces_destination_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("mira");
        fs::write(&dest, b"OLD").unwrap();
        let src = dir.path().join("incoming");
        fs::write(&src, b"NEW BINARY").unwrap();

        atomic_swap(&src, &dest).expect("swap ok");
        assert_eq!(fs::read(&dest).unwrap(), b"NEW BINARY");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "swap should set rwxr-xr-x");
        }
    }

    #[test]
    fn locate_extracted_binary_finds_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join(format!("mira-1.2.3-{BUILD_TARGET}"));
        fs::create_dir_all(&pkg).unwrap();
        let bin = pkg.join("mira");
        fs::write(&bin, b"fake").unwrap();

        let found = locate_extracted_binary(dir.path(), "1.2.3").expect("found");
        assert_eq!(found, bin);
    }

    #[test]
    fn locate_extracted_binary_errors_clearly_when_layout_unexpected() {
        let dir = tempfile::tempdir().unwrap();
        // Don't create the expected dir.
        let err = locate_extracted_binary(dir.path(), "1.2.3").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("expected binary at"), "got: {msg}");
    }
}
