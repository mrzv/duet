use color_eyre::eyre;
use env_logger;
use log;

#[macro_use]
extern crate clap;

use colored::*;

use savefile::{save_file,load_file};

mod profile;
mod scan;
mod utils;

fn main() -> Result<(), eyre::Error> {
    color_eyre::install().unwrap();
    env_logger::init();

    let matches = clap_app!(duet =>
        (version: "0.1.0")
        (author: "Dmitriy Morozov <dmitriy@mrzv.org>")
        (about: "bi-directional synchronization")
        (@arg profile: +required "profile to synchronize")
        (@arg path:              "path to synchronize")
        (@arg dry_run: -n        "don't apply changes")
    ).get_matches();

    let profile_name = matches.value_of("profile").unwrap();
    let prf = profile::parse(profile_name).expect(&format!("Failed to read profile {}", profile_name.yellow()));
    println!("Using profile: {}", profile_name.yellow());

    let dry_run = matches.is_present("dry_run");
    let path = matches.value_of("path").unwrap_or("");

    let restricted_current_entries: Vec<_> = scan::scan(&prf.local, &path, &prf.locations).collect();
    let all_old_entries: Vec<scan::DirEntryWithMeta> =
        if std::path::Path::new("save.bin").exists() {
            load_file("save.bin", 0).unwrap()
        } else {
            Vec::new()
        };

    let restricted_old_entries_iter = all_old_entries
                                        .iter()
                                        .filter(|dir: &&scan::DirEntryWithMeta| dir.starts_with(path));

    for c in scan::changes(restricted_old_entries_iter, restricted_current_entries.iter()) {
        log::debug!("{:?}", c);
        println!("{}", c);
    }

    if dry_run {
        return Ok(());
    }

    // TODO: apply changes

    save_file("save.bin", 0, &restricted_current_entries).unwrap();

    Ok(())
}
