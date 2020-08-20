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


    //let count = scan::scan(&prf.local, &path, &prf.locations).count();
    //println!("Count: {}", count);

    //let entries: Vec<_> = scan::scan(&prf.local, &path, &prf.locations).collect();
    //let count = entries.len();
    //save_file("save.bin", 0, &entries).unwrap();
    //println!("Count: {}", count);

    let mut count = 0;
    for entry in scan::scan(&prf.local, &path, &prf.locations) {
        println!("{:?}", entry);
        count += 1;
    }
    println!("Count: {}", count);

    //let entries: Vec<DirEntryWithMeta> = load_file("save.bin", 0).unwrap();
    //let count = entries.len();
    //println!("Count: {}", count);

    if dry_run {
        return Ok(());
    }

    Ok(())
}
