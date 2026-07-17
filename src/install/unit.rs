// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pure rendering of the systemd user unit file.
//!
//! Kept in its own module so the template is unit-testable without touching
//! the filesystem or shelling out to `systemctl`.

use std::path::Path;

/// Which MIRA process a unit supervises. The main server and the out-of-process
/// Guardian liveness sentinel are separate, independently-supervised units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    /// The main MIRA server (`mira --server …`).
    Server,
    /// The Guardian liveness sentinel (`mira … guardian-watch`). Deliberately a
    /// SEPARATE unit — not bound to the server's, so it outlives a server crash.
    GuardianWatch,
}

impl ServiceKind {
    /// Human-readable description (systemd `Description`, SCM description).
    pub fn description(self) -> &'static str {
        match self {
            ServiceKind::Server        => "MIRA — Multi-tasking Intelligent Responsive Assistant",
            ServiceKind::GuardianWatch => "MIRA-Guardian liveness sentinel (watches that MIRA is alive)",
        }
    }

    /// The full launch argv `[bin, ...mode args...]` for this service. The
    /// server takes `--server` as a top-level flag; the sentinel is a subcommand,
    /// so `--config`/`--data-dir` (top-level flags) come BEFORE `guardian-watch`.
    /// Shared by systemd (joined into ExecStart), launchd (ProgramArguments), and
    /// Windows SCM (ImagePath launch_arguments = argv without the bin).
    pub fn argv(self, bin: &Path, cfg: &Path, data: &Path) -> Vec<String> {
        let (bin, cfg, data) =
            (bin.display().to_string(), cfg.display().to_string(), data.display().to_string());
        match self {
            ServiceKind::Server => vec![
                bin, "--server".into(), "--config".into(), cfg, "--data-dir".into(), data,
            ],
            ServiceKind::GuardianWatch => vec![
                bin, "--config".into(), cfg, "--data-dir".into(), data, "guardian-watch".into(),
            ],
        }
    }

    /// The systemd `ExecStart` line — the argv, space-joined.
    fn exec_start(self, bin: &Path, cfg: &Path, data: &Path) -> String {
        self.argv(bin, cfg, data).join(" ")
    }

    /// launchd `Label` (also the plist basename + `launchctl` target).
    pub fn launchd_label(self) -> &'static str {
        match self {
            ServiceKind::Server        => "com.mira",
            ServiceKind::GuardianWatch => "com.mira.guardian-watch",
        }
    }

    /// Basename for per-service log files (launchd stdout/err, unit naming).
    pub fn log_basename(self) -> &'static str {
        match self {
            ServiceKind::Server        => "mira",
            ServiceKind::GuardianWatch => "mira-guardian-watch",
        }
    }

    /// Windows SCM service name + display name.
    pub fn scm_name(self) -> &'static str {
        match self {
            ServiceKind::Server        => "mira",
            ServiceKind::GuardianWatch => "mira-guardian-watch",
        }
    }
    pub fn scm_display(self) -> &'static str {
        match self {
            ServiceKind::Server        => "MIRA — Multi-tasking Intelligent Responsive Assistant",
            ServiceKind::GuardianWatch => "MIRA-Guardian liveness sentinel",
        }
    }
}

pub struct UnitInputs<'a> {
    /// Which process this unit supervises (server vs guardian sentinel).
    pub service:     ServiceKind,
    pub mira_bin:    &'a Path,
    pub config_path: &'a Path,
    pub working_dir: &'a Path,
    // Absolute data dir, baked into ExecStart as `--data-dir` so the service
    // reads the operator-chosen location even when it runs as a different user
    // (e.g. the `mira` system user under `--system`).
    pub data_dir:    &'a Path,
    // Absolute path to the built React bundle (`web/dist/`). When None,
    // no `MIRA_WEB_DIR` env line is written and the server falls back to
    // the placeholder page on non-API routes.
    pub web_dir:     Option<&'a Path>,
    // when Some, render a system-scoped unit: User= /
    // Group= lines set, WantedBy=multi-user.target so it activates on
    // boot, security-hardening defaults (NoNewPrivileges, ProtectSystem)
    // applied. None (default) renders the existing user-scope unit.
    pub system_user: Option<&'a str>,
}

// Render the [Unit]/[Service]/[Install] sections for a `mira` user
// service. `Restart=always` covers both API-triggered `exit(0)` and
// unexpected crashes; `StartLimit*` prevents a config error from looping
// forever.
pub fn render(inputs: &UnitInputs<'_>) -> String {
    // The web bundle env only applies to the server; the sentinel serves nothing.
    let env_line = match inputs.service {
        ServiceKind::Server => inputs.web_dir
            .map(|p| format!("Environment=\"MIRA_WEB_DIR={}\"\n", p.display()))
            .unwrap_or_default(),
        ServiceKind::GuardianWatch => String::new(),
    };
    // Include $HOME/.local/bin and $HOME/bin in PATH so subprocess adapters
    // (`claude`, `opencode`, `hermes`, etc.) installed for the user are
    // discoverable. systemd user services don't inherit shell PATH; without
    // this line, `which claude` returns None at startup and the coding
    // skill never registers.
    let path_line = format!(
        "Environment=\"PATH={}:{}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\"\n",
        inputs.working_dir.join(".local/bin").display(),
        inputs.working_dir.join("bin").display(),
    );

    // system-scope add-ons: User/Group, hardening directives,
    // and a multi-user.target wanted-by so the service activates on
    // boot. Empty string for user-scope so the existing layout is
    // unchanged.
    let (user_lines, wanted_by, hardening) = match inputs.system_user {
        Some(u) => (
            format!("User={u}\nGroup={u}\n"),
            "multi-user.target",
            // Reasonable defaults — let the service read/write its data
            // dir + config dir, but reject mounts / new-privs / etc.
            // `ProtectSystem=full` makes /usr, /boot, /etc read-only at
            // the syscall layer; `ProtectHome=read-only` keeps it out of
            // *other* users' homes while still letting it read `${HOME}`
            // (the working dir) for its data. Not bulletproof, but
            // catches a class of compromise scenarios.
            "NoNewPrivileges=true\n\
             ProtectSystem=full\n\
             ProtectHome=read-only\n\
             PrivateTmp=true\n",
        ),
        None => (
            String::new(),
            "default.target",
            "",
        ),
    };

    format!(
"[Unit]
Description={description}
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
{user_lines}\
{env_line}\
{path_line}\
ExecStart={exec_start}
WorkingDirectory={dir}
Restart=always
RestartSec=2s
SuccessExitStatus=0
TimeoutStopSec=5
{hardening}\
StandardOutput=journal
StandardError=journal

[Install]
WantedBy={wanted_by}
",
        description = inputs.service.description(),
        exec_start  = inputs.service.exec_start(inputs.mira_bin, inputs.config_path, inputs.data_dir),
        dir = inputs.working_dir.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn renders_required_sections_and_paths() {
        let out = render(&UnitInputs {
            service:     ServiceKind::Server,
            mira_bin:    &PathBuf::from("/home/user/bin/mira"),
            config_path: &PathBuf::from("/home/user/.mira/config/mira_config.json"),
            working_dir: &PathBuf::from("/home/user"),
            data_dir:    &PathBuf::from("/home/user/.mira/data"),
            web_dir:     None,
            system_user: None,
        });

        assert!(out.contains("[Unit]"));
        assert!(out.contains("[Service]"));
        assert!(out.contains("[Install]"));
        assert!(out.contains("Restart=always"));
        assert!(out.contains("ExecStart=/home/user/bin/mira --server --config /home/user/.mira/config/mira_config.json --data-dir /home/user/.mira/data"));
        assert!(out.contains("WorkingDirectory=/home/user"));
        assert!(out.contains("WantedBy=default.target"));
        assert!(!out.contains("MIRA_WEB_DIR"));
    }

    #[test]
    fn start_limits_are_in_unit_section() {
        // StartLimit* moved from [Service] to [Unit] in systemd 230 (2016).
        let out = render(&UnitInputs {
            service:     ServiceKind::Server,
            mira_bin:    &PathBuf::from("/x"),
            config_path: &PathBuf::from("/y"),
            working_dir: &PathBuf::from("/z"),
            data_dir:    &PathBuf::from("/d"),
            web_dir:     None,
            system_user: None,
        });
        let unit_start    = out.find("[Unit]").unwrap();
        let service_start = out.find("[Service]").unwrap();
        let limit_pos     = out.find("StartLimitIntervalSec").unwrap();
        assert!(limit_pos > unit_start && limit_pos < service_start);
    }

    #[test]
    fn system_scope_adds_user_group_and_hardening() {
        let out = render(&UnitInputs {
            service:     ServiceKind::Server,
            mira_bin:    &PathBuf::from("/usr/local/bin/mira"),
            config_path: &PathBuf::from("/etc/mira/mira_config.json"),
            working_dir: &PathBuf::from("/var/lib/mira"),
            data_dir:    &PathBuf::from("/var/lib/mira/.mira/data"),
            web_dir:     None,
            system_user: Some("mira"),
        });
        assert!(out.contains("User=mira"));
        assert!(out.contains("Group=mira"));
        assert!(out.contains("NoNewPrivileges=true"));
        assert!(out.contains("ProtectSystem=full"));
        assert!(out.contains("WantedBy=multi-user.target"));
        assert!(!out.contains("WantedBy=default.target"));
    }

    #[test]
    fn user_scope_omits_hardening_and_user_lines() {
        let out = render(&UnitInputs {
            service:     ServiceKind::Server,
            mira_bin:    &PathBuf::from("/x"),
            config_path: &PathBuf::from("/y"),
            working_dir: &PathBuf::from("/z"),
            data_dir:    &PathBuf::from("/d"),
            web_dir:     None,
            system_user: None,
        });
        assert!(!out.contains("User="));
        assert!(!out.contains("NoNewPrivileges"));
        assert!(out.contains("WantedBy=default.target"));
    }

    #[test]
    fn guardian_variant_renders_subcommand_exec_and_no_web_env() {
        let out = render(&UnitInputs {
            service:     ServiceKind::GuardianWatch,
            mira_bin:    &PathBuf::from("/home/user/bin/mira"),
            config_path: &PathBuf::from("/home/user/.mira/config/mira_config.json"),
            working_dir: &PathBuf::from("/home/user"),
            data_dir:    &PathBuf::from("/home/user/.mira/data"),
            // Even if a web_dir is passed, the sentinel unit must not carry it.
            web_dir:     Some(&PathBuf::from("/home/user/MIRA/web/dist")),
            system_user: None,
        });
        // Top-level flags come BEFORE the `guardian-watch` subcommand word.
        assert!(out.contains(
            "ExecStart=/home/user/bin/mira --config /home/user/.mira/config/mira_config.json \
             --data-dir /home/user/.mira/data guardian-watch"));
        assert!(out.contains("Description=MIRA-Guardian liveness sentinel"));
        assert!(!out.contains("--server"), "sentinel must not launch the server");
        assert!(!out.contains("MIRA_WEB_DIR"), "sentinel serves no web bundle");
        assert!(out.contains("Restart=always"));
    }

    #[test]
    fn web_dir_emits_environment_line_inside_service() {
        let out = render(&UnitInputs {
            service:     ServiceKind::Server,
            mira_bin:    &PathBuf::from("/x"),
            config_path: &PathBuf::from("/y"),
            working_dir: &PathBuf::from("/z"),
            data_dir:    &PathBuf::from("/d"),
            web_dir:     Some(&PathBuf::from("/home/user/MIRA/web/dist")),
            system_user: None,
        });
        assert!(out.contains("Environment=\"MIRA_WEB_DIR=/home/user/MIRA/web/dist\""));
        // Must appear inside [Service], between the section header and ExecStart.
        let svc = out.find("[Service]").unwrap();
        let env = out.find("MIRA_WEB_DIR").unwrap();
        let exec = out.find("ExecStart=").unwrap();
        assert!(svc < env && env < exec);
    }
}
