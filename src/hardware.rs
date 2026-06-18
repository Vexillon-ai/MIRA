// SPDX-License-Identifier: AGPL-3.0-or-later

// src/hardware.rs
//! Host hardware probe (K2 / Q2 #10).
//!
//! Detects GPU vendor + compute runtimes (CUDA / Vulkan) so MIRA can pick a
//! sensible local TTS path and recommend the right backend for the machine:
//!
//!   * **AMD GPU** → recommend the Chatterbox AMD Vulkan server (very fast on
//!     Radeon/Strix Halo); K3 wires the installer + supervisor around it.
//!   * **NVIDIA / CUDA** → native Kokoro on CUDA (build with the GPU feature).
//!   * **otherwise** → native Kokoro on CPU — still good, just slower.
//!
//! The probe shells out to small, ubiquitous tools (`nvidia-smi`,
//! `vulkaninfo`) and reads Linux sysfs PCI ids; every step degrades to "not
//! found" rather than erroring, so detection never fails — worst case it
//! reports a bare CPU machine. Result is memoised: hardware doesn't change
//! under a running process.

use std::sync::OnceLock;

use serde::Serialize;

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GpuInfo {
    /// Lowercased vendor key: `amd` | `nvidia` | `intel` | `apple` | `unknown`.
    pub vendor: String,
    /// Human-readable adapter name when we can get one (`"Radeon 8060S"`,
    /// `"NVIDIA GeForce RTX 4090"`); empty if only the vendor is known.
    pub name:   String,
    /// Where the entry came from — `nvidia-smi` | `vulkaninfo` | `sysfs` |
    /// `wmic`. Useful for debugging odd machines.
    pub source: String,
}

/// What MIRA suggests for the best local voice on this box. Drives the K3
/// recommendation card and the installer/supervisor decision.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TtsRecommendation {
    /// AMD GPU present — point the user at the Chatterbox AMD Vulkan server.
    ChatterboxVulkan { reason: String },
    /// NVIDIA/CUDA present — native Kokoro on GPU.
    KokoroCuda { reason: String },
    /// No usable GPU acceleration — native Kokoro on CPU (still fine).
    KokoroCpu { reason: String },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HardwareInfo {
    pub os:   String,
    pub arch: String,
    /// True when running under WSL2 — relevant because the Chatterbox Vulkan
    /// path is Windows-native, so a WSL2 MIRA typically talks to a Chatterbox
    /// running on the Windows side rather than managing it locally.
    pub is_wsl: bool,
    /// NVIDIA + a working `nvidia-smi`.
    pub has_cuda:   bool,
    /// A Vulkan loader/tooling is present (libvulkan or `vulkaninfo`).
    pub has_vulkan: bool,
    /// Every GPU adapter we could identify, de-duplicated by (vendor, name).
    pub gpus: Vec<GpuInfo>,
    pub recommendation: TtsRecommendation,
}

impl HardwareInfo {
    /// True if any detected GPU is AMD.
    pub fn has_amd_gpu(&self) -> bool {
        self.gpus.iter().any(|g| g.vendor == "amd")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry — memoised
// ─────────────────────────────────────────────────────────────────────────────

static CACHE: OnceLock<HardwareInfo> = OnceLock::new();

/// Detect (once) and return the host hardware profile. Subsequent calls
/// return the cached result.
pub fn info() -> &'static HardwareInfo {
    CACHE.get_or_init(detect)
}

/// Force a fresh probe, ignoring the cache. Used by tests; production code
/// should call [`info`].
pub fn detect() -> HardwareInfo {
    let os   = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();
    let is_wsl = detect_wsl();

    let mut gpus: Vec<GpuInfo> = Vec::new();

    // NVIDIA via nvidia-smi (also implies a usable CUDA driver stack).
    let nvidia = detect_nvidia();
    let has_cuda = !nvidia.is_empty();
    gpus.extend(nvidia);

    // PCI sysfs (Linux) + platform queries fill in AMD/Intel and any NVIDIA
    // adapters nvidia-smi didn't enumerate.
    gpus.extend(detect_platform_gpus());

    dedup_gpus(&mut gpus);

    let has_vulkan = vulkan_present();

    let has_amd = gpus.iter().any(|g| g.vendor == "amd");
    let recommendation = recommend(has_amd, has_cuda, has_vulkan, is_wsl);

    HardwareInfo { os, arch, is_wsl, has_cuda, has_vulkan, gpus, recommendation }
}

fn recommend(has_amd: bool, has_cuda: bool, has_vulkan: bool, is_wsl: bool) -> TtsRecommendation {
    if has_amd && has_vulkan {
        let mut reason = "AMD GPU with Vulkan detected — the Chatterbox AMD \
            Vulkan TTS server is dramatically faster than CPU on this hardware."
            .to_string();
        if is_wsl {
            reason.push_str(" Running under WSL2: install/run Chatterbox on the \
                Windows side and point MIRA at its URL.");
        }
        return TtsRecommendation::ChatterboxVulkan { reason };
    }
    if has_cuda {
        return TtsRecommendation::KokoroCuda {
            reason: "NVIDIA GPU with CUDA detected — run native Kokoro on the \
                GPU (build with the CUDA feature) for fast, fully-local speech.".into(),
        };
    }
    TtsRecommendation::KokoroCpu {
        reason: "No GPU acceleration detected — native Kokoro on CPU gives \
            natural speech with zero setup; it's just slower than a GPU path.".into(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Probes
// ─────────────────────────────────────────────────────────────────────────────

fn detect_wsl() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    // WSL2 leaves "microsoft"/"WSL" in the kernel version string.
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("microsoft") || v.contains("wsl")
        })
        .unwrap_or(false)
}

fn detect_nvidia() -> Vec<GpuInfo> {
    let out = run("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"]);
    let Some(text) = out else { return Vec::new() };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|name| GpuInfo {
            vendor: "nvidia".into(),
            name:   name.to_string(),
            source: "nvidia-smi".into(),
        })
        .collect()
}

/// Linux: read PCI vendor ids from sysfs DRM nodes. Windows: query the video
/// controllers via `wmic`. Anything else: empty.
fn detect_platform_gpus() -> Vec<GpuInfo> {
    if cfg!(target_os = "linux") {
        detect_linux_sysfs_gpus()
    } else if cfg!(target_os = "windows") {
        detect_windows_gpus()
    } else {
        Vec::new()
    }
}

fn detect_linux_sysfs_gpus() -> Vec<GpuInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return out };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Only the card nodes (card0, card1, …), not the connector outputs
        // (card0-DP-1) or render nodes.
        if !(name.starts_with("card") && name[4..].chars().all(|c| c.is_ascii_digit()) && name.len() > 4) {
            continue;
        }
        let vendor_path = entry.path().join("device").join("vendor");
        let Ok(vid) = std::fs::read_to_string(&vendor_path) else { continue };
        let vendor = pci_vendor_name(vid.trim());
        if vendor == "unknown" { continue; }
        out.push(GpuInfo { vendor: vendor.into(), name: String::new(), source: "sysfs".into() });
    }
    out
}

fn detect_windows_gpus() -> Vec<GpuInfo> {
    // `wmic` is deprecated but present on virtually every Windows install and
    // needs no PowerShell quoting gymnastics.
    let Some(text) = run("wmic", &["path", "win32_VideoController", "get", "name"]) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("Name"))
        .map(|name| GpuInfo {
            vendor: vendor_from_name(name),
            name:   name.to_string(),
            source: "wmic".into(),
        })
        .collect()
}

/// Map a PCI vendor id (hex string like `0x1002`) to a vendor key.
fn pci_vendor_name(id: &str) -> &'static str {
    match id.trim_start_matches("0x").to_ascii_lowercase().as_str() {
        "1002" | "1022" => "amd",     // ATI / AMD
        "10de"          => "nvidia",
        "8086"          => "intel",
        "106b"          => "apple",
        _               => "unknown",
    }
}

/// Best-effort vendor guess from an adapter name string.
fn vendor_from_name(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    if n.contains("nvidia") || n.contains("geforce") || n.contains("quadro") || n.contains("tesla") {
        "nvidia".into()
    } else if n.contains("amd") || n.contains("radeon") || n.contains("ati ") {
        "amd".into()
    } else if n.contains("intel") {
        "intel".into()
    } else if n.contains("apple") {
        "apple".into()
    } else {
        "unknown".into()
    }
}

/// Whether a Vulkan loader or tooling is present. We don't need it to *work*
/// here — just to know the stack exists so the recommendation makes sense.
fn vulkan_present() -> bool {
    if run("vulkaninfo", &["--summary"]).is_some() {
        return true;
    }
    if cfg!(target_os = "linux") {
        // Loader on PATH-independent library lookup.
        if let Some(text) = run("ldconfig", &["-p"]) {
            return text.contains("libvulkan.so");
        }
    }
    false
}

fn dedup_gpus(gpus: &mut Vec<GpuInfo>) {
    let mut seen = std::collections::HashSet::new();
    gpus.retain(|g| seen.insert((g.vendor.clone(), g.name.clone())));
}

/// Run a command and capture trimmed stdout, or `None` if the binary is
/// missing or it exits non-zero. Never panics.
fn run(bin: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_vendor_mapping() {
        assert_eq!(pci_vendor_name("0x1002"), "amd");
        assert_eq!(pci_vendor_name("0x10de"), "nvidia");
        assert_eq!(pci_vendor_name("0x8086"), "intel");
        assert_eq!(pci_vendor_name("0xbeef"), "unknown");
    }

    #[test]
    fn vendor_from_name_guesses() {
        assert_eq!(vendor_from_name("NVIDIA GeForce RTX 4090"), "nvidia");
        assert_eq!(vendor_from_name("AMD Radeon 8060S Graphics"), "amd");
        assert_eq!(vendor_from_name("Intel(R) Iris(R) Xe"), "intel");
        assert_eq!(vendor_from_name("Some Mystery GPU"), "unknown");
    }

    #[test]
    fn recommend_prefers_chatterbox_for_amd() {
        let r = recommend(true, false, true, false);
        assert!(matches!(r, TtsRecommendation::ChatterboxVulkan { .. }));
    }

    #[test]
    fn recommend_wsl_amd_mentions_windows_side() {
        let r = recommend(true, true, true, true);
        // AMD wins even when CUDA is also somehow present, and the WSL note
        // is appended.
        match r {
            TtsRecommendation::ChatterboxVulkan { reason } => {
                assert!(reason.to_lowercase().contains("windows"));
            }
            other => panic!("expected ChatterboxVulkan, got {other:?}"),
        }
    }

    #[test]
    fn recommend_cuda_when_nvidia_only() {
        assert!(matches!(recommend(false, true, false, false), TtsRecommendation::KokoroCuda { .. }));
    }

    #[test]
    fn recommend_cpu_fallback() {
        assert!(matches!(recommend(false, false, false, false), TtsRecommendation::KokoroCpu { .. }));
    }

    #[test]
    fn detect_never_panics_and_is_self_consistent() {
        let hw = detect();
        assert!(!hw.os.is_empty());
        assert!(!hw.arch.is_empty());
        // has_amd_gpu agrees with the gpus list.
        assert_eq!(hw.has_amd_gpu(), hw.gpus.iter().any(|g| g.vendor == "amd"));
    }

    #[test]
    fn dedup_collapses_identical_entries() {
        let mut v = vec![
            GpuInfo { vendor: "amd".into(), name: "X".into(), source: "a".into() },
            GpuInfo { vendor: "amd".into(), name: "X".into(), source: "b".into() },
            GpuInfo { vendor: "amd".into(), name: "Y".into(), source: "a".into() },
        ];
        dedup_gpus(&mut v);
        assert_eq!(v.len(), 2);
    }
}
