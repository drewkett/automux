use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub agent: String,
    pub session_id: String,
}

#[derive(Deserialize)]
struct HookInput {
    session_id: String,
}

pub fn install_hooks() -> Result<()> {
    let executable = env::current_exe().context("could not locate the automux executable")?;
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let mut installed = Vec::new();

    if command_exists("claude") {
        let path = home.join(".claude/settings.json");
        install_hook(&path, "claude", &executable)?;
        installed.push(format!("Claude Code ({})", path.display()));
    }
    if command_exists("codex") {
        let path = home.join(".codex/hooks.json");
        install_hook(&path, "codex", &executable)?;
        installed.push(format!("Codex ({})", path.display()));
    }

    if installed.is_empty() {
        bail!("neither claude nor codex was found on PATH; no hooks installed");
    }
    println!("installed or updated automux hooks for:");
    for item in installed {
        println!("  {item}");
    }
    println!("Codex requires reviewing the new hook with /hooks before it will run.");
    Ok(())
}

pub fn register_agent(config: &Config, agent: &str, mut input: impl Read) -> Result<()> {
    if !matches!(agent, "claude" | "codex") {
        bail!("unsupported agent {agent:?}; expected claude or codex");
    }
    let Some(pane) = env::var_os("TMUX_PANE") else {
        return Ok(());
    };
    let pane = pane.to_string_lossy();
    let pane_number = pane
        .strip_prefix('%')
        .and_then(|value| value.parse::<u64>().ok())
        .context("TMUX_PANE did not contain a valid tmux pane ID")?;
    let hook: HookInput = serde_json::from_reader(&mut input).context("invalid hook input")?;
    if hook.session_id.trim().is_empty() {
        bail!("hook input contained an empty session_id");
    }

    let directory = config.state_dir.join("agents");
    fs::create_dir_all(&directory)?;
    atomic_json(
        &directory.join(format!("pane-{pane_number}.json")),
        &AgentSession {
            agent: agent.to_owned(),
            session_id: hook.session_id,
        },
    )
}

pub fn registry(config: &Config) -> Result<BTreeMap<String, AgentSession>> {
    let directory = config.state_dir.join("agents");
    let mut registry = BTreeMap::new();
    if !directory.exists() {
        return Ok(registry);
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(number) = name
            .to_str()
            .and_then(|name| name.strip_prefix("pane-"))
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(|number| number.parse::<u64>().ok())
        else {
            continue;
        };
        let session = serde_json::from_slice(&fs::read(entry.path())?)
            .with_context(|| format!("invalid JSON in {}", entry.path().display()))?;
        registry.insert(format!("%{number}"), session);
    }
    Ok(registry)
}

fn install_hook(path: &Path, agent: &str, executable: &Path) -> Result<()> {
    let mut root: Value = if path.exists() {
        serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("{} is not valid JSON; it was not modified", path.display()))?
    } else {
        json!({})
    };
    merge_hook(&mut root, agent, executable)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_json(path, &root)
}

fn merge_hook(root: &mut Value, agent: &str, executable: &Path) -> Result<()> {
    let object = root
        .as_object_mut()
        .context("hook configuration root must be an object")?;
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .context("the hooks setting must be an object")?;
    let starts = hooks.entry("SessionStart").or_insert_with(|| json!([]));
    let starts = starts
        .as_array_mut()
        .context("hooks.SessionStart must be an array")?;

    starts.retain(|group| !contains_automux_hook(group, agent));
    let command = format!(
        "{} register-agent {agent}",
        shell_quote(&executable.display().to_string())
    );
    starts.push(json!({
        "matcher": "startup|resume|clear|fork",
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 5
        }]
    }));
    Ok(())
}

fn contains_automux_hook(group: &Value, agent: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| {
            command.contains("automux")
                && command.contains("register-agent")
                && command.ends_with(agent)
        })
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_other_hooks_and_is_idempotent() {
        let mut value = json!({
            "theme": "dark",
            "hooks": {
                "SessionStart": [{
                    "matcher": "startup",
                    "hooks": [{"type": "command", "command": "other-hook"}]
                }]
            }
        });
        let executable = Path::new("/tmp/automux test/bin");

        merge_hook(&mut value, "claude", executable).unwrap();
        merge_hook(&mut value, "claude", executable).unwrap();

        assert_eq!(value["theme"], "dark");
        let starts = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["hooks"][0]["command"], "other-hook");
        assert_eq!(
            starts[1]["hooks"][0]["command"],
            "'/tmp/automux test/bin' register-agent claude"
        );
    }
}
