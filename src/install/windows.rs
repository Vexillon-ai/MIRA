// SPDX-License-Identifier: AGPL-3.0-or-later

//! Windows Service Control Manager backend.
//!
//! Registers `mira.exe` as a native Windows service. No NSSM, no WSL.
//! Mirrors the systemd / launchd backends:
//! - [`install`] writes the SCM registration + starts the service.
//! - [`uninstall`] removes it.
//! - [`start`] / [`stop`] / [`restart`] / [`status`] control it.
//!
//! When SCM launches the service, the Windows runtime calls
//! `service_main` (via the `define_windows_service!` macro). That
//! function registers a control handler, then runs the same server
//! loop `mira --server` runs from the console — driven by a
//! tokio::sync::Notify that the control handler trips on SCM Stop.
//!
//! All SCM install/control calls require an elevated process. Failures
//! surface as a clear "Run as Administrator" message rather than a
//! raw AccessDenied.

#![cfg(target_os = "windows")]

use std::error::Error;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
        ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

// SCM service name. Internal identifier — what `sc query` shows.
pub const SERVICE_NAME: &str = "mira";
// User-visible display name shown in services.msc.
const DISPLAY_NAME: &str = "MIRA — Multi-tasking Intelligent Responsive Assistant";
// Description shown in services.msc tooltip.
const DESCRIPTION: &str =
    "MIRA agent server. Manages chat, channels, automations, and the web UI.";

// The out-of-process Guardian sentinel's SCM service — a SEPARATE service from
// `mira`, deliberately not dependent on it, so it keeps watching when the main
// service crashes. Started with the `guardian-watch` subcommand in its ImagePath,
// which is how the binary routes an SCM start to the sentinel entry.
pub const GUARDIAN_SERVICE_NAME: &str = "mira-guardian-watch";
const GUARDIAN_DISPLAY_NAME: &str = "MIRA-Guardian liveness sentinel";
const GUARDIAN_DESCRIPTION: &str =
    "Out-of-process MIRA-Guardian watchdog: watches that MIRA is alive and raises a direct alarm if not.";

// On Windows there's no on-disk "unit file" — the service registration
// lives in the registry under SCM. Return a virtual identifier so other
// code's "is installed?" check still has something to call.
pub fn unit_path() -> PathBuf {
    PathBuf::from(format!(r"sc://services/{}", SERVICE_NAME))
}

pub struct InstallInputs {
    pub mira_bin:    PathBuf,
    pub config_path: PathBuf,
    pub working_dir: PathBuf,
    // Absolute data dir, baked into the service launch as `--data-dir`. The
    // service runs as LocalSystem, whose `~` would otherwise expand to
    // C:\Windows\System32\config\systemprofile — a different, empty data dir
    // from the one `mira setup` wrote under the installing admin's profile.
    pub data_dir:    PathBuf,
    pub web_dir:     Option<PathBuf>,
    pub enable_now:  bool,
}

pub fn install(inputs: &InstallInputs) -> Result<(), Box<dyn Error>> {
    let manager = open_manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .map_err(elevation_hint("install"))?;

    let mut launch_args: Vec<OsString> = Vec::new();
    launch_args.push(OsString::from("--server"));
    launch_args.push(OsString::from("--config"));
    launch_args.push(inputs.config_path.clone().into_os_string());
    launch_args.push(OsString::from("--data-dir"));
    launch_args.push(inputs.data_dir.clone().into_os_string());

    let service_info = ServiceInfo {
        name:             OsString::from(SERVICE_NAME),
        display_name:     OsString::from(DISPLAY_NAME),
        service_type:     ServiceType::OWN_PROCESS,
        // AutoStart so the service comes up at boot — matches Linux
        // `WantedBy=multi-user.target` semantics.
        start_type:       ServiceStartType::AutoStart,
        error_control:    ServiceErrorControl::Normal,
        executable_path:  inputs.mira_bin.clone(),
        launch_arguments: launch_args,
        dependencies:     vec![],
        // LocalSystem account by default. A dedicated NT-SERVICE
        // account would be more locked-down; ship LocalSystem in v1
        // to match the simplicity of the systemd-user backend.
        account_name:     None,
        account_password: None,
    };

    // Create-or-replace semantics: nuke any half-installed registration
    // from a previous failed install so re-running is idempotent.
    if let Ok(existing) = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE) {
        eprintln!("(removing stale service registration before re-install)");
        let _ = existing.delete();
    }

    let service = manager
        .create_service(
            &service_info,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
        )
        .map_err(elevation_hint("install"))?;
    service.set_description(DESCRIPTION).map_err(box_err)?;
    println!("✓ registered Windows service '{SERVICE_NAME}'");

    // Restart contract parity with the Linux/Docker supervisors. MIRA's
    // cross-platform restart model is "the process exits and the OS
    // supervisor relaunches it" — that's how the web-UI Restart button and
    // self-update work. But SCM does NOT relaunch a service that exits
    // cleanly: by default recovery actions fire only on an *unexpected*
    // termination, never on a reported Stop. Without the config below, a
    // self-initiated restart would leave the service Stopped on Windows.
    //
    // So we register a Restart recovery action and (via
    // `set_failure_actions_on_non_crash_failures`) let a non-zero clean Stop
    // trip it. This recovery path is now the **crash safety net**, not the
    // normal restart route: a deliberate restart takes the clean
    // exit-0 + self-relauncher path in `service_main` (no event-log "failure",
    // no backoff). So the delays here only ever apply to genuine crash loops
    // — and we keep them SHORT and flat (1s/2s/2s) with a short reset window
    // so even the fallback path (relauncher couldn't spawn) restarts in ~1s
    // instead of the old escalating 1s/5s/30s that pinned rapid restarts at
    // 30 s. See design-docs/install-and-supervisor.md.
    let failure_actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(120)),
        reboot_msg:   None,
        command:      None,
        actions:      Some(vec![
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(1) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(2) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(2) },
        ]),
    };
    if let Err(e) = service.update_failure_actions(failure_actions) {
        eprintln!("(warning: couldn't set SCM restart/recovery actions: {e} — \
                   the web-UI Restart button may leave the service stopped)");
    }
    if let Err(e) = service.set_failure_actions_on_non_crash_failures(true) {
        eprintln!("(warning: couldn't enable recovery on non-crash exit: {e})");
    }

    if let Err(e) = std::fs::create_dir_all(&inputs.working_dir) {
        eprintln!(
            "(warning: couldn't create working dir {}: {})",
            inputs.working_dir.display(),
            e
        );
    }

    // Web dir wiring via SCM environment is deferred — needs a winreg
    // dep we haven't pulled in yet. The web SPA still resolves when
    // it's next to mira.exe (the default tarball layout); custom
    // paths need MIRA_WEB_DIR set in the service's registry key.
    if inputs.web_dir.is_some() {
        println!(
            "(note: web_dir wiring via SCM environment is deferred — \
             keep web/ next to mira.exe in the install layout)"
        );
    }

    if inputs.enable_now {
        service.start::<&str>(&[]).map_err(box_err)?;
        println!("✓ service started");
    } else {
        println!("(skipped start per --no-enable; use `mira start` to launch)");
    }
    Ok(())
}

/// Register + start the Guardian sentinel as its OWN SCM service
/// (`mira-guardian-watch`), separate from `mira` so it outlives a crash. Its
/// ImagePath carries the `guardian-watch` subcommand (top-level flags first),
/// which the binary detects on an SCM start to route to the sentinel entry
/// ([`try_run_as_guardian_service`]).
pub fn install_guardian(inputs: &InstallInputs) -> Result<(), Box<dyn Error>> {
    let manager = open_manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .map_err(elevation_hint("guardian-install"))?;

    // Top-level flags BEFORE the `guardian-watch` subcommand word.
    let mut launch_args: Vec<OsString> = Vec::new();
    launch_args.push(OsString::from("--config"));
    launch_args.push(inputs.config_path.clone().into_os_string());
    launch_args.push(OsString::from("--data-dir"));
    launch_args.push(inputs.data_dir.clone().into_os_string());
    launch_args.push(OsString::from("guardian-watch"));

    let service_info = ServiceInfo {
        name:             OsString::from(GUARDIAN_SERVICE_NAME),
        display_name:     OsString::from(GUARDIAN_DISPLAY_NAME),
        service_type:     ServiceType::OWN_PROCESS,
        start_type:       ServiceStartType::AutoStart,
        error_control:    ServiceErrorControl::Normal,
        executable_path:  inputs.mira_bin.clone(),
        launch_arguments: launch_args,
        dependencies:     vec![], // intentionally NOT dependent on `mira`
        account_name:     None,   // LocalSystem
        account_password: None,
    };

    if let Ok(existing) = manager.open_service(GUARDIAN_SERVICE_NAME, ServiceAccess::DELETE) {
        eprintln!("(removing stale guardian service registration before re-install)");
        let _ = existing.delete();
    }
    let service = manager
        .create_service(&service_info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .map_err(elevation_hint("guardian-install"))?;
    service.set_description(GUARDIAN_DESCRIPTION).map_err(box_err)?;
    println!("✓ registered Windows service '{GUARDIAN_SERVICE_NAME}'");

    // Crash safety net (Restart=always parity). Unlike the server we do NOT
    // enable recovery-on-non-crash — the sentinel has no deliberate-restart path,
    // so a clean exit 0 (operator Stop) must stay stopped; only an actual crash
    // (non-zero unexpected exit) trips a restart.
    let failure_actions = ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(120)),
        reboot_msg:   None,
        command:      None,
        actions:      Some(vec![
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(2) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) },
            ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(10) },
        ]),
    };
    if let Err(e) = service.update_failure_actions(failure_actions) {
        eprintln!("(warning: couldn't set SCM restart/recovery actions for the sentinel: {e})");
    }

    if inputs.enable_now {
        service.start::<&str>(&[]).map_err(box_err)?;
        println!("✓ sentinel service started");
    } else {
        println!("(skipped start per --no-enable)");
    }
    Ok(())
}

/// Stop + delete the Guardian sentinel SCM service.
pub fn uninstall_guardian() -> Result<(), Box<dyn Error>> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)
        .map_err(elevation_hint("guardian-uninstall"))?;
    let service = match manager.open_service(
        GUARDIAN_SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(s) => s,
        Err(_) => {
            println!("Guardian sentinel service '{GUARDIAN_SERVICE_NAME}' not found — nothing to do.");
            return Ok(());
        }
    };
    // Best-effort stop before delete.
    let _ = service.stop();
    service.delete().map_err(box_err)?;
    println!("✓ deleted Windows service '{GUARDIAN_SERVICE_NAME}'");
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)
        .map_err(elevation_hint("uninstall"))?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        Ok(s) => s,
        Err(_) => {
            println!("MIRA service not registered — nothing to do.");
            return Ok(());
        }
    };
    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            if let Err(e) = service.stop() {
                eprintln!("(warning: stop returned {e}; deleting anyway)");
            }
        }
    }
    service.delete().map_err(box_err)?;
    println!("✓ deleted Windows service '{SERVICE_NAME}'");
    Ok(())
}

pub fn start() -> Result<(), Box<dyn Error>> {
    let svc = open_service(ServiceAccess::START)?;
    svc.start::<&str>(&[]).map_err(box_err)?;
    println!("✓ service started");
    Ok(())
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    let svc = open_service(ServiceAccess::STOP)?;
    svc.stop().map_err(box_err)?;
    println!("✓ service stopped");
    Ok(())
}

pub fn restart() -> Result<(), Box<dyn Error>> {
    let _ = stop();
    // SCM needs a moment to flush the stop state before accepting
    // start; without this, a fast restart hits
    // ERROR_SERVICE_MARKED_FOR_DELETE / a pending-stop conflict.
    std::thread::sleep(Duration::from_millis(750));
    start()
}

pub fn status() -> Result<(), Box<dyn Error>> {
    let svc = open_service(ServiceAccess::QUERY_STATUS)?;
    let status = svc.query_status().map_err(box_err)?;
    println!("Service:       {SERVICE_NAME}");
    println!("Display name:  {DISPLAY_NAME}");
    println!("State:         {:?}", status.current_state);
    println!("Service type:  {:?}", status.service_type);
    println!("Exit code:     {:?}", status.exit_code);
    if let Some(pid) = status.process_id {
        println!("Process id:    {pid}");
    }
    Ok(())
}

fn open_manager(access: ServiceManagerAccess) -> windows_service::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access)
}

fn open_service(access: ServiceAccess) -> Result<windows_service::service::Service, Box<dyn Error>> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)
        .map_err(elevation_hint("control"))?;
    manager
        .open_service(SERVICE_NAME, access)
        .map_err(|e| -> Box<dyn Error> {
            format!(
                "MIRA service not registered (or not accessible): {e}. \
                 Run `mira install` first from an elevated PowerShell."
            )
            .into()
        })
}

// Wrap a windows-service error with an "elevated PowerShell" hint so
// the most common failure mode (running from a non-admin shell) has a
// clear path to recovery rather than an opaque AccessDenied.
fn elevation_hint(op: &'static str) -> impl Fn(windows_service::Error) -> Box<dyn Error> {
    move |e| {
        format!(
            "`mira {op}` failed: {e}. \
             Most commonly this is because PowerShell isn't elevated — \
             re-open it with 'Run as Administrator' and try again."
        )
        .into()
    }
}

fn box_err<E: Error + 'static>(e: E) -> Box<dyn Error> {
    Box::new(e)
}

// ── SCM dispatcher entry point ───────────────────────────────────────────────
//
// SCM launches the binary without a console. The Windows runtime
// expects us to call `StartServiceCtrlDispatcher` within ~30s; that
// happens via [`try_run_as_service`]. main.rs probes this at top of
// main; on success control never returns until SCM stops the service.
// On failure (console launch — the typical dev case) we fall through
// to the normal clap dispatch.

define_windows_service!(ffi_service_main, service_main);

// Try to attach to SCM as a service. Returns `Ok` only when actually
// running under SCM (and only after the service has stopped). On
// console launches returns an error that callers should swallow and
// continue to the regular CLI flow.
pub fn try_run_as_service() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

// Shared shutdown notify that the control handler trips on SCM Stop
// and the gateway awaits. Populated when `service_main` is invoked
// by SCM; not used in console mode.
static SHUTDOWN: std::sync::OnceLock<Arc<tokio::sync::Notify>> = std::sync::OnceLock::new();

// Set by the SCM control handler when SCM asks us to Stop/Shutdown, so
// `service_main` can tell an operator-requested stop (report exit 0 → SCM
// stays stopped) apart from any other reason the server loop returned —
// an app-initiated restart (web-UI button / self-update) or a crash —
// which must report a NON-zero exit so the SCM recovery action relaunches
// us. This is what makes the "exit → supervisor relaunches" contract hold
// on Windows (see [`install`]).
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

// Public accessor for [`crate::gateway::GatewayBuilder`] — the
// gateway's `run_until_shutdown` awaits this notify in addition to
// its built-in ctrl_c / restart-notify sources, so SCM Stop reaches
// the same graceful-shutdown path.
pub fn external_shutdown_notify() -> Option<Arc<tokio::sync::Notify>> {
    SHUTDOWN.get().cloned()
}

/// True once `service_main` has run — i.e. we were launched by the Windows
/// Service Control Manager rather than a console `mira serve`. The status
/// endpoint reports `supervised = true` in this case: SCM's recovery actions
/// (configured at install) relaunch us on the non-zero exit an app-initiated
/// restart produces, so the web-UI Restart button works the same way it does
/// under systemd/launchd. A bare console run leaves the notify unset and is
/// correctly reported as unsupervised.
pub fn is_running_under_scm() -> bool {
    SHUTDOWN.get().is_some()
}

// Env var that marks a process as the post-restart relauncher (see
// `spawn_restart_relauncher` / `maybe_run_relauncher`). Kept private; only the
// service ever sets it on the detached child it spawns.
const RELAUNCH_ENV: &str = "MIRA_WIN_RELAUNCH";

/// Spawn a detached copy of ourselves whose only job is to start the service
/// again once SCM has marked it Stopped (see [`maybe_run_relauncher`]). This is
/// how a **deliberate** restart comes back WITHOUT the non-zero exit that SCM
/// logs as a crash — so `service_main` can report a clean exit 0 and keep the
/// Windows event log quiet. Returns whether the spawn succeeded; the caller
/// falls back to the exit-1 (SCM crash-recovery) relaunch if it didn't, so a
/// restart is never stranded.
fn spawn_restart_relauncher() -> bool {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP — outlive the parent and don't
    // attach to its (nonexistent) console.
    const DETACHED_PROCESS:         u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let Ok(exe) = std::env::current_exe() else { return false };
    std::process::Command::new(exe)
        .env(RELAUNCH_ENV, "1")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .is_ok()
}

/// If this process was launched as the relauncher (env set by
/// [`spawn_restart_relauncher`]), poll until SCM reports the service Stopped,
/// then start it again — retrying to ride out the STOP_PENDING window. Returns
/// `true` when it handled the relaunch (caller should exit immediately and NOT
/// fall through to clap / the service dispatcher). Must run at the very top of
/// `main`, before arg parsing, since the relauncher takes no CLI args.
pub fn maybe_run_relauncher() -> bool {
    if std::env::var_os(RELAUNCH_ENV).is_none() {
        return false;
    }
    if let Ok(mgr) = open_manager(ServiceManagerAccess::CONNECT) {
        // ~60s budget (120 × 500ms) — generous enough for a slow graceful
        // shutdown, bounded so a wedged stop doesn't hang the relauncher.
        for _ in 0..120 {
            if let Ok(svc) = mgr.open_service(
                SERVICE_NAME,
                ServiceAccess::QUERY_STATUS | ServiceAccess::START,
            ) {
                match svc.query_status().map(|s| s.current_state) {
                    // Already back up (SCM beat us, or a previous iteration
                    // started it) — nothing left to do.
                    Ok(ServiceState::Running | ServiceState::StartPending) => return true,
                    // Fully stopped — relaunch and we're done.
                    Ok(ServiceState::Stopped) => { let _ = svc.start::<&str>(&[]); return true; }
                    // Stop still pending (or a transient query error) — wait.
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    true
}

// Where the dispatcher hands control once SCM has connected.
// Registers a control handler that trips [`SHUTDOWN`] on Stop /
// Shutdown, reports Running, then runs the standard server loop.
fn service_main(_args: Vec<OsString>) {
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let _ = SHUTDOWN.set(Arc::clone(&shutdown_notify));

    // Control handler runs on a thread spawned by SCM. We forward Stop
    // Shutdown into the tokio notify; the gateway's shutdown select!
    // picks it up the next tick.
    let status_handle = match service_control_handler::register(SERVICE_NAME, {
        let n = Arc::clone(&shutdown_notify);
        move |control| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    // Mark this as an operator/SCM-requested stop so the exit
                    // path below reports exit 0 (no SCM recovery relaunch).
                    STOP_REQUESTED.store(true, Ordering::SeqCst);
                    n.notify_one();
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        }
    }) {
        Ok(h) => h,
        Err(_) => return,
    };

    // Report Running ASAP — SCM kills us if we miss the start
    // deadline. Server bring-up that actually takes time happens
    // after this.
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type:      ServiceType::OWN_PROCESS,
        current_state:     ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code:         ServiceExitCode::Win32(0),
        checkpoint:        0,
        wait_hint:         Duration::default(),
        process_id:        None,
    });

    // Run the standard `--server` flow on a fresh tokio runtime.
    // Failures get logged to a file next to the binary so an operator
    // has something to read — Windows event log integration is a
    // follow-up.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            // Load the config `mira install` baked into the launch command.
            // `main` wired `--config` into MIRA_CONFIG before handing off here;
            // without this the service would load the default-path config (for
            // LocalSystem, under systemprofile) and ignore the operator's.
            let cfg_override = std::env::var_os("MIRA_CONFIG").map(PathBuf::from);
            let mut config = match crate::config::MiraConfig::load(cfg_override) {
                Ok(c) => c,
                Err(e) => {
                    return Err::<(), Box<dyn Error + Send + Sync>>(
                        format!("config load: {e}").into(),
                    );
                }
            };
            // Put the service's log under its data-dir (next to the databases)
            // so it's findable. The default `~/.mira/logs/mira.log` resolves
            // under the *supervisor account's* `~` (LocalSystem's profile),
            // which is effectively hidden. Only override the default — an
            // operator who set a custom `logging.file` keeps it.
            if config.logging.file == crate::config::default_log_file() {
                let log_path = config.data_dir_path().join("logs").join("mira.log");
                config.logging.file = log_path.to_string_lossy().into_owned();
            }
            // Install file logging before anything else logs. The console
            // `--server`/TUI path does this in `main`; the service entry has to
            // do it itself. Without it the tracing subscriber is never set, no
            // log file is written, and the web UI Logs page hangs on
            // "connecting to log stream" (the stream handler has no file to
            // tail). The stream handler reads the same `logging.file` in this
            // same process, so reader and writer always agree.
            crate::log_filter::init_to_file(
                &config.logging.level,
                &config.log_file_path(),
            );
            let gateway = crate::gateway::GatewayBuilder::new()
                .with_config(Arc::new(config))
                .build()
                .await
                .map_err(|e| -> Box<dyn Error + Send + Sync> {
                    format!("gateway build: {e}").into()
                })?;
            gateway
                .run_until_shutdown()
                .await
                .map_err(|e| -> Box<dyn Error + Send + Sync> {
                    format!("gateway run: {e}").into()
                })
        })
    }));

    if let Err(panic) = &result {
        let _ = std::fs::write(
            std::env::temp_dir().join("mira-service-panic.log"),
            format!("MIRA service panicked: {panic:?}\n"),
        );
    }
    if let Ok(Err(e)) = &result {
        let _ = std::fs::write(
            std::env::temp_dir().join("mira-service.err"),
            format!("MIRA service exited with error: {e}\n"),
        );
    }

    // Decide how this stop is treated by SCM. Three cases:
    //
    // · operator/SCM Stop (STOP_REQUESTED) → exit 0; SCM leaves us Stopped.
    //
    // · deliberate app restart — the server loop returned cleanly (`Ok(Ok(()))`)
    //   for a web-UI Restart / self-update, NOT an SCM stop. We spawn a detached
    //   relauncher and report a CLEAN exit 0, so SCM logs no "terminated
    //   unexpectedly" crash event and applies no backoff. The relauncher starts
    //   us again once we're Stopped. If the relauncher couldn't be spawned, we
    //   fall back to exit 1 so SCM's recovery action still relaunches us — a
    //   restart is never stranded.
    //
    // · crash / error / panic (`Err`/panic) → exit 1 → SCM recovery relaunch.
    //
    // This keeps the Windows event log clean for the common case (deliberate
    // restarts) while preserving the "exit → relaunch" contract for crashes.
    let stop_requested = STOP_REQUESTED.load(Ordering::SeqCst);
    let clean_return   = matches!(&result, Ok(Ok(())));
    let exit_code: u32 = if stop_requested {
        0
    } else if clean_return {
        if spawn_restart_relauncher() { 0 } else { 1 }
    } else {
        1
    };

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type:      ServiceType::OWN_PROCESS,
        current_state:     ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code:         ServiceExitCode::Win32(exit_code),
        checkpoint:        0,
        wait_hint:         Duration::default(),
        process_id:        None,
    });
}

// ── Guardian sentinel SCM service entry ──────────────────────────────────────

define_windows_service!(ffi_guardian_service_main, guardian_service_main);

/// Attach to SCM as the GUARDIAN sentinel service. `main` calls this (instead of
/// [`try_run_as_service`]) when our launch args carry `guardian-watch`, so the
/// SAME binary routes an SCM start to the sentinel entry rather than the server.
/// Returns `Ok` only when actually running under SCM; on a console launch it
/// errors and the caller falls through to the normal CLI (`mira guardian-watch`
/// in the foreground).
pub fn try_run_as_guardian_service() -> windows_service::Result<()> {
    service_dispatcher::start(GUARDIAN_SERVICE_NAME, ffi_guardian_service_main)
}

fn guardian_service_main(_args: Vec<OsString>) {
    let shutdown = Arc::new(tokio::sync::Notify::new());

    let status_handle = match service_control_handler::register(GUARDIAN_SERVICE_NAME, {
        let n = Arc::clone(&shutdown);
        move |control| -> ServiceControlHandlerResult {
            match control {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    STOP_REQUESTED.store(true, Ordering::SeqCst);
                    n.notify_one();
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        }
    }) {
        Ok(h) => h,
        Err(_) => return,
    };

    let _ = status_handle.set_service_status(ServiceStatus {
        service_type:      ServiceType::OWN_PROCESS,
        current_state:     ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code:         ServiceExitCode::Win32(0),
        checkpoint:        0,
        wait_hint:         Duration::default(),
        process_id:        None,
    });

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async {
            let cfg_override = std::env::var_os("MIRA_CONFIG").map(PathBuf::from);
            let mut config = crate::config::MiraConfig::load(cfg_override)
                .map_err(|e| -> Box<dyn Error + Send + Sync> { format!("config load: {e}").into() })?;
            // Sentinel log next to the data dir (LocalSystem's `~` is hidden).
            if config.logging.file == crate::config::default_log_file() {
                config.logging.file = config.data_dir_path()
                    .join("logs").join("mira-guardian-watch.log")
                    .to_string_lossy().into_owned();
            }
            crate::log_filter::init_to_file(&config.logging.level, &config.log_file_path());
            let config = Arc::new(config);
            // The sentinel loops until stopped; race it against the SCM Stop notify.
            tokio::select! {
                r = crate::guardian_sentinel::run(Arc::clone(&config)) =>
                    r.map_err(|e| -> Box<dyn Error + Send + Sync> { format!("sentinel: {e}").into() }),
                _ = shutdown.notified() => Ok(()),
            }
        })
    }));

    if let Err(panic) = &result {
        let _ = std::fs::write(
            std::env::temp_dir().join("mira-guardian-panic.log"),
            format!("MIRA-Guardian sentinel panicked: {panic:?}\n"),
        );
    }
    // Operator Stop → exit 0 (stay stopped). Any other exit (sentinel error or
    // panic) → exit 1, so SCM's recovery action relaunches the watch. The
    // sentinel has no deliberate self-restart path, so there's no relauncher.
    let exit_code: u32 = if STOP_REQUESTED.load(Ordering::SeqCst) { 0 } else { 1 };
    let _ = status_handle.set_service_status(ServiceStatus {
        service_type:      ServiceType::OWN_PROCESS,
        current_state:     ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code:         ServiceExitCode::Win32(exit_code),
        checkpoint:        0,
        wait_hint:         Duration::default(),
        process_id:        None,
    });
}
