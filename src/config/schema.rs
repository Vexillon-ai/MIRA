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

/// The bundled, generated settings reference (produced by
/// `scripts/gen_settings_reference.py` from `SCHEMA_JSON`).
#[cfg(test)]
const SETTINGS_REFERENCE_MD: &str = include_str!("../../mira-docs/settings-reference.md");

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Collect every property path in the schema (`a`, `a.b`, `a.b.c`),
    /// descending only into object nodes with a `properties` map.
    fn schema_paths(node: &Value, prefix: &str, out: &mut Vec<String>) {
        let Some(props) = node.get("properties").and_then(Value::as_object) else { return };
        for (key, child) in props {
            let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
            out.push(path.clone());
            schema_paths(child, &path, out);
        }
    }

    /// Guards against `settings-reference.md` drifting from the schema — the
    /// exact failure a doc audit found (a whole section, `auth`, undocumented).
    /// Every schema property must appear in the generated reference, either as a
    /// bullet (`**\`path\`**`) or, for a top-level group, as a `## header`.
    /// When this fails, regenerate: `python3 scripts/gen_settings_reference.py`.
    #[test]
    fn settings_reference_documents_every_schema_key() {
        let schema: Value = serde_json::from_str(SCHEMA_JSON).expect("schema is valid JSON");
        let mut paths = Vec::new();
        schema_paths(&schema, "", &mut paths);
        assert!(paths.len() > 400, "sanity: expected many schema paths, got {}", paths.len());

        let doc = SETTINGS_REFERENCE_MD;
        let missing: Vec<&String> = paths
            .iter()
            .filter(|p| {
                !doc.contains(&format!("**`{p}`**"))         // documented as a bullet
                    && !doc.contains(&format!("\n## {p}\n")) // or a top-level section header
            })
            .collect();
        assert!(
            missing.is_empty(),
            "settings-reference.md is missing {} schema key(s): {:?}\n\
             Regenerate with: python3 scripts/gen_settings_reference.py",
            missing.len(),
            missing,
        );
    }
}
