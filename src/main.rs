use color_eyre::eyre;
use std::path::{PathBuf};

#[macro_use]
extern crate clap;

use colored::*;

use env_logger;

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

    let path = matches.value_of("path").map(|x| PathBuf::from(x));

    scan::scan(PathBuf::from(&prf.local), &path, &prf.locations)?;

    if dry_run {
        return Ok(());
    }

    Ok(())
}
