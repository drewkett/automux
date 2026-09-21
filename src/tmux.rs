use anyhow::{bail, Context, Result};
use std::process::Command;

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

/// Separates fields in tmux format output; it cannot appear in names.
pub const SEP: &str = "\u{1f}";

/// Run a tmux list command, returning one row per line holding the requested
/// format `fields` in order. Rows with the wrong field count are dropped.
pub fn table(args: &[&str], fields: &[&str]) -> Result<Vec<Vec<String>>> {
    let format = fields
        .iter()
        .map(|field| format!("#{{{field}}}"))
        .collect::<Vec<_>>()
        .join(SEP);
    Ok(output(&[args, &["-F", &format]].concat())?
        .lines()
        .map(|line| line.split(SEP).map(str::to_owned).collect::<Vec<_>>())
        .filter(|row| row.len() == fields.len())
        .collect())
}

pub fn has_session(name: &str) -> bool {
    output(&["has-session", "-t", name]).is_ok()
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

/// Stable, filesystem-safe key (FNV-1a) for the socket a server listens on.
///
/// It deliberately ignores the pid, so state keyed by it survives the server
/// restart it exists to recover from, while a second server on another `-L`
/// socket still gets its own.
pub fn socket_key(identity: &str) -> String {
    let socket = identity
        .rsplit_once(',')
        .map_or(identity, |(socket, _)| socket);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in socket.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_keys_are_stable_and_ignore_the_server_pid() {
        let first = socket_key("/tmp/tmux-502/default,14663");
        assert_eq!(first, socket_key("/tmp/tmux-502/default,90001"));
        assert_ne!(first, socket_key("/tmp/tmux-502/other,14663"));
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
