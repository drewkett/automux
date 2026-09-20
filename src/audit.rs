use crate::config::Config;
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};

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
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.state_dir.join("automux.log"))?;
    file.write_all(&line)?;
    Ok(())
}

pub fn print(config: &Config, lines: usize) -> Result<()> {
    let data = fs::read_to_string(config.state_dir.join("automux.log"))?;
    let entries = data.lines().collect::<Vec<_>>();
    for entry in entries.iter().skip(entries.len().saturating_sub(lines)) {
        println!("{entry}");
    }
    Ok(())
}
