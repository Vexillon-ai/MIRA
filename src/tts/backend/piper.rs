// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/backend/piper.rs
//! Piper TTS backend.
//!
//! Runs the upstream `piper` executable as a subprocess, one invocation per
//! synthesise call (v1 — long-lived stdin-JSON mode arrives with 's
//! true streaming). The binary and the per-voice ONNX/JSON pair are
//! downloaded on first use into `<data_dir>/tts/{piper,voices}/` so the
//! out-of-the-box experience needs zero manual setup.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::info;

use crate::tts::backend::TtsBackend;
use crate::tts::manifest::{
    self, ArchiveKind, DEFAULT_VOICE_ID, curated_voice, curated_voices,
};
use crate::tts::types::{
    AudioBuffer, AudioChunk, AudioCodec, ProbeResult, SynthesiseRequest, TtsError, Voice,
};

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

// Filesystem layout for a Piper install.  wires the user-facing
// `tts.internal.*` config into this struct;  just needs a clean
// constructor for the CLI and tests.
#[derive(Debug, Clone)]
pub struct PiperConfig {
    // `<data_dir>/tts/piper` — root of the unpacked Piper distribution.
    pub install_root:  PathBuf,
    // `<data_dir>/tts/voices` — flat directory of `<voice>.onnx` +
    // `<voice>.onnx.json` pairs.
    pub voices_dir:    PathBuf,
    // User-supplied path to a Piper executable. When `Some`, skips the
    // auto-download path (used by power users with a system install).
    pub binary_path:   Option<PathBuf>,
    pub default_voice: String,
    pub auto_download: bool,
}

impl PiperConfig {
    // Standard layout under a config-resolved data dir
    // (typically `MiraConfig::data_dir_path()`).
    pub fn under_data_dir(data_dir: &Path) -> Self {
        let tts_root = data_dir.join("tts");
        Self {
            install_root:  tts_root.join("piper"),
            voices_dir:    tts_root.join("voices"),
            binary_path:   None,
            default_voice: DEFAULT_VOICE_ID.to_string(),
            auto_download: true,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backend
// ─────────────────────────────────────────────────────────────────────────────

pub struct PiperBackend {
    cfg:    PiperConfig,
    // Cached binary path after the first successful `ensure_binary`. Held
    // behind a mutex so concurrent first-callers serialise the install.
    binary: Mutex<Option<PathBuf>>,
    http:   reqwest::Client,
}

impl PiperBackend {
    pub fn new(cfg: PiperConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { cfg, binary: Mutex::new(None), http }
    }

    pub fn config(&self) -> &PiperConfig { &self.cfg }

    // Path to the Piper executable, downloading and unpacking the pinned
    // release if necessary. The first call may take several seconds.
    pub async fn ensure_binary(&self) -> Result<PathBuf, TtsError> {
        if let Some(p) = &self.cfg.binary_path {
            if p.exists() {
                return Ok(p.clone());
            }
            return Err(TtsError::BackendUnavailable(
                "piper".into(),
                format!("configured binary_path does not exist: {}", p.display()),
            ));
        }

        if let Some(p) = self.binary.lock().await.clone() {
            if p.exists() { return Ok(p); }
        }

        let bin = manifest::piper_for_host().ok_or_else(|| TtsError::BackendUnavailable(
            "piper".into(),
            format!("no pinned Piper binary for host {}/{}",
                std::env::consts::OS, std::env::consts::ARCH),
        ))?;

        let final_path = self.cfg.install_root.join(bin.binary_path_in_archive);
        if final_path.exists() {
            ensure_executable(&final_path).await?;
            *self.binary.lock().await = Some(final_path.clone());
            return Ok(final_path);
        }

        if !self.cfg.auto_download {
            return Err(TtsError::BackendUnavailable(
                "piper".into(),
                "Piper binary not present and auto_download is disabled".into(),
            ));
        }

        info!("Downloading Piper {} from {}", manifest::PIPER_VERSION, bin.url);
        fs::create_dir_all(&self.cfg.install_root).await?;
        let archive = self.download_with_verify(bin.url, bin.sha256).await?;
        extract_archive(&archive, bin.archive_kind, &self.cfg.install_root)?;

        if !final_path.exists() {
            return Err(TtsError::BackendUnavailable(
                "piper".into(),
                format!("expected binary not found after extraction: {}", final_path.display()),
            ));
        }
        ensure_executable(&final_path).await?;

        *self.binary.lock().await = Some(final_path.clone());
        info!("Piper installed at {}", final_path.display());
        Ok(final_path)
    }

    // Path to a voice's `.onnx` model, downloading the (`.onnx`, `.onnx.json`)
    // pair if necessary. Only voices in the curated manifest can be
    // auto-downloaded; uncurated ids must be placed in `voices_dir` manually.
    pub async fn ensure_voice_path(&self, voice_id: &str) -> Result<PathBuf, TtsError> {
        let onnx = self.cfg.voices_dir.join(format!("{voice_id}.onnx"));
        let json = self.cfg.voices_dir.join(format!("{voice_id}.onnx.json"));
        if onnx.exists() && json.exists() {
            return Ok(onnx);
        }
        if !self.cfg.auto_download {
            return Err(TtsError::VoiceNotInstalled(voice_id.into()));
        }
        let v = curated_voice(voice_id)
            .ok_or_else(|| TtsError::VoiceNotInstalled(voice_id.into()))?;

        info!("Downloading Piper voice {voice_id}");
        fs::create_dir_all(&self.cfg.voices_dir).await?;

        let onnx_bytes = self.download_with_verify(v.onnx_url, v.onnx_sha256).await?;
        write_file(&onnx, &onnx_bytes).await?;
        let json_bytes = self.download_with_verify(v.config_url, v.config_sha256).await?;
        write_file(&json, &json_bytes).await?;
        Ok(onnx)
    }

    async fn download_with_verify(
        &self,
        url:    &str,
        sha256: &str,
    ) -> Result<Vec<u8>, TtsError> {
        let resp = self.http.get(url).send().await?.error_for_status()?;
        let bytes = resp.bytes().await?.to_vec();
        if !sha256.is_empty() {
            let mut h = Sha256::new();
            h.update(&bytes);
            let actual = hex::encode(h.finalize());
            if !actual.eq_ignore_ascii_case(sha256) {
                return Err(TtsError::BackendUnavailable(
                    "piper".into(),
                    format!("SHA256 mismatch for {url}: expected {sha256}, got {actual}"),
                ));
            }
        }
        Ok(bytes)
    }

    async fn run_once(
        &self,
        binary: &Path,
        voice:  &Path,
        text:   &str,
        speed:  f32,
    ) -> Result<Vec<u8>, TtsError> {
        // Piper's `length_scale` is the inverse of speech rate: 1.0 = normal,
        // <1.0 = faster. Clamp to a sane band so a slider that spans 0.5..=2.0
        // never sends a wildly distorted value.
        let length_scale = (1.0_f32 / speed.clamp(0.25, 4.0)).clamp(0.25, 4.0);

        // Piper reads one utterance per stdin line. Collapse internal newlines
        // so a multi-paragraph chunk produces one continuous WAV instead of
        // several concatenated headers.
        let single_line = text.replace(['\r', '\n'], " ");

        let mut child = Command::new(binary)
            .arg("--model").arg(voice)
            .arg("--output_file").arg("-")
            .arg("--length_scale").arg(format!("{length_scale}"))
            .stdin (Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| TtsError::BackendUnavailable(
                "piper".into(), format!("spawn failed: {e}"),
            ))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(single_line.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let out = child.wait_with_output().await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(TtsError::Upstream(format!("piper exited with {}: {stderr}", out.status)));
        }
        Ok(out.stdout)
    }
}

#[async_trait]
impl TtsBackend for PiperBackend {
    fn id(&self) -> &'static str { "piper" }

    async fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        let mut out = Vec::with_capacity(curated_voices().len());
        for v in curated_voices() {
            let onnx = self.cfg.voices_dir.join(format!("{}.onnx",      v.id));
            let json = self.cfg.voices_dir.join(format!("{}.onnx.json", v.id));
            out.push(Voice {
                backend_id:    "piper".into(),
                id:            v.id.into(),
                name:          v.name.into(),
                language:      v.language.into(),
                gender:        v.gender.map(str::to_string),
                sample_rate:   Some(v.sample_rate),
                is_downloaded: onnx.exists() && json.exists(),
            });
        }
        Ok(out)
    }

    async fn synthesise(&self, req: &SynthesiseRequest) -> Result<AudioBuffer, TtsError> {
        if req.text.trim().is_empty() {
            return Err(TtsError::BadRequest("text is empty".into()));
        }
        let binary   = self.ensure_binary().await?;
        let voice_id = req.voice_id.clone().unwrap_or_else(|| self.cfg.default_voice.clone());
        let voice    = self.ensure_voice_path(&voice_id).await?;

        let bytes = self.run_once(&binary, &voice, &req.text, req.speed).await?;
        let sample_rate = curated_voice(&voice_id).map(|v| v.sample_rate).unwrap_or(22_050);
        Ok(AudioBuffer {
            bytes,
            codec: AudioCodec::Wav { sample_rate, channels: 1 },
        })
    }

    async fn synthesise_stream(
        &self,
        req: &SynthesiseRequest,
    ) -> Result<BoxStream<'static, Result<AudioChunk, TtsError>>, TtsError> {
        // full-buffer wrapped as a single final chunk. The sentence
        // chunker + stdin-JSON streaming pipeline lands in 
        let buf = self.synthesise(req).await?;
        let chunk = AudioChunk { bytes: buf.bytes, codec: buf.codec, is_final: true };
        Ok(stream::once(async move { Ok(chunk) }).boxed())
    }

    async fn probe(&self) -> Result<ProbeResult, TtsError> {
        let start = Instant::now();
        match self.synthesise(&SynthesiseRequest::new("Hello.")).await {
            Ok(_) => Ok(ProbeResult {
                healthy:    true,
                latency_ms: Some(start.elapsed().as_millis() as u64),
                note:       Some(format!("Piper {}", manifest::PIPER_VERSION)),
            }),
            Err(e) => Ok(ProbeResult {
                healthy:    false,
                latency_ms: None,
                note:       Some(e.to_string()),
            }),
        }
    }

    async fn ensure_voice(&self, voice_id: &str) -> Result<(), TtsError> {
        // Make sure the binary is present before fetching voices — otherwise
        // a successful download still leaves the user without a synthesiser
        // and the next speak fails noisily.
        self.ensure_binary().await?;
        self.ensure_voice_path(voice_id).await.map(|_| ())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

async fn write_file(path: &Path, bytes: &[u8]) -> Result<(), TtsError> {
    if let Some(parent) = path.parent() { fs::create_dir_all(parent).await?; }
    fs::write(path, bytes).await?;
    Ok(())
}

#[cfg(unix)]
async fn ensure_executable(path: &Path) -> Result<(), TtsError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).await?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn ensure_executable(_path: &Path) -> Result<(), TtsError> { Ok(()) }

fn extract_archive(bytes: &[u8], kind: ArchiveKind, dest: &Path) -> Result<(), TtsError> {
    match kind {
        ArchiveKind::TarGz => {
            let gz = flate2::read::GzDecoder::new(bytes);
            tar::Archive::new(gz).unpack(dest).map_err(|e| TtsError::BackendUnavailable(
                "piper".into(), format!("tar.gz extract failed: {e}"),
            ))?;
            Ok(())
        }
        ArchiveKind::Zip => Err(TtsError::BackendUnavailable(
            "piper".into(),
            "ZIP extraction not built in this binary — install Piper manually \
             and set tts.internal.binary_path".into(),
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cfg(root: &Path) -> PiperConfig {
        let mut c = PiperConfig::under_data_dir(root);
        c.auto_download = false;     // tests must never reach the network
        c
    }

    #[test]
    fn under_data_dir_paths() {
        let cfg = PiperConfig::under_data_dir(Path::new("/tmp/mira-data"));
        assert_eq!(cfg.install_root,  Path::new("/tmp/mira-data/tts/piper"));
        assert_eq!(cfg.voices_dir,    Path::new("/tmp/mira-data/tts/voices"));
        assert_eq!(cfg.default_voice, DEFAULT_VOICE_ID);
        assert!(cfg.auto_download);
        assert!(cfg.binary_path.is_none());
    }

    #[test]
    fn backend_id() {
        let dir = tempdir().unwrap();
        let b = PiperBackend::new(cfg(dir.path()));
        assert_eq!(b.id(), "piper");
    }

    #[tokio::test]
    async fn list_voices_marks_undownloaded() {
        let dir = tempdir().unwrap();
        let b = PiperBackend::new(cfg(dir.path()));
        let voices = b.list_voices().await.unwrap();
        assert!(!voices.is_empty(), "curated list must surface");
        assert!(voices.iter().all(|v| !v.is_downloaded),
            "fresh data dir → nothing downloaded");
        assert!(voices.iter().all(|v| v.backend_id == "piper"));
    }

    #[tokio::test]
    async fn list_voices_marks_downloaded_pair() {
        let dir = tempdir().unwrap();
        let b = PiperBackend::new(cfg(dir.path()));
        // Plant the (.onnx,.onnx.json) pair for the default voice.
        fs::create_dir_all(b.cfg.voices_dir.as_path()).await.unwrap();
        let stem = b.cfg.voices_dir.join(format!("{DEFAULT_VOICE_ID}.onnx"));
        let json = b.cfg.voices_dir.join(format!("{DEFAULT_VOICE_ID}.onnx.json"));
        fs::write(&stem, b"x").await.unwrap();
        fs::write(&json, b"{}").await.unwrap();

        let voices = b.list_voices().await.unwrap();
        let amy = voices.iter().find(|v| v.id == DEFAULT_VOICE_ID).unwrap();
        assert!(amy.is_downloaded);
    }

    #[tokio::test]
    async fn synthesise_rejects_empty_text() {
        let dir = tempdir().unwrap();
        let b = PiperBackend::new(cfg(dir.path()));
        let err = b.synthesise(&SynthesiseRequest::new("   ")).await.unwrap_err();
        assert!(matches!(err, TtsError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn synthesise_errors_when_voice_missing_and_no_download() {
        let dir = tempdir().unwrap();
        let mut c = PiperConfig::under_data_dir(dir.path());
        c.auto_download = false;
        // Plant a fake binary so ensure_binary succeeds without network.
        fs::create_dir_all(c.install_root.join("piper")).await.unwrap();
        let bin_path = c.install_root.join("piper/piper");
        fs::write(&bin_path, b"#!/bin/sh\necho fake\n").await.unwrap();
        c.binary_path = Some(bin_path);

        let b = PiperBackend::new(c);
        let err = b.synthesise(&SynthesiseRequest::new("hello")).await.unwrap_err();
        assert!(matches!(err, TtsError::VoiceNotInstalled(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn ensure_binary_uses_configured_path_when_present() {
        let dir = tempdir().unwrap();
        let mut c = PiperConfig::under_data_dir(dir.path());
        let custom = dir.path().join("my-piper");
        fs::write(&custom, b"#!/bin/sh\n").await.unwrap();
        c.binary_path = Some(custom.clone());

        let b = PiperBackend::new(c);
        assert_eq!(b.ensure_binary().await.unwrap(), custom);
    }

    #[tokio::test]
    async fn ensure_binary_errors_when_configured_path_missing() {
        let dir = tempdir().unwrap();
        let mut c = PiperConfig::under_data_dir(dir.path());
        c.binary_path = Some(dir.path().join("does-not-exist"));

        let b = PiperBackend::new(c);
        let err = b.ensure_binary().await.unwrap_err();
        assert!(matches!(err, TtsError::BackendUnavailable(..)), "got {err:?}");
    }

    #[tokio::test]
    async fn probe_reports_unhealthy_without_binary() {
        let dir = tempdir().unwrap();
        let b = PiperBackend::new(cfg(dir.path()));
        let p = b.probe().await.unwrap();
        assert!(!p.healthy);
        assert!(p.latency_ms.is_none());
        assert!(p.note.is_some());
    }
}
