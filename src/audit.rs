use crate::config::Config;
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    env, fs,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Lifecycle events `automux.tmux` reports through `automux event`. Keep this
/// in step with the hooks the plugin installs.
pub const EVENTS: [&str; 2] = ["plugin-loaded", "client-attached"];

/// Rotate once the log passes this size, keeping one previous generation.
const MAX_LOG_BYTES: u64 = 1 << 20;

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
    let path = log_path(config);
    rotate(&path);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(&line)?;
    Ok(())
}

fn log_path(config: &Config) -> PathBuf {
    config.state_dir.join("automux.log")
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

pub fn print(config: &Config, lines: usize) -> Result<()> {
    for entry in tail(&log_path(config), lines)? {
        println!("{entry}");
    }
    Ok(())
}

/// The last `lines` lines of a file, without reading all of it.
///
/// Reads backwards in chunks until enough newlines have been seen, so a log
/// that has grown to the rotation threshold still costs a few kilobytes.
fn tail(path: &Path, lines: usize) -> Result<Vec<String>> {
    const CHUNK: u64 = 8 * 1024;

    let mut file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut start = size;
    let mut data = Vec::new();
    while start > 0 {
        let step = CHUNK.min(start);
        start -= step;
        let mut chunk = vec![0; step as usize];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        chunk.append(&mut data);
        data = chunk;
        // One extra newline so a chunk boundary cannot split the oldest line
        // we are about to return.
        if data.iter().filter(|byte| **byte == b'\n').count() > lines {
            break;
        }
    }
    let text = String::from_utf8_lossy(&data);
    let entries = text.lines().collect::<Vec<_>>();
    Ok(entries
        .iter()
        .skip(entries.len().saturating_sub(lines))
        .map(|entry| (*entry).to_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_the_last_lines_across_chunk_boundaries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("automux.log");
        let body: String = (0..5000).map(|n| format!("line {n}\n")).collect();
        fs::write(&path, &body).unwrap();

        assert_eq!(
            tail(&path, 3).unwrap(),
            ["line 4997", "line 4998", "line 4999"]
        );
        assert_eq!(tail(&path, 10_000).unwrap().len(), 5000);
    }

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
