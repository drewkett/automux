use anyhow::{bail, Context, Result};
use std::{path::Path, process::Command};

/// The tmux commands a restore issues.
///
/// Restore is the part of automux that is hardest to exercise for real — it
/// depends on server options such as `base-index` and on the exact order
/// windows and panes are created in. Going through this trait lets those
/// rules be tested against a recorded command log instead of a live server.
pub trait Server {
    fn output(&self, args: &[&str]) -> Result<String>;

    fn run(&self, args: &[&str]) -> Result<()> {
        self.output(args).map(|_| ())
    }
}

/// The real tmux server, driven by the `tmux` binary.
pub struct Cli;

impl Server for Cli {
    fn output(&self, args: &[&str]) -> Result<String> {
        output(args)
    }
}

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
///
/// The socket is the stronger signal: tmux unlinks it when the server exits,
/// and unlike the pid it cannot be recycled by an unrelated process. The pid
/// check only narrows a live socket down further, and is treated as
/// inconclusive when it cannot answer — `kill -0` fails with `EPERM` for a
/// process owned by another user, which must not read as "dead".
pub fn server_alive(identity: &str) -> bool {
    let Some((socket, pid)) = identity.rsplit_once(',') else {
        return false;
    };
    if !Path::new(socket).exists() {
        return false;
    }
    // `has-session` against the socket proves a server is accepting
    // connections there; anything less definite keeps the state.
    Command::new("tmux")
        .args(["-S", socket, "has-session"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(true)
        || process_exists(pid)
}

fn process_exists(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(true)
}

/// Stable, filesystem-safe key for the socket a server listens on.
///
/// Unlike [`server_key`], this deliberately ignores the pid, so it survives a
/// restart of the server on the same socket. That is what state which has to
/// outlive the server — the snapshot itself — is keyed by, while a second
/// server on another `-L` socket still gets its own.
pub fn socket_key(identity: &str) -> String {
    server_key(
        identity
            .rsplit_once(',')
            .map_or(identity, |(socket, _)| socket),
    )
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
    fn socket_keys_ignore_the_server_pid() {
        let first = socket_key("/tmp/tmux-502/default,14663");
        assert_eq!(first, socket_key("/tmp/tmux-502/default,90001"));
        assert_ne!(first, socket_key("/tmp/tmux-502/other,14663"));
    }

    #[test]
    fn a_dead_server_is_not_alive() {
        // A socket path that cannot exist means the server is gone, whatever
        // the pid says.
        assert!(!server_alive("/nonexistent/automux-test/socket,4194304"));
        assert!(!server_alive("no-comma"));
    }
}
