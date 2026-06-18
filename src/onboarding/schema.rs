// SPDX-License-Identifier: AGPL-3.0-or-later

// src/onboarding/schema.rs
//! Loader + validator for the onboarding question schema
//! (`prompts/onboarding.yaml`).
//!
//! The schema is embedded at compile time so a fresh install has a working
//! onboarding flow with no external files. A deployment can override it by
//! loading a YAML file at runtime via [`OnboardingSchema::from_yaml`] (wired
//! up in a later step when the admin config knob exists). Invalid schemas
//! fail fast with a concrete error — `fn load()` is meant to be called
//! once at startup and its error surfaced, not silently swallowed.

use serde::{Deserialize, Serialize};

use super::profile_file::PROFILE_SECTIONS;

/// Raw bytes of the bundled default schema. Kept private so callers go
/// through [`OnboardingSchema::bundled`] — which also validates.
const BUNDLED_YAML: &str = include_str!("../../prompts/onboarding.yaml");

/// Currently the only writable column on the `users` table that onboarding
/// touches. `user_profile` keys are validated against [`USER_PROFILE_COLS`].
const USER_COLS: &[&str] = &["avatar"];

/// Columns onboarding may write through `user_profile.<col>`. Kept in sync
/// with the migration in `src/auth/models.rs`.
const USER_PROFILE_COLS: &[&str] = &[
    "full_name",
    "preferred_name",
    "nickname",
    "pronouns",
    "birth_date",
    "height_cm",
    "weight_kg",
    "eye_color",
    "hair_color",
    "timezone",
    "locale",
    "agent_name",
    // Virtual fan-out target handled by the tool layer.
    "contact_hours_start_end",
];

// ── Parsed write target ───────────────────────────────────────────────────────

/// Where a question's answer lands. Parsed from the `writes_to` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteTarget {
    /// `users` table column.
    User(String),
    /// `user_profile` table column.
    UserProfile(String),
    /// Section of the per-user `profile.md`. Key matches one entry in
    /// [`PROFILE_SECTIONS`].
    ProfileMd(String),
    /// A seed memory entry, tagged `source="onboarding"`.
    MemorySeed,
}

impl WriteTarget {
    fn parse(s: &str) -> Result<Self, String> {
        let (prefix, rest) = s.split_once('.').ok_or_else(|| {
            format!("writes_to '{}' is missing a '.<key>' suffix", s)
        })?;
        match prefix {
            "user" => {
                if !USER_COLS.contains(&rest) {
                    return Err(format!(
                        "writes_to 'user.{}' is not an allowed users column (allowed: {:?})",
                        rest, USER_COLS
                    ));
                }
                Ok(WriteTarget::User(rest.to_owned()))
            }
            "user_profile" => {
                if !USER_PROFILE_COLS.contains(&rest) {
                    return Err(format!(
                        "writes_to 'user_profile.{}' is not a known column", rest
                    ));
                }
                Ok(WriteTarget::UserProfile(rest.to_owned()))
            }
            "profile_md" => {
                if !PROFILE_SECTIONS.iter().any(|(k, _)| *k == rest) {
                    return Err(format!(
                        "writes_to 'profile_md.{}' is not a known profile.md section", rest
                    ));
                }
                Ok(WriteTarget::ProfileMd(rest.to_owned()))
            }
            "memory" => {
                if rest != "seed" {
                    return Err(format!(
                        "writes_to 'memory.{}' is not supported (only 'memory.seed')", rest
                    ));
                }
                Ok(WriteTarget::MemorySeed)
            }
            other => Err(format!(
                "writes_to prefix '{}' is not one of user, user_profile, profile_md, memory",
                other
            )),
        }
    }
}

// ── Raw YAML types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawSchema {
    version: u32,
    groups:  Vec<RawGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawGroup {
    id:    String,
    label: String,
    #[serde(default)]
    optional: bool,
    questions: Vec<RawQuestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawQuestion {
    key:         String,
    #[serde(default)]
    writes_to:   Option<String>,
    #[serde(default)]
    prompt_hint: Option<String>,
    #[serde(default)]
    helper_tool: Option<String>,
    #[serde(default)]
    options:     Option<Vec<String>>,
    /// Absent or true → required. Defaults matter here because onboarding
    /// treats absent as "ask unless user skips".
    #[serde(default = "default_required")]
    required: bool,
}

fn default_required() -> bool { true }

// ── Validated types ───────────────────────────────────────────────────────────

/// Validated onboarding schema. Constructed via [`OnboardingSchema::bundled`]
/// or [`OnboardingSchema::from_yaml`] — both enforce the same invariants.
#[derive(Debug, Clone)]
pub struct OnboardingSchema {
    pub version: u32,
    pub groups:  Vec<Group>,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub id:        String,
    pub label:     String,
    pub optional:  bool,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone)]
pub struct Question {
    pub key:         String,
    /// `None` for purely informational prompts (no recording). In practice
    /// all questions today have a target; kept optional to leave room for
    /// UI-only prompts (e.g. a welcome card) without a schema bump.
    pub writes_to:   Option<WriteTarget>,
    pub prompt_hint: Option<String>,
    pub helper_tool: Option<String>,
    pub options:     Option<Vec<String>>,
    pub required:    bool,
}

impl OnboardingSchema {
    /// Load the schema that ships with the binary.
    pub fn bundled() -> Result<Self, String> {
        Self::from_yaml(BUNDLED_YAML)
    }

    /// Parse and validate a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self, String> {
        let raw: RawSchema = serde_yaml::from_str(yaml)
            .map_err(|e| format!("onboarding schema is not valid YAML: {}", e))?;

        if raw.version != 1 {
            return Err(format!("unsupported onboarding schema version: {}", raw.version));
        }
        if raw.groups.is_empty() {
            return Err("onboarding schema has no groups".to_owned());
        }

        let mut seen_group_ids = std::collections::HashSet::new();
        let mut seen_keys      = std::collections::HashSet::new();
        let mut groups         = Vec::with_capacity(raw.groups.len());

        for g in raw.groups {
            if !seen_group_ids.insert(g.id.clone()) {
                return Err(format!("duplicate group id: {}", g.id));
            }
            if g.questions.is_empty() {
                return Err(format!("group '{}' has no questions", g.id));
            }

            let mut questions = Vec::with_capacity(g.questions.len());
            for q in g.questions {
                if !seen_keys.insert(q.key.clone()) {
                    return Err(format!(
                        "duplicate question key '{}' in group '{}'", q.key, g.id
                    ));
                }
                let writes_to = match q.writes_to {
                    Some(ref s) => Some(
                        WriteTarget::parse(s)
                            .map_err(|e| format!("group '{}', question '{}': {}", g.id, q.key, e))?
                    ),
                    None => None,
                };
                questions.push(Question {
                    key:         q.key,
                    writes_to,
                    prompt_hint: q.prompt_hint,
                    helper_tool: q.helper_tool,
                    options:     q.options,
                    required:    q.required,
                });
            }

            groups.push(Group {
                id:        g.id,
                label:     g.label,
                optional:  g.optional,
                questions,
            });
        }

        Ok(OnboardingSchema { version: raw.version, groups })
    }

    /// Lookup a group by id.
    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// Lookup a question by key across all groups.
    pub fn question(&self, key: &str) -> Option<(&Group, &Question)> {
        for g in &self.groups {
            if let Some(q) = g.questions.iter().find(|q| q.key == key) {
                return Some((g, q));
            }
        }
        None
    }

    /// All group ids in order — used by the progress strip.
    pub fn group_ids(&self) -> Vec<&str> {
        self.groups.iter().map(|g| g.id.as_str()).collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_schema_loads_and_is_non_empty() {
        let s = OnboardingSchema::bundled().expect("bundled schema must validate");
        assert!(s.groups.len() >= 5, "expected a meaningful question set");
        // Sanity: a couple of well-known groups exist.
        assert!(s.group("name").is_some());
        assert!(s.group("agent_style").is_some());
    }

    #[test]
    fn writes_to_routes_to_the_correct_target_kind() {
        let s = OnboardingSchema::bundled().unwrap();
        let (_, q) = s.question("preferred_name").unwrap();
        matches!(q.writes_to.as_ref().unwrap(), WriteTarget::UserProfile(c) if c == "preferred_name");

        let (_, q) = s.question("top_goals").unwrap();
        matches!(q.writes_to.as_ref().unwrap(), WriteTarget::ProfileMd(s) if s == "goals");

        let (_, q) = s.question("work_summary").unwrap();
        matches!(q.writes_to.as_ref().unwrap(), WriteTarget::MemorySeed);

        let (_, q) = s.question("avatar").unwrap();
        matches!(q.writes_to.as_ref().unwrap(), WriteTarget::User(c) if c == "avatar");
    }

    #[test]
    fn unknown_writes_to_prefix_fails() {
        let yaml = r#"
version: 1
groups:
  - id: g1
    label: Group
    questions:
      - key: k1
        writes_to: nowhere.x
"#;
        let err = OnboardingSchema::from_yaml(yaml).unwrap_err();
        assert!(err.contains("nowhere"), "got: {}", err);
    }

    #[test]
    fn unknown_profile_md_section_fails() {
        let yaml = r#"
version: 1
groups:
  - id: g1
    label: Group
    questions:
      - key: k1
        writes_to: profile_md.not_a_section
"#;
        let err = OnboardingSchema::from_yaml(yaml).unwrap_err();
        assert!(err.contains("profile_md"));
    }

    #[test]
    fn duplicate_group_id_fails() {
        let yaml = r#"
version: 1
groups:
  - id: g1
    label: A
    questions: [{key: k1}]
  - id: g1
    label: B
    questions: [{key: k2}]
"#;
        let err = OnboardingSchema::from_yaml(yaml).unwrap_err();
        assert!(err.contains("duplicate group id"));
    }

    #[test]
    fn duplicate_question_key_fails() {
        let yaml = r#"
version: 1
groups:
  - id: g1
    label: A
    questions: [{key: dup}]
  - id: g2
    label: B
    questions: [{key: dup}]
"#;
        let err = OnboardingSchema::from_yaml(yaml).unwrap_err();
        assert!(err.contains("duplicate question key"));
    }

    #[test]
    fn wrong_version_fails() {
        let yaml = "version: 99\ngroups: [{id: g, label: x, questions: [{key: k}]}]\n";
        let err = OnboardingSchema::from_yaml(yaml).unwrap_err();
        assert!(err.contains("version"));
    }

    #[test]
    fn empty_groups_fails() {
        let yaml = "version: 1\ngroups: []\n";
        assert!(OnboardingSchema::from_yaml(yaml).is_err());
    }
}
