// SPDX-License-Identifier: AGPL-3.0-or-later

//! The `config_schema` + `setup_guide` manifest grammar (see
//! design-docs/plugin-packages.md §"Manifest grammar").
//!
//! These two grammars are what turn a `cpp_provider` component from a README
//! into a wizard MIRA mostly runs itself: `config_schema` declares the install
//! form (and where each value comes from — admin input, a minted secret, a
//! derived template, or a setup-step output), and `setup_guide` is the ordered,
//! typed, MIRA-verifiable list of steps that provision it.
//!
//! This module defines and *validates* the grammar (, slice 1). The
//! executor that runs the steps lives in [`super::engine`] (slice 3); the
//! `mira.*` action verbs are dispatched there.
//!
//! NOTE: `mira.write_service` (a same-host MIRA-managed provider service) is
//! modelled here but intentionally **not executed** in v1 — see
//! [`Action::WriteService`]. v1 is "connection-only": the admin runs the
//! provider process; MIRA mints the secrets, creates the account, pre-fills the
//! commands, and verifies reachability.

use serde::{Deserialize, Serialize};

use super::manifest::{ManifestError, Runtime};

// ---------------------------------------------------------------------------
// config_schema — the install form
// ---------------------------------------------------------------------------

// One field in a component's `config_schema`. The load-bearing attribute is
// [`source`](ConfigField::source) — where the value comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigField {
    // Config key the value is stored under (referenced as `${config.KEY}`).
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(rename = "type", default)]
    pub field_type: FieldType,
    #[serde(default)]
    pub source: FieldSource,
    // Route the value into the encrypted secret store; masked, never logged
    // or exported. Independent of `field_type` (a `url` can be secret too).
    #[serde(default)]
    pub secret: bool,
    // UI grouping label (the install form lays fields out by group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default)]
    pub required: bool,

    // Exactly one of the following is meaningful, per `source`:
    // `source: generate` — how MIRA mints the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate: Option<GenerateSpec>,
    // `source: derive` — a template over other fields / step outputs / facts,
    // e.g. `"${mira.base_url}/webhook/external/${account_id}"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derive: Option<String>,
    // `source: step_output` — `"<step_id>.<output_key>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_step: Option<String>,

    // Optional, shared across sources:
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(rename = "enum", default, skip_serializing_if = "Vec::is_empty")]
    pub enum_values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<ValidateSpec>,
    // Show this field only when the expression is truthy (e.g.
    // `"config.SEND_VOICE == true"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<String>,
    // Require this field only when the expression is truthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_when: Option<String>,
    // On *update*, re-mint a `source: generate` secret instead of preserving
    // the existing value. Default false — secrets survive updates so the
    // provider doesn't drift. Only meaningful for generated secrets.
    #[serde(default)]
    pub rotate_on_update: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    #[default]
    String,
    Secret,
    Url,
    Host,
    Int,
    Bool,
    Enum,
    Multiline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldSource {
    // Admin types it.
    #[default]
    Input,
    // MIRA mints it from the `generate` spec.
    Generate,
    // Computed from a `derive` template.
    Derive,
    // Produced by a `setup_guide` step (`from_step`).
    StepOutput,
}

// How a `source: generate` value is minted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateSpec {
    // Random byte length before encoding.
    #[serde(default = "default_secret_bytes")]
    pub bytes: usize,
    #[serde(default)]
    pub encoding: Encoding,
}

fn default_secret_bytes() -> usize {
    32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Hex,
    Base64,
}

// Input validation for an admin-supplied field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidateSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
}

// ---------------------------------------------------------------------------
// setup_guide — the wizard
// ---------------------------------------------------------------------------

// One ordered step in a component's `setup_guide`. Steps form a DAG via
// `after` (and implicitly via `produces` → `from_step`); the engine renders a
// wizard, persists progress, and resumes across async/human steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupStep {
    pub id: String,
    pub title: String,
    // Markdown shown to the admin; may reference bundled media and is
    // templated with `${config.X}` / `${out.step.field}` / `${mira.*}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub actor: Actor,
    // Run this step only when the expression is truthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    // Explicit ordering: ids of steps that must complete first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    pub action: Action,
    // Output keys this step produces (referenced as `${out.<id>.<key>}` and
    // by config fields via `from_step`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produces: Vec<String>,
    // Optional probe MIRA runs to confirm the step took effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyProbe>,
    // When this step runs: on first `install`, on `update`, or `both`. On an
    // update MIRA runs only the `update`/`both` steps (default `install`).
    #[serde(default)]
    pub run_on: RunOn,
}

// When a `setup_guide` step runs across an install vs. an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOn {
    #[default]
    Install,
    Update,
    Both,
}

impl RunOn {
    // Does this step run on a fresh install?
    pub fn on_install(self) -> bool {
        matches!(self, RunOn::Install | RunOn::Both)
    }
    // Does this step run on an update?
    pub fn on_update(self) -> bool {
        matches!(self, RunOn::Update | RunOn::Both)
    }
}

// A config-field migration applied when updating *from* an old version range —
// renames carry a value across a key change; drops remove a retired field.
// Adds/removes are inferred from the schema diff; only renames need declaring.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigMigration {
    // Semver range this migration applies *from*, e.g. `"<2.0.0"`. Empty = always.
    #[serde(default)]
    pub from: String,
    // `old_key` → `new_key`: carry the stored value across the rename.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub rename: std::collections::BTreeMap<String, String>,
    // Keys to drop (their stored values are discarded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop: Vec<String>,
}

// Who performs a step. `mira` is fully automated; `admin` runs something on
// this host; `admin_external` does something in a third-party system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    #[default]
    Mira,
    Admin,
    AdminExternal,
}

// A typed action verb. Tagged on `verb`. `mira.*` verbs are MIRA-automated and
// idempotent (check-existing, so a resumed install never double-creates);
// `command`/`paste`/`external_ui`/`note` are human-in-the-loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verb")]
pub enum Action {
    // Generate a secret → output (`secret`).
    #[serde(rename = "mira.mint_secret")]
    MintSecret {
        #[serde(default = "default_secret_bytes")]
        bytes: usize,
        #[serde(default)]
        encoding: Encoding,
    },
    // Create the External/CPP account → `account_id`, `inbound_secret`,
    // `outbound_secret`; sets `send_url`.
    #[serde(rename = "mira.create_channel_account")]
    CreateChannelAccount {
        provider_kind: String,
        #[serde(default)]
        mention_only: bool,
        #[serde(default)]
        supports_voice: bool,
    },
    // Register an MCP server from config (Phase-1 reuse).
    #[serde(rename = "mira.register_mcp_server")]
    RegisterMcpServer,
    // Toggle a MIRA setting.
    #[serde(rename = "mira.set_setting")]
    SetSetting {
        key: String,
        value: serde_json::Value,
    },
    // Render + run a same-host provider as a MIRA-managed service.
    //     // Linux (systemd `--user`) is supported; other platforms degrade to a
    // clear "run it yourself" error (connection-only). `command` is the
    // provider entrypoint — relative paths resolve under the package's
    // extracted payload dir; `args`/`env` values are templated from
    // `${config.*}` / `${out.*}` (so the minted secrets + send_url flow in).
    #[serde(rename = "mira.write_service")]
    WriteService {
        #[serde(default)]
        runtime: Runtime,
        // Provider entrypoint. Required at execution; optional in the grammar
        // so a manifest parses, with a clear error if it's missing at run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        env: std::collections::BTreeMap<String, String>,
    },
    // A shell command. `run_by: admin` = "run this on your server" (pre-filled
    // and shown, MIRA does not execute it); `run_by: mira` = MIRA runs it.
    #[serde(rename = "command")]
    Command {
        #[serde(default)]
        run_by: Actor,
        // The command template, pre-filled with config/step values.
        render: String,
    },
    // Admin pastes a value back → output.
    #[serde(rename = "paste")]
    Paste {
        label: String,
        // Output key the pasted value is stored under (defaults to the step's
        // first `produces`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default)]
        secret: bool,
    },
    // "Do X in the third-party UI," with reference values/media.
    #[serde(rename = "external_ui")]
    ExternalUi {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    // A standalone verification gate.
    #[serde(rename = "verify")]
    Verify { verify: VerifyProbe },
    // Pure info.
    #[serde(rename = "note")]
    Note,
}

impl Action {
    // The verb tag, for diagnostics.
    pub fn verb(&self) -> &'static str {
        match self {
            Action::MintSecret { .. } => "mira.mint_secret",
            Action::CreateChannelAccount { .. } => "mira.create_channel_account",
            Action::RegisterMcpServer => "mira.register_mcp_server",
            Action::SetSetting { .. } => "mira.set_setting",
            Action::WriteService { .. } => "mira.write_service",
            Action::Command { .. } => "command",
            Action::Paste { .. } => "paste",
            Action::ExternalUi { .. } => "external_ui",
            Action::Verify { .. } => "verify",
            Action::Note => "note",
        }
    }

    // Whether this verb is automated by MIRA (vs. human-in-the-loop).
    pub fn is_mira_automated(&self) -> bool {
        matches!(
            self,
            Action::MintSecret { .. }
                | Action::CreateChannelAccount { .. }
                | Action::RegisterMcpServer
                | Action::SetSetting { .. }
                | Action::WriteService { .. }
                | Action::Verify { .. }
        ) || matches!(self, Action::Command { run_by: Actor::Mira, .. })
    }
}

// ---------------------------------------------------------------------------
// verify probes
// ---------------------------------------------------------------------------

// A probe MIRA runs to confirm a step took effect. Tagged on `type`. `port`
// and `url` are templated strings resolved at run time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerifyProbe {
    // HTTP request expecting a status (e.g. an unsigned POST → 401).
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_status: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_fail: Option<OnFail>,
    },
    // TCP connect to `host:port`.
    Tcp {
        #[serde(default = "default_host")]
        host: String,
        port: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_fail: Option<OnFail>,
    },
    // Run a command, expect an exit code.
    CommandExit {
        command: String,
        #[serde(default)]
        expect: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_fail: Option<OnFail>,
    },
    // End-to-end: post a *signed* health-check inbound to MIRA's own webhook
    // and expect it accepted (`kind: cpp`), or complete an MCP `initialize`
    // handshake against an HTTP MCP server (`kind: mcp`, needs `url`).
    Roundtrip {
        kind: String,
        // For `kind: mcp` — the MCP server's HTTP endpoint (templated). Ignored
        // for `cpp` (which targets MIRA's own `/webhook/external/{id}`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_fail: Option<OnFail>,
    },
}

fn default_host() -> String {
    "127.0.0.1".into()
}

impl VerifyProbe {
    pub fn on_fail(&self) -> Option<&OnFail> {
        match self {
            VerifyProbe::Http { on_fail, .. }
            | VerifyProbe::Tcp { on_fail, .. }
            | VerifyProbe::CommandExit { on_fail, .. }
            | VerifyProbe::Roundtrip { on_fail, .. } => on_fail.as_ref(),
        }
    }
}

// What to tell the admin when a probe fails, and whether it blocks the install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnFail {
    pub message: String,
    // A blocking failure stops the install; non-blocking is a warning the
    // admin can proceed past.
    #[serde(default = "default_true")]
    pub blocking: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------

// Validate a component's `config_schema` + `setup_guide` together: unique
// keys/ids, source/spec consistency, resolvable references, and an acyclic
// step DAG. Structural only — no execution, no network.
pub fn validate_wizard(
    config_schema: &[ConfigField],
    setup_guide: &[SetupStep],
) -> Result<(), ManifestError> {
    // --- config_schema ---
    let mut keys = std::collections::HashSet::new();
    for f in config_schema {
        if f.key.trim().is_empty() {
            return Err(ManifestError::Invalid("config field has empty key".into()));
        }
        if !keys.insert(f.key.as_str()) {
            return Err(ManifestError::Invalid(format!(
                "duplicate config field key {:?}",
                f.key
            )));
        }
        // Source ⇒ required spec.
        match f.source {
            FieldSource::Generate if f.generate.is_none() => {
                return Err(ManifestError::Invalid(format!(
                    "config field {:?} has source=generate but no `generate` spec",
                    f.key
                )));
            }
            FieldSource::Derive if f.derive.is_none() => {
                return Err(ManifestError::Invalid(format!(
                    "config field {:?} has source=derive but no `derive` template",
                    f.key
                )));
            }
            FieldSource::StepOutput if f.from_step.is_none() => {
                return Err(ManifestError::Invalid(format!(
                    "config field {:?} has source=step_output but no `from_step`",
                    f.key
                )));
            }
            _ => {}
        }
        if f.field_type == FieldType::Enum && f.enum_values.is_empty() {
            return Err(ManifestError::Invalid(format!(
                "config field {:?} is type=enum but lists no values",
                f.key
            )));
        }
    }

    // --- setup_guide ---
    let mut ids = std::collections::HashSet::new();
    for s in setup_guide {
        if s.id.trim().is_empty() {
            return Err(ManifestError::Invalid("setup step has empty id".into()));
        }
        if !ids.insert(s.id.as_str()) {
            return Err(ManifestError::Invalid(format!(
                "duplicate setup step id {:?}",
                s.id
            )));
        }
    }
    // `after` references resolve, and each `from_step` names a real step + output.
    for s in setup_guide {
        for dep in &s.after {
            if !ids.contains(dep.as_str()) {
                return Err(ManifestError::Invalid(format!(
                    "setup step {:?} lists after={:?} which is not a step id",
                    s.id, dep
                )));
            }
        }
    }
    for f in config_schema {
        if let Some(from) = &f.from_step {
            let (step_id, out_key) = from.split_once('.').ok_or_else(|| {
                ManifestError::Invalid(format!(
                    "config field {:?} from_step={:?} must be \"<step_id>.<output_key>\"",
                    f.key, from
                ))
            })?;
            let step = setup_guide.iter().find(|s| s.id == step_id).ok_or_else(|| {
                ManifestError::Invalid(format!(
                    "config field {:?} from_step references unknown step {:?}",
                    f.key, step_id
                ))
            })?;
            if !produces_output(step, out_key) {
                return Err(ManifestError::Invalid(format!(
                    "config field {:?} from_step references {:?} which step {:?} does not produce",
                    f.key, out_key, step_id
                )));
            }
        }
    }

    // Acyclic DAG over `after` edges.
    check_acyclic(setup_guide)?;
    Ok(())
}

// Whether a step yields the given output key (declared `produces`, or the
// implicit outputs of an automated verb).
fn produces_output(step: &SetupStep, key: &str) -> bool {
    if step.produces.iter().any(|p| p == key) {
        return true;
    }
    // Implicit outputs of `mira.*` verbs, so a manifest can reference them
    // without redundantly listing `produces`.
    match &step.action {
        Action::CreateChannelAccount { .. } => {
            matches!(key, "account_id" | "inbound_secret" | "outbound_secret" | "send_url")
        }
        Action::MintSecret { .. } => key == "secret",
        _ => false,
    }
}

// Detect a cycle in the `after` edges via DFS three-colour marking.
fn check_acyclic(steps: &[SetupStep]) -> Result<(), ManifestError> {
    use std::collections::HashMap;
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        White,
        Grey,
        Black,
    }
    let index: HashMap<&str, usize> =
        steps.iter().enumerate().map(|(i, s)| (s.id.as_str(), i)).collect();
    let mut marks = vec![Mark::White; steps.len()];

    fn visit(
        i: usize,
        steps: &[SetupStep],
        index: &std::collections::HashMap<&str, usize>,
        marks: &mut [Mark],
    ) -> Result<(), ManifestError> {
        match marks[i] {
            Mark::Black => return Ok(()),
            Mark::Grey => {
                return Err(ManifestError::Invalid(format!(
                    "setup_guide has a dependency cycle through step {:?}",
                    steps[i].id
                )));
            }
            Mark::White => {}
        }
        marks[i] = Mark::Grey;
        for dep in &steps[i].after {
            if let Some(&j) = index.get(dep.as_str()) {
                visit(j, steps, index, marks)?;
            }
        }
        marks[i] = Mark::Black;
        Ok(())
    }

    for i in 0..steps.len() {
        visit(i, steps, &index, &mut marks)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The condensed `nextcloud-talk` wizard from design-docs/plugin-packages.md must
    // round-trip through the grammar.
    const NEXTCLOUD_TALK: &str = r#"{
      "config_schema": [
        { "key": "TALK_BOT_SECRET", "label": "Talk bot secret", "type": "secret",
          "source": "generate", "secret": true, "group": "Nextcloud", "required": true,
          "generate": { "bytes": 40, "encoding": "hex" } },
        { "key": "SEND_URL", "type": "url", "source": "derive",
          "derive": "${mira.base_url}/webhook/external/${account_id}" },
        { "key": "BOT_ID", "type": "string", "source": "step_output",
          "from_step": "register_bot.bot_id" },
        { "key": "SEND_VOICE", "type": "bool", "source": "input", "default": false }
      ],
      "setup_guide": [
        { "id": "account", "actor": "mira", "title": "Create the MIRA channel account",
          "action": { "verb": "mira.create_channel_account", "provider_kind": "nctalk" },
          "produces": ["account_id", "inbound_secret", "outbound_secret"] },
        { "id": "botsecret", "actor": "mira", "title": "Generate the Talk bot secret",
          "action": { "verb": "mira.mint_secret", "bytes": 40, "encoding": "hex" },
          "produces": ["talk_bot_secret"] },
        { "id": "run", "actor": "mira", "title": "Start the provider",
          "action": { "verb": "mira.write_service", "runtime": "native" },
          "verify": { "type": "tcp", "host": "127.0.0.1", "port": "${config.LISTEN_PORT}" } },
        { "id": "register_bot", "actor": "admin_external", "after": ["botsecret"],
          "title": "Register the bot on Nextcloud",
          "action": { "verb": "command", "run_by": "admin",
            "render": "occ talk:bot:install \"MIRA\" \"${out.botsecret.talk_bot_secret}\" \"${config.public_webhook_url}/talk\"" },
          "produces": ["bot_id"] },
        { "id": "voice_user", "actor": "admin_external", "when": "config.SEND_VOICE == true",
          "title": "Create the voice app-password",
          "action": { "verb": "paste", "label": "App password for the Mira user" },
          "produces": ["nc_app_pass"] },
        { "id": "roundtrip", "actor": "mira", "after": ["register_bot"],
          "title": "Verify end-to-end",
          "action": { "verb": "verify",
            "verify": { "type": "roundtrip", "kind": "cpp",
              "on_fail": { "message": "No reply came back.", "blocking": false } } } }
      ]
    }"#;

    #[derive(Deserialize)]
    struct Wrap {
        config_schema: Vec<ConfigField>,
        setup_guide: Vec<SetupStep>,
    }

    #[test]
    fn parses_and_validates_nextcloud_talk() {
        let w: Wrap = serde_json::from_str(NEXTCLOUD_TALK).unwrap();
        assert_eq!(w.config_schema.len(), 4);
        assert_eq!(w.setup_guide.len(), 6);
        // Verbs decode to the right variants.
        assert!(matches!(
            w.setup_guide[0].action,
            Action::CreateChannelAccount { .. }
        ));
        assert_eq!(w.setup_guide[0].action.verb(), "mira.create_channel_account");
        assert!(matches!(w.setup_guide[2].action, Action::WriteService { .. }));
        assert!(w.setup_guide[3].action.is_mira_automated() == false); // command run_by admin
        validate_wizard(&w.config_schema, &w.setup_guide).unwrap();
    }

    #[test]
    fn rejects_generate_without_spec() {
        let fields = vec![ConfigField {
            key: "X".into(),
            label: None,
            help: None,
            field_type: FieldType::Secret,
            source: FieldSource::Generate,
            secret: true,
            group: None,
            required: true,
            generate: None,
            derive: None,
            from_step: None,
            default: None,
            enum_values: vec![],
            validate: None,
            visible_when: None,
            required_when: None,
            rotate_on_update: false,
        }];
        assert!(validate_wizard(&fields, &[]).is_err());
    }

    #[test]
    fn rejects_unknown_after_and_cycles() {
        // after → nonexistent
        let steps = vec![SetupStep {
            id: "a".into(),
            title: "a".into(),
            body: None,
            actor: Actor::Mira,
            when: None,
            after: vec!["ghost".into()],
            action: Action::Note,
            produces: vec![],
            verify: None,
            run_on: RunOn::Install,
        }];
        assert!(validate_wizard(&[], &steps).is_err());

        // a → b → a cycle
        let mk = |id: &str, dep: &str| SetupStep {
            id: id.into(),
            title: id.into(),
            body: None,
            actor: Actor::Mira,
            when: None,
            after: vec![dep.into()],
            action: Action::Note,
            produces: vec![],
            verify: None,
            run_on: RunOn::Install,
        };
        let cyc = vec![mk("a", "b"), mk("b", "a")];
        assert!(validate_wizard(&[], &cyc).is_err());
    }

    #[test]
    fn rejects_dangling_from_step() {
        let fields = vec![ConfigField {
            key: "X".into(),
            label: None,
            help: None,
            field_type: FieldType::String,
            source: FieldSource::StepOutput,
            secret: false,
            group: None,
            required: false,
            generate: None,
            derive: None,
            from_step: Some("nope.bot_id".into()),
            default: None,
            enum_values: vec![],
            validate: None,
            visible_when: None,
            required_when: None,
            rotate_on_update: false,
        }];
        assert!(validate_wizard(&fields, &[]).is_err());
    }
}
