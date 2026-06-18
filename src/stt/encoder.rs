// SPDX-License-Identifier: AGPL-3.0-or-later

// src/stt/encoder.rs
//! Audio normalisation for the internal whisper.cpp backend.
//!
//! whisper.cpp expects 16 kHz mono `f32` PCM in `[-1.0, 1.0]`. Browsers and
//! phones produce a zoo of containers (WebM/Opus on Chrome+Firefox, MP4/AAC
//! on iOS Safari, WAV from desktop dictation tools, the occasional MP3 or
//! FLAC). We use Symphonia to demux + decode any of those, then Rubato to
//! resample to 16 kHz when needed. All in pure Rust — no ffmpeg dependency.
//!
//! The cloud backends never call this; they hand the raw bytes through to
//! the upstream's own decoder.

use std::io::Cursor;

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL, CODEC_TYPE_OPUS};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::stt::types::{AudioInputFormat, SttError};

/// whisper.cpp's required input rate. Don't change.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Result of decoding one audio container.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// Mono PCM at [`TARGET_SAMPLE_RATE`], in `[-1.0, 1.0]`.
    pub samples:     Vec<f32>,
    pub sample_rate: u32,
    /// Audio length derived from `samples.len() / sample_rate`. Surfaced so
    /// the service can enforce `stt.max_audio_seconds` and the transcript
    /// can report a duration.
    pub duration_ms: u64,
}

/// Decode any container Symphonia recognises down to 16 kHz mono f32 PCM.
///
/// `format_hint` lets the prober skip sniffing when we already know what
/// the client uploaded (helps with WebM/Opus where the magic bytes can be
/// shy in short clips); pass [`AudioInputFormat::Unknown`] when in doubt.
pub fn decode_to_pcm16k(
    bytes: &[u8],
    format_hint: AudioInputFormat,
) -> Result<DecodedAudio, SttError> {
    if bytes.is_empty() {
        return Err(SttError::Decoding("empty audio payload".into()));
    }

    let cursor = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    let ext = format_hint.extension();
    if ext != "bin" {
        hint.with_extension(ext);
    }
    let mime = format_hint.mime();
    if !mime.starts_with("application/") {
        hint.mime_type(mime);
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| SttError::Decoding(format!("probe failed: {e}")))?;
    let mut format = probed.format;

    // Snapshot the track metadata we need *before* the decode loop so we
    // can iterate `format` mutably afterwards without a borrow conflict.
    let (track_id, codec_params) = {
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| SttError::Decoding("no decodable audio track".into()))?;
        (track.id, track.codec_params.clone())
    };

    let src_rate = codec_params
        .sample_rate
        .ok_or_else(|| SttError::Decoding("track has no sample rate".into()))?;
    let src_channels = codec_params
        .channels
        .map(|c| c.count())
        .unwrap_or(1)
        .max(1);

    // Opus is the one common codec Symphonia demuxes but can't *decode*
    // (it has no Opus decoder as of 0.5). Telegram voice notes (OGG/Opus)
    // and Chrome/Firefox web recordings (WebM/Opus) both land here. Decode
    // the demuxed packets with libopus instead — straight to 16 kHz mono, so
    // no resample step is needed.
    if codec_params.codec == CODEC_TYPE_OPUS {
        let mut packets: Vec<Vec<u8>> = Vec::new();
        loop {
            match format.next_packet() {
                Ok(p) => {
                    if p.track_id() == track_id {
                        packets.push(p.data.to_vec());
                    }
                }
                Err(SymphoniaError::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(SymphoniaError::ResetRequired) => continue,
                Err(e) => return Err(SttError::Decoding(format!("read packet: {e}"))),
            }
        }
        let mono = decode_opus_to_16k_mono(packets.into_iter(), src_channels)?;
        if mono.is_empty() {
            return Err(SttError::Decoding("decoded zero samples".into()));
        }
        let duration_ms = (mono.len() as u64).saturating_mul(1000) / TARGET_SAMPLE_RATE as u64;
        return Ok(DecodedAudio { samples: mono, sample_rate: TARGET_SAMPLE_RATE, duration_ms });
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| SttError::Decoding(format!("decoder init failed: {e}")))?;

    // Decode all packets into one mono f32 buffer at the source rate, then
    // resample to 16 kHz at the very end. Doing the rate conversion last
    // gives Rubato the longest contiguous chunk to work with, which costs a
    // little RAM but produces noticeably cleaner output than chunked
    // resampling for these typically-short voice clips.
    let mut mono_src: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // Symphonia signals EOF via an `IoError` with kind UnexpectedEof.
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymphoniaError::ResetRequired) => {
                // Re-init decoder on stream reset (rare, but spec-required).
                decoder = symphonia::default::get_codecs()
                    .make(&codec_params, &DecoderOptions::default())
                    .map_err(|e| SttError::Decoding(format!("decoder reinit failed: {e}")))?;
                continue;
            }
            Err(e) => return Err(SttError::Decoding(format!("read packet: {e}"))),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A single bad packet shouldn't kill the whole transcription —
            // skip and continue.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(SttError::Decoding(format!("decode: {e}"))),
        };

        push_packet_as_mono_f32(&decoded, src_channels, &mut mono_src);
    }

    if mono_src.is_empty() {
        return Err(SttError::Decoding("decoded zero samples".into()));
    }

    let resampled = if src_rate == TARGET_SAMPLE_RATE {
        mono_src
    } else {
        resample_mono(&mono_src, src_rate, TARGET_SAMPLE_RATE)?
    };

    let duration_ms =
        (resampled.len() as u64).saturating_mul(1000) / TARGET_SAMPLE_RATE as u64;

    Ok(DecodedAudio {
        samples: resampled,
        sample_rate: TARGET_SAMPLE_RATE,
        duration_ms,
    })
}

/// Append one decoded Symphonia packet to the running mono buffer, mixing
/// down multi-channel audio by averaging across channels.
fn push_packet_as_mono_f32(buf: &AudioBufferRef<'_>, channels: usize, out: &mut Vec<f32>) {
    macro_rules! mix {
        ($buf:expr, $convert:expr) => {{
            let frames = $buf.frames();
            out.reserve(frames);
            if channels == 1 {
                let ch = $buf.chan(0);
                for s in ch.iter().take(frames) {
                    out.push($convert(*s));
                }
            } else {
                for i in 0..frames {
                    let mut acc = 0.0f32;
                    for c in 0..channels {
                        acc += $convert($buf.chan(c)[i]);
                    }
                    out.push(acc / channels as f32);
                }
            }
        }};
    }

    match buf {
        AudioBufferRef::F32(b) => mix!(b, |s: f32| s),
        AudioBufferRef::F64(b) => mix!(b, |s: f64| s as f32),
        AudioBufferRef::S16(b) => mix!(b, |s: i16| s as f32 / i16::MAX as f32),
        AudioBufferRef::S32(b) => mix!(b, |s: i32| s as f32 / i32::MAX as f32),
        AudioBufferRef::U8(b)  => mix!(b, |s: u8|  (s as f32 - 128.0) / 128.0),
        AudioBufferRef::U16(b) => mix!(b, |s: u16| (s as f32 - 32_768.0) / 32_768.0),
        AudioBufferRef::U32(b) => mix!(b, |s: u32| (s as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32),
        AudioBufferRef::S8(b)  => mix!(b, |s: i8|  s as f32 / i8::MAX as f32),
        AudioBufferRef::S24(b) => mix!(b, |s: symphonia::core::sample::i24| s.inner() as f32 / 8_388_608.0),
        AudioBufferRef::U24(b) => mix!(b, |s: symphonia::core::sample::u24| (s.inner() as f32 - 8_388_608.0) / 8_388_608.0),
    }
}

/// Decode a stream of raw Opus packets (as demuxed from OGG/WebM) to 16 kHz
/// mono f32 PCM using libopus. Opus can decode directly to any of its
/// supported rates, so we ask for 16 kHz and skip the resampler entirely.
/// Multi-channel audio is downmixed by averaging.
fn decode_opus_to_16k_mono(
    packets: impl Iterator<Item = Vec<u8>>,
    src_channels: usize,
) -> Result<Vec<f32>, SttError> {
    use opus::{Channels, Decoder as OpusDecoder};

    let stereo = src_channels >= 2;
    let nch = if stereo { 2usize } else { 1 };
    let mut decoder = OpusDecoder::new(
        TARGET_SAMPLE_RATE,
        if stereo { Channels::Stereo } else { Channels::Mono },
    )
    .map_err(|e| SttError::Decoding(format!("opus decoder init failed: {e}")))?;

    // Largest Opus frame is 120 ms; at 16 kHz that's 1920 samples/channel.
    // Size generously (120 ms @ 48 kHz) so an over-long packet can't overflow.
    let mut pcm = vec![0i16; 5760 * nch];
    let mut mono: Vec<f32> = Vec::new();

    for packet in packets {
        if packet.is_empty() {
            continue;
        }
        let frames = match decoder.decode(&packet, &mut pcm, false) {
            Ok(n) => n,
            // A single corrupt packet shouldn't sink the whole clip.
            Err(_) => continue,
        };
        mono.reserve(frames);
        for f in 0..frames {
            if nch == 1 {
                mono.push(pcm[f] as f32 / 32_768.0);
            } else {
                let l = pcm[f * 2] as f32;
                let r = pcm[f * 2 + 1] as f32;
                mono.push((l + r) * 0.5 / 32_768.0);
            }
        }
    }
    Ok(mono)
}

/// Resample a mono buffer from `src_rate` → `dst_rate` using Rubato's
/// fixed-input sinc resampler. The output length is approximately
/// `samples.len() * dst_rate / src_rate`.
fn resample_mono(samples: &[f32], src_rate: u32, dst_rate: u32) -> Result<Vec<f32>, SttError> {
    let ratio = dst_rate as f64 / src_rate as f64;

    // Process in 1024-frame chunks. Rubato's `SincFixedIn` requires a
    // fixed input length per call; the last (short) chunk gets zero-padded.
    let chunk_size = 1024;
    let params = SincInterpolationParameters {
        sinc_len:        128,
        f_cutoff:        0.95,
        oversampling_factor: 256,
        interpolation:   SincInterpolationType::Linear,
        window:          WindowFunction::BlackmanHarris2,
    };
    let mut resampler =
        SincFixedIn::<f32>::new(ratio, 2.0, params, chunk_size, 1)
            .map_err(|e| SttError::Decoding(format!("resampler init: {e}")))?;

    let mut out: Vec<f32> = Vec::with_capacity((samples.len() as f64 * ratio) as usize + chunk_size);
    let mut idx = 0;
    while idx < samples.len() {
        let end   = (idx + chunk_size).min(samples.len());
        let slice = &samples[idx..end];
        let chunk_input: Vec<f32> = if slice.len() == chunk_size {
            slice.to_vec()
        } else {
            let mut v = slice.to_vec();
            v.resize(chunk_size, 0.0);
            v
        };
        let waves_in  = vec![chunk_input];
        let waves_out = resampler
            .process(&waves_in, None)
            .map_err(|e| SttError::Decoding(format!("resample: {e}")))?;
        out.extend_from_slice(&waves_out[0]);
        idx += chunk_size;
    }

    // Trim the trailing zero-padded tail back to the expected length.
    let expected = (samples.len() as f64 * ratio).round() as usize;
    if out.len() > expected {
        out.truncate(expected);
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal in-memory WAV file (mono, 16-bit PCM) so the decoder
    /// has something deterministic to chew on without bundling a real audio
    /// asset in the repo.
    fn synth_wav(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let byte_rate   = sample_rate * 2;
        let data_bytes  = (samples.len() * 2) as u32;
        let chunk_size  = 36 + data_bytes;

        let mut v = Vec::with_capacity(44 + samples.len() * 2);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&chunk_size.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());     // fmt chunk size
        v.extend_from_slice(&1u16.to_le_bytes());      // PCM
        v.extend_from_slice(&1u16.to_le_bytes());      // mono
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());      // block align
        v.extend_from_slice(&16u16.to_le_bytes());     // bits per sample
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_bytes.to_le_bytes());
        for &s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    #[test]
    fn decode_passes_through_when_already_16k_mono() {
        // 0.1s of silence at 16 kHz = 1600 samples — no resampling needed.
        let wav = synth_wav(16_000, &vec![0i16; 1600]);
        let decoded = decode_to_pcm16k(&wav, AudioInputFormat::Wav).expect("decode");
        assert_eq!(decoded.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(decoded.samples.len(), 1600);
        assert_eq!(decoded.duration_ms, 100);
    }

    #[test]
    fn decode_resamples_44100_down_to_16k() {
        // 0.1s of silence at 44.1 kHz → expected ~1600 samples after resample.
        let wav = synth_wav(44_100, &vec![0i16; 4410]);
        let decoded = decode_to_pcm16k(&wav, AudioInputFormat::Wav).expect("decode");
        assert_eq!(decoded.sample_rate, TARGET_SAMPLE_RATE);
        // Allow ±1 frame for rounding at chunk boundaries.
        assert!(
            (decoded.samples.len() as i64 - 1600).abs() <= 1,
            "got {} samples", decoded.samples.len()
        );
    }

    #[test]
    fn decode_rejects_empty_payload() {
        let err = decode_to_pcm16k(&[], AudioInputFormat::Unknown).unwrap_err();
        assert!(matches!(err, SttError::Decoding(_)));
    }

    #[test]
    fn decode_rejects_garbage_bytes() {
        let err = decode_to_pcm16k(&[0u8; 32], AudioInputFormat::Unknown).unwrap_err();
        assert!(matches!(err, SttError::Decoding(_)));
    }

    #[test]
    fn opus_packets_decode_to_16k_mono() {
        use opus::{Application, Channels, Encoder};
        // Encode 5 × 20 ms mono frames at 16 kHz (320 samples each), then
        // decode them back through the libopus path. This is exactly the
        // codec Telegram voice notes use — the case Symphonia couldn't handle.
        let mut enc = Encoder::new(TARGET_SAMPLE_RATE, Channels::Mono, Application::Voip)
            .expect("opus encoder");
        let frame = vec![0i16; 320];
        let mut packets = Vec::new();
        for _ in 0..5 {
            let mut buf = vec![0u8; 4000];
            let n = enc.encode(&frame, &mut buf).expect("encode");
            buf.truncate(n);
            packets.push(buf);
        }
        let mono = decode_opus_to_16k_mono(packets.into_iter(), 1).expect("decode");
        // 5 × 320 = 1600 samples expected; allow generous slack for Opus's
        // encoder delay / frame quantisation.
        assert!(
            (1400..=1700).contains(&mono.len()),
            "got {} samples",
            mono.len()
        );
    }
}
