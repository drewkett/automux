use crate::config::Config;
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Rotate once the log passes this size, keeping one previous generation.
const MAX_LOG_BYTES: u64 = 1 << 20;

/// Append an event to `automux.log` as a JSON line. Logging never fails the
/// operation being logged.
pub fn record(config: &Config, event: &str, detail: Value) {
    let _ = try_record(config, event, detail);
}

fn try_record(config: &Config, event: &str, detail: Value) -> Result<()> {
    fs::create_dir_all(&config.state_dir)?;
    let entry = json!({
        "time": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        "event": event,
        "server": crate::tmux::server_identity(),
        "pane": env::var("TMUX_PANE").ok(),
        "detail": detail,
    });
    // Several hooks log concurrently. Build the whole line first so the append
    // is a single write and cannot interleave with another process's entry.
    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');
    let path = config.state_dir.join("automux.log");
    rotate(&path);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(&line)?;
    Ok(())
}

/// Keep the log bounded: past `MAX_LOG_BYTES` it becomes `automux.log.1`,
/// replacing any previous generation, and a fresh log starts.
fn rotate(path: &Path) {
    let too_large = path
        .metadata()
        .map(|meta| meta.len() >= MAX_LOG_BYTES)
        .unwrap_or(false);
    if too_large {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_moves_an_oversized_log_aside() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("automux.log");
        fs::write(&path, vec![b'\n'; MAX_LOG_BYTES as usize]).unwrap();
        rotate(&path);
        assert!(!path.exists());
        assert!(path.with_extension("log.1").exists());
    }
}
