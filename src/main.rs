mod budget;
mod config;
mod decisions;
mod error;
mod head;
mod history;
mod migration;
mod operations;
mod registry;
mod restic;
mod selection;
mod shared;
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
    // Restic invokes this child while holding its normal append lock. It must
    // never enter dotenv startup or wait for the parent's Bot operation lock.
    if let Some(result) = migration::helper_command() {
        if let Err(error) = result {
            eprintln!("{}", error.message);
            std::process::exit(error.code);
        }
        return;
    }
    let cli = Cli::parse();
    let result = std::fs::canonicalize(&cli.config)
        .map_err(|_| error::Error::new("Cannot read configuration"))
        .and_then(|path| {
            let mut variables = config::dotenv(&path)?;
            // Check aliases before applying file values, against the original process.
            for uppercase in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"] {
                let lowercase = uppercase.to_ascii_lowercase();
                if std::env::var_os(uppercase).is_some() || std::env::var_os(&lowercase).is_some() {
                    variables.remove(uppercase);
                    variables.remove(&lowercase);
                }
            }
            for (name, value) in variables {
                if std::env::var_os(&name).is_none() {
                    // SAFETY: Only synchronous CLI/file parsing has run; no threads have
                    // been started. All environment mutation ends before execute begins.
                    unsafe { std::env::set_var(name, value) };
                }
            }
            operations::execute(&path, cli.command)
        });
    if let Err(error) = result {
        eprintln!("{}", error.message);
        std::process::exit(error.code);
    }
}
