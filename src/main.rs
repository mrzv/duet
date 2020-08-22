use color_eyre::eyre;
use env_logger;
//use log;

#[macro_use]
extern crate clap;

use colored::*;

use savefile::{save_file,load_file};

mod profile;
mod scan;
mod utils;

type Entries = Vec<scan::DirEntryWithMeta>;
type Changes = Vec<scan::Change>;

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

    let (local_all_old, local_changes) = old_and_changes(&prf.local, &path, &prf.locations);
    let (_remote_all_old, remote_changes) = old_and_changes(&prf.remote, &path, &prf.locations);

    let mut conflicts: Vec<(&scan::Change, &scan::Change)> = Vec::new();
    for (a,b) in utils::match_sorted(local_changes.iter(), remote_changes.iter()) {
        match (a,b) {
            (Some(a), None) => println!("---> {}", a),
            (None, Some(b)) => println!("<--- {}", b),
            (Some(a), Some(b)) => conflicts.push((a,b)),
            _ => (),
        }
    }
    println!("Conflicts:");
    for (a,_b) in conflicts {
        println!("<--> {}", a);
    }

    if dry_run {
        return Ok(());
    }

    // TODO: apply changes

    save_file("save.bin", 0, &local_all_old).unwrap();

    Ok(())
}

fn old_and_changes(base: &str, restrict: &str, locations: &scan::location::Locations) -> (Entries, Changes) {
    let restricted_current_entries: Entries = scan::scan(base, restrict, locations).collect();
    let all_old_entries: Entries =
        if std::path::Path::new("save.bin").exists() {
            load_file("save.bin", 0).unwrap()
        } else {
            Vec::new()
        };

    let restricted_old_entries_iter = all_old_entries
                                          .iter()
                                          .filter(move |dir: &&scan::DirEntryWithMeta| dir.starts_with(restrict));

    let changes: Vec<_> = scan::changes(restricted_old_entries_iter, restricted_current_entries.iter()).collect();

    (all_old_entries, changes)
}
