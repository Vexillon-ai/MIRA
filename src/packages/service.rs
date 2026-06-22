// SPDX-License-Identifier: AGPL-3.0-or-later

//! Managed same-host provider services — the `mira.write_service` tier (//! follow-up).
//!
//! A `cpp_provider` package can ship the provider binary in its payload and ask
//! MIRA to run it as a supervised service instead of the admin running it by
//! hand (the connection-only default). This module renders + installs that
//! service and tears it down on uninstall/cancel.
//!
//! **Linux (systemd `--user`) is implemented.** MIRA itself runs as a
//! `systemd --user` service, so a plugin service slots in alongside it
//! (`mira-plugin-<id>.service`), supervised the same way (`Restart=always`),
//! and is reachable on the host the provider listens on. On macOS/Windows this
//! returns a clear error so the install falls back to connection-only — the
//! admin runs the provider themselves.
//!
//! Secrets (the minted CPP/bot secrets) are written to a `0600`
//! **EnvironmentFile** in the package's private dir, never into the unit file
//! or the journal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// What to run as the managed provider service.
pub struct ServiceSpec {
    // The package id (→ the unit name).
    pub package_id: String,
    pub description: String,
    // Absolute path to the provider entrypoint.
    pub command: PathBuf,
    pub args: Vec<String>,
    // Environment for the process (templated config — secrets included).
    pub env: BTreeMap<String, String>,
    // Working directory (the package's extracted payload dir).
    pub working_dir: PathBuf,
}

// The systemd unit name for a package's provider service. Deterministic so
// teardown can find it from the package id alone.
pub fn unit_name(package_id: &str) -> String {
    let safe: String = package_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' { c } else { '-' })
        .collect();
    format!("mira-plugin-{safe}.service")
}

// `~/.config/systemd/user`.
#[allow(dead_code)] // used on Linux; dead on macOS/Windows builds
fn user_unit_dir() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("systemd/user"));
        }
    }
    std::env::var("HOME").ok().filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".config/systemd/user"))
}

// Render the systemd `--user` unit. Secrets are pulled in via `EnvironmentFile`
// (rendered separately, `0600`), not inlined here.
pub fn render_unit(spec: &ServiceSpec, env_file: &Path) -> String {
    let mut exec = shell_quote(&spec.command.to_string_lossy());
    for a in &spec.args {
        exec.push(' ');
        exec.push_str(&shell_quote(a));
    }
    let env_file_line = if spec.env.is_empty() {
        String::new()
    } else {
        format!("EnvironmentFile={}\n", env_file.display())
    };
    format!(
        "[Unit]
Description={desc}
After=network-online.target
Wants=network-online.target
# Best-effort: start after MIRA so the provider can reach it on boot.
After=mira.service
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
{env_file_line}ExecStart={exec}
WorkingDirectory={dir}
Restart=always
RestartSec=2s
NoNewPrivileges=true
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
",
        desc = spec.description,
        dir = spec.working_dir.display(),
    )
}

// Render the `0600` EnvironmentFile body (`KEY=VALUE` per line).
#[allow(dead_code)] // used on Linux + in tests; dead on macOS/Windows non-test builds
fn render_env_file(env: &BTreeMap<String, String>) -> String {
    let mut s = String::new();
    for (k, v) in env {
        // systemd EnvironmentFile takes the rest of the line as the value;
        // strip newlines defensively so a value can't inject extra vars.
        let v = v.replace(['\n', '\r'], "");
        s.push_str(&format!("{k}={v}\n"));
    }
    s
}

// Minimal shell-ish quoting for the ExecStart line (systemd splits on spaces
// unless quoted). Wraps in double quotes + escapes embedded quotes/backslashes.
fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@=".contains(c)) {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// Install + start the provider service. Returns the unit name (recorded in the
// ledger for teardown). Linux-only; other platforms return an error so the
// install degrades to connection-only.
pub fn install_and_start(spec: &ServiceSpec) -> Result<String, String> {
    install_and_start_impl(spec)
}

#[cfg(target_os = "linux")]
fn install_and_start_impl(spec: &ServiceSpec) -> Result<String, String> {
    use std::io::Write;

    let unit = unit_name(&spec.package_id);
    let dir = user_unit_dir().ok_or("cannot locate ~/.config/systemd/user (no HOME)")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create unit dir: {e}"))?;

    // Ensure the working dir exists — a package may declare a service without
    // shipping payload files (the install dir is only created when extracting
    // payload), and the EnvironmentFile lives here.
    std::fs::create_dir_all(&spec.working_dir).map_err(|e| format!("create working dir: {e}"))?;

    // 0600 EnvironmentFile in the package's private dir.
    let env_file = spec.working_dir.join(".mira-service.env");
    if !spec.env.is_empty() {
        write_0600(&env_file, &render_env_file(&spec.env))
            .map_err(|e| format!("write env file: {e}"))?;
    }

    let unit_path = dir.join(&unit);
    let mut f = std::fs::File::create(&unit_path).map_err(|e| format!("write unit: {e}"))?;
    f.write_all(render_unit(spec, &env_file).as_bytes())
        .map_err(|e| format!("write unit: {e}"))?;
    drop(f);

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", &unit])
        .map_err(|e| format!("enable {unit}: {e} (unit written to {})", unit_path.display()))?;
    Ok(unit)
}

// ── macOS (launchd LaunchAgent) ─────────────────────────────────────────────

// The launchd label / ledger handle for a package's provider on macOS.
#[allow(dead_code)] // used on macOS + in tests
fn launchd_label(package_id: &str) -> String {
    let safe: String = package_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' { c } else { '-' })
        .collect();
    format!("com.mira.plugin.{safe}")
}

#[cfg(target_os = "macos")]
fn launch_agents_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join("Library/LaunchAgents"))
}

// Render a LaunchAgent plist: `KeepAlive`/`RunAtLoad` mirror systemd's
// `Restart=always`. Secrets live in the plist's `EnvironmentVariables` (the
// file is written `0600`); launchd has no EnvironmentFile equivalent.
pub fn render_plist(spec: &ServiceSpec, label: &str) -> String {
    let mut args = String::new();
    args.push_str(&format!("        <string>{}</string>\n", xml_escape(&spec.command.to_string_lossy())));
    for a in &spec.args {
        args.push_str(&format!("        <string>{}</string>\n", xml_escape(a)));
    }
    let env_block = if spec.env.is_empty() {
        String::new()
    } else {
        let mut s = String::from("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (k, v) in &spec.env {
            let v = v.replace(['\n', '\r'], "");
            s.push_str(&format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(k),
                xml_escape(&v),
            ));
        }
        s.push_str("    </dict>\n");
        s
    };
    format!(
"<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>WorkingDirectory</key>
    <string>{dir}</string>
{env_block}    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
",
        dir = xml_escape(&spec.working_dir.to_string_lossy()),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn install_and_start_impl(spec: &ServiceSpec) -> Result<String, String> {
    let label = launchd_label(&spec.package_id);
    let dir = launch_agents_dir().ok_or("cannot locate ~/Library/LaunchAgents (no HOME)")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create LaunchAgents dir: {e}"))?;
    std::fs::create_dir_all(&spec.working_dir).map_err(|e| format!("create working dir: {e}"))?;

    let plist = dir.join(format!("{label}.plist"));
    write_0600(&plist, &render_plist(spec, &label)).map_err(|e| format!("write plist: {e}"))?;

    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}");
    // bootout first in case a stale agent is loaded; tolerate failure.
    let _ = run_launchctl(&["bootout", &format!("{target}/{label}")]);
    run_launchctl(&["bootstrap", &target, &plist.to_string_lossy()])
        .map_err(|e| format!("launchctl bootstrap {label}: {e}"))?;
    Ok(label)
}

#[cfg(target_os = "macos")]
fn teardown_impl(label: &str) -> Result<(), String> {
    let uid = unsafe { libc::getuid() };
    let _ = run_launchctl(&["bootout", &format!("gui/{uid}/{label}")]);
    if let Some(dir) = launch_agents_dir() {
        let _ = std::fs::remove_file(dir.join(format!("{label}.plist")));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_launchctl(args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| format!("spawn launchctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

// ── Windows + other ─────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn install_and_start_impl(_spec: &ServiceSpec) -> Result<String, String> {
    // A Windows SCM service must answer the Service Control Manager handshake
    // within ~30s or it's killed — a plain provider binary isn't service-aware,
    // so SCM is the wrong tool here. Degrade to connection-only.
    Err("mira.write_service isn't supported on Windows — a generic provider binary can't run \
         as an SCM service. Run the provider yourself (connection-only) and use a \
         `command`/`note` step instead"
        .into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn install_and_start_impl(_spec: &ServiceSpec) -> Result<String, String> {
    Err("mira.write_service (a MIRA-managed same-host provider) isn't supported on this platform \
         — run the provider yourself (connection-only)"
        .into())
}

// Start or stop a managed service **without** removing it (package
// disable/enable). Best-effort + idempotent. Linux only; a no-op elsewhere.
pub fn set_running(unit: &str, on: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let action = if on { "start" } else { "stop" };
        return systemctl(&[action, unit]);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (unit, on);
        Ok(())
    }
}

// Stop, disable, and remove a provider service. Idempotent + best-effort: a
// missing unit is fine. `unit` is the name recorded in the ledger.
pub fn teardown(unit: &str) -> Result<(), String> {
    teardown_impl(unit)
}

#[cfg(target_os = "linux")]
fn teardown_impl(unit: &str) -> Result<(), String> {
    // Best-effort stop/disable — ignore errors (already gone / never started).
    let _ = systemctl(&["disable", "--now", unit]);
    if let Some(dir) = user_unit_dir() {
        let _ = std::fs::remove_file(dir.join(unit));
    }
    let _ = systemctl(&["daemon-reload"]);
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn teardown_impl(_unit: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("spawn systemctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(unix)]
fn write_0600(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_name_is_deterministic_and_safe() {
        assert_eq!(unit_name("com.example.nextcloud-talk"), "mira-plugin-com.example.nextcloud-talk.service");
        assert_eq!(unit_name("a/b c"), "mira-plugin-a-b-c.service");
    }

    #[test]
    fn render_unit_has_execstart_workdir_and_env_file() {
        let mut env = BTreeMap::new();
        env.insert("TALK_BOT_SECRET".to_string(), "deadbeef".to_string());
        let spec = ServiceSpec {
            package_id: "com.x.talk".into(),
            description: "Talk provider".into(),
            command: PathBuf::from("/pkgs/com.x.talk/provider"),
            args: vec!["--port".into(), "8099".into()],
            env,
            working_dir: PathBuf::from("/pkgs/com.x.talk"),
        };
        let envf = PathBuf::from("/pkgs/com.x.talk/.mira-service.env");
        let u = render_unit(&spec, &envf);
        assert!(u.contains("ExecStart=/pkgs/com.x.talk/provider --port 8099"));
        assert!(u.contains("WorkingDirectory=/pkgs/com.x.talk"));
        assert!(u.contains("EnvironmentFile=/pkgs/com.x.talk/.mira-service.env"));
        assert!(u.contains("Restart=always"));
        // The secret value itself is NOT in the unit.
        assert!(!u.contains("deadbeef"));
    }

    #[test]
    fn env_file_is_key_value_and_strips_newlines() {
        let mut env = BTreeMap::new();
        env.insert("A".to_string(), "one".to_string());
        env.insert("B".to_string(), "two\nINJECT=evil".to_string());
        let body = render_env_file(&env);
        assert!(body.contains("A=one\n"));
        // The newline injection is flattened into one line.
        assert!(body.contains("B=twoINJECT=evil\n"));
        assert_eq!(body.lines().count(), 2);
    }

    #[test]
    fn shell_quote_wraps_only_when_needed() {
        assert_eq!(shell_quote("/usr/bin/node"), "/usr/bin/node");
        assert_eq!(shell_quote("a b"), "\"a b\"");
    }

    #[test]
    fn macos_plist_has_program_args_workdir_keepalive_and_escaped_env() {
        let mut env = BTreeMap::new();
        env.insert("TALK_BOT_SECRET".to_string(), "dead<beef>".to_string());
        let spec = ServiceSpec {
            package_id: "com.x.talk".into(),
            description: "Talk provider".into(),
            command: PathBuf::from("/pkgs/com.x.talk/provider"),
            args: vec!["--port".into(), "8099".into()],
            env,
            working_dir: PathBuf::from("/pkgs/com.x.talk"),
        };
        let label = launchd_label(&spec.package_id);
        assert_eq!(label, "com.mira.plugin.com.x.talk");
        let p = render_plist(&spec, &label);
        assert!(p.contains("<string>com.mira.plugin.com.x.talk</string>"));
        assert!(p.contains("<string>/pkgs/com.x.talk/provider</string>"));
        assert!(p.contains("<string>--port</string>"));
        assert!(p.contains("<key>WorkingDirectory</key>"));
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains("<key>RunAtLoad</key>"));
        // The secret value is XML-escaped (and present — it's a user-0600 plist).
        assert!(p.contains("dead&lt;beef&gt;"));
    }
}
