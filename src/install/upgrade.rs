// SPDX-License-Identifier: AGPL-3.0-or-later

//! `mira upgrade` — pull, rebuild, restart.
//!
//! Source-based for v1: the user is expected to have the source repo on
//! disk (the build-time `CARGO_MANIFEST_DIR` tells us where). When binary
//! distribution arrives, this grows a separate path that downloads the
//! release tarball instead of running cargo.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build-time source location. When a user builds from source, this points
/// at the repo they built from. After distribution, it points at a path
/// that doesn't exist on the target machine — the runtime detection below
/// catches that and tells the user to set `MIRA_SOURCE_DIR`.
const BUILD_TIME_SOURCE_DIR: &str = env!("CARGO_MANIFEST_DIR");

pub struct UpgradeOptions {
    pub branch:     Option<String>,
    pub no_restart: bool,
    pub force:      bool,
}

pub fn run_upgrade(opts: UpgradeOptions) -> Result<(), Box<dyn Error>> {
    let source = resolve_source_dir()?;
    println!("Source repo:    {}", source.display());
    println!("Current version: {}", env!("CARGO_PKG_VERSION"));
    println!();

    if is_dirty(&source)? && !opts.force {
        return Err(
            "uncommitted changes in the source tree. Commit, stash, or pass --force.".into()
        );
    }

    git_fetch(&source)?;
    let target_branch = match opts.branch.as_deref() {
        Some(b) => { git_checkout(&source, b)?; b.to_string() }
        None    => detect_current_branch(&source)?,
    };
    git_pull_ff_only(&source, "origin", &target_branch)?;

    println!();
    println!("Building (this may take a few minutes)...");
    cargo_build_release(&source)?;

    let new_version = read_new_version(&source);
    println!();
    if let Some(v) = &new_version {
        println!("Built version:  {v}");
    }

    if opts.no_restart {
        println!();
        println!("Skipping restart per --no-restart. Run `mira restart` when ready.");
        return Ok(());
    }

    let unit_installed = crate::install::supervisor_unit_path()
        .map(|p| p.exists())
        .unwrap_or(false);
    if !unit_installed {
        println!();
        println!("No service unit installed — restart MIRA manually to pick up the new build.");
        return Ok(());
    }

    println!();
    println!("Restarting service...");
    crate::install::run_restart()?;
    println!();
    println!("✓ upgrade complete");
    if let Some(v) = new_version {
        println!("  running version: {v}");
    }
    Ok(())
}

/// True iff a MIRA source repo can be located (via either
/// `MIRA_SOURCE_DIR` or the build-time `CARGO_MANIFEST_DIR`). Used by
/// the CLI to default `mira upgrade` to the source path on dev
/// installs and the binary path on tarball installs.
pub fn source_dir_reachable() -> bool {
    resolve_source_dir().is_ok()
}

/// Find the MIRA source repo, preferring an explicit override. Without
/// this, `cargo build` would either run in the wrong directory or fail.
fn resolve_source_dir() -> Result<PathBuf, Box<dyn Error>> {
    if let Ok(d) = std::env::var("MIRA_SOURCE_DIR") {
        let p = PathBuf::from(d);
        if is_mira_source(&p) { return Ok(p); }
        return Err(format!(
            "MIRA_SOURCE_DIR={} doesn't look like a MIRA source repo \
             (no Cargo.toml + src/main.rs).",
            p.display()
        ).into());
    }
    let p = PathBuf::from(BUILD_TIME_SOURCE_DIR);
    if is_mira_source(&p) { return Ok(p); }
    Err(format!(
        "MIRA source repo not found. Build-time path was {}, but it doesn't \
         exist or isn't a MIRA checkout. Set MIRA_SOURCE_DIR to the absolute \
         path of your `mira` clone.",
        p.display()
    ).into())
}

fn is_mira_source(p: &Path) -> bool {
    p.join("Cargo.toml").is_file() && p.join("src/main.rs").is_file()
}

fn is_dirty(repo: &Path) -> Result<bool, Box<dyn Error>> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo)
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ).into());
    }
    Ok(!out.stdout.is_empty())
}

fn git_fetch(repo: &Path) -> Result<(), Box<dyn Error>> {
    let s = Command::new("git").args(["fetch", "--quiet"]).current_dir(repo).status()?;
    if !s.success() { return Err("git fetch failed".into()); }
    println!("✓ git fetch");
    Ok(())
}

fn git_pull_ff_only(repo: &Path, remote: &str, branch: &str) -> Result<(), Box<dyn Error>> {
    let s = Command::new("git")
        .args(["pull", "--ff-only", "--quiet", remote, branch])
        .current_dir(repo)
        .status()?;
    if !s.success() {
        return Err(format!(
            "git pull --ff-only {remote} {branch} failed. Either {remote}/{branch} \
             doesn't exist, or your local branch has diverged — rebase or merge \
             manually, then re-run."
        ).into());
    }
    println!("✓ git pull --ff-only {remote} {branch}");
    Ok(())
}

/// Read the current branch name. Used when the user doesn't pass
/// `--branch` so the upgrade pulls from `origin/<current-branch>` instead
/// of relying on tracking config the user may not have set.
fn detect_current_branch(repo: &Path) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo)
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ).into());
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name == "HEAD" {
        return Err("HEAD is detached — pass --branch <name> to pick a target.".into());
    }
    Ok(name)
}

fn git_checkout(repo: &Path, branch: &str) -> Result<(), Box<dyn Error>> {
    let s = Command::new("git").args(["checkout", branch]).current_dir(repo).status()?;
    if !s.success() { return Err(format!("git checkout {branch} failed").into()); }
    println!("✓ git checkout {branch}");
    Ok(())
}

/// Run `cargo build --release` in the source dir, inheriting the user's
/// environment so `CARGO_TARGET_DIR` (and any flags) flow through. Cargo
/// streams its own progress output to the inherited stdout/stderr.
fn cargo_build_release(repo: &Path) -> Result<(), Box<dyn Error>> {
    let s = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(repo)
        .status()?;
    if !s.success() {
        return Err("cargo build --release failed — old binary is still on disk and the service is still running.".into());
    }
    Ok(())
}

/// Best-effort read of the newly-built `version =` line from Cargo.toml.
/// Used only for the post-build summary, so a parse failure isn't fatal.
fn read_new_version(repo: &Path) -> Option<String> {
    let toml = std::fs::read_to_string(repo.join("Cargo.toml")).ok()?;
    for line in toml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("version") {
            // matches `version = "X.Y.Z"`
            if let Some(eq) = rest.find('=') {
                let val = rest[eq + 1..].trim().trim_matches('"');
                if !val.is_empty() { return Some(val.to_string()); }
            }
        }
    }
    None
}
