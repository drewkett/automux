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
        "server": server_identity(),
        "pane": env::var("TMUX_PANE").ok(),
        "detail": detail,
    });
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(config.state_dir.join("automux.log"))?;
    serde_json::to_writer(&mut file, &entry)?;
    writeln!(file)?;
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

fn server_identity() -> Option<String> {
    env::var("TMUX")
        .ok()
        .and_then(|value| value.rsplit_once(',').map(|(server, _)| server.to_owned()))
}
