use color_eyre::eyre::{Result,eyre};
use clap::{clap_app,crate_version,crate_authors,ArgMatches,AppSettings};
use colored::*;

use tokio::sync::mpsc;

mod profile;
mod scan;
mod utils;
mod actions;
mod sync;
use actions::{Action,Actions,num_unresolved_conflicts,num_identical,reverse};
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
            (@arg interactive: -i    "interactive conflict resolution")
            (@arg dry_run: -n        "don't apply changes")
            (@arg batch: -b          "run as a batch (abort on conflict)")
            (@arg force: -f          "in batch mode, apply what's possible, even if there are conflicts")
            (@arg verbose: -v        "verbose output")
        )
        (@subcommand server =>
            (about: "run server-side code")
            (setting: AppSettings::Hidden)
        )
        // testing/debugging
        (@subcommand snapshot =>
            (about: "take snapshot")
            (@arg profile: +required "profile to snapshot")
            (@arg state:             "state file to save snapshot")
            (setting: AppSettings::Hidden)
        )
        (@subcommand inspect =>
            (about: "inspect a state file")
            (@arg state: +required "statefile to show")
            (setting: AppSettings::Hidden)
        )
        (@subcommand changes =>
            (about: "show changes compared to a given state")
            (@arg profile: +required "profile to compare")
            (@arg state:             "state file to compare")
            (setting: AppSettings::Hidden)
        )
        (@subcommand info =>
            (about: "show info about a profile")
            (@arg profile: +required "profile to compare")
            (setting: AppSettings::Hidden)
        )
        (@subcommand walk =>
            (about: "walk a directory")
            (@arg path: +required  "path to walk")
            (setting: AppSettings::Hidden)
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

async fn scan_entries(base: &str, path: &str, locations: &Locations, ignore: &profile::Ignore) -> Result<Entries> {
    let base = base.to_string();
    let path = path.to_string();
    let locations = locations.clone();
    let ignore = ignore.clone();

    let mut entries = tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel(32);
        tokio::spawn(async move {
            scan::scan(&base, &path, &locations, &ignore, tx).await
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

    let current_entries: Entries = scan_entries(&local_base, "", &prf.locations, &prf.ignore).await?;

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
    let (_, changes) = old_and_changes(&local_base, "", &prf.locations, &prf.ignore, Some(&statefile)).await?;

    for c in changes {
        println!("{} {}", c, c.path().display());
    }

    Ok(())
}

fn info(matches: &ArgMatches<'_>) -> Result<()> {
    let name = matches.value_of("profile").unwrap();
    println!("Profile {} located at {}", name.yellow(), profile::location(name).to_str().unwrap());
    Ok(())
}

fn parse_remote(remote: &String) -> Result<(String, Option<String>, String)> {
    let elements: Vec<&str> = remote.split_whitespace().collect();
    let (remote_server, i) =
        if elements[0] == "ssh" {
            (Some(elements[1].to_string()), 2)
        } else {
            (None, 0)
        };
    let (remote_cmd, remote_base, i) =
        if i == elements.len() - 1 {
            ("duet".to_string(), elements[i].to_string(), i+1)
        } else {
            (elements[i].to_string(), elements[i+1].to_string(), i+2)
        };
    if i < elements.len() {
        eyre!("Couldn't parse remote, elements remaining");
    }
    Ok((remote_base, remote_server, remote_cmd))
}

async fn sync(matches: &ArgMatches<'_>) -> Result<()> {
    let name = matches.value_of("profile").unwrap();
    let dry_run = matches.is_present("dry_run");
    let batch = matches.is_present("batch");
    let force = matches.is_present("force");
    let verbose = matches.is_present("verbose");
    let interactive = matches.is_present("interactive");
    let path = matches.value_of("path").unwrap_or("");

    let prf = profile::parse(name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.cyan());

    let local_id = local_id(name);

    let local_base = shellexpand::full(&prf.local)?.to_string();
    let (remote_base, remote_server, remote_cmd) = parse_remote(&prf.remote)?;

    let local_state = profile::local_state(name).to_string_lossy().into_owned();
    let (mut local_all_old, local_changes) = old_and_changes(&local_base, &path, &prf.locations, &prf.ignore, Some(&local_state)).await?;

    let mut server = launch_server(remote_server, remote_cmd);
    let mut remote = get_remote(&mut server);
    remote.set_base(remote_base)?;

    let remote_changes = remote.changes(path.to_string(), prf.locations, prf.ignore, local_id).expect("Couldn't get remote changes");

    let mut actions: Actions = utils::match_sorted(local_changes.iter(), remote_changes.iter())
                                .filter_map(|(lc,rc)| Action::create(lc,rc))
                                .collect();

    if actions.is_empty() {
        println!("No changes detected");
        return Ok(())
    }

    if dry_run {
        show_actions(&actions, verbose);
        return Ok(())
    }

    let num_conflicts = num_unresolved_conflicts(&actions);

    let resolution =
        if batch {
            show_actions(&actions, verbose);
            if force {
                AllResolution::Force
            } else if num_conflicts > 0 {
                println!("{} conflicts found; {}\n", num_conflicts, "aborting".bright_red());
                AllResolution::Abort
            } else {
                AllResolution::Proceed
            }
        } else if interactive {
            let resolution = resolve_interactive(&mut actions, verbose)?;
            show_actions(&actions, verbose);
            resolution
        } else {
            show_actions(&actions, verbose);
            resolve_sequential(&mut actions, verbose)?
        };

    if let AllResolution::Abort = resolution {
        println!("Aborting");
        return Ok(());
    }

    let actions: Actions = actions.into_iter().filter(|a| !a.is_unresolved_conflict()).collect();
    let remote_actions: Actions = reverse(&actions);
    remote.set_actions(remote_actions)?;

    let local_signatures  = sync::get_signatures(&local_base, &actions).expect("couldn't get local signatures");
    let remote_signatures = remote.get_signatures().expect("couldn't get remote signatures");
    log::debug!("{} local signatures; {} remote signatures", local_signatures.len(), remote_signatures.len());

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

// TODO: switch to tokio::process::Command; need to support async interface for openssh
fn launch_server(_server: Option<String>, cmd: String) -> Child {
    // launch server
    let cmd = shellexpand::full(&cmd).expect("Failed to expand command").to_string();
    let server = Command::new(cmd)
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to spawn remote server");

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
    fn changes(&mut self, path: String, locations: Locations, ignore: profile::Ignore, remote_id: String) -> Result<Changes, RPCError>;
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
        self.base =
            match shellexpand::full(&base) {
                Ok(s) => s.to_string(),
                Err(_) => { return Err(RPCError::new(RPCErrorKind::Other, "cannot expand base path, when setting remote base")); },
            };
        Ok(())
    }

    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError> {
        self.actions = actions;
        Ok(())
    }

    fn changes(&mut self, path: String, locations: Locations, ignore: profile::Ignore, remote_id: String) -> Result<Changes, RPCError> {
        log::debug!("remote id = {}", remote_id);
        self.remote_id = remote_id;
        let future = async move {
            let result = old_and_changes(&self.base, &path, &locations, &ignore, Some(profile::remote_state(&self.remote_id).to_str().unwrap())).await;
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

async fn old_and_changes(base: &str, restrict: &str, locations: &Locations, ignore: &profile::Ignore, statefile: Option<&str>) -> Result<(Entries, Changes)> {
    let restricted_current_scan = scan_entries(base, restrict, locations, ignore);

    use tokio::fs::File;
    use tokio::io::AsyncReadExt;
    let all_old_entries = async {
        let all_old_entries: Entries =
            if let Some(f) = statefile {
                if std::path::Path::new(f).exists() {
                    log::debug!("Loading: {}", f);
                    let mut f = File::open(f).await.unwrap();
                    let mut contents = vec![];
                    f.read_to_end(&mut contents).await.unwrap();
                    deserialize_from(contents.as_slice()).unwrap()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
        all_old_entries
    };

    use tokio::join;
    let (all_old_entries, restricted_current_scan) = join!(all_old_entries, restricted_current_scan);
    let restricted_old_entries_iter = all_old_entries
                                          .iter()
                                          .filter(move |dir: &&scan::DirEntryWithMeta| dir.starts_with(restrict));


    let changes: Vec<_> = scan::changes(restricted_old_entries_iter, restricted_current_scan?.iter()).collect();

    Ok((all_old_entries, changes))
}

async fn walk(matches: &ArgMatches<'_>) -> Result<()> {
    let path = matches.value_of("path").unwrap();

    use std::path::PathBuf;
    let locations = vec![scan::location::Location::Include(PathBuf::from("."))];

    let path = path.to_string();

    let (tx, mut rx) = mpsc::channel(1024);
    tokio::spawn(async move {
        scan::scan(path, "", &locations, &Vec::new(), tx).await
    });

    while let Some(e) = rx.recv().await {
        println!("{}", e.path().display());
    }
    Ok(())
}

enum Resolution {
    Local,
    Remote,
}

fn resolve_action(action: &Action, resolution: Resolution) -> Action {
    match action {
        Action::Conflict(lc,rc) | Action::ResolvedLocal((lc,rc),_) | Action::ResolvedRemote((lc,rc),_) => {
            match resolution {
                Resolution::Local =>
                    match (lc,rc) {
                        (Change::Added(ln), Change::Added(rn)) => {
                            Action::ResolvedLocal((lc.clone(),rc.clone()),Change::Modified(ln.clone(), rn.clone()))
                        },
                        (Change::Removed(_), Change::Modified(_,rn)) => {
                            Action::ResolvedLocal((lc.clone(),rc.clone()),Change::Added(rn.clone()))
                        },
                        (Change::Modified(_lo,ln), Change::Modified(_ro,rn)) => {
                            Action::ResolvedLocal((lc.clone(),rc.clone()),Change::Modified(ln.clone(),rn.clone()))
                        },
                        (Change::Modified(_,ln), Change::Removed(_)) => {
                            Action::ResolvedLocal((lc.clone(),rc.clone()),Change::Removed(ln.clone()))
                        },
                        _ => unreachable!()
                    },
                Resolution::Remote =>
                    match (lc,rc) {
                        (Change::Added(ln), Change::Added(rn)) => {
                            Action::ResolvedRemote((lc.clone(),rc.clone()),Change::Modified(rn.clone(), ln.clone()))
                        },
                        (Change::Modified(_,ln), Change::Removed(_rn)) => {
                            Action::ResolvedRemote((lc.clone(),rc.clone()),Change::Added(ln.clone()))
                        },
                        (Change::Modified(_lo,ln), Change::Modified(_ro,rn)) => {
                            Action::ResolvedRemote((lc.clone(),rc.clone()),Change::Modified(rn.clone(),ln.clone()))
                        },
                        (Change::Removed(_ln), Change::Modified(_,rn)) => {
                            Action::ResolvedRemote((lc.clone(),rc.clone()),Change::Removed(rn.clone()))
                        },
                        _ => unreachable!()
                    },
            }
        },
        _ => { action.clone() }
    }
}

fn show_actions(actions: &Actions, verbose: bool) {
    let num_identical = num_identical(&actions);
    for a in actions {
        if verbose || !a.is_identical() {
            println!("{}", a);
        }
    }
    if !verbose && num_identical > 0 {
        println!("Skipped {} identical changes (use --verbose to show all)", num_identical);
    }
}

#[derive(Debug)]
enum AllResolution {
    Proceed,
    Abort,
    Force,
}

fn resolve_sequential(actions: &mut Actions, _verbose: bool) -> Result<AllResolution> {
    // not batch
    use console::{Key,Term};
    let term = Term::stdout();
    if num_unresolved_conflicts(actions) > 0 {
        term.write_line("Resolve conflicts:")?;

        for a in actions {
            if let Action::Conflict(_,_) = &a {
                term.write_line(format!("{}", a).as_str())?;

                loop {
                    term.write_line("left/l = update local, right/r = update remote, c = keep conflict, n/a = abort")?;
                    match term.read_key()? {
                        Key::ArrowLeft | Key::Char('l') => {
                            *a = resolve_action(&a, Resolution::Local);
                        }
                        Key::ArrowRight | Key::Char('r') => {
                            *a = resolve_action(&a, Resolution::Remote);
                        }
                        Key::Char('c') => {
                            // keep as is
                        }
                        Key::Char('a') => {
                            term.clear_last_lines(1)?;
                            return Ok(AllResolution::Abort);
                        }
                        _ => {
                            // didn't recognize the choice, try again
                            term.clear_last_lines(1)?;
                            continue;
                        }
                    }
                    term.clear_last_lines(2)?;
                    term.write_line(format!("{}", a).as_str())?;
                    break;
                }
            }
        }
    }

    use dialoguer::Confirm;
    if !Confirm::new().with_prompt("Do you want to continue?").interact()? {
        Ok(AllResolution::Abort)
    } else {
        Ok(AllResolution::Proceed)
    }
}

fn resolve_interactive(actions: &mut Actions, verbose: bool) -> Result<AllResolution> {
    // Taken from dialoguer::prompts::Select::interact_on()
    // The MIT License (MIT)
    // Copyright (c) 2017 Armin Ronacher <armin.ronacher@active-4.com>

    use console::{Key,Term};
    use std::ops::Rem;
    let term = Term::stderr();

    let mut page = 0;

    assert!(!actions.is_empty());

    let capacity = term.size().0 as usize - 2;      // extra -1 for the prompt
    let pages = ((actions.len() - (if verbose { 0 } else { num_identical(actions) })) as f64 / capacity as f64).ceil() as usize;

    let mut sel = 0;
    let mut height = 0;
    let mut num_conflicts = num_unresolved_conflicts(&actions);

    let resolution = loop {
        term.write_line(format!("{}{}n/a = abort, f = force{} [{}]",
                    if num_conflicts == 0 { "y/g = proceed".bright_green() } else { "".normal() },
                    if num_conflicts == 0 { ", ".normal() } else { "".normal() },
                    if actions[sel].is_conflict() { ", left/l = update local, right/r = update remote, c = keep conflict" } else { "" },
                    num_conflicts).as_str())?;
        height += 1;

        for (idx, action) in actions
            .iter()
            .enumerate()
            .skip(page * capacity)
            .take(capacity)
        {
            if verbose || !action.is_identical() {
                term.write_line(format!("{} {}",
                         (if sel == idx { ">" } else {" "}).cyan(),
                         action).as_str())?;
                height += 1;
            }
        }

        term.hide_cursor()?;
        term.flush()?;

        match term.read_key()? {
            Key::ArrowDown | Key::Char('j') => {
                loop {
                    sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
                    if verbose || !actions[sel].is_identical() {
                        break;
                    }
                };
            }
            Key::ArrowUp | Key::Char('k') => {
                loop {
                    sel = ((sel as i64 - 1 + actions.len() as i64)
                        % (actions.len() as i64)) as usize;
                    if verbose || !actions[sel].is_identical() {
                        break;
                    }
                };
            }
            Key::Tab => {       // go to next conflict
                loop {
                    sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
                    if actions[sel].is_conflict() {
                        break;
                    }
                };
            }
            Key::BackTab => {   // go to previous conflict
                loop {
                    sel = ((sel as i64 - 1 + actions.len() as i64)
                        % (actions.len() as i64)) as usize;
                    if actions[sel].is_conflict() {
                        break;
                    }
                };
            }
            Key::ArrowLeft | Key::Char('l') => {
                if actions[sel].is_conflict() {
                    if actions[sel].is_unresolved_conflict() {
                        num_conflicts -= 1;
                    }
                    actions[sel] = resolve_action(&actions[sel], Resolution::Local);
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            Key::ArrowRight | Key::Char('r') => {
                if actions[sel].is_conflict() {
                    if actions[sel].is_unresolved_conflict() {
                        num_conflicts -= 1;
                    }
                    actions[sel] = resolve_action(&actions[sel], Resolution::Remote);
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            Key::Char('c') => {
                if actions[sel].is_conflict() {
                    if !actions[sel].is_unresolved_conflict() {
                        match &actions[sel] {
                            Action::ResolvedLocal((lc,rc),_) | Action::ResolvedRemote((lc,rc),_) => {
                                actions[sel] = Action::Conflict(lc.clone(),rc.clone());
                            },
                            _ => { unreachable!(); }
                        }
                        num_conflicts += 1;
                    }
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            Key::PageUp => {
                if page == 0 {
                    page = pages - 1;
                } else {
                    page -= 1;
                }

                sel = page * capacity;
            }
            Key::PageDown => {
                if page == pages - 1 {
                    page = 0;
                } else {
                    page += 1;
                }

                sel = page * capacity;
            }

            Key::Char('y') | Key::Char('g') if num_conflicts == 0 => {
                break AllResolution::Proceed;
            }

            Key::Escape | Key::Char('a') | Key::Char('n') => {
                break AllResolution::Abort;
            }

            Key::Char('f') => {
                break AllResolution::Force;
            }

            _ => { }
        }

        if sel < page * capacity || sel >= (page + 1) * capacity {
            page = sel / capacity;
        }

        term.clear_last_lines(height)?;
        height = 0;
    };

    term.clear_last_lines(height)?;
    term.show_cursor()?;
    term.flush()?;

    Ok(resolution)
}
