use color_eyre::eyre::Result;
use clap::{clap_app,crate_version,crate_authors};
use colored::*;

mod profile;
mod scan;
mod utils;
mod actions;
use actions::Action;
use scan::location::{Locations};

type Entries = Vec<scan::DirEntryWithMeta>;
type Changes = Vec<scan::Change>;
type Actions = Vec<Action>;

fn main() -> Result<()> {
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
            (@arg state:             "state file to save snapshot")
        )
        (@subcommand inspect =>
            (about: "inspect a state file")
            (@arg state: +required "statefile to show")
        )
        (@subcommand changes =>
            (about: "show changes compared to a given state")
            (@arg profile: +required "profile to compare")
            (@arg state:             "state file to compare")
        )
        (@subcommand info =>
            (about: "show info about a profile")
            (@arg profile: +required "profile to compare")
        )
        (@subcommand server =>
            (about: "run server-side code")
        )
    ).get_matches();

    if let Some(matches) = matches.subcommand_matches("sync") {
        let profile = matches.value_of("profile").unwrap();
        let dry_run = matches.is_present("dry_run");
        let path = matches.value_of("path").unwrap_or("");

        return sync(profile, path, dry_run);
    } else if let Some(matches) = matches.subcommand_matches("snapshot") {
        let profile = matches.value_of("profile").unwrap();
        let statefile = matches.value_of("state");
        return snapshot(profile, statefile);
    } else if let Some(matches) = matches.subcommand_matches("inspect") {
        let statefile = matches.value_of("state").unwrap();
        return inspect(statefile);
    } else if let Some(matches) = matches.subcommand_matches("changes") {
        let profile = matches.value_of("profile").unwrap();
        let statefile = matches.value_of("state");
        return changes(profile, statefile);
    } else if let Some(matches) = matches.subcommand_matches("info") {
        let profile = matches.value_of("profile").unwrap();
        return info(profile);
    } else if let Some(_matches) = matches.subcommand_matches("server") {
        return server();
    }

    Ok(())
}

fn inspect(statefile: &str) -> Result<()> {
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
            savefile::load_file(fname, 0).unwrap()
        } else {
            Vec::new()
        };
    entries
}

fn snapshot(name: &str, statefile: Option<&str>) -> Result<()> {
    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.yellow());

    let local_base = shellexpand::full(&prf.local)?.to_string();
    let current_entries: Entries = scan::scan(&local_base, "", &prf.locations).collect();

    let statefile = if let Some(s) = statefile {
        String::from(s)
    } else {
        String::from(profile::local_state(name).to_str().unwrap())
    };
    savefile::save_file(&statefile, 0, &current_entries).unwrap();
    Ok(())
}

fn changes(name: &str, statefile: Option<&str>) -> Result<()> {
    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.yellow());

    let local_base = shellexpand::full(&prf.local)?.to_string();

    let statefile = if let Some(s) = statefile {
        String::from(s)
    } else {
        String::from(profile::local_state(name).to_str().unwrap())
    };
    let (_, changes) = old_and_changes(&local_base, "", &prf.locations, Some(&statefile));

    for c in changes {
        println!("{}", c);
    }

    Ok(())
}

fn info(name: &str) -> Result<()> {
    println!("Profile {} located at {}", name.yellow(), profile::location(name).to_str().unwrap());
    Ok(())
}

fn sync(name: &str, path: &str, dry_run: bool) -> Result<()> {
    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.yellow());

    let local_id = local_id(name);

    let local_base = shellexpand::full(&prf.local)?.to_string();
    let remote_base = shellexpand::full(&prf.remote)?.to_string();

    let local_state = profile::local_state(name).to_str().unwrap();
    let (local_all_old, local_changes) = old_and_changes(&local_base, &path, &prf.locations, Some(local_state));
    let remote_changes = get_remote_changes(&remote_base, &path, &prf.locations, &local_id).expect("Couldn't get remote changes");

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

    savefile::save_file(local_state, 0, &local_all_old).unwrap();
    Ok(())
}

fn local_id(name: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mid: String = machine_uid::get().unwrap();
    let mut s = DefaultHasher::new();
    mid.hash(&mut s);
    name.hash(&mut s);
    format!("{:x}", s.finish())
}

fn get_remote_changes(base: &str, path: &str, locations: &Locations, local_id: &str) -> Result<Changes> {
    use std::process::{Command, Stdio};
    use savefile::{save,load};

    // launch server
    let mut server = Command::new("target/debug/duet")       // TODO: need a better way to find the command
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to spawn child process");

    let server_in = server.stdin.as_mut().expect("Failed to open stdin");
    let server_out = server.stdout.as_mut().expect("Failed to read stdout");

    save(server_in, 0, &String::from(base)).expect("Can't send base to the server");
    save(server_in, 0, &String::from(path)).expect("Can't send path to the server");
    save(server_in, 0, locations).expect("Can't send locations to the server");
    save(server_in, 0, &String::from(local_id)).expect("Can't send local_id to the server");

    let changes: Changes = load(server_out, 0).expect("Failed to load changes");

    Ok(changes)
}

fn server() -> Result<()> {
    use std::io::{self};
    use savefile::{save,load};

    let stdin = &mut io::stdin();

    let base: String = load(stdin, 0).expect("Failed to load base");
    let path: String = load(stdin, 0).expect("Failed to load path");
    let locations: Locations = load(stdin, 0).expect("Failed to load locations");
    let remote_id: String = load(stdin, 0).expect("Failed to load path");

    let (_all_old, changes) = old_and_changes(&base, &path, &locations, Some(profile::remote_state(&remote_id).to_str().unwrap()));

    let stdout = &mut io::stdout();
    save(stdout, 0, &changes).expect("Can't send changes to the client");

    Ok(())
}

fn old_and_changes(base: &str, restrict: &str, locations: &Locations, statefile: Option<&str>) -> (Entries, Changes) {
    let restricted_current_entries: Entries = scan::scan(base, restrict, locations).collect();
    let all_old_entries: Entries =
        if let Some(f) = statefile {
            if std::path::Path::new(f).exists() {
                log::debug!("Loading: {}", f);
                savefile::load_file(f, 0).unwrap()
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
