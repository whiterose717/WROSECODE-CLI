//! User-defined commands and agents written as markdown files with YAML
//! frontmatter (opencode parity).
//!
//! `.wrosecode/commands/*.md` becomes a slash command whose body is a prompt
//! template; `.wrosecode/agents/*.md` becomes an agent whose body is an extra
//! system prompt. Both are read from the project root and from
//! `~/.wrosecode/`, the project overriding the personal copy on a name clash.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A slash command loaded from `.wrosecode/commands/<name>.md`.
#[derive(Clone, Debug)]
pub struct UserCommand {
    /// Command name including the leading slash, e.g. `/ship`.
    pub name: String,
    pub description: String,
    pub category: String,
    /// Prompt template; `$ARGUMENTS` is replaced with the typed arguments.
    pub body: String,
}

impl UserCommand {
    /// Expand the template with the arguments the user typed after the name.
    pub fn expand(&self, args: &str) -> String {
        let expanded = if self.body.contains("$ARGUMENTS") {
            self.body.replace("$ARGUMENTS", args.trim())
        } else if args.trim().is_empty() {
            self.body.clone()
        } else {
            format!("{}\n{}", self.body.trim_end(), args.trim())
        };
        expanded
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }
}

/// An agent loaded from `.wrosecode/agents/<name>.md`.
#[derive(Clone, Debug)]
pub struct UserAgent {
    pub name: String,
    pub description: String,
    /// `build`, `plan`, or `general`; keeps the current mode when absent.
    pub mode: String,
    /// Thinking level name (`low`, `high`, `deep`, …) when the file names one.
    pub thinking: Option<String>,
    /// Extra system prompt: the markdown body of the file.
    pub body: String,
}

#[derive(Default, Deserialize)]
struct CommandMeta {
    name: Option<String>,
    description: Option<String>,
    category: Option<String>,
}

#[derive(Default, Deserialize)]
struct AgentMeta {
    name: Option<String>,
    description: Option<String>,
    mode: Option<String>,
    thinking: Option<String>,
}

/// Split `---`-fenced YAML frontmatter from the markdown body.
fn split(raw: &str) -> (Option<&str>, &str) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return (None, raw);
    };
    match rest.split_once("\n---\n") {
        Some((yaml, body)) => (Some(yaml), body),
        None => (None, raw),
    }
}

fn parse_meta<T: for<'de> Deserialize<'de> + Default>(raw: &str) -> (T, String) {
    let (yaml, body) = split(raw);
    let meta = yaml
        .and_then(|yaml| serde_yaml::from_str(yaml).ok())
        .unwrap_or_default();
    (meta, body.trim().to_string())
}

/// Project then personal command directories; the project entry wins.
fn command_dirs(root: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    vec![
        home.join(".wrosecode/commands"),
        root.join(".wrosecode/commands"),
    ]
}

fn agent_dirs(root: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    vec![
        home.join(".wrosecode/agents"),
        root.join(".wrosecode/agents"),
    ]
}

/// All slash commands defined by the user, keyed by name so a later
/// directory replaces an earlier definition of the same command.
pub fn discover_commands(root: &Path) -> Vec<UserCommand> {
    let mut found: BTreeMap<String, UserCommand> = BTreeMap::new();
    for dir in command_dirs(root) {
        for (name, raw) in read_markdown(&dir) {
            let (meta, body) = parse_meta::<CommandMeta>(&raw);
            if body.is_empty() {
                continue;
            }
            let command = UserCommand {
                name: meta
                    .name
                    .map(normalize_command_name)
                    .unwrap_or_else(|| normalize_command_name(name)),
                description: meta.description.unwrap_or_default(),
                category: meta.category.unwrap_or_else(|| "User".into()),
                body,
            };
            found.insert(command.name.clone(), command);
        }
    }
    found.into_values().collect()
}

/// All agents defined by the user, keyed by name.
pub fn discover_agents(root: &Path) -> Vec<UserAgent> {
    let mut found: BTreeMap<String, UserAgent> = BTreeMap::new();
    for dir in agent_dirs(root) {
        for (name, raw) in read_markdown(&dir) {
            let (meta, body) = parse_meta::<AgentMeta>(&raw);
            if body.is_empty() {
                continue;
            }
            let agent = UserAgent {
                name: meta.name.unwrap_or(name),
                description: meta.description.unwrap_or_default(),
                mode: meta
                    .mode
                    .filter(|mode| matches!(mode.as_str(), "build" | "plan" | "general"))
                    .unwrap_or_else(|| "build".into()),
                thinking: meta.thinking,
                body,
            };
            found.insert(agent.name.clone(), agent);
        }
    }
    found.into_values().collect()
}

/// Expand a prompt that is a user-defined slash command (`/ship the fix`);
/// any other prompt comes back unchanged.
pub fn expand_prompt(root: &Path, prompt: &str) -> String {
    let (name, args) = prompt
        .split_once(' ')
        .map(|(name, args)| (name, args.trim()))
        .unwrap_or((prompt, ""));
    if !name.starts_with('/') {
        return prompt.to_string();
    }
    discover_commands(root)
        .into_iter()
        .find(|command| command.name == name)
        .map(|command| command.expand(args))
        .unwrap_or_else(|| prompt.to_string())
}

/// File stem to command name: `/ship` for `ship.md`.
fn normalize_command_name(name: String) -> String {
    if name.starts_with('/') {
        name
    } else {
        format!("/{name}")
    }
}

/// Markdown files directly inside `dir`, sorted by file stem.
fn read_markdown(dir: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "md"))
        .filter_map(|entry| {
            let stem = entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".md")
                .to_string();
            let raw = std::fs::read_to_string(entry.path()).ok()?;
            Some((stem, raw))
        })
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wrose-md-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn frontmatter_is_split_from_the_body() {
        let (meta, body) = split("---\nname: ship\n---\nship it\n");
        assert_eq!(meta, Some("name: ship"));
        assert_eq!(body, "ship it\n");
        let (meta, body) = split("no frontmatter\n");
        assert!(meta.is_none());
        assert_eq!(body, "no frontmatter\n");
    }

    #[test]
    fn a_command_file_becomes_a_slash_command() {
        let root = scratch("command");
        std::fs::create_dir_all(root.join(".wrosecode/commands")).unwrap();
        std::fs::write(
            root.join(".wrosecode/commands/ship.md"),
            "---\ndescription: Ship the branch\ncategory: Project\n---\nCommit and push $ARGUMENTS.\n",
        )
        .unwrap();
        let commands = discover_commands(&root);
        let ship = commands
            .iter()
            .find(|command| command.name == "/ship")
            .unwrap();
        assert_eq!(ship.description, "Ship the branch");
        assert_eq!(ship.category, "Project");
        assert_eq!(ship.expand("the fix"), "Commit and push the fix.");
        assert_eq!(ship.expand(""), "Commit and push .");
    }

    #[test]
    fn arguments_without_a_placeholder_are_appended() {
        let root = scratch("append");
        std::fs::create_dir_all(root.join(".wrosecode/commands")).unwrap();
        std::fs::write(
            root.join(".wrosecode/commands/review.md"),
            "Review the working tree.\n",
        )
        .unwrap();
        let commands = discover_commands(&root);
        let review = commands
            .iter()
            .find(|command| command.name == "/review")
            .unwrap();
        assert_eq!(
            review.expand("focus on tests"),
            "Review the working tree.\nfocus on tests"
        );
        assert_eq!(review.expand("  "), "Review the working tree.");
    }

    #[test]
    fn an_agent_file_carries_mode_and_system_prompt() {
        let root = scratch("agent");
        std::fs::create_dir_all(root.join(".wrosecode/agents")).unwrap();
        std::fs::write(
            root.join(".wrosecode/agents/reviewer.md"),
            "---\ndescription: Reviews diffs\nmode: plan\nthinking: deep\n---\nBe suspicious of untested changes.\n",
        )
        .unwrap();
        let agents = discover_agents(&root);
        let reviewer = agents
            .iter()
            .find(|agent| agent.name == "reviewer")
            .unwrap();
        assert_eq!(reviewer.mode, "plan");
        assert_eq!(reviewer.thinking.as_deref(), Some("deep"));
        assert_eq!(reviewer.body, "Be suspicious of untested changes.");
    }

    #[test]
    fn an_unknown_mode_falls_back_to_build() {
        let root = scratch("mode");
        std::fs::create_dir_all(root.join(".wrosecode/agents")).unwrap();
        std::fs::write(
            root.join(".wrosecode/agents/hero.md"),
            "mode: chaos\nDo things.\n",
        )
        .unwrap();
        let agents = discover_agents(&root);
        let hero = agents.iter().find(|agent| agent.name == "hero").unwrap();
        assert_eq!(hero.mode, "build");
    }

    #[test]
    fn a_file_without_frontmatter_is_a_plain_agent() {
        let root = scratch("plain");
        std::fs::create_dir_all(root.join(".wrosecode/agents")).unwrap();
        std::fs::write(
            root.join(".wrosecode/agents/scribe.md"),
            "Always quote sources.\n",
        )
        .unwrap();
        let agents = discover_agents(&root);
        let scribe = agents.iter().find(|agent| agent.name == "scribe").unwrap();
        assert_eq!(scribe.body, "Always quote sources.");
    }

    #[test]
    fn a_prompt_that_is_a_command_expands_and_others_do_not() {
        let root = scratch("expand");
        std::fs::create_dir_all(root.join(".wrosecode/commands")).unwrap();
        std::fs::write(
            root.join(".wrosecode/commands/ship.md"),
            "Commit $ARGUMENTS then push.\n",
        )
        .unwrap();
        assert_eq!(
            expand_prompt(&root, "/ship the fix"),
            "Commit the fix then push."
        );
        assert_eq!(
            expand_prompt(&root, "/deploy production"),
            "/deploy production"
        );
        assert_eq!(expand_prompt(&root, "just a prompt"), "just a prompt");
    }

    #[test]
    fn missing_directories_yield_nothing() {
        let root = scratch("empty");
        assert!(discover_commands(&root).is_empty());
        assert!(discover_agents(&root).is_empty());
    }
}
