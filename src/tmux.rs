use anyhow::{bail, Context, Result};
use std::process::Command;

pub fn output(args: &[&str]) -> Result<String> {
    let result = Command::new("tmux")
        .args(args)
        .output()
        .with_context(|| format!("failed to run tmux {}", args.join(" ")))?;
    if !result.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&result.stdout).into_owned())
}

pub fn run(args: &[&str]) -> Result<()> {
    output(args).map(|_| ())
}

pub fn lines(args: &[&str]) -> Result<Vec<Vec<String>>> {
    Ok(output(args)?
        .lines()
        .map(|line| line.split('\u{1f}').map(str::to_owned).collect())
        .collect())
}

pub fn has_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
