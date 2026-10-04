use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    pub category: &'static str,
    pub description: &'static str,
}

/// One row of the command palette: a built-in spec or a user command from
/// `.wrosecode/commands/*.md`, both rendered the same way.
#[derive(Clone, Debug)]
pub struct PaletteEntry {
    pub name: String,
    pub description: String,
}

impl From<&CommandSpec> for PaletteEntry {
    fn from(spec: &CommandSpec) -> Self {
        PaletteEntry {
            name: spec.name.into(),
            description: spec.description.into(),
        }
    }
}

impl From<&crate::markdown::UserCommand> for PaletteEntry {
    fn from(command: &crate::markdown::UserCommand) -> Self {
        PaletteEntry {
            name: command.name.clone(),
            description: command.description.clone(),
        }
    }
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/agents",
        category: "Agents",
        description: "Choose build, plan, or general",
    },
    CommandSpec {
        name: "/build",
        category: "Agents",
        description: "Switch to the build agent",
    },
    CommandSpec {
        name: "/plan",
        category: "Agents",
        description: "Switch to the read-only plan agent",
    },
    CommandSpec {
        name: "/harness",
        category: "Agents",
        description: "Switch prompt and tool-call style",
    },
    CommandSpec {
        name: "/skills",
        category: "Skills",
        description: "List and activate a skill",
    },
    CommandSpec {
        name: "/connect",
        category: "Providers",
        description: "Connect a provider with a guided setup",
    },
    CommandSpec {
        name: "/providers",
        category: "Providers",
        description: "List providers or add one",
    },
    CommandSpec {
        name: "/models",
        category: "Providers",
        description: "List and switch available models",
    },
    CommandSpec {
        name: "/model",
        category: "Providers",
        description: "Switch directly to a model",
    },
    CommandSpec {
        name: "/mcps",
        category: "Providers",
        description: "List, add, or remove MCP servers",
    },
    CommandSpec {
        name: "/compact",
        category: "Session",
        description: "Summarize the conversation to free context",
    },
    CommandSpec {
        name: "/clear",
        category: "Session",
        description: "Clear the transcript (add --context to also drop conversation context)",
    },
    CommandSpec {
        name: "/theme",
        category: "Session",
        description: "Pick the color theme (or pass a name)",
    },
    CommandSpec {
        name: "/new",
        category: "Session",
        description: "Archive this session and start a new one",
    },
    CommandSpec {
        name: "/sessions",
        category: "Session",
        description: "List and resume saved sessions",
    },
    CommandSpec {
        name: "/move",
        category: "Session",
        description: "Rename the current session",
    },
    CommandSpec {
        name: "/editor",
        category: "Session",
        description: "Compose a prompt in your editor",
    },
    CommandSpec {
        name: "/memory",
        category: "Session",
        description: "List recalled memory facts",
    },
    CommandSpec {
        name: "/greet",
        category: "Session",
        description: "Show the welcome banner and status",
    },
    CommandSpec {
        name: "/debug",
        category: "Session",
        description: "Show diagnostics for this session",
    },
    CommandSpec {
        name: "/stats",
        category: "Session",
        description: "Show live usage and timing counters",
    },
    CommandSpec {
        name: "/dashboard",
        category: "Session",
        description: "Open the eight-panel live dashboard (Ctrl+D)",
    },
    CommandSpec {
        name: "/verbosity",
        category: "Session",
        description: "Set compact, normal, or verbose tool output",
    },
    CommandSpec {
        name: "/think",
        category: "Session",
        description: "Set thinking level: off, low, medium, high, max, auto",
    },
    CommandSpec {
        name: "/help",
        category: "Session",
        description: "Show every slash command",
    },
    CommandSpec {
        name: "/exit",
        category: "Session",
        description: "Exit WROSECODE",
    },
    CommandSpec {
        name: "/quit",
        category: "Session",
        description: "Exit WROSECODE",
    },
    CommandSpec {
        name: "/init",
        category: "Project",
        description: "Create project configuration and guidance",
    },
    CommandSpec {
        name: "/diff",
        category: "Project",
        description: "Show uncommitted changes",
    },
    CommandSpec {
        name: "/commit",
        category: "Project",
        description: "Review and commit working-tree changes",
    },
    CommandSpec {
        name: "/review",
        category: "Project",
        description: "Ask the model to review the current diff",
    },
    CommandSpec {
        name: "/issues",
        category: "Project",
        description: "List open issues from the git remote",
    },
    CommandSpec {
        name: "/rmslop",
        category: "Project",
        description: "Review agent-created scratch files for deletion",
    },
    CommandSpec {
        name: "/undo",
        category: "Project",
        description: "Restore the files changed by the last edit turn",
    },
    CommandSpec {
        name: "/redo",
        category: "Project",
        description: "Re-apply the restore done by /undo",
    },
    CommandSpec {
        name: "/add",
        category: "Project",
        description: "Pin a file's contents into every prompt (/add src/lib.rs)",
    },
    CommandSpec {
        name: "/drop",
        category: "Project",
        description: "Unpin a file, or /drop all to clear the pinned list",
    },
    CommandSpec {
        name: "/recipe",
        category: "Project",
        description: "Run a saved multi-step recipe (/recipe [name])",
    },
    CommandSpec {
        name: "/flags",
        category: "Project",
        description: "Search detected flag history",
    },
    CommandSpec {
        name: "/sandbox",
        category: "Project",
        description: "Inspect, warm, or destroy the shell sandbox",
    },
    CommandSpec {
        name: "/writeup",
        category: "Project",
        description: "Generate a Markdown CTF writeup",
    },
    CommandSpec {
        name: "/ctf",
        category: "Project",
        description: "Run the scope-guarded CTF autopilot on a challenge",
    },
    CommandSpec {
        name: "/export",
        category: "Session",
        description: "Export metrics as JSON and CSV",
    },
];

/// The `/help` listing: every built-in category, then the user's own commands
/// from `.wrosecode/commands/*.md` grouped by their `category:` frontmatter.
pub fn help(user: &[crate::markdown::UserCommand]) -> String {
    let mut output = String::new();
    for category in ["Agents", "Providers", "Session", "Project", "Skills"] {
        output.push_str(category);
        output.push('\n');
        for command in COMMANDS
            .iter()
            .filter(|command| command.category == category)
        {
            output.push_str(&format!("  {:<12} {}\n", command.name, command.description));
        }
        output.push('\n');
    }
    if !user.is_empty() {
        let mut groups: BTreeMap<&str, Vec<&crate::markdown::UserCommand>> = BTreeMap::new();
        for command in user {
            groups
                .entry(command.category.as_str())
                .or_default()
                .push(command);
        }
        for (category, commands) in groups {
            output.push_str(category);
            output.push('\n');
            for command in commands {
                output.push_str(&format!("  {:<12} {}\n", command.name, command.description));
            }
            output.push('\n');
        }
    }
    output
}

/// Fuzzy matches for the palette: built-ins first, then the user's commands.
pub fn filtered(query: &str, user: &[crate::markdown::UserCommand]) -> Vec<PaletteEntry> {
    let needle = query.trim_start_matches('/').to_ascii_lowercase();
    let mut matches: Vec<(usize, PaletteEntry)> = Vec::new();
    matches.extend(
        COMMANDS
            .iter()
            .filter_map(|command| fuzzy_score(command.name, &needle).map(|s| (s, command.into()))),
    );
    matches
        .extend(user.iter().filter_map(|command| {
            fuzzy_score(&command.name, &needle).map(|s| (s, command.into()))
        }));
    matches.sort_by_key(|(score, command)| (*score, command.name.len()));
    matches.into_iter().map(|(_, command)| command).collect()
}

pub(crate) fn fuzzy_score(value: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let value = value.trim_start_matches('/').to_ascii_lowercase();
    let mut position = 0;
    let mut score = 0;
    for character in needle.chars() {
        let offset = value[position..].find(character)?;
        score += offset;
        position += offset + character.len_utf8();
    }
    Some(score)
}

pub fn lookup(input: &str) -> Option<&'static CommandSpec> {
    let name = input.split_whitespace().next()?;
    COMMANDS.iter().find(|command| command.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::UserCommand;

    #[test]
    fn registry_contains_all_requested_commands() {
        for name in [
            "/add",
            "/agents",
            "/clear",
            "/compact",
            "/commit",
            "/connect",
            "/ctf",
            "/dashboard",
            "/debug",
            "/diff",
            "/drop",
            "/editor",
            "/exit",
            "/greet",
            "/help",
            "/init",
            "/issues",
            "/mcps",
            "/models",
            "/move",
            "/new",
            "/review",
            "/rmslop",
            "/sessions",
            "/skills",
            "/stats",
            "/theme",
            "/recipe",
            "/redo",
            "/think",
            "/undo",
            "/writeup",
        ] {
            assert!(lookup(name).is_some(), "missing {name}");
            assert!(help(&[]).contains(name));
        }
    }

    fn user_command(name: &str, description: &str) -> UserCommand {
        UserCommand {
            name: name.into(),
            description: description.into(),
            category: "User".into(),
            body: "do it".into(),
        }
    }

    #[test]
    fn the_palette_shows_user_commands_beside_the_builtins() {
        let user = [user_command("/ship", "Commit and push")];
        let entries = filtered("s", &user);
        assert!(
            entries.iter().any(|entry| entry.name == "/ship"),
            "user command missing from {entries:?}"
        );
        assert!(entries.iter().any(|entry| entry.name == "/skills"));
        assert!(filtered("", &user).len() > filtered("", &[]).len());
    }

    #[test]
    fn help_lists_user_commands_under_their_category() {
        let mut command = user_command("/ship", "Commit and push");
        command.category = "Release".into();
        let output = help(&[command]);
        assert!(output.contains("Release"));
        assert!(output.contains("/ship"));
        assert!(output.contains("Commit and push"));
    }
}
