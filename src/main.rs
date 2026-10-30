mod budget;
mod config;
mod error;
mod history;
mod operations;
mod registry;
mod restic;
mod selection;
mod state;

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "utsurigi-agent-hub",
    version,
    about = "Encrypted Bot storage and fleet coordination"
)]
struct Cli {
    #[arg(long, global = true, default_value = "config.yaml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    Validate {
        #[arg(long)]
        template: bool,
    },
    Setup,
    Sync {
        #[arg(long)]
        scheduled: bool,
    },
    Pull {
        bot_id: String,
        #[arg(long)]
        destination: PathBuf,
    },
    Restore {
        #[arg(long, value_enum)]
        repository: RepositoryKind,
        #[arg(long)]
        snapshot: String,
        #[arg(long)]
        destination: PathBuf,
    },
    Registry {
        #[command(subcommand)]
        command: RegistryAction,
    },
    Prune,
    Status,
}

#[derive(Clone, ValueEnum)]
enum RepositoryKind {
    Shared,
    Bot,
}
#[derive(Subcommand)]
enum RegistryAction {
    Publish {
        #[arg(long)]
        takeover: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(error) = operations::execute(&cli.config, cli.command) {
        eprintln!("{}", error.message);
        std::process::exit(error.code);
    }
}
