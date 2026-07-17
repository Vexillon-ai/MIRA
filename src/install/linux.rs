// SPDX-License-Identifier: AGPL-3.0-or-later

//! systemd --user backend for `mira install` / `uninstall`.
//!
//! User-scoped (not system-scoped) so no sudo is required, and the unit
//! file lives next to the rest of the user's config under `$XDG_CONFIG_HOME`.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::unit::{render, ServiceKind, UnitInputs};

// `$XDG_CONFIG_HOME/systemd/user/mira.service`, falling back to
// `~/.config/systemd/user/mira.service` per XDG.
pub fn unit_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .expect("HOME must be set");
    base.join("systemd/user/mira.service")
}

// `/etc/systemd/system/mira.service` — written by `mira install --system`.
// System-scoped: starts at boot, runs as a dedicated `mira` system user,
// requires sudo at install time.
pub fn system_unit_path() -> PathBuf {
    PathBuf::from("/etc/systemd/system/mira.service")
}

// The Guardian liveness sentinel's user unit —
// `$XDG_CONFIG_HOME/systemd/user/mira-guardian-watch.service`. A SEPARATE unit
// from `mira.service` on purpose: it must outlive a server crash.
pub fn guardian_unit_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .expect("HOME must be set");
    base.join("systemd/user/mira-guardian-watch.service")
}

// System-scope Guardian sentinel unit (`mira guardian-install --system`).
pub fn guardian_system_unit_path() -> PathBuf {
    PathBuf::from("/etc/systemd/system/mira-guardian-watch.service")
}

const GUARDIAN_UNIT: &str = "mira-guardian-watch.service";

// True when `systemctl --user` operations will actually succeed in this
// environment. Probes the user manager with a side-effect-free query
// (`show --property=Version`) — it exits 0 only when the user bus is
// reachable, so this catches both "systemctl missing" and "WSL distro
// without systemd enabled" without any false positives.
pub fn is_systemd_user_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "show", "--property=Version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub struct InstallInputs {
    pub mira_bin:    PathBuf,
    pub config_path: PathBuf,
    pub working_dir: PathBuf,
    // Absolute data dir, baked into ExecStart so the service (which under
    // `--system` runs as the `mira` user) reads the operator-chosen location.
    pub data_dir:    PathBuf,
    pub web_dir:     Option<PathBuf>,
    pub enable_now:  bool,
    // when true, write the system-scope unit + run
    // system-bus systemctl. Caller (run_install) has already
    // confirmed effective UID == 0 or `sudo` is available.
    pub system:      bool,
}

// System-scope unix user MIRA runs as when --system is used. Created
// with `useradd --system` at install time if it doesn't already exist.
pub const SYSTEM_USER: &str = "mira";

pub fn install(inputs: &InstallInputs) -> Result<(), Box<dyn Error>> {
    if inputs.system {
        return install_system(inputs);
    }
    install_user(inputs)
}

fn install_user(inputs: &InstallInputs) -> Result<(), Box<dyn Error>> {
    let unit = unit_path();
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = render(&UnitInputs {
        service:     ServiceKind::Server,
        mira_bin:    &inputs.mira_bin,
        config_path: &inputs.config_path,
        working_dir: &inputs.working_dir,
        data_dir:    &inputs.data_dir,
        web_dir:     inputs.web_dir.as_deref(),
        system_user: None,
    });
    write_atomic(&unit, &body)?;
    println!("✓ wrote {}", unit.display());

    // From here on, any failure cleans up the unit file we just wrote.
    // A half-installed unit traps the user: the next `mira install`
    // refuses ("already exists") but the service doesn't actually run.
    let activate = || -> Result<(), Box<dyn Error>> {
        run_systemctl(&["--user", "daemon-reload"])?;
        println!("✓ systemctl --user daemon-reload");
        if inputs.enable_now {
            run_systemctl(&["--user", "enable", "--now", "mira.service"])?;
            println!("✓ systemctl --user enable --now mira.service");
        } else {
            println!("(skipped enable --now per --no-enable)");
        }
        Ok(())
    };

    match activate() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&unit);
            eprintln!("(cleaned up partial install: removed {})", unit.display());
            Err(e)
        }
    }
}

fn install_system(inputs: &InstallInputs) -> Result<(), Box<dyn Error>> {
    // Effective-UID gate — system-scope writes to /etc, owns files as
    // root, and needs system-bus systemctl. Tell the caller to re-run
    // under sudo rather than blowing up at the first EACCES.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return Err(
            "`mira install --system` writes to /etc/systemd/system, creates the \
             `mira` system user, and runs system-bus systemctl. \
             Re-run with sudo: `sudo mira install --system [args…]`".into()
        );
    }

    // Make sure the `mira` system user exists. Best-effort: useradd
    // returns 9 when the user already exists, which is fine; any other
    // non-zero exit is a real error (no /sbin/useradd, etc.).
    ensure_system_user(SYSTEM_USER)?;

    let unit = system_unit_path();
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = render(&UnitInputs {
        service:     ServiceKind::Server,
        mira_bin:    &inputs.mira_bin,
        config_path: &inputs.config_path,
        working_dir: &inputs.working_dir,
        data_dir:    &inputs.data_dir,
        web_dir:     inputs.web_dir.as_deref(),
        system_user: Some(SYSTEM_USER),
    });
    write_atomic(&unit, &body)?;
    println!("✓ wrote {} (root-owned)", unit.display());

    // Make the service's working dir + config dir + data dir owned by the mira
    // user so it can read/write them. Best-effort — chown failures shouldn't
    // abort the install (operator can fix manually).
    if let Err(e) = chown_user(&inputs.working_dir, SYSTEM_USER) {
        eprintln!("(warning: chown {} to {}: {})", inputs.working_dir.display(), SYSTEM_USER, e);
    }
    if let Some(cfg_parent) = inputs.config_path.parent() {
        if let Err(e) = chown_user(cfg_parent, SYSTEM_USER) {
            eprintln!("(warning: chown {} to {}: {})", cfg_parent.display(), SYSTEM_USER, e);
        }
    }
    // The data dir often lives outside working_dir (custom location, or the
    // default ~/.mira/data which resolves to root's home when run via sudo).
    // chown it explicitly so the `mira` user can open its databases.
    if let Err(e) = chown_user(&inputs.data_dir, SYSTEM_USER) {
        eprintln!("(warning: chown {} to {}: {})", inputs.data_dir.display(), SYSTEM_USER, e);
    }

    let activate = || -> Result<(), Box<dyn Error>> {
        run_systemctl(&["daemon-reload"])?;
        println!("✓ systemctl daemon-reload");
        if inputs.enable_now {
            run_systemctl(&["enable", "--now", "mira.service"])?;
            println!("✓ systemctl enable --now mira.service");
        } else {
            println!("(skipped enable --now per --no-enable)");
        }
        Ok(())
    };

    match activate() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&unit);
            eprintln!("(cleaned up partial install: removed {})", unit.display());
            Err(e)
        }
    }
}

/// Install + enable the Guardian liveness sentinel as its own supervised unit
/// (`mira-guardian-watch.service`) — separate from `mira.service` so it keeps
/// watching when the server crashes. Mirrors [`install`] (user vs system by
/// `inputs.system`), reusing the same render/write/activate machinery. The unit
/// runs `mira guardian-watch`, which self-gates on `guardian.process.enabled`
/// (it idles quietly when disabled, so an installed-but-disabled sentinel does
/// NOT restart-loop).
pub fn install_guardian(inputs: &InstallInputs) -> Result<(), Box<dyn Error>> {
    let system_user = if inputs.system {
        let euid = unsafe { libc::geteuid() };
        if euid != 0 {
            return Err(
                "`mira guardian-install --system` writes to /etc/systemd/system and runs \
                 system-bus systemctl. Re-run with sudo.".into()
            );
        }
        ensure_system_user(SYSTEM_USER)?;
        Some(SYSTEM_USER)
    } else {
        None
    };

    let unit = if inputs.system { guardian_system_unit_path() } else { guardian_unit_path() };
    if let Some(parent) = unit.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = render(&UnitInputs {
        service:     ServiceKind::GuardianWatch,
        mira_bin:    &inputs.mira_bin,
        config_path: &inputs.config_path,
        working_dir: &inputs.working_dir,
        data_dir:    &inputs.data_dir,
        web_dir:     None, // sentinel serves no web bundle
        system_user,
    });
    write_atomic(&unit, &body)?;
    println!("✓ wrote {}", unit.display());

    let scope: &[&str] = if inputs.system { &[] } else { &["--user"] };
    // Same delete-on-failure cleanup as the server install: never leave a
    // half-installed unit behind.
    let activate = || -> Result<(), Box<dyn Error>> {
        let mut reload = scope.to_vec(); reload.push("daemon-reload");
        run_systemctl(&reload)?;
        if inputs.enable_now {
            let mut en = scope.to_vec(); en.extend_from_slice(&["enable", "--now", GUARDIAN_UNIT]);
            run_systemctl(&en)?;
            println!("✓ enabled + started {GUARDIAN_UNIT}");
        } else {
            println!("(skipped enable --now per --no-enable)");
        }
        Ok(())
    };
    match activate() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&unit);
            eprintln!("(cleaned up partial install: removed {})", unit.display());
            Err(e)
        }
    }
}

/// Remove the Guardian sentinel unit (system-scope first, then user-scope).
/// Best-effort disable; used both by `mira guardian-uninstall` and folded into
/// `mira uninstall` so it doesn't orphan a unit.
pub fn uninstall_guardian() -> Result<(), Box<dyn Error>> {
    let sys = guardian_system_unit_path();
    if sys.exists() {
        let euid = unsafe { libc::geteuid() };
        if euid != 0 {
            return Err(format!(
                "System-scope Guardian unit found at {} — re-run with sudo.", sys.display()
            ).into());
        }
        let _ = run_systemctl(&["disable", "--now", GUARDIAN_UNIT]);
        std::fs::remove_file(&sys)?;
        println!("✓ removed {}", sys.display());
        let _ = run_systemctl(&["daemon-reload"]);
        return Ok(());
    }
    let unit = guardian_unit_path();
    if !unit.exists() {
        println!("Guardian sentinel unit not found at {} — nothing to do.", unit.display());
        return Ok(());
    }
    let _ = run_systemctl(&["--user", "disable", "--now", GUARDIAN_UNIT]);
    std::fs::remove_file(&unit)?;
    println!("✓ removed {}", unit.display());
    let _ = run_systemctl(&["--user", "daemon-reload"]);
    Ok(())
}

fn ensure_system_user(name: &str) -> Result<(), Box<dyn Error>> {
    // `id -u name` exits 0 when the user exists. Skip useradd in that case.
    let exists = Command::new("id").args(["-u", name]).output()
        .map(|o| o.status.success()).unwrap_or(false);
    if exists {
        println!("✓ system user {name} already exists");
        return Ok(());
    }
    // `--system` creates a uid below SYS_UID_MAX with no aging, no
    // password, no expiry. `--home-dir` + `--create-home` give it a
    // place to keep ssh keys / dotfiles if an operator later wants
    // them; the actual working_dir for the service is the explicit
    // one in InstallInputs.
    let status = Command::new("useradd")
        .args([
            "--system",
            "--create-home",
            "--home-dir", &format!("/var/lib/{name}"),
            "--shell", "/usr/sbin/nologin",
            "--comment", "MIRA service",
            name,
        ])
        .status()?;
    if !status.success() && status.code() != Some(9) {
        return Err(format!("useradd {name} failed (status {status})").into());
    }
    println!("✓ created system user {name}");
    Ok(())
}

fn chown_user(path: &Path, user: &str) -> Result<(), Box<dyn Error>> {
    let status = Command::new("chown")
        .args(["-R", &format!("{user}:{user}"), &path.display().to_string()])
        .status()?;
    if !status.success() {
        return Err(format!("chown failed (status {status})").into());
    }
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    // Try system-scope first (more impactful) — if a system unit is
    // present it almost certainly takes precedence over a user unit
    // on the same host, and silently leaving it behind would be
    // confusing. Operator can re-run uninstall to clean the user unit
    // afterwards if both existed.
    let system_unit = system_unit_path();
    if system_unit.exists() {
        let euid = unsafe { libc::geteuid() };
        if euid != 0 {
            return Err(
                "System-scope MIRA unit found at /etc/systemd/system/mira.service — \
                 re-run with sudo: `sudo mira uninstall`".into()
            );
        }
        let _ = run_systemctl(&["disable", "--now", "mira.service"]);
        std::fs::remove_file(&system_unit)?;
        println!("✓ removed {}", system_unit.display());
        let _ = run_systemctl(&["daemon-reload"]);
        return Ok(());
    }
    let unit = unit_path();
    if !unit.exists() {
        println!("MIRA service unit not found at {} — nothing to do.", unit.display());
        return Ok(());
    }
    // Best-effort: unit may not be enabled or running.
    let _ = run_systemctl(&["--user", "disable", "--now", "mira.service"]);
    std::fs::remove_file(&unit)?;
    println!("✓ removed {}", unit.display());
    let _ = run_systemctl(&["--user", "daemon-reload"]);
    Ok(())
}

fn run_systemctl(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = Command::new("systemctl").args(args).output()?;
    if !out.status.success() {
        return Err(format!(
            "systemctl {} failed (status {}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ).into());
    }
    Ok(())
}

// Run systemctl with stdout/stderr inherited so colors and journal hints
// reach the terminal. Used by `mira status` whose output is the whole point.
fn run_systemctl_inherited(args: &[&str], allow_codes: &[i32]) -> Result<(), Box<dyn Error>> {
    let s = Command::new("systemctl").args(args).status()?;
    if s.success() { return Ok(()); }
    if let Some(code) = s.code() {
        if allow_codes.contains(&code) { return Ok(()); }
    }
    Err(format!("systemctl {} returned {}", args.join(" "), s).into())
}

fn ensure_unit_installed() -> Result<(), Box<dyn Error>> {
    let unit = unit_path();
    if !unit.exists() {
        return Err(format!(
            "MIRA service unit not found at {}. Run `mira install` first, \
             or use `mira --server` to run in the foreground.",
            unit.display()
        ).into());
    }
    Ok(())
}

pub fn start() -> Result<(), Box<dyn Error>> {
    ensure_unit_installed()?;
    run_systemctl(&["--user", "start", "mira.service"])?;
    println!("✓ started mira.service");
    Ok(())
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    ensure_unit_installed()?;
    run_systemctl(&["--user", "stop", "mira.service"])?;
    println!("✓ stopped mira.service");
    Ok(())
}

pub fn restart() -> Result<(), Box<dyn Error>> {
    ensure_unit_installed()?;
    run_systemctl(&["--user", "restart", "mira.service"])?;
    println!("✓ restarted mira.service");
    Ok(())
}

pub fn status() -> Result<(), Box<dyn Error>> {
    ensure_unit_installed()?;
    // `systemctl status` exits 3 when the unit is inactive/dead — that's
    // not an error in this context, the user just wants the report.
    run_systemctl_inherited(&["--user", "status", "mira.service"], &[3])
}

// Write to a sibling `.tmp` then rename, so a concurrent reader (or a
// systemctl daemon-reload mid-write) never observes a half-written unit.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("service.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}
