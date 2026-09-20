use anyhow::{Context, Result};
use std::{collections::HashMap, env, path::PathBuf, time::Duration};

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
        // Config::load runs on every invocation, including every agent hook
        // and every debounced save, so the options are read in one pass rather
        // than one `show-option` subprocess per setting.
        let options = Options::read();
        let state_dir = options
            .get("@automux-state-dir")
            .map(PathBuf::from)
            .or_else(|| env::var_os("AUTOMUX_STATE_DIR").map(PathBuf::from))
            .unwrap_or_else(default_state_dir);
        Ok(Self {
            state_dir,
            history_limit: options
                .get("@automux-history-limit")
                .unwrap_or_else(|| "-".into()),
            debounce: Duration::from_secs(options.u64("@automux-save-interval", 5)?),
            restore_scrollback: options.bool("@automux-restore-scrollback", true)?,
            resume_nvim: options.bool("@automux-resume-nvim", false)?,
            resume_claude: options.bool("@automux-resume-claude", false)?,
            resume_codex: options.bool("@automux-resume-codex", false)?,
        })
    }
}

struct Options(HashMap<String, String>);

impl Options {
    fn read() -> Self {
        let Ok(output) = std::process::Command::new("tmux")
            .args(["show-options", "-g"])
            .output()
        else {
            return Self(HashMap::new());
        };
        if !output.status.success() {
            return Self(HashMap::new());
        }
        Self(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_once(' '))
                .map(|(name, value)| (name.to_owned(), unquote(value).to_owned()))
                .collect(),
        )
    }

    fn get(&self, name: &str) -> Option<String> {
        self.0
            .get(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn bool(&self, name: &str, default: bool) -> Result<bool> {
        Ok(match self.get(name).as_deref() {
            None => default,
            Some("1" | "on" | "yes" | "true") => true,
            Some("0" | "off" | "no" | "false") => false,
            Some(value) => anyhow::bail!("invalid boolean for {name}: {value}"),
        })
    }

    fn u64(&self, name: &str, default: u64) -> Result<u64> {
        self.get(name)
            .map(|v| {
                v.parse()
                    .with_context(|| format!("invalid number for {name}: {v}"))
            })
            .transpose()
            .map(|v| v.unwrap_or(default))
    }
}

/// `show-options` quotes values that need it; user options are plain strings.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

fn default_state_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("automux");
    }
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".local/state/automux")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, &str)]) -> Options {
        Options(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn values_are_unquoted_and_parsed() {
        assert_eq!(unquote("\"~/state\""), "~/state");
        assert_eq!(unquote("plain"), "plain");

        let options = options(&[
            ("@automux-resume-nvim", "on"),
            ("@automux-save-interval", "12"),
            ("@automux-history-limit", ""),
        ]);
        assert!(options.bool("@automux-resume-nvim", false).unwrap());
        assert!(!options.bool("@automux-resume-codex", false).unwrap());
        assert_eq!(options.u64("@automux-save-interval", 5).unwrap(), 12);
        assert_eq!(options.u64("@automux-missing", 5).unwrap(), 5);
        assert_eq!(options.get("@automux-history-limit"), None);
    }

    #[test]
    fn invalid_values_are_rejected() {
        let options = options(&[
            ("@automux-resume-nvim", "maybe"),
            ("@automux-save-interval", "x"),
        ]);
        assert!(options.bool("@automux-resume-nvim", false).is_err());
        assert!(options.u64("@automux-save-interval", 5).is_err());
    }
}
