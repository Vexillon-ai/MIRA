// SPDX-License-Identifier: AGPL-3.0-or-later

// src/voice/mod.rs
//! Voice subsystem glue — channel registry + per-user voice preferences.
//!
//! The TTS and STT modules deal with synthesis and transcription respectively;
//! this module owns the layer above both: which channels exist, what the
//! response policy is per channel (always / on voice input / never), and
//! what voice id to use. Built so plugin-defined channels can register a
//! `ChannelDescriptor` at runtime — every UI grid then iterates over the
//! registry rather than a hard-coded channel list.

pub mod prefs;
pub mod registry;

pub use prefs::{
    normalise, parse_user_prefs, resolve_voice, to_storage_json,
    ChannelVoicePrefs, ResolvedVoice, ResponsePolicy, VoicePrefsMap,
};
pub use registry::{ChannelDescriptor, ChannelRegistry};
