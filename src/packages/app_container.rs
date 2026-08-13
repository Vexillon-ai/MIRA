// SPDX-License-Identifier: AGPL-3.0-or-later

// src/packages/app_container.rs
//! Managed service containers for apps (FDI-2 — the family-domain security
//! stack, etc.). When an installed, active app declares an [`AppContainer`],
//! MIRA runs it **detached** (`-d`), **restart-on-failure**, and **named**
//! `mira-app-<id>` — starting it on enable and removing it on disable/uninstall,
//! reconciling on startup. This is distinct from the confined, foreground
//! `subprocess` tool handler in `container.rs` (a one-shot); this is a
//! long-running backend a family provides (Wazuh, OPNsense, …).
//!
//! Uses the container engine `container::detect_engine()` finds (docker/podman).
//! When no engine is present, container apps degrade to inert (their tools/UI
//! still install; the backend just isn't run) — surfaced, never a hard failure.

use crate::packages::apps::AppContainer;

/// The container name MIRA manages for an app.
pub fn container_name(app_id: &str) -> String {
    format!("mira-app-{}", app_id.replace('.', "-"))
}

/// Build the detached `run …` argument vector for an app service container.
/// Pure — unit-testable without an engine. `env` is the already-resolved
/// `KEY=VALUE` set (config templates expanded by the caller).
pub fn run_args(app_id: &str, spec: &AppContainer, env: &[(String, String)]) -> Vec<String> {
    let name = container_name(app_id);
    let mut a: Vec<String> = vec![
        "run".into(), "-d".into(),
        "--name".into(), name.clone(),
        // Label so a reconcile pass can find MIRA-managed app containers.
        "--label".into(), "mira.app=1".into(),
        "--label".into(), format!("mira.app.id={app_id}"),
        "--restart".into(), "unless-stopped".into(),
        "--memory".into(), spec.memory.clone(),
        "--security-opt".into(), "no-new-privileges".into(),
    ];
    for p in &spec.ports {
        a.push("-p".into());
        let host = if p.public { format!("0.0.0.0:{}", p.host) } else { format!("127.0.0.1:{}", p.host) };
        a.push(format!("{host}:{}", p.container));
    }
    for (k, v) in env {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    for vol in &spec.volumes {
        // A per-app named volume so an uninstall can leave data intact if wanted.
        a.push("-v".into());
        a.push(format!("{name}-{}:{}", vol.name, vol.path));
    }
    a.push(spec.image.clone());
    a
}

/// Resolve `${config.KEY}` templates in the declared env against the app's
/// stored config JSON. An unresolved template is dropped (never leaked raw).
pub fn resolve_env(spec: &AppContainer, config: &serde_json::Value) -> Vec<(String, String)> {
    spec.env.iter().filter_map(|(k, raw)| {
        let mut val = raw.clone();
        if let Some(inner) = raw.strip_prefix("${config.").and_then(|s| s.strip_suffix('}')) {
            match config.get(inner).and_then(|v| v.as_str()) {
                Some(resolved) => val = resolved.to_string(),
                None => return None, // unresolved config ref → omit
            }
        }
        Some((k.clone(), val))
    }).collect()
}

/// Start (or restart) the app's container detached. Idempotent — removes any
/// stale container of the same name first. Blocking; call from `spawn_blocking`.
pub fn start(engine: &str, app_id: &str, spec: &AppContainer, env: &[(String, String)]) -> Result<(), String> {
    let _ = std::process::Command::new(engine)
        .args(["rm", "-f", &container_name(app_id)]).output();
    let out = std::process::Command::new(engine)
        .args(run_args(app_id, spec, env))
        .output()
        .map_err(|e| format!("spawn {engine}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Stop + remove the app's container. Idempotent (no-op if absent).
pub fn stop(engine: &str, app_id: &str) -> Result<(), String> {
    let out = std::process::Command::new(engine)
        .args(["rm", "-f", &container_name(app_id)])
        .output()
        .map_err(|e| format!("spawn {engine}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Whether the app's container is currently running.
pub fn is_running(engine: &str, app_id: &str) -> bool {
    std::process::Command::new(engine)
        .args(["inspect", "-f", "{{.State.Running}}", &container_name(app_id)])
        .output().ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Names of all MIRA-managed app containers currently present (running or not),
/// via the `mira.app=1` label. Used to reap orphans a reconcile no longer wants.
pub fn managed_container_names(engine: &str) -> Vec<String> {
    std::process::Command::new(engine)
        .args(["ps", "-a", "--filter", "label=mira.app=1", "--format", "{{.Names}}"])
        .output().ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::apps::{AppPort, AppVolume};

    fn spec() -> AppContainer {
        AppContainer {
            image: "wazuh/wazuh-manager:4.9.0".into(),
            ports: vec![
                AppPort { host: 1514, container: 1514, public: true },
                AppPort { host: 55000, container: 55000, public: false },
            ],
            env: Default::default(),
            volumes: vec![AppVolume { name: "data".into(), path: "/var/ossec/data".into() }],
            memory: "2g".into(),
        }
    }

    #[test]
    fn container_name_sanitizes_dots() {
        assert_eq!(container_name("com.mira.wazuh"), "mira-app-com-mira-wazuh");
    }

    #[test]
    fn run_args_are_detached_named_restart_and_map_ports_volumes() {
        let args = run_args("com.mira.wazuh", &spec(), &[("KEY".into(), "v".into())]);
        let joined = args.join(" ");
        assert!(joined.starts_with("run -d --name mira-app-com-mira-wazuh"));
        assert!(joined.contains("--restart unless-stopped"));
        assert!(joined.contains("--memory 2g"));
        // Public port on all interfaces, private on loopback.
        assert!(joined.contains("-p 0.0.0.0:1514:1514"));
        assert!(joined.contains("-p 127.0.0.1:55000:55000"));
        // Env + per-app named volume.
        assert!(joined.contains("-e KEY=v"));
        assert!(joined.contains("-v mira-app-com-mira-wazuh-data:/var/ossec/data"));
        // Image is last.
        assert_eq!(args.last().unwrap(), "wazuh/wazuh-manager:4.9.0");
    }

    #[test]
    fn resolve_env_expands_config_templates_and_drops_unresolved() {
        let mut s = spec();
        s.env.insert("WAZUH_API_URL".into(), "${config.api_url}".into());
        s.env.insert("STATIC".into(), "literal".into());
        s.env.insert("MISSING".into(), "${config.nope}".into());
        let cfg = serde_json::json!({ "api_url": "https://198.51.100.10:55000" });
        let mut got = resolve_env(&s, &cfg);
        got.sort();
        assert_eq!(got, vec![
            ("STATIC".to_string(), "literal".to_string()),
            ("WAZUH_API_URL".to_string(), "https://198.51.100.10:55000".to_string()),
        ]);
    }
}
