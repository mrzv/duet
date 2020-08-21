use color_eyre::eyre;
use env_logger;

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
    let prf = profile::parse(profile_name);
    if let Ok(_) = prf {
        println!("Using profile: {}", profile_name.yellow());
    } else if let Err(why) = prf {
        eprintln!("Failed to read {}:\n{}", profile_name.yellow(), why.to_string().magenta());
        std::process::exit(1);
    }
    let prf = prf.unwrap();

    let dry_run = matches.is_present("dry_run");

    let path = matches.value_of("path");


    let current_entries: Vec<_> = scan::scan(&prf.local, &path, &prf.locations).collect();
    let old_entries: Vec<scan::DirEntryWithMeta> = load_file("save.bin", 0).unwrap();

    scan::changes(old_entries.into_iter(), current_entries.into_iter());

    //save_file("save.bin", 0, &current_entries).unwrap();

    if dry_run {
        return Ok(());
    }

    Ok(())
}
