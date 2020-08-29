use color_eyre::eyre;
use env_logger;
//use log;

#[macro_use]
extern crate clap;

use colored::*;

use shellexpand;

use savefile::{save_file,load_file};

mod profile;
mod scan;
mod utils;
mod actions;
use actions::Action;

type Entries = Vec<scan::DirEntryWithMeta>;
type Changes = Vec<scan::Change>;
type Actions = Vec<actions::Action>;

fn main() -> Result<(), eyre::Error> {
    color_eyre::install().unwrap();
    env_logger::init();

    let matches = clap_app!(duet =>
        (version: crate_version!())
        (author: crate_authors!())
        (about: "bi-directional synchronization")
        (@subcommand sync =>
            (about: "synchronize according to profile")
            (@arg profile: +required "profile to synchronize")
            (@arg path:              "path to synchronize")
            (@arg dry_run: -n        "don't apply changes")
        )
        (@subcommand snapshot =>
            (about: "take snapshot")
            (@arg profile: +required "profile to snapshot")
            (@arg state:   +required "state file to save snapshot")
        )
        (@subcommand inspect =>
            (about: "inspect a state file")
            (@arg state: +required "statefile to show")
        )
    ).get_matches();

    // inspect subcommand
    if let Some(matches) = matches.subcommand_matches("inspect") {
        let statefile = matches.value_of("state").unwrap();
        return inspect(statefile);
    } else if let Some(matches) = matches.subcommand_matches("snapshot") {
        let profile = matches.value_of("profile").unwrap();
        let statefile = matches.value_of("state").unwrap();
        return snapshot(profile, statefile);
    } else if let Some(matches) = matches.subcommand_matches("sync") {
        let profile = matches.value_of("profile").unwrap();
        let dry_run = matches.is_present("dry_run");
        let path = matches.value_of("path").unwrap_or("");

        return sync(profile, path, dry_run);
    }

    Ok(())
}

fn inspect(statefile: &str) -> Result<(), eyre::Error> {
    let entries: Entries = old_entries(statefile);
    for e in entries {
        println!("{:?}", e);
    }
    Ok(())
}

fn old_entries(fname: &str) -> Entries {
    let entries: Entries =
        if std::path::Path::new(fname).exists() {
            log::debug!("Loading: {}", fname);
            load_file(fname, 0).unwrap()
        } else {
            Vec::new()
        };
    entries
}

fn snapshot(name: &str, statefile: &str) -> Result<(), eyre::Error> {
    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.yellow());

    let local_base = shellexpand::full(&prf.local).ok().unwrap().to_string();
    let current_entries: Entries = scan::scan(&local_base, "", &prf.locations).collect();
    save_file(statefile, 0, &current_entries).unwrap();
    Ok(())
}

fn sync(name: &str, path: &str, dry_run: bool) -> Result<(), eyre::Error> {
    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.yellow());

    let local_base = shellexpand::full(&prf.local).ok().unwrap().to_string();
    let remote_base = shellexpand::full(&prf.remote).ok().unwrap().to_string();

    let (local_all_old, local_changes) = old_and_changes(&local_base, &path, &prf.locations, Some(profile::local_state(name).to_str().unwrap()));
    let (_remote_all_old, remote_changes) = old_and_changes(&remote_base, &path, &prf.locations, None);

    let actions: Actions = utils::match_sorted(local_changes.iter(), remote_changes.iter())
                                .filter_map(|(lc,rc)| Action::create(lc,rc))
                                .collect();
    for a in &actions {
        println!("{}", a);
    }

    if dry_run {
        return Ok(());
    }

    // apply changes
    for a in &actions {
        a.apply();
    }

    save_file("save.bin", 0, &local_all_old).unwrap();
    Ok(())
}

fn old_and_changes(base: &str, restrict: &str, locations: &scan::location::Locations, statefile: Option<&str>) -> (Entries, Changes) {
    let restricted_current_entries: Entries = scan::scan(base, restrict, locations).collect();
    let all_old_entries: Entries =
        if let Some(f) = statefile {
            if std::path::Path::new(f).exists() {
                log::debug!("Loading: {}", f);
                load_file(f, 0).unwrap()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

    let restricted_old_entries_iter = all_old_entries
                                          .iter()
                                          .filter(move |dir: &&scan::DirEntryWithMeta| dir.starts_with(restrict));

    let changes: Vec<_> = scan::changes(restricted_old_entries_iter, restricted_current_entries.iter()).collect();

    (all_old_entries, changes)
}
