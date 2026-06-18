// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tui/completion.rs
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub command:     String,
    pub description: String,
    #[allow(dead_code)] // reserved for command-palette preview (feat/rich-tui)
    pub example:     String,
}

pub struct CommandDef {
    pub command:     &'static str,
    pub description: &'static str,
    pub example:     &'static str,
}

pub fn all_commands() -> Vec<CommandDef> {
    vec![
        // Memory
        CommandDef { command: "/memory-list",            description: "List stored memories",                   example: "/memory-list"                      },
        CommandDef { command: "/memory-store <text>",    description: "Save a memory",                          example: "/memory-store My name is Alice"     },
        CommandDef { command: "/memory-delete <id>",     description: "Delete memory by ID",                    example: "/memory-delete 42"                  },
        CommandDef { command: "/memory-search <query>",  description: "Search memories by keyword",             example: "/memory-search project"             },
        // Context / conversation
        CommandDef { command: "/clear",                  description: "Clear chat context and start fresh",      example: "/clear"                             },
        CommandDef { command: "/new",                    description: "Start a new conversation (same as /clear)", example: "/new"                             },
        CommandDef { command: "/ctx",                    description: "Show current context stats",              example: "/ctx"                               },
        CommandDef { command: "/tokens",                 description: "Show current token estimate",             example: "/tokens"                            },
        CommandDef { command: "/export <file>",          description: "Export conversation to a file",           example: "/export chat.md"                    },
        // Provider / model
        CommandDef { command: "/provider-list",          description: "List available providers",                example: "/provider-list"                     },
        CommandDef { command: "/provider-use <name>",    description: "Switch active provider",                  example: "/provider-use openrouter"           },
        CommandDef { command: "/model-list",             description: "List available models",                   example: "/model-list"                        },
        CommandDef { command: "/model-use <idx>",        description: "Switch model by index",                   example: "/model-use 1"                       },
        CommandDef { command: "/openrouter-list [filter]", description: "Browse OpenRouter catalog (paged)",     example: "/openrouter-list gpt"                 },
        CommandDef { command: "/openrouter-page <n>",    description: "Jump to page N of catalog",               example: "/openrouter-page 2"                  },
        CommandDef { command: "/openrouter-info <id>",   description: "Show one OpenRouter model in detail",     example: "/openrouter-info openai/gpt-4o"      },
        CommandDef { command: "/openrouter-use <id>",    description: "Use an OpenRouter model for next turn",   example: "/openrouter-use openai/gpt-4o"       },
        CommandDef { command: "/openrouter-refresh",     description: "Re-fetch the OpenRouter catalog",         example: "/openrouter-refresh"                 },
        // UI
        CommandDef { command: "/theme <name>",           description: "Switch colour theme",                     example: "/theme dracula"                     },
        CommandDef { command: "/layout <mode>",          description: "Change layout: simple/standard/right-full/left-full/right-only/left-only", example: "/layout right-full" },
        // Session
        CommandDef { command: "/session-info",           description: "Show session information",                example: "/session-info"                      },
        CommandDef { command: "/session-clear",          description: "Clear session history",                   example: "/session-clear"                     },
        CommandDef { command: "/session-summary",        description: "Generate conversation summary",           example: "/session-summary"                   },
        // Tools
        CommandDef { command: "/tool-list",              description: "List available tools",                    example: "/tool-list"                         },
        CommandDef { command: "/tool-run <name>",        description: "Run a tool",                              example: "/tool-run shell_execute {\"command\":\"ls\"}" },
        // Misc
        CommandDef { command: "/signal-setup",           description: "Configure Signal messaging",              example: "/signal-setup"                      },
        CommandDef { command: "/reconnect",              description: "Re-probe backend health (clears banner)", example: "/reconnect"                         },
        CommandDef { command: "/help",                   description: "Show all commands with descriptions",     example: "/help"                              },
        CommandDef { command: "/version",                description: "Show MIRA version",                       example: "/version"                           },
        CommandDef { command: "/quit",                   description: "Exit MIRA",                               example: "/quit"                              },
    ]
}

/// Return fuzzy-matched completions for the current input prefix.
pub fn complete(input: &str) -> Vec<CompletionItem> {
    if input.is_empty() {
        return all_commands().into_iter().map(def_to_item).collect();
    }
    // For prefix match on the command name only (before any space/argument)
    let cmd_part = input.split_whitespace().next().unwrap_or(input);
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, CompletionItem)> = all_commands()
        .into_iter()
        .filter_map(|def| {
            // Match against the command prefix (before <args>)
            let def_cmd = def.command.split_whitespace().next().unwrap_or(def.command);
            matcher.fuzzy_match(def_cmd, cmd_part)
                .map(|score| (score, def_to_item(def)))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, item)| item).collect()
}

fn def_to_item(def: CommandDef) -> CompletionItem {
    CompletionItem {
        command:     def.command.to_string(),
        description: def.description.to_string(),
        example:     def.example.to_string(),
    }
}

/// Returns the bare command string (no argument placeholder) for tab-filling.
pub fn command_base(full: &str) -> &str {
    full.split_whitespace().next().unwrap_or(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_complete_slash_memory() {
        let results = complete("/mem");
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.command.contains("/memory")));
    }
    #[test]
    fn test_complete_empty_returns_all() {
        let results = complete("");
        assert!(results.len() >= 8);
    }
    #[test]
    fn test_complete_no_match() {
        let results = complete("/zzznomatch");
        assert!(results.is_empty());
    }
    #[test]
    fn test_all_commands_have_descriptions() {
        for cmd in all_commands() {
            assert!(!cmd.description.is_empty(), "command '{}' has no description", cmd.command);
        }
    }
    #[test]
    fn test_command_names_use_dashes_not_spaces() {
        for cmd in all_commands() {
            let base = cmd.command.split_whitespace().next().unwrap_or(cmd.command);
            // Command base (before args) must not contain spaces when hyphen was intended
            // This checks that multi-word commands like "/memory list" are gone
            let legacy = ["/memory list", "/memory store", "/memory delete",
                          "/memory search", "/provider list", "/provider use",
                          "/model list", "/model use", "/session info",
                          "/session clear", "/session summary", "/tool list",
                          "/tool run", "/signal setup"];
            assert!(!legacy.contains(&cmd.command.trim()), "command '{}' should use dashes", cmd.command);
        }
    }
    #[test]
    fn test_complete_model_list() {
        let results = complete("/model");
        assert!(results.iter().any(|r| r.command.contains("/model-list")));
    }
}
