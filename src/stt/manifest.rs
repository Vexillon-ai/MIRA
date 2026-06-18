// SPDX-License-Identifier: AGPL-3.0-or-later

// src/stt/manifest.rs
//! Pinned download URLs for whisper.cpp ggml model files.
//!
//! Mirrors the role of `tts/manifest.rs`: the user picks a model id from a
//! curated list, the internal backend resolves the id to a download URL,
//! and we fetch it on demand under `<data_dir>/stt/models`.
//!
//! Source of truth is the official `ggerganov/whisper.cpp` huggingface
//! mirror — same URLs that the upstream `models/download-ggml-model.sh`
//! script uses. SHA256s are intentionally empty (advisory only) to match
//! the TTS manifest's bootstrap-friendly stance.

/// One downloadable whisper.cpp model.
#[derive(Debug, Clone)]
pub struct WhisperModel {
    /// Stable id used in config (`tiny.en`, `base.en`, …).
    pub id:          &'static str,
    /// Human-readable label for the settings UI.
    pub label:       &'static str,
    /// BCP-47 tag — `"en"` for English-only `.en` variants, `"multi"` for
    /// the multilingual variants.
    pub language:    &'static str,
    /// Direct download URL.
    pub url:         &'static str,
    /// Hex SHA256 of the file. Empty = skip verification.
    pub sha256:      &'static str,
    /// Approximate on-disk size (MB). Surfaced in the UI so the user knows
    /// how big a download they're committing to.
    pub size_mb:     u32,
}

/// Default model id baked into [`crate::config::SttInternalConfig`]. Picked
/// for being small enough to download quickly on first launch (~150 MB)
/// while still being noticeably more accurate than `tiny.en`.
pub const DEFAULT_MODEL_ID: &str = "base.en";

/// Curated whisper.cpp models offered to the user. We deliberately omit the
/// quantised variants and the v1/v2 large models — they're either marginal
/// improvements over the listed picks or irrelevant for English voice notes.
pub fn curated_models() -> &'static [WhisperModel] {
    &[
        WhisperModel {
            id: "tiny.en",
            label: "tiny.en — fastest, English only (~75 MB)",
            language: "en",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.en.bin",
            sha256: "",
            size_mb: 75,
        },
        WhisperModel {
            id: "tiny",
            label: "tiny — fastest, multilingual (~75 MB)",
            language: "multi",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
            sha256: "",
            size_mb: 75,
        },
        WhisperModel {
            id: "base.en",
            label: "base.en — small, English only (~150 MB)",
            language: "en",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
            sha256: "",
            size_mb: 150,
        },
        WhisperModel {
            id: "base",
            label: "base — small, multilingual (~150 MB)",
            language: "multi",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
            sha256: "",
            size_mb: 150,
        },
        WhisperModel {
            id: "small.en",
            label: "small.en — balanced, English only (~470 MB)",
            language: "en",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin",
            sha256: "",
            size_mb: 470,
        },
        WhisperModel {
            id: "small",
            label: "small — balanced, multilingual (~470 MB)",
            language: "multi",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
            sha256: "",
            size_mb: 470,
        },
        WhisperModel {
            id: "medium.en",
            label: "medium.en — accurate, English only (~1.5 GB)",
            language: "en",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.en.bin",
            sha256: "",
            size_mb: 1500,
        },
        WhisperModel {
            id: "medium",
            label: "medium — accurate, multilingual (~1.5 GB)",
            language: "multi",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
            sha256: "",
            size_mb: 1500,
        },
        WhisperModel {
            id: "large-v3",
            label: "large-v3 — best quality, multilingual (~3 GB)",
            language: "multi",
            url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",
            sha256: "",
            size_mb: 3000,
        },
    ]
}

/// Look up a model by id. Returns `None` when the id is unknown — callers
/// surface that as `SttError::ModelNotInstalled`.
pub fn curated_model(id: &str) -> Option<&'static WhisperModel> {
    curated_models().iter().find(|m| m.id == id)
}

/// File name used on disk for a given model id. Matches whisper.cpp's
/// convention so a manually-placed file is found without renaming.
pub fn model_file_name(id: &str) -> String {
    format!("ggml-{id}.bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_in_curated_list() {
        assert!(curated_model(DEFAULT_MODEL_ID).is_some());
    }

    #[test]
    fn all_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for m in curated_models() {
            assert!(seen.insert(m.id), "duplicate id: {}", m.id);
            assert!(!m.url.is_empty());
            assert!(!m.label.is_empty());
            assert!(m.size_mb > 0);
        }
    }

    #[test]
    fn file_name_matches_whisper_cpp_convention() {
        assert_eq!(model_file_name("base.en"),  "ggml-base.en.bin");
        assert_eq!(model_file_name("large-v3"), "ggml-large-v3.bin");
    }
}
