// SPDX-License-Identifier: AGPL-3.0-or-later

// src/sandbox/rootfs.rs
//! Sandbox rootfs management.
//!
//! Tier 4 tools need an isolated filesystem to `pivot_root` into. We use
//! `python-build-standalone` (https://github.com/indygreg/python-build-standalone)
//! — a relocatable, self-contained CPython tarball with no host glibc
//! dependency. Pin a version + SHA256 in the manifest below; bumping the
//! pinned version is a MIRA release.
//!
//! On-disk layout under `<data_dir>/sandbox/`:
//!
//! ```text
//! sandbox/
//! ├── rootfs/
//! │   └── python-3.12.7/        ← extracted tarball; this is the pivot root
//! │       ├── bin/python3
//! │       └── lib/python3.12/…
//! └── cache/
//!     └── cpython-3.12.7-…tar.gz  ← original archive, kept for re-extract
//! ```
//!
//! The same rootfs is shared (read-only) across all `code_run` invocations;
//! the per-call scratch tmpfs is bound on top at pivot time.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::MiraError;

// ── Manifest (pinned releases) ───────────────────────────────────────────────

/// One pinned tarball: which CPython, which CPU arch, where to fetch it,
/// and the SHA256 to verify against. URLs and version are fixed for any
/// given MIRA release.
pub struct ReleaseAsset {
    pub arch:   &'static str,    // matches `std::env::consts::ARCH`
    pub url:    &'static str,
    pub sha256: &'static str,    // hex-encoded; PLACEHOLDER means "not finalized"
}

pub struct PythonRelease {
    pub version: &'static str,    // e.g. "3.12.7"
    pub tag:     &'static str,    // python-build-standalone release tag
    pub assets:  &'static [ReleaseAsset],
}

/// Sentinel SHA256 that means "this release has not been verified yet".
/// `install` refuses to proceed against any asset still set to this value
/// unless `MIRA_SANDBOX_DEV_INSTALL=1` is exported. Replace before shipping.
pub const PLACEHOLDER_SHA: &str = "PLACEHOLDER_SHA256_REPLACE_BEFORE_SHIP";

/// Currently pinned Python rootfs. To bump:
///   1. Pick a release at https://github.com/indygreg/python-build-standalone/releases
///   2. Update `version` and `tag`.
///   3. Replace the SHA256 placeholders with the values from the release's
///      `SHA256SUMS` file (verify the GPG signature first).
///   4. Bump MIRA's MINOR version per `CLAUDE.md`.
pub const PYTHON: PythonRelease = PythonRelease {
    version: "3.12.7",
    tag:     "20241016",
    assets: &[
        ReleaseAsset {
            arch:   "x86_64",
            url:    "https://github.com/indygreg/python-build-standalone/releases/download/20241016/cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only.tar.gz",
            sha256: "43576f7db1033dd57b900307f09c2e86f371152ac8a2607133afa51cbfc36064",
        },
        ReleaseAsset {
            arch:   "aarch64",
            url:    "https://github.com/indygreg/python-build-standalone/releases/download/20241016/cpython-3.12.7+20241016-aarch64-unknown-linux-gnu-install_only.tar.gz",
            sha256: "bba3c6be6153f715f2941da34f3a6a69c2d0035c9c5396bc5bb68c6d2bd1065a",
        },
    ],
};

impl PythonRelease {
    pub fn asset_for_host(&self) -> Option<&ReleaseAsset> {
        let arch = std::env::consts::ARCH;
        self.assets.iter().find(|a| a.arch == arch)
    }
}

// ── On-disk layout ───────────────────────────────────────────────────────────

/// Resolves and creates the `<data_dir>/sandbox/{rootfs,cache}` paths and
/// answers questions about what's installed.
#[derive(Debug, Clone)]
pub struct RootfsManager {
    base_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct InstalledRootfs {
    pub language:   &'static str,
    pub version:    String,
    pub path:       PathBuf,
    pub size_bytes: u64,
}

impl RootfsManager {
    /// `data_dir` is whatever `MiraConfig::data_dir_path()` returned. The
    /// constructor doesn't create directories — call `ensure_dirs()` if you
    /// want them, or just call `install_python` which creates as needed.
    pub fn new(data_dir: &Path) -> Self {
        Self { base_dir: data_dir.join("sandbox") }
    }

    pub fn rootfs_dir(&self) -> PathBuf { self.base_dir.join("rootfs") }
    pub fn cache_dir(&self)  -> PathBuf { self.base_dir.join("cache") }

    /// Where the Python rootfs lives once installed. The directory may not
    /// exist; check via `python_installed()` or `list()`.
    pub fn python_root(&self) -> PathBuf {
        self.rootfs_dir().join(format!("python-{}", PYTHON.version))
    }

    /// The directory the sandbox actually `pivot_root`s into. The standalone
    /// tarball extracts to a `python/` subdir, and that subdir is what we
    /// treat as the new `/`. Callers wiring `ResourceLimits::rootfs` should
    /// use this, not `python_root()`.
    pub fn python_pivot_root(&self) -> PathBuf {
        self.python_root().join("python")
    }

    /// `<pivot_root>/bin/python3` — the interpreter `Language::Python` will
    /// invoke after `pivot_root`. The path is _inside_ the pivoted root,
    /// so callers building the post-pivot command want `/bin/python3`, not
    /// this absolute host path. This getter returns the host-side path for
    /// install verification.
    pub fn python_interpreter(&self) -> PathBuf {
        self.python_pivot_root().join("bin/python3")
    }

    pub fn python_installed(&self) -> bool {
        self.python_interpreter().is_file()
    }

    /// All currently installed rootfs entries with their on-disk size.
    pub fn list(&self) -> Vec<InstalledRootfs> {
        let mut out = Vec::new();
        if self.python_installed() {
            let path = self.python_root();
            let size = dir_size(&path).unwrap_or(0);
            out.push(InstalledRootfs {
                language:   "python",
                version:    PYTHON.version.to_string(),
                path,
                size_bytes: size,
            });
        }
        out
    }

    pub fn ensure_dirs(&self) -> Result<(), MiraError> {
        for dir in [self.rootfs_dir(), self.cache_dir()] {
            fs::create_dir_all(&dir).map_err(|e| MiraError::ConfigError(format!(
                "creating sandbox dir {}: {e}", dir.display()
            )))?;
        }
        Ok(())
    }

    /// Fetch + verify + extract the pinned Python rootfs. No-op if already
    /// installed (compare the interpreter binary's existence).
    pub async fn install_python(&self, force_redownload: bool) -> Result<PathBuf, MiraError> {
        let asset = PYTHON.asset_for_host().ok_or_else(|| MiraError::ConfigError(format!(
            "no python-build-standalone asset for arch '{}'", std::env::consts::ARCH
        )))?;

        if self.python_installed() && !force_redownload {
            info!("python rootfs already installed at {}", self.python_root().display());
            return Ok(self.python_root());
        }

        self.ensure_dirs()?;
        let archive = self.cache_dir().join(format!(
            "cpython-{}-{}-{}.tar.gz", PYTHON.version, PYTHON.tag, asset.arch
        ));

        if !archive.exists() || force_redownload {
            download_to(asset.url, &archive).await?;
        } else {
            info!("using cached archive {}", archive.display());
        }

        verify_sha256(&archive, asset.sha256)?;

        // Extract in two steps so a half-extracted dir never lingers under
        // the canonical name.
        let staging = self.rootfs_dir().join(format!("python-{}.partial", PYTHON.version));
        if staging.exists() {
            fs::remove_dir_all(&staging).ok();
        }
        fs::create_dir_all(&staging).map_err(|e| MiraError::ConfigError(format!(
            "creating staging dir: {e}"
        )))?;
        extract_tar_gz(&archive, &staging)?;

        let final_dir = self.python_root();
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir).map_err(|e| MiraError::ConfigError(format!(
                "removing prior install at {}: {e}", final_dir.display()
            )))?;
        }
        fs::rename(&staging, &final_dir).map_err(|e| MiraError::ConfigError(format!(
            "promoting staging dir: {e}"
        )))?;

        if !self.python_installed() {
            return Err(MiraError::ConfigError(format!(
                "extraction completed but {} is missing — archive layout may have changed",
                self.python_interpreter().display()
            )));
        }

        // Pre-create empty mountpoints inside the pivot root. The Linux
        // sandbox's pre_exec mounts tmpfs at /tmp, proc at /proc, a tmpfs at
        // /dev (for null + urandom binds), and pivot_roots into the pivot
        // root with old / parked under /old_root. Those directories must
        // exist on the underlying rootfs before the bind goes RO.
        let pivot = self.python_pivot_root();
        for sub in ["tmp", "proc", "dev", "old_root"] {
            fs::create_dir_all(pivot.join(sub)).map_err(|e| MiraError::ConfigError(format!(
                "creating mountpoint {sub} in pivot root: {e}"
            )))?;
        }

        // Bake the dynamic linker + glibc + every shared lib python and its
        // C extensions need, into the rootfs. python-build-standalone's
        // `install_only` archive is *not* self-contained — its ELF carries
        // `PT_INTERP=/lib64/ld-linux-x86-64.so.2`, and once the loader runs
        // it pulls libc, libpthread, libssl, libffi, etc. from the host's
        // glibc tree. Without these inside the rootfs, execve() of
        // /bin/python3 returns ENOENT after pivot_root.
        //
        // Previously we worked around this by bind-mounting the host's
        // /lib64 + /lib/x86_64-linux-gnu + /usr/lib/x86_64-linux-gnu RO
        // into the rootfs at sandbox spawn time. That works but exposes
        // every shared library on the host to the sandbox — defeats the
        // isolation we just built. Baking at install time costs ~50–80 MB
        // of disk and pins the rootfs to the host's glibc ABI, both
        // acceptable for a machine-local install.
        bake_loader_and_libs(&pivot)?;

        let size = dir_size(&final_dir).unwrap_or(0);
        info!(
            "installed python rootfs ({} {}, {:.1} MB) at {}",
            PYTHON.version, asset.arch, size as f64 / (1024.0 * 1024.0),
            final_dir.display()
        );
        Ok(final_dir)
    }

    pub fn uninstall_python(&self) -> Result<bool, MiraError> {
        let path = self.python_root();
        if !path.exists() { return Ok(false); }
        fs::remove_dir_all(&path).map_err(|e| MiraError::ConfigError(format!(
            "removing {}: {e}", path.display()
        )))?;
        Ok(true)
    }
}

// ── Download / verify / extract ──────────────────────────────────────────────

async fn download_to(url: &str, dest: &Path) -> Result<(), MiraError> {
    info!("downloading {} → {}", url, dest.display());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| MiraError::ConfigError(format!("http client: {e}")))?;

    let resp = client.get(url).send().await
        .map_err(|e| MiraError::ConfigError(format!("download {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(MiraError::ConfigError(format!(
            "download {url} returned HTTP {}", resp.status()
        )));
    }
    let bytes = resp.bytes().await
        .map_err(|e| MiraError::ConfigError(format!("read body {url}: {e}")))?;

    let tmp = dest.with_extension("partial");
    fs::write(&tmp, &bytes).map_err(|e| MiraError::ConfigError(format!(
        "write {}: {e}", tmp.display()
    )))?;
    fs::rename(&tmp, dest).map_err(|e| MiraError::ConfigError(format!(
        "rename {} → {}: {e}", tmp.display(), dest.display()
    )))?;
    Ok(())
}

pub fn verify_sha256(path: &Path, expected_hex: &str) -> Result<(), MiraError> {
    if expected_hex == PLACEHOLDER_SHA {
        let dev = std::env::var("MIRA_SANDBOX_DEV_INSTALL").ok().as_deref() == Some("1");
        if !dev {
            return Err(MiraError::ConfigError(
                "rootfs SHA256 manifest has not been finalized in this MIRA build. \
                 Set MIRA_SANDBOX_DEV_INSTALL=1 to bypass for development. \
                 See design-docs/phase7a-5-code-run.md.".into()
            ));
        }
        warn!("MIRA_SANDBOX_DEV_INSTALL=1 — skipping SHA256 verification for {}", path.display());
        return Ok(());
    }

    let actual = sha256_hex(path)?;
    if !actual.eq_ignore_ascii_case(expected_hex) {
        return Err(MiraError::ConfigError(format!(
            "SHA256 mismatch for {}\n  expected {}\n  actual   {}",
            path.display(), expected_hex, actual
        )));
    }
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String, MiraError> {
    let mut f = fs::File::open(path).map_err(|e| MiraError::ConfigError(format!(
        "open {} for hashing: {e}", path.display()
    )))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(io_to_mira)?;
        if n == 0 { break; }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// Extract a `.tar.gz` archive into `dest`. We shell out to the host `tar`
/// binary — it's universally available on Linux, the only platform where
/// rootfs install runs at all, and avoids pulling tar+flate2 reader code
/// into the binary.
fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), MiraError> {
    let status = Command::new("tar")
        .arg("-xzf").arg(archive)
        .arg("-C").arg(dest)
        .status()
        .map_err(|e| MiraError::ConfigError(format!(
            "spawning tar (is it installed?): {e}"
        )))?;
    if !status.success() {
        return Err(MiraError::ConfigError(format!(
            "tar exited with {}", status
        )));
    }
    Ok(())
}

// ── Loader + libs bake (replaces runtime bind-mounts) ────────────────────────

/// Discover every dynamically-linked binary under the pivot root that the
/// sandbox is going to execute or `dlopen`, and copy each shared library it
/// needs (along with the dynamic linker itself) from the host into the rootfs
/// at the same absolute path. After this runs, the rootfs is self-contained:
/// `pivot_root` into it and `/lib64/ld-linux-x86-64.so.2` plus everything
/// glibc cascades into are real files inside the chroot.
///
/// We seed with `<pivot>/bin/python3` and every `.so` under
/// `<pivot>/lib/python3.X/lib-dynload/` (Python's C-extension modules are
/// loaded at runtime, so their NEEDED entries matter as much as python3's).
fn bake_loader_and_libs(pivot: &Path) -> Result<(), MiraError> {
    let mut binaries: Vec<PathBuf> = Vec::new();

    let py3 = pivot.join("bin/python3");
    if !py3.exists() {
        return Err(MiraError::ConfigError(format!(
            "bake_loader_and_libs: {} missing", py3.display()
        )));
    }
    binaries.push(py3);

    // Walk the lib-dynload dir(s) for Python's C extensions. Glob-style
    // scan over lib/python*/lib-dynload — the dir name is python3.12 today
    // but the version might bump.
    let lib_dir = pivot.join("lib");
    if let Ok(rd) = fs::read_dir(&lib_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("python") { continue; }
            let dynload = entry.path().join("lib-dynload");
            if !dynload.is_dir() { continue; }
            collect_so_files(&dynload, &mut binaries);
        }
    }

    // ldd every binary, dedupe paths.
    let mut to_copy: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for bin in &binaries {
        for path in ldd_paths(bin)? {
            // Skip paths that already live inside the pivot — those are
            // libpython.so and friends that python-build-standalone bundles.
            // The dynamic linker resolves them via $ORIGIN at runtime.
            if path.starts_with(pivot) { continue; }
            to_copy.insert(path);
        }
    }

    // Copy each lib into the rootfs at its host absolute path, preserving
    // permissions. fs::copy follows symlinks, so a symlinked libc.so.6 →
    // libc-2.39.so becomes a real file at libc.so.6 inside the rootfs —
    // that's what the loader opens by name, so it works.
    let mut copied = 0usize;
    let mut total_bytes = 0u64;
    for src in &to_copy {
        if !src.exists() {
            warn!("bake_loader_and_libs: ldd reported {} but it doesn't exist on host — skipping", src.display());
            continue;
        }
        let rel = src.strip_prefix("/").unwrap_or(src);
        let dst = pivot.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| MiraError::ConfigError(format!(
                "bake_loader_and_libs: mkdir {}: {e}", parent.display()
            )))?;
        }
        let bytes = fs::copy(src, &dst).map_err(|e| MiraError::ConfigError(format!(
            "bake_loader_and_libs: copy {} → {}: {e}", src.display(), dst.display()
        )))?;
        let perm = fs::metadata(src).map_err(io_to_mira)?.permissions();
        fs::set_permissions(&dst, perm).map_err(io_to_mira)?;
        copied += 1;
        total_bytes += bytes;
    }

    info!(
        "bake_loader_and_libs: copied {} libs ({:.1} MB) into rootfs",
        copied, total_bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

/// Recursively find every `.so` (or `.so.N`) file under `dir`.
fn collect_so_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let rd = match fs::read_dir(dir) { Ok(r) => r, Err(_) => return };
    for entry in rd.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() { Ok(m) => m, Err(_) => continue };
        if meta.is_dir() {
            collect_so_files(&path, out);
        } else if meta.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".so") || name.contains(".so.") {
                    out.push(path);
                }
            }
        }
    }
}

/// Run `ldd` on `binary` and return every absolute host path it reports.
///
/// `ldd` output shapes we care about:
///   ```
///   linux-vdso.so.1 (0x00007ffe...)              ← skip, no path
///   libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x...)   ← lib + path
///   /lib64/ld-linux-x86-64.so.2 (0x...)          ← loader, no `=>`
///   libfoo.so.1 => not found                     ← skip, missing on host
///   ```
fn ldd_paths(binary: &Path) -> Result<Vec<PathBuf>, MiraError> {
    let out = Command::new("ldd").arg(binary).output().map_err(|e| {
        MiraError::ConfigError(format!(
            "spawning ldd (is glibc-bin installed?): {e}"
        ))
    })?;
    // ldd exits non-zero for ELF files it can't statically link (rare for
    // python3/.so). Don't hard-fail — surface what we got.
    if !out.status.success() {
        warn!(
            "ldd {} exited {}: {}",
            binary.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);

    let mut paths = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        let path_str = if let Some(idx) = line.find(" => ") {
            let after = &line[idx + 4..];
            if after.starts_with("not found") { continue; }
            after.split_whitespace().next().unwrap_or("")
        } else if line.starts_with('/') {
            line.split_whitespace().next().unwrap_or("")
        } else {
            continue;
        };
        if path_str.starts_with('/') {
            paths.push(PathBuf::from(path_str));
        }
    }
    Ok(paths)
}

fn dir_size(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(r)  => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let meta = match entry.metadata() {
                Ok(m)  => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

fn io_to_mira(e: io::Error) -> MiraError {
    MiraError::ConfigError(e.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn paths_resolve_under_data_dir() {
        let dir = tempdir().unwrap();
        let m = RootfsManager::new(dir.path());
        assert_eq!(m.rootfs_dir(), dir.path().join("sandbox/rootfs"));
        assert_eq!(m.cache_dir(),  dir.path().join("sandbox/cache"));
        assert!(m.python_root().ends_with("python-3.12.7"));
        assert!(!m.python_installed());
        assert!(m.list().is_empty());
    }

    #[test]
    fn ensure_dirs_creates_layout() {
        let dir = tempdir().unwrap();
        let m = RootfsManager::new(dir.path());
        m.ensure_dirs().unwrap();
        assert!(m.rootfs_dir().is_dir());
        assert!(m.cache_dir().is_dir());
    }

    #[test]
    fn placeholder_sha_blocks_install_without_override() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("blob");
        fs::write(&f, b"anything").unwrap();
        let r = verify_sha256(&f, PLACEHOLDER_SHA);
        assert!(r.is_err(), "placeholder must refuse without env override");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("MIRA_SANDBOX_DEV_INSTALL"), "msg = {msg}");
    }

    #[test]
    fn sha256_mismatch_is_reported() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("blob");
        fs::write(&f, b"hello world").unwrap();
        // "hello world" sha256
        let real = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        verify_sha256(&f, real).unwrap();
        let r = verify_sha256(&f, "0000000000000000000000000000000000000000000000000000000000000000");
        assert!(r.is_err());
    }

    #[test]
    fn arch_lookup_matches_host() {
        // On x86_64 / aarch64 hosts we should resolve an asset; on others
        // we don't.
        let asset = PYTHON.asset_for_host();
        match std::env::consts::ARCH {
            "x86_64" | "aarch64" => assert!(asset.is_some()),
            _ => assert!(asset.is_none()),
        }
    }

    #[test]
    fn list_picks_up_installed_python() {
        let dir = tempdir().unwrap();
        let m = RootfsManager::new(dir.path());
        let interp = m.python_interpreter();
        fs::create_dir_all(interp.parent().unwrap()).unwrap();
        fs::write(&interp, b"#!/bin/false\n").unwrap();
        assert!(m.python_installed());
        let listed = m.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].language, "python");
        assert!(listed[0].size_bytes > 0);
    }
}
