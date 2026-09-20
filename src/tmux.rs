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

/// Identity of the tmux server this process belongs to, as
/// `<socket path>,<server pid>`.
///
/// Hooks run inside a pane and can read it from `$TMUX`, but the plugin also
/// runs while tmux is sourcing its configuration, before any client exists and
/// therefore before `$TMUX` is set. Fall back to asking the server directly;
/// `#{socket_path},#{pid}` is the same pair `$TMUX` carries.
pub fn server_identity() -> Option<String> {
    if let Some(value) = std::env::var("TMUX")
        .ok()
        .and_then(|value| value.rsplit_once(',').map(|(server, _)| server.to_owned()))
    {
        return Some(value);
    }
    output(&["display-message", "-p", "#{socket_path},#{pid}"])
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && value.contains(','))
}

/// Whether the tmux server that wrote a state directory is still alive.
pub fn server_alive(identity: &str) -> bool {
    let Some((_, pid)) = identity.rsplit_once(',') else {
        return false;
    };
    Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(true)
}

/// Stable, filesystem-safe key for a server identity (FNV-1a).
pub fn server_key(identity: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in identity.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_keys_are_stable_and_distinct() {
        let first = server_key("/tmp/tmux-502/default,14663");
        assert_eq!(first, server_key("/tmp/tmux-502/default,14663"));
        assert_ne!(first, server_key("/tmp/tmux-502/default,14664"));
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_dead_server_is_not_alive() {
        // PID 2^22 is above the macOS and Linux defaults for pid_max.
        assert!(!server_alive("/tmp/tmux-502/default,4194304"));
        assert!(!server_alive("no-comma"));
    }
}
