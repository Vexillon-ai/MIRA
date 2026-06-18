// SPDX-License-Identifier: AGPL-3.0-or-later

//! Install / uninstall engine.
//!
//! supports the **`mcp_server`** component: translate it into a real
//! `mcp_servers` row (which MIRA's MCP host then spawns), recording every
//! created resource in the package's provisioning **ledger**. Uninstall (and a
//! cancelled install) reverse that ledger. Other component kinds are rejected
//! with a clear "not supported in this MIRA yet" message.
//!
//! The engine operates on the stores only; the caller reloads the MCP registry
//! (an async, no-restart hot-reload) after install/uninstall.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::channel_accounts::ChannelAccountStore;
use crate::mcp::{McpServerStore, NewMcpServer};
use crate::skills::secrets::{Scope, SecretsStore};

use super::bundle::ParsedBundle;
use super::manifest::{Component, ComponentKind, PackageManifest, Runtime};
use super::store::{Ledger, LedgerEntry, NewInstall, PackageStore};
use super::verify::TrustLevel;

// What an install produced. `mcp_server_ids` lets the caller hot-reload the
// MCP registry so the new tools come live without a restart.
pub struct InstallOutcome {
    pub package: super::store::InstalledPackage,
    pub mcp_server_ids: Vec<String>,
    // Non-fatal limitations surfaced to the admin (e.g. best-effort Tier-B egress).
    pub warnings: Vec<String>,
}

// The `spec` shape of an `mcp_server` component.
#[derive(Debug, Clone, Deserialize, Default)]
struct McpServerSpec {
    // Server name (the `mcp__<name>__<tool>` prefix). Defaults to the last
    // dotted segment of the package id.
    name: Option<String>,
    #[serde(default = "default_stdio")]
    transport: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    url: Option<String>,
    // Container image ref — required when the component's `runtime` is
    // `container`; ignored for native (the entrypoint is the MCP server).
    image: Option<String>,
    #[serde(default)]
    sampling_enabled: bool,
}

fn default_stdio() -> String {
    "stdio".to_string()
}

// Derive a default server name from a package id (last dotted segment).
fn default_name(pkg_id: &str) -> String {
    pkg_id.rsplit('.').next().unwrap_or(pkg_id).to_string()
}

// Build the `NewMcpServer` for an `mcp_server` component, merging the
// admin-supplied `config` over the manifest's declared `env` (config wins).
fn build_mcp_server(
    manifest: &PackageManifest,
    component: &Component,
    config: &HashMap<String, String>,
) -> Result<NewMcpServer, String> {
    let spec: McpServerSpec = serde_json::from_value(component.spec.clone())
        .map_err(|e| format!("invalid mcp_server spec: {e}"))?;

    // A container component's MCP server is the image's entrypoint, so it needs
    // no `command` (the spawn becomes `docker run … <image>`); a native stdio
    // server still does.
    let is_container = component.runtime == Runtime::Container;
    if spec.transport == "stdio" && !is_container && spec.command.as_deref().unwrap_or("").is_empty() {
        return Err("mcp_server (stdio) requires a `command`".into());
    }
    if spec.transport == "http" && spec.url.as_deref().unwrap_or("").is_empty() {
        return Err("mcp_server (http) requires a `url`".into());
    }

    // Least-privilege granting: the admin's config may only FILL slots the
    // package declared — a key in the component's `env` or its declared
    // `secrets`. Anything else is an attempt to inject undeclared values, so we
    // refuse rather than silently widen what the component receives.
    let declared: std::collections::HashSet<&str> = spec
        .env
        .keys()
        .map(String::as_str)
        .chain(component.capabilities.secrets.iter().map(String::as_str))
        .collect();
    let undeclared: Vec<&str> =
        config.keys().map(String::as_str).filter(|k| !declared.contains(k)).collect();
    if !undeclared.is_empty() {
        return Err(format!(
            "config provides keys the package didn't declare (not in its env or secrets): {}",
            undeclared.join(", ")
        ));
    }

    // env = declared env, then admin config overlaid (declared keys only).
    let mut env = spec.env.clone();
    for (k, v) in config {
        env.insert(k.clone(), v.clone());
    }

    Ok(NewMcpServer {
        name: spec.name.unwrap_or_else(|| default_name(&manifest.id)),
        transport: spec.transport,
        command: spec.command,
        args: spec.args,
        env,
        url: spec.url,
        enabled: true,
        sampling_enabled: spec.sampling_enabled,
    })
}

// Capability × trust install gate (docs "Capability & sandbox model"). A
// tampered/invalid signature is never installable. An unverified package that
// requests sensitive capabilities needs the admin's explicit acknowledgement
// (`allow_untrusted`); the dangerous secrets + broad-egress combination is
// called out specifically. Verified packages pass.
pub fn gate_install(
    trust: &TrustLevel,
    manifest: &PackageManifest,
    allow_untrusted: bool,
) -> Result<(), String> {
    if matches!(trust, TrustLevel::Invalid { .. }) {
        return Err("package signature is invalid (tampered or corrupt) — refusing to install".into());
    }
    if trust.is_verified() || allow_untrusted {
        return Ok(());
    }
    let wants_secrets = manifest.components.iter().any(|c| !c.capabilities.secrets.is_empty());
    let broad_egress = manifest
        .components
        .iter()
        .any(|c| c.capabilities.network_egress.iter().any(|h| h.contains('*')));
    let why = if wants_secrets && broad_egress {
        "it holds secrets and can reach arbitrary hosts (exfiltration risk)"
    } else if wants_secrets {
        "it requests access to secrets"
    } else {
        "it is not from a verified publisher"
    };
    Err(format!("refusing to install an unverified package: {why}. Confirm to install anyway."))
}

// Install a parsed, verified package. provisions each `mcp_server`
// component as an `mcp_servers` row owned by the installing admin, records the
// ledger, and writes the package record. On a component failure, the
// already-provisioned rows are rolled back (teardown of the partial ledger).
pub fn install_package(
    parsed: &ParsedBundle,
    trust: &TrustLevel,
    admin_id: &str,
    config: &HashMap<String, String>,
    packages_dir: &Path,
    mcp_store: &McpServerStore,
    pkg_store: &PackageStore,
) -> Result<InstallOutcome, String> {
    let m = &parsed.manifest;
    let mut ledger: Ledger = Vec::new();
    let mut mcp_ids: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 1. Extract payload files to <packages_dir>/<id>/ (path-safety already
    //  enforced by parse_bundle). Record the dir so teardown removes it.
    let install_dir = packages_dir.join(&m.id);
    if let Err(e) = extract_payload(parsed, &install_dir) {
        let _ = fs::remove_dir_all(&install_dir);
        return Err(format!("extract payload: {e}"));
    }
    ledger.push(LedgerEntry::Files { dir: install_dir.to_string_lossy().to_string() });

    // 2. Provision each component.
    for component in &m.components {
        match component.kind {
            ComponentKind::McpServer => {
                let mut new = match build_mcp_server(m, component, config) {
                    Ok(n) => n,
                    Err(e) => {
                        rollback(&ledger, mcp_store);
                        return Err(e);
                    }
                };
                // Native: resolve relative command/args to the extracted files,
                // then wrap the spawn in the confinement launcher (network
                // isolation + read-only-root fs scoping). Container: replace the
                // spawn with `docker run … <image>` (higher-isolation tier).
                match component.runtime {
                    Runtime::Native => {
                        rewrite_paths(&mut new, &install_dir);
                        confine_command(&mut new, component, &install_dir, config);
                    }
                    Runtime::Container => {
                        if let Err(e) = containerize_command(&mut new, component, &install_dir, config, &mut warnings) {
                            rollback(&ledger, mcp_store);
                            return Err(e);
                        }
                    }
                }
                match mcp_store.create(admin_id, new) {
                    Ok(row) => {
                        ledger.push(LedgerEntry::McpServer { id: row.id.clone() });
                        mcp_ids.push(row.id);
                    }
                    Err(e) => {
                        rollback(&ledger, mcp_store);
                        return Err(format!("create mcp server: {e}"));
                    }
                }
            }
            other => {
                rollback(&ledger, mcp_store);
                return Err(format!(
                    "component type {other:?} is not installable in this MIRA \
                     (only mcp_server components are supported)"
                ));
            }
        }
    }

    let manifest_json = serde_json::to_value(m).unwrap_or(serde_json::Value::Null);
    let package = pkg_store
        .upsert(NewInstall {
            id: m.id.clone(),
            version: m.version.to_string(),
            name: m.name.clone(),
            trust: trust.label().to_string(),
            installed_by: admin_id.to_string(),
            ledger: ledger.clone(),
            manifest: manifest_json,
            config: serde_json::json!({}), // one-shot mcp_server installs carry no wizard config
        })
        .map_err(|e| {
            // Couldn't record the install — undo the whole provisioning.
            rollback(&ledger, mcp_store);
            format!("record install: {e}")
        })?;

    Ok(InstallOutcome { package, mcp_server_ids: mcp_ids, warnings })
}

// Write a bundle's payload files under `install_dir`, stripping the top-level
// (id) directory and skipping the manifest itself.
pub(crate) fn extract_payload(parsed: &ParsedBundle, install_dir: &Path) -> std::io::Result<()> {
    for (path, bytes) in &parsed.files {
        let rel = path.strip_prefix(&parsed.top_dir).unwrap_or(path);
        if rel.as_os_str().is_empty() || rel == Path::new(super::bundle::MANIFEST_NAME) {
            continue;
        }
        let dest = install_dir.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, bytes)?;
    }
    Ok(())
}

// Make a relative `command`/`args` that names an extracted file resolve to its
// absolute path under the install dir, so MIRA can spawn it from any cwd.
fn rewrite_paths(new: &mut NewMcpServer, install_dir: &Path) {
    let abs = |s: &str| {
        let cand = install_dir.join(s);
        cand.exists().then(|| cand.to_string_lossy().to_string())
    };
    if let Some(cmd) = new.command.as_deref() {
        if let Some(a) = abs(cmd) {
            new.command = Some(a);
        }
    }
    for arg in new.args.iter_mut() {
        if let Some(a) = abs(arg) {
            *arg = a;
        }
    }
}

// Wrap a stdio server's command in the `mira pkg-exec` confinement launcher.
// No-op for http servers (no command) or when the running binary can't be
// located. Unix only.
// // Builds the confinement *policy* from the component's capabilities (the
// launcher is the *mechanism*):
// - **no_network** when the component declares no egress.
// - **fs_scope** (Linux): the host root is read-only, with writable holes for
// the component's declared `filesystem` paths plus a private per-plugin data
// dir (`<install_dir>/_data`, also exported as `$HOME` + `MIRA_PLUGIN_DATA_DIR`
// so tools that expect a writable home work). MIRA's own secrets — the config
// dir (provider keys), the credential/personal DBs — and the user's
// `~/.ssh` `~/.aws` `~/.gnupg` are masked so a plugin can't read them.
#[cfg(unix)]
fn confine_command(
    new: &mut NewMcpServer,
    component: &Component,
    install_dir: &Path,
    config: &HashMap<String, String>,
) {
    let Some(cmd) = new.command.clone() else { return };
    let Ok(exe) = std::env::current_exe() else { return };

    // Native-tier egress allowlist: declared hosts, template-expanded + reduced to
    // bare hostnames. When non-empty the launcher filters egress to these (via the
    // privileged helper); when empty, the component runs with no network.
    let egress: Vec<String> = component
        .capabilities
        .network_egress
        .iter()
        .filter_map(|h| egress_host(h, config))
        .collect();
    let no_network = egress.is_empty();
    let home = std::env::var("HOME").unwrap_or_default();

    // Private per-plugin data dir — the default writable area + a writable HOME.
    let data_dir = install_dir.join("_data");
    let _ = fs::create_dir_all(&data_dir);
    let data_dir_s = data_dir.to_string_lossy().to_string();
    new.env.entry("HOME".to_string()).or_insert_with(|| data_dir_s.clone());
    new.env.insert("MIRA_PLUGIN_DATA_DIR".to_string(), data_dir_s.clone());

    // Writable holes: declared filesystem capabilities (templated) + the data dir.
    let mut rw_paths: Vec<String> = component
        .capabilities
        .filesystem
        .iter()
        .map(|p| expand_path(p, &home, config))
        .collect();
    rw_paths.push(data_dir_s);
    // The bind in the launcher needs each writable hole to exist.
    for p in &rw_paths {
        let _ = fs::create_dir_all(p);
    }

    let mask_paths = secret_mask_paths(install_dir, &home);

    let fs_scope = cfg!(target_os = "linux");
    let spec = super::launcher::ConfineSpec {
        fsize_mb: Some(1024),
        no_network,
        fs_scope,
        rw_paths,
        mask_paths,
        egress,
    };
    let (c, a) = super::launcher::wrap(&exe.to_string_lossy(), &cmd, &new.args, &spec);
    new.command = Some(c);
    new.args = a;
}

// Replace a container component's spawn with a hardened `docker run … <image>`
// invocation MIRA's MCP host speaks stdio to. Pulls the image at install (fails
// fast + guides if the engine/daemon is unavailable), maps declared capabilities
// to engine flags, and forwards secrets value-less so they stay out of argv.
fn containerize_command(
    new: &mut NewMcpServer,
    component: &Component,
    install_dir: &Path,
    config: &HashMap<String, String>,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    use super::container::{self, ContainerSpec, NetworkMode};

    let spec: McpServerSpec = serde_json::from_value(component.spec.clone())
        .map_err(|e| format!("invalid mcp_server spec: {e}"))?;
    let image = spec
        .image
        .filter(|s| !s.trim().is_empty())
        .ok_or("runtime: container requires a non-empty `image` in the mcp_server spec")?;
    if new.transport != "stdio" {
        return Err("runtime: container currently supports stdio mcp_server only".into());
    }
    let engine = container::detect_engine().ok_or(
        "runtime: container needs a container engine (docker or podman) on PATH — \
         install one, or repackage the component as runtime: native",
    )?;
    container::pull(&engine, &image).map_err(|e| {
        format!("could not pull image `{image}` with {engine}: {e} (is the container daemon running?)")
    })?;

    // Private per-plugin data dir (host) → /data (container), writable.
    let data_dir = install_dir.join("_data");
    let _ = fs::create_dir_all(&data_dir);
    let home = std::env::var("HOME").unwrap_or_default();

    let mut volumes: Vec<(String, String)> = vec![(data_dir.to_string_lossy().to_string(), "/data".to_string())];
    for p in &component.capabilities.filesystem {
        let host = expand_path(p, &home, config);
        let _ = fs::create_dir_all(&host);
        volumes.push((host.clone(), host)); // mount declared paths at the same path inside
    }
    new.env.insert("MIRA_PLUGIN_DATA_DIR".to_string(), "/data".to_string());

    let mut plugin = ContainerSpec::new(image.clone(), NetworkMode::None);
    plugin.volumes = volumes;
    // Forward every env the server was given (declared env + filled secrets) by
    // name only — the value rides in the engine process's environment.
    plugin.env_keys = new.env.keys().cloned().collect();
    plugin.listen_port = component.capabilities.listen_port;

    let egress = &component.capabilities.network_egress;
    if egress.is_empty() {
        // No declared egress → fully offline.
        plugin.network = NetworkMode::None;
        new.command = Some(engine);
        new.args = container::build_run_args(&plugin);
        return Ok(());
    }

    // Declared egress → a per-host allowlist via the `mira ctr-run` wrapper.
    let allow: Vec<String> = egress.iter().filter_map(|h| egress_host(h, config)).collect();
    if allow.is_empty() {
        return Err("runtime: container declares network_egress but no resolvable hostnames".into());
    }
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate mira binary for ctr-run: {e}"))?;

    // Tier A (NET_ADMIN nft sidecar — proper) when available, else Tier B
    // (best-effort HTTP/S proxy allowlist). `MIRA_FORCE_EGRESS_TIER` overrides for
    // testing/ops (`proxy`|`b` → Tier B; `egress`|`a` → Tier A).
    let forced = std::env::var("MIRA_FORCE_EGRESS_TIER").unwrap_or_default().to_lowercase();
    let use_proxy = match forced.as_str() {
        "proxy" | "b" => true,
        "egress" | "a" | "netadmin" => false,
        _ => !container::supports_net_admin(&engine),
    };

    let mut args = vec![
        "ctr-run".to_string(),
        "--mode".into(), if use_proxy { "proxy".into() } else { "egress".to_string() },
        "--engine".into(), engine.clone(),
        "--upstream".into(), "1.1.1.1".into(),
        "--image".into(), image.clone(),
        "--memory".into(), plugin.memory.clone(),
        "--pids".into(), plugin.pids_limit.to_string(),
    ];
    for h in &allow {
        args.push("--allow".into());
        args.push(h.clone());
    }
    for (hp, cp) in &plugin.volumes {
        args.push("--volume".into());
        args.push(format!("{hp}:{cp}"));
    }
    for k in &plugin.env_keys {
        args.push("--env".into());
        args.push(k.clone());
    }

    if use_proxy {
        // Fail fast if the proxy image can't be built, and surface the limitation.
        container::ensure_proxy_image(&engine)?;
        warnings.push(format!(
            "egress for `{image}` is best-effort (HTTP/S allowlist proxy) — the container \
             engine could not grant CAP_NET_ADMIN for full per-host filtering. Non-HTTP egress \
             is blocked; HTTP/S is limited to: {}.",
            allow.join(", ")
        ));
        eprintln!("mira: container egress for `{image}` using Tier-B HTTP/S proxy allowlist (no NET_ADMIN)");
    }
    new.command = Some(exe.to_string_lossy().to_string());
    new.args = args;
    Ok(())
}

// Reduce a declared `network_egress` entry to a bare hostname for the allowlist:
// expand `${VAR}` from config, strip scheme / path / port, and turn a `*.dom`
// suffix wildcard into `dom` (dnsmasq matches a domain and all its subdomains).
fn egress_host(raw: &str, config: &HashMap<String, String>) -> Option<String> {
    let expanded = expand_path(raw, "", config);
    let s = expanded.trim();
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let s = s.split('/').next().unwrap_or(s); // drop any path
    let s = s.split(':').next().unwrap_or(s); // drop any port
    let s = s.strip_prefix("*.").unwrap_or(s); // suffix wildcard → base domain
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

// Secret-bearing paths a plugin must never read: MIRA's own config (provider
// keys) and credential/personal DBs (derived from the install layout —
// `<install_dir>` is `<data_dir>/packages/<id>`), plus the user's standard
// credential stores. Only existing paths matter; the launcher skips the rest.
#[cfg(unix)]
fn secret_mask_paths(install_dir: &Path, home: &str) -> Vec<String> {
    let mut masks: Vec<String> = Vec::new();
    // install_dir = <data_dir>/packages/<id>  →  data_dir =../../
    if let Some(data_root) = install_dir.parent().and_then(Path::parent) {
        for db in ["auth.db", "memory.db", "history.db"] {
            for suffix in ["", "-wal", "-shm"] {
                masks.push(data_root.join(format!("{db}{suffix}")).to_string_lossy().to_string());
            }
        }
        // <data_dir>/../config holds mira_config.json (provider API keys).
        if let Some(mira_root) = data_root.parent() {
            masks.push(mira_root.join("config").to_string_lossy().to_string());
        }
    }
    if !home.is_empty() {
        for d in [".ssh", ".aws", ".gnupg"] {
            masks.push(format!("{home}/{d}"));
        }
    }
    masks
}

// Expand a declared filesystem path: leading `~/` → `$HOME`, and `${VAR}`
// placeholders from the admin-supplied config.
fn expand_path(s: &str, home: &str, config: &HashMap<String, String>) -> String {
    let mut out = if let Some(rest) = s.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if s == "~" {
        home.to_string()
    } else {
        s.to_string()
    };
    for (k, v) in config {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

#[cfg(not(unix))]
fn confine_command(
    _new: &mut NewMcpServer,
    _component: &Component,
    _install_dir: &Path,
    _config: &HashMap<String, String>,
) {
}

// Reverse a (partial) ledger — best-effort, used on install failure.
fn rollback(ledger: &Ledger, mcp_store: &McpServerStore) {
    for entry in ledger.iter().rev() {
        match entry {
            LedgerEntry::McpServer { id } => {
                let _ = mcp_store.delete(id);
            }
            LedgerEntry::Files { dir } => {
                let _ = fs::remove_dir_all(dir);
            }
            LedgerEntry::Secret { .. } => {}
            // Channel-account teardown needs the channel store + secret vault,
            // threaded in by the cpp_provider install path (, slice 5).
            // No cpp_provider install creates these entries yet.
            LedgerEntry::ChannelAccount { .. } => {}
            LedgerEntry::Service { .. } => {}
        }
    }
}

// Reverse a full ledger in reverse order — best-effort, idempotent, never
// stuck (a missing resource is fine). Handles every entry type: an
// `mcp_servers` row, a created CPP/External channel account, a vaulted secret,
// and an extracted payload directory. Returns the mcp_server ids removed so the
// caller can hot-reload the registry.
pub fn reverse_ledger(
    ledger: &Ledger,
    package_id: &str,
    mcp_store: &McpServerStore,
    channel_store: &ChannelAccountStore,
    secrets: &SecretsStore,
) -> Vec<String> {
    let mut removed = Vec::new();
    for entry in ledger.iter().rev() {
        match entry {
            LedgerEntry::McpServer { id } => {
                let _ = mcp_store.delete(id);
                removed.push(id.clone());
            }
            LedgerEntry::ChannelAccount { id } => {
                let _ = channel_store.delete(id);
            }
            LedgerEntry::Service { unit } => {
                let _ = super::service::teardown(unit);
            }
            LedgerEntry::Secret { key } => {
                let _ = secrets.delete(Scope::System, "", package_id, key);
            }
            LedgerEntry::Files { dir } => {
                let _ = fs::remove_dir_all(dir);
            }
        }
    }
    removed
}

// Uninstall a package: reverse its ledger (best-effort, in reverse order),
// then drop the package record. Returns the mcp_server ids removed so the
// caller can hot-reload the registry. Idempotent and never-stuck: a missing
// resource is fine.
pub fn uninstall_package(
    id: &str,
    mcp_store: &McpServerStore,
    channel_store: &ChannelAccountStore,
    secrets: &SecretsStore,
    pkg_store: &PackageStore,
) -> Result<Vec<String>, String> {
    let pkg = pkg_store
        .get(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("package {id} is not installed"))?;
    let removed = reverse_ledger(&pkg.ledger, id, mcp_store, channel_store, secrets);
    pkg_store.delete(id).map_err(|e| e.to_string())?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::manifest::PackageManifest;

    fn manifest_with_spec(spec: serde_json::Value) -> PackageManifest {
        let m = serde_json::json!({
            "format": "1",
            "id": "com.example.nextcloud-mcp",
            "name": "Nextcloud MCP",
            "version": "1.0.0",
            "components": [ { "type": "mcp_server", "spec": spec } ]
        });
        serde_json::from_value(m).unwrap()
    }

    #[test]
    fn builds_mcp_server_with_config_overlay() {
        // NC_BASE_URL is declared in env; NC_APP_PASS is a declared secret —
        // both are fillable slots.
        let m: PackageManifest = serde_json::from_value(serde_json::json!({
            "format": "1", "id": "com.example.nextcloud-mcp", "name": "Nextcloud MCP", "version": "1.0.0",
            "components": [{
                "type": "mcp_server",
                "capabilities": { "secrets": ["NC_APP_PASS"] },
                "spec": { "command": "python3", "args": ["/opt/nc/server.py"], "env": { "NC_BASE_URL": "https://placeholder" } }
            }]
        }))
        .unwrap();
        let mut config = HashMap::new();
        config.insert("NC_BASE_URL".into(), "https://nc.example.com".into());
        config.insert("NC_APP_PASS".into(), "secret".into());

        let new = build_mcp_server(&m, &m.components[0], &config).unwrap();
        assert_eq!(new.name, "nextcloud-mcp"); // last dotted segment
        assert_eq!(new.command.as_deref(), Some("python3"));
        assert_eq!(new.env.get("NC_BASE_URL").unwrap(), "https://nc.example.com"); // config wins
        assert_eq!(new.env.get("NC_APP_PASS").unwrap(), "secret");
        assert!(new.enabled);
    }

    #[test]
    fn stdio_without_command_is_rejected() {
        let m = manifest_with_spec(serde_json::json!({ "transport": "stdio" }));
        let err = build_mcp_server(&m, &m.components[0], &HashMap::new()).unwrap_err();
        assert!(err.contains("requires a `command`"));
    }

    #[test]
    fn explicit_name_is_respected() {
        let m = manifest_with_spec(serde_json::json!({ "name": "nextcloud", "command": "python3" }));
        let new = build_mcp_server(&m, &m.components[0], &HashMap::new()).unwrap();
        assert_eq!(new.name, "nextcloud");
    }

    fn manifest_with_caps(secrets: &[&str]) -> PackageManifest {
        let secrets: Vec<&str> = secrets.to_vec();
        serde_json::from_value(serde_json::json!({
            "format": "1", "id": "com.x.p", "name": "P", "version": "1.0.0",
            "components": [{
                "type": "mcp_server",
                "capabilities": { "secrets": secrets },
                "spec": { "command": "python3" }
            }]
        }))
        .unwrap()
    }

    #[test]
    fn rejects_undeclared_config_key() {
        let m = manifest_with_spec(serde_json::json!({
            "command": "python3", "env": { "NC_USER": "alice" }
        }));
        let mut config = HashMap::new();
        config.insert("NC_USER".to_string(), "bob".to_string()); // declared (in env)
        config.insert("EVIL".to_string(), "x".to_string()); //   undeclared → reject
        let err = build_mcp_server(&m, &m.components[0], &config).unwrap_err();
        assert!(err.contains("EVIL"), "err={err}");
    }

    #[test]
    fn egress_host_reduces_to_bare_hostname() {
        let mut cfg = HashMap::new();
        cfg.insert("NC_BASE_URL".to_string(), "https://nc.example.com:8443/path".to_string());
        assert_eq!(egress_host("https://api.foo.com/x", &cfg).as_deref(), Some("api.foo.com"));
        assert_eq!(egress_host("api.foo.com:443", &cfg).as_deref(), Some("api.foo.com"));
        assert_eq!(egress_host("*.example.com", &cfg).as_deref(), Some("example.com"));
        assert_eq!(egress_host("${NC_BASE_URL}", &cfg).as_deref(), Some("nc.example.com"));
        assert_eq!(egress_host("   ", &cfg), None);
    }

    #[test]
    fn allows_declared_secret_not_in_env() {
        let m = manifest_with_caps(&["NC_APP_PASS"]); // declared secret, not in env
        let mut config = HashMap::new();
        config.insert("NC_APP_PASS".to_string(), "s".to_string());
        let new = build_mcp_server(&m, &m.components[0], &config).unwrap();
        assert_eq!(new.env.get("NC_APP_PASS").map(String::as_str), Some("s"));
    }

    #[test]
    fn gate_blocks_invalid_signature_even_with_ack() {
        let m = manifest_with_caps(&[]);
        let t = TrustLevel::Invalid { reason: "bad".into() };
        assert!(gate_install(&t, &m, true).is_err());
    }

    #[test]
    fn gate_requires_ack_for_unverified_with_secrets() {
        let m = manifest_with_caps(&["K"]);
        assert!(gate_install(&TrustLevel::Unsigned, &m, false).is_err());
        assert!(gate_install(&TrustLevel::Unsigned, &m, true).is_ok());
        assert!(gate_install(&TrustLevel::Verified { publisher: "x".into() }, &m, false).is_ok());
    }

    // Full pipeline against the real `nextcloud-mcp` manifest: build a bundle,
    // parse + verify it, install it into real stores (mcp row created, payload
    // extracted, paths rewritten, secret applied), then uninstall and confirm
    // everything is reversed.
    #[test]
    fn end_to_end_install_then_uninstall() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use rusqlite::Connection;

        let manifest = r#"{
          "format":"1","id":"com.example.nextcloud-mcp","name":"Nextcloud MCP","version":"1.0.0",
          "publisher":"Example",
          "components":[{"type":"mcp_server",
            "capabilities":{"network_egress":["${NC_BASE_URL}"],"secrets":["NC_APP_PASS"]},
            "spec":{"name":"nextcloud","transport":"stdio","command":"python3","args":["server.py"],
              "env":{"NC_BASE_URL":"https://nc.example.com","NC_USER":"alice","NC_APP_PASS":""}}}]
        }"#;

        // Build an in-memory.mirapkg (single top-level dir == id).
        let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        let entries: [(&str, &[u8]); 2] = [
            ("com.example.nextcloud-mcp/package.json", manifest.as_bytes()),
            ("com.example.nextcloud-mcp/server.py", b"print('hi')\n"),
        ];
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, path, data).unwrap();
        }
        let bundle = tar.into_inner().unwrap().finish().unwrap();

        // Temp env: a db with a users row (the FK target), the two stores, a pkg dir.
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("auth.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE users(id TEXT PRIMARY KEY); INSERT INTO users(id) VALUES('admin-1');",
            )
            .unwrap();
        }
        let mcp_store = McpServerStore::open(&db).unwrap();
        let pkg_store = PackageStore::open(&db).unwrap();
        let pkgs_dir = dir.path().join("packages");

        // parse → verify → install
        let parsed = crate::packages::parse_bundle(&bundle).unwrap();
        let trust = crate::packages::verify_package(
            &parsed.manifest,
            &crate::skills::trust::TrustStore::empty(),
        );
        let mut config = HashMap::new();
        config.insert("NC_APP_PASS".to_string(), "app-secret".to_string());

        let outcome = install_package(
            &parsed, &trust, "admin-1", &config, &pkgs_dir, &mcp_store, &pkg_store,
        )
        .unwrap();
        assert_eq!(outcome.package.id, "com.example.nextcloud-mcp");
        assert_eq!(outcome.mcp_server_ids.len(), 1);

        // The mcp server row: name, the extracted absolute server.py, the secret.
        let rows = mcp_store.list_for_user("admin-1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "nextcloud");
        let cfg = rows[0].to_config().unwrap();
        // The command is wrapped in the `mira pkg-exec` confinement launcher.
        assert!(cfg.args.iter().any(|a| a == "pkg-exec"), "args={:?}", cfg.args);
        assert!(cfg.args.iter().any(|a| a == "python3"), "real command after --: {:?}", cfg.args);
        let server_arg = cfg
            .args
            .iter()
            // Separator-agnostic so this holds on Windows (…\server.py) too.
            .find(|a| a.replace('\\', "/").ends_with("/server.py"))
            .expect("extracted server.py arg");
        assert!(std::path::Path::new(server_arg).exists(), "extracted server.py must exist");
        assert_eq!(cfg.env.get("NC_APP_PASS").map(String::as_str), Some("app-secret"));

        let install_dir = pkgs_dir.join("com.example.nextcloud-mcp");
        assert!(install_dir.join("server.py").exists());
        assert_eq!(pkg_store.list().unwrap().len(), 1);

        // Uninstall reverses everything.
        let channel_store = ChannelAccountStore::open(&db).unwrap();
        let (sdb, skey) = (dir.path().join("skill_secrets.db"), dir.path().join("master.key"));
        let secrets = SecretsStore::open(&sdb, &skey).unwrap();
        let removed = uninstall_package(
            "com.example.nextcloud-mcp", &mcp_store, &channel_store, &secrets, &pkg_store,
        )
        .unwrap();
        assert_eq!(removed.len(), 1);
        assert!(mcp_store.list_for_user("admin-1").unwrap().is_empty());
        assert!(pkg_store.get("com.example.nextcloud-mcp").unwrap().is_none());
        assert!(!install_dir.exists(), "extracted dir must be removed");
    }
}
