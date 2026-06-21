// SPDX-License-Identifier: AGPL-3.0-or-later

//! The `setup_guide` wizard engine (, slice 3) — the resumable executor
//! that drives a `cpp_provider` install (design-docs/plugin-packages.md §"Install &
//! lifecycle").
//!
//! It walks the manifest's `setup_guide` steps in dependency order, runs the
//! automated `mira.*` verbs itself, and **pauses** at human steps (the admin
//! runs an `occ` command, pastes an app-password, does something in a
//! third-party UI). A [`ProvisionSession`] is the serializable state in between:
//! it persists, so the install resumes exactly where it left off — across
//! page reloads and across the minutes/hours an external step can take.
//!
//! The engine is pure logic over a [`WizardHost`] (the side-effect surface:
//! mint a secret, create a channel account, set a setting, store a secret, run
//! a probe). The real host wraps MIRA's stores; tests use a fake. This keeps
//! the DAG walk, templating, idempotency, and resume semantics testable without
//! a database.
//!
//! ## Scope
//!
//! `mira.write_service` (run a same-host provider as a MIRA-managed service)
//! runs on Linux (systemd `--user`); elsewhere it returns a clear error so the
//! install falls back to connection-only (the admin runs the provider). The
//! `roundtrip {kind:"cpp"}` probe posts a signed health-check inbound to MIRA's
//! own webhook to prove the account is live + the minted secret verifies. Still
//! deferred: `command { run_by: mira }` (MIRA executing arbitrary shell) and the
//! `mcp` roundtrip variant.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::store::{Ledger, LedgerEntry};
use super::wizard::{Action, Actor, Encoding, SetupStep, VerifyProbe};

// ---------------------------------------------------------------------------
// Host surface (side effects)
// ---------------------------------------------------------------------------

// What a `mira.create_channel_account` step needs.
#[derive(Debug, Clone)]
pub struct CreateAccountReq {
    pub provider_kind: String,
    pub account_label: String,
    // Where MIRA POSTs outbound replies (the provider's endpoint). May be
    // empty if the provider's address isn't known yet at create time.
    pub send_url: String,
    pub mention_only: bool,
    pub supports_voice: bool,
}

// What it produced. These become the step's outputs.
#[derive(Debug, Clone)]
pub struct CreatedAccount {
    pub account_id: String,
    pub inbound_secret: String,
    pub outbound_secret: String,
    pub send_url: String,
}

// A verify probe with all templates already resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedProbe {
    Http { url: String, method: String, expect_status: Option<u16> },
    Tcp { host: String, port: u16 },
    CommandExit { command: String, expect: i32 },
    // End-to-end: POST a signed health-check inbound to MIRA's own webhook and
    // expect it accepted — proving the account is live and the minted
    // `inbound_secret` verifies. `account_id`/`inbound_secret` come from the
    // create-account step's outputs (`mcp` kind is reported skipped).
    Roundtrip { kind: String, url: String, account_id: String, inbound_secret: String },
}

// Outcome of running a probe.
pub enum ProbeOutcome {
    Pass,
    Fail(String),
    Skipped(String),
}

// Result of running a shell command (a `command { run_by: mira }` step or a
// `command_exit` probe).
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

// The side effects the engine performs. The real implementation wraps the
// channel-account store, the secret vault, the settings store, and a probe
// client; tests fake it.
pub trait WizardHost {
    // Mint `bytes` random bytes in the given encoding.
    fn mint_secret(&self, bytes: usize, encoding: Encoding) -> Result<String, String>;
    // Create a CPP/External channel account (idempotent at the store layer is
    // not assumed — the engine guards re-runs via session state).
    fn create_channel_account(
        &self,
        admin_id: &str,
        req: CreateAccountReq,
    ) -> Result<CreatedAccount, String>;
    // Toggle a MIRA setting.
    fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<(), String>;
    // Store a secret in the encrypted vault under the package id.
    fn store_secret(&self, package_id: &str, key: &str, value: &str) -> Result<(), String>;
    // Read a vaulted secret (used to preserve secrets across an update).
    fn get_secret(&self, package_id: &str, key: &str) -> Option<String>;
    // MIRA's public base URL (for `${mira.base_url}`), no trailing slash.
    fn base_url(&self) -> String;
    // Run a resolved verify probe.
    fn run_probe(&self, probe: &ResolvedProbe) -> ProbeOutcome;
    // Install + start a same-host provider service. Returns an opaque handle
    // (the systemd unit name) recorded in the ledger for teardown.
    fn write_service(&self, spec: super::service::ServiceSpec) -> Result<String, String>;
    // Run a shell command (for `command { run_by: mira }` + the `command_exit`
    // probe), in `cwd` when non-empty. Runs as the MIRA user, unconfined — only
    // reachable from an admin-installed, trust-gated manifest.
    fn run_command(&self, command: &str, cwd: &str) -> Result<CommandResult, String>;
}

// ---------------------------------------------------------------------------
// Session state (persisted between steps)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    // Engine is actively running automated steps.
    InProgress,
    // Paused on a human step — the admin must act (see the `awaiting` step).
    AwaitingInput,
    // All steps done/skipped — caller should finalize (write the package row).
    Complete,
    // A blocking step failed — caller should roll back the ledger.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Done,
    Skipped,
    AwaitingInput,
    Failed,
}

// Per-step runtime state, surfaced to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepState {
    pub id: String,
    pub title: String,
    pub actor: Actor,
    pub verb: String,
    pub status: StepStatus,
    // The templated body / command, ready to show or copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<String>,
    // A human-readable note: a verify warning, a failure reason, instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    // For a `paste` step: the output keys the admin must supply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub awaiting_outputs: Vec<String>,
}

// The serializable wizard state. Persist this between calls; resume by loading
// it and calling [`advance`] (or [`submit_step`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionSession {
    pub package_id: String,
    pub admin_id: String,
    // Resolved config values (`input` + minted `generate`). `derive` /
    // `step_output` fields resolve lazily through [`outputs`] at template time.
    pub config: BTreeMap<String, serde_json::Value>,
    // `step_id` → (`output_key` → value).
    pub outputs: BTreeMap<String, BTreeMap<String, String>>,
    pub steps: Vec<StepState>,
    pub ledger: Ledger,
    pub status: SessionStatus,
    // ── finalize metadata ──────────────────────────────────────────────────
    // Carried with the session so the install handler can write the package
    // record (and reverse the ledger on cancel) when the wizard completes,
    // without re-uploading the bundle. The engine itself ignores these.
    #[serde(default)]
    pub manifest: serde_json::Value,
    #[serde(default)]
    pub trust: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub name: String,
    // The package's extracted payload dir — the working dir + relative-command
    // base for a `mira.write_service` step. Set by the install handler.
    #[serde(default)]
    pub install_dir: String,
}

impl ProvisionSession {
    // The step currently blocked on the admin, if any.
    pub fn awaiting(&self) -> Option<&StepState> {
        self.steps.iter().find(|s| s.status == StepStatus::AwaitingInput)
    }
    fn step_status(&self, id: &str) -> Option<StepStatus> {
        self.steps.iter().find(|s| s.id == id).map(|s| s.status)
    }
}

// ---------------------------------------------------------------------------
// Begin / advance / submit
// ---------------------------------------------------------------------------

// Start a session: take the admin's `input` answers, mint `generate` secrets,
// build the step list, then [`advance`] through the automated prefix.
pub fn begin(
    package_id: &str,
    admin_id: &str,
    config_schema: &[super::wizard::ConfigField],
    setup_guide: &[SetupStep],
    admin_input: &BTreeMap<String, serde_json::Value>,
    install_dir: &str,
    host: &dyn WizardHost,
) -> Result<ProvisionSession, String> {
    use super::wizard::FieldSource;

    let mut config: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    // Resolve `input` and `generate` fields up front. `derive`/`step_output`
    // are left to lazy resolution (they may depend on step outputs).
    for f in config_schema {
        match f.source {
            FieldSource::Input => {
                if let Some(v) = admin_input.get(&f.key) {
                    config.insert(f.key.clone(), v.clone());
                } else if let Some(d) = &f.default {
                    config.insert(f.key.clone(), d.clone());
                } else if f.required {
                    return Err(format!("required config field {:?} was not provided", f.key));
                }
                // A secret input is vaulted (and kept out of the plaintext config).
                if f.secret {
                    if let Some(serde_json::Value::String(s)) = config.get(&f.key) {
                        host.store_secret(package_id, &f.key, s)?;
                    }
                }
            }
            FieldSource::Generate => {
                let spec = f.generate.as_ref().ok_or_else(|| {
                    format!("config field {:?} is source=generate but has no spec", f.key)
                })?;
                let val = host.mint_secret(spec.bytes, spec.encoding)?;
                if f.secret {
                    host.store_secret(package_id, &f.key, &val)?;
                }
                config.insert(f.key.clone(), serde_json::Value::String(val));
            }
            FieldSource::Derive | FieldSource::StepOutput => {}
        }
    }

    let steps = setup_guide
        .iter()
        .map(|s| StepState {
            id: s.id.clone(),
            title: s.title.clone(),
            actor: s.actor,
            verb: s.action.verb().to_string(),
            status: StepStatus::Pending,
            render: None,
            message: None,
            awaiting_outputs: Vec::new(),
        })
        .collect();

    let mut session = ProvisionSession {
        package_id: package_id.to_string(),
        admin_id: admin_id.to_string(),
        config,
        outputs: BTreeMap::new(),
        steps,
        ledger: Vec::new(),
        status: SessionStatus::InProgress,
        manifest: serde_json::Value::Null,
        trust: String::new(),
        version: String::new(),
        name: String::new(),
        install_dir: install_dir.to_string(),
    };
    advance(&mut session, config_schema, setup_guide, host);
    Ok(session)
}

// Start an **update** session: seed config from the prior install (already
// migrated by the caller), preserve generated secrets (re-mint only those
// flagged `rotate_on_update`), and run **only** the `update`/`both` steps. The
// full `setup_guide` is still passed so [`advance`] can resolve templates and
// honour `after` deps on `install`-only steps (treated as already done).
#[allow(clippy::too_many_arguments)]
pub fn begin_update(
    package_id: &str,
    admin_id: &str,
    config_schema: &[super::wizard::ConfigField],
    setup_guide: &[SetupStep],
    prior_config: &BTreeMap<String, serde_json::Value>,
    admin_input: &BTreeMap<String, serde_json::Value>,
    install_dir: &str,
    host: &dyn WizardHost,
) -> Result<ProvisionSession, String> {
    use super::wizard::FieldSource;

    let mut config: BTreeMap<String, serde_json::Value> = prior_config.clone();
    for f in config_schema {
        match f.source {
            FieldSource::Input if f.secret => {
                // Secret inputs live in the vault, not the plaintext seed config.
                if let Some(serde_json::Value::String(s)) = admin_input.get(&f.key) {
                    host.store_secret(package_id, &f.key, s)?;
                    config.insert(f.key.clone(), serde_json::Value::String(s.clone()));
                } else if let Some(existing) = host.get_secret(package_id, &f.key) {
                    config.insert(f.key.clone(), serde_json::Value::String(existing));
                } else if let Some(d) = &f.default {
                    config.insert(f.key.clone(), d.clone());
                } else if f.required {
                    return Err(format!("required config field {:?} was not provided", f.key));
                }
            }
            FieldSource::Input => {
                if let Some(v) = admin_input.get(&f.key) {
                    config.insert(f.key.clone(), v.clone());
                } else if config.contains_key(&f.key) {
                    // preserved from the prior install
                } else if let Some(d) = &f.default {
                    config.insert(f.key.clone(), d.clone());
                } else if f.required {
                    return Err(format!("required config field {:?} was not provided", f.key));
                }
            }
            FieldSource::Generate => {
                let spec = f.generate.as_ref().ok_or_else(|| {
                    format!("config field {:?} is source=generate but has no spec", f.key)
                })?;
                // Preserve the existing secret unless the field opts into rotation
                // (or is brand-new in this version → mint it).
                let val = if !f.rotate_on_update {
                    match host.get_secret(package_id, &f.key) {
                        Some(existing) => existing,
                        None => host.mint_secret(spec.bytes, spec.encoding)?,
                    }
                } else {
                    host.mint_secret(spec.bytes, spec.encoding)?
                };
                if f.secret {
                    host.store_secret(package_id, &f.key, &val)?;
                }
                config.insert(f.key.clone(), serde_json::Value::String(val));
            }
            FieldSource::Derive | FieldSource::StepOutput => {}
        }
    }

    let steps = setup_guide
        .iter()
        .filter(|s| s.run_on.on_update())
        .map(|s| StepState {
            id: s.id.clone(),
            title: s.title.clone(),
            actor: s.actor,
            verb: s.action.verb().to_string(),
            status: StepStatus::Pending,
            render: None,
            message: None,
            awaiting_outputs: Vec::new(),
        })
        .collect();

    let mut session = ProvisionSession {
        package_id: package_id.to_string(),
        admin_id: admin_id.to_string(),
        config,
        outputs: BTreeMap::new(),
        steps,
        ledger: Vec::new(),
        status: SessionStatus::InProgress,
        manifest: serde_json::Value::Null,
        trust: String::new(),
        version: String::new(),
        name: String::new(),
        install_dir: install_dir.to_string(),
    };
    advance(&mut session, config_schema, setup_guide, host);
    Ok(session)
}

// Drive the session forward: run every ready automated step until we hit a
// human step (→ `AwaitingInput`), exhaust the steps (→ `Complete`), or hit a
// blocking failure (→ `Failed`). Idempotent on already-`Done`/`Skipped` steps,
// so resuming a persisted session is safe.
pub fn advance(
    session: &mut ProvisionSession,
    config_schema: &[super::wizard::ConfigField],
    setup_guide: &[SetupStep],
    host: &dyn WizardHost,
) {
    if matches!(session.status, SessionStatus::Failed | SessionStatus::Complete) {
        return;
    }
    for step in setup_guide {
        // Fold any now-resolvable derive / step_output config fields into
        // `config` so this step's `when`, render, and verify see them.
        resolve_pending_config(session, config_schema, host);
        let cur = match session.step_status(&step.id) {
            Some(s) => s,
            None => continue,
        };
        if matches!(cur, StepStatus::Done | StepStatus::Skipped) {
            continue;
        }
        if cur == StepStatus::AwaitingInput {
            // Still waiting on the admin — can't pass it.
            session.status = SessionStatus::AwaitingInput;
            return;
        }
        // A predecessor (via `after`) that isn't finished blocks us.
        if !deps_ready(session, step) {
            return;
        }
        // `when` gate.
        if let Some(expr) = &step.when {
            match eval_when(expr, session) {
                Ok(true) => {}
                Ok(false) => {
                    set_step(session, &step.id, StepStatus::Skipped, None, None, &[]);
                    continue;
                }
                Err(e) => {
                    fail_step(session, &step.id, format!("could not evaluate `when`: {e}"));
                    return;
                }
            }
        }

        if step.action.is_mira_automated() {
            match run_automated(session, step, host) {
                Ok(()) => continue,
                Err(msg) => {
                    fail_step(session, &step.id, msg);
                    return;
                }
            }
        } else {
            // Human step. `note` is informational → mark Done immediately.
            match &step.action {
                Action::Note => {
                    let body = step
                        .body
                        .as_ref()
                        .map(|b| resolve(b, session, host).unwrap_or_else(|_| b.clone()));
                    set_step(session, &step.id, StepStatus::Done, body, None, &[]);
                    continue;
                }
                _ => {
                    present_human_step(session, step, host);
                    session.status = SessionStatus::AwaitingInput;
                    return;
                }
            }
        }
    }
    session.status = SessionStatus::Complete;
}

// Submit a human step's result (admin pasted a value / confirmed they ran a
// command), record its outputs, mark it Done, then [`advance`].
pub fn submit_step(
    session: &mut ProvisionSession,
    step_id: &str,
    outputs: BTreeMap<String, String>,
    config_schema: &[super::wizard::ConfigField],
    setup_guide: &[SetupStep],
    host: &dyn WizardHost,
) -> Result<(), String> {
    let step = setup_guide
        .iter()
        .find(|s| s.id == step_id)
        .ok_or_else(|| format!("no such step {step_id:?}"))?;
    if session.step_status(step_id) != Some(StepStatus::AwaitingInput) {
        return Err(format!("step {step_id:?} is not awaiting input"));
    }
    // Persist any `paste` secrets to the vault rather than the plaintext blob.
    if let Action::Paste { secret: true, .. } = &step.action {
        for (k, v) in &outputs {
            host.store_secret(&session.package_id, k, v)?;
        }
    }
    if !outputs.is_empty() {
        session.outputs.insert(step_id.to_string(), outputs);
    }
    set_step(session, step_id, StepStatus::Done, None, None, &[]);
    // Clear the AwaitingInput status so advance() can proceed.
    if session.status == SessionStatus::AwaitingInput {
        session.status = SessionStatus::InProgress;
    }
    advance(session, config_schema, setup_guide, host);
    Ok(())
}

// ---------------------------------------------------------------------------
// Step execution
// ---------------------------------------------------------------------------

// Are all of `step.after` finished (Done or Skipped)? A dep that isn't in this
// session's step list ran in a prior phase (an `install`-only step during an
// update) and counts as satisfied — `validate_wizard` already guarantees every
// `after` names a real step, so an absent one was filtered, not a typo.
fn deps_ready(session: &ProvisionSession, step: &SetupStep) -> bool {
    step.after.iter().all(|dep| match session.step_status(dep) {
        Some(s) => matches!(s, StepStatus::Done | StepStatus::Skipped),
        None => true,
    })
}

fn run_automated(
    session: &mut ProvisionSession,
    step: &SetupStep,
    host: &dyn WizardHost,
) -> Result<(), String> {
    match &step.action {
        Action::MintSecret { bytes, encoding } => {
            let val = host
                .mint_secret(*bytes, *encoding)
                ?;
            // The canonical output is `secret`; also alias to the first declared
            // `produces` key so manifests can name it whatever they like.
            let mut out = BTreeMap::new();
            out.insert("secret".to_string(), val.clone());
            if let Some(first) = step.produces.first() {
                out.insert(first.clone(), val);
            }
            record_outputs(session, &step.id, out);
            set_step(session, &step.id, StepStatus::Done, None, None, &[]);
            Ok(())
        }
        Action::CreateChannelAccount {
            provider_kind,
            mention_only,
            supports_voice,
        } => {
            // send_url comes from config (the provider's endpoint) if declared.
            let send_url = lookup_config_str(session, host, "send_url")
                .or_else(|| lookup_config_str(session, host, "SEND_URL"))
                .unwrap_or_default();
            let created = host
                .create_channel_account(
                    &session.admin_id,
                    CreateAccountReq {
                        provider_kind: provider_kind.clone(),
                        account_label: format!("{} ({})", session.package_id, provider_kind),
                        send_url,
                        mention_only: *mention_only,
                        supports_voice: *supports_voice,
                    },
                )
                ?;
            session
                .ledger
                .push(LedgerEntry::ChannelAccount { id: created.account_id.clone() });
            let mut out = BTreeMap::new();
            out.insert("account_id".to_string(), created.account_id);
            out.insert("inbound_secret".to_string(), created.inbound_secret);
            out.insert("outbound_secret".to_string(), created.outbound_secret);
            out.insert("send_url".to_string(), created.send_url);
            // CPP HMAC secrets are sensitive — vault them under the package id.
            if let Some(s) = out.get("inbound_secret") {
                let _ = host.store_secret(&session.package_id, "inbound_secret", s);
                session.ledger.push(LedgerEntry::Secret { key: "inbound_secret".into() });
            }
            if let Some(s) = out.get("outbound_secret") {
                let _ = host.store_secret(&session.package_id, "outbound_secret", s);
                session.ledger.push(LedgerEntry::Secret { key: "outbound_secret".into() });
            }
            record_outputs(session, &step.id, out);
            set_step(session, &step.id, StepStatus::Done, None, None, &[]);
            Ok(())
        }
        Action::SetSetting { key, value } => {
            host.set_setting(key, value)?;
            set_step(session, &step.id, StepStatus::Done, None, None, &[]);
            Ok(())
        }
        Action::Verify { verify } => run_verify(session, step, verify, host),
        // A command MIRA runs itself: execute it in the payload dir. A
        // single `produces` key captures trimmed stdout as that output.
        Action::Command { run_by: Actor::Mira, render } => {
            let cmd = resolve(render, session, host)?;
            let r = host.run_command(&cmd, &session.install_dir)?;
            if r.code != 0 {
                return Err(format!(
                    "command exited {}: {}",
                    r.code,
                    if r.stderr.trim().is_empty() { r.stdout.trim() } else { r.stderr.trim() }
                ));
            }
            if let Some(key) = step.produces.first() {
                let mut out = BTreeMap::new();
                out.insert(key.clone(), r.stdout.trim().to_string());
                record_outputs(session, &step.id, out);
            }
            set_step(session, &step.id, StepStatus::Done, None, None, &[]);
            Ok(())
        }
        Action::WriteService { command, args, env, .. } => {
            let command = command
                .as_ref()
                .ok_or("mira.write_service step needs a `command` (the provider entrypoint)")?;
            // Resolve the entrypoint: relative paths sit under the payload dir.
            let cmd_resolved = resolve(command, session, host)?;
            let install_dir = std::path::PathBuf::from(&session.install_dir);
            let cmd_path = {
                let p = std::path::PathBuf::from(&cmd_resolved);
                if p.is_absolute() { p } else { install_dir.join(p) }
            };
            let mut rargs = Vec::with_capacity(args.len());
            for a in args {
                rargs.push(resolve(a, session, host)?);
            }
            let mut renv = BTreeMap::new();
            for (k, v) in env {
                renv.insert(k.clone(), resolve(v, session, host)?);
            }
            let unit = host.write_service(super::service::ServiceSpec {
                package_id: session.package_id.clone(),
                description: format!("MIRA plugin provider — {}", session.package_id),
                command: cmd_path,
                args: rargs,
                env: renv,
                working_dir: install_dir,
            })?;
            session.ledger.push(LedgerEntry::Service { unit });
            set_step(session, &step.id, StepStatus::Done, None, None, &[]);
            Ok(())
        }
        Action::RegisterMcpServer => Err(
            "mira.register_mcp_server is not yet wired into the wizard (use an mcp_server \
             component for now)"
                .into(),
        ),
        // Human verbs never reach here (is_mira_automated() is false).
        Action::Command { .. }
        | Action::Paste { .. }
        | Action::ExternalUi { .. }
        | Action::Note => Err(format!(
            "internal: human verb {} routed to automated path",
            step.action.verb()
        )),
    }
}

fn run_verify(
    session: &mut ProvisionSession,
    step: &SetupStep,
    probe: &VerifyProbe,
    host: &dyn WizardHost,
) -> Result<(), String> {
    let resolved = match resolve_probe(probe, session, host) {
        Ok(p) => p,
        Err(e) => return Err(format!("resolve probe: {e}")),
    };
    let outcome = host.run_probe(&resolved);
    match outcome {
        ProbeOutcome::Pass => {
            set_step(session, &step.id, StepStatus::Done, None, None, &[]);
            Ok(())
        }
        ProbeOutcome::Skipped(why) => {
            set_step(session, &step.id, StepStatus::Done, None, Some(why), &[]);
            Ok(())
        }
        ProbeOutcome::Fail(why) => {
            let blocking = probe.on_fail().map(|f| f.blocking).unwrap_or(true);
            let msg = probe
                .on_fail()
                .map(|f| format!("{} ({why})", f.message))
                .unwrap_or(why);
            if blocking {
                Err(msg)
            } else {
                // Non-blocking: pass with a warning the admin can read.
                set_step(session, &step.id, StepStatus::Done, None, Some(msg), &[]);
                Ok(())
            }
        }
    }
}

// Render a human step (templated body / command) and park it on AwaitingInput.
fn present_human_step(session: &mut ProvisionSession, step: &SetupStep, host: &dyn WizardHost) {
    let mut render = step
        .body
        .as_ref()
        .map(|b| resolve(b, session, host).unwrap_or_else(|_| b.clone()));
    let mut awaiting: Vec<String> = Vec::new();
    match &step.action {
        Action::Command { render: tmpl, .. } => {
            render = Some(resolve(tmpl, session, host).unwrap_or_else(|_| tmpl.clone()));
            // If the step declares outputs, the admin must paste them back.
            awaiting = step.produces.clone();
        }
        Action::Paste { label, output, .. } => {
            render = Some(label.clone());
            awaiting = output
                .clone()
                .map(|o| vec![o])
                .unwrap_or_else(|| step.produces.clone());
        }
        Action::ExternalUi { label } => {
            if render.is_none() {
                render = label.clone();
            }
            awaiting = step.produces.clone();
        }
        _ => {}
    }
    set_step(session, &step.id, StepStatus::AwaitingInput, render, None, &awaiting);
}

// ---------------------------------------------------------------------------
// State mutation helpers
// ---------------------------------------------------------------------------

fn set_step(
    session: &mut ProvisionSession,
    id: &str,
    status: StepStatus,
    render: Option<String>,
    message: Option<String>,
    awaiting: &[String],
) {
    if let Some(s) = session.steps.iter_mut().find(|s| s.id == id) {
        s.status = status;
        if render.is_some() {
            s.render = render;
        }
        if message.is_some() {
            s.message = message;
        }
        s.awaiting_outputs = awaiting.to_vec();
    }
}

fn fail_step(session: &mut ProvisionSession, id: &str, msg: String) {
    set_step(session, id, StepStatus::Failed, None, Some(msg), &[]);
    session.status = SessionStatus::Failed;
}

fn record_outputs(session: &mut ProvisionSession, step_id: &str, out: BTreeMap<String, String>) {
    session.outputs.entry(step_id.to_string()).or_default().extend(out);
}

// ---------------------------------------------------------------------------
// Templating + expression evaluation
// ---------------------------------------------------------------------------

// Resolve `${…}` tokens in `template` against the session + host facts.
// Supported references: `mira.base_url`, `config.KEY`, `out.STEP.FIELD`, and a
// bare `NAME` (searched across all step outputs, then config).
pub fn resolve(
    template: &str,
    session: &ProvisionSession,
    host: &dyn WizardHost,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let end = template[i + 2..]
                .find('}')
                .map(|p| i + 2 + p)
                .ok_or_else(|| format!("unterminated ${{ in template: {template:?}"))?;
            let token = &template[i + 2..end];
            out.push_str(&resolve_token(token.trim(), session, host)?);
            i = end + 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

fn resolve_token(
    token: &str,
    session: &ProvisionSession,
    host: &dyn WizardHost,
) -> Result<String, String> {
    if token == "mira.base_url" {
        return Ok(host.base_url().trim_end_matches('/').to_string());
    }
    if let Some(key) = token.strip_prefix("config.") {
        return lookup_config_str(session, host, key)
            .ok_or_else(|| format!("config field {key:?} has no value yet"));
    }
    if let Some(rest) = token.strip_prefix("out.") {
        let (step, field) = rest
            .split_once('.')
            .ok_or_else(|| format!("bad out reference {token:?} (want out.<step>.<field>)"))?;
        return session
            .outputs
            .get(step)
            .and_then(|m| m.get(field))
            .cloned()
            .ok_or_else(|| format!("step {step:?} has not produced {field:?} yet"));
    }
    // Bare name: search step outputs first (e.g. `account_id`), then config.
    for m in session.outputs.values() {
        if let Some(v) = m.get(token) {
            return Ok(v.clone());
        }
    }
    lookup_config_str(session, host, token)
        .ok_or_else(|| format!("unknown template reference {token:?}"))
}

// Resolve a config value to a string (only `input`/`generate`/already-folded
// values; `derive`/`step_output` are folded into `config` by
// [`resolve_pending_config`] as their dependencies become available).
fn lookup_config_str(session: &ProvisionSession, _host: &dyn WizardHost, key: &str) -> Option<String> {
    session.config.get(key).map(value_to_str)
}

// Fold every `derive` / `step_output` config field that can now be resolved
// into `session.config`, iterating to a fixpoint (a derive may depend on
// another). Secret fields are vaulted once, on first resolution.
fn resolve_pending_config(
    session: &mut ProvisionSession,
    config_schema: &[super::wizard::ConfigField],
    host: &dyn WizardHost,
) {
    use super::wizard::FieldSource;
    loop {
        let mut to_insert: Vec<(String, String, bool)> = Vec::new();
        for f in config_schema {
            if session.config.contains_key(&f.key) {
                continue;
            }
            match f.source {
                FieldSource::Derive => {
                    if let Some(tmpl) = &f.derive {
                        if let Ok(val) = resolve(tmpl, session, host) {
                            to_insert.push((f.key.clone(), val, f.secret));
                        }
                    }
                }
                FieldSource::StepOutput => {
                    if let Some(from) = &f.from_step {
                        if let Some((sid, fld)) = from.split_once('.') {
                            if let Some(v) = session.outputs.get(sid).and_then(|m| m.get(fld)) {
                                to_insert.push((f.key.clone(), v.clone(), f.secret));
                            }
                        }
                    }
                }
                FieldSource::Input | FieldSource::Generate => {}
            }
        }
        if to_insert.is_empty() {
            break;
        }
        for (key, val, secret) in to_insert {
            if secret {
                let _ = host.store_secret(&session.package_id, &key, &val);
            }
            session.config.insert(key, serde_json::Value::String(val));
        }
    }
}

fn value_to_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// Resolve a [`VerifyProbe`] into a [`ResolvedProbe`] with templates expanded.
fn resolve_probe(
    probe: &VerifyProbe,
    session: &ProvisionSession,
    host: &dyn WizardHost,
) -> Result<ResolvedProbe, String> {
    Ok(match probe {
        VerifyProbe::Http { url, method, expect_status, .. } => ResolvedProbe::Http {
            url: resolve(url, session, host)?,
            method: method.clone().unwrap_or_else(|| "GET".into()),
            expect_status: *expect_status,
        },
        VerifyProbe::Tcp { host: h, port, .. } => {
            let host_s = resolve(h, session, host)?;
            let port_s = resolve(port, session, host)?;
            let port = port_s
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("verify port {port_s:?} is not a valid port"))?;
            ResolvedProbe::Tcp { host: host_s, port }
        }
        VerifyProbe::CommandExit { command, expect, .. } => ResolvedProbe::CommandExit {
            command: resolve(command, session, host)?,
            expect: *expect,
        },
        VerifyProbe::Roundtrip { kind, url, .. } if kind == "mcp" => {
            // MCP handshake targets the server's HTTP endpoint (templated).
            let resolved = match url {
                Some(u) => resolve(u, session, host)?,
                None => String::new(),
            };
            ResolvedProbe::Roundtrip {
                kind: kind.clone(),
                url: resolved,
                account_id: String::new(),
                inbound_secret: String::new(),
            }
        }
        VerifyProbe::Roundtrip { kind, .. } => {
            // cpp: the signed health-check posts to MIRA's own webhook for the
            // account the create-account step produced.
            let account_id = output_lookup(session, "account_id").unwrap_or_default();
            let inbound_secret = output_lookup(session, "inbound_secret").unwrap_or_default();
            let base = host.base_url();
            let url = format!("{}/webhook/external/{}", base.trim_end_matches('/'), account_id);
            ResolvedProbe::Roundtrip { kind: kind.clone(), url, account_id, inbound_secret }
        }
    })
}

// Find an output value by bare key across all steps (e.g. `account_id`).
fn output_lookup(session: &ProvisionSession, key: &str) -> Option<String> {
    session.outputs.values().find_map(|m| m.get(key).cloned())
}

// Evaluate a minimal `when` expression. Supports `<ref> == <lit>`,
// `<ref> != <lit>`, and a bare `<ref>` (truthy). `<ref>` is `config.KEY` or a
// bare name; `<lit>` is `true`/`false`, a number, or a `"quoted"` string.
fn eval_when(expr: &str, session: &ProvisionSession) -> Result<bool, String> {
    let expr = expr.trim();
    for (op, negate) in [("==", false), ("!=", true)] {
        if let Some((lhs, rhs)) = expr.split_once(op) {
            let lv = ref_value(lhs.trim(), session);
            let rv = literal_value(rhs.trim());
            let eq = lv == rv;
            return Ok(if negate { !eq } else { eq });
        }
    }
    // Bare reference → truthy.
    Ok(truthy(&ref_value(expr, session)))
}

// The value a `config.KEY` / bare reference holds, as JSON (Null if unset).
fn ref_value(reference: &str, session: &ProvisionSession) -> serde_json::Value {
    let key = reference.strip_prefix("config.").unwrap_or(reference);
    if let Some(v) = session.config.get(key) {
        return v.clone();
    }
    for m in session.outputs.values() {
        if let Some(v) = m.get(key) {
            return serde_json::Value::String(v.clone());
        }
    }
    serde_json::Value::Null
}

fn literal_value(lit: &str) -> serde_json::Value {
    match lit {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => {
            if let Ok(n) = lit.parse::<i64>() {
                return serde_json::Value::from(n);
            }
            let unquoted = lit.trim_matches(|c| c == '"' || c == '\'');
            serde_json::Value::String(unquoted.to_string())
        }
    }
}

fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.is_empty() && s != "false",
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use crate::packages::wizard::{ConfigField, FieldSource, FieldType, GenerateSpec};

    // A deterministic, recording fake host.
    struct FakeHost {
        base: String,
        accounts: RefCell<u32>,
        secrets: RefCell<Vec<(String, String, String)>>,
        settings: RefCell<Vec<(String, serde_json::Value)>>,
        services: RefCell<Vec<(String, Vec<String>, BTreeMap<String, String>)>>,
        commands: RefCell<Vec<(String, String)>>,
        probe_pass: bool,
    }
    impl Default for FakeHost {
        fn default() -> Self {
            FakeHost {
                base: "https://mira.example.com".into(),
                accounts: RefCell::new(0),
                secrets: RefCell::new(vec![]),
                settings: RefCell::new(vec![]),
                services: RefCell::new(vec![]),
                commands: RefCell::new(vec![]),
                probe_pass: true,
            }
        }
    }
    impl WizardHost for FakeHost {
        fn mint_secret(&self, bytes: usize, _enc: Encoding) -> Result<String, String> {
            // Deterministic "random": repeat a byte; encode hex.
            Ok(hex::encode(vec![0xABu8; bytes]))
        }
        fn create_channel_account(
            &self,
            _admin: &str,
            req: CreateAccountReq,
        ) -> Result<CreatedAccount, String> {
            *self.accounts.borrow_mut() += 1;
            Ok(CreatedAccount {
                account_id: format!("acct-{}", self.accounts.borrow()),
                inbound_secret: "in-secret".into(),
                outbound_secret: "out-secret".into(),
                send_url: req.send_url,
            })
        }
        fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<(), String> {
            self.settings.borrow_mut().push((key.to_string(), value.clone()));
            Ok(())
        }
        fn store_secret(&self, pkg: &str, key: &str, value: &str) -> Result<(), String> {
            self.secrets
                .borrow_mut()
                .push((pkg.to_string(), key.to_string(), value.to_string()));
            Ok(())
        }
        fn get_secret(&self, pkg: &str, key: &str) -> Option<String> {
            self.secrets
                .borrow()
                .iter()
                .rev()
                .find(|(p, k, _)| p == pkg && k == key)
                .map(|(_, _, v)| v.clone())
        }
        fn base_url(&self) -> String {
            self.base.clone()
        }
        fn run_probe(&self, probe: &ResolvedProbe) -> ProbeOutcome {
            if let ResolvedProbe::Roundtrip { .. } = probe {
                return ProbeOutcome::Skipped("roundtrip deferred in v1".into());
            }
            if self.probe_pass {
                ProbeOutcome::Pass
            } else {
                ProbeOutcome::Fail("unreachable".into())
            }
        }
        fn write_service(&self, spec: super::super::service::ServiceSpec) -> Result<String, String> {
            self.services
                .borrow_mut()
                .push((spec.command.to_string_lossy().to_string(), spec.args.clone(), spec.env.clone()));
            Ok(super::super::service::unit_name(&spec.package_id))
        }
        fn run_command(&self, command: &str, cwd: &str) -> Result<CommandResult, String> {
            self.commands.borrow_mut().push((command.to_string(), cwd.to_string()));
            // Deterministic: a command containing "FAIL" exits non-zero.
            let code = if command.contains("FAIL") { 1 } else { 0 };
            Ok(CommandResult { code, stdout: "fake-out".into(), stderr: "boom".into() })
        }
    }

    fn field(key: &str, source: FieldSource) -> ConfigField {
        ConfigField {
            key: key.into(),
            label: None,
            help: None,
            field_type: FieldType::String,
            source,
            secret: false,
            group: None,
            required: false,
            generate: None,
            derive: None,
            from_step: None,
            default: None,
            enum_values: vec![],
            validate: None,
            visible_when: None,
            required_when: None,
            rotate_on_update: false,
        }
    }

    fn step(id: &str, actor: Actor, action: Action) -> SetupStep {
        SetupStep {
            id: id.into(),
            title: id.into(),
            body: None,
            actor,
            when: None,
            after: vec![],
            action,
            produces: vec![],
            verify: None,
            run_on: crate::packages::wizard::RunOn::Install,
        }
    }

    #[test]
    fn runs_automated_prefix_then_pauses_on_human_step() {
        let host = FakeHost::default();
        let schema: Vec<ConfigField> = vec![];
        let guide = vec![
            step(
                "account",
                Actor::Mira,
                Action::CreateChannelAccount {
                    provider_kind: "nctalk".into(),
                    mention_only: false,
                    supports_voice: false,
                },
            ),
            {
                let mut s = step(
                    "register",
                    Actor::AdminExternal,
                    Action::Command {
                        run_by: Actor::Admin,
                        render: "occ talk:bot:install ${out.account.account_id}".into(),
                    },
                );
                s.produces = vec!["bot_id".into()];
                s
            },
        ];
        let session = begin("com.x.talk", "admin1", &schema, &guide, &BTreeMap::new(), "", &host).unwrap();
        // create_channel_account ran; we're paused on the human command step.
        assert_eq!(*host.accounts.borrow(), 1);
        assert_eq!(session.status, SessionStatus::AwaitingInput);
        let awaiting = session.awaiting().unwrap();
        assert_eq!(awaiting.id, "register");
        // The command was templated with the produced account_id.
        assert_eq!(
            awaiting.render.as_deref(),
            Some("occ talk:bot:install acct-1")
        );
        assert_eq!(awaiting.awaiting_outputs, vec!["bot_id".to_string()]);
        // The CPP secrets were vaulted, not left in the blob.
        let vaulted: Vec<_> = host.secrets.borrow().iter().map(|(_, k, _)| k.clone()).collect();
        assert!(vaulted.contains(&"inbound_secret".to_string()));
    }

    #[test]
    fn submit_resumes_and_completes() {
        let host = FakeHost::default();
        let guide = vec![{
            let mut s = step("paste", Actor::Admin, Action::Paste {
                label: "App password".into(),
                output: Some("nc_app_pass".into()),
                secret: true,
            });
            s.produces = vec!["nc_app_pass".into()];
            s
        }];
        let mut session = begin("com.x.talk", "a", &[], &guide, &BTreeMap::new(), "", &host).unwrap();
        assert_eq!(session.status, SessionStatus::AwaitingInput);

        let mut out = BTreeMap::new();
        out.insert("nc_app_pass".to_string(), "hunter2".to_string());
        submit_step(&mut session, "paste", out, &[], &guide, &host).unwrap();
        assert_eq!(session.status, SessionStatus::Complete);
        // Pasted secret went to the vault.
        assert!(host
            .secrets
            .borrow()
            .iter()
            .any(|(_, k, v)| k == "nc_app_pass" && v == "hunter2"));
    }

    #[test]
    fn generate_field_is_minted_and_vaulted() {
        let host = FakeHost::default();
        let mut f = field("TALK_BOT_SECRET", FieldSource::Generate);
        f.secret = true;
        f.generate = Some(GenerateSpec { bytes: 4, encoding: Encoding::Hex });
        let session = begin("com.x.talk", "a", &[f], &[], &BTreeMap::new(), "", &host).unwrap();
        assert_eq!(session.status, SessionStatus::Complete);
        assert_eq!(
            session.config.get("TALK_BOT_SECRET").map(|v| v.as_str().unwrap().to_string()),
            Some("abababab".to_string())
        );
        assert!(host.secrets.borrow().iter().any(|(_, k, _)| k == "TALK_BOT_SECRET"));
    }

    #[test]
    fn when_false_skips_step() {
        let host = FakeHost::default();
        let schema = vec![{
            let mut f = field("SEND_VOICE", FieldSource::Input);
            f.field_type = FieldType::Bool;
            f.default = Some(serde_json::Value::Bool(false));
            f
        }];
        let mut s = step("voice", Actor::Admin, Action::Paste {
            label: "x".into(),
            output: None,
            secret: false,
        });
        s.when = Some("config.SEND_VOICE == true".into());
        let session = begin("p", "a", &schema, &std::slice::from_ref(&s), &BTreeMap::new(), "", &host).unwrap();
        // SEND_VOICE defaulted false → step skipped → whole guide complete.
        assert_eq!(session.status, SessionStatus::Complete);
        assert_eq!(session.steps[0].status, StepStatus::Skipped);
    }

    #[test]
    fn write_service_runs_with_templated_command_and_ledgers_the_unit() {
        let host = FakeHost::default();
        let mut acct_out = BTreeMap::new();
        acct_out.insert("account_id".to_string(), "acct-1".to_string());
        let mut env = BTreeMap::new();
        env.insert("SEND_URL".to_string(), "${out.account.account_id}".to_string());
        let guide = vec![
            step(
                "account",
                Actor::Mira,
                Action::CreateChannelAccount {
                    provider_kind: "nctalk".into(),
                    mention_only: false,
                    supports_voice: false,
                },
            ),
            step(
                "run",
                Actor::Mira,
                Action::WriteService {
                    runtime: crate::packages::manifest::Runtime::Native,
                    command: Some("provider".into()),
                    args: vec!["--account".into(), "${out.account.account_id}".into()],
                    env,
                },
            ),
        ];
        let session = begin("p", "a", &[], &guide, &BTreeMap::new(), "/pkgs/p", &host).unwrap();
        assert_eq!(session.status, SessionStatus::Complete);
        // The service was installed with a resolved command + templated args.
        let svcs = host.services.borrow();
        assert_eq!(svcs.len(), 1);
        assert_eq!(svcs[0].0, "/pkgs/p/provider"); // relative cmd → under install_dir
        assert_eq!(svcs[0].1, vec!["--account".to_string(), "acct-1".to_string()]);
        assert_eq!(svcs[0].2.get("SEND_URL").map(String::as_str), Some("acct-1"));
        // The unit is ledgered for teardown.
        assert!(session
            .ledger
            .iter()
            .any(|e| matches!(e, LedgerEntry::Service { .. })));
        let _ = acct_out;
    }

    #[test]
    fn command_run_by_mira_runs_in_install_dir_and_captures_stdout() {
        let host = FakeHost::default();
        let mut s = step(
            "gen_field",
            Actor::Mira,
            Action::Command { run_by: Actor::Mira, render: "echo hi".into() },
        );
        s.produces = vec!["token".into()];
        let session =
            begin("p", "a", &[], &std::slice::from_ref(&s), &BTreeMap::new(), "/wd", &host).unwrap();
        assert_eq!(session.status, SessionStatus::Complete);
        // Ran the resolved command in the payload dir.
        assert_eq!(host.commands.borrow()[0], ("echo hi".to_string(), "/wd".to_string()));
        // Trimmed stdout captured into the single produced key.
        assert_eq!(
            session.outputs.get("gen_field").and_then(|m| m.get("token")).map(String::as_str),
            Some("fake-out")
        );
    }

    #[test]
    fn begin_update_preserves_secret_seeds_prior_and_runs_only_update_steps() {
        use crate::packages::wizard::RunOn;
        let host = FakeHost::default();
        // A prior install vaulted this secret.
        host.store_secret("p", "TALK_BOT_SECRET", "old-secret").unwrap();

        let mut gen_field = field("TALK_BOT_SECRET", FieldSource::Generate);
        gen_field.secret = true;
        gen_field.generate = Some(GenerateSpec { bytes: 4, encoding: Encoding::Hex });
        let schema = vec![gen_field, field("SEND_URL", FieldSource::Input)];

        let mut install_step = step("install_only", Actor::Mira, Action::Note);
        install_step.run_on = RunOn::Install;
        let mut update_step = step(
            "on_upd",
            Actor::Mira,
            Action::SetSetting { key: "x".into(), value: serde_json::json!(true) },
        );
        update_step.run_on = RunOn::Update;
        // The update step legitimately depends on an install-only step.
        update_step.after = vec!["install_only".into()];
        let guide = vec![install_step, update_step];

        let mut prior = BTreeMap::new();
        prior.insert("SEND_URL".to_string(), serde_json::json!("https://nc"));

        let session =
            begin_update("p", "a", &schema, &guide, &prior, &BTreeMap::new(), "", &host).unwrap();
        assert_eq!(session.status, SessionStatus::Complete);
        // Only the update step is in this session.
        assert_eq!(session.steps.len(), 1);
        assert_eq!(session.steps[0].id, "on_upd");
        // The generated secret was preserved (not re-minted).
        assert_eq!(
            session.config.get("TALK_BOT_SECRET").and_then(|v| v.as_str()),
            Some("old-secret")
        );
        // The prior input value seeded the update.
        assert_eq!(session.config.get("SEND_URL").and_then(|v| v.as_str()), Some("https://nc"));
        // The setting toggle ran.
        assert!(host.settings.borrow().iter().any(|(k, _)| k == "x"));
    }

    #[test]
    fn secret_input_is_vaulted_on_install_and_preserved_on_update() {
        let host = FakeHost::default();
        let mut f = field("API_TOKEN", FieldSource::Input);
        f.secret = true;
        f.required = true;
        let mut input = BTreeMap::new();
        input.insert("API_TOKEN".to_string(), serde_json::json!("tok-1"));

        let s = begin("p", "a", &[f.clone()], &[], &input, "", &host).unwrap();
        assert_eq!(s.status, SessionStatus::Complete);
        // The typed secret was vaulted, not just left in config.
        assert!(host.secrets.borrow().iter().any(|(p, k, v)| p == "p" && k == "API_TOKEN" && v == "tok-1"));

        // Updating without re-entering it preserves the value from the vault
        // (a required secret input doesn't force a re-prompt on update).
        let s2 = begin_update("p", "a", &[f], &[], &BTreeMap::new(), &BTreeMap::new(), "", &host).unwrap();
        assert_eq!(s2.status, SessionStatus::Complete);
        assert_eq!(s2.config.get("API_TOKEN").and_then(|v| v.as_str()), Some("tok-1"));
    }

    #[test]
    fn begin_update_rotates_secret_when_flagged() {
        use crate::packages::wizard::RunOn;
        let host = FakeHost::default();
        host.store_secret("p", "ROTATING", "old").unwrap();
        let mut gen_field = field("ROTATING", FieldSource::Generate);
        gen_field.secret = true;
        gen_field.rotate_on_update = true;
        gen_field.generate = Some(GenerateSpec { bytes: 4, encoding: Encoding::Hex });
        let _ = RunOn::Both;
        let session =
            begin_update("p", "a", &[gen_field], &[], &BTreeMap::new(), &BTreeMap::new(), "", &host).unwrap();
        // Re-minted (the fake mints 0xAB repeated → "abababab"), not the old value.
        assert_eq!(session.config.get("ROTATING").and_then(|v| v.as_str()), Some("abababab"));
    }

    #[test]
    fn command_run_by_mira_nonzero_exit_fails_install() {
        let host = FakeHost::default();
        let s = step(
            "bad",
            Actor::Mira,
            Action::Command { run_by: Actor::Mira, render: "do FAIL".into() },
        );
        let session =
            begin("p", "a", &[], &std::slice::from_ref(&s), &BTreeMap::new(), "", &host).unwrap();
        assert_eq!(session.status, SessionStatus::Failed);
        assert!(session.steps[0].message.as_deref().unwrap().contains("exited"));
    }

    #[test]
    fn write_service_without_command_fails() {
        let host = FakeHost::default();
        let guide = vec![step(
            "run",
            Actor::Mira,
            Action::WriteService {
                runtime: crate::packages::manifest::Runtime::Native,
                command: None,
                args: vec![],
                env: BTreeMap::new(),
            },
        )];
        let session = begin("p", "a", &[], &guide, &BTreeMap::new(), "", &host).unwrap();
        assert_eq!(session.status, SessionStatus::Failed);
        assert!(session.steps[0].message.as_deref().unwrap().contains("command"));
    }

    #[test]
    fn blocking_verify_failure_fails_install() {
        let mut host = FakeHost::default();
        host.probe_pass = false;
        let mut s = step("check", Actor::Mira, Action::Verify {
            verify: VerifyProbe::Tcp {
                host: "127.0.0.1".into(),
                port: "8099".into(),
                on_fail: Some(crate::packages::wizard::OnFail {
                    message: "provider not up".into(),
                    blocking: true,
                }),
            },
        });
        s.title = "check".into();
        let session = begin("p", "a", &[], &std::slice::from_ref(&s), &BTreeMap::new(), "", &host).unwrap();
        assert_eq!(session.status, SessionStatus::Failed);
    }

    #[test]
    fn resolve_handles_base_url_and_bare_output() {
        let host = FakeHost::default();
        let mut session = ProvisionSession {
            package_id: "p".into(),
            admin_id: "a".into(),
            config: BTreeMap::new(),
            outputs: BTreeMap::new(),
            steps: vec![],
            ledger: vec![],
            status: SessionStatus::InProgress,
            manifest: serde_json::Value::Null,
            trust: String::new(),
            version: String::new(),
            name: String::new(),
            install_dir: String::new(),
        };
        let mut acct = BTreeMap::new();
        acct.insert("account_id".to_string(), "acct-1".to_string());
        session.outputs.insert("account".to_string(), acct);
        let got = resolve(
            "${mira.base_url}/webhook/external/${account_id}",
            &session,
            &host,
        )
        .unwrap();
        assert_eq!(got, "https://mira.example.com/webhook/external/acct-1");
    }

    #[test]
    fn roundtrip_probe_resolves_account_and_secret_from_outputs() {
        let host = FakeHost::default();
        let mut session = ProvisionSession {
            package_id: "p".into(),
            admin_id: "a".into(),
            config: BTreeMap::new(),
            outputs: BTreeMap::new(),
            steps: vec![],
            ledger: vec![],
            status: SessionStatus::InProgress,
            manifest: serde_json::Value::Null,
            trust: String::new(),
            version: String::new(),
            name: String::new(),
            install_dir: String::new(),
        };
        let mut acct = BTreeMap::new();
        acct.insert("account_id".to_string(), "acct-1".to_string());
        acct.insert("inbound_secret".to_string(), "in-secret".to_string());
        session.outputs.insert("account".to_string(), acct);

        let probe = VerifyProbe::Roundtrip { kind: "cpp".into(), url: None, on_fail: None };
        match resolve_probe(&probe, &session, &host).unwrap() {
            ResolvedProbe::Roundtrip { kind, url, account_id, inbound_secret } => {
                assert_eq!(kind, "cpp");
                assert_eq!(account_id, "acct-1");
                assert_eq!(inbound_secret, "in-secret");
                assert_eq!(url, "https://mira.example.com/webhook/external/acct-1");
            }
            _ => panic!("expected a resolved roundtrip probe"),
        }
    }
}
