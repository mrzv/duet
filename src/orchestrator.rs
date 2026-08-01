use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{eyre, Result, WrapErr};
use colored::*;
use essrpc::{RPCError, RPCErrorKind};
use openssh::{ControlPersist, KnownHosts, Session, SessionBuilder};

use crate::actions::{num_identical, num_unresolved_conflicts, reverse, Action, Actions};
use crate::cli::SyncOptions;
use crate::performance::{
    duration_ms, DetailTransferStats, PerformanceProfile, StagingProfile, StreamingProfile,
};
use crate::profile::{self, ProfileSource};
use crate::progress;
use crate::remote;
use crate::resolution::{self, AllResolution};
use crate::rpc::{self, DuetServerAsync};
use crate::scan::{self, Change};
use crate::state;
use crate::sync as sync_ops;
use crate::sync_error;
use crate::utils;

const PROFILE_ERROR_CODE: u8 = 2;
const SSH_ERROR_CODE: u8 = 3;
const SERVER_ERROR_CODE: u8 = 4;
const CTRLC_CODE: u8 = 6;
#[cfg(debug_assertions)]
const TEST_PAUSE_AFTER_REMOTE_APPLY_PREPARE_MS: &str =
    "DUET_TEST_PAUSE_AFTER_REMOTE_APPLY_PREPARE_MS";
const TEST_PAUSE_AFTER_STAGED_PREPARE_MS: &str = "DUET_TEST_PAUSE_AFTER_STAGED_PREPARE_MS";
const TEST_PAUSE_AFTER_STAGED_COMMIT_MS: &str = "DUET_TEST_PAUSE_AFTER_STAGED_COMMIT_MS";
const POST_PREFLIGHT_RECOVERY_ADVICE: &str = "Recovery: filesystem changes may have been partially applied, but Duet state was not saved. Inspect and reconcile both synchronized trees and snapshots before explicitly clearing the recovery markers; do not rerun sync against stale snapshots.";
const STATE_SAVE_RECOVERY_ADVICE: &str = "Recovery: filesystem changes were applied, but Duet state was not saved on both sides. Inspect and reconcile both synchronized trees and snapshots before explicitly clearing the recovery markers; do not rerun sync against stale snapshots.";
const MAX_NON_STREAMED_DETAIL_BYTES: u64 = 64 * 1024 * 1024;
const FILE_BYTE_CHUNK_RPC_THRESHOLD: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncOutcome {
    Success,
    UserAbort,
    Interrupted,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptPhase {
    PreCommit = 0,
    CancelRequested = 1,
    Committed = 2,
    CommittedInterrupted = 3,
    Complete = 4,
    CompleteInterrupted = 5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptRequest {
    Cancel,
    Deferred,
    Force,
}

#[derive(Clone)]
struct InterruptState {
    phase: Arc<AtomicU8>,
    local_server_process_group: Arc<AtomicI32>,
}

impl InterruptState {
    fn new() -> Self {
        Self {
            phase: Arc::new(AtomicU8::new(InterruptPhase::PreCommit as u8)),
            local_server_process_group: Arc::new(AtomicI32::new(0)),
        }
    }

    fn request_interrupt(&self) -> InterruptRequest {
        loop {
            let phase = self.phase.load(Ordering::SeqCst);
            let (next, result) = match phase {
                x if x == InterruptPhase::PreCommit as u8 => {
                    (InterruptPhase::CancelRequested, InterruptRequest::Cancel)
                }
                x if x == InterruptPhase::Committed as u8 => (
                    InterruptPhase::CommittedInterrupted,
                    InterruptRequest::Deferred,
                ),
                x if x == InterruptPhase::Complete as u8 => (
                    InterruptPhase::CompleteInterrupted,
                    InterruptRequest::Deferred,
                ),
                x if x == InterruptPhase::CancelRequested as u8
                    || x == InterruptPhase::CommittedInterrupted as u8
                    || x == InterruptPhase::CompleteInterrupted as u8 =>
                {
                    return InterruptRequest::Force;
                }
                _ => unreachable!("invalid interrupt phase"),
            };
            if self
                .phase
                .compare_exchange(phase, next as u8, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return result;
            }
        }
    }

    fn is_cancel_requested(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == InterruptPhase::CancelRequested as u8
    }

    fn try_begin_commit(&self) -> bool {
        self.phase
            .compare_exchange(
                InterruptPhase::PreCommit as u8,
                InterruptPhase::Committed as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    fn try_reset_after_checkpoint(&self) -> bool {
        match self.phase.compare_exchange(
            InterruptPhase::Committed as u8,
            InterruptPhase::PreCommit as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => !self.is_cancel_requested(),
            Err(phase) if phase == InterruptPhase::CommittedInterrupted as u8 => false,
            Err(phase) if phase == InterruptPhase::CancelRequested as u8 => false,
            Err(phase) => unreachable!("invalid interrupt phase at checkpoint: {}", phase),
        }
    }

    fn complete(&self) -> bool {
        loop {
            let phase = self.phase.load(Ordering::SeqCst);
            let next = match phase {
                x if x == InterruptPhase::PreCommit as u8
                    || x == InterruptPhase::Committed as u8 =>
                {
                    InterruptPhase::Complete
                }
                x if x == InterruptPhase::CommittedInterrupted as u8 => {
                    InterruptPhase::CompleteInterrupted
                }
                x if x == InterruptPhase::CancelRequested as u8 => return true,
                _ => return false,
            };
            if self
                .phase
                .compare_exchange(phase, next as u8, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return false;
            }
        }
    }

    #[cfg(unix)]
    fn register_local_server(&self, server: &remote::Server<'_>) {
        self.local_server_process_group
            .store(server.local_process_group().unwrap_or(0), Ordering::SeqCst);
    }

    #[cfg(not(unix))]
    fn register_local_server(&self, _server: &remote::Server<'_>) {}

    fn clear_local_server(&self) {
        self.local_server_process_group.store(0, Ordering::SeqCst);
    }

    #[cfg(unix)]
    fn force_stop_local_server(&self) {
        let process_group = self.local_server_process_group.load(Ordering::SeqCst);
        if process_group > 0 {
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }

    #[cfg(not(unix))]
    fn force_stop_local_server(&self) {}
}

struct SyncContext {
    profile: profile::Profile,
    local_id: String,
    legacy_local_id: Option<String>,
    local_base: PathBuf,
    remote_base: String,
    remote_server: Option<String>,
    remote_cmd: String,
    scope: scan::ScanScope,
    local_state: PathBuf,
    remote_state_dir: Option<PathBuf>,
    server_log: PathBuf,
}

struct LocalIds {
    stable: String,
    legacy: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyStrategy {
    StagedStream,
    LegacyStream,
    LegacyNonStream,
}

impl ApplyStrategy {
    fn is_staged(self) -> bool {
        matches!(self, Self::StagedStream)
    }
}

pub async fn sync(
    source: ProfileSource,
    path: Option<PathBuf>,
    options: SyncOptions,
) -> Result<SyncOutcome> {
    let interrupt = InterruptState::new();
    install_ctrlc_handler(interrupt.clone())?;
    let total_start = Instant::now();
    let print_performance = options.profile_performance;
    let performance_json = options.profile_performance_json.clone();
    let profiling_enabled = print_performance || performance_json.is_some();
    let mut performance = PerformanceProfile::default();

    let setup_start = Instant::now();
    env_logger::init();

    let context = prepare_context(source, path, &options.excludes)?;
    sync_ops::check_apply_attempt_clear(&context.local_state)?;
    performance.record_phase("setup", setup_start.elapsed());
    if interrupt.is_cancel_requested() {
        return Ok(SyncOutcome::Interrupted);
    }

    let SyncContext {
        profile: prf,
        local_id,
        legacy_local_id,
        local_base,
        remote_base,
        remote_server,
        remote_cmd,
        scope,
        local_state,
        remote_state_dir,
        server_log,
    } = context;
    let apply_attempt_id = new_apply_attempt_id(&local_id);
    let locations = outbound_scan_locations(&prf.locations);
    let scan_ignore = prf.scan_ignore();
    let scan_policy =
        sync_ops::ScanPolicy::with_prune(locations.clone(), prf.ignore.clone(), prf.prune.clone())
            .with_excludes(scope.excludes.clone());
    let mut apply_options = sync_ops::ApplyOptions {
        prune_ignored: options.prune_ignored,
    };

    let remote_setup_start = Instant::now();
    let remote_session = open_remote_session(remote_server).await;
    if interrupt.is_cancel_requested() {
        return Ok(SyncOutcome::Interrupted);
    }
    let mut server = remote::launch_server(&remote_session, remote_cmd, &server_log)
        .await
        .unwrap_or_else(|e| {
            let diagnostic =
                sync_error::render_report("setup", "launch server", Some(server_log.clone()), e);
            eprintln!("{}", diagnostic.cyan());
            quit::with_code(SERVER_ERROR_CODE);
        });
    interrupt.register_local_server(&server);
    let sync_result = async {
        let remote = remote::get_remote(&mut server)?;
        if interrupt.is_cancel_requested() {
            return Ok(SyncOutcome::Interrupted);
        }
        remote
        .set_base(remote_base)
        .await
        .map_err(|e| remote_rpc_error("Couldn't set server base", e))?;
    let remote_info = remote.server_info().await.map_err(server_info_error)?;
    if !scope.excludes.is_empty() {
        require_remote_capability(&remote_info, rpc::CAPABILITY_SCAN_EXCLUDES)?;
    }
    if !prf.prune.is_empty() {
        require_remote_capability(&remote_info, rpc::CAPABILITY_PRUNE_PATTERNS)?;
        remote.set_prune_patterns(prf.prune.clone()).await
            .map_err(|e| remote_rpc_error("Couldn't set remote prune patterns", e))?;
    }
    if let Some(remote_state_dir) = remote_state_dir {
        require_remote_capability(&remote_info, rpc::CAPABILITY_PROFILE_FILE_STATE_DIR)?;
        remote.set_remote_state_dir(remote_state_dir).await.map_err(remote_state_dir_error)?;
    }
    let remote_id = select_remote_state_id(&remote, &remote_info, local_id, legacy_local_id).await?;
    let strong = has_remote_capability(&remote_info, rpc::CAPABILITY_CONTENT_DIGEST_BLAKE2B256);
    if !strong {
        eprintln!("Warning: peer lacks {}; Adler-32 compatibility fallback is active and snapshots will be saved as legacy V1.", rpc::CAPABILITY_CONTENT_DIGEST_BLAKE2B256);
    }
    performance.record_phase("remote_setup", remote_setup_start.elapsed());
    if interrupt.is_cancel_requested() {
        return Ok(SyncOutcome::Interrupted);
    }

    let local_fut = async {
        let start = Instant::now();
        let result = state::old_and_changes(&local_base, &scope, &locations, &scan_ignore, Some(&local_state), strong).await;
        (result, start.elapsed())
    };
    let remote_scope = scope.clone();
    let remote_locations = locations.clone();
    let remote_ignore = scan_ignore.clone();
    let remote_fut = async {
        let start = Instant::now();
        let result = if !remote_scope.excludes.is_empty() {
            remote.changes_scope(remote_scope, remote_locations, remote_ignore, remote_id.clone(), strong).await
                .map_err(|e| remote_rpc_error("Couldn't get remote scoped changes", e))
        } else if strong {
            remote.changes_v2(remote_scope.restrict, remote_locations, remote_ignore, remote_id.clone()).await
                .map_err(|e| remote_rpc_error("Couldn't get remote V2 changes", e))
        } else {
            remote.changes(remote_scope.restrict, remote_locations, remote_ignore, remote_id.clone()).await
                .map(|changes| state::ChangesV2 {
                    changes: changes.into_iter().map(Into::into).collect(),
                    current: Vec::new(),
                    migration_needed: false,
                })
                .map_err(|e| remote_rpc_error("Couldn't get remote changes", e))
        };
        (result, start.elapsed())
    };

    let (local_result, remote_result) = tokio::join!(local_fut, remote_fut);
    let (local_result, local_scan_duration) = local_result;
    let (remote_result, remote_scan_duration) = remote_result;
    performance.record_phase("local_scan", local_scan_duration);
    performance.record_phase("remote_scan_rpc", remote_scan_duration);
    let local_context = local_result?;
    let remote_context = remote_result?;
    if interrupt.is_cancel_requested() {
        return Ok(SyncOutcome::Interrupted);
    }
    let mut local_all_old = local_context.all_old;
    let local_changes = local_context.changes;
    let remote_changes = remote_context.changes;

    performance.counters.local_entries = local_all_old.len();
    performance.counters.local_changes = local_changes.len();
    performance.counters.remote_changes = remote_changes.len();
    performance.counters.local_changed_bytes = changed_bytes(&local_changes);
    performance.counters.remote_changed_bytes = changed_bytes(&remote_changes);

    let tuning_start = Instant::now();
    let tuning = negotiate_sync_tuning(&remote, &remote_info).await?;
    performance.record_phase("sync_tuning", tuning_start.elapsed());
    performance.sync_tuning = Some(tuning.normalized());
    if interrupt.is_cancel_requested() {
        return Ok(SyncOutcome::Interrupted);
    }

    let resolve_start = Instant::now();
    let migration = strong && (local_context.migration_needed || remote_context.migration_needed);
    let mut actions = if migration {
        state::replace_scope(&mut local_all_old, &scope, &local_context.current);
        remote.prepare_migration_v2().await
            .map_err(|e| remote_rpc_error("Couldn't prepare remote strong-digest migration", e))?;
        build_migration_actions(
            &local_changes,
            &remote_changes,
            &local_context.current,
            &remote_context.current,
        )
    } else {
        build_actions(&local_changes, &remote_changes, strong)
    };
    if options.debug_info {
        show_debug_info(&remote_info, tuning);
    }
    performance.counters.total_actions = actions.len();
    let resolution = if options.dry_run {
        show_dry_run_actions(&actions, options.verbose);
        AllResolution::Proceed
    } else if migration && actions.is_empty() {
        println!("Migrating synchronized state to strong content digests");
        AllResolution::Proceed
    } else {
        resolve_actions(&mut actions, options.clone())?
    };
    performance.counters.unresolved_conflicts = num_unresolved_conflicts(actions.iter());
    performance.counters.identical_actions = num_identical(actions.iter());
    performance.record_phase("resolve_actions", resolve_start.elapsed());

    if resolution == AllResolution::Interrupted {
        handle_interrupt_request(&interrupt);
        return Ok(SyncOutcome::Interrupted);
    }
    if interrupt.is_cancel_requested() {
        return Ok(SyncOutcome::Interrupted);
    }

    if migration && performance.counters.unresolved_conflicts > 0 && !options.dry_run {
        return Err(eyre!(
            "strong-digest migration found divergent current files; resolve every conflict before migration can save a shared baseline"
        ));
    }

    if let AllResolution::Abort = resolution {
        println!("Aborting");
        return Ok(SyncOutcome::UserAbort);
    }

    if strong {
        sync_ops::validate_strong_actions(&actions)?;
    }

    if actions.is_empty() && !options.dry_run {
        return Ok(SyncOutcome::Success);
    }

    if options.dry_run && actions.is_empty() {
        performance.counters.active_actions = 0;
        finish_dry_run(
            performance.counters.total_actions,
            performance.counters.active_actions,
            performance.counters.unresolved_conflicts,
        );
        if profiling_enabled {
            performance.finish(total_start.elapsed());
            if print_performance {
                performance.print_human();
            }
            if let Some(path) = performance_json {
                performance.write_json(&path)?;
            }
        }
        return Ok(SyncOutcome::Success);
    }

    log::debug!("synchronizing");

    let actions: Arc<Actions> = Arc::new(
        actions
            .into_iter()
            .filter(|a| !a.is_unresolved_conflict())
            .collect(),
    );
    performance.counters.active_actions = actions.len();

    if options.dry_run && actions.is_empty() {
        finish_dry_run(
            performance.counters.total_actions,
            performance.counters.active_actions,
            performance.counters.unresolved_conflicts,
        );
        if profiling_enabled {
            performance.finish(total_start.elapsed());
            if print_performance {
                performance.print_human();
            }
            if let Some(path) = performance_json {
                performance.write_json(&path)?;
            }
        }
        return Ok(SyncOutcome::Success);
    }

    let preflight_start = Instant::now();
    let remote_actions: Actions = reverse(&actions);
    apply_options = resolve_removal_blockers(
        &remote,
        &remote_info,
        &local_base,
        actions.as_ref(),
        &remote_actions,
        &scan_policy,
        apply_options,
    )
    .await?;
    sync_ops::preflight_state_save(&local_state)?;
    sync_ops::preflight_apply_with_policy(
        &local_base,
        actions.as_ref(),
        Some(&scan_policy),
        apply_options,
    )?;
    let can_stream_details =
        has_remote_capability(&remote_info, rpc::CAPABILITY_STREAMED_DETAIL_BATCHES)
            && sync_ops::can_stream_details(&actions)
            && sync_ops::can_stream_details(&remote_actions);
    let mut apply_strategy = select_apply_strategy(
        &remote_info,
        can_stream_details,
        options.staging_policy_explicit,
    )?;
    if apply_strategy == ApplyStrategy::StagedStream
        && actions_have_directory_to_nondirectory_change(actions.as_ref())
    {
        if options.staging_policy_explicit {
            return Err(eyre!(
                "--staging-limit/--staging-reserve cannot currently be enforced for directory-to-nondirectory replacements"
            ));
        }
        log::debug!("using legacy stream for a directory-to-nondirectory replacement");
        apply_strategy = ApplyStrategy::LegacyStream;
    }
    if !can_stream_details {
        preflight_non_streamed_detail_size(actions.as_ref(), &remote_actions)?;
    }
    let can_prepare_remote_apply =
        has_remote_capability(&remote_info, rpc::CAPABILITY_APPLY_ATTEMPT_PREPARE);
    let can_prepare_remote_apply_with_id =
        has_remote_capability(&remote_info, rpc::CAPABILITY_APPLY_ATTEMPT_ID);
    if actions_require_creatable_added_parents(&remote_actions) {
        require_remote_capability(&remote_info, rpc::CAPABILITY_CREATABLE_ADDED_PARENTS)?;
    }
    if apply_options.prune_ignored {
        require_remote_capability(&remote_info, rpc::CAPABILITY_APPLY_OPTIONS)?;
    }
    let staging_plan = if apply_strategy == ApplyStrategy::StagedStream {
        require_remote_capability(&remote_info, rpc::CAPABILITY_STAGING_CAPACITY)?;
        require_remote_capability(
            &remote_info,
            rpc::CAPABILITY_STAGING_RESERVE_ENFORCEMENT,
        )?;
        remote
            .set_staging_policy(options.staging_policy)
            .await
            .map_err(|e| remote_rpc_error("Couldn't set remote staging policy", e))?;
        let local_filesystem = sync_ops::staging_filesystem_info_with_clone_probe(&local_base)?;
        let remote_filesystem = remote
            .staging_filesystem_info()
            .await
            .map_err(|e| remote_rpc_error("Couldn't get remote staging capacity", e))?;
        let local_budget = options.staging_policy.budget(local_filesystem);
        let remote_budget = options.staging_policy.budget(remote_filesystem);
        let remote_inode_capacity_known = has_remote_capability(
            &remote_info,
            rpc::CAPABILITY_STAGING_INODE_CAPACITY,
        );
        match sync_ops::plan_staging_waves(actions.as_ref(), local_budget, remote_budget) {
            Ok(plan) => {
                for (wave_index, wave) in plan.waves.iter().enumerate() {
                    validate_wave_capacity(
                        wave,
                        options.staging_policy,
                        local_filesystem,
                        remote_filesystem,
                        remote_inode_capacity_known,
                        wave_index,
                        plan.waves.len(),
                    )?;
                }
                if !can_use_staging_plan(
                    &remote_info,
                    plan.waves.len(),
                    migration,
                    options.staging_policy_explicit,
                )? {
                    log::debug!(
                        "falling back to legacy streaming because checkpointed staging is unavailable"
                    );
                    apply_strategy = ApplyStrategy::LegacyStream;
                    None
                } else {
                    performance.counters.staging = Some(StagingProfile {
                        wave_count: plan.waves.len(),
                        local_reconstructed_bytes: plan.local_reconstructed_bytes,
                        remote_reconstructed_bytes: plan.remote_reconstructed_bytes,
                        local_staged_regular_outputs: plan.local_staged_regular_outputs,
                        remote_staged_regular_outputs: plan.remote_staged_regular_outputs,
                        local_budget_bytes: local_budget.budget_bytes,
                        remote_budget_bytes: remote_budget.budget_bytes,
                        local_usable_bytes: local_budget.usable_bytes,
                        remote_usable_bytes: remote_budget.usable_bytes,
                        local_reserve_bytes: local_budget.reserve_bytes,
                        remote_reserve_bytes: remote_budget.reserve_bytes,
                        local_cow_clone_supported: local_budget.cow_clone_supported,
                        remote_cow_clone_supported: remote_budget.cow_clone_supported,
                        local_cow_oversize_waves: plan
                            .waves
                            .iter()
                            .filter(|wave| wave.local_requires_cow_capacity)
                            .count(),
                        remote_cow_oversize_waves: plan
                            .waves
                            .iter()
                            .filter(|wave| wave.remote_requires_cow_capacity)
                            .count(),
                    });
                    if options.dry_run {
                        print_staging_plan_summary(&plan, local_budget, remote_budget);
                    }
                    Some(plan)
                }
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    if options.dry_run {
        require_remote_capability(&remote_info, rpc::CAPABILITY_PREFLIGHT_APPLY)?;
        if strong {
            remote.preflight_apply_v2(remote_actions, apply_options).await
                .map_err(|e| remote_rpc_error("Failed to preflight remote apply", e))?;
        } else {
            remote.preflight_apply(crate::actions::to_legacy(remote_actions), apply_options).await
                .map_err(|e| remote_rpc_error("Failed to preflight remote apply", e))?;
        }
        if interrupt.is_cancel_requested() {
            return Ok(SyncOutcome::Interrupted);
        }
        performance.record_phase("preflight_and_set_actions", preflight_start.elapsed());
        finish_dry_run(
            performance.counters.total_actions,
            performance.counters.active_actions,
            performance.counters.unresolved_conflicts,
        );
        if profiling_enabled {
            performance.finish(total_start.elapsed());
            if print_performance {
                performance.print_human();
            }
            if let Some(path) = performance_json {
                performance.write_json(&path)?;
            }
        }
        return Ok(SyncOutcome::Success);
    }
    if apply_options.prune_ignored {
        remote
            .set_apply_options(apply_options)
            .await
            .map_err(|e| remote_rpc_error("Failed to set remote apply options", e))?;
    }
    if !apply_strategy.is_staged() {
        set_remote_actions(&remote, remote_actions, strong).await?;
    }
    performance.record_phase("preflight_and_set_actions", preflight_start.elapsed());
    log::debug!("set remote actions");
    if interrupt.is_cancel_requested() {
        return Ok(SyncOutcome::Interrupted);
    }

    let (local_signatures, remote_signatures) = if apply_strategy.is_staged() {
        (Vec::new(), Vec::new())
    } else {
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
    if interrupt.is_cancel_requested() {
        return Ok(SyncOutcome::Interrupted);
    }
    (local_signatures, remote_signatures)
    };

    let local_all_old = if apply_strategy == ApplyStrategy::StagedStream {
        let plan = staging_plan
            .as_ref()
            .expect("staged strategy must have a staging plan");
        let mut checkpoint_entries = local_all_old;
        for (wave_index, wave) in plan.waves.iter().enumerate() {
            if interrupt.is_cancel_requested() {
                return Ok(SyncOutcome::Interrupted);
            }
            let local_filesystem = sync_ops::staging_filesystem_info(&local_base)?;
            let remote_filesystem = remote
                .staging_filesystem_info()
                .await
                .map_err(|e| remote_rpc_error("Couldn't recheck remote staging capacity", e))?;
            validate_wave_capacity(
                wave,
                options.staging_policy,
                local_filesystem,
                remote_filesystem,
                has_remote_capability(&remote_info, rpc::CAPABILITY_STAGING_INODE_CAPACITY),
                wave_index,
                plan.waves.len(),
            )?;

            let wave_actions: Arc<Actions> = Arc::new(
                wave.action_indices
                    .iter()
                    .map(|&index| actions[index].clone())
                    .collect(),
            );
            let remote_wave_actions = reverse(&wave_actions);
            let wave_attempt_id = format!(
                "{}-wave-{}-of-{}",
                apply_attempt_id,
                wave_index + 1,
                plan.waves.len()
            );
            let set_actions_start = Instant::now();
            set_remote_actions(&remote, remote_wave_actions, strong).await?;
            record_phase_aggregate(
                &mut performance,
                "remote_set_actions_rpc",
                set_actions_start.elapsed(),
            );

            let local_signatures_fut = {
                let local_base = local_base.clone();
                let wave_actions = wave_actions.clone();
                let window_config = tuning.signature_window_config();
                tokio::task::spawn_blocking(move || {
                    let start = Instant::now();
                    let result = sync_ops::get_signatures_with_config(
                        &local_base,
                        &wave_actions,
                        window_config,
                    );
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
            let remote_signatures = remote_signatures
                .map_err(|e| remote_rpc_error("couldn't get remote signatures", e))?;
            record_phase_aggregate(
                &mut performance,
                "local_signatures",
                local_signature_duration,
            );
            record_phase_aggregate(
                &mut performance,
                "remote_signatures_rpc",
                remote_signature_duration,
            );
            performance.counters.local_signatures += local_signatures.len();
            performance.counters.remote_signatures += remote_signatures.len();

            let prepare_start = Instant::now();
            log::debug!(
                "preparing staged detailed changes for wave {}/{}",
                wave_index + 1,
                plan.waves.len()
            );
            let stream_result = stream_detailed_changes(
                &remote,
                &local_base,
                &local_state,
                &wave_actions,
                checkpoint_entries,
                local_signatures,
                remote_signatures,
                tuning,
                Some(scan_policy.clone()),
                apply_options,
                remote_stream_performance_enabled(profiling_enabled, &remote_info),
                has_remote_capability(&remote_info, rpc::CAPABILITY_FILE_BYTE_CHUNKS),
                Some(&wave_attempt_id),
                Some(options.staging_policy),
                Some(&interrupt),
            )
            .await?;
            let StreamDetailedChangesRun::Complete(mut stream_result) = stream_result else {
                return Ok(SyncOutcome::Interrupted);
            };
            record_stream_performance(&mut performance, &mut stream_result);
            let StreamApplyOutcome::Staged {
                prepared,
                local_report,
                remote_report,
            } = stream_result.outcome
            else {
                unreachable!("staged stream returned legacy outcome");
            };
            if let Err(error) = validate_staged_prepare_barrier(
                &local_report,
                wave_actions.len(),
                &remote_report,
                wave_actions.len(),
            ) {
                let local_cleanup = prepared.abort().err();
                let remote_cleanup = remote
                    .abort_staged_apply(wave_attempt_id.clone())
                    .await
                    .err();
                return Err(add_staged_cleanup_context(
                    error,
                    local_cleanup,
                    remote_cleanup,
                ));
            }
            record_phase_aggregate(
                &mut performance,
                "staged_prepare_transfer",
                prepare_start.elapsed(),
            );

            test_pause_until_interrupt(TEST_PAUSE_AFTER_STAGED_PREPARE_MS, &interrupt).await;
            let local_validation = tokio::task::spawn_blocking(move || {
                let result = prepared.validate_commit();
                (prepared, result)
            });
            let remote_validation = remote.validate_staged_apply(wave_attempt_id.clone());
            let (local_validation, remote_validation) =
                tokio::join!(local_validation, remote_validation);
            let (prepared, local_validation) = match local_validation {
                Ok(validation) => validation,
                Err(error) => {
                    let local_cleanup = sync_ops::abort_staged_apply_attempt(
                        &local_state,
                        &wave_attempt_id,
                    )
                    .err();
                    let remote_cleanup = remote
                        .abort_staged_apply(wave_attempt_id.clone())
                        .await
                        .err();
                    return Err(add_staged_cleanup_context(
                        eyre!("local staged validation task failed: {}", error),
                        local_cleanup,
                        remote_cleanup,
                    ));
                }
            };
            if let Err(error) = local_validation {
                let local_cleanup = prepared.abort().err();
                let remote_cleanup = remote
                    .abort_staged_apply(wave_attempt_id.clone())
                    .await
                    .err();
                return Err(add_staged_cleanup_context(
                    error,
                    local_cleanup,
                    remote_cleanup,
                ));
            }
            if let Err(error) = remote_validation {
                let local_cleanup = prepared.abort().err();
                let remote_cleanup = remote
                    .abort_staged_apply(wave_attempt_id.clone())
                    .await
                    .err();
                return Err(add_staged_cleanup_context(
                    remote_rpc_error("remote staged validation failed", error),
                    local_cleanup,
                    remote_cleanup,
                ));
            }

            if !interrupt.try_begin_commit() {
                let local_cleanup = prepared.abort().err();
                let remote_cleanup = remote
                    .abort_staged_apply(wave_attempt_id.clone())
                    .await
                    .err();
                if local_cleanup.is_some() || remote_cleanup.is_some() {
                    return Err(add_staged_cleanup_context(
                        eyre!("sync interrupted before staged commit"),
                        local_cleanup,
                        remote_cleanup,
                    ));
                }
                return Ok(SyncOutcome::Interrupted);
            }

            let commit_start = Instant::now();
            let local_commit = tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                let result = prepared.commit();
                (result, start.elapsed())
            });
            let remote_commit = async {
                let start = Instant::now();
                let result = remote.commit_staged_apply(wave_attempt_id.clone()).await;
                (result, start.elapsed())
            };
            // This join is the bilateral commit barrier. Once entered, both commits must be awaited.
            let (local_commit, remote_commit) = tokio::join!(local_commit, remote_commit);
            let (local_commit, local_commit_duration) = match local_commit {
                Ok(result) => result,
                Err(error) => (
                    Err(eyre!("local staged commit task failed: {}", error)),
                    Duration::default(),
                ),
            };
            let (remote_commit, remote_commit_duration) = remote_commit;
            checkpoint_entries = finish_staged_commit(local_commit, remote_commit)?;
            record_phase_aggregate(
                &mut performance,
                "staged_local_commit",
                local_commit_duration,
            );
            record_phase_aggregate(
                &mut performance,
                "staged_remote_commit_rpc",
                remote_commit_duration,
            );
            record_phase_aggregate(
                &mut performance,
                "staged_commit",
                commit_start.elapsed(),
            );
            test_pause_until_interrupt(TEST_PAUSE_AFTER_STAGED_COMMIT_MS, &interrupt).await;

            sync_ops::mark_staged_apply_attempt_state_save(&local_state, &wave_attempt_id)?;
            let state_save_start = Instant::now();
            let local_state_display = local_state.display().to_string();
            let local_state_for_save = local_state.clone();
            let entries_for_save = checkpoint_entries.clone();
            let (remote_result, local_result) = tokio::join!(
                async {
                    let start = Instant::now();
                    let result = remote
                        .save_staged_state_pending(wave_attempt_id.clone(), strong)
                        .await;
                    (result, start.elapsed())
                },
                tokio::task::spawn_blocking(move || {
                    let start = Instant::now();
                    let format = if strong {
                        state::SnapshotFormat::V2
                    } else {
                        state::SnapshotFormat::LegacyV1
                    };
                    let result =
                        state::save_entries_as(&local_state_for_save, &entries_for_save, format);
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
            let (remote_result, remote_state_save_duration) = remote_result;
            remote_result
                .map_err(|e| post_state_save_rpc_error("failed to save remote state", e))?;
            remote
                .complete_staged_apply(wave_attempt_id.clone())
                .await
                .map_err(|e| {
                    post_state_save_rpc_error("failed to complete remote staged apply", e)
                })?;
            sync_ops::finish_staged_apply_attempt(&local_state, &wave_attempt_id)?;
            record_phase_aggregate(
                &mut performance,
                "local_state_save",
                local_state_save_duration,
            );
            record_phase_aggregate(
                &mut performance,
                "remote_state_save_rpc",
                remote_state_save_duration,
            );
            record_phase_aggregate(
                &mut performance,
                "state_save_total",
                state_save_start.elapsed(),
            );

            if wave_index + 1 < plan.waves.len() && !interrupt.try_reset_after_checkpoint() {
                return Ok(SyncOutcome::Interrupted);
            }
        }
        checkpoint_entries
    } else if apply_strategy == ApplyStrategy::LegacyStream {
        log::debug!("streaming detailed changes");
        if !interrupt.try_begin_commit() {
            return Ok(SyncOutcome::Interrupted);
        }
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
            Some(scan_policy.clone()),
            apply_options,
            remote_stream_performance_enabled(profiling_enabled, &remote_info),
            has_remote_capability(&remote_info, rpc::CAPABILITY_FILE_BYTE_CHUNKS),
            None,
            None,
            None,
        )
        .await?;
        let StreamDetailedChangesRun::Complete(mut stream_result) = stream_result else {
            unreachable!("legacy stream cannot be cancelled after commit")
        };
        record_stream_performance(&mut performance, &mut stream_result);
        let StreamApplyOutcome::Legacy(local_all_old) = stream_result.outcome else {
            unreachable!("legacy stream returned staged outcome");
        };
        local_all_old
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

        if interrupt.is_cancel_requested() {
            return Ok(SyncOutcome::Interrupted);
        }
        if !interrupt.try_begin_commit() {
            return Ok(SyncOutcome::Interrupted);
        }

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
            let scan_policy = scan_policy.clone();
            tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                sync_ops::apply_detailed_changes_with_policy(
                    &local_base,
                    &actions,
                    &remote_detailed_changes,
                    &mut local_all_old,
                    Some(&local_state),
                    Some(&scan_policy),
                    apply_options,
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

    if !apply_strategy.is_staged() {
        sync_ops::mark_apply_attempt_state_save(
            "local",
            &local_state,
            &local_base,
            actions.as_ref(),
            Some(&apply_attempt_id),
        )?;

        let state_save_start = Instant::now();
        let coordinated_cleanup = has_remote_capability(
            &remote_info,
            rpc::CAPABILITY_COORDINATED_MARKER_CLEANUP,
        );
        let local_state_display = local_state.display().to_string();
        let local_state_for_save = local_state.clone();
        let (remote_result, local_result) = tokio::join!(
            async {
                let start = Instant::now();
                let result = if coordinated_cleanup {
                    remote.save_state_pending(strong).await
                } else if strong {
                    remote.save_state_v2().await
                } else {
                    remote.save_state().await
                };
                (result, start.elapsed())
            },
            tokio::task::spawn_blocking(move || {
                let start = Instant::now();
                let format = if strong {
                    state::SnapshotFormat::V2
                } else {
                    state::SnapshotFormat::LegacyV1
                };
                let result = state::save_entries_as(&local_state_for_save, &local_all_old, format);
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
        let (remote_result, remote_state_save_duration) = remote_result;
        remote_result.map_err(|e| post_state_save_rpc_error("failed to save remote state", e))?;
        if coordinated_cleanup {
            remote.clear_apply_attempt(remote_id).await
                .map_err(|e| remote_rpc_error("failed to clear remote recovery marker", e))?;
            sync_ops::finish_apply_attempt(&local_state)?;
        } else {
            sync_ops::finish_apply_attempt(&local_state)?;
        }
        performance.record_phase("local_state_save", local_state_save_duration);
        performance.record_phase("remote_state_save_rpc", remote_state_save_duration);
        performance.record_phase("state_save_total", state_save_start.elapsed());
    }

    if profiling_enabled {
        performance.finish(total_start.elapsed());
        if print_performance {
            performance.print_human();
        }
        if let Some(path) = performance_json {
            performance.write_json(&path)?;
        }
    }

    Ok(SyncOutcome::Success)
    }
    .await;

    let server_wait = server.wait().await;
    interrupt.clear_local_server();
    let result = finalize_server(sync_result, server_wait);
    if interrupt.complete() {
        result.map(|_| SyncOutcome::Interrupted)
    } else {
        result
    }
}

fn outbound_scan_locations(
    locations: &crate::scan::location::Locations,
) -> crate::scan::location::Locations {
    crate::scan::location::canonicalize(locations)
}

pub async fn recover_remote(target: PathBuf, clear: bool, yes: bool) -> Result<()> {
    env_logger::init();
    let profile_name = remote_recovery_profile_name(&target)?;
    let context = prepare_context(ProfileSource::Named(profile_name.to_string()), None, &[])?;

    let SyncContext {
        local_id,
        legacy_local_id,
        remote_server,
        remote_cmd,
        remote_state_dir,
        server_log,
        ..
    } = context;

    let remote_session = open_remote_session(remote_server).await;
    let mut server = remote::launch_server(&remote_session, remote_cmd, &server_log)
        .await
        .unwrap_or_else(|e| {
            let diagnostic =
                sync_error::render_report("setup", "launch server", Some(server_log.clone()), e);
            eprintln!("{}", diagnostic.cyan());
            quit::with_code(SERVER_ERROR_CODE);
        });
    let result = async {
        let remote = remote::get_remote(&mut server)?;
        let remote_info = remote.server_info().await.map_err(server_info_error)?;
        require_remote_capability(&remote_info, rpc::CAPABILITY_RECOVERY)?;
        if let Some(remote_state_dir) = remote_state_dir {
            require_remote_capability(&remote_info, rpc::CAPABILITY_PROFILE_FILE_STATE_DIR)?;
            remote
                .set_remote_state_dir(remote_state_dir)
                .await
                .map_err(remote_state_dir_error)?;
        }
        let remote_id =
            select_remote_state_id(&remote, &remote_info, local_id, legacy_local_id).await?;

        match remote
            .describe_apply_attempt(remote_id.clone())
            .await
            .map_err(|e| remote_rpc_error("Failed to inspect remote recovery marker", e))?
        {
            Some(description) => {
                println!("{}", description);
                if clear && crate::commands::confirm_clear_recovery_marker(yes)? {
                    remote.clear_apply_attempt(remote_id).await.map_err(|e| {
                        remote_rpc_error("Failed to clear remote recovery marker", e)
                    })?;
                    println!(
                        "Removed remote recovery marker for profile {}",
                        profile_name
                    );
                }
            }
            None => println!(
                "No unfinished remote Duet apply attempt for profile {}",
                profile_name
            ),
        }

        Ok(())
    }
    .await;
    finalize_server(result, server.wait().await)
}

fn remote_recovery_profile_name(target: &Path) -> Result<&str> {
    if target.components().count() != 1 {
        return Err(eyre!(
            "--remote recovery requires a named profile, for example `duet recover --remote cole`"
        ));
    }
    let name = target.to_str().ok_or_else(|| {
        eyre!("--remote recovery requires a UTF-8 named profile, for example `duet recover --remote cole`")
    })?;
    if name.is_empty() || name == "." || name == ".." || name.contains('\\') {
        return Err(eyre!(
            "--remote recovery requires a named profile, for example `duet recover --remote cole`"
        ));
    }
    Ok(name)
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

async fn select_remote_state_id<R>(
    remote: &R,
    info: &rpc::ServerInfo,
    stable_id: String,
    legacy_id: Option<String>,
) -> Result<String>
where
    R: DuetServerAsync,
{
    if has_remote_capability(info, rpc::CAPABILITY_REMOTE_STATE_ID_SELECTION) {
        return remote
            .select_remote_state_id(stable_id, legacy_id)
            .await
            .map_err(|e| remote_rpc_error("Couldn't select remote state id", e));
    }

    Ok(legacy_id.unwrap_or(stable_id))
}

fn new_apply_attempt_id(local_id: &str) -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", local_id, std::process::id(), since_epoch)
}

fn remote_stream_performance_enabled(profiling_enabled: bool, info: &rpc::ServerInfo) -> bool {
    profiling_enabled && has_remote_capability(info, rpc::CAPABILITY_STREAM_PERFORMANCE)
}

fn select_apply_strategy(
    info: &rpc::ServerInfo,
    can_stream_details: bool,
    staging_policy_explicit: bool,
) -> Result<ApplyStrategy> {
    if !can_stream_details {
        if staging_policy_explicit {
            return Err(eyre!(
                "--staging-limit/--staging-reserve requires a streamable staged apply plan"
            ));
        }
        return Ok(ApplyStrategy::LegacyNonStream);
    }
    if has_remote_capability(info, rpc::CAPABILITY_STAGED_APPLY)
        && has_remote_capability(info, rpc::CAPABILITY_STAGING_CAPACITY)
        && has_remote_capability(info, rpc::CAPABILITY_STAGING_RESERVE_ENFORCEMENT)
    {
        return Ok(ApplyStrategy::StagedStream);
    }
    if staging_policy_explicit {
        return Err(eyre!(
            "remote duet {} cannot enforce --staging-limit/--staging-reserve; upgrade it to a version supporting {}",
            info.duet_version,
            rpc::CAPABILITY_STAGING_RESERVE_ENFORCEMENT
        ));
    }
    Ok(ApplyStrategy::LegacyStream)
}

fn can_use_staging_plan(
    info: &rpc::ServerInfo,
    wave_count: usize,
    migration: bool,
    staging_policy_explicit: bool,
) -> Result<bool> {
    if wave_count <= 1 {
        return Ok(true);
    }
    let unavailable = if migration {
        Some("strong-digest migration cannot checkpoint partial wave results")
    } else if !has_remote_capability(info, rpc::CAPABILITY_CHECKPOINTED_STAGING) {
        Some("the remote peer does not support checkpointed staging")
    } else {
        None
    };
    let Some(reason) = unavailable else {
        return Ok(true);
    };
    if staging_policy_explicit {
        return Err(eyre!(
            "staging policy requires {wave_count} waves, but {reason}; increase --staging-limit/free space or upgrade the remote peer"
        ));
    }
    Ok(false)
}

async fn set_remote_actions<R>(remote: &R, actions: Actions, strong: bool) -> Result<()>
where
    R: DuetServerAsync,
{
    if strong {
        remote.set_actions_v2(actions).await
    } else {
        remote.set_actions(crate::actions::to_legacy(actions)).await
    }
    .map_err(|e| remote_rpc_error("Failed to set remote actions", e))
}

fn print_staging_plan_summary(
    plan: &sync_ops::StagingWavePlan,
    local_budget: sync_ops::StagingBudget,
    remote_budget: sync_ops::StagingBudget,
) {
    println!(
        "Staging: {} wave(s), local {} total / {} per-wave budget, remote {} total / {} per-wave budget",
        plan.waves.len(),
        indicatif::HumanBytes(plan.local_reconstructed_bytes),
        indicatif::HumanBytes(local_budget.budget_bytes),
        indicatif::HumanBytes(plan.remote_reconstructed_bytes),
        indicatif::HumanBytes(remote_budget.budget_bytes),
    );
}

fn validate_wave_capacity(
    wave: &sync_ops::StagingWave,
    policy: sync_ops::StagingPolicy,
    local_filesystem: sync_ops::StagingFilesystemInfo,
    remote_filesystem: sync_ops::StagingFilesystemInfo,
    remote_inode_capacity_known: bool,
    wave_index: usize,
    wave_count: usize,
) -> Result<()> {
    validate_wave_side_capacity(
        "local",
        wave.local_reconstructed_bytes,
        wave.local_staged_regular_outputs,
        wave.local_exceeds_budget,
        wave.local_requires_cow_capacity,
        policy.budget(local_filesystem),
        local_filesystem.available_inodes,
        true,
        wave_index,
        wave_count,
    )?;
    validate_wave_side_capacity(
        "remote",
        wave.remote_reconstructed_bytes,
        wave.remote_staged_regular_outputs,
        wave.remote_exceeds_budget,
        wave.remote_requires_cow_capacity,
        policy.budget(remote_filesystem),
        remote_filesystem.available_inodes,
        remote_inode_capacity_known,
        wave_index,
        wave_count,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_wave_side_capacity(
    side: &str,
    required_bytes: u64,
    required_outputs: usize,
    isolated_oversize: bool,
    requires_cow_capacity: bool,
    budget: sync_ops::StagingBudget,
    available_inodes: u64,
    inode_capacity_known: bool,
    wave_index: usize,
    wave_count: usize,
) -> Result<()> {
    let wave_number = wave_index + 1;
    if required_bytes > budget.usable_bytes {
        if !requires_cow_capacity {
            return Err(eyre!(
                "staging capacity shrank before wave {wave_number}/{wave_count}: {side} requires {required_bytes} logical output bytes but only {} bytes fit after reserve",
                budget.usable_bytes
            ));
        }
        log::debug!(
            "wave {wave_number}/{wave_count} {side} COW output has {required_bytes} logical bytes exceeding current usable {}; prepare-time physical-space monitoring will enforce reserve",
            budget.usable_bytes
        );
    }
    if required_bytes > budget.budget_bytes && !isolated_oversize {
        return Err(eyre!(
            "staging capacity shrank before wave {wave_number}/{wave_count}: {side} requires {required_bytes} bytes but the current staging limit is {} bytes",
            budget.budget_bytes
        ));
    }
    if (inode_capacity_known || available_inodes != 0)
        && required_outputs as u128 > available_inodes as u128
    {
        return Err(eyre!(
            "staging capacity shrank before wave {wave_number}/{wave_count}: {side} requires {required_outputs} staged files but only {available_inodes} inodes are available"
        ));
    }
    Ok(())
}

fn actions_have_directory_to_nondirectory_change(actions: &[Action]) -> bool {
    fn replacement(change: &Change) -> bool {
        matches!(change, Change::Modified(old, new) if old.is_dir() && !new.is_dir())
    }
    actions.iter().any(|action| match action {
        Action::Local(change) | Action::Remote(change) => replacement(change),
        Action::Identical(left, right) | Action::Conflict(left, right) => {
            replacement(left) || replacement(right)
        }
        Action::ResolvedLocal((left, right), resolved)
        | Action::ResolvedRemote((left, right), resolved) => {
            replacement(left) || replacement(right) || replacement(resolved)
        }
    })
}

fn validate_prepared_report(
    side: &str,
    report: &sync_ops::PreparedApplyReport,
    expected_actions: usize,
) -> Result<()> {
    if report.action_count != expected_actions {
        return Err(eyre!(
            "{} staged preparation reported {} actions, expected {}",
            side,
            report.action_count,
            expected_actions
        ));
    }
    if report.prepared_file_count > report.action_count {
        return Err(eyre!(
            "{} staged preparation reported {} prepared files for {} actions",
            side,
            report.prepared_file_count,
            report.action_count
        ));
    }
    if report.prepared_file_count == 0 && report.prepared_file_bytes != 0 {
        return Err(eyre!(
            "{} staged preparation reported bytes without prepared files",
            side
        ));
    }
    Ok(())
}

fn validate_staged_prepare_barrier(
    local_report: &sync_ops::PreparedApplyReport,
    local_action_count: usize,
    remote_report: &sync_ops::PreparedApplyReport,
    remote_action_count: usize,
) -> Result<()> {
    validate_prepared_report("local", local_report, local_action_count)?;
    validate_prepared_report("remote", remote_report, remote_action_count)
}

fn finish_staged_commit(
    local: Result<state::Entries>,
    remote: std::result::Result<(), RPCError>,
) -> Result<state::Entries> {
    match (local, remote) {
        (Ok(entries), Ok(())) => Ok(entries),
        (Err(local), Ok(())) => Err(local).wrap_err(POST_PREFLIGHT_RECOVERY_ADVICE),
        (Ok(_), Err(remote)) => Err(post_preflight_rpc_error(
            "remote staged commit failed after preflight",
            remote,
        )),
        (Err(local), Err(remote)) => Err(eyre!(
            "local staged commit failed: {:#}\nremote staged commit failed: {}\n{}",
            local,
            sync_error::render_rpc_error(&remote),
            POST_PREFLIGHT_RECOVERY_ADVICE
        )),
    }
}

async fn resolve_removal_blockers<R>(
    remote: &R,
    remote_info: &rpc::ServerInfo,
    local_base: &PathBuf,
    local_actions: &Actions,
    remote_actions: &Actions,
    scan_policy: &sync_ops::ScanPolicy,
    apply_options: sync_ops::ApplyOptions,
) -> Result<sync_ops::ApplyOptions>
where
    R: DuetServerAsync,
{
    let local_report = sync_ops::preflight_apply_report(
        local_base,
        local_actions,
        Some(scan_policy),
        apply_options,
    )?;
    let local_blocked = local_report.has_unprunable_blockers();
    if local_blocked {
        print_preflight_report("local", &local_report);
    }

    let remote_report =
        match remote_preflight_report(remote, remote_info, remote_actions, apply_options).await {
            Ok(report) => report,
            Err(_) if local_blocked => return Err(preflight_blocker_error("local")),
            Err(error) => return Err(error),
        };
    let remote_blocked = remote_report.has_unprunable_blockers();
    if remote_blocked {
        print_preflight_report("remote", &remote_report);
    }

    if local_blocked {
        return Err(preflight_blocker_error("local"));
    }
    if remote_blocked {
        return Err(preflight_blocker_error("remote"));
    }
    Ok(apply_options)
}

async fn remote_preflight_report<R>(
    remote: &R,
    remote_info: &rpc::ServerInfo,
    remote_actions: &Actions,
    apply_options: sync_ops::ApplyOptions,
) -> Result<sync_ops::ApplyPreflightReport>
where
    R: DuetServerAsync,
{
    if has_remote_capability(remote_info, rpc::CAPABILITY_REMOVAL_BLOCKER_REPORT) {
        return if has_remote_capability(remote_info, rpc::CAPABILITY_CONTENT_DIGEST_BLAKE2B256) {
            remote
                .removal_blocker_report_v2(remote_actions.clone(), apply_options)
                .await
        } else {
            remote
                .removal_blocker_report(
                    crate::actions::to_legacy(remote_actions.clone()),
                    apply_options,
                )
                .await
        }
        .map_err(|e| remote_rpc_error("Failed to get remote removal blocker report", e));
    }
    if !has_remote_capability(remote_info, rpc::CAPABILITY_PREFLIGHT_REPORT) {
        return Ok(sync_ops::ApplyPreflightReport::default());
    }
    if has_remote_capability(remote_info, rpc::CAPABILITY_CONTENT_DIGEST_BLAKE2B256) {
        remote
            .preflight_apply_report_v2(remote_actions.clone(), apply_options)
            .await
    } else {
        remote
            .preflight_apply_report(
                crate::actions::to_legacy(remote_actions.clone()),
                apply_options,
            )
            .await
    }
    .map_err(|e| remote_rpc_error("Failed to get remote preflight report", e))
}

#[cfg(test)]
fn ensure_preflight_report_clear(
    side: &str,
    report: &sync_ops::ApplyPreflightReport,
) -> Result<()> {
    if report.blockers.is_empty() || !report.has_unprunable_blockers() {
        return Ok(());
    }
    print_preflight_report(side, report);
    Err(preflight_blocker_error(side))
}

fn preflight_blocker_error(side: &str) -> color_eyre::eyre::Report {
    eyre!(
        "{} preflight found directory removal blockers; resolve them manually, use --prune-ignored for disposable ignored content, or mark disposable patterns in [prune]",
        side
    )
}

fn print_preflight_report(side: &str, report: &sync_ops::ApplyPreflightReport) {
    if report.blockers.is_empty() {
        return;
    }
    println!("{} directory removal blockers:", side.cyan());
    for blocker in &report.blockers {
        let kind = match blocker.kind {
            sync_ops::RemovalBlockerType::Ignored => "ignored",
            sync_ops::RemovalBlockerType::Prune => "prunable",
            sync_ops::RemovalBlockerType::Excluded => "excluded",
            sync_ops::RemovalBlockerType::Unexpected => "unexpected",
        };
        let action = if blocker.prunable {
            "will prune"
        } else {
            "blocks removal"
        };
        if let Some(pattern) = &blocker.pattern {
            println!(
                "  {} {} matched {:?}: {}",
                kind,
                blocker.child.display(),
                pattern,
                action
            );
        } else {
            println!("  {} {}: {}", kind, blocker.child.display(), action);
        }
    }
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

#[cfg(debug_assertions)]
async fn test_pause_until_interrupt(variable: &str, interrupt: &InterruptState) {
    let Ok(raw_ms) = std::env::var(variable) else {
        return;
    };
    let Ok(ms) = raw_ms.parse::<u64>() else {
        return;
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while tokio::time::Instant::now() < deadline {
        if matches!(
            interrupt.phase.load(Ordering::SeqCst),
            x if x == InterruptPhase::CancelRequested as u8
                || x == InterruptPhase::CommittedInterrupted as u8
        ) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(not(debug_assertions))]
async fn test_pause_until_interrupt(_variable: &str, _interrupt: &InterruptState) {}

fn install_ctrlc_handler(interrupt: InterruptState) -> Result<()> {
    ctrlc::set_handler(move || {
        handle_interrupt_request(&interrupt);
    })
    .wrap_err("failed to install Ctrl-C handler")?;
    Ok(())
}

fn handle_interrupt_request(interrupt: &InterruptState) {
    match interrupt.request_interrupt() {
        InterruptRequest::Cancel => eprintln!("\nInterrupt requested; stopping safely"),
        InterruptRequest::Deferred => {
            eprintln!("\nInterrupt received; finishing committed sync")
        }
        InterruptRequest::Force => {
            eprintln!("\nSecond interrupt; forcing exit");
            interrupt.force_stop_local_server();
            std::process::exit(CTRLC_CODE.into());
        }
    }
}

fn finalize_server<T>(primary: Result<T>, wait: Result<ExitStatus>) -> Result<T> {
    let wait = wait.and_then(|status| {
        if status.success() {
            Ok(())
        } else {
            Err(eyre!("duet server exited with status {}", status))
        }
    });
    match (primary, wait) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(wait)) => Err(wait),
        (Err(primary), Err(wait)) => {
            Err(primary.wrap_err(format!("waiting for duet server also failed: {:#}", wait)))
        }
    }
}

fn prepare_context(
    source: ProfileSource,
    path: Option<PathBuf>,
    excludes: &[PathBuf],
) -> Result<SyncContext> {
    let config = profile::load(&source).unwrap_or_else(|e| {
        let diagnostic =
            sync_error::render_error("setup", "load profile", profile_source_path(&source), e);
        eprintln!("{}", diagnostic.cyan());
        quit::with_code(PROFILE_ERROR_CODE);
    });

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

    let scope = normalize_scope(&local_base, path, excludes)?;
    println!(
        "Using profile: {} {}",
        config.display_name.cyan(),
        scope.restrict.display().to_string().yellow()
    );

    let remote_state_dir = remote_state_dir_for_source(&source, remote_server.as_deref(), &config)?;
    let local_ids = local_ids(&config.identity)?;

    Ok(SyncContext {
        profile: config.profile,
        local_id: local_ids.stable,
        legacy_local_id: local_ids.legacy,
        local_base,
        remote_base,
        remote_server,
        remote_cmd,
        scope,
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

enum StreamApplyOutcome {
    Legacy(state::Entries),
    Staged {
        prepared: sync_ops::PreparedApply,
        local_report: sync_ops::PreparedApplyReport,
        remote_report: sync_ops::PreparedApplyReport,
    },
}

struct StreamDetailedChangesResult {
    outcome: StreamApplyOutcome,
    profile: StreamingProfile,
    remote_detail_and_local_apply_duration: Duration,
    local_detail_and_remote_apply_duration: Duration,
    remote_detail_duration: Duration,
    local_apply_duration: Duration,
    local_detail_duration: Duration,
    remote_apply_duration: Duration,
}

enum StreamDetailedChangesRun {
    Complete(StreamDetailedChangesResult),
    Interrupted,
    InterruptedCleaned,
}

fn record_stream_performance(
    performance: &mut PerformanceProfile,
    result: &mut StreamDetailedChangesResult,
) {
    record_phase_aggregate(
        performance,
        "stream_remote_detail_and_local_apply",
        result.remote_detail_and_local_apply_duration,
    );
    record_phase_aggregate(
        performance,
        "stream_remote_detail_rpc",
        result.remote_detail_duration,
    );
    record_phase_aggregate(
        performance,
        "stream_local_apply",
        result.local_apply_duration,
    );
    record_phase_aggregate(
        performance,
        "stream_local_detail_and_remote_apply",
        result.local_detail_and_remote_apply_duration,
    );
    record_phase_aggregate(
        performance,
        "stream_local_detail",
        result.local_detail_duration,
    );
    record_phase_aggregate(
        performance,
        "stream_remote_apply_rpc",
        result.remote_apply_duration,
    );
    performance.counters.streamed_details = true;
    merge_streaming_profile(
        &mut performance.counters.streaming,
        std::mem::take(&mut result.profile),
    );
}

fn record_phase_aggregate(performance: &mut PerformanceProfile, name: &str, duration: Duration) {
    if let Some(phase) = performance
        .phases
        .iter_mut()
        .find(|phase| phase.name == name)
    {
        phase.ms = phase.ms.saturating_add(duration_ms(duration));
    } else {
        performance.record_phase(name, duration);
    }
}

fn merge_streaming_profile(target: &mut StreamingProfile, source: StreamingProfile) {
    merge_transfer_stats(&mut target.remote_to_local, source.remote_to_local);
    merge_transfer_stats(&mut target.local_to_remote, source.local_to_remote);
    if let Some(source_remote) = source.remote_server {
        let target_remote = target.remote_server.get_or_insert_with(Default::default);
        target_remote.detail_generate_ms = target_remote
            .detail_generate_ms
            .saturating_add(source_remote.detail_generate_ms);
        target_remote.detail_batches = target_remote
            .detail_batches
            .saturating_add(source_remote.detail_batches);
        target_remote.apply_frames_ms = target_remote
            .apply_frames_ms
            .saturating_add(source_remote.apply_frames_ms);
        target_remote.apply_finish_ms = target_remote
            .apply_finish_ms
            .saturating_add(source_remote.apply_finish_ms);
        target_remote.apply_batches = target_remote
            .apply_batches
            .saturating_add(source_remote.apply_batches);
        merge_transfer_stats(
            &mut target_remote.detail_transfer,
            source_remote.detail_transfer,
        );
        merge_transfer_stats(
            &mut target_remote.apply_transfer,
            source_remote.apply_transfer,
        );
    }
}

fn merge_transfer_stats(target: &mut DetailTransferStats, source: DetailTransferStats) {
    target.batches = target.batches.saturating_add(source.batches);
    target.empty_batches = target.empty_batches.saturating_add(source.empty_batches);
    target.frames = target.frames.saturating_add(source.frames);
    target.message_payload_bytes = target
        .message_payload_bytes
        .saturating_add(source.message_payload_bytes);
    target.reconstructed_bytes = target
        .reconstructed_bytes
        .saturating_add(source.reconstructed_bytes);
    target.file_bytes = target.file_bytes.saturating_add(source.file_bytes);
    target.diff_literal_bytes = target
        .diff_literal_bytes
        .saturating_add(source.diff_literal_bytes);
    target.diff_copy_bytes = target
        .diff_copy_bytes
        .saturating_add(source.diff_copy_bytes);
    target.file_byte_frames = target
        .file_byte_frames
        .saturating_add(source.file_byte_frames);
    target.diff_literal_frames = target
        .diff_literal_frames
        .saturating_add(source.diff_literal_frames);
    target.diff_copy_frames = target
        .diff_copy_frames
        .saturating_add(source.diff_copy_frames);
    target.max_batch_frames = target.max_batch_frames.max(source.max_batch_frames);
    target.max_batch_payload_bytes = target
        .max_batch_payload_bytes
        .max(source.max_batch_payload_bytes);
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
    scan_policy: Option<sync_ops::ScanPolicy>,
    apply_options: sync_ops::ApplyOptions,
    remote_stream_performance: bool,
    file_byte_chunks: bool,
    staged_attempt_id: Option<&str>,
    staging_policy: Option<sync_ops::StagingPolicy>,
    interrupt: Option<&InterruptState>,
) -> Result<StreamDetailedChangesRun>
where
    R: DuetServerAsync,
{
    if interrupt.is_some_and(InterruptState::is_cancel_requested) {
        return Ok(StreamDetailedChangesRun::Interrupted);
    }
    let staged_remote_apply_stream = if let Some(attempt_id) = staged_attempt_id {
        let stream_id = remote
            .begin_staged_apply(attempt_id.to_string())
            .await
            .map_err(|e| remote_rpc_error("Couldn't begin remote staged apply", e))?;
        if interrupt.is_some_and(InterruptState::is_cancel_requested) {
            let cleanup = remote.abort_staged_apply(attempt_id.to_string()).await;
            return match cleanup {
                Ok(()) => Ok(StreamDetailedChangesRun::Interrupted),
                Err(error) => Err(add_staged_cleanup_context(
                    eyre!("sync interrupted while beginning staged preparation"),
                    None,
                    Some(error),
                )),
            };
        }
        if let Err(error) = sync_ops::start_staged_apply_attempt(
            "local",
            local_state,
            local_base,
            actions,
            attempt_id,
        ) {
            let cleanup = remote.abort_staged_apply(attempt_id.to_string()).await;
            return Err(add_staged_cleanup_context(error, None, cleanup.err()));
        }
        Some(stream_id)
    } else {
        None
    };

    let result = stream_detailed_changes_started(
        remote,
        local_base,
        local_state,
        actions,
        local_all_old,
        local_signatures,
        remote_signatures,
        tuning,
        scan_policy,
        apply_options,
        remote_stream_performance,
        file_byte_chunks,
        staged_attempt_id,
        staging_policy,
        staged_remote_apply_stream,
        interrupt,
    )
    .await;

    match result {
        Ok(StreamDetailedChangesRun::Complete(result)) => {
            Ok(StreamDetailedChangesRun::Complete(result))
        }
        Ok(StreamDetailedChangesRun::Interrupted) => {
            let attempt_id = staged_attempt_id
                .expect("only staged detail streaming can be interrupted before commit");
            let local_cleanup = sync_ops::abort_staged_apply_attempt(local_state, attempt_id).err();
            let remote_cleanup = remote
                .abort_staged_apply(attempt_id.to_string())
                .await
                .err();
            if local_cleanup.is_some() || remote_cleanup.is_some() {
                Err(add_staged_cleanup_context(
                    eyre!("sync interrupted during staged preparation"),
                    local_cleanup,
                    remote_cleanup,
                ))
            } else {
                Ok(StreamDetailedChangesRun::Interrupted)
            }
        }
        Ok(StreamDetailedChangesRun::InterruptedCleaned) => {
            Ok(StreamDetailedChangesRun::Interrupted)
        }
        Err(primary) => {
            if let Some(attempt_id) = staged_attempt_id {
                let local_cleanup =
                    sync_ops::abort_staged_apply_attempt(local_state, attempt_id).err();
                let remote_cleanup = remote
                    .abort_staged_apply(attempt_id.to_string())
                    .await
                    .err();
                return Err(add_staged_cleanup_context(
                    primary,
                    local_cleanup,
                    remote_cleanup,
                ));
            }
            Err(primary)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_detailed_changes_started<R>(
    remote: &R,
    local_base: &PathBuf,
    local_state: &Path,
    actions: &Actions,
    local_all_old: state::Entries,
    local_signatures: Vec<sync_ops::SignatureWithPath>,
    remote_signatures: Vec<sync_ops::SignatureWithPath>,
    tuning: sync_ops::SyncTuning,
    scan_policy: Option<sync_ops::ScanPolicy>,
    apply_options: sync_ops::ApplyOptions,
    remote_stream_performance: bool,
    file_byte_chunks: bool,
    staged_attempt_id: Option<&str>,
    staging_policy: Option<sync_ops::StagingPolicy>,
    staged_remote_apply_stream: Option<sync_ops::ApplyStreamId>,
    interrupt: Option<&InterruptState>,
) -> Result<StreamDetailedChangesRun>
where
    R: DuetServerAsync,
{
    if interrupt.is_some_and(InterruptState::is_cancel_requested) {
        return Ok(StreamDetailedChangesRun::Interrupted);
    }
    let total_transfer_bytes = sync_ops::detail_transfer_bytes(actions);
    let progress = stream_progress_bar(total_transfer_bytes)?;
    let mut progress_position = 0;

    let mut local_producer = sync_ops::DetailProducer::new(
        local_base.clone(),
        actions.clone(),
        remote_signatures,
        tuning.detail_chunk_bytes(),
    );
    let mut local_applier = if let Some(attempt_id) = staged_attempt_id {
        sync_ops::DetailApplier::new_capacity_aware_staged_with_attempt_and_policy(
            local_base.clone(),
            actions.clone(),
            local_all_old,
            local_state.to_path_buf(),
            attempt_id.to_string(),
            scan_policy,
            apply_options,
            staging_policy.expect("staged apply must provide a staging policy"),
        )
    } else {
        sync_ops::DetailApplier::new_with_attempt_and_policy(
            local_base.clone(),
            actions.clone(),
            local_all_old,
            Some(local_state.to_path_buf()),
            scan_policy,
            apply_options,
        )
    };

    let remote_detail_stream = remote
        .begin_detail_stream(local_signatures, tuning.detail_chunk_bytes() as u32)
        .await
        .map_err(|e| remote_rpc_error("Couldn't begin remote detail stream", e))?;
    if interrupt.is_some_and(InterruptState::is_cancel_requested) {
        return Ok(StreamDetailedChangesRun::Interrupted);
    }
    let remote_apply_stream = if let Some(stream_id) = staged_remote_apply_stream {
        stream_id
    } else {
        remote
            .begin_apply_stream()
            .await
            .map_err(|e| remote_rpc_error("Couldn't begin remote apply stream", e))?
    };
    let mut local_done = false;
    let mut remote_done = false;
    let mut profile = StreamingProfile::default();
    let mut remote_detail_duration = Duration::default();
    let mut local_apply_duration = Duration::default();
    let mut local_detail_duration = Duration::default();
    let mut remote_apply_duration = Duration::default();
    while !local_done || !remote_done {
        if interrupt.is_some_and(InterruptState::is_cancel_requested) {
            return Ok(StreamDetailedChangesRun::Interrupted);
        }
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
            if interrupt.is_some_and(InterruptState::is_cancel_requested) {
                return Ok(StreamDetailedChangesRun::Interrupted);
            }
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
            if interrupt.is_some_and(InterruptState::is_cancel_requested) {
                return Ok(StreamDetailedChangesRun::Interrupted);
            }
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
                if !apply_detail_frames(
                    remote,
                    remote_apply_stream,
                    frames,
                    file_byte_chunks,
                    interrupt,
                )
                .await?
                {
                    return Ok(StreamDetailedChangesRun::Interrupted);
                }
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
    if interrupt.is_some_and(InterruptState::is_cancel_requested) {
        return Ok(StreamDetailedChangesRun::Interrupted);
    }
    let outcome = if let Some(attempt_id) = staged_attempt_id {
        let prepared = local_applier
            .finish_preparation()
            .wrap_err("Couldn't finish local staged preparation")?;
        if interrupt.is_some_and(InterruptState::is_cancel_requested) {
            let local_cleanup = prepared.abort().err();
            let remote_cleanup = remote
                .abort_staged_apply(attempt_id.to_string())
                .await
                .err();
            if local_cleanup.is_some() || remote_cleanup.is_some() {
                return Err(add_staged_cleanup_context(
                    eyre!("sync interrupted while finishing staged preparation"),
                    local_cleanup,
                    remote_cleanup,
                ));
            }
            return Ok(StreamDetailedChangesRun::InterruptedCleaned);
        }
        let local_report = prepared.report();
        local_apply_duration += start.elapsed();
        let start = Instant::now();
        let remote_report = remote
            .finish_staged_prepare(remote_apply_stream, attempt_id.to_string())
            .await
            .map_err(|e| remote_rpc_error("Couldn't finish remote staged preparation", e))?;
        remote_apply_duration += start.elapsed();
        StreamApplyOutcome::Staged {
            prepared,
            local_report,
            remote_report,
        }
    } else {
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
        StreamApplyOutcome::Legacy(local_all_old)
    };
    if remote_stream_performance {
        let remote_server_profile = remote
            .stream_performance()
            .await
            .map_err(|e| remote_rpc_error("Couldn't read remote stream performance", e))?;
        if !remote_server_profile.is_empty() {
            profile.remote_server = Some(remote_server_profile);
        }
    }
    progress.finish_and_clear();
    Ok(StreamDetailedChangesRun::Complete(
        StreamDetailedChangesResult {
            outcome,
            profile,
            remote_detail_and_local_apply_duration: remote_detail_duration + local_apply_duration,
            local_detail_and_remote_apply_duration: local_detail_duration + remote_apply_duration,
            remote_detail_duration,
            local_apply_duration,
            local_detail_duration,
            remote_apply_duration,
        },
    ))
}

fn add_staged_cleanup_context(
    primary: color_eyre::eyre::Report,
    local_cleanup: Option<color_eyre::eyre::Report>,
    remote_cleanup: Option<RPCError>,
) -> color_eyre::eyre::Report {
    let mut cleanup_errors = Vec::new();
    if let Some(error) = local_cleanup {
        cleanup_errors.push(format!("local abort: {:#}", error));
    }
    if let Some(error) = remote_cleanup {
        cleanup_errors.push(format!(
            "remote abort: {}",
            sync_error::render_rpc_error(&error)
        ));
    }
    if cleanup_errors.is_empty() {
        primary
    } else {
        primary.wrap_err(format!(
            "staged preparation cleanup also failed ({})",
            cleanup_errors.join("; ")
        ))
    }
}

async fn apply_detail_frames<R>(
    remote: &R,
    remote_apply_stream: sync_ops::ApplyStreamId,
    frames: Vec<sync_ops::DetailFrame>,
    file_byte_chunks: bool,
    interrupt: Option<&InterruptState>,
) -> Result<bool>
where
    R: DuetServerAsync,
{
    if !file_byte_chunks {
        remote
            .apply_detail_chunks(remote_apply_stream, frames)
            .await
            .map_err(|e| post_preflight_rpc_error("Couldn't apply remote detail stream", e))?;
        return Ok(!interrupt.is_some_and(InterruptState::is_cancel_requested));
    }

    for batch in route_file_byte_frames(frames) {
        match batch {
            ApplyDetailBatch::Frames(frames) => {
                remote
                    .apply_detail_chunks(remote_apply_stream, frames)
                    .await
                    .map_err(|e| {
                        post_preflight_rpc_error("Couldn't apply remote detail stream", e)
                    })?;
            }
            ApplyDetailBatch::FileByteChunk(chunk) => {
                remote
                    .apply_file_byte_chunk(remote_apply_stream, chunk)
                    .await
                    .map_err(|e| {
                        post_preflight_rpc_error("Couldn't apply remote file byte stream", e)
                    })?;
            }
        }
        if interrupt.is_some_and(InterruptState::is_cancel_requested) {
            return Ok(false);
        }
    }

    Ok(true)
}

enum ApplyDetailBatch {
    Frames(Vec<sync_ops::DetailFrame>),
    FileByteChunk(sync_ops::FileByteChunk),
}

fn route_file_byte_frames(frames: Vec<sync_ops::DetailFrame>) -> Vec<ApplyDetailBatch> {
    let mut batches = Vec::new();
    let mut buffered = Vec::new();
    for frame in frames {
        match frame.payload {
            sync_ops::DetailPayload::FileBytes(bytes)
                if should_apply_file_bytes_as_chunk(bytes.len()) =>
            {
                if !buffered.is_empty() {
                    batches.push(ApplyDetailBatch::Frames(std::mem::take(&mut buffered)));
                }
                batches.push(ApplyDetailBatch::FileByteChunk(
                    sync_ops::FileByteChunk::new(frame.action_index, bytes),
                ));
            }
            payload => buffered.push(sync_ops::DetailFrame {
                action_index: frame.action_index,
                payload,
            }),
        }
    }

    if !buffered.is_empty() {
        batches.push(ApplyDetailBatch::Frames(buffered));
    }

    batches
}

fn should_apply_file_bytes_as_chunk(len: usize) -> bool {
    len >= FILE_BYTE_CHUNK_RPC_THRESHOLD
}

fn stream_progress_bar(total_transfer_bytes: u64) -> Result<indicatif::ProgressBar> {
    progress::bytes_bar(total_transfer_bytes, "streaming changes")
}

fn preflight_non_streamed_detail_size(
    actions: &[Action],
    _remote_actions: &[Action],
) -> Result<()> {
    let detail_bytes = sync_ops::detail_transfer_bytes(actions);
    if detail_bytes > MAX_NON_STREAMED_DETAIL_BYTES {
        return Err(eyre!(
            "sync requires {} of file detail data, but this peer cannot stream it; refusing to materialize more than {} in memory",
            indicatif::HumanBytes(detail_bytes),
            indicatif::HumanBytes(MAX_NON_STREAMED_DETAIL_BYTES)
        ));
    }
    Ok(())
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
    progress.set_message("streaming changes");
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
            .control_persist(ControlPersist::ClosedAfterInitialConnection)
            .known_hosts_check(KnownHosts::Strict)
            .connect_mux(server)
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

fn build_actions(
    local_changes: &state::Changes,
    remote_changes: &state::Changes,
    strong: bool,
) -> Actions {
    utils::match_sorted(local_changes.iter(), remote_changes.iter())
        .filter_map(|(lc, rc)| {
            if strong {
                Action::create_strong(lc, rc)
            } else {
                Action::create(lc, rc)
            }
        })
        .collect()
}

fn build_migration_actions(
    local_changes: &state::Changes,
    remote_changes: &state::Changes,
    local_current: &state::Entries,
    remote_current: &state::Entries,
) -> Actions {
    use std::collections::BTreeSet;
    let mut paths = BTreeSet::new();
    paths.extend(local_current.iter().map(|entry| entry.path().clone()));
    paths.extend(remote_current.iter().map(|entry| entry.path().clone()));
    paths.extend(local_changes.iter().map(|change| change.path().clone()));
    paths.extend(remote_changes.iter().map(|change| change.path().clone()));

    paths
        .into_iter()
        .filter_map(|path| {
            let local = local_current
                .binary_search_by(|entry| entry.path().cmp(&path))
                .ok()
                .map(|i| &local_current[i]);
            let remote = remote_current
                .binary_search_by(|entry| entry.path().cmp(&path))
                .ok()
                .map(|i| &remote_current[i]);
            let local_change = local_changes
                .binary_search_by(|change| change.path().cmp(&path))
                .ok()
                .map(|i| &local_changes[i]);
            let remote_change = remote_changes
                .binary_search_by(|change| change.path().cmp(&path))
                .ok()
                .map(|i| &remote_changes[i]);
            let equivalent = match (local, remote) {
                (None, None) => true,
                (Some(a), Some(b)) => crate::scan::change::same_strong(
                    &Change::Added(a.clone()),
                    &Change::Added(b.clone()),
                ),
                _ => false,
            };

            match (local_change.is_some(), remote_change.is_some(), equivalent) {
                (true, false, _) => migration_change(remote, local).map(Action::Remote),
                (false, true, _) => migration_change(local, remote).map(Action::Local),
                (true, true, true) => match (local, remote) {
                    (Some(a), Some(b)) => Some(Action::Identical(
                        Change::Added(a.clone()),
                        Change::Added(b.clone()),
                    )),
                    (None, None) => None,
                    _ => unreachable!(),
                },
                (true, true, false) | (false, false, false) => {
                    Some(migration_conflict(local, remote))
                }
                (false, false, true) => None,
            }
        })
        .collect()
}

fn migration_conflict(
    local: Option<&crate::scan::DirEntryWithMeta>,
    remote: Option<&crate::scan::DirEntryWithMeta>,
) -> Action {
    match (local, remote) {
        (Some(local), Some(remote)) => Action::Conflict(
            Change::Modified(remote.clone(), local.clone()),
            Change::Modified(local.clone(), remote.clone()),
        ),
        (None, Some(remote)) => Action::Conflict(
            Change::Removed(remote.clone()),
            Change::Modified(remote.clone(), remote.clone()),
        ),
        (Some(local), None) => Action::Conflict(
            Change::Modified(local.clone(), local.clone()),
            Change::Removed(local.clone()),
        ),
        (None, None) => unreachable!("migration conflict requires a current entry"),
    }
}

fn migration_change(
    old: Option<&crate::scan::DirEntryWithMeta>,
    new: Option<&crate::scan::DirEntryWithMeta>,
) -> Option<Change> {
    match (old, new) {
        (None, None) => None,
        (None, Some(new)) => Some(Change::Added(new.clone())),
        (Some(old), None) => Some(Change::Removed(old.clone())),
        (Some(old), Some(new)) => Some(Change::Modified(old.clone(), new.clone())),
    }
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

fn show_dry_run_actions(actions: &Actions, verbose: bool) {
    if !actions.is_empty() {
        resolution::show_actions(actions, verbose);
    }
}

fn finish_dry_run(total_actions: usize, active_actions: usize, unresolved_conflicts: usize) {
    if total_actions == 0 {
        println!("Dry run completed: no changes detected");
    } else if unresolved_conflicts > 0 {
        println!(
            "Dry run completed with {} unresolved conflicts. Checks for those paths are incomplete until the conflicts are resolved.",
            unresolved_conflicts
        );
    } else if active_actions == 0 {
        println!("Dry run completed: no changes would be applied");
    } else {
        println!("Dry run completed: no changes applied");
    }
}

fn resolve_actions(actions: &mut Actions, options: SyncOptions) -> Result<AllResolution> {
    let SyncOptions {
        interactive,
        yes,
        dry_run: _,
        batch,
        force,
        verbose,
        debug_info: _,
        prune_ignored: _,
        profile_performance: _,
        profile_performance_json: _,
        ..
    } = options;

    if actions.is_empty() {
        println!("No changes detected");
        return Ok(AllResolution::Proceed);
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
    let cwd = std::env::current_dir()?;
    normalize_path_from_cwd(local_base, path, &cwd)
}

fn normalize_scope(
    local_base: &PathBuf,
    restrict: Option<PathBuf>,
    excludes: &[PathBuf],
) -> Result<scan::ScanScope> {
    let restrict = normalize_path(local_base, &restrict.unwrap_or_default())?;
    let excludes = excludes
        .iter()
        .map(|path| normalize_exclusion(local_base, path))
        .collect::<Result<Vec<_>>>()?;
    Ok(scan::ScanScope::new(restrict, excludes))
}

fn normalize_exclusion(local_base: &PathBuf, path: &PathBuf) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let absolute = if path.is_absolute() {
        path.clone()
    } else {
        let anchor = if path.starts_with("./")
            || path.starts_with("../")
            || path == Path::new(".")
            || path == Path::new("..")
        {
            cwd.as_path()
        } else {
            local_base.as_path()
        };
        anchor.join(path)
    };

    // Exclusions identify entries in the scanned namespace. Resolve their
    // ancestors for containment checks, but preserve the final component so an
    // existing or broken symlink excludes the symlink rather than its target.
    let resolved = match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => resolve_existing_prefix(parent)?.join(name),
        _ => resolve_existing_prefix(&absolute)?,
    };
    let relative = match resolved.strip_prefix(local_base) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => {
            let canonical_base = local_base.canonicalize().wrap_err_with(|| {
                format!("unable to resolve local base {}", local_base.display())
            })?;
            resolved
                .strip_prefix(&canonical_base)
                .map(Path::to_path_buf)
                .wrap_err_with(|| {
                    format!(
                        "excluded path {} is outside local base {}",
                        resolved.display(),
                        local_base.display()
                    )
                })?
        }
    };
    validate_relative_restriction(&relative)?;
    Ok(relative)
}

fn normalize_path_from_cwd(local_base: &PathBuf, path: &PathBuf, cwd: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return normalize_absolute_restriction(local_base, path);
    }

    let anchor = if path.starts_with("./")
        || path.starts_with("../")
        || path == Path::new(".")
        || path == Path::new("..")
    {
        cwd
    } else {
        local_base.as_path()
    };
    normalize_absolute_restriction(local_base, &anchor.join(path))
}

fn normalize_absolute_restriction(local_base: &PathBuf, path: &Path) -> Result<PathBuf> {
    let path = resolve_existing_prefix(path)?;
    let relative = match path.strip_prefix(local_base) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => {
            let canonical_base = local_base.canonicalize().wrap_err_with(|| {
                format!("unable to resolve local base {}", local_base.display())
            })?;
            path.strip_prefix(&canonical_base)
                .map(Path::to_path_buf)
                .wrap_err_with(|| {
                    format!(
                        "restricted path {} is outside local base {}",
                        path.display(),
                        local_base.display()
                    )
                })?
        }
    };
    validate_relative_restriction(&relative)?;
    Ok(relative)
}

fn resolve_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(component) => {
                resolved.push(component);
                if resolved.exists() {
                    resolved = resolved.canonicalize().wrap_err_with(|| {
                        format!("unable to resolve restricted path {}", path.display())
                    })?;
                }
            }
        }
    }
    Ok(resolved)
}

fn validate_relative_restriction(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(eyre!(
                    "restricted path {} must not contain .. components",
                    path.display()
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(eyre!(
                    "restricted path {} must be relative to the local base",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn remote_state_dir_for_source(
    source: &ProfileSource,
    remote_server: Option<&str>,
    config: &profile::ProfileConfig,
) -> Result<Option<PathBuf>> {
    match source {
        ProfileSource::Named(_) => Ok(None),
        ProfileSource::File(_) if remote_server.is_some() => Err(eyre!(
            "--profile-file cannot be used with SSH remotes because the derived remote state directory {} is local to this client; use a named profile or a local remote",
            config.remote_state_dir.display()
        )),
        ProfileSource::File(_) => Ok(Some(config.remote_state_dir.clone())),
    }
}

fn local_ids(name: &str) -> Result<LocalIds> {
    let (mid, legacy_mid) = match machine_uid::get() {
        Ok(mid) => (mid.clone(), mid),
        Err(e) => {
            log::warn!(
                "Unable to read machine id: {:?}; using persisted Duet client id",
                e
            );
            (profile::client_id()?, "unknown-machine".to_string())
        }
    };

    Ok(LocalIds {
        stable: stable_local_id(&mid, name),
        legacy: Some(legacy_local_id(&legacy_mid, name)),
    })
}

fn legacy_local_id(machine_id: &str, name: &str) -> String {
    let mut s = DefaultHasher::new();
    machine_id.hash(&mut s);
    name.hash(&mut s);
    format!("{:x}", s.finish())
}

fn stable_local_id(machine_id: &str, name: &str) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(machine_id.as_bytes());
    input.push(0);
    input.extend_from_slice(name.as_bytes());

    let hash = blake2_rfc::blake2b::blake2b(16, &[], &input);
    hash.as_bytes()
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;

    #[test]
    fn precommit_interrupt_wins_over_commit() {
        let interrupt = InterruptState::new();

        assert_eq!(interrupt.request_interrupt(), InterruptRequest::Cancel);
        assert!(!interrupt.try_begin_commit());
        assert!(interrupt.is_cancel_requested());
    }

    #[test]
    fn commit_wins_over_later_interrupt() {
        let interrupt = InterruptState::new();

        assert!(interrupt.try_begin_commit());
        assert_eq!(interrupt.request_interrupt(), InterruptRequest::Deferred);
        assert!(!interrupt.is_cancel_requested());
    }

    #[test]
    fn completed_checkpoint_reopens_interrupt_fence() {
        let interrupt = InterruptState::new();

        assert!(interrupt.try_begin_commit());
        assert!(interrupt.try_reset_after_checkpoint());
        assert_eq!(interrupt.request_interrupt(), InterruptRequest::Cancel);
        assert!(!interrupt.try_begin_commit());
    }

    #[test]
    fn deferred_interrupt_wins_over_checkpoint_reset() {
        let interrupt = InterruptState::new();

        assert!(interrupt.try_begin_commit());
        assert_eq!(interrupt.request_interrupt(), InterruptRequest::Deferred);
        assert!(!interrupt.try_reset_after_checkpoint());
        assert!(!interrupt.is_cancel_requested());
    }

    #[test]
    fn second_interrupt_requests_force() {
        let interrupt = InterruptState::new();

        assert_eq!(interrupt.request_interrupt(), InterruptRequest::Cancel);
        assert_eq!(interrupt.request_interrupt(), InterruptRequest::Force);
    }

    #[test]
    fn postcommit_interrupt_preserves_completed_success() {
        let interrupt = InterruptState::new();

        assert!(interrupt.try_begin_commit());
        assert_eq!(interrupt.request_interrupt(), InterruptRequest::Deferred);
        interrupt.complete();
        assert!(!interrupt.is_cancel_requested());
        assert_eq!(interrupt.request_interrupt(), InterruptRequest::Force);
    }

    fn digested_entry(path: &str, contents: &[u8]) -> scan::DirEntryWithMeta {
        let mut entry = scan::DirEntryWithMeta::test_file_with_size(
            PathBuf::from(path),
            contents.len() as u64,
            adler32::adler32(contents).unwrap(),
        );
        entry.set_digest(Some(sync_ops::content_digest(contents)));
        entry
    }

    #[test]
    fn migration_detects_hidden_adler_collision_as_synthetic_conflict() {
        let local = digested_entry("same", &[10, 10, 10, 10]);
        let remote = digested_entry("same", &[11, 9, 9, 11]);
        assert_eq!(local.checksum(), remote.checksum());
        assert!(!local.same_contents(&remote));

        let actions =
            build_migration_actions(&Vec::new(), &Vec::new(), &vec![local], &vec![remote]);

        assert!(matches!(actions.as_slice(), [Action::Conflict(_, _)]));
    }

    #[test]
    fn migration_delete_modify_conflict_uses_resolvable_change_shapes() {
        let old = digested_entry("file", b"old");
        let remote = digested_entry("file", b"new");
        let local_changes = vec![Change::Removed(old.clone())];
        let remote_changes = vec![Change::Modified(old, remote.clone())];

        let actions =
            build_migration_actions(&local_changes, &remote_changes, &Vec::new(), &vec![remote]);

        assert!(matches!(
            actions.as_slice(),
            [Action::Conflict(Change::Removed(_), Change::Modified(_, _))]
        ));
    }

    #[test]
    fn migration_preserves_one_side_change_direction_using_current_entries() {
        let old = digested_entry("file", b"old");
        let local = digested_entry("file", b"new");
        let remote = old.clone();
        let local_changes = vec![Change::Modified(old, local.clone())];

        let actions = build_migration_actions(
            &local_changes,
            &Vec::new(),
            &vec![local.clone()],
            &vec![remote],
        );

        match actions.as_slice() {
            [Action::Remote(Change::Modified(_, new))] => assert_eq!(new.digest(), local.digest()),
            other => panic!("unexpected migration actions: {:?}", other),
        }
    }

    #[test]
    fn outbound_locations_are_safe_for_legacy_scanners() {
        use crate::scan::location::Location;

        let locations = outbound_scan_locations(&vec![
            Location::Exclude(PathBuf::from(".")),
            Location::Include(PathBuf::new()),
            Location::Include(PathBuf::from("dir/./nested")),
            Location::Exclude(PathBuf::from("dir/nested")),
        ]);

        assert_eq!(locations.len(), 2);
        assert!(locations[0].is_include());
        assert!(locations[0].path().as_os_str().is_empty());
        assert!(locations[1].is_exclude());
        assert_eq!(locations[1].path(), Path::new("dir/nested"));
    }

    #[test]
    fn stream_progress_message_does_not_duplicate_byte_counts() {
        let progress = stream_progress_bar(1024).unwrap();
        assert_eq!(progress.message(), "streaming changes");

        let mut position = 0;
        advance_stream_progress(&progress, &mut position, 1024, 512);
        assert_eq!(progress.message(), "streaming changes");
    }

    #[test]
    fn normalize_path_leaves_relative_paths_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        let normalized = normalize_path(&base, &PathBuf::from("sub/path")).unwrap();

        assert_eq!(normalized, PathBuf::from("sub/path"));
    }

    #[test]
    fn normalize_scope_sorts_deduplicates_and_collapses_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir_all(base.join("tree")).unwrap();

        let scope = normalize_scope(
            &base,
            Some(PathBuf::from("tree")),
            &[
                PathBuf::from("tree/cache/deep"),
                PathBuf::from("outside"),
                PathBuf::from("tree/cache"),
                PathBuf::from("tree/cache"),
            ],
        )
        .unwrap();

        assert_eq!(scope.restrict, PathBuf::from("tree"));
        assert_eq!(
            scope.excludes,
            vec![PathBuf::from("outside"), PathBuf::from("tree/cache")]
        );
    }

    #[test]
    fn normalize_scope_accepts_missing_in_base_suffix_and_rejects_outside_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir(&base).unwrap();

        let scope = normalize_scope(
            &base,
            None,
            &[base.join("missing/deep"), PathBuf::from("ordinary/missing")],
        )
        .unwrap();
        assert_eq!(
            scope.excludes,
            vec![
                PathBuf::from("missing/deep"),
                PathBuf::from("ordinary/missing")
            ]
        );
        assert!(normalize_scope(&base, None, &[dir.path().join("outside")]).is_err());
    }

    #[test]
    fn normalize_scope_preserves_excluded_symlink_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();

        let scope = normalize_scope(&base, None, &[PathBuf::from("link")]).unwrap();

        assert_eq!(scope.excludes, vec![PathBuf::from("link")]);
    }

    #[test]
    fn remote_recovery_requires_plain_profile_name() {
        assert_eq!(
            remote_recovery_profile_name(Path::new("cole")).unwrap(),
            "cole"
        );
        assert!(remote_recovery_profile_name(Path::new("./cole")).is_err());
        assert!(remote_recovery_profile_name(Path::new("/tmp/cole.snp")).is_err());
        assert!(remote_recovery_profile_name(Path::new("work\\old")).is_err());
        assert!(remote_recovery_profile_name(Path::new(".")).is_err());
    }

    #[test]
    fn normalize_path_makes_absolute_paths_relative_to_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        let normalized = normalize_path(&base, &base.join("sub/path")).unwrap();

        assert_eq!(normalized, PathBuf::from("sub/path"));
    }

    #[test]
    fn normalize_path_rejects_absolute_paths_outside_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        assert!(normalize_path(&base, &dir.path().join("other/path"),).is_err());
        assert!(normalize_path(&base, &base.join("../other/path"),).is_err());
    }

    #[test]
    fn normalize_path_allows_resolved_in_base_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(
            normalize_path(&base, &base.join("sub/../path")).unwrap(),
            PathBuf::from("path")
        );
        assert_eq!(
            normalize_path(&base, &PathBuf::from("sub/../path"),).unwrap(),
            PathBuf::from("path")
        );
    }

    #[test]
    fn normalize_path_rejects_symlink_resolved_parent_escape() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(outside.join("child")).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();

        assert!(normalize_path(&base, &PathBuf::from("link/child/../secret")).is_err());
    }

    #[test]
    fn normalize_path_resolves_symlink_before_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let outside_child = dir.path().join("outside/child");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside_child).unwrap();
        std::os::unix::fs::symlink(&outside_child, base.join("link")).unwrap();

        assert!(normalize_path(&base, &PathBuf::from("link/../secret")).is_err());
    }

    #[test]
    fn normalize_path_resolves_symlink_before_missing_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();

        assert!(normalize_path(&base, &PathBuf::from("link/missing/../secret")).is_err());
    }

    #[test]
    fn normalize_path_allows_cwd_relative_parent_within_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let subdir = base.join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();

        let normalized = normalize_path_from_cwd(&base, &PathBuf::from(".."), &subdir).unwrap();

        assert_eq!(normalized, PathBuf::new());
    }

    #[test]
    fn normalize_path_checks_resolved_cwd_relative_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let subdir = base.join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();

        assert_eq!(
            normalize_path_from_cwd(&base, &PathBuf::from("./nested/../secret"), &subdir).unwrap(),
            PathBuf::from("subdir/secret")
        );
        assert_eq!(
            normalize_path_from_cwd(&base, &PathBuf::from("../secret"), &subdir).unwrap(),
            PathBuf::from("secret")
        );
        assert!(normalize_path_from_cwd(&base, &PathBuf::from("../../secret"), &subdir).is_err());
    }

    #[test]
    fn local_id_is_stable_and_profile_specific() {
        assert_eq!(
            stable_local_id("machine", "work"),
            stable_local_id("machine", "work")
        );
        assert_ne!(
            stable_local_id("machine", "work"),
            stable_local_id("machine", "personal")
        );
        assert_ne!(
            stable_local_id("machine", "work"),
            stable_local_id("other", "work")
        );
        assert_eq!(stable_local_id("machine", "work").len(), 32);
    }

    #[test]
    fn profile_file_remote_state_dir_rejects_ssh_remotes() {
        let config = profile::ProfileConfig {
            display_name: "profile".to_string(),
            identity: "profile".to_string(),
            profile: profile::Profile {
                local: "/local".to_string(),
                remote: "ssh host /remote".to_string(),
                locations: Vec::new(),
                ignore: Vec::new(),
                prune: Vec::new(),
            },
            local_state: PathBuf::from("profile.snp"),
            remote_state_dir: PathBuf::from("profile.remotes"),
            server_log: PathBuf::from("profile.remote.log"),
        };

        let error = remote_state_dir_for_source(
            &ProfileSource::File(PathBuf::from("profile.prf")),
            Some("host"),
            &config,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("--profile-file"));
        assert!(error.contains("SSH"));
    }

    #[test]
    fn non_streamed_detail_size_limit_rejects_large_payloads() {
        let actions = vec![Action::Local(Change::Added(
            scan::DirEntryWithMeta::test_file_with_size(
                PathBuf::from("large.bin"),
                MAX_NON_STREAMED_DETAIL_BYTES + 1,
                0,
            ),
        ))];

        let error = preflight_non_streamed_detail_size(&actions, &Vec::new())
            .unwrap_err()
            .to_string();

        assert!(error.contains("cannot stream"));
    }

    #[test]
    fn non_streamed_detail_size_limit_accepts_small_payloads() {
        let actions = vec![Action::Local(Change::Added(
            scan::DirEntryWithMeta::test_file_with_size(PathBuf::from("small.bin"), 1024, 0),
        ))];

        preflight_non_streamed_detail_size(&actions, &Vec::new()).unwrap();
    }

    #[test]
    fn non_streamed_detail_size_limit_counts_actions_once() {
        let actions = vec![Action::Local(Change::Added(
            scan::DirEntryWithMeta::test_file_with_size(
                PathBuf::from("fits.bin"),
                MAX_NON_STREAMED_DETAIL_BYTES,
                0,
            ),
        ))];
        let remote_actions = reverse(&actions);

        preflight_non_streamed_detail_size(&actions, &remote_actions).unwrap();
    }

    #[test]
    fn small_file_byte_frames_stay_in_detail_batches() {
        assert!(!should_apply_file_bytes_as_chunk(
            FILE_BYTE_CHUNK_RPC_THRESHOLD - 1
        ));
    }

    #[test]
    fn large_file_byte_frames_use_dedicated_rpc() {
        assert!(should_apply_file_bytes_as_chunk(
            FILE_BYTE_CHUNK_RPC_THRESHOLD
        ));
    }

    #[test]
    fn medium_file_byte_frame_sizes_route_around_threshold() {
        let cases = [
            (1 * 1024, false),
            (16 * 1024, false),
            (63 * 1024, false),
            (64 * 1024, false),
            (128 * 1024, false),
            (1024 * 1024, false),
            (8 * 1024 * 1024, true),
        ];

        for (len, expected_chunk) in cases {
            assert_eq!(should_apply_file_bytes_as_chunk(len), expected_chunk);
        }
    }

    #[test]
    fn route_file_byte_frames_batches_small_frames_and_splits_large_chunks() {
        let batches = route_file_byte_frames(vec![
            sync_ops::DetailFrame {
                action_index: 7,
                payload: sync_ops::DetailPayload::FileBegin,
            },
            sync_ops::DetailFrame {
                action_index: 7,
                payload: sync_ops::DetailPayload::FileBytes(vec![1; 1024]),
            },
            sync_ops::DetailFrame {
                action_index: 7,
                payload: sync_ops::DetailPayload::FileBytes(vec![2; FILE_BYTE_CHUNK_RPC_THRESHOLD]),
            },
            sync_ops::DetailFrame {
                action_index: 7,
                payload: sync_ops::DetailPayload::FileEnd,
            },
        ]);

        assert_eq!(batches.len(), 3);
        match &batches[0] {
            ApplyDetailBatch::Frames(frames) => {
                assert_eq!(frames.len(), 2);
                assert!(matches!(
                    frames[0].payload,
                    sync_ops::DetailPayload::FileBegin
                ));
                assert!(matches!(
                    frames[1].payload,
                    sync_ops::DetailPayload::FileBytes(_)
                ));
            }
            ApplyDetailBatch::FileByteChunk(_) => panic!("expected buffered detail frames"),
        }
        match &batches[1] {
            ApplyDetailBatch::FileByteChunk(chunk) => {
                assert_eq!(chunk.action_index, 7);
                assert_eq!(chunk.len(), FILE_BYTE_CHUNK_RPC_THRESHOLD);
            }
            ApplyDetailBatch::Frames(_) => panic!("expected dedicated file byte chunk"),
        }
        match &batches[2] {
            ApplyDetailBatch::Frames(frames) => {
                assert_eq!(frames.len(), 1);
                assert!(matches!(
                    frames[0].payload,
                    sync_ops::DetailPayload::FileEnd
                ));
            }
            ApplyDetailBatch::FileByteChunk(_) => panic!("expected trailing detail frames"),
        }
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
    fn remote_stream_performance_requires_profiling_and_capability() {
        let info = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "0.3.2".to_string(),
            capabilities: vec![rpc::CAPABILITY_STREAM_PERFORMANCE.to_string()],
        };
        let without_capability = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "0.3.2".to_string(),
            capabilities: Vec::new(),
        };

        assert!(remote_stream_performance_enabled(true, &info));
        assert!(!remote_stream_performance_enabled(false, &info));
        assert!(!remote_stream_performance_enabled(
            true,
            &without_capability
        ));
    }

    #[test]
    fn staged_apply_requires_capability_and_stream_eligible_plan() {
        let staged = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "test".to_string(),
            capabilities: vec![
                rpc::CAPABILITY_STAGED_APPLY.to_string(),
                rpc::CAPABILITY_STAGING_CAPACITY.to_string(),
                rpc::CAPABILITY_STAGING_RESERVE_ENFORCEMENT.to_string(),
            ],
        };
        let staged_without_capacity = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "old".to_string(),
            capabilities: vec![
                rpc::CAPABILITY_STAGED_APPLY.to_string(),
                rpc::CAPABILITY_STAGING_CAPACITY.to_string(),
            ],
        };
        let legacy = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "test".to_string(),
            capabilities: Vec::new(),
        };

        assert_eq!(
            select_apply_strategy(&staged, true, false).unwrap(),
            ApplyStrategy::StagedStream
        );
        assert_eq!(
            select_apply_strategy(&staged, false, false).unwrap(),
            ApplyStrategy::LegacyNonStream
        );
        assert!(select_apply_strategy(&staged, false, true).is_err());
        assert_eq!(
            select_apply_strategy(&legacy, true, false).unwrap(),
            ApplyStrategy::LegacyStream
        );
        assert_eq!(
            select_apply_strategy(&staged_without_capacity, true, false).unwrap(),
            ApplyStrategy::LegacyStream
        );
        let error = select_apply_strategy(&staged_without_capacity, true, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains(rpc::CAPABILITY_STAGING_RESERVE_ENFORCEMENT));
    }

    #[test]
    fn wave_revalidation_allows_only_cow_logical_overage_and_still_requires_inode() {
        let budget = sync_ops::StagingBudget {
            reserve_bytes: 10,
            usable_bytes: 90,
            budget_bytes: 90,
            cow_clone_supported: true,
        };

        validate_wave_side_capacity("local", 100, 1, true, true, budget, 1, true, 0, 1).unwrap();
        assert!(
            validate_wave_side_capacity("local", 100, 1, true, false, budget, 1, true, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("logical output bytes")
        );
        assert!(
            validate_wave_side_capacity("local", 100, 2, true, true, budget, 1, true, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("inodes")
        );
        assert!(
            validate_wave_side_capacity("local", 100, 1, true, true, budget, 0, true, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("inodes")
        );
        validate_wave_side_capacity("remote", 100, 1, true, true, budget, 0, false, 0, 1).unwrap();
        assert!(
            validate_wave_side_capacity("remote", 100, 2, true, true, budget, 1, false, 0, 1)
                .unwrap_err()
                .to_string()
                .contains("inodes")
        );
    }

    #[test]
    fn checkpointed_staging_falls_back_only_for_default_policy() {
        let legacy_staged = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "test".to_string(),
            capabilities: vec![rpc::CAPABILITY_STAGING_CAPACITY.to_string()],
        };
        let checkpointed = rpc::ServerInfo {
            protocol_version: rpc::PROTOCOL_VERSION,
            duet_version: "test".to_string(),
            capabilities: vec![rpc::CAPABILITY_CHECKPOINTED_STAGING.to_string()],
        };

        assert!(can_use_staging_plan(&legacy_staged, 1, false, true).unwrap());
        assert!(!can_use_staging_plan(&legacy_staged, 2, false, false).unwrap());
        assert!(can_use_staging_plan(&legacy_staged, 2, false, true).is_err());
        assert!(!can_use_staging_plan(&checkpointed, 2, true, false).unwrap());
        assert!(can_use_staging_plan(&checkpointed, 2, true, true).is_err());
        assert!(can_use_staging_plan(&checkpointed, 2, false, true).unwrap());
    }

    #[test]
    fn staged_prepare_barrier_checks_both_action_counts() {
        let local = sync_ops::PreparedApplyReport {
            action_count: 2,
            prepared_file_count: 1,
            prepared_file_bytes: 10,
        };
        let remote = sync_ops::PreparedApplyReport {
            action_count: 3,
            prepared_file_count: 0,
            prepared_file_bytes: 0,
        };

        validate_staged_prepare_barrier(&local, 2, &remote, 3).unwrap();
        assert!(validate_staged_prepare_barrier(&local, 1, &remote, 3).is_err());
        assert!(validate_staged_prepare_barrier(&local, 2, &remote, 2).is_err());
    }

    #[test]
    fn ignored_removal_blockers_remain_blocking_without_explicit_prune() {
        let report = sync_ops::ApplyPreflightReport {
            blockers: vec![sync_ops::RemovalBlocker {
                parent: PathBuf::from("removed"),
                child: PathBuf::from("removed/__pycache__"),
                kind: sync_ops::RemovalBlockerType::Ignored,
                pattern: Some("__pycache__".to_string()),
                prunable: false,
            }],
        };

        let error = ensure_preflight_report_clear("local", &report)
            .unwrap_err()
            .to_string();

        assert!(error.contains("directory removal blockers"), "{}", error);
        assert!(error.contains("--prune-ignored"), "{}", error);
    }

    #[test]
    fn prunable_removal_blockers_do_not_block_preflight_report() {
        let report = sync_ops::ApplyPreflightReport {
            blockers: vec![sync_ops::RemovalBlocker {
                parent: PathBuf::from("removed"),
                child: PathBuf::from("removed/__pycache__"),
                kind: sync_ops::RemovalBlockerType::Prune,
                pattern: Some("__pycache__".to_string()),
                prunable: true,
            }],
        };

        ensure_preflight_report_clear("local", &report).unwrap();
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
