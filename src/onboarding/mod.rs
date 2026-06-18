// SPDX-License-Identifier: AGPL-3.0-or-later

// src/onboarding/mod.rs
//! User onboarding — conversational flow, profile capture, and seed memories.
//!
//! This module owns the three-layer storage split described in
//! `design/onboarding/ONBOARDING_DESIGN.md`:
//!
//! - Structured facts live in `user_profile` (managed by `auth::AuthDb`).
//! - Load-bearing preferences live in a per-user `profile.md` (see
//!   [`profile_file`]).
//! - Narrative memories are written to the memory DB via seed entries at
//!   completion time.
//!
//! The schema loader, prompt builder, and tool handlers arrive in later
//! steps of the plan; this `mod.rs` starts minimal and grows in-place.

pub mod extractor;
pub mod preamble;
pub mod profile_file;
pub mod prompt;
pub mod schema;

pub use extractor::{apply_ops, extract_updates_from_transcript, ExtractedUpdates, Op};
pub use preamble::{build_profile_preamble, ProfilePreambleCache};
pub use profile_file::{profile_md_heading, profile_md_path, read_profile_md, write_profile_section, ProfileMdError, PROFILE_SECTIONS};
pub use prompt::build_onboarding_prompt;
pub use schema::{Group, OnboardingSchema, Question, WriteTarget};
