// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tts/encoder.rs
//! WAV / PCM → OGG/Opus transcoder used at the channel boundary.
//!
//! Local TTS backends (Piper, eSpeak) emit 16-bit PCM in a WAV wrapper at
//! whatever sample rate their voice was trained for. Messaging channels
//! (Signal, Telegram) only accept OGG/Opus voice notes, so the dispatchers
//! run the synthesised audio through this module before attaching it to the
//! outbound message.
//!
//! Pipeline:
//!   1. Decode the input container down to mono `f32` PCM with Symphonia
//!      (already in the dep tree for STT).
//!   2. Resample to 48 kHz with Rubato — Opus is happiest at 48 kHz mono and
//!      that's what every voice-note client expects.
//!   3. Encode 20 ms frames (960 samples) with libopus at 24 kbps in VOIP
//!      mode — bandwidth that matches Telegram/Signal voice notes and keeps
//!      file sizes small.
//!   4. Pack the frames into the OGG container with the correct OpusHead /
//!      OpusTags headers and granule positions.
//!
//! `ensure_ogg_opus` is the single public entry point. It's a no-op when the
//! input buffer already carries `OggOpus`, so callers can pass anything the
//! backends produce.

use std::io::Cursor;

use ogg::PacketWriter;
use ogg::writing::PacketWriteEndInfo;
use opus::{Application, Channels as OpusChannels, Encoder as OpusEncoder};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::tts::types::{AudioBuffer, AudioCodec, TtsError};

/// Opus's natural sample rate. Frames are sized at this rate.
const OPUS_RATE: u32 = 48_000;
/// 20 ms frames at 48 kHz mono = 960 samples per frame.
const FRAME_SAMPLES: usize = 960;
/// Voice-note bitrate. Matches what Signal / Telegram clients ship.
const TARGET_BITRATE: i32 = 24_000;
/// Stream serial — fixed value is fine since we only ever produce one
/// logical bitstream per buffer.
const STREAM_SERIAL: u32 = 0xDEAD_BEEF;

/// Convert any backend output to OGG/Opus, leaving OGG/Opus untouched.
pub fn ensure_ogg_opus(buf: AudioBuffer) -> Result<AudioBuffer, TtsError> {
    if matches!(buf.codec, AudioCodec::OggOpus) {
        return Ok(buf);
    }
    // Symphonia handles WAV containers natively. Raw PCM needs a synthetic
    // WAV wrapper before we hand it off; cheaper than a second decode path.
    let bytes = match buf.codec {
        AudioCodec::Wav { .. } | AudioCodec::Mp3 => buf.bytes,
        AudioCodec::Pcm { sample_rate, channels } => {
            wrap_pcm_in_wav(&buf.bytes, sample_rate, channels)
        }
        AudioCodec::OggOpus => unreachable!(),
    };

    let (samples, src_rate) = decode_to_mono_f32(&bytes)?;
    let resampled = resample_to_48k(&samples, src_rate)?;
    let ogg_bytes = encode_ogg_opus(&resampled)?;

    Ok(AudioBuffer { bytes: ogg_bytes, codec: AudioCodec::OggOpus })
}

// ── decode ──────────────────────────────────────────────────────────────────

fn decode_to_mono_f32(bytes: &[u8]) -> Result<(Vec<f32>, u32), TtsError> {
    if bytes.is_empty() {
        return Err(TtsError::Encoding("empty audio payload".into()));
    }
    let cursor = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let probed = symphonia::default::get_probe()
        .format(&Hint::new(), mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| TtsError::Encoding(format!("probe failed: {e}")))?;

    let mut format = probed.format;
    let track = format.tracks().iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| TtsError::Encoding("no decodable audio track".into()))?;
    let track_id = track.id;
    let src_rate = track.codec_params.sample_rate
        .ok_or_else(|| TtsError::Encoding("track is missing sample_rate".into()))?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| TtsError::Encoding(format!("decoder build failed: {e}")))?;

    let mut samples: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(TtsError::Encoding(format!("packet read failed: {e}"))),
        };
        if packet.track_id() != track_id { continue; }

        match decoder.decode(&packet) {
            Ok(decoded) => append_mono_f32(decoded, &mut samples),
            Err(SymphoniaError::DecodeError(_)) => continue, // drop bad packets
            Err(e) => return Err(TtsError::Encoding(format!("decode failed: {e}"))),
        }
    }

    Ok((samples, src_rate))
}

fn append_mono_f32(buf: AudioBufferRef<'_>, out: &mut Vec<f32>) {
    macro_rules! mix {
        ($buf:expr, $convert:expr) => {{
            let chans = $buf.spec().channels.count();
            let frames = $buf.frames();
            for f in 0..frames {
                let mut acc = 0.0f32;
                for c in 0..chans {
                    acc += $convert($buf.chan(c)[f]);
                }
                out.push(acc / chans as f32);
            }
        }};
    }
    match buf {
        AudioBufferRef::F32(b) => mix!(b, |x: f32| x),
        AudioBufferRef::S16(b) => mix!(b, |x: i16| x as f32 / i16::MAX as f32),
        AudioBufferRef::S32(b) => mix!(b, |x: i32| x as f32 / i32::MAX as f32),
        AudioBufferRef::U8(b)  => mix!(b, |x: u8|  (x as f32 - 128.0) / 128.0),
        AudioBufferRef::F64(b) => mix!(b, |x: f64| x as f32),
        AudioBufferRef::S24(b) => mix!(b, |x: symphonia::core::sample::i24| x.inner() as f32 / 8_388_608.0),
        AudioBufferRef::U16(b) => mix!(b, |x: u16| (x as f32 - 32_768.0) / 32_768.0),
        AudioBufferRef::U24(b) => mix!(b, |x: symphonia::core::sample::u24| (x.inner() as f32 - 8_388_608.0) / 8_388_608.0),
        AudioBufferRef::U32(b) => mix!(b, |x: u32| (x as f32 - 2_147_483_648.0) / 2_147_483_648.0),
        AudioBufferRef::S8(b)  => mix!(b, |x: i8|  x as f32 / i8::MAX as f32),
    }
}

// ── resample ────────────────────────────────────────────────────────────────

fn resample_to_48k(samples: &[f32], src_rate: u32) -> Result<Vec<f32>, TtsError> {
    if src_rate == OPUS_RATE { return Ok(samples.to_vec()); }
    if samples.is_empty() { return Ok(Vec::new()); }

    let params = SincInterpolationParameters {
        sinc_len:           128,
        f_cutoff:           0.95,
        interpolation:      SincInterpolationType::Linear,
        oversampling_factor: 128,
        window:             WindowFunction::Blackman2,
    };
    let chunk_size = 1024usize;
    let ratio = OPUS_RATE as f64 / src_rate as f64;
    let mut resampler = SincFixedIn::<f32>::new(ratio, 2.0, params, chunk_size, 1)
        .map_err(|e| TtsError::Encoding(format!("resampler init failed: {e}")))?;

    let mut out = Vec::with_capacity((samples.len() as f64 * ratio) as usize + 1024);
    let mut cursor = 0usize;
    while cursor + chunk_size <= samples.len() {
        let chunk = &samples[cursor..cursor + chunk_size];
        let processed = resampler.process(&[chunk], None)
            .map_err(|e| TtsError::Encoding(format!("resample failed: {e}")))?;
        out.extend_from_slice(&processed[0]);
        cursor += chunk_size;
    }
    // Pad-and-flush the trailing remainder so we don't lose the last 20 ms or
    // so of audio. Padding with zeros is fine for voice notes.
    let tail = samples.len() - cursor;
    if tail > 0 {
        let mut last = vec![0.0f32; chunk_size];
        last[..tail].copy_from_slice(&samples[cursor..]);
        let processed = resampler.process(&[&last], None)
            .map_err(|e| TtsError::Encoding(format!("resample tail failed: {e}")))?;
        // Trim the zero-pad's resampled tail so we don't append silence.
        let kept = (tail as f64 * ratio).round() as usize;
        let kept = kept.min(processed[0].len());
        out.extend_from_slice(&processed[0][..kept]);
    }
    Ok(out)
}

// ── encode ──────────────────────────────────────────────────────────────────

fn encode_ogg_opus(samples_48k_mono: &[f32]) -> Result<Vec<u8>, TtsError> {
    let mut enc = OpusEncoder::new(OPUS_RATE, OpusChannels::Mono, Application::Voip)
        .map_err(|e| TtsError::Encoding(format!("opus encoder init: {e}")))?;
    enc.set_bitrate(opus::Bitrate::Bits(TARGET_BITRATE))
        .map_err(|e| TtsError::Encoding(format!("opus bitrate: {e}")))?;
    let pre_skip = enc.get_lookahead()
        .map_err(|e| TtsError::Encoding(format!("opus lookahead: {e}")))? as u16;

    // Convert f32 [-1,1] → i16. Clamp first so peaks don't wrap.
    let pcm_i16: Vec<i16> = samples_48k_mono.iter()
        .map(|x| (x.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect();

    let mut out = Cursor::new(Vec::with_capacity(pcm_i16.len() / 8 + 1024));
    {
        let mut pw = PacketWriter::new(&mut out);

        // ── Header pages ────────────────────────────────────────────────────
        pw.write_packet(
            opus_head_packet(pre_skip),
            STREAM_SERIAL,
            PacketWriteEndInfo::EndPage,
            0,
        ).map_err(|e| TtsError::Encoding(format!("ogg head: {e}")))?;
        pw.write_packet(
            opus_tags_packet(),
            STREAM_SERIAL,
            PacketWriteEndInfo::EndPage,
            0,
        ).map_err(|e| TtsError::Encoding(format!("ogg tags: {e}")))?;

        // ── Audio pages ─────────────────────────────────────────────────────
        let total_frames = pcm_i16.len().div_ceil(FRAME_SAMPLES);
        // ~50 frames per page = ~1 second of audio per page. Keeps pages well
        // under the 64 KB ogg limit while avoiding header overhead per frame.
        const FRAMES_PER_PAGE: usize = 50;

        let mut granule: u64 = 0;
        let mut scratch = vec![0u8; 4000];
        let mut frame_buf = vec![0i16; FRAME_SAMPLES];

        for fi in 0..total_frames {
            let start = fi * FRAME_SAMPLES;
            let end = (start + FRAME_SAMPLES).min(pcm_i16.len());
            // Last frame may be short; zero-pad to a full frame.
            for (dst, src) in frame_buf.iter_mut().zip(pcm_i16[start..end].iter()) {
                *dst = *src;
            }
            for s in &mut frame_buf[end - start..] { *s = 0; }

            let len = enc.encode(&frame_buf, &mut scratch)
                .map_err(|e| TtsError::Encoding(format!("opus encode: {e}")))?;
            let pkt = scratch[..len].to_vec();

            granule = granule.saturating_add(FRAME_SAMPLES as u64);
            let is_last = fi + 1 == total_frames;
            let end_info = if is_last {
                PacketWriteEndInfo::EndStream
            } else if (fi + 1) % FRAMES_PER_PAGE == 0 {
                PacketWriteEndInfo::EndPage
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            pw.write_packet(pkt, STREAM_SERIAL, end_info, granule)
                .map_err(|e| TtsError::Encoding(format!("ogg audio packet: {e}")))?;
        }
    }
    Ok(out.into_inner())
}

fn opus_head_packet(pre_skip: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);                                    // version
    head.push(1);                                    // channel count = mono
    head.extend_from_slice(&pre_skip.to_le_bytes()); // pre-skip
    head.extend_from_slice(&OPUS_RATE.to_le_bytes());// input sample rate (info only)
    head.extend_from_slice(&0u16.to_le_bytes());     // output gain Q7.8
    head.push(0);                                    // channel mapping family = 0 (mono/stereo)
    head
}

fn opus_tags_packet() -> Vec<u8> {
    let vendor = b"mira-tts";
    let mut tags = Vec::with_capacity(16 + vendor.len());
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // user comment list length = 0
    tags
}

// ── PCM → WAV wrapper ───────────────────────────────────────────────────────
//
// Used when a backend reports raw PCM. Wrapping into a minimal 16-bit RIFF
// container is cheaper than writing a second decoder.
fn wrap_pcm_in_wav(pcm: &[u8], sample_rate: u32, channels: u16) -> Vec<u8> {
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;
    let data_size = pcm.len() as u32;
    let chunk_size = 36 + data_size;

    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&chunk_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());      // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes());       // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());      // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a 100 ms 440 Hz sine at 22050 Hz mono, wrapped in a WAV
    /// container — close to what Piper hands us.
    fn synthetic_wav() -> AudioBuffer {
        let sr = 22_050u32;
        let dur_secs = 0.1;
        let n = (sr as f32 * dur_secs) as usize;
        let mut pcm: Vec<u8> = Vec::with_capacity(2 + 2 * n);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.3;
            let s16 = (s * i16::MAX as f32) as i16;
            pcm.extend_from_slice(&s16.to_le_bytes());
        }
        AudioBuffer {
            bytes: wrap_pcm_in_wav(&pcm, sr, 1),
            codec: AudioCodec::Wav { sample_rate: sr, channels: 1 },
        }
    }

    #[test]
    fn passthrough_when_already_ogg_opus() {
        let buf = AudioBuffer { bytes: vec![1, 2, 3], codec: AudioCodec::OggOpus };
        let out = ensure_ogg_opus(buf).unwrap();
        assert!(matches!(out.codec, AudioCodec::OggOpus));
        assert_eq!(out.bytes, vec![1, 2, 3]);
    }

    #[test]
    fn wav_round_trips_to_ogg_opus() {
        let out = ensure_ogg_opus(synthetic_wav()).unwrap();
        assert!(matches!(out.codec, AudioCodec::OggOpus));
        // The output must be a real OGG stream — that means the very first
        // bytes are the "OggS" capture pattern.
        assert!(out.bytes.starts_with(b"OggS"), "missing OggS magic: {:?}", &out.bytes[..8.min(out.bytes.len())]);
        // OpusHead must appear early in the stream.
        let head_idx = out.bytes.windows(8).position(|w| w == b"OpusHead");
        assert!(head_idx.is_some(), "missing OpusHead packet");
        assert!(out.bytes.len() > 100, "output suspiciously small: {}", out.bytes.len());
    }

    #[test]
    fn raw_pcm_input_is_wrapped_and_encoded() {
        // 50 ms of silence at 16 kHz mono.
        let sr = 16_000u32;
        let n = (sr as f32 * 0.05) as usize;
        let pcm = vec![0u8; n * 2];
        let buf = AudioBuffer {
            bytes: pcm,
            codec: AudioCodec::Pcm { sample_rate: sr, channels: 1 },
        };
        let out = ensure_ogg_opus(buf).unwrap();
        assert!(matches!(out.codec, AudioCodec::OggOpus));
        assert!(out.bytes.starts_with(b"OggS"));
    }
}
