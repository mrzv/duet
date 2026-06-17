use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bincode::serde::encode_into_std_write as serialize_into;
use color_eyre::eyre::{eyre, Result, WrapErr};
use colored::*;
use essrpc::{RPCError, RPCErrorKind};
use openssh::{KnownHosts, Session, SessionBuilder};

use crate::actions::{num_identical, num_unresolved_conflicts, reverse, Action, Actions};
use crate::cli::SyncOptions;
use crate::performance::{PerformanceProfile, StreamingProfile};
use crate::profile::{self, ProfileSource};
use crate::remote;
use crate::resolution::{self, AllResolution};
use crate::rpc::{self, DuetServerAsync};
use crate::scan::{self, Change};
use crate::state;
use crate::sync as sync_ops;
use crate::sync_error;
use crate::utils;

const OK_CODE: u8 = 0;
const ABORT_CODE: u8 = 1;
const PROFILE_ERROR_CODE: u8 = 2;
const SSH_ERROR_CODE: u8 = 3;
const SERVER_ERROR_CODE: u8 = 4;
const CTRLC_CODE: u8 = 6;
#[cfg(debug_assertions)]
const TEST_PAUSE_AFTER_REMOTE_APPLY_PREPARE_MS: &str =
    "DUET_TEST_PAUSE_AFTER_REMOTE_APPLY_PREPARE_MS";
const POST_PREFLIGHT_RECOVERY_ADVICE: &str = "Recovery: filesystem changes may have been partially applied, but Duet state was not saved. Fix the reported problem, inspect both sides if needed, then rerun duet. If conflicts appear, resolve them manually.";
const STATE_SAVE_RECOVERY_ADVICE: &str = "Recovery: filesystem changes were applied, but Duet state was not saved on both sides. Fix state storage permissions, then rerun duet before making unrelated changes.";

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

pub async fn sync(
    source: ProfileSource,
    path: Option<PathBuf>,
    options: SyncOptions,
) -> Result<()> {
    let total_start = Instant::now();
    let print_performance = options.profile_performance;
    let performance_json = options.profile_performance_json.clone();
    let profiling_enabled = print_performance || performance_json.is_some();
    let mut performance = PerformanceProfile::default();

    let setup_start = Instant::now();
    env_logger::init();
    install_ctrlc_handler()?;

    let context = prepare_context(source, path)?;
    sync_ops::check_apply_attempt_clear(&context.local_state)?;
    performance.record_phase("setup", setup_start.elapsed());

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
    } = context;
    let apply_attempt_id = new_apply_attempt_id(&local_id);

    let local_fut = async {
        let start = Instant::now();
        let result = state::old_and_changes(
            &local_base,
            &path,
            &prf.locations,
            &prf.ignore,
            Some(&local_state),
        )
        .await;
        (result, start.elapsed())
    };

    let remote_setup_start = Instant::now();
    let remote_session = open_remote_session(remote_server).await;
    let mut server = remote::launch_server(&remote_session, remote_cmd, &server_log)
        .await
        .unwrap_or_else(|e| {
            let diagnostic =
                sync_error::render_report("setup", "launch server", Some(server_log.clone()), e);
            eprintln!("{}", diagnostic.cyan());
            quit::with_code(SERVER_ERROR_CODE);
        });
    let remote = remote::get_remote(&mut server)?;
    performance.record_phase("remote_setup", remote_setup_start.elapsed());

    let remote_path = path.clone();
    let remote_locations = prf.locations.clone();
    let remote_ignore = prf.ignore.clone();
    let remote_fut = async {
        let start = Instant::now();
        let result = async {
            remote
                .set_base(remote_base)
                .await
                .map_err(|e| remote_rpc_error("Couldn't set server base", e))?;
            let remote_info = remote.server_info().await.map_err(server_info_error)?;
            if let Some(remote_state_dir) = remote_state_dir {
                require_remote_capability(&remote_info, rpc::CAPABILITY_PROFILE_FILE_STATE_DIR)?;
                remote
                    .set_remote_state_dir(remote_state_dir)
                    .await
                    .map_err(remote_state_dir_error)?;
            }
            let changes = remote
                .changes(remote_path, remote_locations, remote_ignore, local_id)
                .await
                .map_err(|e| remote_rpc_error("Couldn't get remote changes", e))?;
            Ok::<_, color_eyre::eyre::Report>((changes, remote_info))
        }
        .await;
        (result, start.elapsed())
    };

    let (local_result, remote_result) = tokio::join!(local_fut, remote_fut);
    let (local_result, local_scan_duration) = local_result;
    let (remote_result, remote_scan_duration) = remote_result;
    performance.record_phase("local_scan", local_scan_duration);
    performance.record_phase("remote_scan_rpc", remote_scan_duration);
    let (mut local_all_old, local_changes) = local_result?;
    let (remote_changes, remote_info) = remote_result?;

    performance.counters.local_entries = local_all_old.len();
    performance.counters.local_changes = local_changes.len();
    performance.counters.remote_changes = remote_changes.len();
    performance.counters.local_changed_bytes = changed_bytes(&local_changes);
    performance.counters.remote_changed_bytes = changed_bytes(&remote_changes);

    let tuning_start = Instant::now();
    let tuning = negotiate_sync_tuning(&remote, &remote_info).await?;
    performance.record_phase("sync_tuning", tuning_start.elapsed());
    performance.sync_tuning = Some(tuning.normalized());

    let resolve_start = Instant::now();
    let mut actions = build_actions(&local_changes, &remote_changes);
    if options.debug_info {
        show_debug_info(&remote_info, tuning);
    }
    performance.counters.total_actions = actions.len();
    let resolution = resolve_actions(&mut actions, options)?;
    performance.counters.unresolved_conflicts = num_unresolved_conflicts(actions.iter());
    performance.counters.identical_actions = num_identical(actions.iter());
    performance.record_phase("resolve_actions", resolve_start.elapsed());

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
    performance.counters.active_actions = actions.len();

    let preflight_start = Instant::now();
    sync_ops::preflight_state_save(&local_state)?;
    sync_ops::preflight_apply(&local_base, actions.as_ref())?;
    let remote_actions: Actions = reverse(&actions);
    let can_stream_details =
        has_remote_capability(&remote_info, rpc::CAPABILITY_STREAMED_DETAIL_BATCHES)
            && sync_ops::can_stream_details(&actions)
            && sync_ops::can_stream_details(&remote_actions);
    let can_prepare_remote_apply =
        has_remote_capability(&remote_info, rpc::CAPABILITY_APPLY_ATTEMPT_PREPARE);
    let can_prepare_remote_apply_with_id =
        has_remote_capability(&remote_info, rpc::CAPABILITY_APPLY_ATTEMPT_ID);
    if actions_require_creatable_added_parents(&remote_actions) {
        require_remote_capability(&remote_info, rpc::CAPABILITY_CREATABLE_ADDED_PARENTS)?;
    }
    remote
        .set_actions(remote_actions)
        .await
        .map_err(|e| remote_rpc_error("Failed to set remote actions", e))?;
    performance.record_phase("preflight_and_set_actions", preflight_start.elapsed());
    log::debug!("set remote actions");

    let local_signatures_fut = {
        let local_base = local_base.clone();
        let actions = actions.clone();
        let window_config = tuning.signature_window_config();
        tokio::task::spawn_blocking(move || {
            let start = Instant::now();
            let result = sync_ops::get_signatures_with_config(&local_base, &actions, window_config);
            (result, start.elapsed())
        })
    };
    let remote_signatures_fut = async {
        let start = Instant::now();
        let result = remote.get_signatures().await;
        (result, start.elapsed())
    };
    let (local_signatures, remote_signatures) =
        tokio::join!(local_signatures_fut, remote_signatures_fut);
    let (local_signatures, local_signature_duration) =
        local_signatures.wrap_err("local signature task failed")?;
    let local_signatures = local_signatures?;
    let (remote_signatures, remote_signature_duration) = remote_signatures;
    let remote_signatures =
        remote_signatures.map_err(|e| remote_rpc_error("couldn't get remote signatures", e))?;
    performance.record_phase("local_signatures", local_signature_duration);
    performance.record_phase("remote_signatures_rpc", remote_signature_duration);
    performance.counters.local_signatures = local_signatures.len();
    performance.counters.remote_signatures = remote_signatures.len();
    log::debug!(
        "{} local signatures; {} remote signatures",
        local_signatures.len(),
        remote_signatures.len()
    );

    let local_all_old = if can_stream_details {
        log::debug!("streaming detailed changes");
        prepare_remote_apply_attempt(
            &remote,
            can_prepare_remote_apply,
            can_prepare_remote_apply_with_id,
            &apply_attempt_id,
        )
        .await?;
        sync_ops::start_apply_attempt(
            "local",
            &local_state,
            &local_base,
            actions.as_ref(),
            Some(&apply_attempt_id),
        )?;
        let stream_result = stream_detailed_changes(
            &remote,
            &local_base,
            &local_state,
            &actions,
            local_all_old,
            local_signatures,
            remote_signatures,
            tuning,
        )
        .await?;
        performance.record_phase(
            "stream_remote_detail_and_local_apply",
            stream_result.remote_detail_and_local_apply_duration,
        );
        performance.record_phase("stream_remote_detail_rpc", stream_result.remote_detail_duration);
        performance.record_phase("stream_local_apply", stream_result.local_apply_duration);
        performance.record_phase(
            "stream_local_detail_and_remote_apply",
            stream_result.local_detail_and_remote_apply_duration,
        );
        performance.record_phase("stream_local_detail", stream_result.local_detail_duration);
        performance.record_phase("stream_remote_apply_rpc", stream_result.remote_apply_duration);
        performance.counters.streamed_details = true;
        performance.counters.streaming = stream_result.profile;
        stream_result.local_all_old
    } else {
        let local_detailed_changes_fut = {
            let local_base = local_base.clone();
            let actions = actions.clone();
            tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                let result =
                    sync_ops::get_detailed_changes(&local_base, &actions, &remote_signatures);
                (result, start.elapsed())
            })
        };
        let remote_detailed_changes_fut = async {
            let start = Instant::now();
            let result = remote.get_detailed_changes(local_signatures).await;
            (result, start.elapsed())
        };
        let (local_detailed_changes, remote_detailed_changes) =
            tokio::join!(local_detailed_changes_fut, remote_detailed_changes_fut);
        let (local_detailed_changes, local_detail_duration) =
            local_detailed_changes.wrap_err("local detailed changes task failed")?;
        let local_detailed_changes = local_detailed_changes?;
        let (remote_detailed_changes, remote_detail_duration) = remote_detailed_changes;
        let remote_detailed_changes = remote_detailed_changes
            .map_err(|e| remote_rpc_error("couldn't get remote detailed changes", e))?;
        performance.record_phase("local_details", local_detail_duration);
        performance.record_phase("remote_details_rpc", remote_detail_duration);
        log::debug!("got detailed changes");

        prepare_remote_apply_attempt(
            &remote,
            can_prepare_remote_apply,
            can_prepare_remote_apply_with_id,
            &apply_attempt_id,
        )
        .await?;
        sync_ops::start_apply_attempt(
            "local",
            &local_state,
            &local_base,
            actions.as_ref(),
            Some(&apply_attempt_id),
        )?;
        let local_apply_fut = {
            let local_base = local_base.clone();
            let local_state = local_state.clone();
            let actions = actions.clone();
            tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                sync_ops::apply_detailed_changes(
                    &local_base,
                    &actions,
                    &remote_detailed_changes,
                    &mut local_all_old,
                    Some(&local_state),
                )?;
                Ok::<_, color_eyre::eyre::Report>((local_all_old, start.elapsed()))
            })
        };
        let remote_apply_fut = async {
            let start = Instant::now();
            let result = remote.apply_detailed_changes(local_detailed_changes).await;
            (result, start.elapsed())
        };
        let (local_apply, remote_apply) = tokio::join!(local_apply_fut, remote_apply_fut);
        let (remote_apply, remote_apply_duration) = remote_apply;
        let _ = remote_apply
            .map_err(|e| post_preflight_rpc_error("remote apply failed after preflight", e))?;
        let (local_all_old, local_apply_duration) = local_apply
            .wrap_err("local apply task failed")?
            .wrap_err(POST_PREFLIGHT_RECOVERY_ADVICE)?;
        performance.record_phase("local_apply", local_apply_duration);
        performance.record_phase("remote_apply_rpc", remote_apply_duration);
        local_all_old
    };

    sync_ops::mark_apply_attempt_state_save(
        "local",
        &local_state,
        &local_base,
        actions.as_ref(),
        Some(&apply_attempt_id),
    )?;

    let state_save_start = Instant::now();
    let local_state_display = local_state.display().to_string();
    let local_state_for_save = local_state.clone();
    let (remote_result, local_result) = tokio::join!(
        async {
            let start = Instant::now();
            let result = remote.save_state().await;
            (result, start.elapsed())
        },
        tokio::task::spawn_blocking(move || {
            let start = Instant::now();
            use atomicwrites::{AllowOverwrite, AtomicFile};
            let af = AtomicFile::new(local_state_for_save, AllowOverwrite);
            let result = af.write(|f| {
                let mut f = BufWriter::new(f);
                serialize_into(&local_all_old, &mut f, bincode::config::legacy())
            });
            (result, start.elapsed())
        })
    );
    let (local_result, local_state_save_duration) =
        local_result.wrap_err("local state save task failed")?;
    local_result.wrap_err_with(|| {
        format!(
            "failed to save local state {}\n{}",
            local_state_display, STATE_SAVE_RECOVERY_ADVICE
        )
    })?;
    sync_ops::finish_apply_attempt(&local_state)?;
    let (remote_result, remote_state_save_duration) = remote_result;
    remote_result.map_err(|e| post_state_save_rpc_error("failed to save remote state", e))?;
    performance.record_phase("local_state_save", local_state_save_duration);
    performance.record_phase("remote_state_save_rpc", remote_state_save_duration);
    performance.record_phase("state_save_total", state_save_start.elapsed());

    if profiling_enabled {
        performance.finish(total_start.elapsed());
        if print_performance {
            performance.print_human();
        }
        if let Some(path) = performance_json {
            performance.write_json(&path)?;
        }
    }

    Ok(())
}

async fn prepare_remote_apply_attempt<R>(
    remote: &R,
    supported: bool,
    supports_attempt_id: bool,
    attempt_id: &str,
) -> Result<()>
where
    R: DuetServerAsync,
{
    if supported {
        if supports_attempt_id {
            remote
                .prepare_apply_attempt_with_id(attempt_id.to_string())
                .await
                .map_err(|e| remote_rpc_error("Couldn't prepare remote apply recovery", e))?;
        } else {
            remote
                .prepare_apply_attempt()
                .await
                .map_err(|e| remote_rpc_error("Couldn't prepare remote apply recovery", e))?;
        }
        test_pause_after_remote_apply_prepare().await;
    }
    Ok(())
}

async fn negotiate_sync_tuning<R>(
    remote: &R,
    info: &rpc::ServerInfo,
) -> Result<sync_ops::SyncTuning>
where
    R: DuetServerAsync,
{
    if !has_remote_capability(info, rpc::CAPABILITY_SYNC_TUNING) {
        return Ok(sync_ops::SyncTuning::legacy());
    }

    remote
        .negotiate_sync_tuning(sync_ops::SyncTuningRequest::preferred())
        .await
        .map_err(|e| remote_rpc_error("Couldn't negotiate sync tuning", e))
}

fn new_apply_attempt_id(local_id: &str) -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", local_id, std::process::id(), since_epoch)
}

#[cfg(debug_assertions)]
async fn test_pause_after_remote_apply_prepare() {
    let Ok(raw_ms) = std::env::var(TEST_PAUSE_AFTER_REMOTE_APPLY_PREPARE_MS) else {
        return;
    };
    let Ok(ms) = raw_ms.parse::<u64>() else {
        return;
    };
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(not(debug_assertions))]
async fn test_pause_after_remote_apply_prepare() {}

fn install_ctrlc_handler() -> Result<()> {
    ctrlc::set_handler(|| {
        eprintln!("\nQuitting");
        quit::with_code(CTRLC_CODE);
    })
    .wrap_err("failed to install Ctrl-C handler")?;
    Ok(())
}

fn prepare_context(source: ProfileSource, path: Option<PathBuf>) -> Result<SyncContext> {
    let config = profile::load(&source).unwrap_or_else(|e| {
        let diagnostic =
            sync_error::render_error("setup", "load profile", profile_source_path(&source), e);
        eprintln!("{}", diagnostic.cyan());
        quit::with_code(PROFILE_ERROR_CODE);
    });

    let local_id = local_id(&config.identity);

    let local_base = crate::full(&config.profile.local).map_err(|e| {
        eyre!(
            "{}",
            sync_error::render_report(
                "setup",
                "resolve local base",
                Some(PathBuf::from(&config.profile.local)),
                e
            )
        )
    })?;
    let (remote_base, remote_server, remote_cmd) = remote::parse_remote(&config.profile.remote)
        .map_err(|e| {
            eyre!(
                "{}",
                sync_error::render_report("setup", "parse remote", None, e)
            )
        })?;

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
        _ => remote_rpc_error("Couldn't set remote state dir", error),
    }
}

fn remote_rpc_error(context: &str, error: RPCError) -> color_eyre::eyre::Report {
    eyre!("{}: {}", context, sync_error::render_rpc_error(&error))
}

fn post_preflight_rpc_error(context: &str, error: RPCError) -> color_eyre::eyre::Report {
    eyre!(
        "{}: {}\n{}",
        context,
        sync_error::render_rpc_error(&error),
        POST_PREFLIGHT_RECOVERY_ADVICE
    )
}

fn post_state_save_rpc_error(context: &str, error: RPCError) -> color_eyre::eyre::Report {
    eyre!(
        "{}: {}\n{}",
        context,
        sync_error::render_rpc_error(&error),
        STATE_SAVE_RECOVERY_ADVICE
    )
}

struct StreamDetailedChangesResult {
    local_all_old: state::Entries,
    profile: StreamingProfile,
    remote_detail_and_local_apply_duration: Duration,
    local_detail_and_remote_apply_duration: Duration,
    remote_detail_duration: Duration,
    local_apply_duration: Duration,
    local_detail_duration: Duration,
    remote_apply_duration: Duration,
}

async fn stream_detailed_changes<R>(
    remote: &R,
    local_base: &PathBuf,
    local_state: &Path,
    actions: &Actions,
    local_all_old: state::Entries,
    local_signatures: Vec<sync_ops::SignatureWithPath>,
    remote_signatures: Vec<sync_ops::SignatureWithPath>,
    tuning: sync_ops::SyncTuning,
) -> Result<StreamDetailedChangesResult>
where
    R: DuetServerAsync,
{
    let total_transfer_bytes = sync_ops::detail_transfer_bytes(actions);
    let progress = stream_progress_bar(total_transfer_bytes)?;
    let mut progress_position = 0;

    let mut local_producer = sync_ops::DetailProducer::new(
        local_base.clone(),
        actions.clone(),
        remote_signatures,
        tuning.detail_chunk_bytes(),
    );
    let mut local_applier = sync_ops::DetailApplier::new_with_attempt(
        local_base.clone(),
        actions.clone(),
        local_all_old,
        Some(local_state.to_path_buf()),
    );

    let remote_detail_stream = remote
        .begin_detail_stream(local_signatures, tuning.detail_chunk_bytes() as u32)
        .await
        .map_err(|e| remote_rpc_error("Couldn't begin remote detail stream", e))?;
    let remote_apply_stream = remote
        .begin_apply_stream()
        .await
        .map_err(|e| remote_rpc_error("Couldn't begin remote apply stream", e))?;

    let mut local_done = false;
    let mut remote_done = false;
    let mut profile = StreamingProfile::default();
    let mut remote_detail_duration = Duration::default();
    let mut local_apply_duration = Duration::default();
    let mut local_detail_duration = Duration::default();
    let mut remote_apply_duration = Duration::default();
    while !local_done || !remote_done {
        if !remote_done {
            let start = Instant::now();
            let frames = remote
                .next_detail_chunks(
                    remote_detail_stream,
                    tuning.detail_batch_frames() as u32,
                    tuning.detail_batch_payload_bytes() as u32,
                )
                .await
                .map_err(|e| post_preflight_rpc_error("Couldn't read remote detail stream", e))?;
            remote_detail_duration += start.elapsed();
            profile.remote_to_local.record_batch(&frames);
            if frames.is_empty() {
                remote_done = true;
            } else {
                let transfer_bytes = sync_ops::detail_frames_transfer_bytes(&frames);
                let start = Instant::now();
                for frame in frames {
                    local_applier
                        .apply_frame(frame)
                        .wrap_err(POST_PREFLIGHT_RECOVERY_ADVICE)?;
                }
                advance_stream_progress(
                    &progress,
                    &mut progress_position,
                    total_transfer_bytes,
                    transfer_bytes,
                );
                local_apply_duration += start.elapsed();
            }
        }

        if !local_done {
            let start = Instant::now();
            let frames = local_producer
                .next_frames(
                    tuning.detail_batch_frames(),
                    tuning.detail_batch_payload_bytes(),
                )
                .wrap_err(POST_PREFLIGHT_RECOVERY_ADVICE)?;
            local_detail_duration += start.elapsed();
            profile.local_to_remote.record_batch(&frames);
            if frames.is_empty() {
                local_done = true;
            } else {
                let transfer_bytes = sync_ops::detail_frames_transfer_bytes(&frames);
                let start = Instant::now();
                remote
                    .apply_detail_chunks(remote_apply_stream, frames)
                    .await
                    .map_err(|e| {
                        post_preflight_rpc_error("Couldn't apply remote detail stream", e)
                    })?;
                advance_stream_progress(
                    &progress,
                    &mut progress_position,
                    total_transfer_bytes,
                    transfer_bytes,
                );
                remote_apply_duration += start.elapsed();
            }
        }
    }

    let start = Instant::now();
    let local_all_old = local_applier
        .finish()
        .wrap_err(POST_PREFLIGHT_RECOVERY_ADVICE)?;
    local_apply_duration += start.elapsed();
    let start = Instant::now();
    remote
        .finish_apply_stream(remote_apply_stream)
        .await
        .map_err(|e| post_preflight_rpc_error("Couldn't finish remote apply stream", e))?;
    remote_apply_duration += start.elapsed();
    progress.finish_and_clear();
    Ok(StreamDetailedChangesResult {
        local_all_old,
        profile,
        remote_detail_and_local_apply_duration: remote_detail_duration + local_apply_duration,
        local_detail_and_remote_apply_duration: local_detail_duration + remote_apply_duration,
        remote_detail_duration,
        local_apply_duration,
        local_detail_duration,
        remote_apply_duration,
    })
}

fn stream_progress_bar(total_transfer_bytes: u64) -> Result<indicatif::ProgressBar> {
    let progress = indicatif::ProgressBar::new(total_transfer_bytes);
    let style = indicatif::ProgressStyle::default_bar()
        .template("[{elapsed_precise}] {bar:40.cyan/blue} {wide_msg}")?
        .progress_chars("##-");
    progress.set_style(style);
    progress.set_message(format!(
        "streaming changes {} / {}",
        indicatif::HumanBytes(0),
        indicatif::HumanBytes(total_transfer_bytes)
    ));
    Ok(progress)
}

fn advance_stream_progress(
    progress: &indicatif::ProgressBar,
    position: &mut u64,
    total_transfer_bytes: u64,
    transfer_bytes: u64,
) {
    if transfer_bytes == 0 {
        return;
    }

    *position = position.saturating_add(transfer_bytes);
    if total_transfer_bytes > 0 {
        *position = (*position).min(total_transfer_bytes);
    }

    progress.set_position(*position);
    progress.set_message(format!(
        "streaming changes {} / {}",
        indicatif::HumanBytes(*position),
        indicatif::HumanBytes(total_transfer_bytes)
    ));
}

fn has_remote_capability(info: &rpc::ServerInfo, capability: &str) -> bool {
    info.capabilities.iter().any(|c| c == capability)
}

fn agreed_capabilities(info: &rpc::ServerInfo) -> Vec<&'static str> {
    rpc::client_capabilities()
        .iter()
        .copied()
        .filter(|capability| has_remote_capability(info, capability))
        .collect()
}

fn format_capabilities(capabilities: &[impl AsRef<str>]) -> String {
    if capabilities.is_empty() {
        "none".to_string()
    } else {
        capabilities
            .iter()
            .map(|capability| capability.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn show_debug_info(info: &rpc::ServerInfo, tuning: sync_ops::SyncTuning) {
    println!("Debug information:");
    println!("  client protocol: {}", rpc::PROTOCOL_VERSION);
    println!(
        "  client capabilities: {}",
        format_capabilities(rpc::client_capabilities())
    );
    println!("  server version: {}", info.duet_version);
    println!("  server protocol: {}", info.protocol_version);
    println!(
        "  server capabilities: {}",
        format_capabilities(&info.capabilities)
    );
    println!(
        "  agreed capabilities: {}",
        format_capabilities(&agreed_capabilities(info))
    );
    println!("  sync tuning: {}", format_sync_tuning(tuning));
}

fn format_sync_tuning(tuning: sync_ops::SyncTuning) -> String {
    let tuning = tuning.normalized();
    format!(
        "signature-window={}..{} bytes, detail-chunk={}, detail-batch-frames={}, detail-batch-payload={}",
        indicatif::HumanBytes(tuning.signature_window_min as u64),
        indicatif::HumanBytes(tuning.signature_window_max as u64),
        indicatif::HumanBytes(tuning.detail_chunk_bytes as u64),
        tuning.detail_batch_frames,
        indicatif::HumanBytes(tuning.detail_batch_payload_bytes as u64)
    )
}

fn server_info_error(error: RPCError) -> color_eyre::eyre::Report {
    match error.kind {
        RPCErrorKind::TransportEOF
        | RPCErrorKind::SerializationError
        | RPCErrorKind::UnknownMethod => eyre!(
            "remote server does not support capability negotiation; upgrade remote duet ({:?})",
            error
        ),
        _ => remote_rpc_error("Couldn't get remote server info", error),
    }
}

fn require_remote_capability(info: &rpc::ServerInfo, capability: &str) -> Result<()> {
    if has_remote_capability(info, capability) {
        return Ok(());
    }

    Err(eyre!(
        "remote duet {} protocol {} does not support {}; upgrade remote duet",
        info.duet_version,
        info.protocol_version,
        capability
    ))
}

fn actions_require_creatable_added_parents(actions: &Actions) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            Action::Local(Change::Added(_)) | Action::ResolvedLocal((_, _), Change::Added(_))
        )
    })
}

fn profile_source_path(source: &ProfileSource) -> Option<PathBuf> {
    match source {
        ProfileSource::Named(name) => profile::location(name).ok(),
        ProfileSource::File(path) => Some(path.clone()),
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
                let diagnostic = sync_error::render_message(
                    "setup",
                    "open SSH session",
                    None,
                    ssh_diagnostic(&e),
                );
                eprintln!("{}", diagnostic.cyan());
                log::error!("Unable to get SSH session: {:?}", e);
                quit::with_code(SSH_ERROR_CODE);
            }
        }
    } else {
        None
    }
}

fn ssh_diagnostic(error: &openssh::Error) -> String {
    let display = error.to_string();
    let debug = format!("{:?}", error);
    ssh_permission_hint(&display, &debug).unwrap_or(display)
}

fn ssh_permission_hint(display: &str, debug: &str) -> Option<String> {
    let combined = format!("{}\n{}", display, debug).to_lowercase();

    if combined.contains("bad permissions")
        || combined.contains("bad owner or permissions")
        || combined.contains("permissions are too open")
        || combined.contains("unprotected private key")
    {
        return Some(format!(
            "{}. OpenSSH rejected a key or SSH config because its permissions are too open; try `chmod 700 ~/.ssh` and `chmod 600 ~/.ssh/<private-key>`, then retry.",
            display
        ));
    }

    if combined.contains("permission denied") && combined.contains("publickey") {
        return Some(format!(
            "{}. SSH public-key authentication failed; check that the correct key is loaded and that private key permissions are not too open (`chmod 600 ~/.ssh/<private-key>`).",
            display
        ));
    }

    None
}

fn build_actions(local_changes: &state::Changes, remote_changes: &state::Changes) -> Actions {
    utils::match_sorted(local_changes.iter(), remote_changes.iter())
        .filter_map(|(lc, rc)| Action::create(lc, rc))
        .collect()
}

fn changed_bytes(changes: &state::Changes) -> u64 {
    changes
        .iter()
        .map(|change| match change {
            Change::Added(entry) => entry.is_file().then_some(entry.size()).unwrap_or(0),
            Change::Removed(entry) => entry.is_file().then_some(entry.size()).unwrap_or(0),
            Change::Modified(old, new) => {
                if new.is_file() && (!old.is_file() || !old.same_contents(new)) {
                    new.size()
                } else if old.is_file() && !new.is_file() {
                    old.size()
                } else {
                    0
                }
            }
        })
        .sum()
}

fn resolve_actions(actions: &mut Actions, options: SyncOptions) -> Result<AllResolution> {
    let SyncOptions {
        interactive,
        yes,
        dry_run,
        batch,
        force,
        verbose,
        debug_info: _,
        profile_performance: _,
        profile_performance_json: _,
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

    #[test]
    fn added_local_apply_actions_require_creatable_parent_capability() {
        let actions = vec![Action::Local(Change::Added(
            scan::DirEntryWithMeta::test_file(PathBuf::from(".git/objects/0c/object"), 0),
        ))];

        assert!(actions_require_creatable_added_parents(&actions));
    }

    #[test]
    fn removals_do_not_require_creatable_parent_capability() {
        let actions = vec![Action::Local(Change::Removed(
            scan::DirEntryWithMeta::test_file(PathBuf::from("removed.txt"), 0),
        ))];

        assert!(!actions_require_creatable_added_parents(&actions));
    }

    #[test]
    fn agreed_capabilities_intersects_client_and_server_capabilities() {
        let info = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "0.3.2".to_string(),
            capabilities: vec![
                rpc::CAPABILITY_STREAMED_DETAILS.to_string(),
                "server-only".to_string(),
            ],
        };

        assert_eq!(
            agreed_capabilities(&info),
            vec![rpc::CAPABILITY_STREAMED_DETAILS]
        );
    }

    #[test]
    fn format_capabilities_reports_none_for_empty_list() {
        let capabilities: [&str; 0] = [];

        assert_eq!(format_capabilities(&capabilities), "none");
    }

    #[test]
    fn ssh_permission_diagnostic_mentions_chmod_hint() {
        let diagnostic = ssh_permission_hint(
            "Bad owner or permissions on /home/user/.ssh/config",
            "ignored",
        )
        .unwrap();

        assert!(diagnostic.contains("chmod 700 ~/.ssh"));
        assert!(diagnostic.contains("chmod 600 ~/.ssh/<private-key>"));
    }
}
