// SPDX-License-Identifier: AGPL-3.0-or-later

// src/install/chatterbox.rs
//! One-call installer for the Chatterbox AMD Vulkan TTS server (K3 / Q2 #10).
//!
//! Chatterbox ships a Windows PowerShell one-liner that downloads the ~8 MB
//! app binary and fetches the (~1.4 GB, SHA-256-verified) model weights on
//! first launch:
//!
//! ```powershell
//! irm https://github.com/tarekedOz/Chatterbox_AMDVulkan/releases/download/v1/install.ps1 | iex
//! ```
//!
//! We invoke that via `powershell.exe`. On native Windows that's direct; on
//! **WSL2** it's reachable through Windows interop (`powershell.exe` resolves
//! on the WSL PATH), which installs Chatterbox on the Windows side — the
//! correct place for the native-Vulkan build. On plain Linux/macOS there is
//! no Windows installer, so this returns an error pointing at the manual /
//! Docker path.
//!
//! NOT runtime-tested from the Linux/WSL2 dev box — the install is validated
//! on a Windows MIRA install. The command is built against the documented
//! one-liner.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// The pinned installer URL. Kept here (not in config) so the operator can't
/// accidentally point MIRA's "install" button at an arbitrary script.
pub const INSTALL_PS1_URL: &str =
    "https://github.com/tarekedOz/Chatterbox_AMDVulkan/releases/download/v1/install.ps1";

/// True when a Windows installer is reachable: native Windows, or Linux under
/// WSL2 (where `powershell.exe` works via interop).
pub fn installer_available() -> bool {
    if cfg!(target_os = "windows") {
        return true;
    }
    if cfg!(target_os = "linux") && is_wsl() {
        return which_powershell().is_some();
    }
    false
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("microsoft") || v.contains("wsl")
        })
        .unwrap_or(false)
}

/// Locate `powershell.exe` on PATH (present on Windows; on WSL2 via interop).
fn which_powershell() -> Option<String> {
    for name in ["powershell.exe", "powershell"] {
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// Run the Chatterbox PowerShell installer. Returns combined stdout/stderr on
/// success. Long-running (downloads + first-launch model fetch can take
/// minutes) — the caller runs it off the request path / with a generous
/// budget.
pub async fn install() -> Result<String, String> {
    if !installer_available() {
        return Err(
            "The Chatterbox one-click installer is Windows-only (native or WSL2). \
             On Linux/macOS, use the Docker image or build from source — see \
             https://github.com/tarekedOz/Chatterbox_AMDVulkan".to_string()
        );
    }

    let ps = which_powershell().unwrap_or_else(|| "powershell.exe".to_string());
    // `irm <url> | iex` — the documented one-liner. -NoProfile keeps a user's
    // PowerShell profile from interfering; -ExecutionPolicy Bypass lets the
    // downloaded script run without a machine-policy prompt.
    let script = format!("irm {INSTALL_PS1_URL} | iex");

    let out = Command::new(&ps)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        // Generous cap — the first run downloads ~1.4 GB of weights.
        .output();

    let out = tokio::time::timeout(Duration::from_secs(900), out)
        .await
        .map_err(|_| "installer timed out after 15 minutes".to_string())?
        .map_err(|e| format!("failed to launch {ps}: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(format!("{stdout}\n{stderr}").trim().to_string())
    } else {
        Err(format!("installer exited with {}: {}", out.status, stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_url_is_pinned_to_the_repo() {
        assert!(INSTALL_PS1_URL.contains("tarekedOz/Chatterbox_AMDVulkan"));
        assert!(INSTALL_PS1_URL.ends_with("install.ps1"));
    }

    #[test]
    fn installer_unavailable_on_plain_linux() {
        // This test box is Linux; if it's not WSL, the installer must report
        // unavailable. (Under WSL2 with interop it may be available — both
        // outcomes are valid, so only assert the non-WSL branch.)
        if cfg!(target_os = "linux") && !is_wsl() {
            assert!(!installer_available());
        }
    }
}
