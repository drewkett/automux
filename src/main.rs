mod config;
mod integrations;
mod layout;
mod snapshot;
mod tmux;

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
        /// Also remove the empty session tmux created while loading the plugin.
        #[arg(long)]
        replace_empty: bool,
    },
    /// Print a human-readable summary of the saved snapshot.
    Status,
    /// Print the resolved state directory (useful to plugin scripts).
    StateDir,
    /// Install global integrations for detected Neovim, Claude Code, and Codex.
    InstallHooks,
    /// Record the exact agent session associated with the current tmux pane.
    #[command(hide = true)]
    RegisterAgent {
        /// Agent emitting the SessionStart hook: claude or codex.
        agent: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::Config::load()?;
    match cli.command {
        Command::Save { quiet, force } => snapshot::save(&config, quiet, force),
        Command::Restore { replace_empty } => snapshot::restore(&config, replace_empty),
        Command::Status => snapshot::status(&config),
        Command::StateDir => {
            println!("{}", config.state_dir.display());
            Ok(())
        }
        Command::InstallHooks => integrations::install_hooks(),
        Command::RegisterAgent { agent } => {
            integrations::register_agent(&config, &agent, io::stdin())
        }
    }
}
