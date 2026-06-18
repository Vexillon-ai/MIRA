// SPDX-License-Identifier: AGPL-3.0-or-later

// src/config/schema.rs
//! Compile-time embedded config artefacts.
//!
//! Both strings are included verbatim from the repo's `config/` directory so
//! the binary can always produce a valid template and schema without needing
//! external files at runtime.

/// JSON Schema (Draft-7) that defines the structure of `mira_config.json`.
/// Validated against every config file at startup.
pub const SCHEMA_JSON: &str = include_str!("../../config/mira_config.schema.json");

/// JSONC example / template shown by `mira --print-config-template`.
/// This is intentionally JSONC (JSON with `//` comments) — it is never parsed
/// as JSON by MIRA; it is for human reference only.
pub const EXAMPLE_JSONC: &str = include_str!("../../config/mira_config.example.json");
