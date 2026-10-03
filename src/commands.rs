#[derive(Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    pub category: &'static str,
    pub description: &'static str,
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
        name: "/verbosity",
        category: "Session",
        description: "Set compact, normal, or verbose tool output",
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
        name: "/export",
        category: "Session",
        description: "Export metrics as JSON and CSV",
    },
];

pub fn help() -> String {
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
    output
}

pub fn filtered(query: &str) -> Vec<&'static CommandSpec> {
    let needle = query.trim_start_matches('/').to_ascii_lowercase();
    let mut matches: Vec<_> = COMMANDS
        .iter()
        .filter_map(|command| fuzzy_score(command.name, &needle).map(|score| (score, command)))
        .collect();
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

    #[test]
    fn registry_contains_all_requested_commands() {
        for name in [
            "/agents",
            "/commit",
            "/connect",
            "/debug",
            "/diff",
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
        ] {
            assert!(lookup(name).is_some(), "missing {name}");
            assert!(help().contains(name));
        }
    }
}
