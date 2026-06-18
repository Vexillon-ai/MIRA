// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/manifest.rs
//! Pinned URLs for downloadable TTS artefacts: the Piper binary release for
//! each host arch, and a curated starter list of Piper voices.
//!
//! v1 ships Piper as a managed subprocess (see Section 2.3 of the design
//! doc), so we only need the host-side binary and the per-voice ONNX +
//! config pair. SHA256 fields are advisory — empty means "skip verification"
//! so dev/bootstrap works before the hashes are baked in.

// ─────────────────────────────────────────────────────────────────────────────
// Piper host binary
// ─────────────────────────────────────────────────────────────────────────────

/// Piper release we pin against. Bump together with the per-host URLs and
/// SHA256s when refreshing.
pub const PIPER_VERSION: &str = "2023.11.14-2";

/// How to unpack a downloaded artefact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind { TarGz, Zip }

/// Description of a downloadable Piper binary archive.
#[derive(Debug, Clone)]
pub struct PiperBinary {
    /// Direct download URL.
    pub url:                    &'static str,
    /// Hex-encoded SHA256 of the archive. Empty = skip verification.
    pub sha256:                 &'static str,
    pub archive_kind:           ArchiveKind,
    /// Relative path to the executable inside the unpacked archive.
    pub binary_path_in_archive: &'static str,
}

/// Pick the Piper archive matching the current host. Returns `None` on
/// architectures Piper does not ship a prebuilt binary for — the runtime
/// then falls back to eSpeak NG.
pub fn piper_for_host() -> Option<PiperBinary> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(PiperBinary {
            url: "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_x86_64.tar.gz",
            sha256: "",
            archive_kind: ArchiveKind::TarGz,
            binary_path_in_archive: "piper/piper",
        })
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(PiperBinary {
            url: "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_aarch64.tar.gz",
            sha256: "",
            archive_kind: ArchiveKind::TarGz,
            binary_path_in_archive: "piper/piper",
        })
    } else if cfg!(all(target_os = "linux", target_arch = "arm")) {
        Some(PiperBinary {
            url: "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_armv7l.tar.gz",
            sha256: "",
            archive_kind: ArchiveKind::TarGz,
            binary_path_in_archive: "piper/piper",
        })
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(PiperBinary {
            url: "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_macos_aarch64.tar.gz",
            sha256: "",
            archive_kind: ArchiveKind::TarGz,
            binary_path_in_archive: "piper/piper",
        })
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(PiperBinary {
            url: "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_macos_x64.tar.gz",
            sha256: "",
            archive_kind: ArchiveKind::TarGz,
            binary_path_in_archive: "piper/piper",
        })
    } else if cfg!(target_os = "windows") {
        Some(PiperBinary {
            url: "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_windows_amd64.zip",
            sha256: "",
            archive_kind: ArchiveKind::Zip,
            binary_path_in_archive: "piper/piper.exe",
        })
    } else {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Voices
// ─────────────────────────────────────────────────────────────────────────────

/// Description of one downloadable Piper voice (an ONNX model + its JSON
/// config). Hosted on `huggingface.co/rhasspy/piper-voices`.
#[derive(Debug, Clone)]
pub struct VoiceManifest {
    pub id:            &'static str,
    pub name:          &'static str,
    pub language:      &'static str,
    pub gender:        Option<&'static str>,
    pub sample_rate:   u32,
    pub onnx_url:      &'static str,
    pub onnx_sha256:   &'static str,
    pub config_url:    &'static str,
    pub config_sha256: &'static str,
}

/// Voice we download on first run when no other voice is configured.
pub const DEFAULT_VOICE_ID: &str = "en_US-amy-medium";

/// The default voice — small, female, American English, ~63 MB pair.
pub fn default_voice() -> VoiceManifest {
    VoiceManifest {
        id:            "en_US-amy-medium",
        name:          "Amy (US English, medium)",
        language:      "en-US",
        gender:        Some("female"),
        sample_rate:   22_050,
        onnx_url:      "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium/en_US-amy-medium.onnx",
        onnx_sha256:   "",
        config_url:    "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium/en_US-amy-medium.onnx.json",
        config_sha256: "",
    }
}

/// Curated starter list shown in the settings dropdown. Users can install
/// any voice from `huggingface.co/rhasspy/piper-voices`; this is just the
/// "good defaults" set for first-run discoverability.
pub fn curated_voices() -> Vec<VoiceManifest> {
    vec![
        default_voice(),
        VoiceManifest {
            id:            "en_US-ryan-medium",
            name:          "Ryan (US English, medium)",
            language:      "en-US",
            gender:        Some("male"),
            sample_rate:   22_050,
            onnx_url:      "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/ryan/medium/en_US-ryan-medium.onnx",
            onnx_sha256:   "",
            config_url:    "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/ryan/medium/en_US-ryan-medium.onnx.json",
            config_sha256: "",
        },
        VoiceManifest {
            id:            "en_GB-alan-medium",
            name:          "Alan (British English, medium)",
            language:      "en-GB",
            gender:        Some("male"),
            sample_rate:   22_050,
            onnx_url:      "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/alan/medium/en_GB-alan-medium.onnx",
            onnx_sha256:   "",
            config_url:    "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/alan/medium/en_GB-alan-medium.onnx.json",
            config_sha256: "",
        },
        VoiceManifest {
            id:            "en_GB-jenny_dioco-medium",
            name:          "Jenny (British English, medium)",
            language:      "en-GB",
            gender:        Some("female"),
            sample_rate:   22_050,
            onnx_url:      "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/jenny_dioco/medium/en_GB-jenny_dioco-medium.onnx",
            onnx_sha256:   "",
            config_url:    "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/jenny_dioco/medium/en_GB-jenny_dioco-medium.onnx.json",
            config_sha256: "",
        },
        VoiceManifest {
            id:            "de_DE-thorsten-medium",
            name:          "Thorsten (German, medium)",
            language:      "de-DE",
            gender:        Some("male"),
            sample_rate:   22_050,
            onnx_url:      "https://huggingface.co/rhasspy/piper-voices/resolve/main/de/de_DE/thorsten/medium/de_DE-thorsten-medium.onnx",
            onnx_sha256:   "",
            config_url:    "https://huggingface.co/rhasspy/piper-voices/resolve/main/de/de_DE/thorsten/medium/de_DE-thorsten-medium.onnx.json",
            config_sha256: "",
        },
    ]
}

/// Look up a curated voice by id. Returns `None` for voices the user
/// installs from outside the curated list.
pub fn curated_voice(id: &str) -> Option<VoiceManifest> {
    curated_voices().into_iter().find(|v| v.id == id)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_voice_id_matches_constant() {
        assert_eq!(default_voice().id, DEFAULT_VOICE_ID);
    }

    #[test]
    fn curated_voices_include_default() {
        assert!(curated_voices().iter().any(|v| v.id == DEFAULT_VOICE_ID));
    }

    #[test]
    fn curated_voice_ids_unique() {
        let mut ids: Vec<&str> = curated_voices().iter().map(|v| v.id).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate voice id in curated list");
    }

    #[test]
    fn curated_voice_lookup_round_trip() {
        let v = curated_voice(DEFAULT_VOICE_ID).expect("default voice in list");
        assert_eq!(v.language, "en-US");
        assert!(curated_voice("definitely-not-a-voice").is_none());
    }

    #[test]
    fn curated_voice_urls_well_formed() {
        for v in curated_voices() {
            assert!(v.onnx_url.starts_with("https://"),   "{}", v.id);
            assert!(v.onnx_url.ends_with(".onnx"),        "{}", v.id);
            assert!(v.config_url.starts_with("https://"), "{}", v.id);
            assert!(v.config_url.ends_with(".onnx.json"), "{}", v.id);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn piper_binary_pinned_for_supported_host() {
        let bin = piper_for_host()
            .expect("supported host should have a pinned Piper binary");
        assert!(bin.url.contains(PIPER_VERSION),
            "binary URL should reference pinned version: {}", bin.url);
        assert!(!bin.binary_path_in_archive.is_empty());
    }
}
