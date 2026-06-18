// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-package manifest types (see design-docs/plugin-packages.md).
//!
//! A package is a signed, multi-component bundle. The manifest is the only
//! thing MIRA must understand to decide trust, capabilities, and how to
//! install. On disk it's `package.json` inside a `.mirapkg` tar.gz; the bytes
//! that get *signed* are the canonical JSON of the parsed struct with the
//! `verification` block stripped — identical to the skill-manifest scheme so
//! the crypto path is shared, not parallel.
//!
//! defines the types + parsing + validation; later phases act on them
//! (install / update / teardown) per component `kind`.

use serde::{Deserialize, Serialize};
use semver::Version;

use crate::skills::manifest::Verification;
use super::wizard::{validate_wizard, Action, ConfigField, ConfigMigration, SetupStep};

// Manifest format version this code understands.
pub const PACKAGE_FORMAT: &str = "1";

// The top-level package manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManifest {
    // Manifest format version. v1 code accepts `"1"`.
    pub format: String,
    // Reverse-DNS, globally unique, immutable across versions.
    pub id: String,
    pub name: String,
    pub version: Version,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    // Minimum MIRA version this package supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_mira_version: Option<Version>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    // Display name of the publisher. The *trusted* identity comes from the
    // trust store (keyed by the signature's fingerprint), not from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    // One package may carry several components.
    #[serde(default)]
    pub components: Vec<Component>,
    // Detached signature block (reused verbatim from the skills trust model).
    // Stripped before computing the canonical bytes that are signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
}

// One component. `kind` decides which subsystem it registers into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    #[serde(rename = "type")]
    pub kind: ComponentKind,
    #[serde(default)]
    pub runtime: Runtime,
    #[serde(default)]
    pub capabilities: Capabilities,
    // Component-type-specific configuration (e.g. an mcp_server's
    // command/args/env, or a cpp_provider's provider_kind/send_url). Kept as a
    // free JSON object in; later phases type it per `kind`.
    #[serde(default)]
    pub spec: serde_json::Value,
    // The install form: typed fields the admin fills (or MIRA mints/derives).
    // Drives the `cpp_provider` install wizard; empty for components
    // that need no guided config.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_schema: Vec<ConfigField>,
    // Ordered, typed, MIRA-verifiable provisioning steps. Empty for
    // components MIRA installs in one shot (e.g. `mcp_server`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup_guide: Vec<SetupStep>,
    // Config-field migrations applied when updating from an older version
    // renames carry a stored value across a key change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_migrations: Vec<ConfigMigration>,
    // Optional plugin-internal data-migration hook run on update (a `command`
    // the plugin knows how to run against its own schema). 
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_update: Option<Action>,
    // Path of this component's payload within the bundle, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Skill,
    McpServer,
    CppProvider,
    App,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    #[default]
    Native,
    Container,
}

// Least-privilege capability request. Mirrors the skill `Permissions` shape;
// `listen_port` is the one genuinely new (ingress) capability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub network_egress: Vec<String>,
    #[serde(default)]
    pub filesystem: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub subprocess: bool,
    #[serde(default)]
    pub subprocess_allowlist: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_port: Option<u16>,
}

// Errors from parsing or validating a manifest.
#[derive(Debug)]
pub enum ManifestError {
    Parse(String),
    Invalid(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Parse(e)   => write!(f, "manifest parse error: {e}"),
            ManifestError::Invalid(e) => write!(f, "invalid manifest: {e}"),
        }
    }
}
impl std::error::Error for ManifestError {}

impl PackageManifest {
    // Parse a `package.json` manifest.
    pub fn parse_json(text: &str) -> Result<Self, ManifestError> {
        serde_json::from_str(text).map_err(|e| ManifestError::Parse(e.to_string()))
    }

    // Structural validation (independent of signature/trust).
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.format != PACKAGE_FORMAT {
            return Err(ManifestError::Invalid(format!(
                "unsupported manifest format {:?} (this MIRA understands \"{PACKAGE_FORMAT}\")",
                self.format,
            )));
        }
        if self.id.trim().is_empty() {
            return Err(ManifestError::Invalid("package id is empty".into()));
        }
        if self.name.trim().is_empty() {
            return Err(ManifestError::Invalid("package name is empty".into()));
        }
        if self.components.is_empty() {
            return Err(ManifestError::Invalid("package has no components".into()));
        }
        for c in &self.components {
            validate_wizard(&c.config_schema, &c.setup_guide)?;
        }
        Ok(())
    }

    // Convenience: does this package contain a component of the given kind?
    pub fn has_kind(&self, kind: ComponentKind) -> bool {
        self.components.iter().any(|c| c.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "format": "1",
        "id": "com.example.nextcloud-mcp",
        "name": "Nextcloud MCP",
        "version": "1.0.0",
        "description": "Nextcloud Files + Calendar as agent tools.",
        "publisher": "Example",
        "components": [
            {
                "type": "mcp_server",
                "capabilities": { "network_egress": ["https://nextcloud.example.com"], "secrets": ["nc_app_pass"] },
                "spec": { "transport": "stdio", "command": "python3" }
            }
        ]
    }"#;

    #[test]
    fn parses_and_validates_sample() {
        let m = PackageManifest::parse_json(SAMPLE).unwrap();
        assert_eq!(m.id, "com.example.nextcloud-mcp");
        assert_eq!(m.version, Version::parse("1.0.0").unwrap());
        assert!(m.validate().is_ok());
        assert!(m.has_kind(ComponentKind::McpServer));
        assert!(!m.has_kind(ComponentKind::CppProvider));
        assert_eq!(m.components[0].runtime, Runtime::Native); // default
        assert_eq!(m.components[0].capabilities.secrets, vec!["nc_app_pass"]);
    }

    #[test]
    fn rejects_wrong_format() {
        let mut m = PackageManifest::parse_json(SAMPLE).unwrap();
        m.format = "2".into();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn rejects_empty_components() {
        let mut m = PackageManifest::parse_json(SAMPLE).unwrap();
        m.components.clear();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_))));
    }
}
