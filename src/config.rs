use anyhow::{Context, Result};
use std::{env, path::PathBuf, time::Duration};

#[derive(Debug, Clone)]
pub struct Config {
    pub state_dir: PathBuf,
    pub history_limit: String,
    pub debounce: Duration,
    pub restore_scrollback: bool,
    pub resume_nvim: bool,
    pub resume_claude: bool,
    pub resume_codex: bool,
}

impl Config {
    pub fn load() -> Result<Self> {
        let state_dir = option("@automux-state-dir")?
            .map(PathBuf::from)
            .or_else(|| env::var_os("AUTOMUX_STATE_DIR").map(PathBuf::from))
            .unwrap_or_else(default_state_dir);
        Ok(Self {
            state_dir,
            history_limit: option("@automux-history-limit")?.unwrap_or_else(|| "-".into()),
            debounce: Duration::from_secs(parse_u64("@automux-save-interval", 5)?),
            restore_scrollback: parse_bool("@automux-restore-scrollback", true)?,
            resume_nvim: parse_bool("@automux-resume-nvim", false)?,
            resume_claude: parse_bool("@automux-resume-claude", false)?,
            resume_codex: parse_bool("@automux-resume-codex", false)?,
        })
    }
}

fn default_state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("automux");
    }
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local/state/automux")
}

fn option(name: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("tmux")
        .args(["show-option", "-gqv", name])
        .output()
        .with_context(|| "could not run tmux")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

fn parse_bool(name: &str, default: bool) -> Result<bool> {
    Ok(match option(name)?.as_deref() {
        None => default,
        Some("1" | "on" | "yes" | "true") => true,
        Some("0" | "off" | "no" | "false") => false,
        Some(value) => anyhow::bail!("invalid boolean for {name}: {value}"),
    })
}

fn parse_u64(name: &str, default: u64) -> Result<u64> {
    option(name)?
        .map(|v| {
            v.parse()
                .with_context(|| format!("invalid number for {name}: {v}"))
        })
        .transpose()
        .map(|v| v.unwrap_or(default))
}
