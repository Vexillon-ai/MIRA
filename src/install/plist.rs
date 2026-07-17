// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pure rendering of the macOS LaunchAgent plist.
//!
//! Kept in its own module so the template is unit-testable without touching
//! the filesystem or shelling out to `launchctl`. Mirrors `unit.rs` for the
//! Linux/systemd backend.

use std::path::Path;

/// Reverse-DNS service identifier. Also the plist filename
/// (`{LABEL}.plist`) and the path used by `launchctl` (`gui/$UID/{LABEL}`).
pub const LAUNCHD_LABEL: &str = "com.mira";

pub struct PlistInputs<'a> {
    /// Which process this agent supervises (server vs guardian sentinel).
    pub service:     crate::install::unit::ServiceKind,
    pub mira_bin:    &'a Path,
    pub config_path: &'a Path,
    pub working_dir: &'a Path,
    /// Absolute data dir, baked into ProgramArguments as `--data-dir` so the
    /// agent reads the operator-chosen location regardless of `~` resolution.
    pub data_dir:    &'a Path,
    /// Absolute path to the built React bundle (`web/dist/`). When None,
    /// no `MIRA_WEB_DIR` env var is written and the server falls back to
    /// the placeholder page on non-API routes.
    pub web_dir:     Option<&'a Path>,
    /// Where launchd writes stdout/stderr captures. macOS doesn't have a
    /// journal equivalent, so we write to per-service log files.
    pub log_dir:     &'a Path,
    /// Extra environment variables to inject. Used for `ORT_DYLIB_PATH` so
    /// fastembed's onnxruntime fallback can locate the dylib — macOS's
    /// SIP-restricted launchd dlopen path doesn't search Homebrew prefixes.
    pub extra_env:   &'a [(&'a str, &'a str)],
}

/// Render a `LaunchAgent` plist. `KeepAlive=true` covers both API-triggered
/// `exit(0)` and unexpected crashes — same contract as `Restart=always` on
/// systemd. `RunAtLoad=true` so the service starts the moment the agent is
/// bootstrapped.
pub fn render(inputs: &PlistInputs<'_>) -> String {
    // Build the EnvironmentVariables dict, omitting it entirely if nothing
    // to set (rather than emitting an empty <dict/>, which launchd accepts
    // but is ugly).
    let mut entries = Vec::<(String, String)>::new();
    // Only the server serves the React bundle; the guardian sentinel exposes
    // no HTTP surface, so `MIRA_WEB_DIR` is meaningless (and misleading) there.
    if let (Some(p), crate::install::unit::ServiceKind::Server) = (inputs.web_dir, inputs.service) {
        entries.push(("MIRA_WEB_DIR".to_string(), p.display().to_string()));
    }
    for (k, v) in inputs.extra_env {
        entries.push(((*k).to_string(), (*v).to_string()));
    }
    let env_block = if entries.is_empty() {
        String::new()
    } else {
        let mut s = String::from("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (k, v) in &entries {
            s.push_str(&format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v),
            ));
        }
        s.push_str("    </dict>\n");
        s
    };

    // ProgramArguments from the shared per-service argv (server: `--server …`;
    // guardian: `--config … --data-dir … guardian-watch`).
    let prog_args: String = inputs.service
        .argv(inputs.mira_bin, inputs.config_path, inputs.data_dir)
        .iter()
        .map(|a| format!("        <string>{}</string>\n", xml_escape(a)))
        .collect();

    format!(
"<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{prog_args}\
    </array>
    <key>WorkingDirectory</key>
    <string>{dir}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{logs}/{base}.out.log</string>
    <key>StandardErrorPath</key>
    <string>{logs}/{base}.err.log</string>
{env}\
</dict>
</plist>
",
        label = inputs.service.launchd_label(),
        base  = inputs.service.log_basename(),
        dir   = xml_escape(&inputs.working_dir.display().to_string()),
        logs  = xml_escape(&inputs.log_dir.display().to_string()),
        env   = env_block,
    )
}

/// Escape the five XML/plist metacharacters. Apple's plist parser is strict
/// — an unescaped `&` in a path will refuse to load with a useless error.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&'  => out.push_str("&amp;"),
            '<'  => out.push_str("&lt;"),
            '>'  => out.push_str("&gt;"),
            '"'  => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _    => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn guardian_variant_renders_subcommand_label_and_logs() {
        let out = render(&PlistInputs {
            service:     crate::install::unit::ServiceKind::GuardianWatch,
            mira_bin:    &PathBuf::from("/Users/me/.cargo/bin/mira"),
            config_path: &PathBuf::from("/Users/me/.mira/config/mira_config.json"),
            working_dir: &PathBuf::from("/Users/me"),
            data_dir:    &PathBuf::from("/Users/me/.mira/data"),
            web_dir:     Some(&PathBuf::from("/Users/me/MIRA/web/dist")),
            log_dir:     &PathBuf::from("/Users/me/Library/Logs/mira"),
            extra_env:   &[],
        });
        // Own launchd label + sentinel log basename.
        assert!(out.contains("<string>com.mira.guardian-watch</string>"));
        assert!(out.contains("mira-guardian-watch.out.log"));
        assert!(out.contains("mira-guardian-watch.err.log"));
        // Subcommand argv order: top-level flags BEFORE `guardian-watch`, no `--server`.
        let cfg_at = out.find("<string>--config</string>").unwrap();
        // Match the argv entry specifically, not the `com.mira.guardian-watch` Label.
        let sub_at = out.find("<string>guardian-watch</string>").unwrap();
        assert!(cfg_at < sub_at, "flags must precede the subcommand");
        assert!(!out.contains("<string>--server</string>"));
        // Sentinel serves no web bundle even if a web_dir is passed.
        assert!(!out.contains("MIRA_WEB_DIR"));
    }

    #[test]
    fn renders_required_keys_and_paths() {
        let out = render(&PlistInputs {
            service:     crate::install::unit::ServiceKind::Server,
            mira_bin:    &PathBuf::from("/Users/me/.cargo/bin/mira"),
            config_path: &PathBuf::from("/Users/me/.mira/config/mira_config.json"),
            working_dir: &PathBuf::from("/Users/me"),
            data_dir:    &PathBuf::from("/Users/me/.mira/data"),
            web_dir:     None,
            log_dir:     &PathBuf::from("/Users/me/Library/Logs/mira"),
            extra_env:   &[],
        });

        assert!(out.contains("<key>Label</key>"));
        assert!(out.contains("<string>com.mira</string>"));
        assert!(out.contains("<key>ProgramArguments</key>"));
        assert!(out.contains("<string>/Users/me/.cargo/bin/mira</string>"));
        assert!(out.contains("<string>--server</string>"));
        assert!(out.contains("<string>/Users/me/.mira/config/mira_config.json</string>"));
        assert!(out.contains("<string>--data-dir</string>"));
        assert!(out.contains("<string>/Users/me/.mira/data</string>"));
        assert!(out.contains("<key>RunAtLoad</key>"));
        assert!(out.contains("<key>KeepAlive</key>"));
        assert!(out.contains("<string>/Users/me/Library/Logs/mira/mira.out.log</string>"));
        assert!(out.contains("<string>/Users/me/Library/Logs/mira/mira.err.log</string>"));
        assert!(!out.contains("MIRA_WEB_DIR"));
    }

    #[test]
    fn web_dir_emits_environment_variables_dict() {
        let out = render(&PlistInputs {
            service:     crate::install::unit::ServiceKind::Server,
            mira_bin:    &PathBuf::from("/x"),
            config_path: &PathBuf::from("/y"),
            working_dir: &PathBuf::from("/z"),
            data_dir:    &PathBuf::from("/d"),
            web_dir:     Some(&PathBuf::from("/Users/me/MIRA/web/dist")),
            log_dir:     &PathBuf::from("/tmp"),
            extra_env:   &[],
        });
        assert!(out.contains("<key>EnvironmentVariables</key>"));
        assert!(out.contains("<key>MIRA_WEB_DIR</key>"));
        assert!(out.contains("<string>/Users/me/MIRA/web/dist</string>"));
    }

    #[test]
    fn extra_env_entries_appear_in_environment_dict() {
        let out = render(&PlistInputs {
            service:     crate::install::unit::ServiceKind::Server,
            mira_bin:    &PathBuf::from("/x"),
            config_path: &PathBuf::from("/y"),
            working_dir: &PathBuf::from("/z"),
            data_dir:    &PathBuf::from("/d"),
            web_dir:     None,
            log_dir:     &PathBuf::from("/tmp"),
            extra_env:   &[("ORT_DYLIB_PATH", "/opt/homebrew/lib/libonnxruntime.dylib")],
        });
        assert!(out.contains("<key>EnvironmentVariables</key>"));
        assert!(out.contains("<key>ORT_DYLIB_PATH</key>"));
        assert!(out.contains("<string>/opt/homebrew/lib/libonnxruntime.dylib</string>"));
    }

    #[test]
    fn xml_metacharacters_are_escaped() {
        // Pretend a user has an absurd home directory; the plist must still parse.
        let out = render(&PlistInputs {
            service:     crate::install::unit::ServiceKind::Server,
            mira_bin:    &PathBuf::from("/Users/a&b/<bin>/mira"),
            config_path: &PathBuf::from("/y"),
            working_dir: &PathBuf::from("/z"),
            data_dir:    &PathBuf::from("/d"),
            web_dir:     None,
            log_dir:     &PathBuf::from("/tmp"),
            extra_env:   &[],
        });
        assert!(out.contains("/Users/a&amp;b/&lt;bin&gt;/mira"));
        assert!(!out.contains("/Users/a&b/<bin>/mira"));
    }
}
