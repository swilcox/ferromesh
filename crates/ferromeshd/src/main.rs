//! ferromeshd: records MeshCore traffic from MQTT into a raw log and database.

mod config;
mod import;
mod meshcoretomqtt;
mod pipeline;
mod rawlog;
mod rebuild;
mod serve;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Configuration file.
    #[arg(long, short, global = true, env = "FERROMESH_CONFIG", default_value = "ferromesh.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Subscribe to MQTT and record everything that arrives.
    Serve,
    /// Load a JSONL capture written by the Python mqtt_observer watcher.
    Import {
        file: PathBuf,
        /// Names the raw log file and marks where the records came from.
        #[arg(long, default_value = "mqtt_observer")]
        label: String,
    },
    /// Recreate the database from the raw log and compare it with the current one.
    Rebuild {
        /// Swap the rebuilt database in, keeping a backup. Stop `serve` first.
        #[arg(long)]
        replace: bool,
    },
    /// Print row counts.
    Stats,
}

fn main() -> Result<()> {
    use std::io::IsTerminal;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        // Plain text when piped or under Docker, where colour codes are noise.
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    match cli.command {
        Command::Serve => tokio::runtime::Runtime::new()?.block_on(serve::run(config)),
        Command::Import { file, label } => import::run(&config, &file, &label),
        Command::Rebuild { replace } => rebuild::run(&config, replace),
        Command::Stats => {
            let store = pipeline::open_store(&config)?;
            println!("{}", pipeline::format_counts(&store.counts()?));
            Ok(())
        }
    }
}
