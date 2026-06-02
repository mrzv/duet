use std::path::PathBuf;

use color_eyre::eyre::Result;
use colored::*;
use tokio::sync::mpsc;

use crate::{full, profile, scan, state};

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

pub(crate) fn show_help() {
    print!(
        "\
duet {}
bi-directional synchronization

USAGE:
    duet [FLAGS] <profile> [path]

FLAGS:
    -i, --interactive   interactive conflict resolution
    -y, --yes           assume yes (i.e., synchronize, if there are no conflicts)
    -b, --batch         run as a batch (abort on conflict)
    -f, --force         in batch mode, apply what's possible, even if there are conflicts
    -v, --verbose       verbose output
    -n, --dry-run       don't apply changes

        --version       prints version information
        --license       prints license information (including dependencies)
    -h, --help          prints help information

ARGS:
    <profile>    profile to synchronize
    <path>       path to synchronize
",
        built_info::PKG_VERSION
    );
}

pub(crate) fn version() {
    println!("duet {}", built_info::PKG_VERSION);
    for (name, version) in built_info::DEPENDENCIES {
        println!("  {} {}", name, version);
    }
}

pub(crate) fn license() {
    println!("{}\n", include_str!("../LICENSE"));
    println!("{}", include_str!("../licenses/deps.txt"));
    println!("{}", include_str!("../licenses/included.txt"));
}

pub(crate) fn inspect(statefile: PathBuf) -> Result<()> {
    let entries = state::load_entries(&statefile)?;
    for e in entries {
        println!("{:?}", e);
    }
    Ok(())
}

pub(crate) async fn snapshot(name: String, statefile: Option<PathBuf>) -> Result<()> {
    let prf = profile::parse(&name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.cyan());

    let local_base = full(&prf.local)?;

    let current_entries =
        state::scan_entries(&local_base, &PathBuf::from(""), &prf.locations, &prf.ignore).await?;

    let statefile = statefile.unwrap_or(profile::local_state(&name));
    state::save_entries(&statefile, &current_entries)?;
    Ok(())
}

pub(crate) async fn changes(name: String, statefile: Option<PathBuf>) -> Result<()> {
    let prf = profile::parse(&name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.cyan());

    let local_base = full(&prf.local)?;

    let statefile = statefile.unwrap_or(profile::local_state(&name));

    let (_, changes) = state::old_and_changes(
        &local_base,
        &PathBuf::from(""),
        &prf.locations,
        &prf.ignore,
        Some(&statefile),
    )
    .await?;

    for c in changes {
        println!("{} {}", c, c.path().display());
    }

    Ok(())
}

pub(crate) fn info(name: String) -> Result<()> {
    println!(
        "Profile {} located at {}",
        name.cyan(),
        profile::location(&name).display().to_string().yellow()
    );
    Ok(())
}

pub(crate) async fn walk(path: PathBuf) -> Result<()> {
    let locations = vec![scan::location::Location::Include(PathBuf::from("."))];

    let (tx, mut rx) = mpsc::channel(1024);
    tokio::spawn(async move { scan::scan(path, "", &locations, &Vec::new(), tx).await });

    while let Some(e) = rx.recv().await {
        println!("{}", e.path().display());
    }
    Ok(())
}
