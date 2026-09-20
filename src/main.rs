mod audit;
mod config;
mod integrations;
mod layout;
mod process;
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
    /// Save the workspace and stop the tmux server.
    Shutdown,
    /// Print a human-readable summary of the saved snapshot.
    Status,
    /// Print the resolved state directory (useful to plugin scripts).
    StateDir,
    /// Print this pane's server-scoped Neovim state directory.
    #[command(hide = true)]
    PaneDir,
    /// Finish a startup restore once a client attaches.
    #[command(hide = true)]
    Attach,
    /// Install global integrations for detected Neovim, Claude Code, and Codex.
    InstallHooks,
    /// Print recent structured Automux log entries.
    Logs {
        /// Maximum number of entries to print.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Record an internal tmux lifecycle event.
    #[command(hide = true)]
    Event { event: String },
    /// Record the exact agent session associated with the current tmux pane.
    #[command(hide = true)]
    RegisterAgent {
        /// Agent emitting the SessionStart hook: claude or codex.
        agent: String,
    },
    /// Remove an agent registration when its session ends.
    #[command(hide = true)]
    UnregisterAgent {
        /// Agent emitting the SessionEnd hook: claude or codex.
        agent: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::Config::load()?;
    match cli.command {
        Command::Save { quiet, force } => snapshot::save(&config, quiet, force),
        Command::Restore { replace_empty } => snapshot::restore(&config, replace_empty),
        Command::Shutdown => {
            audit::record(&config, "shutdown_requested", serde_json::json!({}));
            snapshot::save(&config, true, true)?;
            audit::record(&config, "shutdown_saved", serde_json::json!({}));
            tmux::run(&["kill-server"])
        }
        Command::Status => snapshot::status(&config),
        Command::StateDir => {
            println!("{}", config.state_dir.display());
            Ok(())
        }
        Command::PaneDir => integrations::print_pane_dir(&config),
        Command::Attach => snapshot::attach(&config),
        Command::InstallHooks => integrations::install_hooks(),
        Command::Logs { lines } => audit::print(&config, lines),
        Command::Event { event } => {
            if !matches!(event.as_str(), "plugin-loaded" | "client-attached") {
                anyhow::bail!("unsupported internal event {event:?}");
            }
            audit::record(&config, &event, serde_json::json!({}));
            Ok(())
        }
        Command::RegisterAgent { agent } => {
            integrations::register_agent(&config, &agent, io::stdin())
        }
        Command::UnregisterAgent { agent } => {
            integrations::unregister_agent(&config, &agent, io::stdin())
        }
    }
}
