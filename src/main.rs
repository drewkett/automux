mod audit;
mod config;
mod integrations;
mod layout;
mod snapshot;
mod tmux;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save all tmux sessions, windows, panes, directories, and scrollback.
    Save {
        /// Do not print the snapshot path.
        #[arg(long)]
        quiet: bool,
        /// Ignore the configured debounce interval.
        #[arg(long)]
        force: bool,
    },
    /// Restore the most recent snapshot into the current tmux server.
    Restore {
        /// Set by the plugin at server start: once a client attaches, move it
        /// to the restored session and drop the empty one tmux made for it.
        #[arg(long)]
        startup: bool,
    },
    /// Save the workspace and stop the tmux server.
    Shutdown,
    /// Print a human-readable summary of the saved snapshot.
    Status,
    /// Print the resolved state directory.
    StateDir,
    /// Print (and create) where Neovim keeps session files for this server.
    #[command(hide = true)]
    NvimDir,
    /// Finish a startup restore once a client attaches.
    #[command(hide = true)]
    Attach,
    /// Install global integrations for detected Neovim, Claude Code, and Codex.
    InstallHooks,
    /// Label the current pane with an agent session (SessionStart hook).
    #[command(hide = true)]
    RegisterAgent {
        /// Agent emitting the hook: claude or codex.
        agent: String,
    },
    /// Clear the current pane's agent label (SessionEnd hook).
    #[command(hide = true)]
    UnregisterAgent {
        /// Agent emitting the hook: claude or codex.
        agent: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::Config::load()?;
    match cli.command {
        Command::Save { quiet, force } => snapshot::save(&config, quiet, force),
        Command::Restore { startup } => snapshot::restore(&config, startup),
        Command::Shutdown => {
            snapshot::save(&config, true, true)?;
            audit::record(&config, "shutdown", serde_json::json!({}));
            tmux::run(&["kill-server"])
        }
        Command::Status => snapshot::status(&config),
        Command::StateDir => {
            println!("{}", config.state_dir.display());
            Ok(())
        }
        Command::NvimDir => integrations::print_nvim_dir(&config),
        Command::Attach => snapshot::attach(&config),
        Command::InstallHooks => integrations::install_hooks(),
        Command::RegisterAgent { agent } => integrations::register_agent(&agent, io::stdin()),
        Command::UnregisterAgent { agent } => integrations::unregister_agent(&agent, io::stdin()),
    }
}
