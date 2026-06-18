// SPDX-License-Identifier: AGPL-3.0-or-later

//! `mira setup` — the first-run guided setup wizard (the "essentials" tier).
//!
//! Gets a **secure, working** MIRA instance up in ~2 minutes: an admin account,
//! one LLM provider (validated with a live API call), and the network/security
//! posture — then writes a validated `mira_config.json`, creates the admin in the
//! auth DB, and prints the URL + login. Voice, channels, and companion are left
//! to the polished web UI (this wizard hands off to it).
//!
//! Two paths share the same apply/validate logic:
//! - **interactive** (default) — `dialoguer` prompts (select menus, masked input,
//!   confirmations) with live provider testing.
//! - **unattended** (`--unattended` + flags / env) — for Docker, CI, and scripted
//!   installs; no TTY required.
//!
//! Network default is **localhost-only**; exposing on the LAN is an explicit
//! opt-in with an HTTPS/reverse-proxy note.

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use colored::Colorize;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};

use crate::auth::models::{NewUser, Role};
use crate::auth::LocalAuthService;
use crate::config::MiraConfig;

/// CLI inputs (interactive when `unattended` is false; otherwise read from these).
#[derive(Default)]
pub struct SetupOptions {
    pub config_path: Option<PathBuf>,
    /// Where MIRA stores its data (databases, memory, auth). When None, the
    /// wizard prompts (interactive) or falls back to MIRA_SETUP_DATA_DIR /
    /// the `~/.mira/data` default (unattended). Persisted into the config.
    pub data_dir: Option<PathBuf>,
    pub unattended: bool,
    pub force: bool,
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub admin_user: Option<String>,
    pub admin_pass: Option<String>,
    pub bind: Option<String>, // "localhost" | "lan" | an explicit host
    pub port: Option<u16>,
    pub skip_provider_test: bool,
}

/// A provider the wizard can configure.
struct ProviderSpec {
    id: &'static str,
    label: &'static str,
    local: bool, // local = URL-based, no API key
}

const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec { id: "ollama",     label: "Ollama (local)",            local: true },
    ProviderSpec { id: "lmstudio",   label: "LM Studio (local)",         local: true },
    ProviderSpec { id: "anthropic",  label: "Anthropic (Claude)",        local: false },
    ProviderSpec { id: "openai",     label: "OpenAI",                    local: false },
    ProviderSpec { id: "openrouter", label: "OpenRouter",                local: false },
    ProviderSpec { id: "gemini",     label: "Google Gemini",             local: false },
    ProviderSpec { id: "deepseek",   label: "DeepSeek",                  local: false },
    ProviderSpec { id: "groq",       label: "Groq",                      local: false },
    ProviderSpec { id: "xai",        label: "xAI (Grok)",                local: false },
];

/// The collected answers, ready to apply.
struct Answers {
    provider_id: String,
    api_key: Option<String>,
    base_url: String,
    model: String,
    admin_user: String,
    admin_pass: String,
    host: String,
    port: u16,
    data_dir: String,
}

pub fn run(opts: SetupOptions) -> Result<(), Box<dyn Error>> {
    let config_path = opts
        .config_path
        .clone()
        .unwrap_or_else(default_config_path);

    // Preflight: an existing config is a re-configure, not a fresh install.
    if config_path.exists() && !opts.force {
        if opts.unattended {
            return Err(format!(
                "config already exists at {} — pass --force to overwrite",
                config_path.display()
            )
            .into());
        }
        eprintln!(
            "{} a config already exists at {}",
            "!".yellow().bold(),
            config_path.display().to_string().cyan()
        );
        if !Confirm::with_theme(&theme())
            .with_prompt("Reconfigure it? (your data is kept; provider/admin/security are rewritten)")
            .default(false)
            .interact()?
        {
            eprintln!("Aborted — nothing changed.");
            return Ok(());
        }
    }

    if !opts.unattended {
        banner();
    }

    // On a reconfigure, surface the existing admin so we default to it (no
    // accidental second admin) and the unattended path can reuse it.
    let existing_admin = existing_admin_username(&config_path);
    let answers = if opts.unattended {
        from_opts(&opts, existing_admin)?
    } else {
        match interactive(&opts, existing_admin)? {
            Some(a) => a,
            None => {
                eprintln!("Aborted — nothing changed.");
                return Ok(());
            }
        }
    };

    apply(&answers, &config_path)?;
    summary(&answers);
    Ok(())
}

// ── interactive flow ───────────────────────────────────────────────────────

fn interactive(opts: &SetupOptions, existing_admin: Option<String>) -> Result<Option<Answers>, Box<dyn Error>> {
    let def = MiraConfig::default_with_path();

    // 1) Admin account. On a reconfigure, default to the existing admin so the
    //    user doesn't accidentally type a new name and create a second admin.
    section("1/3", "Admin account");
    if let Some(u) = &existing_admin {
        eprintln!("  {}", format!("reconfiguring — existing admin is '{u}'").dimmed());
    }
    let admin_user: String = Input::with_theme(&theme())
        .with_prompt("Admin username")
        .default(
            existing_admin
                .clone()
                .or_else(|| opts.admin_user.clone())
                .unwrap_or_else(|| "admin".into()),
        )
        .interact_text()?;
    let admin_pass = Password::with_theme(&theme())
        .with_prompt("Admin password (min 8 chars)")
        .with_confirmation("Confirm password", "Passwords don't match — try again")
        .validate_with(|p: &String| -> Result<(), &str> {
            if p.len() >= 8 { Ok(()) } else { Err("at least 8 characters") }
        })
        .interact()?;

    // 2) LLM provider — auto-detect locals, then a menu that ALWAYS lists every
    //    provider (so a non-standard Ollama/LM Studio port is never a dead end).
    section("2/3", "AI provider");
    eprintln!("  {}", "Looking for a local model server…".dimmed());
    let ollama_url = def.providers.ollama.url.clone();
    let lmstudio_url = def.providers.lmstudio.url.clone();
    let ollama_found = probe_local("ollama", &ollama_url).is_ok();
    let lmstudio_found = probe_local("lmstudio", &lmstudio_url).is_ok();

    let items: Vec<String> = PROVIDERS
        .iter()
        .map(|p| match p.id {
            "ollama" if ollama_found => format!("{}  {}", p.label, format!("✓ detected at {ollama_url}").green()),
            "ollama" => format!("{}  {}", p.label, "(auto-detect didn't find one — you'll enter the URL)".dimmed()),
            "lmstudio" if lmstudio_found => format!("{}  {}", p.label, format!("✓ detected at {lmstudio_url}").green()),
            "lmstudio" => format!("{}  {}", p.label, "(auto-detect didn't find one — you'll enter the URL)".dimmed()),
            _ => p.label.to_string(),
        })
        .collect();
    // Default selection: a detected local if present, else Anthropic.
    let default_idx = if ollama_found {
        0
    } else if lmstudio_found {
        1
    } else {
        2
    };
    let idx = Select::with_theme(&theme())
        .with_prompt("Which provider should MIRA use?")
        .items(&items)
        .default(default_idx)
        .interact()?;
    let spec = &PROVIDERS[idx];

    // Provider details + a live test loop.
    let mut base_url = default_base(&def, spec.id);
    let mut api_key: Option<String> = None;
    let model: String;
    loop {
        if spec.local {
            base_url = Input::with_theme(&theme())
                .with_prompt(format!("{} URL", spec.label))
                .default(base_url.clone())
                .interact_text()?;
        } else {
            let key = Password::with_theme(&theme())
                .with_prompt(format!("{} API key", spec.label))
                .interact()?;
            api_key = Some(key);
        }

        if opts.skip_provider_test {
            model = ask_model(opts, &[])?;
            break;
        }

        eprint!("  {} ", "Testing the connection…".dimmed());
        match test_provider(spec.id, &base_url, api_key.as_deref()) {
            Ok(models) => {
                eprintln!("{}", "✓ connected".green().bold());
                model = ask_model(opts, &models)?;
                break;
            }
            Err(e) => {
                eprintln!("{}", format!("✗ {e}").red().bold());
                let retry = Select::with_theme(&theme())
                    .with_prompt("What now?")
                    .items(&["Re-enter the details and retry", "Use this provider anyway (skip the test)", "Pick a different provider"])
                    .default(0)
                    .interact()?;
                match retry {
                    0 => continue,
                    1 => {
                        model = ask_model(opts, &[])?;
                        break;
                    }
                    _ => return interactive(opts, existing_admin.clone()), // restart provider selection
                }
            }
        }
    }

    // 3) Network / security.
    section("3/3", "Network & security");
    eprintln!(
        "  {}",
        "By default MIRA listens on localhost only — reachable from this machine \n  (or via an SSH tunnel). You can expose it on your network instead.".dimmed()
    );
    let expose = Confirm::with_theme(&theme())
        .with_prompt("Expose MIRA on your local network (0.0.0.0)?")
        .default(false)
        .interact()?;
    if expose {
        eprintln!(
            "  {}",
            "→ Exposing on the LAN. Put it behind HTTPS (a reverse proxy like nginx/Caddy) \n    before exposing to the internet. A strong admin password + auto JWT secret are set.".yellow()
        );
    }
    let host = if expose { "0.0.0.0".to_string() } else { "127.0.0.1".to_string() };
    let port: u16 = Input::with_theme(&theme())
        .with_prompt("Port")
        .default(opts.port.unwrap_or(8080))
        .interact_text()?;
    if !port_available(&host, port) {
        eprintln!(
            "  {}",
            format!("note: {host}:{port} looks busy — change it later in the config if MIRA won't start").yellow()
        );
    }

    // Storage location. Defaulted to ~/.mira/data — most users just press Enter.
    // Surfaced so it can live on a backed-up volume / external disk instead.
    eprintln!();
    eprintln!(
        "  {}",
        "Where MIRA keeps its data (databases, memory, auth). Press Enter for the \n  default, or point it at a backed-up volume / external disk.".dimmed()
    );
    let data_dir: String = Input::with_theme(&theme())
        .with_prompt("Data directory")
        .default(
            opts.data_dir
                .clone()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.mira/data".to_string()),
        )
        .interact_text()?;

    // Review + confirm before writing anything.
    let net = if host == "0.0.0.0" { format!("0.0.0.0:{port} (LAN)") } else { format!("localhost:{port}") };
    eprintln!();
    eprintln!("  {}", "Review".bold());
    eprintln!("    {}  {}", "Admin:   ".dimmed(), admin_user.bold());
    eprintln!("    {}  {} — {}", "Provider:".dimmed(), spec.label, model.bold());
    eprintln!("    {}  {}", "Network: ".dimmed(), net);
    eprintln!("    {}  {}", "Data dir:".dimmed(), data_dir);
    eprintln!();
    if !Confirm::with_theme(&theme())
        .with_prompt("Apply these settings?")
        .default(true)
        .interact()?
    {
        return Ok(None);
    }

    Ok(Some(Answers { provider_id: spec.id.to_string(), api_key, base_url, model, admin_user, admin_pass, host, port, data_dir }))
}

fn ask_model(opts: &SetupOptions, models: &[String]) -> Result<String, Box<dyn Error>> {
    if let Some(m) = &opts.model {
        return Ok(m.clone());
    }
    if models.is_empty() {
        let m: String = Input::with_theme(&theme())
            .with_prompt("Default model id")
            .interact_text()?;
        return Ok(m);
    }
    let idx = Select::with_theme(&theme())
        .with_prompt("Default model")
        .items(models)
        .default(0)
        .interact()?;
    Ok(models[idx].clone())
}

// ── unattended flow ─────────────────────────────────────────────────────────

fn from_opts(opts: &SetupOptions, existing_admin: Option<String>) -> Result<Answers, Box<dyn Error>> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let provider_id = opts
        .provider
        .clone()
        .or_else(|| env("MIRA_SETUP_PROVIDER"))
        .ok_or("unattended setup needs --provider (or MIRA_SETUP_PROVIDER)")?;
    let spec = PROVIDERS
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| format!("unknown provider '{provider_id}'"))?;

    let def = MiraConfig::default_with_path();
    let base_url = opts
        .base_url
        .clone()
        .or_else(|| env("MIRA_SETUP_BASE_URL"))
        .unwrap_or_else(|| default_base(&def, spec.id));
    let api_key = opts.api_key.clone().or_else(|| env("MIRA_SETUP_API_KEY"));
    if !spec.local && api_key.is_none() {
        return Err(format!("provider '{provider_id}' needs --api-key (or MIRA_SETUP_API_KEY)").into());
    }

    let admin_user = opts
        .admin_user
        .clone()
        .or_else(|| env("MIRA_SETUP_ADMIN_USER"))
        .or(existing_admin)
        .unwrap_or_else(|| "admin".into());
    let admin_pass = opts
        .admin_pass
        .clone()
        .or_else(|| env("MIRA_SETUP_ADMIN_PASS"))
        .ok_or("unattended setup needs --admin-pass (or MIRA_SETUP_ADMIN_PASS)")?;
    if admin_pass.len() < 8 {
        return Err("admin password must be at least 8 characters".into());
    }

    let host = match opts.bind.clone().or_else(|| env("MIRA_SETUP_BIND")).as_deref() {
        Some("lan") | Some("0.0.0.0") => "0.0.0.0".to_string(),
        _ => "127.0.0.1".to_string(),
    };
    let port = opts.port.or_else(|| env("MIRA_SETUP_PORT").and_then(|p| p.parse().ok())).unwrap_or(8080);

    let data_dir = opts
        .data_dir
        .clone()
        .map(|p| p.display().to_string())
        .or_else(|| env("MIRA_SETUP_DATA_DIR"))
        .unwrap_or_else(|| "~/.mira/data".to_string());

    let skip_test = opts.skip_provider_test || env("MIRA_SETUP_SKIP_PROVIDER_TEST").is_some();
    let model = if let Some(m) = opts.model.clone().or_else(|| env("MIRA_SETUP_MODEL")) {
        m
    } else if skip_test {
        def_model(&def, spec.id)
    } else {
        match test_provider(spec.id, &base_url, api_key.as_deref()) {
            Ok(models) => models.into_iter().next().unwrap_or_else(|| def_model(&def, spec.id)),
            Err(e) => return Err(format!("provider test failed: {e} (pass --model and/or --skip-provider-test)").into()),
        }
    };

    Ok(Answers { provider_id: spec.id.to_string(), api_key, base_url, model, admin_user, admin_pass, host, port, data_dir })
}

// ── apply (shared) ──────────────────────────────────────────────────────────

fn apply(a: &Answers, config_path: &PathBuf) -> Result<(), Box<dyn Error>> {
    let mut cfg = MiraConfig::default_with_path();
    cfg.config_path = config_path.clone();
    cfg.primary_provider = a.provider_id.clone();

    let p = &mut cfg.providers;
    match a.provider_id.as_str() {
        "ollama" => { p.ollama.enabled = true; p.ollama.url = a.base_url.clone(); p.ollama.default_model = a.model.clone(); }
        "lmstudio" => { p.lmstudio.enabled = true; p.lmstudio.url = a.base_url.clone(); p.lmstudio.default_model = a.model.clone(); }
        "anthropic" => { p.anthropic.enabled = true; p.anthropic.api_key = a.api_key.clone(); p.anthropic.base_url = a.base_url.clone(); p.anthropic.default_model = a.model.clone(); }
        "openai" => { p.openai.enabled = true; p.openai.api_key = a.api_key.clone(); p.openai.base_url = a.base_url.clone(); p.openai.default_model = a.model.clone(); }
        "openrouter" => { p.openrouter.enabled = true; p.openrouter.api_key = a.api_key.clone(); p.openrouter.base_url = a.base_url.clone(); p.openrouter.default_model = a.model.clone(); }
        "gemini" => { p.gemini.enabled = true; p.gemini.api_key = a.api_key.clone(); p.gemini.base_url = a.base_url.clone(); p.gemini.default_model = a.model.clone(); }
        "deepseek" => { p.deepseek.enabled = true; p.deepseek.api_key = a.api_key.clone(); p.deepseek.base_url = a.base_url.clone(); p.deepseek.default_model = a.model.clone(); }
        "groq" => { p.groq.enabled = true; p.groq.api_key = a.api_key.clone(); p.groq.base_url = a.base_url.clone(); p.groq.default_model = a.model.clone(); }
        "xai" => { p.xai.enabled = true; p.xai.api_key = a.api_key.clone(); p.xai.base_url = a.base_url.clone(); p.xai.default_model = a.model.clone(); }
        other => return Err(format!("unknown provider '{other}'").into()),
    }

    cfg.server.enabled = true;
    cfg.server.host = a.host.clone();
    cfg.server.port = a.port;

    // Persist the chosen data dir. `mira install` later reads this to bake the
    // resolved absolute path into the service launch (so a supervised service
    // reads the same data regardless of which account runs it).
    if !a.data_dir.trim().is_empty() {
        cfg.data_dir = a.data_dir.trim().to_string();
    }

    // Persist a JWT secret so the admin DB we create below + the server agree.
    if cfg.security.jwt_secret.as_deref().unwrap_or("").is_empty() {
        cfg.security.jwt_secret = Some(random_secret());
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    cfg.save().map_err(|e| format!("writing config: {e}"))?;

    // Create the admin in the auth DB (same jwt_secret + session_days the server uses).
    let data_dir = expand_tilde(&cfg.data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let auth_db = data_dir.join("auth.db");
    let jwt = cfg.security.jwt_secret.clone().unwrap_or_default();
    let auth = LocalAuthService::new(&auth_db, jwt, cfg.security.session_days)
        .map_err(|e| format!("opening auth db: {e}"))?;
    // Fresh install → create the admin. Reconfigure (the user already exists from
    // a previous run) → update its password, so "log in with the password you
    // just set" is always true.
    let existing = auth
        .list_users()
        .unwrap_or_default()
        .into_iter()
        .find(|u| u.username.eq_ignore_ascii_case(&a.admin_user));
    match existing {
        Some(user) => {
            auth.change_password(&user.id, &a.admin_pass)
                .map_err(|e| format!("updating admin '{}' password: {e}", a.admin_user))?;
        }
        None => {
            auth.create_user(NewUser {
                username: a.admin_user.clone(),
                display_name: None,
                email: None,
                password: a.admin_pass.clone(),
                role: Role::Admin,
            })
            .map_err(|e| format!("creating admin '{}': {e}", a.admin_user))?;
        }
    }
    Ok(())
}

// ── provider testing (reqwest blocking) ──────────────────────────────────────

/// Quick reachability probe for a local server (1.5s); returns Ok if up.
fn probe_local(id: &str, url: &str) -> Result<(), String> {
    test_provider(id, url, None).map(|_| ())
}

/// Validate a provider + return its model ids. Errors are short + human.
fn test_provider(id: &str, base_url: &str, api_key: Option<&str>) -> Result<Vec<String>, String> {
    let timeout = if id == "ollama" || id == "lmstudio" {
        Duration::from_millis(1500)
    } else {
        Duration::from_secs(10)
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let base = base_url.trim_end_matches('/');

    let (url, builder): (String, reqwest::blocking::RequestBuilder) = match id {
        "ollama" => (format!("{base}/api/tags"), client.get(format!("{base}/api/tags"))),
        "gemini" => {
            let key = api_key.unwrap_or("");
            (format!("{base}/models"), client.get(format!("{base}/models?key={key}")))
        }
        "anthropic" => (
            format!("{base}/models"),
            client
                .get(format!("{base}/models"))
                .header("x-api-key", api_key.unwrap_or(""))
                .header("anthropic-version", "2023-06-01"),
        ),
        // OpenAI-compatible: openai, openrouter, groq, xai, deepseek, lmstudio
        _ => {
            let mut b = client.get(format!("{base}/models"));
            if let Some(k) = api_key {
                b = b.bearer_auth(k);
            }
            (format!("{base}/models"), b)
        }
    };

    let resp = builder.send().map_err(|e| {
        if e.is_timeout() || e.is_connect() {
            format!("couldn't reach {url}")
        } else {
            e.to_string()
        }
    })?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err("API key rejected".into());
    }
    if !status.is_success() {
        return Err(format!("server returned {status}"));
    }
    let json: serde_json::Value = resp.json().map_err(|e| format!("bad response: {e}"))?;
    Ok(parse_models(id, &json))
}

fn parse_models(id: &str, json: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    match id {
        "ollama" => {
            if let Some(arr) = json.get("models").and_then(|v| v.as_array()) {
                for m in arr {
                    if let Some(n) = m.get("name").and_then(|v| v.as_str()) {
                        out.push(n.to_string());
                    }
                }
            }
        }
        "gemini" => {
            if let Some(arr) = json.get("models").and_then(|v| v.as_array()) {
                for m in arr {
                    if let Some(n) = m.get("name").and_then(|v| v.as_str()) {
                        out.push(n.strip_prefix("models/").unwrap_or(n).to_string());
                    }
                }
            }
        }
        _ => {
            if let Some(arr) = json.get("data").and_then(|v| v.as_array()) {
                for m in arr {
                    if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                        out.push(id.to_string());
                    }
                }
            }
        }
    }
    out.sort();
    out
}

// ── presentation + helpers ───────────────────────────────────────────────────

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

fn banner() {
    eprintln!();
    eprintln!("  {}", "✦ MIRA setup".bold().cyan());
    eprintln!("  {}", "Let's get a secure, working instance running.".dimmed());
    eprintln!();
}

fn section(step: &str, title: &str) {
    eprintln!();
    eprintln!("  {} {}", format!("[{step}]").cyan().bold(), title.bold());
}

fn summary(a: &Answers) {
    let shown_host = if a.host == "0.0.0.0" { "<this-machine-ip>" } else { "localhost" };
    let url = format!("http://{shown_host}:{}/", a.port);
    eprintln!();
    eprintln!("  {}", "✓ MIRA is configured.".green().bold());
    eprintln!();
    eprintln!("    {}  {}", "URL: ".dimmed(), url.cyan().bold());
    eprintln!("    {}  {}", "User:".dimmed(), a.admin_user.bold());
    eprintln!("    {}  {}", "Pass:".dimmed(), "(the one you just set)".dimmed());
    eprintln!();
    eprintln!(
        "  {}",
        "Next: start MIRA (the installer does this for you), open the URL, log in,\n  and finish voice & channels from the web UI.".dimmed()
    );
    eprintln!();
}

fn default_config_path() -> PathBuf {
    expand_tilde("~/.mira/config/mira_config.json")
}

/// Best-effort: the existing active admin's username, read from the config's
/// auth DB. Used to default the username on a reconfigure so we don't create a
/// second admin by accident. Reads the JSON directly to avoid load() side effects.
fn existing_admin_username(config_path: &std::path::Path) -> Option<String> {
    let txt = std::fs::read_to_string(config_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let data_dir = v.get("data_dir").and_then(|x| x.as_str()).unwrap_or("~/.mira/data");
    let sec = v.get("security");
    let jwt = sec.and_then(|s| s.get("jwt_secret")).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let session_days = sec.and_then(|s| s.get("session_days")).and_then(|x| x.as_u64()).unwrap_or(7);
    let auth = LocalAuthService::new(&expand_tilde(data_dir).join("auth.db"), jwt, session_days).ok()?;
    auth.list_users()
        .ok()?
        .into_iter()
        .find(|u| matches!(u.role, Role::Admin) && u.is_active)
        .map(|u| u.username)
}

fn default_base(def: &MiraConfig, id: &str) -> String {
    match id {
        "ollama" => def.providers.ollama.url.clone(),
        "lmstudio" => def.providers.lmstudio.url.clone(),
        "anthropic" => def.providers.anthropic.base_url.clone(),
        "openai" => def.providers.openai.base_url.clone(),
        "openrouter" => def.providers.openrouter.base_url.clone(),
        "gemini" => def.providers.gemini.base_url.clone(),
        "deepseek" => def.providers.deepseek.base_url.clone(),
        "groq" => def.providers.groq.base_url.clone(),
        "xai" => def.providers.xai.base_url.clone(),
        _ => String::new(),
    }
}

fn def_model(def: &MiraConfig, id: &str) -> String {
    match id {
        "ollama" => def.providers.ollama.default_model.clone(),
        "lmstudio" => def.providers.lmstudio.default_model.clone(),
        "anthropic" => def.providers.anthropic.default_model.clone(),
        "openai" => def.providers.openai.default_model.clone(),
        "openrouter" => def.providers.openrouter.default_model.clone(),
        "gemini" => def.providers.gemini.default_model.clone(),
        "deepseek" => def.providers.deepseek.default_model.clone(),
        "groq" => def.providers.groq.default_model.clone(),
        "xai" => def.providers.xai.default_model.clone(),
        _ => String::new(),
    }
}

fn random_secret() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..48).map(|_| format!("{:x}", rng.gen_range(0..16u8))).collect()
}

fn port_available(host: &str, port: u16) -> bool {
    let bind_host = if host == "0.0.0.0" { "0.0.0.0" } else { "127.0.0.1" };
    std::net::TcpListener::bind((bind_host, port)).is_ok()
}

fn expand_tilde(p: &str) -> PathBuf {
    // Delegate to the canonical resolver so `~` expansion matches the rest of
    // MIRA. Crucially it uses `dirs::home_dir()` (which honors %USERPROFILE% on
    // Windows) instead of the Unix-only `$HOME` env var — on Windows `$HOME` is
    // normally unset, which left `~` literal and made `mira setup` write its
    // config/data under a `~` directory relative to the CWD.
    crate::config::expand_path(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_models() {
        let j = serde_json::json!({"data":[{"id":"gpt-4o"},{"id":"gpt-4o-mini"}]});
        assert_eq!(parse_models("openai", &j), vec!["gpt-4o", "gpt-4o-mini"]);
    }

    #[test]
    fn parse_ollama_models() {
        let j = serde_json::json!({"models":[{"name":"llama3.2"},{"name":"qwen2.5"}]});
        assert_eq!(parse_models("ollama", &j), vec!["llama3.2", "qwen2.5"]);
    }

    #[test]
    fn parse_gemini_strips_prefix() {
        let j = serde_json::json!({"models":[{"name":"models/gemini-2.5-flash"}]});
        assert_eq!(parse_models("gemini", &j), vec!["gemini-2.5-flash"]);
    }

    #[test]
    fn secret_is_long_and_hex() {
        let s = random_secret();
        assert_eq!(s.len(), 48);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
