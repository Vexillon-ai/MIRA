// SPDX-License-Identifier: AGPL-3.0-or-later

//! `mira install` / `uninstall` — make the OS supervise MIRA so the web
//! UI's Restart button actually restarts.
//!
//! Slices 2/6 of design-docs/install-and-supervisor.md ship the Linux/systemd-user,
//! macOS/launchd, and Windows/SCM backends. Each also installs the out-of-process
//! Guardian sentinel as its own supervised unit (`mira guardian-install`). Docker
//! users are pointed at `docker compose` (slice 5).

pub mod binary_upgrade;
pub mod chatterbox;
pub mod data_version;
pub mod deps;
pub mod unit;
pub mod plist;
pub mod rollback;
pub mod upgrade;
pub mod backup;
pub mod backup_crypto;
pub mod backup_scheduler;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

use std::error::Error;
use std::path::{Path, PathBuf};

pub struct InstallOptions {
    pub config_path: PathBuf,
    pub working_dir: PathBuf,
    // Explicit override for the built React bundle. When None, install
    // auto-detects via the same resolver the server uses at runtime.
    pub web_dir:     Option<PathBuf>,
    pub no_enable:   bool,
    pub force:       bool,
    // write a system-scoped unit instead of a user-scoped one.
    // Linux: `/etc/systemd/system/mira.service`, requires sudo at install
    // time, runs as a dedicated `mira` system user, starts on boot
    // regardless of who's logged in. Use this on VPS / shared hosts.
    // macOS: a `/Library/LaunchDaemons/com.mira.plist` (deferred — Linux
    // is implemented today). Default (false) keeps the user-scope install.
    pub system:      bool,
}

// What kind of host we're running on, for the purposes of choosing an
// install backend (or refusing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKind {
    // Linux with `systemctl --user` available — happy path.
    LinuxSystemdUser,
    // Linux without a working user systemd. WSL where systemd hasn't been
    // enabled lands here.
    LinuxNoSystemdUser,
    // Inside a container — supervision is the runtime's job.
    Docker,
    Macos,
    Windows,
    Other,
}

pub fn detect_host() -> HostKind {
    if is_docker() { return HostKind::Docker; }
    if cfg!(target_os = "macos") { return HostKind::Macos; }
    if cfg!(target_os = "windows") { return HostKind::Windows; }
    #[cfg(target_os = "linux")]
    {
        return if linux::is_systemd_user_available() {
            HostKind::LinuxSystemdUser
        } else {
            HostKind::LinuxNoSystemdUser
        };
    }
    #[cfg(not(target_os = "linux"))]
    HostKind::Other
}

// Resolve the data dir to bake into the service launch, as an absolute path.
// Priority: `--data-dir` / `MIRA_DATA_DIR` (set by the flag) > the `data_dir`
// field in the config at `config_path` (written by `mira setup`) > the default.
// We read the config as raw JSON rather than fully loading it so a partial /
// pre-setup config still yields a sensible answer.
fn resolve_install_data_dir(config_path: &Path) -> PathBuf {
    if let Some(d) = crate::config::data_dir_env_override() {
        return absolutize(&d);
    }
    if let Ok(txt) = std::fs::read_to_string(config_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(s) = v
                .get("data_dir")
                .and_then(|x| x.as_str())
                .filter(|s| !s.trim().is_empty())
            {
                return absolutize(&crate::config::expand_path(s));
            }
        }
    }
    absolutize(&crate::config::default_data_dir_path())
}

// Make a path absolute without requiring it to exist (canonicalize would fail
// on a not-yet-created dir). `~` is assumed already expanded by the caller.
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

pub fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|s| {
            let l = s.to_lowercase();
            l.contains("microsoft") || l.contains("wsl")
        })
        .unwrap_or(false)
}

pub fn is_docker() -> bool {
    if std::path::Path::new("/.dockerenv").exists() { return true; }
    std::fs::read_to_string("/proc/1/cgroup")
        .map(|s| s.contains("docker") || s.contains("containerd") || s.contains("podman"))
        .unwrap_or(false)
}

pub fn run_install(opts: InstallOptions) -> Result<(), Box<dyn Error>> {
    match detect_host() {
        HostKind::Docker => {
            return Err(
                "MIRA is running inside a container — supervision is the container runtime's job. \
                 Use `docker compose restart` (or your orchestrator) to bounce the service, or \
                 click the Restart button in the web UI.".into()
            );
        }
        HostKind::Windows => {
            // Windows native service support shipped. The
            // `--system` flag is irrelevant on Windows (SCM services
            // are always system-scope); ignore it with a hint rather
            // than rejecting.
            if opts.system {
                eprintln!("(note: --system is implied on Windows — SCM services are always system-scope)");
            }
        }
        HostKind::Other => {
            return Err("Unsupported platform — run `mira --server` directly.".into());
        }
        HostKind::LinuxNoSystemdUser => {
            if is_wsl() {
                return Err(
                    "WSL systemd is not enabled. Add the following to /etc/wsl.conf, then run \
                     `wsl --shutdown` from PowerShell and try `mira install` again:\n\n\
                     [boot]\n\
                     systemd=true\n".into()
                );
            }
            return Err(
                "systemd --user is not available on this system ($XDG_RUNTIME_DIR/systemd/private \
                 missing). Run `mira --server` under a supervisor of your choice.".into()
            );
        }
        // Windows is matched + handled above; this arm only catches
        // the "valid platform, no early-return needed" case for Linux
        // macOS.
        HostKind::LinuxSystemdUser | HostKind::Macos => {}
    }

    // ── Platform-agnostic preparation ─────────────────────────────────────
    let mira_bin = std::env::current_exe()
        .map_err(|e| -> Box<dyn Error> {
            format!("could not resolve current binary path: {e}").into()
        })?;

    if let Some(existing) = supervisor_unit_path() {
        if existing.exists() && !opts.force {
            return Err(format!(
                "MIRA service unit already exists at {}. Pass --force to overwrite, \
                 or run `mira uninstall` first.",
                existing.display()
            ).into());
        }
    }

    if is_wsl() {
        println!("Detected WSL.");
        println!("  · The service stops when WSL stops (closing the last shell).");
        println!("  · Run `loginctl enable-linger $USER` if you want it to survive logout");
        println!("    (the WSL distro itself still shuts down on idle — see ~/.wslconfig");
        println!("    `vmIdleTimeout` to extend that).");
        println!();
    }

    // Make sure parent of config exists so the service can read it on
    // first start. The data dir is created by MIRA itself.
    if let Some(p) = opts.config_path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::create_dir_all(&opts.working_dir)?;

    // The web UI is embedded in the binary, so a release build always serves it
    // with no extra files. An on-disk `web/dist` (MIRA_WEB_DIR / cwd / repo) only
    // takes precedence as a dev override for live rebuilds.
    let web_dir = opts.web_dir.clone()
        .or_else(crate::web::static_files::resolve_web_dist);
    match &web_dir {
        Some(p) => println!("✓ web bundle (disk override): {}", p.display()),
        None    => println!("✓ web UI: embedded in binary"),
    }

    // Resolve the data dir to an ABSOLUTE path now, as the installing user, and
    // bake it into the service launch below. A supervised service may run under
    // a different account (systemd --system's `mira` user, a launchd agent, a
    // Windows LocalSystem service) whose `~` expands elsewhere — pinning the
    // absolute path the installer resolved keeps the service reading the same
    // data the operator set up via `mira setup`. Priority: --data-dir /
    // MIRA_DATA_DIR > the config's `data_dir` field > the built-in default.
    let data_dir = resolve_install_data_dir(&opts.config_path);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("(warning: couldn't create data dir {}: {})", data_dir.display(), e);
    }
    println!("✓ data dir: {}", data_dir.display());

    #[cfg(target_os = "linux")]
    {
        linux::install(&linux::InstallInputs {
            mira_bin,
            config_path: opts.config_path.clone(),
            working_dir: opts.working_dir.clone(),
            data_dir:    data_dir.clone(),
            web_dir,
            enable_now:  !opts.no_enable,
            system:      opts.system,
        })?;

        if opts.system {
            // System-scope follow-up tips. Different commands, different
            // log location, different "stops with logout" implications.
            println!();
            println!("MIRA installed as a system service (runs as user `{}`).", linux::SYSTEM_USER);
            println!("  status: systemctl status mira");
            println!("  logs:   journalctl -u mira -f");
            println!("  stop:   systemctl stop mira");
            println!("  starts at boot regardless of who's logged in.");
        } else if !opts.no_enable {
            println!();
            println!("MIRA installed as a systemd user service.");
            println!("  status: systemctl --user status mira");
            println!("  logs:   journalctl --user -u mira -f");
            println!("  stop:   systemctl --user stop mira");
        } else {
            println!();
            println!("Unit file written. Start with: systemctl --user enable --now mira");
        }
        // If the operator has already enabled the liveness sentinel in config,
        // point them at its (separate) install — we don't auto-install a second
        // unit from `mira install` to keep the two services explicit.
        if guardian_process_enabled(&opts.config_path) {
            println!();
            println!("guardian.process.enabled is set — install the liveness sentinel's");
            println!("separate unit with:  mira guardian-install");
        }
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        if opts.system {
            return Err(
                "`--system` install for macOS (LaunchDaemons under /Library) is not \
                 implemented yet. Use the default user-agent install: `mira install` \
                 (without --system). System-scope macOS install lands in a follow-up.".into()
            );
        }
        macos::install(&macos::InstallInputs {
            mira_bin,
            config_path: opts.config_path.clone(),
            working_dir: opts.working_dir.clone(),
            data_dir:    data_dir.clone(),
            web_dir,
            enable_now:  !opts.no_enable,
        })?;

        if !opts.no_enable {
            println!();
            println!("MIRA installed as a launchd user agent.");
            println!("  status: launchctl print gui/$UID/{label}", label = plist::LAUNCHD_LABEL);
            println!("  logs:   ~/Library/Logs/mira/mira.{{out,err}}.log");
            println!("  stop:   mira stop");
        } else {
            println!();
            println!(
                "Plist written. Load with: launchctl bootstrap gui/$UID {}",
                macos::plist_path().display(),
            );
        }
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        windows::install(&windows::InstallInputs {
            mira_bin,
            config_path: opts.config_path.clone(),
            working_dir: opts.working_dir.clone(),
            data_dir:    data_dir.clone(),
            web_dir,
            enable_now:  !opts.no_enable,
        })?;
        println!();
        println!("MIRA installed as a Windows service ('{}').", windows::SERVICE_NAME);
        println!("  status: mira status   (or: sc query {})", windows::SERVICE_NAME);
        println!("  logs:   Event Viewer  (Windows Logs → Application)");
        println!("  stop:   mira stop     (or: sc stop {})", windows::SERVICE_NAME);
        println!("  starts at boot regardless of who's logged in.");
        return Ok(());
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    Err("This branch is unreachable on platforms without a supervisor backend.".into())
}

// Path to the platform-native unit/plist file, when the platform has a
// backend at all. Used by `run_install` for the "already installed" check
// and by `upgrade.rs` to decide whether to bounce the service.
pub fn supervisor_unit_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // Prefer the system-scope unit when one's already installed;
        // re-running `mira install` on top of a system unit should
        // refuse-with-message rather than create a parallel user
        // unit that the OS will quietly never start.
        let sys = linux::system_unit_path();
        if sys.exists() { return Some(sys); }
        return Some(linux::unit_path());
    }
    #[cfg(target_os = "macos")]
    { return Some(macos::plist_path()); }
    #[cfg(target_os = "windows")]
    { return Some(windows::unit_path()); }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    None
}

pub fn run_uninstall() -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    {
        // Clean the Guardian sentinel unit too (best-effort) so uninstall never
        // orphans it. Failure here (e.g. a system-scope unit needing sudo) is
        // logged but doesn't block removing the main service.
        if let Err(e) = linux::uninstall_guardian() {
            eprintln!("(note: could not remove the Guardian sentinel unit: {e})");
        }
        return linux::uninstall();
    }
    #[cfg(target_os = "macos")]
    {
        if let Err(e) = macos::uninstall_guardian() {
            eprintln!("(note: could not remove the Guardian sentinel agent: {e})");
        }
        return macos::uninstall();
    }
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = windows::uninstall_guardian() {
            eprintln!("(note: could not remove the Guardian sentinel service: {e})");
        }
        return windows::uninstall();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    Err("Uninstall is only supported on Linux/systemd, macOS/launchd, or Windows/SCM.".into())
}

// Whether `guardian.process.enabled` is true in the config at `config_path`,
// read as raw JSON (like `resolve_install_data_dir`) so it works pre-`setup`.
fn guardian_process_enabled(config_path: &Path) -> bool {
    std::fs::read_to_string(config_path).ok()
        .and_then(|txt| serde_json::from_str::<serde_json::Value>(&txt).ok())
        .and_then(|v| v.get("guardian")
            .and_then(|g| g.get("process"))
            .and_then(|p| p.get("enabled"))
            .and_then(|e| e.as_bool()))
        .unwrap_or(false)
}

/// Install + enable the Guardian liveness sentinel as its own supervised unit
/// (`mira guardian-install`). Separate from the main service so it outlives a
/// server crash. Supervised on all three desktop backends: Linux/systemd-user,
/// macOS/launchd, and Windows/SCM — each as a service distinct from the main
/// MIRA unit so a MIRA crash never takes the watchdog down with it.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos", target_os = "windows")), allow(unused_variables))]
pub fn run_guardian_install(opts: InstallOptions) -> Result<(), Box<dyn Error>> {
    match detect_host() {
        HostKind::LinuxSystemdUser | HostKind::Macos | HostKind::Windows => {}
        HostKind::Docker => return Err(
            "Inside a container — run the sentinel as a compose sidecar service \
             (`command: mira guardian-watch`), not via this installer.".into()
        ),
        HostKind::LinuxNoSystemdUser if is_wsl() => return Err(
            "WSL systemd is not enabled. Add to /etc/wsl.conf then `wsl --shutdown`:\n\n\
             [boot]\nsystemd=true\n".into()
        ),
        HostKind::LinuxNoSystemdUser => return Err("systemd --user is not available on this system.".into()),
        HostKind::Other => return Err("Unsupported platform.".into()),
    }

    // Platform-agnostic prep.
    let mira_bin = std::env::current_exe()
        .map_err(|e| -> Box<dyn Error> { format!("could not resolve current binary path: {e}").into() })?;
    if let Some(p) = opts.config_path.parent() { std::fs::create_dir_all(p)?; }
    std::fs::create_dir_all(&opts.working_dir)?;
    let data_dir = resolve_install_data_dir(&opts.config_path);
    let enabled  = guardian_process_enabled(&opts.config_path);

    #[cfg(target_os = "linux")]
    {
        linux::install_guardian(&linux::InstallInputs {
            mira_bin,
            config_path: opts.config_path.clone(),
            working_dir: opts.working_dir.clone(),
            data_dir,
            web_dir:     None,
            enable_now:  !opts.no_enable,
            system:      opts.system,
        })?;
        println!();
        println!("✓ MIRA-Guardian liveness sentinel installed as a separate unit.");
        if opts.system {
            println!("  status: systemctl status mira-guardian-watch");
            println!("  logs:   journalctl -u mira-guardian-watch -f");
        } else {
            println!("  status: systemctl --user status mira-guardian-watch");
            println!("  logs:   journalctl --user -u mira-guardian-watch -f");
        }
    }
    #[cfg(target_os = "macos")]
    {
        macos::install_guardian(&macos::InstallInputs {
            mira_bin,
            config_path: opts.config_path.clone(),
            working_dir: opts.working_dir.clone(),
            data_dir,
            web_dir:     None,
            enable_now:  !opts.no_enable,
        })?;
        println!();
        println!("✓ MIRA-Guardian liveness sentinel installed as a launchd agent.");
        println!("  status: launchctl print gui/$UID/com.mira.guardian-watch");
        println!("  logs:   ~/Library/Logs/mira/mira-guardian-watch.{{out,err}}.log");
    }
    #[cfg(target_os = "windows")]
    {
        windows::install_guardian(&windows::InstallInputs {
            mira_bin,
            config_path: opts.config_path.clone(),
            working_dir: opts.working_dir.clone(),
            data_dir,
            web_dir:     None,
            enable_now:  !opts.no_enable,
        })?;
        println!();
        println!("✓ MIRA-Guardian liveness sentinel installed as a Windows service ('{}').",
                 windows::GUARDIAN_SERVICE_NAME);
        println!("  status: sc query {}", windows::GUARDIAN_SERVICE_NAME);
    }

    if !enabled {
        println!("  note:   guardian.process.enabled is false — the sentinel is installed but idles.");
        println!("          Enable it (Settings → Guardian → Liveness sentinel), then restart the unit.");
    }
    // The MIRA-down alarm can only reach you via a channel that doesn't need MIRA
    // — direct web-push. Remind the operator to register a device, or the alarm
    // reaches no one.
    println!("  reach:  the down-alarm uses direct web-push when MIRA is down — register at least one");
    println!("          device (open MIRA in a browser and allow notifications), or it can't reach you.");
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    return Err("Guardian sentinel install is only supported on Linux/systemd, macOS/launchd, or Windows/SCM.".into());
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    Ok(())
}

/// Remove the Guardian sentinel unit (`mira guardian-uninstall`).
pub fn run_guardian_uninstall() -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    return linux::uninstall_guardian();
    #[cfg(target_os = "macos")]
    return macos::uninstall_guardian();
    #[cfg(target_os = "windows")]
    return windows::uninstall_guardian();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    Err("Guardian sentinel uninstall is only supported on Linux/systemd, macOS/launchd, or Windows/SCM.".into())
}

/// Build guardian-sentinel `InstallOptions` from a running server's config path,
/// mirroring the CLI defaults in `main.rs`. `system` is inferred from an
/// `/etc/mira` config (the `--system` convention), so a system-scoped MIRA
/// installs a system-scoped sentinel and a user-scoped one stays user-scoped.
pub fn guardian_install_opts_for(config_path: PathBuf) -> InstallOptions {
    let system = config_path.starts_with("/etc/mira");
    let working_dir = if system {
        PathBuf::from("/var/lib/mira")
    } else {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    };
    InstallOptions { config_path, working_dir, web_dir: None, no_enable: false, force: false, system }
}

/// React to a **live** change of `guardian.process.enabled` from the WebUI: when
/// it flips on, register + start the sentinel as its own supervised service; when
/// it flips off, stop + unregister it. Runs on a blocking thread and is
/// fire-and-forget so a slow service op never blocks the settings save — the
/// outcome is logged, and the Guardian panel reflects the resulting state.
///
/// This needs the running server to have the privilege to manage its sibling
/// service (LocalSystem on a Windows SCM install; the user's own systemd/launchd
/// scope on Linux/macOS). When it doesn't (e.g. a bare console dev run), the op
/// fails and we log a clear "run `mira guardian-install` yourself" fallback rather
/// than silently elevating — matching the least-privilege / degrade-and-notify
/// posture of the privileged-helper design.
pub fn apply_guardian_enable_change(enabled: bool, config_path: PathBuf) {
    tokio::task::spawn_blocking(move || {
        if enabled {
            match run_guardian_install(guardian_install_opts_for(config_path)) {
                Ok(())  => tracing::info!("guardian: auto-registered + started the sentinel service (enabled via WebUI)"),
                Err(e)  => tracing::error!(
                    "guardian: could not auto-register the sentinel service ({e}). \
                     Run `mira guardian-install` from a privileged shell to finish enabling it."
                ),
            }
        } else {
            match run_guardian_uninstall() {
                Ok(())  => tracing::info!("guardian: auto-unregistered + stopped the sentinel service (disabled via WebUI)"),
                Err(e)  => tracing::error!(
                    "guardian: could not auto-unregister the sentinel service ({e}). \
                     Run `mira guardian-uninstall` from a privileged shell to remove it."
                ),
            }
        }
    });
}

// Common pre-flight for service-control subcommands. Refuses with a
// targeted message when the platform/host can't honour the operation.
fn require_supervised() -> Result<(), Box<dyn Error>> {
    match detect_host() {
        HostKind::LinuxSystemdUser | HostKind::Macos | HostKind::Windows => Ok(()),
        HostKind::Docker => Err(
            "Inside a container — supervision is the runtime's job. Use \
             `docker compose restart` (or your orchestrator) instead.".into()
        ),
        HostKind::LinuxNoSystemdUser if is_wsl() => Err(
            "WSL systemd is not enabled. Add the following to /etc/wsl.conf, \
             then run `wsl --shutdown` from PowerShell:\n\n\
             [boot]\nsystemd=true\n".into()
        ),
        HostKind::LinuxNoSystemdUser => Err(
            "systemd --user is not available on this system.".into()
        ),
        HostKind::Other => Err("Unsupported platform.".into()),
    }
}

pub fn run_start() -> Result<(), Box<dyn Error>> {
    require_supervised()?;
    #[cfg(target_os = "linux")]
    return linux::start();
    #[cfg(target_os = "macos")]
    return macos::start();
    #[cfg(target_os = "windows")]
    return windows::start();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    unreachable!()
}

pub fn run_stop() -> Result<(), Box<dyn Error>> {
    require_supervised()?;
    #[cfg(target_os = "linux")]
    return linux::stop();
    #[cfg(target_os = "macos")]
    return macos::stop();
    #[cfg(target_os = "windows")]
    return windows::stop();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    unreachable!()
}

pub fn run_restart() -> Result<(), Box<dyn Error>> {
    require_supervised()?;
    #[cfg(target_os = "linux")]
    return linux::restart();
    #[cfg(target_os = "macos")]
    return macos::restart();
    #[cfg(target_os = "windows")]
    return windows::restart();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    unreachable!()
}

pub fn run_status() -> Result<(), Box<dyn Error>> {
    require_supervised()?;
    #[cfg(target_os = "linux")]
    return linux::status();
    #[cfg(target_os = "macos")]
    return macos::status();
    #[cfg(target_os = "windows")]
    return windows::status();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    unreachable!()
}

pub fn run_upgrade(opts: upgrade::UpgradeOptions) -> Result<(), Box<dyn Error>> {
    upgrade::run_upgrade(opts)
}

pub fn run_binary_upgrade(opts: binary_upgrade::BinaryUpgradeOptions) -> Result<(), Box<dyn Error>> {
    binary_upgrade::run_binary_upgrade(opts)
}
