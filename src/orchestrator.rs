use std::io::BufWriter;
use std::path::{Path, PathBuf};

use bincode::serde::encode_into_std_write as serialize_into;
use color_eyre::eyre::{eyre, Result};
use colored::*;
use essrpc::{RPCError, RPCErrorKind};
use openssh::{KnownHosts, Session, SessionBuilder};

use crate::actions::{num_identical, num_unresolved_conflicts, reverse, Action, Actions};
use crate::cli::SyncOptions;
use crate::profile::{self, ProfileSource};
use crate::remote;
use crate::resolution::{self, AllResolution};
use crate::rpc::{self, DuetServerAsync};
use crate::scan;
use crate::state;
use crate::sync as sync_ops;
use crate::utils;

const OK_CODE: u8 = 0;
const ABORT_CODE: u8 = 1;
const PROFILE_ERROR_CODE: u8 = 2;
const SSH_ERROR_CODE: u8 = 3;
const SERVER_ERROR_CODE: u8 = 4;
const CTRLC_CODE: u8 = 6;

struct SyncContext {
    profile: profile::Profile,
    local_id: String,
    local_base: PathBuf,
    remote_base: String,
    remote_server: Option<String>,
    remote_cmd: String,
    path: PathBuf,
    local_state: PathBuf,
    remote_state_dir: Option<PathBuf>,
    server_log: PathBuf,
}

pub async fn sync(source: ProfileSource, path: Option<PathBuf>, options: SyncOptions) -> Result<()> {
    env_logger::init();
    install_ctrlc_handler();

    let SyncContext {
        profile: prf,
        local_id,
        local_base,
        remote_base,
        remote_server,
        remote_cmd,
        path,
        local_state,
        remote_state_dir,
        server_log,
    } = prepare_context(source, path)?;

    let local_fut = state::old_and_changes(
        &local_base,
        &path,
        &prf.locations,
        &prf.ignore,
        Some(&local_state),
    );

    let remote_session = open_remote_session(remote_server).await;
    let mut server = remote::launch_server(&remote_session, remote_cmd, &server_log)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to start server ({})", e.to_string().cyan());
            quit::with_code(SERVER_ERROR_CODE);
        });
    let remote = remote::get_remote(&mut server);

    let remote_path = path.clone();
    let remote_locations = prf.locations.clone();
    let remote_ignore = prf.ignore.clone();
    let remote_fut = async {
        remote
            .set_base(remote_base)
            .await
            .expect("Couldn't set server base");
        if let Some(remote_state_dir) = remote_state_dir {
            let info = remote.server_info().await.map_err(server_info_error)?;
            require_remote_capability(&info, rpc::CAPABILITY_PROFILE_FILE_STATE_DIR)?;
            remote
                .set_remote_state_dir(remote_state_dir)
                .await
                .map_err(remote_state_dir_error)?;
        }
        remote
            .changes(remote_path, remote_locations, remote_ignore, local_id)
            .await
            .map_err(|e| eyre!("Couldn't get remote changes: {:?}", e))
    };

    let (local_result, remote_result) = tokio::join!(local_fut, remote_fut);
    let (mut local_all_old, local_changes) = local_result.expect("Couldn't get local changes");
    let remote_changes = remote_result?;

    let mut actions = build_actions(&local_changes, &remote_changes);
    let resolution = resolve_actions(&mut actions, options)?;

    if let AllResolution::Abort = resolution {
        println!("Aborting");
        quit::with_code(ABORT_CODE);
    }

    log::debug!("synchronizing");

    use std::sync::Arc;
    let actions: Arc<Actions> = Arc::new(
        actions
            .into_iter()
            .filter(|a| !a.is_unresolved_conflict())
            .collect(),
    );
    let remote_actions: Actions = reverse(&actions);
    remote
        .set_actions(remote_actions)
        .await
        .expect("Failed to set remote actions");
    log::debug!("set remote actions");

    let local_signatures_fut = {
        let local_base = local_base.clone();
        let actions = actions.clone();
        tokio::task::spawn_blocking(move || sync_ops::get_signatures(&local_base, &actions))
    };
    let remote_signatures_fut = remote.get_signatures();
    let (local_signatures, remote_signatures) =
        tokio::join!(local_signatures_fut, remote_signatures_fut);
    let local_signatures = local_signatures?.expect("couldn't get local signatures");
    let remote_signatures = remote_signatures.expect("couldn't get remote signatures");
    log::debug!(
        "{} local signatures; {} remote signatures",
        local_signatures.len(),
        remote_signatures.len()
    );

    let local_detailed_changes_fut = {
        let local_base = local_base.clone();
        let actions = actions.clone();
        tokio::task::spawn_blocking(move || {
            sync_ops::get_detailed_changes(&local_base, &actions, &remote_signatures)
        })
    };
    let remote_detailed_changes_fut = remote.get_detailed_changes(local_signatures);
    let (local_detailed_changes, remote_detailed_changes) =
        tokio::join!(local_detailed_changes_fut, remote_detailed_changes_fut);
    let local_detailed_changes =
        local_detailed_changes?.expect("couldn't get local detailed changes");
    let remote_detailed_changes =
        remote_detailed_changes.expect("couldn't get remote detailed changes");
    log::debug!("got detailed changes");

    let local_apply_fut = {
        let local_base = local_base.clone();
        let actions = actions.clone();
        tokio::task::spawn_blocking(move || {
            sync_ops::apply_detailed_changes(
                &local_base,
                &actions,
                &remote_detailed_changes,
                &mut local_all_old,
            )
            .expect("failed to apply local changes");
            local_all_old
        })
    };
    let remote_apply_fut = remote.apply_detailed_changes(local_detailed_changes);
    let (local_apply, remote_apply) = tokio::join!(local_apply_fut, remote_apply_fut);
    let local_all_old = local_apply?;
    let _ = remote_apply?;

    let (remote_result, local_result) = tokio::join!(
        remote.save_state(),
        tokio::task::spawn_blocking(move || {
            use atomicwrites::{AllowOverwrite, AtomicFile};
            let af = AtomicFile::new(local_state, AllowOverwrite);
            af.write(|f| {
                let mut f = BufWriter::new(f);
                serialize_into(&local_all_old, &mut f, bincode::config::legacy())
            })
        })
    );
    let _ = local_result.expect("Failed to save local state");
    let _ = remote_result.expect("Failed to save remote state");

    Ok(())
}

fn install_ctrlc_handler() {
    ctrlc::set_handler(|| {
        eprintln!("\nQuitting");
        quit::with_code(CTRLC_CODE);
    })
    .expect("Error setting Ctrl-C handler");
}

fn prepare_context(source: ProfileSource, path: Option<PathBuf>) -> Result<SyncContext> {
    let config = profile::load(&source).unwrap_or_else(|e| {
        eprintln!(
            "Failed to read profile {} ({})",
            profile_name(&source).yellow(),
            e.to_string().cyan()
        );
        quit::with_code(PROFILE_ERROR_CODE);
    });

    let local_id = local_id(&config.identity);

    let local_base = crate::full(&config.profile.local)?;
    let (remote_base, remote_server, remote_cmd) = remote::parse_remote(&config.profile.remote)?;

    let path = normalize_path(&local_base, &path.unwrap_or(PathBuf::from("")))?;
    println!(
        "Using profile: {} {}",
        config.display_name.cyan(),
        path.display().to_string().yellow()
    );

    let remote_state_dir = match source {
        ProfileSource::Named(_) => None,
        ProfileSource::File(_) => Some(config.remote_state_dir),
    };

    Ok(SyncContext {
        profile: config.profile,
        local_id,
        local_base,
        remote_base,
        remote_server,
        remote_cmd,
        path,
        local_state: config.local_state,
        remote_state_dir,
        server_log: config.server_log,
    })
}

fn remote_state_dir_error(error: RPCError) -> color_eyre::eyre::Report {
    match error.kind {
        RPCErrorKind::TransportEOF | RPCErrorKind::SerializationError => eyre!(
            "remote server does not support --profile-file state isolation; upgrade remote duet ({:?})",
            error
        ),
        _ => eyre!("Couldn't set remote state dir: {:?}", error),
    }
}

fn server_info_error(error: RPCError) -> color_eyre::eyre::Report {
    match error.kind {
        RPCErrorKind::TransportEOF
        | RPCErrorKind::SerializationError
        | RPCErrorKind::UnknownMethod => eyre!(
            "remote server does not support capability negotiation; upgrade remote duet ({:?})",
            error
        ),
        _ => eyre!("Couldn't get remote server info: {:?}", error),
    }
}

fn require_remote_capability(info: &rpc::ServerInfo, capability: &str) -> Result<()> {
    if info.capabilities.iter().any(|c| c == capability) {
        return Ok(());
    }

    Err(eyre!(
        "remote duet {} protocol {} does not support {}; upgrade remote duet",
        info.duet_version,
        info.protocol_version,
        capability
    ))
}

fn profile_name(source: &ProfileSource) -> String {
    match source {
        ProfileSource::Named(name) => name.clone(),
        ProfileSource::File(path) => path.display().to_string(),
    }
}

async fn open_remote_session(remote_server: Option<String>) -> Option<Session> {
    if let Some(server) = remote_server {
        let session_result = SessionBuilder::default()
            .control_directory(std::env::temp_dir())
            .known_hosts_check(KnownHosts::Strict)
            .connect(server)
            .await;
        match session_result {
            Ok(session) => Some(session),
            Err(e) => {
                eprintln!("Unable to get SSH session ({})", e.to_string().cyan());
                log::error!("Unable to get SSH session: {:?}", e);
                quit::with_code(SSH_ERROR_CODE);
            }
        }
    } else {
        None
    }
}

fn build_actions(local_changes: &state::Changes, remote_changes: &state::Changes) -> Actions {
    utils::match_sorted(local_changes.iter(), remote_changes.iter())
        .filter_map(|(lc, rc)| Action::create(lc, rc))
        .collect()
}

fn resolve_actions(actions: &mut Actions, options: SyncOptions) -> Result<AllResolution> {
    let SyncOptions {
        interactive,
        yes,
        dry_run,
        batch,
        force,
        verbose,
    } = options;

    if actions.is_empty() {
        println!("No changes detected");
        quit::with_code(OK_CODE);
    }

    if dry_run {
        resolution::show_actions(&actions, verbose);
        quit::with_code(OK_CODE);
    }

    let num_conflicts = num_unresolved_conflicts(actions.iter());
    let num_identical = num_identical(actions.iter());

    let resolution = if batch {
        resolution::show_actions(&actions, verbose);
        if force {
            AllResolution::Force
        } else if num_conflicts > 0 {
            println!(
                "{} conflicts found; {}\n",
                num_conflicts,
                "aborting".bright_red()
            );
            AllResolution::Abort
        } else {
            AllResolution::Proceed
        }
    } else if interactive && (num_identical < actions.len() || verbose) {
        let resolution = if yes && num_conflicts == 0 {
            AllResolution::Proceed
        } else {
            resolution::resolve_interactive(actions, verbose)?
        };
        resolution::show_actions(&actions, verbose);
        resolution
    } else {
        resolution::show_actions(&actions, verbose);
        if yes && num_conflicts == 0 {
            AllResolution::Proceed
        } else {
            resolution::resolve_sequential(actions, verbose)?
        }
    };

    Ok(resolution)
}

fn normalize_path(local_base: &PathBuf, path: &PathBuf) -> Result<PathBuf> {
    if path.starts_with("./")
        || path.starts_with("../")
        || path == Path::new(".")
        || path == Path::new("..")
    {
        let cwd = std::env::current_dir()?;
        use path_clean::PathClean;
        let path = cwd.join(path).clean();
        return Ok(scan::relative(local_base, &path).to_path_buf());
    }

    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(scan::relative(local_base, &path).to_path_buf())
    } else {
        Ok(path)
    }
}

fn local_id(name: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mid = machine_uid::get().unwrap_or_else(|e| {
        log::warn!("Unable to read machine id: {:?}", e);
        "unknown-machine".to_string()
    });
    let mut s = DefaultHasher::new();
    mid.hash(&mut s);
    name.hash(&mut s);
    format!("{:x}", s.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_leaves_relative_paths_unchanged() {
        let normalized =
            normalize_path(&PathBuf::from("/tmp/duet-base"), &PathBuf::from("sub/path")).unwrap();

        assert_eq!(normalized, PathBuf::from("sub/path"));
    }

    #[test]
    fn normalize_path_makes_absolute_paths_relative_to_base() {
        let normalized = normalize_path(
            &PathBuf::from("/tmp/duet-base"),
            &PathBuf::from("/tmp/duet-base/sub/path"),
        )
        .unwrap();

        assert_eq!(normalized, PathBuf::from("sub/path"));
    }

    #[test]
    fn local_id_is_stable_and_profile_specific() {
        assert_eq!(local_id("work"), local_id("work"));
        assert_ne!(local_id("work"), local_id("personal"));
    }

    #[test]
    fn require_remote_capability_accepts_advertised_capability() {
        let info = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "0.3.2".to_string(),
            capabilities: vec![rpc::CAPABILITY_PROFILE_FILE_STATE_DIR.to_string()],
        };

        require_remote_capability(&info, rpc::CAPABILITY_PROFILE_FILE_STATE_DIR).unwrap();
    }

    #[test]
    fn require_remote_capability_rejects_missing_capability() {
        let info = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "0.3.2".to_string(),
            capabilities: Vec::new(),
        };

        let error = require_remote_capability(&info, rpc::CAPABILITY_PROFILE_FILE_STATE_DIR)
            .unwrap_err()
            .to_string();

        assert!(error.contains("0.3.2"));
        assert!(error.contains(rpc::CAPABILITY_PROFILE_FILE_STATE_DIR));
    }
}
