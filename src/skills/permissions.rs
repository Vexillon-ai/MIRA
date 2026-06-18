// SPDX-License-Identifier: AGPL-3.0-or-later

//! Skill permission policy checks.
//!
//! Two layers of defence make the Skill permission model trustworthy:
//!
//! 1. **These checks** answer "does the manifest permit this action?"
//!    They run BEFORE the tool executes. Cheap, deterministic, easy to test
//!    in isolation, no platform deps.
//! 2. **The sandbox** (Linux namespaces + seccomp) enforces the same
//!    constraints at the syscall level. A Skill that lies about needing
//!    network access still has its connections refused by the network
//!    namespace; a Skill that bypasses these checks still can't write
//!    outside its bind-mounted writable dirs.
//!
//! Both layers reach the same verdict for honest Skills. The point is that
//! a *malicious* Skill that finds a way around one layer is still caught
//! by the other.
//!
//! What's *not* in this module:
//! - Actual execution of tool calls (slice A3).
//! - HTTP proxy that filters network requests by URL (deferred — we map
//!   "any allowlist entries" to "network on" for now; URL-level filtering
//!   needs a sidecar proxy).
//! - LLM-spend tracking (Phase D, the policy engine).

use std::path::{Component, Path, PathBuf};

use crate::config::expand_path;
use crate::sandbox::limits::ResourceLimits;
use crate::skills::manifest::Permissions;

/// What kind of access a tool is asking for. Manifest entries are
/// `read:`, `write:`, or `read+write:` — `Read` matches both reading
/// modes; `Write` matches both writing modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode { Read, Write }

/// A check that didn't pass. Carries a human-readable reason that's safe
/// to surface to users (and to the LLM, which sometimes sees these as
/// tool errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied(pub String);

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Denied {}

// ── Filesystem ────────────────────────────────────────────────────────────

/// Is the Skill allowed to access `requested` in the given mode?
///
/// Manifest entries look like `read:/abs/path`, `write:~/relative/path`,
/// or `read+write:/abs/path`. Tilde expands to `$HOME`. Access is
/// recursive — granting access to `/a/b/` permits `/a/b/c/file.txt` but
/// not `/a/c/file.txt`.
pub fn check_filesystem(
    perms: &Permissions,
    requested: &Path,
    mode: AccessMode,
) -> Result<(), Denied> {
    let canonical = canonicalise_or_lexical(requested);

    for entry in &perms.filesystem {
        let Some((entry_mode, raw_path)) = entry.split_once(':') else { continue; };
        if !mode_matches(entry_mode, mode) { continue; }

        let allowed = canonicalise_or_lexical(&expand_path(raw_path.trim()));
        if path_under(&canonical, &allowed) {
            return Ok(());
        }
    }

    Err(Denied(format!(
        "filesystem access denied: {mode:?} {} is not covered by skill permissions",
        requested.display(),
    )))
}

/// `read` matches the requested Read; `write` matches Write; `read+write`
/// matches either.
fn mode_matches(entry_mode: &str, requested: AccessMode) -> bool {
    match (entry_mode, requested) {
        ("read",       AccessMode::Read)  => true,
        ("write",      AccessMode::Write) => true,
        ("read+write", _)                 => true,
        _                                 => false,
    }
}

/// Returns true when `child` is `parent` or a descendant. Uses a
/// component-by-component prefix check on lexically-normalised paths so
/// `..` segments can't escape the granted root.
fn path_under(child: &Path, parent: &Path) -> bool {
    let child_norm = lexical_normalise(child);
    let parent_norm = lexical_normalise(parent);
    let cs: Vec<_> = child_norm.components().collect();
    let ps: Vec<_> = parent_norm.components().collect();
    cs.len() >= ps.len() && cs.iter().take(ps.len()).eq(ps.iter())
}

/// Try `canonicalize`; fall back to lexical normalisation when the path
/// doesn't yet exist (we still want to permit the *intent* of a future
/// write to a not-yet-created file).
fn canonicalise_or_lexical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| lexical_normalise(p))
}

/// Drops `.` segments and resolves `..` against earlier segments. This
/// is a string-level normalisation — it doesn't follow symlinks. Good
/// enough for the policy check; the sandbox does the real enforcement.
fn lexical_normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir         => {}
            Component::ParentDir      => { out.pop(); }
            other                     => out.push(other.as_os_str()),
        }
    }
    out
}

// ── Network ──────────────────────────────────────────────────────────────

/// Is the Skill allowed to fetch `url`?
///
/// Manifest entries are URL prefixes — `https://duckduckgo.com` allows
/// every URL with that prefix; `https://*.wikipedia.org` allows any
/// subdomain (single label only — `en.m.wikipedia.org` does *not* match
/// `*.wikipedia.org`, which would let attackers register
/// `evil.wikipedia.org`-style domains and slip in. We're conservative).
pub fn check_network_egress(perms: &Permissions, url: &str) -> Result<(), Denied> {
    if perms.network_egress.is_empty() {
        return Err(Denied(format!(
            "network egress denied: skill declares no network_egress entries",
        )));
    }
    let req_host = parse_host(url).ok_or_else(|| Denied(format!(
        "network egress denied: {url:?} is not a parsable http(s) URL",
    )))?;
    let req_scheme = if url.starts_with("https://") { "https" } else { "http" };

    for entry in &perms.network_egress {
        if matches_egress_entry(entry, req_scheme, &req_host, url) {
            return Ok(());
        }
    }

    Err(Denied(format!(
        "network egress denied: {url:?} not covered by skill's network_egress allowlist",
    )))
}

fn parse_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let host_and_path = rest;
    let host = host_and_path.split('/').next()?.split(':').next()?;
    if host.is_empty() { None } else { Some(host.to_lowercase()) }
}

fn matches_egress_entry(entry: &str, req_scheme: &str, req_host: &str, full_url: &str) -> bool {
    // Wildcard form: scheme://*.host  (single-label subdomain match)
    let lc_entry = entry.to_lowercase();
    if let Some(rest) = lc_entry.strip_prefix(&format!("{req_scheme}://*.")) {
        // `rest` is the host suffix the user wants to allow under, plus
        // optional path. Take everything up to the next '/' as the host
        // suffix.
        let parent_host = rest.split('/').next().unwrap_or("");
        if parent_host.is_empty() { return false; }
        // Match exactly one DNS label: `<label>.<parent_host>`. Multiple
        // labels would let `evil.legit.example.com`-style domains through,
        // which is not what users mean.
        if let Some(prefix) = req_host.strip_suffix(&format!(".{parent_host}")) {
            return !prefix.is_empty() && !prefix.contains('.');
        }
        return false;
    }

    // Exact-prefix form: must match scheme + host + (optionally) path prefix.
    full_url.to_lowercase().starts_with(&lc_entry)
}

// ── Subprocess ───────────────────────────────────────────────────────────

/// Is the Skill allowed to spawn `binary`?
///
/// `subprocess: false` denies everything. `subprocess: true` plus an
/// empty `subprocess_allowlist` allows anything the sandbox permits.
/// A non-empty allowlist requires an exact path match.
pub fn check_subprocess(perms: &Permissions, binary: &Path) -> Result<(), Denied> {
    if !perms.subprocess {
        return Err(Denied("subprocess denied: skill declares subprocess = false".into()));
    }
    if perms.subprocess_allowlist.is_empty() {
        return Ok(());
    }
    let bin_str = binary.to_string_lossy();
    if perms.subprocess_allowlist.iter().any(|a| a == bin_str.as_ref()) {
        Ok(())
    } else {
        Err(Denied(format!(
            "subprocess denied: {binary:?} not in subprocess_allowlist",
        )))
    }
}

// ── Secrets ──────────────────────────────────────────────────────────────

pub fn check_secret_access(perms: &Permissions, key: &str) -> Result<(), Denied> {
    if perms.secrets.iter().any(|s| s.key() == key) {
        Ok(())
    } else {
        Err(Denied(format!(
            "secret denied: {key:?} not in skill's declared secrets allowlist",
        )))
    }
}

// ── LLM provider ─────────────────────────────────────────────────────────

pub fn check_llm_provider(perms: &Permissions, alias: &str) -> Result<(), Denied> {
    if perms.llm_providers.is_empty() {
        return Err(Denied(
            "llm provider denied: skill declares no llm_providers entries".into(),
        ));
    }
    if perms.llm_providers.iter().any(|p| p == alias) {
        Ok(())
    } else {
        Err(Denied(format!(
            "llm provider denied: {alias:?} not in skill's declared llm_providers",
        )))
    }
}

// ── Sandbox config derivation ────────────────────────────────────────────

/// Translate manifest permissions into a `ResourceLimits` for an
/// executable tool run.
///
/// Defence in depth: even if these checks were skipped, the sandbox
/// would still enforce the network-namespace and nproc constraints.
///
/// **Known gap:** network egress is currently binary — empty allowlist
/// disables network entirely; non-empty allowlist enables network with
/// no URL-level filtering. URL filtering needs a sidecar HTTP proxy that
/// reads the allowlist; tracked as a follow-up in
/// `design-docs/skills-and-agents.md` Phase D / open questions.
pub fn build_sandbox_limits(
    perms: &Permissions,
    defaults: ResourceLimits,
) -> ResourceLimits {
    let mut limits = defaults;

    // Network: any allowlist entries → leave network on (broad, will be
    // narrowed by the future proxy). Empty allowlist → disable_network.
    limits.disable_network = perms.network_egress.is_empty();

    // Subprocess: false → nproc=1 so the child can't fork at all.
    // true → keep the caller-supplied default. The seccomp filter
    // already rejects fork/clone for nproc=1 because RLIMIT_NPROC is
    // checked at the kernel level.
    if !perms.subprocess {
        limits.nproc = 1;
    }

    // Filesystem bind mounts are intentionally *not* set here. The
    // existing sandbox requires extra_writable_mounts targets to live
    // under /tmp; mapping skill manifest paths into the sandbox is a
    // separate design (path translation table + env var so the tool
    // sees the manifest paths). Tracked for slice A3 / executable-tool
    // wiring.
    limits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::manifest::Permissions;

    fn perms_with_filesystem(entries: &[&str]) -> Permissions {
        Permissions {
            filesystem: entries.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    fn perms_with_network(entries: &[&str]) -> Permissions {
        Permissions {
            network_egress: entries.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    // ── filesystem ──

    #[test]
    fn filesystem_grants_read_under_explicit_dir() {
        let perms = perms_with_filesystem(&["read:/srv/data"]);
        check_filesystem(&perms, Path::new("/srv/data/file.txt"), AccessMode::Read).unwrap();
    }

    #[test]
    fn filesystem_denies_write_when_only_read_granted() {
        let perms = perms_with_filesystem(&["read:/srv/data"]);
        let r = check_filesystem(&perms, Path::new("/srv/data/file.txt"), AccessMode::Write);
        assert!(r.is_err());
    }

    #[test]
    fn filesystem_read_plus_write_grants_both() {
        let perms = perms_with_filesystem(&["read+write:/srv/data"]);
        check_filesystem(&perms, Path::new("/srv/data/x"), AccessMode::Read).unwrap();
        check_filesystem(&perms, Path::new("/srv/data/x"), AccessMode::Write).unwrap();
    }

    #[test]
    fn filesystem_denies_outside_granted_subtree() {
        let perms = perms_with_filesystem(&["read:/srv/data"]);
        let r = check_filesystem(&perms, Path::new("/etc/passwd"), AccessMode::Read);
        assert!(r.is_err());
    }

    #[test]
    fn filesystem_dotdot_does_not_escape_grant() {
        // /srv/data/../../etc normalises to /etc, which is outside the grant.
        let perms = perms_with_filesystem(&["read:/srv/data"]);
        let r = check_filesystem(
            &perms,
            Path::new("/srv/data/../../etc/passwd"),
            AccessMode::Read,
        );
        assert!(r.is_err(), "dotdot must not escape the grant");
    }

    #[test]
    fn filesystem_empty_grants_denies_all() {
        let perms = Permissions::default();
        let r = check_filesystem(&perms, Path::new("/srv/data"), AccessMode::Read);
        assert!(r.is_err());
    }

    // ── network ──

    #[test]
    fn network_exact_prefix_matches() {
        let p = perms_with_network(&["https://duckduckgo.com"]);
        check_network_egress(&p, "https://duckduckgo.com/").unwrap();
        check_network_egress(&p, "https://duckduckgo.com/?q=hi").unwrap();
    }

    #[test]
    fn network_wildcard_matches_one_label_only() {
        let p = perms_with_network(&["https://*.wikipedia.org"]);
        check_network_egress(&p, "https://en.wikipedia.org/wiki/Foo").unwrap();
        // Two labels deep — must not match the conservative one-label rule.
        let r = check_network_egress(&p, "https://en.m.wikipedia.org/wiki/Foo");
        assert!(r.is_err(), "two-label subdomain must not match a single-* wildcard");
        // Apex doesn't match either (would require explicit entry).
        let r2 = check_network_egress(&p, "https://wikipedia.org/wiki/Foo");
        assert!(r2.is_err());
    }

    #[test]
    fn network_wildcard_rejects_lookalike_suffix() {
        // `evil-wikipedia.org` ends with the parent host string but isn't
        // a real subdomain. The match must use a leading dot to anchor.
        let p = perms_with_network(&["https://*.example.com"]);
        let r = check_network_egress(&p, "https://evilexample.com/");
        assert!(r.is_err());
        let r2 = check_network_egress(&p, "https://attacker-example.com/");
        assert!(r2.is_err());
    }

    #[test]
    fn network_empty_allowlist_denies_everything() {
        let p = Permissions::default();
        let r = check_network_egress(&p, "https://example.com/");
        assert!(r.is_err());
    }

    #[test]
    fn network_scheme_must_match() {
        let p = perms_with_network(&["https://api.example.com"]);
        let r = check_network_egress(&p, "http://api.example.com/");
        assert!(r.is_err(), "https-only allowlist must not permit http");
    }

    // ── subprocess ──

    #[test]
    fn subprocess_false_denies_all() {
        let p = Permissions::default();
        let r = check_subprocess(&p, Path::new("/usr/bin/git"));
        assert!(r.is_err());
    }

    #[test]
    fn subprocess_true_with_empty_allowlist_permits_anything() {
        let p = Permissions { subprocess: true, ..Default::default() };
        check_subprocess(&p, Path::new("/usr/bin/git")).unwrap();
        check_subprocess(&p, Path::new("/usr/bin/curl")).unwrap();
    }

    #[test]
    fn subprocess_allowlist_requires_exact_match() {
        let p = Permissions {
            subprocess: true,
            subprocess_allowlist: vec!["/usr/bin/git".to_string()],
            ..Default::default()
        };
        check_subprocess(&p, Path::new("/usr/bin/git")).unwrap();
        let r = check_subprocess(&p, Path::new("/usr/bin/curl"));
        assert!(r.is_err());
    }

    // ── secrets ──

    #[test]
    fn secret_access_requires_explicit_grant() {
        let p = Permissions { secrets: vec!["FOO".into()], ..Default::default() };
        check_secret_access(&p, "FOO").unwrap();
        assert!(check_secret_access(&p, "BAR").is_err());
    }

    // ── llm provider ──

    #[test]
    fn llm_provider_requires_explicit_grant() {
        let p = Permissions { llm_providers: vec!["primary".into()], ..Default::default() };
        check_llm_provider(&p, "primary").unwrap();
        assert!(check_llm_provider(&p, "secondary").is_err());
    }

    #[test]
    fn llm_provider_empty_denies_everything() {
        let p = Permissions::default();
        assert!(check_llm_provider(&p, "primary").is_err());
    }

    // ── sandbox limits derivation ──

    #[test]
    fn sandbox_limits_disable_network_when_no_egress() {
        let p = Permissions::default();
        let limits = build_sandbox_limits(&p, ResourceLimits::default());
        assert!(limits.disable_network, "no network_egress entries → disable_network=true");
    }

    #[test]
    fn sandbox_limits_enable_network_when_any_egress() {
        let p = perms_with_network(&["https://example.com"]);
        let limits = build_sandbox_limits(&p, ResourceLimits::default());
        assert!(!limits.disable_network, "any network_egress → network on (broad until proxy lands)");
    }

    #[test]
    fn sandbox_limits_pin_nproc_to_1_when_subprocess_false() {
        let p = Permissions::default();
        let limits = build_sandbox_limits(&p, ResourceLimits::default());
        assert_eq!(limits.nproc, 1);
    }

    #[test]
    fn sandbox_limits_keep_default_nproc_when_subprocess_true() {
        let p = Permissions { subprocess: true, ..Default::default() };
        let defaults = ResourceLimits::default();
        let expected = defaults.nproc;
        let limits = build_sandbox_limits(&p, defaults);
        assert_eq!(limits.nproc, expected);
    }
}
