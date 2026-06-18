// SPDX-License-Identifier: AGPL-3.0-or-later

// src/config/validate.rs
//! JSON Schema validation for the MIRA config file.
//!
//! Uses the `jsonschema` crate (Draft-7) to validate a parsed
//! `serde_json::Value` against the embedded schema and returns
//! human-readable, path-annotated error messages.

use jsonschema::{Draft, JSONSchema};
use serde_json::Value;

use super::schema::SCHEMA_JSON;

/// Validate `config_value` against the embedded JSON Schema.
///
/// Returns `Ok(())` when the config is valid, or `Err(messages)` where each
/// string describes one schema violation in the form:
///
/// ```text
///   'providers.openrouter.api_key': null is not of types "string"
/// ```
pub fn validate_config_json(config_value: &Value) -> Result<(), Vec<String>> {
    let schema_value: Value = serde_json::from_str(SCHEMA_JSON)
        .expect("embedded mira_config.schema.json must be valid JSON");

    let compiled = JSONSchema::options()
        .with_draft(Draft::Draft7)
        .compile(&schema_value)
        .expect("embedded mira_config.schema.json must be a valid JSON Schema");

    match compiled.validate(config_value) {
        Ok(()) => Ok(()),
        Err(errors) => {
            let messages: Vec<String> = errors
                .map(|e| {
                    // instance_path is a JSONPath like "/providers/openrouter/api_key"
                    // Convert to dotted notation for readability.
                    let raw = e.instance_path.to_string();
                    let location = if raw.is_empty() || raw == "/" {
                        "config root".to_string()
                    } else {
                        raw.trim_start_matches('/').replace('/', ".")
                    };
                    format!("  '{}': {}", location, e)
                })
                .collect();
            Err(messages)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_valid() -> Value {
        json!({
            "config_version": "1",
            "primary_provider": "lmstudio"
        })
    }

    #[test]
    fn valid_minimal_config_passes() {
        assert!(validate_config_json(&minimal_valid()).is_ok());
    }

    #[test]
    fn missing_required_field_fails() {
        let v = json!({ "config_version": "1" });
        let errs = validate_config_json(&v).unwrap_err();
        assert!(!errs.is_empty());
        assert!(errs.iter().any(|e| e.contains("primary_provider")));
    }

    #[test]
    fn invalid_primary_provider_fails() {
        let mut v = minimal_valid();
        v["primary_provider"] = json!("unknown-provider");
        let errs = validate_config_json(&v).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("primary_provider")));
    }

    #[test]
    fn invalid_tui_theme_fails() {
        let mut v = minimal_valid();
        v["tui"] = json!({ "theme": "not-a-theme" });
        let errs = validate_config_json(&v).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("tui")));
    }

    #[test]
    fn invalid_log_level_fails() {
        let mut v = minimal_valid();
        v["logging"] = json!({ "level": "verbose" });
        let errs = validate_config_json(&v).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("logging")));
    }

    #[test]
    fn unknown_top_level_key_fails() {
        let mut v = minimal_valid();
        v["unknown_key"] = json!("oops");
        let errs = validate_config_json(&v).unwrap_err();
        assert!(!errs.is_empty());
    }

    #[test]
    fn port_out_of_range_fails() {
        let mut v = minimal_valid();
        v["server"] = json!({ "port": 0 });
        let errs = validate_config_json(&v).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("server")));
    }

    #[test]
    fn null_optional_field_passes() {
        let mut v = minimal_valid();
        v["providers"] = json!({
            "openrouter": { "api_key": null, "base_url": "https://openrouter.ai/api/v1", "default_model": "llama3" }
        });
        assert!(validate_config_json(&v).is_ok());
    }
}
