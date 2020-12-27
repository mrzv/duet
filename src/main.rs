use color_eyre::eyre::Result;
use clap::{clap_app,crate_version,crate_authors,ArgMatches};
use colored::*;

use tokio::sync::mpsc;

mod profile;
mod scan;
mod utils;
mod actions;
use actions::Action;
use scan::location::{Locations};

type Entries = Vec<scan::DirEntryWithMeta>;
type Changes = Vec<scan::Change>;
type Actions = Vec<Action>;

#[tokio::main]
pub async fn main() -> Result<()> {
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
        // testing/debugging
        (@subcommand walk =>
            (about: "walk a directory")
            (@arg path: +required  "path to walk")
        )
    ).get_matches();

    if let Some(matches) = matches.subcommand_matches("sync") {
        return sync(matches).await;
    } else if let Some(matches) = matches.subcommand_matches("snapshot") {
        return snapshot(matches).await;
    } else if let Some(matches) = matches.subcommand_matches("inspect") {
        return inspect(matches);
    } else if let Some(matches) = matches.subcommand_matches("changes") {
        return changes(matches).await;
    } else if let Some(matches) = matches.subcommand_matches("info") {
        return info(matches);
    } else if let Some(_matches) = matches.subcommand_matches("server") {
        return server().await;
    } else if let Some(matches) = matches.subcommand_matches("walk") {
        return walk(matches).await;
    }

    Ok(())
}

fn inspect(matches: &ArgMatches<'_>) -> Result<()> {
    let statefile = matches.value_of("state").unwrap();
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

async fn scan_entries(base: &str, path: &str, locations: &Locations) -> Result<Entries> {
    let base = base.to_string();
    let path = path.to_string();
    let locations = locations.clone();

    let mut entries = tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel(32);
        tokio::spawn(async move {
            scan::scan(&base, &path, &locations, tx).await
        });

        let mut entries: Entries = Entries::new();
        while let Some(e) = rx.recv().await {
            entries.push(e);
        }

        entries
    }).await?;

    entries.sort();

    Ok(entries)
}

async fn snapshot(matches: &ArgMatches<'_>) -> Result<()> {
    let name = matches.value_of("profile").unwrap();
    let statefile = matches.value_of("state");

    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.yellow());

    let local_base = shellexpand::full(&prf.local)?.to_string();

    let current_entries: Entries = scan_entries(&local_base, "", &prf.locations).await?;

    let statefile = if let Some(s) = statefile {
        String::from(s)
    } else {
        String::from(profile::local_state(name).to_str().unwrap())
    };
    savefile::save_file(&statefile, 0, &current_entries).unwrap();
    Ok(())
}

async fn changes(matches: &ArgMatches<'_>) -> Result<()> {
    let name = matches.value_of("profile").unwrap();
    let statefile = matches.value_of("state");

    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.yellow());

    let local_base = shellexpand::full(&prf.local)?.to_string();

    let statefile = if let Some(s) = statefile {
        String::from(s)
    } else {
        String::from(profile::local_state(name).to_str().unwrap())
    };
    let (_, changes) = old_and_changes(&local_base, "", &prf.locations, Some(&statefile)).await?;

    for c in changes {
        println!("{}", c);
    }

    Ok(())
}

fn info(matches: &ArgMatches<'_>) -> Result<()> {
    let name = matches.value_of("profile").unwrap();
    println!("Profile {} located at {}", name.yellow(), profile::location(name).to_str().unwrap());
    Ok(())
}

async fn sync(matches: &ArgMatches<'_>) -> Result<()> {
    let name = matches.value_of("profile").unwrap();
    let dry_run = matches.is_present("dry_run");
    let path = matches.value_of("path").unwrap_or("");

    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.yellow());

    let local_id = local_id(name);

    let local_base = shellexpand::full(&prf.local)?.to_string();
    let remote_base = shellexpand::full(&prf.remote)?.to_string();

    let local_state = profile::local_state(name).to_string_lossy().into_owned();
    let (local_all_old, local_changes) = old_and_changes(&local_base, &path, &prf.locations, Some(&local_state)).await?;
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

    savefile::save_file(&local_state, 0, &local_all_old).unwrap();
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
        .stderr(Stdio::inherit())
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

async fn server() -> Result<()> {
    use std::io::{self};
    use savefile::{save,load};

    let stdin = &mut io::stdin();

    let base: String = load(stdin, 0).expect("Failed to load base");
    let path: String = load(stdin, 0).expect("Failed to load path");
    let locations: Locations = load(stdin, 0).expect("Failed to load locations");
    let remote_id: String = load(stdin, 0).expect("Failed to load path");

    let (_all_old, changes) = old_and_changes(&base, &path, &locations, Some(profile::remote_state(&remote_id).to_str().unwrap())).await?;

    let stdout = &mut io::stdout();
    save(stdout, 0, &changes).expect("Can't send changes to the client");

    Ok(())
}

async fn old_and_changes(base: &str, restrict: &str, locations: &Locations, statefile: Option<&str>) -> Result<(Entries, Changes)> {
    let restricted_current_scan = scan_entries(base, restrict, locations);

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

    let changes: Vec<_> = scan::changes(restricted_old_entries_iter, restricted_current_scan.await?.iter()).collect();

    Ok((all_old_entries, changes))
}

async fn walk(matches: &ArgMatches<'_>) -> Result<()> {
    let path = matches.value_of("path").unwrap();

    use std::path::PathBuf;
    let locations = vec![scan::location::Location::Include(PathBuf::from("."))];

    let path = path.to_string();

    let (tx, mut rx) = mpsc::channel(1024);
    tokio::spawn(async move {
        scan::scan(path, "", &locations, tx).await
    });

    while let Some(e) = rx.recv().await {
        println!("{}", e.path());
    }
    Ok(())
}
