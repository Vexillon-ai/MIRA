// SPDX-License-Identifier: AGPL-3.0-or-later

//! launchd backend for `mira install` / `uninstall` on macOS.
//!
//! User-scoped LaunchAgent (not a system-wide LaunchDaemon) so no sudo is
//! required and the plist lives under the user's `~/Library/LaunchAgents/`.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::install::plist::{render, PlistInputs, LAUNCHD_LABEL};
use crate::install::unit::ServiceKind;

/// `~/Library/LaunchAgents/<label>.plist`.
fn plist_path_for(label: &str) -> PathBuf {
    home_dir().join("Library/LaunchAgents").join(format!("{label}.plist"))
}

/// `~/Library/LaunchAgents/com.mira.plist`.
pub fn plist_path() -> PathBuf {
    plist_path_for(LAUNCHD_LABEL)
}

/// `~/Library/LaunchAgents/com.mira.guardian-watch.plist` — the sentinel agent.
pub fn guardian_plist_path() -> PathBuf {
    plist_path_for(ServiceKind::GuardianWatch.launchd_label())
}

/// Default log directory (`~/Library/Logs/mira/`). macOS doesn't expose a
/// journal, so we write `mira.out.log` / `mira.err.log` here.
pub fn log_dir() -> PathBuf {
    home_dir().join("Library/Logs/mira")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME must be set")
}

/// Service-manager target for `launchctl`. The `gui/$UID/...` form scopes
/// commands to the current user's GUI session, which is what hosts user
/// LaunchAgents.
fn service_target() -> String {
    service_target_for(LAUNCHD_LABEL)
}

fn service_target_for(label: &str) -> String {
    format!("gui/{}/{label}", current_uid())
}

fn domain_target() -> String {
    format!("gui/{}", current_uid())
}

fn current_uid() -> u32 {
    // SAFETY: `getuid()` is defined to be infallible on POSIX systems and
    // is reentrant.
    unsafe { libc::getuid() }
}

pub struct InstallInputs {
    pub mira_bin:    PathBuf,
    pub config_path: PathBuf,
    pub working_dir: PathBuf,
    /// Absolute data dir, baked into the agent's ProgramArguments.
    pub data_dir:    PathBuf,
    pub web_dir:     Option<PathBuf>,
    pub enable_now:  bool,
}

pub fn install(inputs: &InstallInputs) -> Result<(), Box<dyn Error>> {
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let logs = log_dir();
    std::fs::create_dir_all(&logs)?;

    // fastembed's ORT fallback dlopens libonnxruntime.dylib whenever the
    // configured embedding endpoint is unreachable. macOS's launchd-spawned
    // dlopen only searches /usr/lib (SIP-protected), so we have to point at
    // a Homebrew install via ORT_DYLIB_PATH or the agent will crashloop.
    let ort = detect_onnxruntime();
    let mut extra_env: Vec<(&str, &str)> = Vec::new();
    if let Some(p) = ort.as_deref() {
        extra_env.push(("ORT_DYLIB_PATH", p));
        println!("✓ onnxruntime: {p}");
    } else {
        println!("(no libonnxruntime.dylib found in Homebrew prefixes — fastembed");
        println!(" fallback will crash on first start. Install with:");
        println!("   brew install onnxruntime");
        println!(" then re-run `mira install --force`.)");
    }

    let body = render(&PlistInputs {
        service:     ServiceKind::Server,
        mira_bin:    &inputs.mira_bin,
        config_path: &inputs.config_path,
        working_dir: &inputs.working_dir,
        data_dir:    &inputs.data_dir,
        web_dir:     inputs.web_dir.as_deref(),
        log_dir:     &logs,
        extra_env:   &extra_env,
    });
    write_atomic(&plist, &body)?;
    println!("✓ wrote {}", plist.display());

    // From here on, any failure cleans up the plist we just wrote — same
    // motivation as the Linux backend: a half-installed agent traps the
    // user (next `mira install` refuses but the service doesn't actually
    // run).
    let activate = || -> Result<(), Box<dyn Error>> {
        // bootstrap will refuse if the agent is already loaded; tolerate
        // that by booting it out first.
        let _ = run_launchctl(&["bootout", &service_target()]);

        if inputs.enable_now {
            run_launchctl(&["bootstrap", &domain_target(), &plist.display().to_string()])?;
            println!("✓ launchctl bootstrap {}", service_target());
        } else {
            println!("(skipped bootstrap per --no-enable; load with: launchctl bootstrap {} {})",
                domain_target(), plist.display());
        }
        Ok(())
    };

    match activate() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&plist);
            eprintln!("(cleaned up partial install: removed {})", plist.display());
            Err(e)
        }
    }
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    let plist = plist_path();
    if !plist.exists() {
        println!("MIRA LaunchAgent not found at {} — nothing to do.", plist.display());
        return Ok(());
    }
    // Best-effort: agent may not be loaded.
    let _ = run_launchctl(&["bootout", &service_target()]);
    std::fs::remove_file(&plist)?;
    println!("✓ removed {}", plist.display());
    Ok(())
}

fn ensure_plist_installed() -> Result<(), Box<dyn Error>> {
    let plist = plist_path();
    if !plist.exists() {
        return Err(format!(
            "MIRA LaunchAgent not found at {}. Run `mira install` first, \
             or use `mira --server` to run in the foreground.",
            plist.display()
        ).into());
    }
    Ok(())
}

/// Install + load the Guardian sentinel as its OWN LaunchAgent
/// (`com.mira.guardian-watch`), separate from the main agent so it outlives a
/// MIRA crash. Mirrors [`install`].
pub fn install_guardian(inputs: &InstallInputs) -> Result<(), Box<dyn Error>> {
    let plist = guardian_plist_path();
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let logs = log_dir();
    std::fs::create_dir_all(&logs)?;
    // The sentinel's recall_history embeds via onnxruntime when reachable; point
    // at a Homebrew ORT if present so recall works (best-effort — recall degrades
    // gracefully without it, and the sentinel serves no web bundle).
    let ort = detect_onnxruntime();
    let mut extra_env: Vec<(&str, &str)> = Vec::new();
    if let Some(p) = ort.as_deref() {
        extra_env.push(("ORT_DYLIB_PATH", p));
    }

    let body = render(&PlistInputs {
        service:     ServiceKind::GuardianWatch,
        mira_bin:    &inputs.mira_bin,
        config_path: &inputs.config_path,
        working_dir: &inputs.working_dir,
        data_dir:    &inputs.data_dir,
        web_dir:     None,
        log_dir:     &logs,
        extra_env:   &extra_env,
    });
    write_atomic(&plist, &body)?;
    println!("✓ wrote {}", plist.display());

    let target = service_target_for(ServiceKind::GuardianWatch.launchd_label());
    let activate = || -> Result<(), Box<dyn Error>> {
        let _ = run_launchctl(&["bootout", &target]);
        if inputs.enable_now {
            run_launchctl(&["bootstrap", &domain_target(), &plist.display().to_string()])?;
            println!("✓ launchctl bootstrap {target}");
        } else {
            println!("(skipped bootstrap per --no-enable; load with: launchctl bootstrap {} {})",
                domain_target(), plist.display());
        }
        Ok(())
    };
    match activate() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&plist);
            eprintln!("(cleaned up partial install: removed {})", plist.display());
            Err(e)
        }
    }
}

/// Boot out + remove the Guardian sentinel LaunchAgent.
pub fn uninstall_guardian() -> Result<(), Box<dyn Error>> {
    let plist = guardian_plist_path();
    if !plist.exists() {
        println!("Guardian LaunchAgent not found at {} — nothing to do.", plist.display());
        return Ok(());
    }
    let _ = run_launchctl(&["bootout", &service_target_for(ServiceKind::GuardianWatch.launchd_label())]);
    std::fs::remove_file(&plist)?;
    println!("✓ removed {}", plist.display());
    Ok(())
}

pub fn start() -> Result<(), Box<dyn Error>> {
    ensure_plist_installed()?;
    // `kickstart` only works on a loaded agent. If `mira stop` was called
    // first (which `bootout`s the agent entirely), kickstart fails with 113
    // — fall back to `bootstrap` which loads + starts in one step.
    if try_kickstart(false) { return Ok(()); }
    run_launchctl(&["bootstrap", &domain_target(), &plist_path().display().to_string()])?;
    println!("✓ bootstrap {}", service_target());
    Ok(())
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    ensure_plist_installed()?;
    // bootout fully unloads the agent. We can't just SIGTERM because
    // KeepAlive=true would respawn it. Tolerate 113 ("not loaded") so
    // `mira stop` is idempotent across repeated calls. Capture stderr
    // (rather than inheriting) so the "Could not find service" message
    // on the idempotent path doesn't leak to the user.
    let o = Command::new("launchctl")
        .args(["bootout", &service_target()])
        .output()?;
    if o.status.success() || o.status.code() == Some(113) {
        println!("✓ bootout {}", service_target());
        return Ok(());
    }
    Err(format!(
        "launchctl bootout failed (status {}): {}",
        o.status,
        String::from_utf8_lossy(&o.stderr).trim(),
    ).into())
}

pub fn restart() -> Result<(), Box<dyn Error>> {
    ensure_plist_installed()?;
    // `-k` kills + restarts an already-loaded agent. Same fallback as
    // `start`: if the agent was bootout'd, kickstart returns 113 — recover
    // by bootstrapping fresh.
    if try_kickstart(true) { return Ok(()); }
    run_launchctl(&["bootstrap", &domain_target(), &plist_path().display().to_string()])?;
    println!("✓ bootstrap {}", service_target());
    Ok(())
}

/// Returns true on success. Prints the verb on success so callers don't
/// double-print. Uses `output()` rather than `status()` so launchctl's
/// "Could not find service" stderr on the expected failure path (agent
/// not loaded → caller falls back to bootstrap) doesn't leak to the user.
fn try_kickstart(kill: bool) -> bool {
    let mut args = vec!["kickstart"];
    if kill { args.push("-k"); }
    let target = service_target();
    args.push(&target);
    match Command::new("launchctl").args(&args).output() {
        Ok(o) if o.status.success() => {
            let flag = if kill { " -k" } else { "" };
            println!("✓ kickstart{flag} {target}");
            true
        }
        _ => false,
    }
}

pub fn status() -> Result<(), Box<dyn Error>> {
    ensure_plist_installed()?;
    // `launchctl print` exits 113 ("could not find service") when the agent
    // isn't loaded — that's not an error in this context, the user just
    // wants the report.
    run_launchctl_inherited(&["print", &service_target()], &[113])
}

fn run_launchctl(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = Command::new("launchctl").args(args).output()?;
    if !out.status.success() {
        return Err(format!(
            "launchctl {} failed (status {}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ).into());
    }
    Ok(())
}

fn run_launchctl_inherited(args: &[&str], allow_codes: &[i32]) -> Result<(), Box<dyn Error>> {
    let s = Command::new("launchctl").args(args).status()?;
    if s.success() { return Ok(()); }
    if let Some(code) = s.code() {
        if allow_codes.contains(&code) { return Ok(()); }
    }
    Err(format!("launchctl {} returned {}", args.join(" "), s).into())
}

fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("plist.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Probe a few standard Homebrew prefixes for `libonnxruntime.dylib`.
/// Apple Silicon brews to `/opt/homebrew`, Intel to `/usr/local`. We don't
/// shell out to `brew --prefix onnxruntime` to avoid taking a hard
/// dependency on Homebrew being on PATH at install time.
fn detect_onnxruntime() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "/opt/homebrew/lib/libonnxruntime.dylib",
        "/usr/local/lib/libonnxruntime.dylib",
        "/opt/homebrew/opt/onnxruntime/lib/libonnxruntime.dylib",
        "/usr/local/opt/onnxruntime/lib/libonnxruntime.dylib",
    ];
    CANDIDATES.iter()
        .find(|p| Path::new(p).exists())
        .map(|p| (*p).to_string())
}
