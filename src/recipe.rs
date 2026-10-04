//! Reusable multi-step workflows (goose `recipes`): YAML or Markdown files
//! with parameters, discovered from `.wrosecode/recipes/` and
//! `~/.wrosecode/recipes/`, run with `wrosecode recipe run <name>` or `/recipe`.

use crate::agent::Agent;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How much of the accumulated step output `{{steps}}` hands to later steps.
const STEP_CONTEXT: usize = 12_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Params {
    /// `params: [target, port]` — names only, all required.
    Names(Vec<String>),
    /// `params: {target: {description: …, default: localhost}}`.
    Defined(BTreeMap<String, ParamDef>),
}

impl Default for Params {
    fn default() -> Self {
        Self::Names(Vec::new())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ParamDef {
    /// Shown by `/recipe` when it asks for this parameter.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    required: Option<bool>,
}

impl ParamDef {
    /// Required unless it has a default or says `required: false`.
    fn is_required(&self) -> bool {
        self.required.unwrap_or(self.default.is_none())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Step {
    Prompt { prompt: String },
    Command { command: String },
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawRecipe {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    params: Params,
    #[serde(default)]
    steps: Vec<Step>,
}

/// A discovered recipe: metadata plus the concrete file it came from.
#[derive(Debug, Clone)]
pub struct Recipe {
    pub name: String,
    pub description: String,
    pub version: String,
    pub params: Vec<(String, ParamDef)>,
    pub steps: Vec<Step>,
    pub origin: PathBuf,
}

impl Recipe {
    /// Missing parameters that have no default — what `/recipe` must ask for.
    pub fn required_params(&self) -> Vec<String> {
        self.params
            .iter()
            .filter(|(_, def)| def.is_required() && def.default.is_none())
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// Every recipe under the project and the user's home directory, sorted by
/// name; the project file wins a name clash.
pub fn discover(root: &Path) -> Result<Vec<Recipe>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut found: BTreeMap<String, Recipe> = BTreeMap::new();
    for dir in [
        root.join(".wrosecode/recipes"),
        home.join(".wrosecode/recipes"),
    ] {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let extension = path.extension().and_then(|value| value.to_str());
            if !matches!(extension, Some("yaml" | "yml" | "md")) {
                continue;
            }
            let recipe = parse(&path)?;
            found.insert(recipe.name.clone(), recipe);
        }
    }
    Ok(found.into_values().collect())
}

/// Find one recipe by name (or unique prefix), or fail listing what exists.
pub fn find(root: &Path, name: &str) -> Result<Recipe> {
    let all = discover(root)?;
    if let Some(recipe) = all.iter().find(|recipe| recipe.name == name) {
        return Ok(recipe.clone());
    }
    let mut prefix = all
        .iter()
        .filter(|recipe| recipe.name.starts_with(name))
        .map(|recipe| recipe.name.clone())
        .collect::<Vec<_>>();
    if prefix.len() == 1 {
        let unique = prefix.remove(0);
        return Ok(all
            .into_iter()
            .find(|recipe| recipe.name == unique)
            .expect("prefix match"));
    }
    let available = all
        .iter()
        .map(|recipe| recipe.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    bail!("no recipe named `{name}` (available: {})", available);
}

/// Parse one recipe file: YAML for `.yaml`/`.yml`, Markdown frontmatter for
/// `.md` (the body becomes the single prompt step).
pub fn parse(path: &Path) -> Result<Recipe> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let parsed = if path.extension().and_then(|value| value.to_str()) == Some("md") {
        let (meta, body) = split_frontmatter(&raw);
        let mut recipe: RawRecipe = serde_yaml::from_str(meta).unwrap_or_default();
        if recipe.steps.is_empty() && !body.trim().is_empty() {
            recipe.steps.push(Step::Prompt {
                prompt: body.trim().to_string(),
            });
        }
        recipe
    } else {
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
    };
    let params = match parsed.params {
        Params::Names(names) => names
            .into_iter()
            .map(|name| (name, ParamDef::default()))
            .collect(),
        Params::Defined(map) => map.into_iter().collect(),
    };
    if parsed.steps.is_empty() {
        bail!("{} has no steps", path.display());
    }
    Ok(Recipe {
        name: parsed.name.unwrap_or(stem),
        description: parsed.description,
        version: parsed.version,
        params,
        steps: parsed.steps,
        origin: path.to_path_buf(),
    })
}

/// `---` frontmatter split; a file without it is treated as body-only.
fn split_frontmatter(raw: &str) -> (&str, &str) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return ("", raw);
    };
    match rest.split_once("\n---\n") {
        Some((meta, body)) => (meta, body),
        None => ("", raw),
    }
}

/// Merge provided values with defaults and the step context, then fail if a
/// required parameter is still missing or an unknown one was supplied.
pub fn resolve(
    recipe: &Recipe,
    provided: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let known = recipe
        .params
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    for key in provided.keys() {
        if !known.contains(&key.as_str()) {
            bail!(
                "unknown parameter `{key}` for recipe `{}` (expects: {})",
                recipe.name,
                known.join(", ")
            );
        }
    }
    let mut values = BTreeMap::new();
    let mut missing = Vec::new();
    for (name, def) in &recipe.params {
        if let Some(value) = provided.get(name) {
            values.insert(name.clone(), value.clone());
        } else if let Some(default) = &def.default {
            values.insert(name.clone(), default.clone());
        } else if def.is_required() {
            missing.push(name.clone());
        }
    }
    if !missing.is_empty() {
        bail!(
            "missing parameters for `{}`: {}",
            recipe.name,
            missing.join(", ")
        );
    }
    Ok(values)
}

/// `{{name}}` substitution; unknown placeholders are left alone so a recipe
/// can hand `{{prev}}`/`{{steps}}` through untouched.
pub fn substitute(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = template.to_string();
    for (key, value) in values {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

/// Execute the steps in order: `prompt` steps run a normal agent turn,
/// `command` steps run in the project shell, and each output becomes
/// `{{prev}}` (and, trimmed to a cap, `{{steps}}`) for the steps after it.
///
/// Recipes are files the user placed in their own repository and invoked by
/// hand, so `command:` steps run with the configured shell timeout.
pub async fn run(
    agent: &mut Agent,
    recipe: &Recipe,
    values: &BTreeMap<String, String>,
    mut report: impl FnMut(String),
) -> Result<String> {
    let mut prev = String::new();
    let mut all = String::new();
    for (index, step) in recipe.steps.iter().enumerate() {
        let mut vars = values.clone();
        vars.insert("prev".into(), prev.clone());
        vars.insert("steps".into(), all.clone());
        let (label, output) = match step {
            Step::Prompt { prompt } => {
                let text = substitute(prompt, &vars);
                report(format!(
                    "step {} · prompt: {}",
                    index + 1,
                    first_line(&text)
                ));
                ("prompt", agent.turn(&text).await?)
            }
            Step::Command { command } => {
                let text = substitute(command, &vars);
                report(format!("step {} · command: {text}", index + 1));
                let output = crate::tools::shell::run_with_timeout(
                    &text,
                    &agent.config.root,
                    agent.config.shell_timeout_seconds,
                )
                .await?;
                ("command", output)
            }
        };
        prev = output.clone();
        push_step_context(&mut all, label, index + 1, &output);
    }
    Ok(prev)
}

/// Append one step's output to the `{{steps}}` context, dropping the oldest
/// output once it passes [`STEP_CONTEXT`]. `run` and the TUI's `/recipe`
/// share this so both hand later steps the same bounded history.
pub fn push_step_context(all: &mut String, label: &str, index: usize, output: &str) {
    if !all.is_empty() {
        all.push_str("\n\n");
    }
    all.push_str(&format!("[{label} step {index}]\n{output}"));
    if all.len() > STEP_CONTEXT {
        let mut cut = all.len() - STEP_CONTEXT;
        while cut < all.len() && !all.is_char_boundary(cut) {
            cut += 1;
        }
        *all = format!("(earlier steps omitted)\n{}", &all[cut..]);
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect()
}

/// Parse `key=value` pairs from `recipe run … --set key=value`.
pub fn parse_sets(pairs: &[String]) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            bail!("--set expects KEY=VALUE, got `{pair}`");
        };
        if key.trim().is_empty() {
            bail!("--set expects KEY=VALUE, got `{pair}`");
        }
        values.insert(key.trim().to_string(), value.to_string());
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("wrosecode-recipe-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".wrosecode/recipes")).unwrap();
        root
    }

    #[test]
    fn yaml_recipes_carry_params_and_ordered_steps() {
        let root = workspace("yaml");
        std::fs::write(
            root.join(".wrosecode/recipes/audit.yaml"),
            r#"
name: web-audit
description: Probe a host
version: "1.0.0"
params:
  target:
    description: Host to audit
    default: localhost
  depth:
    default: "2"
steps:
  - prompt: "Recon {{target}} to depth {{depth}}"
  - command: "printf '%s' {{target}}"
  - prompt: "Summarize what happened: {{steps}}"
"#,
        )
        .unwrap();

        let recipe = find(&root, "web-audit").expect("discover");
        assert_eq!(recipe.description, "Probe a host");
        assert_eq!(recipe.version, "1.0.0");
        assert_eq!(recipe.steps.len(), 3);
        assert_eq!(
            recipe.required_params(),
            Vec::<String>::new(),
            "defaults make every parameter optional"
        );

        let values = resolve(&recipe, &BTreeMap::new()).expect("defaults");
        assert_eq!(values.get("target").map(String::as_str), Some("localhost"));

        let mut provided = BTreeMap::new();
        provided.insert("target".to_string(), "example.com".to_string());
        let values = resolve(&recipe, &provided).expect("provided");
        assert_eq!(
            values.get("target").map(String::as_str),
            Some("example.com")
        );

        let mut unknown = BTreeMap::new();
        unknown.insert("targets".to_string(), "x".to_string());
        assert!(resolve(&recipe, &unknown).is_err(), "typo must fail");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_required_parameter_must_be_supplied() {
        let root = workspace("required");
        std::fs::write(
            root.join(".wrosecode/recipes/scan.yaml"),
            r#"
name: scan
params: [target]
steps:
  - prompt: "scan {{target}}"
"#,
        )
        .unwrap();
        let recipe = find(&root, "scan").expect("discover");
        assert_eq!(recipe.required_params(), vec!["target".to_string()]);
        let error = resolve(&recipe, &BTreeMap::new()).expect_err("must fail");
        assert!(error.to_string().contains("target"), "{error}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_markdown_recipe_uses_its_body_as_the_prompt_step() {
        let root = workspace("markdown");
        std::fs::write(
            root.join(".wrosecode/recipes/explain.md"),
            "---\ndescription: Explain a command\nparams: [command]\n---\nExplain what `{{command}}` does in this repository.\n",
        )
        .unwrap();
        let recipe = find(&root, "explain").expect("discover");
        assert_eq!(recipe.name, "explain");
        assert_eq!(recipe.description, "Explain a command");
        assert_eq!(recipe.steps.len(), 1);
        match &recipe.steps[0] {
            Step::Prompt { prompt } => assert!(prompt.contains("{{command}}"), "{prompt}"),
            Step::Command { .. } => panic!("the body must be a prompt step"),
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_project_recipe_wins_over_the_user_one_and_prefixes_resolve() {
        let root = workspace("override");
        std::fs::write(
            root.join(".wrosecode/recipes/ping.md"),
            "---\n---\nPROJECT-MARKER\n",
        )
        .unwrap();
        let recipe = find(&root, "ping").expect("discover");
        match &recipe.steps[0] {
            Step::Prompt { prompt } => assert!(prompt.contains("PROJECT-MARKER"), "{prompt}"),
            Step::Command { .. } => panic!("expected a prompt step"),
        }
        assert!(find(&root, "pi").expect("prefix").name == "ping");
        let error = find(&root, "nope").expect_err("missing");
        assert!(error.to_string().contains("available"), "{error}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn substitution_fills_values_and_leaves_unknown_placeholders_alone() {
        let mut values = BTreeMap::new();
        values.insert("target".to_string(), "example.com".to_string());
        assert_eq!(
            substitute("scan {{target}} then {{prev}}", &values),
            "scan example.com then {{prev}}"
        );
        assert!(parse_sets(&["a=1".into(), "b=x=y".into()])
            .expect("parse")
            .get("b")
            .is_some_and(|value| value == "x=y"));
        assert!(parse_sets(&["novalue".into()]).is_err());
    }
}
