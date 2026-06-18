// SPDX-License-Identifier: AGPL-3.0-or-later

//! Skill manifest types and TOML parsing.
//!
//! The on-disk format is `skill.toml` at the root of each Skill directory.
//! Schema is documented in `design-docs/skills-and-agents.md` §"Manifest format".
//! Keep that doc and this module in sync.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use semver::{Version, VersionReq};

/// Top-level manifest. Mirrors the `[skill]` / `[permissions]` /
/// `[tools]` / `[dependencies]` / `[verification]`
/// sections of `skill.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    pub skill: SkillMeta,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub tools:        HashMap<String, ToolSpec>,
    #[serde(default)]
    pub dependencies: HashMap<String, VersionReq>,
    #[serde(default)]
    pub verification: Option<Verification>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    /// Reverse-DNS identifier (e.g. `com.mira.research`). Immutable across
    /// versions of the same Skill — renaming is a new Skill.
    pub id: String,
    pub version: Version,
    pub display_name: String,
    pub description:  String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub license: Option<String>,
    /// Minimum MIRA version this Skill requires. Loader skips Skills whose
    /// `mira_min` is greater than the running binary's version.
    #[serde(default)]
    pub mira_min: Option<Version>,
    /// System skill: a built-in capability whose tools/services/UI ship in
    /// the binary (the manifest only *exposes* them). Cannot be uninstalled —
    /// only enabled/disabled per user. Removing the manifest wouldn't remove
    /// the underlying feature, so the uninstall path refuses it. Default
    /// `false` (ordinary, removable skill).
    #[serde(default)]
    pub system: bool,
}

/// Declared permissions. The narrowest set the Skill needs to function;
/// the user/admin sees this list during install and either approves or
/// rejects. Enforced by the policy engine + sandbox at runtime.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    /// Allowed outbound URL prefixes. Empty = no network egress.
    /// Wildcards limited to host suffixes (`https://*.wikipedia.org`).
    #[serde(default)]
    pub network_egress: Vec<String>,

    /// `<mode>:<path>` entries. Modes: `read`, `write`, `read+write`.
    /// Paths starting with `~/` are interpreted relative to the user's
    /// home; absolute paths are used as-is.
    #[serde(default)]
    pub filesystem: Vec<String>,

    /// May the Skill spawn child processes?
    #[serde(default)]
    pub subprocess: bool,

    /// Specific binaries the Skill may exec (when `subprocess = true`).
    /// Empty + `subprocess = true` = any binary the sandbox lets through.
    #[serde(default)]
    pub subprocess_allowlist: Vec<String>,

    /// Secrets the Skill may read (e.g. `ANTHROPIC_API_KEY`). Each
    /// entry is either a bare key name (legacy form,
    /// `secrets = ["FOO"]`) or a typed declaration that the web UI
    /// uses to render an editable form:
    /// ```toml
    /// [[permissions.secrets]]
    /// key         = "ANTHROPIC_API_KEY"
    /// description = "Anthropic API key passed to the claude subprocess"
    /// required    = true
    /// scope       = "user"
    /// ```
    #[serde(default)]
    pub secrets: Vec<SecretSpec>,

    /// LLM provider aliases the Skill is allowed to invoke.
    /// Most Skills should pass `["primary"]`.
    #[serde(default)]
    pub llm_providers: Vec<String>,

    /// Hard cap on total LLM cost for one invocation of any tool in this
    /// Skill. Enforced by the policy engine.
    #[serde(default)]
    pub max_llm_spend_per_invocation_usd: Option<f64>,
}

/// One tool exposed to the agent. The map key (in `[tools]`) is the
/// name the LLM sees; this struct describes how to invoke it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ToolSpec {
    /// Wraps a built-in tool from `src/tools/`. `impl` is the registry name.
    Builtin { r#impl: String },

    /// A prompt template applied as a tool. The agent calls it like any
    /// tool, but execution just runs the template through the LLM.
    Prompt { template: String },

    /// An executable shipped with the Skill. Path is relative to the
    /// Skill's root directory.
    Executable {
        path: String,
        #[serde(default = "default_run_in_sandbox")]
        run_in_sandbox: bool,
    },
}

fn default_run_in_sandbox() -> bool { true }

/// One declared secret. Two TOML forms parse equivalently:
///
/// ```toml
/// secrets = ["LEGACY_KEY"]                           # bare name
/// [[permissions.secrets]]                            # typed
/// key         = "ANTHROPIC_API_KEY"
/// description = "Anthropic API key passed to claude"
/// required    = true
/// scope       = "user"            # "system" | "user" | "either"
/// sensitive   = true              # default true; UI masks the value
/// ```
///
/// All accessors are `key()`-aware so callers don't need to match.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SecretSpec {
    /// Legacy bare-name form. Defaults equivalent to a typed
    /// declaration with `required=false`, `sensitive=true`,
    /// `scope=either`.
    Name(String),
    /// Typed declaration. Drives the secrets-management UI.
    Typed {
        key: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        required: bool,
        #[serde(default = "default_secret_sensitive")]
        sensitive: bool,
        #[serde(default = "default_secret_scope")]
        scope: SecretScopeHint,
        /// Inline example shown under the input field in the UI to
        /// hint at the expected value shape (e.g. provider-prefixed
        /// model id, base URL with `/v1`). The placeholder text on
        /// the input also picks this up. Optional — falls back to
        /// generic "value" placeholder when absent.
        #[serde(default)]
        example: Option<String>,
    },
}

fn default_secret_sensitive() -> bool { true }
fn default_secret_scope() -> SecretScopeHint { SecretScopeHint::Either }

/// Where the operator is *expected* to set the value. Advisory only —
/// the runtime always merges system + user with user shadowing on
/// collision (see `SecretsStore::env_vars_for`). The hint just tells
/// the UI which scope to default to when offering "Set value".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretScopeHint {
    /// Suggested at the host/admin level (one value for everyone).
    System,
    /// Suggested at the per-user level (each user sets their own).
    User,
    /// Either is fine; UI shows both options.
    Either,
}

impl SecretSpec {
    /// The env-var key, regardless of which form was used.
    pub fn key(&self) -> &str {
        match self {
            SecretSpec::Name(s) => s,
            SecretSpec::Typed { key, .. } => key,
        }
    }

    pub fn description(&self) -> Option<&str> {
        match self {
            SecretSpec::Name(_) => None,
            SecretSpec::Typed { description, .. } => description.as_deref(),
        }
    }

    pub fn required(&self) -> bool {
        match self {
            SecretSpec::Name(_) => false,
            SecretSpec::Typed { required, .. } => *required,
        }
    }

    pub fn sensitive(&self) -> bool {
        match self {
            SecretSpec::Name(_) => true,
            SecretSpec::Typed { sensitive, .. } => *sensitive,
        }
    }

    pub fn scope_hint(&self) -> SecretScopeHint {
        match self {
            SecretSpec::Name(_) => SecretScopeHint::Either,
            SecretSpec::Typed { scope, .. } => *scope,
        }
    }

    pub fn example(&self) -> Option<&str> {
        match self {
            SecretSpec::Name(_) => None,
            SecretSpec::Typed { example, .. } => example.as_deref(),
        }
    }
}

impl PartialEq<str> for SecretSpec {
    fn eq(&self, other: &str) -> bool { self.key() == other }
}

impl From<String> for SecretSpec {
    fn from(s: String) -> Self { SecretSpec::Name(s) }
}

impl From<&str> for SecretSpec {
    fn from(s: &str) -> Self { SecretSpec::Name(s.to_string()) }
}

/// Detached signature material. Absent in the manifest = unsigned.
/// Validation happens in `verify` (slice A7); A1 only parses the fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    /// `ed25519:<base64-signature>` over the canonicalised manifest
    /// (the manifest with this `[verification]` section removed).
    pub signature: String,
    /// `fingerprint:<hex>` of the publisher's public key.
    pub publisher_key: String,
    /// RFC 3339 timestamp of when the signature was produced.
    pub signed_at: String,
}

impl SkillManifest {
    /// Parse a manifest from TOML text. Doesn't perform semantic
    /// validation — call `validate` for that.
    pub fn parse(toml_text: &str) -> Result<Self, ManifestError> {
        toml::from_str(toml_text).map_err(ManifestError::Parse)
    }

    /// Validate the manifest beyond what serde/toml enforces. Returns the
    /// full list of problems so users see everything wrong at once.
    pub fn validate(&self) -> Result<(), Vec<ManifestError>> {
        let mut errs = Vec::new();

        if !is_reverse_dns(&self.skill.id) {
            errs.push(ManifestError::InvalidId(self.skill.id.clone()));
        }
        // version is already a semver::Version — toml/serde rejects bad
        // versions at parse time, so no further check needed.

        if self.skill.display_name.trim().is_empty() {
            errs.push(ManifestError::EmptyField("display_name"));
        }
        if self.skill.description.trim().is_empty() {
            errs.push(ManifestError::EmptyField("description"));
        }

        // Filesystem entries must match `<mode>:<path>` with a known mode.
        for fs in &self.permissions.filesystem {
            if !is_valid_fs_entry(fs) {
                errs.push(ManifestError::BadFilesystemEntry(fs.clone()));
            }
        }

        // Network egress entries must be http(s):// URLs (with optional
        // wildcard host suffix). Reject things like raw hostnames or
        // schemes we don't proxy yet.
        for net in &self.permissions.network_egress {
            if !is_valid_network_entry(net) {
                errs.push(ManifestError::BadNetworkEntry(net.clone()));
            }
        }

        // Tool keys must be valid identifiers — the LLM uses them as
        // function names.
        for name in self.tools.keys() {
            if !is_valid_tool_name(name) {
                errs.push(ManifestError::InvalidToolName(name.clone()));
            }
        }

        // Subprocess allowlist is meaningless when subprocess=false.
        if !self.permissions.subprocess && !self.permissions.subprocess_allowlist.is_empty() {
            errs.push(ManifestError::SubprocessAllowlistWithoutSubprocess);
        }

        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    /// Resolve any tool paths that point to files on disk. Used by the
    /// loader to verify executable paths exist relative to the Skill root.
    pub fn executable_paths<'a>(&'a self) -> impl Iterator<Item = &'a str> + 'a {
        self.tools.values().filter_map(|t| match t {
            ToolSpec::Executable { path, .. } => Some(path.as_str()),
            _ => None,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("could not parse skill.toml: {0}")]
    Parse(toml::de::Error),

    #[error("skill id {0:?} is not a valid reverse-DNS identifier (e.g. com.example.skill)")]
    InvalidId(String),

    #[error("required field {0} is empty")]
    EmptyField(&'static str),

    #[error("filesystem permission {0:?} is not in `<mode>:<path>` format with a known mode (read/write/read+write)")]
    BadFilesystemEntry(String),

    #[error("network_egress entry {0:?} is not a valid http(s) URL prefix")]
    BadNetworkEntry(String),

    #[error("tool name {0:?} is not a valid identifier (alphanumeric + underscore, ≤64 chars)")]
    InvalidToolName(String),

    #[error("subprocess_allowlist is set but subprocess=false — the allowlist will never be consulted")]
    SubprocessAllowlistWithoutSubprocess,

    #[error("missing executable referenced in tools: {0:?}")]
    MissingExecutable(String),

    #[error("skill requires MIRA >= {required}, but this binary is {running}")]
    MiraVersionTooOld { required: Version, running: Version },
}

// ── small validators (kept local so the regex compiles once per call —
//    swap for once_cell if this becomes hot) ───────────────────────────

fn is_reverse_dns(s: &str) -> bool {
    // Two or more dot-separated labels; each label is `[a-z0-9][a-z0-9-]*`
    // (lowercase, hyphen-separated). Rejects underscores, leading hyphens,
    // and uppercase to keep the namespace canonical.
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 2 { return false; }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !p.starts_with('-')
            && !p.ends_with('-')
    })
}

fn is_valid_fs_entry(s: &str) -> bool {
    let Some((mode, path)) = s.split_once(':') else { return false; };
    matches!(mode, "read" | "write" | "read+write") && !path.trim().is_empty()
}

fn is_valid_network_entry(s: &str) -> bool {
    // Accept http(s)://host[/path…] with optional `*.` host wildcard.
    // No port-only entries, no raw hostnames — keeps the proxy logic
    // simple and avoids "looks like a typo" entries silently allowing
    // more than the user intended.
    if !(s.starts_with("https://") || s.starts_with("http://")) {
        return false;
    }
    let rest = s.trim_start_matches("https://").trim_start_matches("http://");
    let host = rest.split('/').next().unwrap_or("");
    !host.is_empty() && !host.contains(' ')
}

fn is_valid_tool_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// Used by the loader to check that tool paths actually exist on disk.
pub fn executable_resolves(skill_root: &Path, rel: &str) -> bool {
    skill_root.join(rel).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> &'static str {
        r#"
[skill]
id = "com.example.minimal"
version = "1.0.0"
display_name = "Minimal"
description = "The smallest valid skill."
"#
    }

    #[test]
    fn parses_minimal_valid_manifest() {
        let m = SkillManifest::parse(minimal_toml()).expect("parses");
        assert_eq!(m.skill.id, "com.example.minimal");
        assert_eq!(m.skill.version, Version::parse("1.0.0").unwrap());
        assert!(m.tools.is_empty());
        assert!(m.permissions.network_egress.is_empty());
        m.validate().expect("valid");
    }

    #[test]
    fn parses_full_manifest() {
        let toml = r#"
[skill]
id = "com.mira.research"
version = "1.2.3"
display_name = "Deep Research"
description = "Multi-source research."
authors = ["MIRA Team <hello@mira.dev>"]
license = "MIT"
mira_min = "1.0.0"

[permissions]
network_egress = ["https://duckduckgo.com", "https://*.wikipedia.org"]
filesystem = ["read:~/Documents/research/", "write:~/Documents/research/"]
subprocess = false
secrets = []
llm_providers = ["primary"]
max_llm_spend_per_invocation_usd = 1.00

[tools]
web_search = { kind = "builtin", impl = "web_fetch" }
synthesize = { kind = "prompt", template = "prompts/synthesize.md" }
extract    = { kind = "executable", path = "tools/extract.py", run_in_sandbox = true }

[dependencies]
"com.mira.web_fetch" = ">=1.0.0"

[verification]
signature = "ed25519:abc"
publisher_key = "fingerprint:def"
signed_at = "2026-05-15T12:00:00Z"
"#;
        let m = SkillManifest::parse(toml).expect("parses");
        m.validate().expect("valid");
        assert_eq!(m.tools.len(), 3);
        assert_eq!(m.permissions.network_egress.len(), 2);
        assert!(m.verification.is_some());
        assert_eq!(m.dependencies.len(), 1);

        // Executable paths surface for filesystem checks
        let exes: Vec<&str> = m.executable_paths().collect();
        assert_eq!(exes, vec!["tools/extract.py"]);
    }

    #[test]
    fn rejects_invalid_id() {
        let toml = minimal_toml().replace("com.example.minimal", "MyCoolSkill");
        let m = SkillManifest::parse(&toml).unwrap();
        let errs = m.validate().unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ManifestError::InvalidId(_))));
    }

    #[test]
    fn rejects_bad_semver_at_parse_time() {
        let toml = minimal_toml().replace(r#"version = "1.0.0""#, r#"version = "not-a-version""#);
        let result = SkillManifest::parse(&toml);
        assert!(result.is_err(), "bad semver should fail parsing");
    }

    #[test]
    fn rejects_malformed_filesystem_entry() {
        let toml = format!(
            r#"
[skill]
id = "com.example.skill"
version = "1.0.0"
display_name = "Skill"
description = "x"

[permissions]
filesystem = ["weird-no-mode"]
"#
        );
        let m = SkillManifest::parse(&toml).unwrap();
        let errs = m.validate().unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ManifestError::BadFilesystemEntry(_))));
    }

    #[test]
    fn rejects_malformed_network_entry() {
        let toml = r#"
[skill]
id = "com.example.skill"
version = "1.0.0"
display_name = "Skill"
description = "x"

[permissions]
network_egress = ["just-a-hostname.com"]
"#;
        let m = SkillManifest::parse(toml).unwrap();
        let errs = m.validate().unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ManifestError::BadNetworkEntry(_))));
    }

    #[test]
    fn flags_subprocess_allowlist_without_subprocess() {
        let toml = r#"
[skill]
id = "com.example.skill"
version = "1.0.0"
display_name = "Skill"
description = "x"

[permissions]
subprocess = false
subprocess_allowlist = ["/usr/bin/git"]
"#;
        let m = SkillManifest::parse(toml).unwrap();
        let errs = m.validate().unwrap_err();
        assert!(errs.iter().any(|e| matches!(
            e, ManifestError::SubprocessAllowlistWithoutSubprocess
        )));
    }

    #[test]
    fn rejects_invalid_tool_name() {
        let toml = r#"
[skill]
id = "com.example.skill"
version = "1.0.0"
display_name = "Skill"
description = "x"

[tools]
"with spaces" = { kind = "builtin", impl = "web_fetch" }
"#;
        let m = SkillManifest::parse(toml).unwrap();
        let errs = m.validate().unwrap_err();
        assert!(errs.iter().any(|e| matches!(e, ManifestError::InvalidToolName(_))));
    }

    /// Backwards-compat: a manifest using the legacy bare-string
    /// form for secrets must still parse and round-trip through
    /// `key()`.
    #[test]
    fn secrets_legacy_string_form_parses() {
        let toml = r#"
[skill]
id = "com.x"
version = "0.1.0"
display_name = "X"
description = "x"
authors = ["a"]
license = "MIT"
[permissions]
secrets = ["FOO_KEY", "BAR_KEY"]
[tools]
"#;
        let m = SkillManifest::parse(toml).unwrap();
        let keys: Vec<&str> = m.permissions.secrets.iter().map(|s| s.key()).collect();
        assert_eq!(keys, vec!["FOO_KEY", "BAR_KEY"]);
        // Legacy form gets safe defaults.
        for s in &m.permissions.secrets {
            assert_eq!(s.required(), false);
            assert_eq!(s.sensitive(), true);
            assert_eq!(s.scope_hint(), SecretScopeHint::Either);
        }
    }

    /// New typed form parses and surfaces all the fields the UI
    /// renders.
    #[test]
    fn secrets_typed_form_parses() {
        let toml = r#"
[skill]
id = "com.x"
version = "0.1.0"
display_name = "X"
description = "x"
authors = ["a"]
license = "MIT"
[permissions]
[[permissions.secrets]]
key         = "ANTHROPIC_API_KEY"
description = "An Anthropic key."
required    = true
scope       = "user"
[[permissions.secrets]]
key         = "OPENAI_API_KEY"
[tools]
"#;
        let m = SkillManifest::parse(toml).unwrap();
        let s0 = &m.permissions.secrets[0];
        assert_eq!(s0.key(), "ANTHROPIC_API_KEY");
        assert_eq!(s0.description(), Some("An Anthropic key."));
        assert_eq!(s0.required(), true);
        assert_eq!(s0.scope_hint(), SecretScopeHint::User);
        // Typed declaration with only `key` set: defaults must apply.
        let s1 = &m.permissions.secrets[1];
        assert_eq!(s1.key(), "OPENAI_API_KEY");
        assert_eq!(s1.required(), false);
        assert_eq!(s1.sensitive(), true);
        assert_eq!(s1.scope_hint(), SecretScopeHint::Either);
    }

    /// All entries can be the new typed form — including
    /// minimal-typed (only `key`) which mirrors the legacy form's
    /// safe defaults. Manifests pick a form per file; TOML itself
    /// disallows mixing inline-array `secrets = [...]` with
    /// array-of-tables `[[permissions.secrets]]`.
    #[test]
    fn secrets_typed_minimal_matches_legacy_defaults() {
        let toml = r#"
[skill]
id = "com.x"
version = "0.1.0"
display_name = "X"
description = "x"
authors = ["a"]
license = "MIT"
[permissions]
[[permissions.secrets]]
key = "ONE"
[[permissions.secrets]]
key = "TWO"
[tools]
"#;
        let m = SkillManifest::parse(toml).unwrap();
        let keys: Vec<&str> = m.permissions.secrets.iter().map(|s| s.key()).collect();
        assert_eq!(keys, vec!["ONE", "TWO"]);
        for s in &m.permissions.secrets {
            assert_eq!(s.required(), false);
            assert_eq!(s.scope_hint(), SecretScopeHint::Either);
        }
    }

    #[test]
    fn reverse_dns_validator_edge_cases() {
        assert!(is_reverse_dns("com.mira.research"));
        assert!(is_reverse_dns("io.example.thing-name"));
        assert!(is_reverse_dns("a.b"));

        assert!(!is_reverse_dns("singleword"));
        assert!(!is_reverse_dns("UpperCase.skill"));
        assert!(!is_reverse_dns("com.example.")); // trailing empty segment
        assert!(!is_reverse_dns("com..example"));  // empty middle segment
        assert!(!is_reverse_dns("com.example.-skill"));
        assert!(!is_reverse_dns("com.with_underscore.skill"));
    }
}
