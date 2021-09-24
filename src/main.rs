use color_eyre::eyre::{Result,eyre};
use colored::*;

use tokio::sync::mpsc;

mod profile;
mod scan;
mod utils;
mod actions;
mod sync;
mod rustsync;
#[macro_use]
extern crate serde_derive;

use actions::{Action,Actions,num_unresolved_conflicts,num_identical,reverse};
use scan::location::{Locations};
use scan::{Change,DirEntryWithMeta};
use std::path::{Path,PathBuf};

use std::fs::File;
use std::io::{BufWriter,BufReader};
use bincode::{serialize_into,deserialize_from};

use essrpc::essrpc;
use essrpc::transports::{BincodeTransport,BincodeAsyncClientTransport,ReadWrite};
use essrpc::{AsyncRPCClient, RPCError, RPCErrorKind, RPCServer};
use std::process::{Stdio};
use tokio::process::{Command, Child, ChildStdin, ChildStdout};
use openssh::{Session, KnownHosts, RemoteChild};

type Entries = Vec<DirEntryWithMeta>;
type Changes = Vec<Change>;

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn show_help() {
        print!("\
duet {}
bi-directional synchronization

USAGE:
    duet [FLAGS] <profile> [path]

FLAGS:
    -i, --interactive   interactive conflict resolution
    -y, --yes           assume yes (i.e., synchronize, if there are no conflicts)
    -b, --batch         run as a batch (abort on conflict)
    -f, --force         in batch mode, apply what's possible, even if there are conflicts
    -v, --verbose       verbose output
    -n, --dry-run       don't apply changes

        --version       prints version information
        --license       prints license information (including dependencies)
    -h, --help          prints help information

ARGS:
    <profile>    profile to synchronize
    <path>       path to synchronize
", built_info::PKG_VERSION);
}

#[tokio::main]
#[quit::main]
pub async fn main() -> Result<()> {
    color_eyre::install().unwrap();

    let mut pargs = pico_args::Arguments::from_env();

    if pargs.contains(["-h", "--help"]) {
        show_help();
        return Ok(());
    }

    if pargs.contains("--version") {
        println!("duet {}", built_info::PKG_VERSION);
        for (name,version) in built_info::DEPENDENCIES {
            println!("  {} {}", name, version);
        }
        return Ok(());
    }

    if pargs.contains("--license") {
        println!("{}\n", include_str!("../LICENSE"));
        println!("{}", include_str!("../licenses/deps.txt"));
        println!("{}", include_str!("../licenses/included.txt"));
        return Ok(());
    }

    if pargs.contains("--server") {
        return server().await;
    }

    let interactive = pargs.contains(["-i", "--interactive"]);
    let yes         = pargs.contains(["-y", "--yes"]);
    let dry_run     = pargs.contains(["-n", "--dry-run"]);
    let batch       = pargs.contains(["-b", "--batch"]);
    let force       = pargs.contains(["-f", "--force"]);
    let verbose     = pargs.contains(["-v", "--verbose"]);
    let profile     = pargs.free_from_str::<String>();

    if let Err(_) = profile {
        show_help();
        return Ok(());
    }
    let profile = profile.unwrap();

    // check for possible (hidden) subcommands
    match profile.as_str() {
        "_snapshot" => {
            // take snapshot of a profile into a statefile
            let profile = pargs.free_from_str()?;
            let state = pargs.opt_free_from_os_str(parse_path)?;
            return snapshot(profile, state).await;
        },
        "_inspect" => {
            // inspect a state file
            let state = pargs.free_from_os_str(parse_path)?;
            return inspect(state);
        },
        "_changes" => {
            // show changes compared to a given state
            let profile = pargs.free_from_str()?;
            let state = pargs.opt_free_from_os_str(parse_path)?;
            return changes(profile, state).await;
        },
        "_info" => {
            // show info about a profile
            let profile = pargs.free_from_str()?;
            return info(profile);
        },
        "_walk" => {
            // walk a directory
            let path = pargs.free_from_os_str(parse_path)?;
            return walk(path).await;
        },
        _ => {
            // default = synchronize according to profile
            let path = pargs.opt_free_from_os_str(parse_path)?;
            return sync(profile, path, interactive, yes, dry_run, batch, force, verbose).await;
        },
    }
}

fn parse_path(s: &std::ffi::OsStr) -> Result<std::path::PathBuf, &'static str> {
    Ok(s.into())
}

fn inspect(statefile: PathBuf) -> Result<()> {
    let entries: Entries =
        if statefile.exists() {
            log::debug!("Loading: {}", statefile.display());
            let f = BufReader::new(File::open(statefile).unwrap());
            deserialize_from(f).unwrap()
        } else {
            Vec::new()
        };
    for e in entries {
        println!("{:?}", e);
    }
    Ok(())
}

async fn scan_entries(base: &PathBuf, path: &PathBuf, locations: &Locations, ignore: &profile::Ignore) -> Result<Entries> {
    let base = base.clone();
    let path = path.clone();
    let locations = locations.clone();
    let ignore = ignore.clone();

    let mut entries = async move {
        let (tx, mut rx) = mpsc::channel(32);
        tokio::spawn(async move {
            scan::scan(&base, &path, &locations, &ignore, tx).await
        });

        let mut entries: Entries = Entries::new();
        while let Some(e) = rx.recv().await {
            entries.push(e);
        }

        entries
    }.await;
    log::debug!("Done scanning");

    entries.sort();

    Ok(entries)
}

async fn snapshot(name: String, statefile: Option<PathBuf>) -> Result<()> {
    let prf = profile::parse(&name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.cyan());

    let local_base = full(&prf.local)?;

    let current_entries: Entries = scan_entries(&local_base, &PathBuf::from(""), &prf.locations, &prf.ignore).await?;

    let statefile = statefile.unwrap_or(profile::local_state(&name));
    let f = BufWriter::new(File::create(statefile).unwrap());
    serialize_into(f, &current_entries)?;
    Ok(())
}

async fn changes(name: String, statefile: Option<PathBuf>) -> Result<()> {
    let prf = profile::parse(&name).expect(&format!("Failed to read profile {}", name.yellow()));
    println!("Using profile: {}", name.cyan());

    let local_base = full(&prf.local)?;

    let statefile = statefile.unwrap_or(profile::local_state(&name));

    let (_, changes) = old_and_changes(&local_base, &PathBuf::from(""), &prf.locations, &prf.ignore, Some(&statefile)).await?;

    for c in changes {
        println!("{} {}", c, c.path().display());
    }

    Ok(())
}

fn info(name: String) -> Result<()> {
    println!("Profile {} located at {}", name.cyan(), profile::location(&name).display().to_string().yellow());
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

fn normalize_path(local_base: &PathBuf, path: &PathBuf) -> Result<PathBuf> {
    // if path starts with a . or .., treat it as relative to current directory
    if path.starts_with("./") || path.starts_with("../") || path == Path::new(".") || path == Path::new("..") {
        let cwd = std::env::current_dir()?;
        use path_clean::{PathClean};
        let path = cwd.join(path).clean();
        return Ok(scan::relative(&local_base, &path).to_path_buf());
    }

    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(scan::relative(&local_base, &path).to_path_buf())
    } else {
        Ok(path)
    }
}

fn full(s: &String) -> Result<PathBuf> {
    Ok(PathBuf::from(shellexpand::full(s)?.into_owned()))
}

const OK_CODE: i32 = 0;
const ABORT_CODE: i32 = 1;
const PROFILE_ERROR_CODE: i32 = 2;
const SSH_ERROR_CODE: i32 = 3;
const CTRLC_CODE: i32 = 6;

async fn sync(name: String, path: Option<PathBuf>,
              interactive: bool, yes: bool, dry_run: bool,
              batch: bool, force: bool, verbose: bool) -> Result<()> {
    env_logger::init();

    ctrlc::set_handler(|| {
        eprintln!("\nQuitting");
        quit::with_code(CTRLC_CODE);
    }).expect("Error setting Ctrl-C handler");

    let prf = profile::parse(&name).unwrap_or_else(|e| {
        eprintln!("Failed to read profile {} ({})", name.yellow(), e.to_string().cyan());
        quit::with_code(PROFILE_ERROR_CODE);
    });

    let local_id = local_id(&name);

    let local_base = full(&prf.local)?;
    let (remote_base, remote_server, remote_cmd) = parse_remote(&prf.remote)?;

    let path = normalize_path(&local_base, &path.unwrap_or(PathBuf::from("")))?;
    println!("Using profile: {} {}", name.cyan(), path.display().to_string().yellow());

    let local_state = profile::local_state(&name);

    // --- Get local and remote changes concurrently ---
    let local_fut = old_and_changes(&local_base, &path, &prf.locations, &prf.ignore, Some(&local_state));

    let remote_session =
        if let Some(server) = remote_server {
            Some(Session::connect(server, KnownHosts::Strict).await.unwrap_or_else(|_| {
                eprintln!("Unable to get SSH session");
                quit::with_code(SSH_ERROR_CODE);
            }))
        } else {
            None
        };
    let mut server = launch_server(&remote_session, remote_cmd);
    let remote = get_remote(&mut server);

    let path = path.clone();
    let remote_fut = async {
        remote.set_base(remote_base).await.expect("Couldn't set server base");
        remote.changes(path, prf.locations.clone(), prf.ignore.clone(), local_id).await.expect("Couldn't get remote changes")
    };

    use tokio::join;
    let (local_result, remote_changes) = join!(local_fut,remote_fut);
    let (mut local_all_old, local_changes) = local_result.expect("Couldn't get local changes");
    // -------------------------------------------------

    let mut actions: Actions = utils::match_sorted(local_changes.iter(), remote_changes.iter())
                                .filter_map(|(lc,rc)| Action::create(lc,rc))
                                .collect();

    if actions.is_empty() {
        println!("No changes detected");
        quit::with_code(OK_CODE);
    }

    if dry_run {
        show_actions(&actions, verbose);
        quit::with_code(OK_CODE);
    }

    let num_conflicts = num_unresolved_conflicts(actions.iter());

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
            let resolution =
                if yes && num_conflicts == 0 {
                    AllResolution::Proceed
                } else {
                    resolve_interactive(&mut actions, verbose)?
                };
            show_actions(&actions, verbose);
            resolution
        } else {
            show_actions(&actions, verbose);
            if yes && num_conflicts == 0 {
                AllResolution::Proceed
            } else {
                resolve_sequential(&mut actions, verbose)?
            }
        };

    if let AllResolution::Abort = resolution {
        println!("Aborting");
        quit::with_code(ABORT_CODE);
    }

    log::debug!("synchronizing");

    let actions: Actions = actions.into_iter().filter(|a| !a.is_unresolved_conflict()).collect();
    let remote_actions: Actions = reverse(&actions);
    remote.set_actions(remote_actions).await.expect("Failed to set remote actions");
    log::debug!("set remote actions");

    let local_signatures  = sync::get_signatures(&local_base, &actions).expect("couldn't get local signatures");
    let remote_signatures = remote.get_signatures().await.expect("couldn't get remote signatures");
    log::debug!("{} local signatures; {} remote signatures", local_signatures.len(), remote_signatures.len());

    let local_detailed_changes  = sync::get_detailed_changes(&local_base, &actions, &remote_signatures).expect("couldn't get local detailed changes");
    let remote_detailed_changes = remote.get_detailed_changes(local_signatures).await.expect("couldn't get remote detailed changes");
    log::debug!("got detailed changes");

    // updates local_all_old to be the new state
    sync::apply_detailed_changes(&local_base, &actions, &remote_detailed_changes, &mut local_all_old)?;
    remote.apply_detailed_changes(local_detailed_changes).await?;

    let (remote_result, local_result) =
        tokio::join!(remote.save_state(),
                     tokio::task::spawn_blocking(move || {
                        use atomicwrites::{AtomicFile,AllowOverwrite};
                        let af = AtomicFile::new(local_state, AllowOverwrite);
                        af.write(|f| {
                            let f = BufWriter::new(f);
                            serialize_into(f, &local_all_old)
                        })
                     }));
    let _ = local_result.expect("Failed to save local state");
    let _ = remote_result.expect("Failed to save remote state");

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

enum Server<'a> {
    Local(Child),
    Remote(RemoteChild<'a>),
}

fn launch_server(session: &Option<Session>, cmd: String) -> Server {
    // launch server
    if let Some(session) = session {
        let server = session.command(cmd)
            .arg("--server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Failed to spawn remote server");

        log::trace!("launched remote server");

        Server::Remote(server)
    } else {
        let cmd = shellexpand::full(&cmd).expect("Failed to expand command").to_string();
        let server = Command::new(cmd)
            .arg("--server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Failed to spawn remote server");

        log::trace!("launched local server");

        Server::Local(server)
    }
}


use readwrite::ReadWriteAsyncstd;
use tokio_util::compat::{Compat,TokioAsyncReadCompatExt,TokioAsyncWriteCompatExt};
use async_std::io::{BufReader as AsyncBufReader, BufWriter as AsyncBufWriter};

fn get_remote<'a>(server: &'a mut Server) -> DuetServerAsyncRPCClient<BincodeAsyncClientTransport<ReadWriteAsyncstd<AsyncBufReader<Compat<ChildStdout>>, AsyncBufWriter<Compat<ChildStdin>>>>> {
    let (server_in, server_out) =
        match server {
            Server::Local(server) => {
                let server_in = server.stdin.take().expect("Failed to open local stdin");
                let server_out = server.stdout.take().expect("Failed to read local stdout");
                (server_in, server_out)
            },
            Server::Remote(server) => {
                let server_in = server.stdin().take().expect("Failed to open remote stdin");
                let server_out = server.stdout().take().expect("Failed to open remote stdout");
                (server_in, server_out)
            },
        };

    let server_io = ReadWriteAsyncstd::new(AsyncBufReader::new(server_out.compat()), AsyncBufWriter::new(server_in.compat_write()));

    let remote = DuetServerAsyncRPCClient::new(BincodeAsyncClientTransport::new(server_io));
    remote
}

use sync::{SignatureWithPath,ChangeDetails};

#[essrpc(sync,async)]
pub trait DuetServer {
    fn set_base(&mut self, base: String) -> Result<(), RPCError>;
    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError>;
    fn changes(&mut self, path: PathBuf, locations: Locations, ignore: profile::Ignore, remote_id: String) -> Result<Changes, RPCError>;
    fn get_signatures(&self) -> Result<Vec<SignatureWithPath>, RPCError>;
    fn get_detailed_changes(&self, signatures: Vec<SignatureWithPath>) -> Result<Vec<sync::ChangeDetails>, RPCError>;
    fn apply_detailed_changes(&mut self, details: Vec<ChangeDetails>) -> Result<(), RPCError>;
    fn save_state(&self) -> Result<(), RPCError>;
}

struct DuetServerImpl
{
    base:       PathBuf,
    remote_id:  String,
    all_old:    Entries,
    actions:    Actions,
}

impl DuetServerImpl {
    fn new() -> Self {
        DuetServerImpl {
            base:       PathBuf::from(""),
            remote_id:  "".to_string(),
            all_old:    Vec::new(),
            actions:    Vec::new(),
        }
    }
}

impl DuetServer for DuetServerImpl {
    fn set_base(&mut self, base: String) -> Result<(), RPCError> {
        self.base =
            match full(&base) {
                Ok(s) => s,
                Err(_) => { return Err(RPCError::new(RPCErrorKind::Other, "cannot expand base path, when setting remote base")); },
            };
        log::debug!("Set base {}", self.base.display());
        Ok(())
    }

    fn set_actions(&mut self, actions: Actions) -> Result<(), RPCError> {
        log::debug!("Setting {} actions", actions.len());
        self.actions = actions;
        Ok(())
    }

    fn changes(&mut self, path: PathBuf, locations: Locations, ignore: profile::Ignore, remote_id: String) -> Result<Changes, RPCError> {
        log::debug!("remote id = {}", remote_id);
        self.remote_id = remote_id;

        let handle = tokio::runtime::Handle::current();
        let result = handle.block_on(async {
            old_and_changes(&self.base, &path, &locations, &ignore, Some(&profile::remote_state(&self.remote_id))).await
        });

        match result {
            Ok((all_old, changes)) => {
                self.all_old = all_old;
                Ok(changes)
            },
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in getting changes from the server"))
        }
    }

    fn get_signatures(&self) -> Result<Vec<SignatureWithPath>, RPCError> {
        log::debug!("Getting signatures");
        let result = sync::get_signatures(&self.base, &self.actions);
        match result {
            Ok(signatures) => Ok(signatures),
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in getting signatures from the server"))
        }
    }

    fn get_detailed_changes(&self, signatures: Vec<SignatureWithPath>) -> Result<Vec<sync::ChangeDetails>, RPCError> {
        log::debug!("Getting detailed changes for {} signatures", signatures.len());
        let result = sync::get_detailed_changes(&self.base, &self.actions, &signatures);
        match result {
            Ok(details) => Ok(details),
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in getting detailed changes from the server"))
        }
    }

    fn apply_detailed_changes(&mut self, details: Vec<ChangeDetails>) -> Result<(), RPCError> {
        log::debug!("Appling detailed changes, with {} details", details.len());
        let result = sync::apply_detailed_changes(&self.base, &self.actions, &details, &mut self.all_old);
        match result {
            Ok(()) => Ok(()),
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in applying detailed changes on the server"))
        }
    }

    fn save_state(&self) -> Result<(), RPCError> {
        log::debug!("Saving state");
        std::fs::create_dir_all(profile::remote_state_dir())?;
        let remote_state = profile::remote_state(&self.remote_id);
        log::info!("Saving remote state {} with {} entries", remote_state.to_str().unwrap(), &self.all_old.len());
        use atomicwrites::{AtomicFile,AllowOverwrite};
        let af = AtomicFile::new(remote_state, AllowOverwrite);
        let result = af.write(|f| {
            let f = BufWriter::new(f);
            serialize_into(f, &self.all_old)
        });
        match result {
            Ok(()) => Ok(()),
            Err(_) => Err(RPCError::new(RPCErrorKind::Other, "error in saving remote state on the server"))
        }
    }
}

async fn server() -> Result<()> {
    std::fs::create_dir_all(full(&"~/.config/duet".to_string())?)?;
    use log::LevelFilter;
    simple_logging::log_to_file(full(&"~/.config/duet/remote.log".to_string())?, LevelFilter::Debug)?;

    use std::io::{self};

    let stdin = io::stdin();
    let stdout = io::stdout();

    let stdio = ReadWrite::new(stdin, stdout);

    log::debug!("in server()");

    tokio::task::spawn_blocking(|| {
        let mut serve = DuetServerRPCServer::new(DuetServerImpl::new(), BincodeTransport::new(stdio));
        match serve.serve() {
            Ok(_) => panic!("Expected EOF error"),
            Err(e) => assert_eq!(e.kind, RPCErrorKind::TransportEOF),
        };
    }).await?;

    Ok(())
}

async fn old_and_changes(base: &PathBuf, restrict: &PathBuf, locations: &Locations, ignore: &profile::Ignore, statefile: Option<&PathBuf>) -> Result<(Entries, Changes)> {
    let restricted_current_scan = scan_entries(base, restrict, locations, ignore);

    use tokio::fs::File;
    use tokio::io::AsyncReadExt;
    let all_old_entries = async {
        let all_old_entries: Entries =
            if let Some(f) = statefile {
                if f.exists() {
                    log::debug!("Loading: {}", f.display());
                    let mut f = File::open(f).await.unwrap();
                    let mut contents = vec![];
                    f.read_to_end(&mut contents).await.unwrap();
                    log::debug!("Done loading");
                    deserialize_from(contents.as_slice()).unwrap()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
        log::debug!("Done reading out entries");
        all_old_entries
    };

    use tokio::join;
    let (all_old_entries, restricted_current_scan) = join!(all_old_entries, restricted_current_scan);
    let restricted_old_entries_iter = all_old_entries
                                          .iter()
                                          .filter(move |dir: &&scan::DirEntryWithMeta| dir.starts_with(restrict));


    let mut changes: Vec<_> = scan::changes(restricted_old_entries_iter, restricted_current_scan?.iter()).collect();

    // compute checksums
    log::debug!("Computing checksums for {} changes", changes.len());
    let base = PathBuf::from(base);
    for change in &mut changes {
        match change {
            Change::Added(n) => { n.compute_checksum(&base).await.expect(format!("Unable to compute checksum for {:?}", n).as_str()); },
            Change::Modified(_,n) => { n.compute_checksum(&base).await.expect(format!("Unable to compute checksum for {:?}", n).as_str()); },
            Change::Removed(_) => {},
        }
    }

    Ok((all_old_entries, changes))
}

async fn walk(path: PathBuf) -> Result<()> {
    let locations = vec![scan::location::Location::Include(PathBuf::from("."))];

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
    let num_identical = num_identical(actions.iter());
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
    if num_unresolved_conflicts(actions.iter()) > 0 {
        term.write_line("Resolve conflicts:")?;

        for a in actions {
            if let Action::Conflict(_,_) = &a {
                term.write_line(format!("{}", a).as_str())?;
                term.write_line(actions::details(a).as_str())?;

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
                    term.clear_last_lines(3)?;
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

    let mut actions: Vec<&mut Action> = actions
                    .iter_mut()
                    .filter(|a| verbose || !a.is_identical())
                    .collect();

    let capacity = term.size().0 as usize - 3;      // extra -1 for the prompt, -1 for detaled changes
    let pages = (actions.len() as f64 / capacity as f64).ceil() as usize;

    let mut sel = 0;
    let mut height = 0;
    let mut num_conflicts = num_unresolved_conflicts(actions.iter().map(|a| &**a));

    let resolution = loop {
        term.write_line(format!("{}{}n/a = abort, f = force{} [{}]",
                    if num_conflicts == 0 { "y/g = proceed".bright_green() } else { "".normal() },
                    if num_conflicts == 0 { ", ".normal() } else { "".normal() },
                    if actions[sel].is_conflict() { ", left/l = update local, right/r = update remote, c = keep conflict" } else { "" },
                    num_conflicts).as_str())?;
        term.write_line(actions::details(&actions[sel]).as_str())?;
        height += 2;

        for (idx, action) in actions
            .iter()
            .enumerate()
            .skip(page * capacity)
            .take(capacity)
        {
            term.write_line(format!("{} {}",
                     (if sel == idx { ">" } else {" "}).cyan(),
                     action).as_str())?;
            height += 1;
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
                    *actions[sel] = resolve_action(&actions[sel], Resolution::Local);
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            Key::ArrowRight | Key::Char('r') => {
                if actions[sel].is_conflict() {
                    if actions[sel].is_unresolved_conflict() {
                        num_conflicts -= 1;
                    }
                    *actions[sel] = resolve_action(&actions[sel], Resolution::Remote);
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            Key::Char('c') => {
                if actions[sel].is_conflict() {
                    if !actions[sel].is_unresolved_conflict() {
                        match &actions[sel] {
                            Action::ResolvedLocal((lc,rc),_) | Action::ResolvedRemote((lc,rc),_) => {
                                *actions[sel] = Action::Conflict(lc.clone(),rc.clone());
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
