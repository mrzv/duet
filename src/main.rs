use color_eyre::eyre::Result;
use clap::{clap_app,crate_version,crate_authors,ArgMatches};
use colored::*;

use tokio::sync::mpsc;

mod profile;
mod scan;
mod utils;
mod actions;
mod sync;
use actions::{Action,num_conflicts,reverse};
use scan::location::{Locations};
use scan::{Change,DirEntryWithMeta};

use std::fs::File;
use std::io::{BufWriter,BufReader};
use bincode::{serialize_into,deserialize_from};

use essrpc::essrpc;
use essrpc::transports::{BincodeTransport,ReadWrite};
use essrpc::{RPCClient, RPCError, RPCErrorKind, RPCServer};
use std::process::{Command, Stdio, Child, ChildStdin, ChildStdout};

type Entries = Vec<DirEntryWithMeta>;
type Changes = Vec<Change>;
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
            (@arg batch: -b          "run as a batch (abort on conflict)")
            (@arg force: -f          "in batch mode, apply what's possible, even if there are conflicts")
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
            let f = BufReader::new(File::open(fname).unwrap());
            deserialize_from(f).unwrap()
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
    println!("Using profile: {}", name.cyan());

    let local_base = shellexpand::full(&prf.local)?.to_string();

    let current_entries: Entries = scan_entries(&local_base, "", &prf.locations).await?;

    let statefile = if let Some(s) = statefile {
        String::from(s)
    } else {
        String::from(profile::local_state(name).to_str().unwrap())
    };
    let f = BufWriter::new(File::create(statefile).unwrap());
    serialize_into(f, &current_entries)?;
    Ok(())
}

async fn changes(matches: &ArgMatches<'_>) -> Result<()> {
    let name = matches.value_of("profile").unwrap();
    let statefile = matches.value_of("state");

    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.cyan());

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
    let batch = matches.is_present("batch");
    let force = matches.is_present("force");
    let path = matches.value_of("path").unwrap_or("");

    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.cyan());

    let local_id = local_id(name);

    let local_base = shellexpand::full(&prf.local)?.to_string();
    let remote_base = shellexpand::full(&prf.remote)?.to_string();

    let local_state = profile::local_state(name).to_string_lossy().into_owned();
    let (mut local_all_old, local_changes) = old_and_changes(&local_base, &path, &prf.locations, Some(&local_state)).await?;

    let mut server = launch_server();
    let mut remote = get_remote(&mut server);
    remote.set_base(remote_base)?;

    let remote_changes = remote.changes(path.to_string(), prf.locations, local_id).expect("Couldn't get remote changes");

    let actions: Actions = utils::match_sorted(local_changes.iter(), remote_changes.iter())
                                .filter_map(|(lc,rc)| Action::create(lc,rc))
                                .collect();

    if actions.is_empty() {
        println!("No changes detected");
        return Ok(())
    }

    for a in &actions {
        println!("{}", a);
    }

    if dry_run {
        return Ok(())
    }

    let num_conflicts = num_conflicts(&actions);
    if batch && num_conflicts > 0 && !force {
        println!("{} conflicts found; {}\n", num_conflicts, "aborting".bright_red());
        return Ok(())
    }

    let actions = {
        if num_conflicts == 0 || (batch && force) {
            actions
        } else {
            // not batch
            println!("Resolve conflicts:");
            let mut resolved_actions: Actions = Vec::new();
            for a in &actions {
                if let Action::Conflict(lc,rc) = a {
                    println!("{}", a);
                    let choice = loop {
                        println!("l = update local, r = update remote, c = keep conflict");
                        let choice: String = text_io::read!("{}\n");
                        if choice == "l" || choice == "r" || choice == "c" {
                            break choice;
                        } else {
                            println!("Unrecognized choice: {}", choice);
                        }
                    };
                    if choice == "l" {
                        if let (Change::Added(lc), Change::Added(rc)) = (lc, rc) {
                            resolved_actions.push(Action::Local(Change::Modified(lc.clone(), rc.clone())));
                        } else {
                            resolved_actions.push(Action::Local(rc.clone()));
                        }
                    } else if choice == "r" {
                        if let (Change::Added(lc), Change::Added(rc)) = (lc, rc) {
                            resolved_actions.push(Action::Remote(Change::Modified(rc.clone(), lc.clone())));
                        } else {
                            resolved_actions.push(Action::Remote(lc.clone()));
                        }
                    } else if choice == "c" {
                        resolved_actions.push(a.clone());
                    }
                    println!("{}", resolved_actions.last().unwrap());
                } else {
                    resolved_actions.push(a.clone());
                }
            }
            // resolve conflicts
            resolved_actions
        }
    };

    if !batch {
        use dialoguer::Confirm;

        if !Confirm::new().with_prompt("Do you want to continue?").interact()? {
            return Ok(());
        }
    }

    let actions: Actions = actions.into_iter().filter(|a| !a.is_conflict()).collect();
    let remote_actions: Actions = reverse(&actions);
    remote.set_actions(remote_actions)?;

    let local_signatures  = sync::get_signatures(&local_base, &actions).expect("couldn't get local signatures");
    let remote_signatures = remote.get_signatures().expect("couldn't get remote signatures");
    println!("{} local signatures; {} remote signatures\n", local_signatures.len(), remote_signatures.len());

    let local_detailed_changes  = sync::get_detailed_changes(&local_base, &actions, &remote_signatures).expect("couldn't get local detailed changes");
    let remote_detailed_changes = remote.get_detailed_changes(local_signatures).expect("couldn't get remote detailed changes");

    // updates local_all_old to be the new state
    sync::apply_detailed_changes(&local_base, &actions, &remote_detailed_changes, &mut local_all_old)?;
    remote.apply_detailed_changes(local_detailed_changes)?;

    remote.save_state()?;

    let f = BufWriter::new(File::create(local_state).unwrap());
    serialize_into(f, &local_all_old)?;
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

fn launch_server() -> Child {
    // launch server
    let server = Command::new("target/debug/duet")       // TODO: need a better way to find the command
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to spawn child process");

    log::trace!("launched server");

    server
}

fn get_remote(server: &mut Child) -> DuetServerRPCClient<BincodeTransport<ReadWrite<&mut ChildStdout, &mut ChildStdin>>> {
    let server_in = server.stdin.as_mut().expect("Failed to open stdin");
    let server_out = server.stdout.as_mut().expect("Failed to read stdout");

    let server_io = ReadWrite::new(server_out, server_in);

    let remote = DuetServerRPCClient::new(BincodeTransport::new(server_io));
    remote
}

use sync::{SignatureWithPath,ChangeDetails};

#[essrpc]
pub trait DuetServer {
    fn set_base(&mut self, base: String) -> Result<(), RPCError>;
    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError>;
    fn changes(&mut self, path: String, locations: Locations, remote_id: String) -> Result<Changes, RPCError>;
    fn get_signatures(&self) -> Result<Vec<SignatureWithPath>, RPCError>;
    fn get_detailed_changes(&self, signatures: Vec<SignatureWithPath>) -> Result<Vec<sync::ChangeDetails>, RPCError>;
    fn apply_detailed_changes(&mut self, details: Vec<ChangeDetails>) -> Result<(), RPCError>;
    fn save_state(&self) -> Result<(), RPCError>;
}

struct DuetServerImpl
{
    base:       String,
    remote_id:  String,
    all_old:    Entries,
    actions:    Actions,
}

impl DuetServerImpl {
    fn new() -> Self {
        DuetServerImpl {
            base:       "".to_string(),
            remote_id:  "".to_string(),
            all_old:    Vec::new(),
            actions:    Vec::new(),
        }
    }
}

impl DuetServer for DuetServerImpl {
    fn set_base(&mut self, base: String) -> Result<(), RPCError> {
        self.base = base;
        Ok(())
    }

    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError> {
        self.actions = actions;
        Ok(())
    }

    fn changes(&mut self, path: String, locations: Locations, remote_id: String) -> Result<Changes, RPCError> {
        log::debug!("remote id = {}", remote_id);
        self.remote_id = remote_id;
        let future = async move {
            let result = old_and_changes(&self.base, &path, &locations, Some(profile::remote_state(&self.remote_id).to_str().unwrap())).await;
            match result {
                Ok((all_old, changes)) => {
                    self.all_old = all_old;
                    Ok(changes)
                },
                Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in getting changes from the server"))
            }
        };
        use futures::{executor::block_on};
        let changes = block_on(future)?;
        Ok(changes)
    }

    fn get_signatures(&self) -> Result<Vec<SignatureWithPath>, RPCError> {
        let result = sync::get_signatures(&self.base, &self.actions);
        match result {
            Ok(signatures) => Ok(signatures),
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in getting signatures from the server"))
        }
    }

    fn get_detailed_changes(&self, signatures: Vec<SignatureWithPath>) -> Result<Vec<sync::ChangeDetails>, RPCError> {
        let result = sync::get_detailed_changes(&self.base, &self.actions, &signatures);
        match result {
            Ok(details) => Ok(details),
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in getting detailed changes from the server"))
        }
    }

    fn apply_detailed_changes(&mut self, details: Vec<ChangeDetails>) -> Result<(), RPCError> {
        let result = sync::apply_detailed_changes(&self.base, &self.actions, &details, &mut self.all_old);
        match result {
            Ok(()) => Ok(()),
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in applying detailed changes on the server"))
        }
    }

    fn save_state(&self) -> Result<(), RPCError> {
        let remote_state = profile::remote_state(&self.remote_id);
        log::info!("Saving remote state {} with {} entries", remote_state.to_str().unwrap(), &self.all_old.len());
        let f = BufWriter::new(File::create(remote_state).unwrap());
        let result = serialize_into(f, &self.all_old);
        match result {
            Ok(()) => Ok(()),
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in saving remote state on the server"))
        }
    }
}

async fn server() -> Result<()> {
    use std::io::{self};

    let stdin = io::stdin();
    let stdout = io::stdout();

    let stdio = ReadWrite::new(stdin, stdout);

    log::trace!("in server()");

    let mut serve = DuetServerRPCServer::new(DuetServerImpl::new(), BincodeTransport::new(stdio));
    match serve.serve() {
        Ok(_) => panic!("Expected EOF error"),
        Err(e) => assert_eq!(e.kind, RPCErrorKind::TransportEOF),
    };

    Ok(())
}

async fn old_and_changes(base: &str, restrict: &str, locations: &Locations, statefile: Option<&str>) -> Result<(Entries, Changes)> {
    let restricted_current_scan = scan_entries(base, restrict, locations);

    let all_old_entries: Entries =
        if let Some(f) = statefile {
            if std::path::Path::new(f).exists() {
                log::debug!("Loading: {}", f);
                let f = BufReader::new(File::open(f).unwrap());
                deserialize_from(f).unwrap()
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
