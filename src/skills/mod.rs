// SPDX-License-Identifier: AGPL-3.0-or-later

//! The Skills system — Phase A of `design-docs/skills-and-agents.md`.
//!
//! A1 (this slice): manifest schema + loader. Subsequent slices wire
//! Skills into the agent's tool dispatch (A3), enforce permissions
//! through the sandbox (A2), and surface them in the web UI (A4-A6).
//!
//! Layout:
//! - `manifest` — TOML types and validation rules.
//! - `loader`   — directory scan that produces a `SkillRegistry`.

pub mod bundled;
pub mod cli;
pub mod manifest;
pub mod loader;
pub mod permissions;
pub mod prefs;
pub mod runtime;
pub mod secrets;
pub mod signing;
pub mod trust;

pub use manifest::{SkillManifest, SkillMeta, Permissions, ToolSpec, ManifestError, Verification};
pub use loader::{SkillRegistry, LoadedSkill, LoadError, load_dir, load_dir_with_trust};
pub use permissions::{
    AccessMode, Denied,
    check_filesystem, check_network_egress, check_subprocess,
    check_secret_access, check_llm_provider,
    build_sandbox_limits,
};
pub use prefs::SkillPrefsStore;
pub use secrets::{Scope as SecretScope, SecretEntry, SecretsError, SecretsStore};
pub use runtime::{SkillTool, BuiltinDispatcher};
pub use signing::{verify_manifest, sign_manifest, apply_signature, VerificationOutcome};
pub use trust::{TrustStore, TrustEntry, fingerprint_of, parse_public_key_b64};

use std::path::{Path, PathBuf};

/// Conventional location for a user's installed Skills, derived from the
/// configured data dir. The web UI's install flow (slice A6) writes
/// here; the loader reads from here at startup.
pub fn default_skills_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("skills")
}
