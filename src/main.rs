use color_eyre::eyre::Result;

mod actions;
mod cli;
mod commands;
mod io_wrappers;
mod orchestrator;
mod profile;
mod remote;
mod resolution;
mod rpc;
mod rustsync;
mod scan;
mod state;
mod sync;
mod utils;
#[macro_use]
extern crate serde_derive;

use cli::Command;
use std::path::PathBuf;

#[tokio::main]
#[quit::main]
pub async fn main() -> Result<()> {
    color_eyre::install().unwrap();

    match cli::parse_from_env()? {
        Command::Help => commands::show_help(),
        Command::Version => commands::version(),
        Command::License => commands::license(),
        Command::Server => return rpc::server().await,
        Command::Snapshot { profile, statefile } => {
            return commands::snapshot(profile, statefile).await;
        }
        Command::Inspect { statefile } => return commands::inspect(statefile),
        Command::Changes { profile, statefile } => {
            return commands::changes(profile, statefile).await;
        }
        Command::Info { profile } => return commands::info(profile),
        Command::Walk { path } => return commands::walk(path).await,
        Command::Sync {
            profile,
            path,
            options,
        } => return orchestrator::sync(profile, path, options).await,
    }
    Ok(())
}

pub(crate) fn full(s: &String) -> Result<PathBuf> {
    Ok(PathBuf::from(shellexpand::full(s)?.into_owned()))
}
