use super::scan::{Change, ContentDigest, DirEntryWithMeta as Entry};
use color_eyre::eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Deserializer, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::actions::Action;
use crate::profile::{Ignore, Prune};
use crate::scan::location::{Location, Locations};

use crate::rustsync::{compare, compare_stream, restore_seek, signature, DeltaOp};
pub use crate::rustsync::{Delta, Signature};

#[allow(dead_code)]
const STAGING_RESERVE_BASIS_POINTS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum StagingReserve {
    Bytes(u64),
    BasisPoints(u16),
}

impl<'de> Deserialize<'de> for StagingReserve {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum WireReserve {
            Bytes(u64),
            BasisPoints(u16),
        }

        match WireReserve::deserialize(deserializer)? {
            WireReserve::Bytes(bytes) => Ok(Self::Bytes(bytes)),
            WireReserve::BasisPoints(basis_points) if basis_points < 10_000 => {
                Ok(Self::BasisPoints(basis_points))
            }
            WireReserve::BasisPoints(_) => Err(serde::de::Error::custom(
                "staging reserve percentage must be less than 100%",
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagingPolicy {
    pub limit_bytes: Option<u64>,
    pub reserve: StagingReserve,
}

impl Default for StagingPolicy {
    fn default() -> Self {
        Self {
            limit_bytes: None,
            reserve: StagingReserve::BasisPoints(500),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagingFilesystemInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub available_inodes: u64,
    pub block_size: u64,
    pub cow_clone_supported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct StagingBudget {
    pub reserve_bytes: u64,
    pub usable_bytes: u64,
    pub budget_bytes: u64,
    pub cow_clone_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct StagingWave {
    pub action_indices: Vec<usize>,
    pub local_reconstructed_bytes: u64,
    pub remote_reconstructed_bytes: u64,
    pub local_staged_regular_outputs: usize,
    pub remote_staged_regular_outputs: usize,
    pub local_exceeds_budget: bool,
    pub remote_exceeds_budget: bool,
    pub local_requires_cow_capacity: bool,
    pub remote_requires_cow_capacity: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct StagingWavePlan {
    pub waves: Vec<StagingWave>,
    pub local_reconstructed_bytes: u64,
    pub remote_reconstructed_bytes: u64,
    pub local_staged_regular_outputs: usize,
    pub remote_staged_regular_outputs: usize,
}

#[derive(Debug)]
#[allow(dead_code)]
struct StagingDependencyGroup {
    action_indices: Vec<usize>,
    local_reconstructed_bytes: u64,
    remote_reconstructed_bytes: u64,
    local_staged_regular_outputs: usize,
    remote_staged_regular_outputs: usize,
    local_cow_candidate_outputs: usize,
    remote_cow_candidate_outputs: usize,
    local_requires_cow_capacity: bool,
    remote_requires_cow_capacity: bool,
    isolated: bool,
}

#[allow(dead_code)]
pub fn plan_staging_waves(
    actions: &[Action],
    local_budget: StagingBudget,
    remote_budget: StagingBudget,
) -> Result<StagingWavePlan> {
    validate_staging_budget("local", local_budget)?;
    validate_staging_budget("remote", remote_budget)?;
    for (index, action) in actions.iter().enumerate() {
        if index > 0 && actions[index - 1].path() >= action.path() {
            return Err(eyre!(
                "staging wave actions must be in strictly increasing path order: {} then {}",
                actions[index - 1].path().display(),
                action.path().display()
            ));
        }
        if action.is_unresolved_conflict() {
            return Err(eyre!(
                "staging wave planner cannot plan unresolved conflict {}",
                action.path().display()
            ));
        }
        if action_has_directory_to_nondirectory_change(action) {
            return Err(eyre!(
                "staging wave planner does not support directory-to-nondirectory replacement {}",
                action.path().display()
            ));
        }
    }

    let mut groups = Vec::new();
    let mut start = 0;
    while start < actions.len() {
        let mut end = start + 1;
        if action_has_directory_change(&actions[start]) {
            while end < actions.len() && actions[end].path().starts_with(actions[start].path()) {
                end += 1;
            }
        }
        groups.push(staging_dependency_group(
            actions,
            start,
            end,
            local_budget,
            remote_budget,
        )?);
        start = end;
    }

    let mut plan = StagingWavePlan {
        waves: Vec::new(),
        local_reconstructed_bytes: 0,
        remote_reconstructed_bytes: 0,
        local_staged_regular_outputs: 0,
        remote_staged_regular_outputs: 0,
    };
    let mut pending: Option<StagingWave> = None;
    for group in groups {
        plan.local_reconstructed_bytes = checked_staging_add(
            plan.local_reconstructed_bytes,
            group.local_reconstructed_bytes,
            "local aggregate reconstructed bytes",
        )?;
        plan.remote_reconstructed_bytes = checked_staging_add(
            plan.remote_reconstructed_bytes,
            group.remote_reconstructed_bytes,
            "remote aggregate reconstructed bytes",
        )?;
        plan.local_staged_regular_outputs = plan
            .local_staged_regular_outputs
            .checked_add(group.local_staged_regular_outputs)
            .ok_or_else(|| eyre!("staging wave local aggregate output count overflow"))?;
        plan.remote_staged_regular_outputs = plan
            .remote_staged_regular_outputs
            .checked_add(group.remote_staged_regular_outputs)
            .ok_or_else(|| eyre!("staging wave remote aggregate output count overflow"))?;

        if group.isolated {
            if let Some(wave) = pending.take() {
                plan.waves.push(wave);
            }
            plan.waves
                .push(staging_wave_from_group(group, local_budget, remote_budget));
            continue;
        }

        if let Some(wave) = pending.as_mut() {
            let local_bytes = checked_staging_add(
                wave.local_reconstructed_bytes,
                group.local_reconstructed_bytes,
                "local wave reconstructed bytes",
            )?;
            let remote_bytes = checked_staging_add(
                wave.remote_reconstructed_bytes,
                group.remote_reconstructed_bytes,
                "remote wave reconstructed bytes",
            )?;
            if local_bytes <= local_budget.budget_bytes
                && remote_bytes <= remote_budget.budget_bytes
            {
                wave.action_indices.extend(group.action_indices);
                wave.local_reconstructed_bytes = local_bytes;
                wave.remote_reconstructed_bytes = remote_bytes;
                wave.local_staged_regular_outputs = wave
                    .local_staged_regular_outputs
                    .checked_add(group.local_staged_regular_outputs)
                    .ok_or_else(|| eyre!("staging wave local output count overflow"))?;
                wave.remote_staged_regular_outputs = wave
                    .remote_staged_regular_outputs
                    .checked_add(group.remote_staged_regular_outputs)
                    .ok_or_else(|| eyre!("staging wave remote output count overflow"))?;
                continue;
            }
            plan.waves.push(pending.take().unwrap());
        }
        pending = Some(staging_wave_from_group(group, local_budget, remote_budget));
    }
    if let Some(wave) = pending {
        plan.waves.push(wave);
    }
    Ok(plan)
}

fn validate_staging_budget(side: &str, budget: StagingBudget) -> Result<()> {
    if budget.budget_bytes > budget.usable_bytes {
        return Err(eyre!(
            "invalid {side} staging budget: target {} exceeds usable {} with reserve {}",
            budget.budget_bytes,
            budget.usable_bytes,
            budget.reserve_bytes
        ));
    }
    Ok(())
}

fn action_has_directory_change(action: &Action) -> bool {
    match action {
        Action::Identical(left, right) => left.is_dir() || right.is_dir(),
        Action::ResolvedLocal((left, right), resolved)
        | Action::ResolvedRemote((left, right), resolved) => {
            left.is_dir() || right.is_dir() || resolved.is_dir()
        }
        _ => action_change(action).is_dir(),
    }
}

fn action_has_directory_to_nondirectory_change(action: &Action) -> bool {
    let is_replacement = |change: &Change| matches!(change, Change::Modified(old, new) if old.is_dir() && !new.is_dir());
    match action {
        Action::Identical(left, right) => is_replacement(left) || is_replacement(right),
        Action::ResolvedLocal((left, right), resolved)
        | Action::ResolvedRemote((left, right), resolved) => {
            is_replacement(left) || is_replacement(right) || is_replacement(resolved)
        }
        _ => is_replacement(action_change(action)),
    }
}

fn staging_dependency_group(
    actions: &[Action],
    start: usize,
    end: usize,
    local_budget: StagingBudget,
    remote_budget: StagingBudget,
) -> Result<StagingDependencyGroup> {
    let mut group = StagingDependencyGroup {
        action_indices: (start..end).collect(),
        local_reconstructed_bytes: 0,
        remote_reconstructed_bytes: 0,
        local_staged_regular_outputs: 0,
        remote_staged_regular_outputs: 0,
        local_cow_candidate_outputs: 0,
        remote_cow_candidate_outputs: 0,
        local_requires_cow_capacity: false,
        remote_requires_cow_capacity: false,
        isolated: false,
    };
    for action in &actions[start..end] {
        let (local, change) = match action {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => (Some(true), change),
            Action::Remote(change) | Action::ResolvedRemote((_, _), change) => {
                (Some(false), change)
            }
            Action::Identical(_, _) => (None, action_change(action)),
            Action::Conflict(_, _) => unreachable!("unresolved conflicts were rejected"),
        };
        let detail_kind = apply_detail_kind_for_change(change);
        if local.is_none() || detail_kind.is_none() {
            continue;
        }
        // Diff staging is planned pessimistically at the full reconstructed logical size.
        let size = change_output_entry(change)?.size();
        if local == Some(true) {
            group.local_reconstructed_bytes = checked_staging_add(
                group.local_reconstructed_bytes,
                size,
                "local dependency-group reconstructed bytes",
            )?;
            group.local_staged_regular_outputs = group
                .local_staged_regular_outputs
                .checked_add(1)
                .ok_or_else(|| eyre!("staging wave local group output count overflow"))?;
            if detail_kind == Some(ApplyDetailKind::Diff) {
                group.local_cow_candidate_outputs += 1;
            }
        } else {
            group.remote_reconstructed_bytes = checked_staging_add(
                group.remote_reconstructed_bytes,
                size,
                "remote dependency-group reconstructed bytes",
            )?;
            group.remote_staged_regular_outputs = group
                .remote_staged_regular_outputs
                .checked_add(1)
                .ok_or_else(|| eyre!("staging wave remote group output count overflow"))?;
            if detail_kind == Some(ApplyDetailKind::Diff) {
                group.remote_cow_candidate_outputs += 1;
            }
        }
    }
    let (local_isolated, local_requires_cow_capacity) = validate_staging_group_side(
        "local",
        group.local_reconstructed_bytes,
        group.local_staged_regular_outputs,
        group.local_cow_candidate_outputs == 1,
        local_budget.cow_clone_supported,
        local_budget,
        actions[start].path(),
    )?;
    let (remote_isolated, remote_requires_cow_capacity) = validate_staging_group_side(
        "remote",
        group.remote_reconstructed_bytes,
        group.remote_staged_regular_outputs,
        group.remote_cow_candidate_outputs == 1,
        remote_budget.cow_clone_supported,
        remote_budget,
        actions[start].path(),
    )?;
    group.isolated |= local_isolated || remote_isolated;
    group.local_requires_cow_capacity = local_requires_cow_capacity;
    group.remote_requires_cow_capacity = remote_requires_cow_capacity;
    Ok(group)
}

fn validate_staging_group_side(
    side: &str,
    required: u64,
    outputs: usize,
    single_cow_candidate: bool,
    cow_clone_supported: bool,
    budget: StagingBudget,
    path: &Path,
) -> Result<(bool, bool)> {
    if required > budget.usable_bytes {
        if outputs == 1 && single_cow_candidate && cow_clone_supported {
            return Ok((true, true));
        }
        return Err(eyre!(
            "{side} staging dependency group at {} requires {required} logical bytes, but only {} bytes are usable after reserving {} bytes; only an isolated single COW diff may rely on prepare-time physical-space monitoring",
            path.display(),
            budget.usable_bytes,
            budget.reserve_bytes
        ));
    }
    if required <= budget.budget_bytes {
        return Ok((false, false));
    }
    if outputs != 1 {
        return Err(eyre!(
            "unsafe {side} staging dependency group at {} requires {required} bytes across {outputs} regular outputs, exceeding wave target {}",
            path.display(),
            budget.budget_bytes
        ));
    }
    Ok((true, false))
}

fn staging_wave_from_group(
    group: StagingDependencyGroup,
    local_budget: StagingBudget,
    remote_budget: StagingBudget,
) -> StagingWave {
    StagingWave {
        action_indices: group.action_indices,
        local_reconstructed_bytes: group.local_reconstructed_bytes,
        remote_reconstructed_bytes: group.remote_reconstructed_bytes,
        local_staged_regular_outputs: group.local_staged_regular_outputs,
        remote_staged_regular_outputs: group.remote_staged_regular_outputs,
        local_exceeds_budget: group.local_reconstructed_bytes > local_budget.budget_bytes,
        remote_exceeds_budget: group.remote_reconstructed_bytes > remote_budget.budget_bytes,
        local_requires_cow_capacity: group.local_requires_cow_capacity,
        remote_requires_cow_capacity: group.remote_requires_cow_capacity,
    }
}

fn checked_staging_add(left: u64, right: u64, context: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| eyre!("staging wave {context} overflow: {left} + {right}"))
}

impl StagingPolicy {
    #[allow(dead_code)]
    pub fn budget(self, filesystem: StagingFilesystemInfo) -> StagingBudget {
        let reserve_bytes = match self.reserve {
            StagingReserve::Bytes(bytes) => bytes,
            StagingReserve::BasisPoints(basis_points) => {
                ((filesystem.total_bytes as u128 * basis_points as u128)
                    / STAGING_RESERVE_BASIS_POINTS as u128) as u64
            }
        };
        let usable_bytes = filesystem.available_bytes.saturating_sub(reserve_bytes);
        let budget_bytes = self
            .limit_bytes
            .map_or(usable_bytes, |limit| limit.min(usable_bytes));
        StagingBudget {
            reserve_bytes,
            usable_bytes,
            budget_bytes,
            cow_clone_supported: filesystem.cow_clone_supported,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StagingWriteCapacity {
    reserve_bytes: u64,
    required_bytes: u64,
    available_above_reserve: u64,
}

const STAGING_CAPACITY_WINDOW_BYTES: u64 = 64 * 1024 * 1024;

fn staging_write_capacity(
    policy: StagingPolicy,
    filesystem: StagingFilesystemInfo,
    requested_bytes: u64,
) -> StagingWriteCapacity {
    let block_size = filesystem.block_size.max(1);
    let rounded_bytes = requested_bytes
        .checked_add(block_size - 1)
        .map(|bytes| bytes / block_size)
        .and_then(|blocks| blocks.checked_mul(block_size))
        .unwrap_or(u64::MAX);
    let required_bytes = rounded_bytes.saturating_add(block_size);
    let reserve_bytes = policy.budget(filesystem).reserve_bytes;
    StagingWriteCapacity {
        reserve_bytes,
        required_bytes,
        available_above_reserve: filesystem.available_bytes.saturating_sub(reserve_bytes),
    }
}

#[derive(Clone)]
struct StagingSpaceMonitor {
    base: PathBuf,
    policy: StagingPolicy,
    state: Arc<Mutex<StagingSpaceMonitorState>>,
}

#[derive(Default)]
struct StagingSpaceMonitorState {
    block_size: u64,
    remaining_credit: u64,
}

impl StagingSpaceMonitor {
    fn check(&self, path: &Path, requested_bytes: u64) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| eyre!("staging capacity monitor lock was poisoned"))?;
        if state.block_size != 0 {
            let charged_bytes = round_staging_charge(requested_bytes, state.block_size);
            if charged_bytes <= state.remaining_credit {
                state.remaining_credit -= charged_bytes;
                return Ok(());
            }
        }

        self.refresh_locked(path, requested_bytes, &mut state)
    }

    fn recheck(&self, path: &Path) -> Result<()> {
        let filesystem = staging_filesystem_info(&self.base)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| eyre!("staging capacity monitor lock was poisoned"))?;
        self.refresh_with_filesystem_locked(path, 0, filesystem, &mut state)
    }

    fn refresh_locked(
        &self,
        path: &Path,
        requested_bytes: u64,
        state: &mut StagingSpaceMonitorState,
    ) -> Result<()> {
        let filesystem = staging_filesystem_info(&self.base)?;
        self.refresh_with_filesystem_locked(path, requested_bytes, filesystem, state)
    }

    fn refresh_with_filesystem_locked(
        &self,
        path: &Path,
        requested_bytes: u64,
        filesystem: StagingFilesystemInfo,
        state: &mut StagingSpaceMonitorState,
    ) -> Result<()> {
        let block_size = filesystem.block_size.max(1);
        let charged_bytes = round_staging_charge(requested_bytes, block_size);
        let capacity = staging_write_capacity(self.policy, filesystem, requested_bytes);
        if capacity.required_bytes > capacity.available_above_reserve {
            return Err(eyre!(
                "staged preparation for {} was aborted before commit: requested {} bytes requires {} bytes with block rounding and safety allowance, but {} bytes are available and {} bytes are reserved",
                path.display(),
                requested_bytes,
                capacity.required_bytes,
                filesystem.available_bytes,
                capacity.reserve_bytes
            ));
        }
        let spendable = capacity.available_above_reserve.saturating_sub(block_size);
        let window = STAGING_CAPACITY_WINDOW_BYTES.max(charged_bytes);
        state.block_size = block_size;
        state.remaining_credit = spendable.min(window).saturating_sub(charged_bytes);
        // statvfs is authoritative for policy enforcement at this instant, but a concurrent
        // writer can still consume space before the write. Native ENOSPC remains precommit.
        Ok(())
    }
}

fn round_staging_charge(requested_bytes: u64, block_size: u64) -> u64 {
    requested_bytes
        .checked_add(block_size - 1)
        .map(|bytes| bytes / block_size)
        .and_then(|blocks| blocks.checked_mul(block_size))
        .unwrap_or(u64::MAX)
        .max(block_size)
}

pub fn staging_filesystem_info(base: &Path) -> Result<StagingFilesystemInfo> {
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(base)
        .wrap_err_with(|| format!("open synchronization base {}", base.display()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(directory.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error())
            .wrap_err_with(|| format!("read filesystem capacity for {}", base.display()));
    }
    let stats = unsafe { stats.assume_init() };
    staging_filesystem_info_from_counts(
        stats.f_blocks as u64,
        stats.f_bavail as u64,
        stats.f_files as u64,
        stats.f_favail as u64,
        stats.f_frsize as u64,
        stats.f_bsize as u64,
        base,
    )
}

pub fn staging_filesystem_info_with_clone_probe(base: &Path) -> Result<StagingFilesystemInfo> {
    let mut info = staging_filesystem_info(base)?;
    info.cow_clone_supported = match probe_cow_clone(base) {
        Ok(supported) => supported,
        Err(error) => {
            log::debug!(
                "copy-on-write staging probe failed for {}: {:#}",
                base.display(),
                error
            );
            false
        }
    };
    Ok(info)
}

fn staging_filesystem_info_from_counts(
    blocks: u64,
    available_blocks: u64,
    total_inodes: u64,
    available_inodes: u64,
    fragment_size: u64,
    block_size_fallback: u64,
    base: &Path,
) -> Result<StagingFilesystemInfo> {
    let block_size = if fragment_size != 0 {
        fragment_size
    } else {
        block_size_fallback
    };
    if block_size == 0 {
        return Err(eyre!(
            "filesystem capacity for {} reported a zero block size",
            base.display()
        ));
    }

    Ok(StagingFilesystemInfo {
        total_bytes: blocks.saturating_mul(block_size),
        available_bytes: available_blocks.saturating_mul(block_size),
        // Preserve the v1 wire layout while representing filesystems without a fixed inode table.
        available_inodes: if total_inodes == 0 {
            u64::MAX
        } else {
            available_inodes
        },
        block_size,
        // A trustworthy clone-capability check requires creating files on the target filesystem.
        cow_clone_supported: false,
    })
}

pub const LEGACY_SIGNATURE_WINDOW: usize = 1024;
pub const DEFAULT_SIGNATURE_WINDOW_MIN: usize = LEGACY_SIGNATURE_WINDOW;
pub const DEFAULT_SIGNATURE_WINDOW_MAX: usize = 64 * 1024;
pub const LEGACY_DETAIL_CHUNK_BYTES: usize = 1024 * 1024;
pub const LEGACY_DETAIL_BATCH_FRAMES: usize = 256;
pub const LEGACY_DETAIL_BATCH_PAYLOAD_BYTES: usize = LEGACY_DETAIL_CHUNK_BYTES;
pub const DEFAULT_DETAIL_CHUNK_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_DETAIL_BATCH_FRAMES: usize = LEGACY_DETAIL_BATCH_FRAMES;
pub const DEFAULT_DETAIL_BATCH_PAYLOAD_BYTES: usize = DEFAULT_DETAIL_CHUNK_BYTES;
const MAX_SIGNATURE_WINDOW: u32 = 16 * 1024 * 1024;
const MAX_DETAIL_CHUNK_BYTES: u32 = 64 * 1024 * 1024;
const MAX_DETAIL_BATCH_FRAMES: u32 = 4096;
const MAX_DETAIL_BATCH_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 128 * 1024;
const SYNCED_MODE_MASK: u32 = 0o7777;
const DEFAULT_OUTPUT_BATCH_FILES: usize = 256;
const DEFAULT_OUTPUT_BATCH_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_OUTPUT_SYNC_WORKERS_MAX: usize = 64;
const MAX_OUTPUT_BATCH_FILES: usize = 512;
const MAX_OUTPUT_BATCH_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_OUTPUT_SYNC_WORKERS: usize = 64;
const OUTPUT_BATCH_FD_HEADROOM: usize = 64;
const ENV_OUTPUT_BATCH_FILES: &str = "DUET_SYNC_OUTPUT_BATCH_FILES";
const ENV_OUTPUT_BATCH_BYTES: &str = "DUET_SYNC_OUTPUT_BATCH_BYTES";
const ENV_OUTPUT_SYNC_WORKERS: &str = "DUET_SYNC_OUTPUT_SYNC_WORKERS";
static TEMP_OUTPUT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "macos")]
const CLONE_NOOWNERCOPY: u32 = 0x0002;
#[cfg(target_os = "macos")]
const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, acl_type: libc::c_int)
        -> libc::c_int;
    fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_get_entry(
        acl: *mut libc::c_void,
        entry_id: libc::c_int,
        entry: *mut *mut libc::c_void,
    ) -> libc::c_int;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ApplyOptions {
    pub prune_ignored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ApplyPreflightReport {
    pub blockers: Vec<RemovalBlocker>,
}

impl ApplyPreflightReport {
    pub fn has_unprunable_blockers(&self) -> bool {
        self.blockers.iter().any(|blocker| !blocker.prunable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalBlocker {
    pub parent: PathBuf,
    pub child: PathBuf,
    pub kind: RemovalBlockerType,
    pub pattern: Option<String>,
    pub prunable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemovalBlockerType {
    Ignored,
    Prune,
    Excluded,
    Unexpected,
}

impl RemovalBlocker {
    fn to_error(&self, prune_ignored: bool) -> color_eyre::eyre::Report {
        removal_blocker_error(
            &self.parent,
            &self.child,
            self.kind.to_internal(self.pattern.as_deref()),
            prune_ignored,
        )
    }
}

impl RemovalBlockerType {
    fn to_internal<'a>(self, pattern: Option<&'a str>) -> RemovalBlockerKind<'a> {
        match self {
            RemovalBlockerType::Ignored => RemovalBlockerKind::Ignored(pattern.unwrap_or("")),
            RemovalBlockerType::Prune => RemovalBlockerKind::Prune(pattern.unwrap_or("")),
            RemovalBlockerType::Excluded => RemovalBlockerKind::Excluded,
            RemovalBlockerType::Unexpected => RemovalBlockerKind::Unexpected,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanPolicy {
    pub locations: Locations,
    pub ignore: Ignore,
    pub prune: Prune,
    pub excludes: Vec<PathBuf>,
}

impl ScanPolicy {
    #[allow(dead_code)]
    pub fn new(locations: Locations, ignore: Ignore) -> Self {
        Self::with_prune(locations, ignore, Vec::new())
    }

    pub fn with_prune(locations: Locations, ignore: Ignore, prune: Prune) -> Self {
        Self {
            locations,
            ignore,
            prune,
            excludes: Vec::new(),
        }
    }

    pub fn with_excludes(mut self, excludes: Vec<PathBuf>) -> Self {
        self.excludes = excludes;
        self
    }
}

const ENV_SIGNATURE_WINDOW_MIN: &str = "DUET_SYNC_SIGNATURE_WINDOW_MIN";
const ENV_SIGNATURE_WINDOW_MAX: &str = "DUET_SYNC_SIGNATURE_WINDOW_MAX";
const ENV_DETAIL_CHUNK_BYTES: &str = "DUET_SYNC_DETAIL_CHUNK_BYTES";
const ENV_DETAIL_BATCH_FRAMES: &str = "DUET_SYNC_DETAIL_BATCH_FRAMES";
const ENV_DETAIL_BATCH_PAYLOAD_BYTES: &str = "DUET_SYNC_DETAIL_BATCH_PAYLOAD_BYTES";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureWindowConfig {
    pub min: usize,
    pub max: usize,
}

impl SignatureWindowConfig {
    pub fn normalized(self) -> Self {
        let min = self.min.max(1);
        let max = self.max.max(min);
        Self { min, max }
    }

    pub fn window_for_size(self, size: u64) -> usize {
        let config = self.normalized();
        let window = integer_sqrt(size).max(config.min as u64);
        window.min(config.max as u64) as usize
    }
}

fn integer_sqrt(n: u64) -> u64 {
    if n <= 1 {
        return n;
    }

    let mut x = n;
    let mut y = (x + n / x) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncTuningRequest {
    pub preferred: SyncTuning,
}

impl SyncTuningRequest {
    pub fn preferred() -> Self {
        Self {
            preferred: SyncTuning::preferred_with_env(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncTuning {
    pub signature_window_min: u32,
    pub signature_window_max: u32,
    pub detail_chunk_bytes: u32,
    pub detail_batch_frames: u32,
    pub detail_batch_payload_bytes: u32,
}

impl SyncTuning {
    pub fn legacy() -> Self {
        Self {
            signature_window_min: LEGACY_SIGNATURE_WINDOW as u32,
            signature_window_max: LEGACY_SIGNATURE_WINDOW as u32,
            detail_chunk_bytes: LEGACY_DETAIL_CHUNK_BYTES as u32,
            detail_batch_frames: LEGACY_DETAIL_BATCH_FRAMES as u32,
            detail_batch_payload_bytes: LEGACY_DETAIL_BATCH_PAYLOAD_BYTES as u32,
        }
    }

    pub fn preferred() -> Self {
        Self {
            signature_window_min: DEFAULT_SIGNATURE_WINDOW_MIN as u32,
            signature_window_max: DEFAULT_SIGNATURE_WINDOW_MAX as u32,
            detail_chunk_bytes: DEFAULT_DETAIL_CHUNK_BYTES as u32,
            detail_batch_frames: DEFAULT_DETAIL_BATCH_FRAMES as u32,
            detail_batch_payload_bytes: DEFAULT_DETAIL_BATCH_PAYLOAD_BYTES as u32,
        }
    }

    pub fn preferred_with_env() -> Self {
        Self::preferred().with_env_overrides_from(|name| std::env::var(name).ok())
    }

    fn with_env_overrides_from(mut self, mut get: impl FnMut(&str) -> Option<String>) -> Self {
        if let Some(value) = get(ENV_SIGNATURE_WINDOW_MIN).and_then(|value| value.parse().ok()) {
            self.signature_window_min = value;
        }
        if let Some(value) = get(ENV_SIGNATURE_WINDOW_MAX).and_then(|value| value.parse().ok()) {
            self.signature_window_max = value;
        }
        if let Some(value) = get(ENV_DETAIL_CHUNK_BYTES).and_then(|value| value.parse().ok()) {
            self.detail_chunk_bytes = value;
        }
        if let Some(value) = get(ENV_DETAIL_BATCH_FRAMES).and_then(|value| value.parse().ok()) {
            self.detail_batch_frames = value;
        }
        if let Some(value) =
            get(ENV_DETAIL_BATCH_PAYLOAD_BYTES).and_then(|value| value.parse().ok())
        {
            self.detail_batch_payload_bytes = value;
        }
        self.normalized()
    }

    pub fn normalized(self) -> Self {
        let signature_window_min = self.signature_window_min.clamp(1, MAX_SIGNATURE_WINDOW);
        let signature_window_max = self
            .signature_window_max
            .clamp(signature_window_min, MAX_SIGNATURE_WINDOW);
        Self {
            signature_window_min,
            signature_window_max,
            detail_chunk_bytes: self.detail_chunk_bytes.clamp(1, MAX_DETAIL_CHUNK_BYTES),
            detail_batch_frames: self.detail_batch_frames.clamp(1, MAX_DETAIL_BATCH_FRAMES),
            detail_batch_payload_bytes: self
                .detail_batch_payload_bytes
                .clamp(1, MAX_DETAIL_BATCH_PAYLOAD_BYTES),
        }
    }

    pub fn negotiate(self, peer: Self) -> Self {
        let local = self.normalized();
        let peer = peer.normalized();
        let signature_window_min = local.signature_window_min.max(peer.signature_window_min);
        let signature_window_max = local
            .signature_window_max
            .min(peer.signature_window_max)
            .max(signature_window_min);
        Self {
            signature_window_min,
            signature_window_max,
            detail_chunk_bytes: local.detail_chunk_bytes.min(peer.detail_chunk_bytes),
            detail_batch_frames: local.detail_batch_frames.min(peer.detail_batch_frames),
            detail_batch_payload_bytes: local
                .detail_batch_payload_bytes
                .min(peer.detail_batch_payload_bytes),
        }
        .normalized()
    }

    pub fn signature_window_config(self) -> SignatureWindowConfig {
        let tuning = self.normalized();
        SignatureWindowConfig {
            min: tuning.signature_window_min as usize,
            max: tuning.signature_window_max as usize,
        }
    }

    pub fn detail_chunk_bytes(self) -> usize {
        self.normalized().detail_chunk_bytes as usize
    }

    pub fn detail_batch_frames(self) -> usize {
        self.normalized().detail_batch_frames as usize
    }

    pub fn detail_batch_payload_bytes(self) -> usize {
        self.normalized().detail_batch_payload_bytes as usize
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureWithPath(PathBuf, Signature);

pub fn validate_relative_path(path: &Path) -> Result<()> {
    validate_relative_path_components(path)?;

    if !path
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(eyre!(
            "path {} must name an entry below the sync base",
            path.display()
        ));
    }

    Ok(())
}

pub fn validate_scan_path(path: &Path) -> Result<()> {
    validate_relative_path_components(path)
}

fn validate_relative_path_components(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(eyre!(
                    "path {} must not contain .. components",
                    path.display()
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(eyre!(
                    "path {} must be relative to the sync base",
                    path.display()
                ));
            }
        }
    }

    Ok(())
}

pub fn safe_join(base: &Path, path: &Path) -> Result<PathBuf> {
    validate_relative_path(path)?;
    Ok(base.join(path))
}

pub fn validate_entries(description: &str, entries: &[Entry]) -> Result<()> {
    for entry in entries {
        validate_relative_path(entry.path()).wrap_err_with(|| {
            format!(
                "invalid {} entry path {}",
                description,
                entry.path().display()
            )
        })?;
    }
    Ok(())
}

pub fn validate_actions(actions: &[Action]) -> Result<()> {
    for action in actions {
        match action {
            Action::Local(change) | Action::Remote(change) => validate_change_paths(change)?,
            Action::Conflict(left, right) => {
                validate_change_paths(left)?;
                validate_change_paths(right)?;
            }
            Action::ResolvedLocal((left, right), change)
            | Action::ResolvedRemote((left, right), change) => {
                validate_change_paths(left)?;
                validate_change_paths(right)?;
                validate_change_paths(change)?;
            }
            Action::Identical(left, right) => {
                validate_change_paths(left)?;
                validate_change_paths(right)?;
            }
        }
    }
    Ok(())
}

pub fn validate_strong_actions(actions: &[Action]) -> Result<()> {
    validate_actions(actions)?;
    for action in actions {
        match action {
            Action::Local(change) | Action::Remote(change) => validate_strong_change(change)?,
            Action::Conflict(left, right) | Action::Identical(left, right) => {
                validate_strong_change(left)?;
                validate_strong_change(right)?;
            }
            Action::ResolvedLocal((left, right), change)
            | Action::ResolvedRemote((left, right), change) => {
                validate_strong_change(left)?;
                validate_strong_change(right)?;
                validate_strong_change(change)?;
            }
        }
    }
    Ok(())
}

fn validate_strong_change(change: &Change) -> Result<()> {
    match change {
        Change::Added(entry) | Change::Removed(entry) => validate_strong_entry(entry),
        Change::Modified(old, new) => {
            validate_strong_entry(old)?;
            validate_strong_entry(new)
        }
    }
}

fn validate_strong_entry(entry: &Entry) -> Result<()> {
    if entry.is_file() && entry.digest().is_none() {
        return Err(eyre!(
            "strong-digest action entry {} is missing its content digest",
            entry.path().display()
        ));
    }
    Ok(())
}

fn validate_signature_window(window: usize) -> Result<()> {
    if window == 0 || window > MAX_SIGNATURE_WINDOW as usize {
        return Err(eyre!(
            "invalid signature window {}, expected 1..={}",
            window,
            MAX_SIGNATURE_WINDOW
        ));
    }
    Ok(())
}

fn validate_delta(delta: &Delta) -> Result<()> {
    validate_signature_window(delta.window)
        .wrap_err_with(|| format!("invalid diff window {}", delta.window))
}

fn next_signature<'a, I>(sig_iter: &mut I, path: &Path) -> Result<&'a Signature>
where
    I: Iterator<Item = &'a SignatureWithPath>,
{
    let signature = sig_iter
        .next()
        .ok_or_else(|| eyre!("missing signature for {}", path.display()))?;
    if signature.0 != path {
        return Err(eyre!(
            "signature path mismatch: expected {}, got {}",
            path.display(),
            signature.0.display()
        ));
    }
    validate_signature_window(signature.1.window)
        .wrap_err_with(|| format!("invalid signature window for {}", signature.0.display()))?;
    Ok(&signature.1)
}

fn validate_change_paths(change: &Change) -> Result<()> {
    match change {
        Change::Added(entry) | Change::Removed(entry) => validate_entry_path(entry),
        Change::Modified(old, new) => {
            validate_entry_path(old)?;
            validate_entry_path(new)?;
            Ok(())
        }
    }
}

fn validate_entry_path(entry: &Entry) -> Result<()> {
    validate_relative_path(entry.path())
        .wrap_err_with(|| format!("invalid action entry path {}", entry.path().display()))
}

pub fn get_signatures_with_config(
    base: &PathBuf,
    actions: &Vec<Action>,
    window_config: SignatureWindowConfig,
) -> Result<Vec<SignatureWithPath>> {
    validate_actions(actions)?;
    let mut signatures: Vec<SignatureWithPath> = Vec::new();
    for action in actions {
        match action {
            Action::Local(Change::Modified(e1, e2))
            | Action::ResolvedLocal((_, _), Change::Modified(e1, e2)) => {
                if e1.is_file() && e2.is_file() && !e1.same_contents(&e2) {
                    let f = fs::File::open(safe_join(base, e1.path())?)?;
                    let block = vec![0; window_config.window_for_size(e1.size())];
                    let sig = signature(f, block)?;
                    signatures.push(SignatureWithPath(e1.path().clone(), sig));
                }
            }
            _ => {}
        }
    }
    Ok(signatures)
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ChangeDetails {
    Contents(Vec<u8>),
    Diff(Delta),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DetailStreamId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApplyStreamId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetailFrame {
    pub action_index: u32,
    pub payload: DetailPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DetailPayload {
    FileBegin,
    FileBytes(Vec<u8>),
    FileEnd,
    DiffBegin,
    DiffCopy { offset: u64, len: u64 },
    DiffBytes(Vec<u8>),
    DiffEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileByteChunk {
    pub action_index: u32,
    pub bytes: serde_bytes::ByteBuf,
}

impl FileByteChunk {
    pub fn new(action_index: u32, bytes: Vec<u8>) -> Self {
        Self {
            action_index,
            bytes: serde_bytes::ByteBuf::from(bytes),
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes.into_vec()
    }
}

pub fn detail_transfer_bytes(actions: &[Action]) -> u64 {
    actions.iter().map(action_detail_bytes).sum()
}

pub fn detail_frame_transfer_bytes(frame: &DetailFrame) -> u64 {
    match &frame.payload {
        DetailPayload::FileBytes(bytes) | DetailPayload::DiffBytes(bytes) => bytes.len() as u64,
        DetailPayload::DiffCopy { len, .. } => *len,
        DetailPayload::FileBegin
        | DetailPayload::FileEnd
        | DetailPayload::DiffBegin
        | DetailPayload::DiffEnd => 0,
    }
}

pub fn detail_frames_transfer_bytes(frames: &[DetailFrame]) -> u64 {
    frames.iter().map(detail_frame_transfer_bytes).sum()
}

fn action_detail_bytes(action: &Action) -> u64 {
    let change = match action {
        Action::Local(change)
        | Action::Remote(change)
        | Action::ResolvedLocal((_, _), change)
        | Action::ResolvedRemote((_, _), change) => change,
        Action::Conflict(_, _) | Action::Identical(_, _) => return 0,
    };

    match change {
        Change::Removed(_) => 0,
        Change::Added(entry) => entry.is_file().then_some(entry.size()).unwrap_or(0),
        Change::Modified(old, new) => {
            if new.is_file() && (!old.is_file() || !old.same_contents(new)) {
                new.size()
            } else {
                0
            }
        }
    }
}

pub fn can_stream_details(actions: &[Action]) -> bool {
    actions.iter().all(|action| {
        let change = match action {
            Action::Local(change)
            | Action::Remote(change)
            | Action::ResolvedLocal((_, _), change)
            | Action::ResolvedRemote((_, _), change) => change,
            Action::Conflict(_, _) | Action::Identical(_, _) => return true,
        };

        !matches!(change, Change::Modified(old, new) if old.is_dir() && !new.is_dir())
    })
}

#[allow(dead_code)]
pub fn preflight_apply(base: &PathBuf, actions: &Vec<Action>) -> Result<()> {
    preflight_apply_with_policy(base, actions, None, ApplyOptions::default())
}

pub fn preflight_apply_with_policy(
    base: &PathBuf,
    actions: &Vec<Action>,
    scan_policy: Option<&ScanPolicy>,
    apply_options: ApplyOptions,
) -> Result<()> {
    validate_actions(actions)?;
    preflight_source_reads(base, actions)?;
    let removal_policy = RemovalBlockerPolicy::new(scan_policy, apply_options)?;
    let report = removal_blocker_report(base, actions, &removal_policy)?;
    if let Some(blocker) = report.blockers.iter().find(|blocker| !blocker.prunable) {
        return Err(blocker.to_error(apply_options.prune_ignored));
    }
    preflight_removed_directories(base, actions, &removal_policy)?;

    let readonly_metadata_changes = readonly_directory_metadata_changes(actions);
    let planned_directories = planned_destination_directories(actions);
    for target in apply_metadata_targets(actions) {
        let target_path = safe_join(base, &target)?;
        fs::symlink_metadata(&target_path).wrap_err_with(|| {
            format!(
                "unable to preflight destination metadata for {}",
                target_path.display()
            )
        })?;
    }

    for mutation in apply_parent_mutations(actions) {
        let target = mutation.path;
        let Some(parent) = target.parent() else {
            continue;
        };
        let parent_path = if parent.as_os_str().is_empty() {
            base.clone()
        } else {
            safe_join(base, parent)?
        };
        if !parent_path.try_exists().wrap_err_with(|| {
            format!(
                "unable to preflight destination parent {}",
                parent_path.display()
            )
        })? {
            if planned_directories.contains(parent) {
                continue;
            }
            if mutation.allow_missing_parent {
                preflight_directory_writable_or_creatable(&parent_path, "destination parent")?;
                continue;
            }
            return Err(eyre!(
                "destination parent {} does not exist",
                parent_path.display()
            ));
        }
        let meta = fs::symlink_metadata(&parent_path).wrap_err_with(|| {
            format!(
                "unable to preflight destination parent {}",
                parent_path.display()
            )
        })?;
        if !meta.is_dir() {
            return Err(eyre!(
                "destination parent {} is not a directory",
                parent_path.display()
            ));
        }
        if owner_write_execute(meta.permissions().mode()) {
            continue;
        }

        if !mutation.allow_writable_guard || readonly_metadata_changes.contains(parent) {
            return Err(eyre!(
                "destination parent {} is not writable",
                parent_path.display()
            ));
        }
    }
    Ok(())
}

pub fn preflight_apply_report(
    base: &PathBuf,
    actions: &Vec<Action>,
    scan_policy: Option<&ScanPolicy>,
    apply_options: ApplyOptions,
) -> Result<ApplyPreflightReport> {
    validate_actions(actions)?;
    let removal_policy = RemovalBlockerPolicy::new(scan_policy, apply_options)?;
    removal_blocker_report(base, actions, &removal_policy)
}

pub fn preflight_state_save(state_path: &Path) -> Result<()> {
    let parent = state_path.parent().ok_or_else(|| {
        eyre!(
            "state file {} has no parent directory",
            state_path.display()
        )
    })?;

    preflight_directory_writable_or_creatable(parent, "state directory")?;

    if state_path
        .try_exists()
        .wrap_err_with(|| format!("unable to preflight state file {}", state_path.display()))?
    {
        let meta = fs::symlink_metadata(state_path).wrap_err_with(|| {
            format!(
                "unable to preflight state file metadata for {}",
                state_path.display()
            )
        })?;
        if !meta.is_file() {
            return Err(eyre!(
                "state path {} is not a regular file",
                state_path.display()
            ));
        }
        if !owner_writable(meta.permissions().mode()) {
            return Err(eyre!("state file {} is not writable", state_path.display()));
        }
        fs::OpenOptions::new()
            .write(true)
            .open(state_path)
            .wrap_err_with(|| {
                format!(
                    "unable to open state file {} for writing",
                    state_path.display()
                )
            })?;
    }

    Ok(())
}

pub fn check_apply_attempt_clear(state_path: &Path) -> Result<()> {
    if let Some(description) = describe_apply_attempt(state_path)? {
        return Err(eyre!("{}", description));
    }
    Ok(())
}

pub fn describe_apply_attempt(state_path: &Path) -> Result<Option<String>> {
    let marker_path = apply_attempt_path(state_path)?;
    if !marker_path.try_exists().wrap_err_with(|| {
        format!(
            "unable to check apply recovery marker {}",
            marker_path.display()
        )
    })? {
        return Ok(None);
    }

    let marker = fs::read_to_string(&marker_path).wrap_err_with(|| {
        format!(
            "unable to read apply recovery marker {}",
            marker_path.display()
        )
    })?;
    Ok(Some(apply_attempt_description(
        state_path,
        &marker_path,
        &marker,
    )))
}

pub fn start_apply_attempt(
    side: &str,
    state_path: &Path,
    base: &Path,
    actions: &[Action],
    attempt_id: Option<&str>,
) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    let parent = marker_path.parent().ok_or_else(|| {
        eyre!(
            "apply recovery marker {} has no parent directory",
            marker_path.display()
        )
    })?;
    create_dir_all_durable(parent).wrap_err_with(|| {
        format!(
            "unable to create apply recovery marker directory {}",
            parent.display()
        )
    })?;
    let contents = apply_attempt_contents(side, state_path, base, "apply", actions, attempt_id);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&marker_path)
        .and_then(|mut file| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()
        }) {
        Ok(()) => sync_directory(parent).wrap_err_with(|| {
            format!(
                "unable to sync apply recovery marker directory {}",
                parent.display()
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(&marker_path).wrap_err_with(|| {
                format!(
                    "unable to read apply recovery marker {}",
                    marker_path.display()
                )
            })?;
            if existing == contents {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&marker_path)
                    .wrap_err_with(|| {
                        format!(
                            "unable to open existing apply recovery marker {}",
                            marker_path.display()
                        )
                    })?;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                file.sync_all().wrap_err_with(|| {
                    format!(
                        "unable to sync existing apply recovery marker {}",
                        marker_path.display()
                    )
                })?;
                sync_directory(parent).wrap_err_with(|| {
                    format!(
                        "unable to sync apply recovery marker directory {}",
                        parent.display()
                    )
                })
            } else {
                Err(eyre!(
                    "{}",
                    apply_attempt_description(state_path, &marker_path, &existing)
                ))
            }
        }
        Err(e) => Err(e).wrap_err_with(|| {
            format!(
                "unable to create apply recovery marker {}",
                marker_path.display()
            )
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ApplyAttemptPhase {
    Preparing,
    Prepared,
    Committing,
    Committed,
    StateSave,
    Finished,
}

impl ApplyAttemptPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Prepared => "prepared",
            Self::Committing => "committing",
            Self::Committed => "committed",
            Self::StateSave => "state-save",
            Self::Finished => "finished",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "preparing" => Some(Self::Preparing),
            "prepared" => Some(Self::Prepared),
            "committing" => Some(Self::Committing),
            "committed" => Some(Self::Committed),
            "state-save" => Some(Self::StateSave),
            "finished" => Some(Self::Finished),
            _ => None,
        }
    }
}

#[allow(dead_code)]
pub(crate) fn start_staged_apply_attempt(
    side: &str,
    state_path: &Path,
    base: &Path,
    actions: &[Action],
    attempt_id: &str,
) -> Result<()> {
    if attempt_id.is_empty() || attempt_id.contains(['\n', '\r']) {
        return Err(eyre!(
            "staged apply attempt ID must be non-empty and single-line"
        ));
    }
    if base.as_os_str().as_bytes().contains(&b'\n') || base.as_os_str().as_bytes().contains(&b'\r')
    {
        return Err(eyre!("staged apply base path must be single-line"));
    }
    if state_path.as_os_str().as_bytes().contains(&b'\n')
        || state_path.as_os_str().as_bytes().contains(&b'\r')
    {
        return Err(eyre!("staged apply state path must be single-line"));
    }
    if actions.iter().any(|action| {
        let path = action.path().as_os_str().as_bytes();
        path.contains(&b'\n') || path.contains(&b'\r')
    }) {
        return Err(eyre!("staged apply action paths must be single-line"));
    }
    let marker_path = apply_attempt_path(state_path)?;
    let parent = marker_path.parent().ok_or_else(|| {
        eyre!(
            "apply recovery marker {} has no parent directory",
            marker_path.display()
        )
    })?;
    create_dir_all_durable(parent)?;
    let mut contents = apply_attempt_contents(
        side,
        state_path,
        base,
        ApplyAttemptPhase::Preparing.as_str(),
        actions,
        Some(attempt_id),
    );
    contents.replace_range(.."duet-apply-attempt-v1".len(), "duet-apply-attempt-v2");
    write_new_apply_marker(state_path, &marker_path, parent, &contents)
}

#[allow(dead_code)]
fn write_new_apply_marker(
    state_path: &Path,
    marker_path: &Path,
    parent: &Path,
    contents: &str,
) -> Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(marker_path)
        .and_then(|mut file| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()
        }) {
        Ok(()) => sync_directory(parent),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(marker_path)?;
            if existing == contents {
                Ok(())
            } else {
                Err(eyre!(
                    "{}",
                    apply_attempt_description(state_path, marker_path, &existing)
                ))
            }
        }
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "unable to create apply recovery marker {}",
                marker_path.display()
            )
        }),
    }
}

fn transition_staged_apply_attempt(
    state_path: &Path,
    attempt_id: &str,
    expected: &[ApplyAttemptPhase],
    next: ApplyAttemptPhase,
) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    let contents = fs::read_to_string(&marker_path)?;
    let marker = parse_v2_apply_attempt(&contents)?;
    if marker.attempt_id != attempt_id {
        return Err(eyre!(
            "staged apply attempt ID mismatch: expected {}, marker contains {}",
            attempt_id,
            marker.attempt_id
        ));
    }
    if !expected.contains(&marker.phase) {
        return Err(eyre!(
            "cannot transition staged apply attempt {} from {} to {}",
            attempt_id,
            marker.phase.as_str(),
            next.as_str()
        ));
    }
    let updated = replace_marker_line(&contents, "phase: ", next.as_str())?;
    write_apply_marker_atomic(&marker_path, &updated)
}

pub(crate) fn mark_staged_apply_attempt_state_save(
    state_path: &Path,
    attempt_id: &str,
) -> Result<()> {
    transition_staged_apply_attempt(
        state_path,
        attempt_id,
        &[ApplyAttemptPhase::Committed],
        ApplyAttemptPhase::StateSave,
    )
}

#[allow(dead_code)]
pub(crate) fn finish_staged_apply_attempt(state_path: &Path, attempt_id: &str) -> Result<()> {
    transition_staged_apply_attempt(
        state_path,
        attempt_id,
        &[ApplyAttemptPhase::StateSave],
        ApplyAttemptPhase::Finished,
    )?;
    finish_apply_attempt(state_path)
}

#[allow(dead_code)]
pub(crate) fn abort_staged_apply_attempt(state_path: &Path, attempt_id: &str) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    let contents = fs::read_to_string(&marker_path)?;
    let marker = parse_v2_apply_attempt(&contents)?;
    if marker.attempt_id != attempt_id {
        return Err(eyre!(
            "staged apply attempt ID mismatch: expected {}, marker contains {}",
            attempt_id,
            marker.attempt_id
        ));
    }
    cleanup_v2_precommit_stage(&contents)?;
    finish_apply_attempt(state_path)
}

fn replace_marker_line(contents: &str, prefix: &str, value: &str) -> Result<String> {
    let mut found = false;
    let mut updated = String::new();
    for line in contents.lines() {
        if line.starts_with(prefix) {
            if found {
                return Err(eyre!(
                    "apply recovery marker has duplicate {} field",
                    prefix.trim()
                ));
            }
            found = true;
            updated.push_str(prefix);
            updated.push_str(value);
        } else {
            updated.push_str(line);
        }
        updated.push('\n');
    }
    if !found {
        return Err(eyre!(
            "apply recovery marker is missing {} field",
            prefix.trim()
        ));
    }
    Ok(updated)
}

fn write_apply_marker_atomic(marker_path: &Path, contents: &str) -> Result<()> {
    use atomicwrites::{AllowOverwrite, AtomicFile};
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    AtomicFile::new(marker_path, AllowOverwrite)
        .write_with_options(
            |file| {
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                file.write_all(contents.as_bytes())?;
                file.sync_all()
            },
            options,
        )
        .wrap_err_with(|| {
            format!(
                "unable to update apply recovery marker {}",
                marker_path.display()
            )
        })?;
    let parent = marker_path
        .parent()
        .ok_or_else(|| eyre!("apply marker has no parent"))?;
    sync_directory(parent)
}

pub fn mark_apply_attempt_state_save(
    side: &str,
    state_path: &Path,
    base: &Path,
    actions: &[Action],
    attempt_id: Option<&str>,
) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    let existing = fs::read_to_string(&marker_path).wrap_err_with(|| {
        format!(
            "unable to read apply recovery marker {}",
            marker_path.display()
        )
    })?;
    if existing.starts_with("duet-apply-attempt-v2\n") {
        let attempt_id =
            attempt_id.ok_or_else(|| eyre!("V2 apply marker requires an attempt ID"))?;
        return mark_staged_apply_attempt_state_save(state_path, attempt_id);
    }
    let mut contents =
        apply_attempt_contents(side, state_path, base, "state-save", actions, attempt_id);
    for line in existing.lines().filter(|line| {
        line.starts_with("staged-file: ")
            || line.starts_with("committed-operation: ")
            || line.starts_with("committed-step: ")
    }) {
        contents.push_str(line);
        contents.push('\n');
    }
    use atomicwrites::{AllowOverwrite, AtomicFile};
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    AtomicFile::new(&marker_path, AllowOverwrite)
        .write_with_options(
            |file| {
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                file.write_all(contents.as_bytes())
            },
            options,
        )
        .wrap_err_with(|| {
            format!(
                "unable to atomically update apply recovery marker {}",
                marker_path.display()
            )
        })?;
    Ok(())
}

fn record_committed_action(attempt_state: Option<&Path>, action: &Action) -> Result<()> {
    let Some(state_path) = attempt_state else {
        return Ok(());
    };
    let Some(change) = applied_change(action) else {
        return Ok(());
    };
    let marker_path = apply_attempt_path(state_path)?;
    let line = format!(
        "committed-operation: {} {}\n",
        change_operation(change),
        action.path().display()
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&marker_path)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .wrap_err_with(|| {
            format!(
                "unable to record committed operation in apply recovery marker {}",
                marker_path.display()
            )
        })?;
    Ok(())
}

fn record_staged_file(attempt_state: Option<&Path>, path: &Path) -> Result<()> {
    let Some(state_path) = attempt_state else {
        return Ok(());
    };
    let marker_path = apply_attempt_path(state_path)?;
    let line = format!("staged-file: {}\n", path.display());
    fs::OpenOptions::new()
        .append(true)
        .open(&marker_path)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .wrap_err_with(|| {
            format!(
                "unable to record staged path in apply recovery marker {}",
                marker_path.display()
            )
        })?;
    Ok(())
}

fn record_committed_step(attempt_state: Option<&Path>, operation: &str, path: &Path) -> Result<()> {
    let Some(state_path) = attempt_state else {
        return Ok(());
    };
    let marker_path = apply_attempt_path(state_path)?;
    let line = format!("committed-step: {} {}\n", operation, path.display());
    fs::OpenOptions::new()
        .append(true)
        .open(&marker_path)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .wrap_err_with(|| {
            format!(
                "unable to record committed step in apply recovery marker {}",
                marker_path.display()
            )
        })?;
    Ok(())
}

fn applied_change(action: &Action) -> Option<&Change> {
    match action {
        Action::Local(change) | Action::ResolvedLocal((_, _), change) => Some(change),
        _ => None,
    }
}

pub fn finish_apply_attempt(state_path: &Path) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    match fs::remove_file(&marker_path) {
        Ok(()) => {
            let parent = marker_path.parent().ok_or_else(|| {
                eyre!(
                    "apply recovery marker {} has no parent directory",
                    marker_path.display()
                )
            })?;
            sync_directory(parent).wrap_err_with(|| {
                format!(
                    "unable to sync cleared apply recovery marker directory {}",
                    parent.display()
                )
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).wrap_err_with(|| {
            format!(
                "unable to remove apply recovery marker {}",
                marker_path.display()
            )
        }),
    }
}

pub fn clear_apply_attempt(state_path: &Path) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    let marker = fs::read_to_string(&marker_path).wrap_err_with(|| {
        format!(
            "unable to read apply recovery marker {}",
            marker_path.display()
        )
    })?;
    if marker.starts_with("duet-apply-attempt-v2\n") {
        let parsed = parse_v2_apply_attempt(&marker)?;
        if matches!(
            parsed.phase,
            ApplyAttemptPhase::Preparing | ApplyAttemptPhase::Prepared
        ) {
            cleanup_v2_precommit_stage(&marker)?;
        }
        return finish_apply_attempt(state_path);
    }
    if !marker.starts_with("duet-apply-attempt-v1\n") {
        return Err(eyre!(
            "refusing to remove malformed apply recovery marker {}",
            marker_path.display()
        ));
    }
    finish_apply_attempt(state_path)
}

fn apply_attempt_path(state_path: &Path) -> Result<PathBuf> {
    let file_name = state_path.file_name().ok_or_else(|| {
        eyre!(
            "state file {} has no file name for apply recovery marker",
            state_path.display()
        )
    })?;
    Ok(state_path.with_file_name(format!(".{}.duet-apply", file_name.to_string_lossy())))
}

fn apply_attempt_contents(
    side: &str,
    state_path: &Path,
    base: &Path,
    phase: &str,
    actions: &[Action],
    attempt_id: Option<&str>,
) -> String {
    let mut paths: Vec<_> = actions.iter().map(|action| action.path().clone()).collect();
    paths.sort();
    paths.dedup();
    let operations = apply_attempt_operations(actions);
    let unstaged_operations = apply_attempt_unstaged_operations(actions);

    let mut contents = format!(
        "duet-apply-attempt-v1\nside: {}\nbase: {}\nstate: {}\nphase: {}\npath-count: {}\noperation-count: {}\nunstaged-operation-count: {}\n",
        side,
        base.display(),
        state_path.display(),
        phase,
        paths.len(),
        operations.len(),
        unstaged_operations.len()
    );

    if let Some(attempt_id) = attempt_id {
        contents.push_str("attempt-id: ");
        contents.push_str(attempt_id);
        contents.push('\n');
    }

    for path in paths.iter().take(50) {
        contents.push_str("path: ");
        contents.push_str(&path.display().to_string());
        contents.push('\n');
    }
    if paths.len() > 50 {
        contents.push_str("paths-truncated: true\n");
    }
    for operation in operations.iter().take(50) {
        contents.push_str("operation: ");
        contents.push_str(operation);
        contents.push('\n');
    }
    if operations.len() > 50 {
        contents.push_str("operations-truncated: true\n");
    }
    for operation in unstaged_operations.iter().take(50) {
        contents.push_str("unstaged-operation: ");
        contents.push_str(operation);
        contents.push('\n');
    }
    if unstaged_operations.len() > 50 {
        contents.push_str("unstaged-operations-truncated: true\n");
    }
    contents
}

fn apply_attempt_operations(actions: &[Action]) -> Vec<String> {
    let mut operations: Vec<_> = actions
        .iter()
        .map(|action| {
            let change = action_change(action);
            format!("{} {}", change_operation(change), change.path().display())
        })
        .collect();
    operations.sort();
    operations.dedup();
    operations
}

fn apply_attempt_unstaged_operations(actions: &[Action]) -> Vec<String> {
    let mut operations: Vec<_> = actions
        .iter()
        .filter_map(|action| {
            unstaged_change_operation(action_change(action))
                .map(|op| format!("{} {}", op, action.path().display()))
        })
        .collect();
    operations.sort();
    operations.dedup();
    operations
}

fn action_change(action: &Action) -> &Change {
    match action {
        Action::Local(change)
        | Action::Remote(change)
        | Action::ResolvedLocal((_, _), change)
        | Action::ResolvedRemote((_, _), change) => change,
        Action::Conflict(left, _) | Action::Identical(left, _) => left,
    }
}

fn unstaged_change_operation(change: &Change) -> Option<&'static str> {
    match change {
        Change::Added(entry) => {
            if entry.is_file() {
                Some("metadata")
            } else {
                Some(entry_operation("add", entry))
            }
        }
        Change::Removed(entry) => Some(entry_operation("remove", entry)),
        Change::Modified(old, new) => {
            if old.is_file() && new.is_file() && !old.same_contents(new) {
                Some("metadata")
            } else if old.is_file() == new.is_file()
                && old.is_dir() == new.is_dir()
                && old.is_symlink() == new.is_symlink()
            {
                Some(change_operation(change))
            } else {
                Some("replace")
            }
        }
    }
}

fn change_operation(change: &Change) -> &'static str {
    match change {
        Change::Added(entry) => entry_operation("add", entry),
        Change::Removed(entry) => entry_operation("remove", entry),
        Change::Modified(old, new) => {
            if old.is_file() && new.is_file() && !old.same_contents(new) {
                "modify-file"
            } else if old.is_dir() && new.is_dir() {
                "modify-dir-metadata"
            } else if old.is_symlink() && new.is_symlink() {
                "modify-symlink"
            } else if old.is_file() == new.is_file()
                && old.is_dir() == new.is_dir()
                && old.is_symlink() == new.is_symlink()
            {
                "modify-metadata"
            } else {
                "replace"
            }
        }
    }
}

fn entry_operation(prefix: &'static str, entry: &Entry) -> &'static str {
    match (prefix, entry.is_dir(), entry.is_symlink()) {
        ("add", true, _) => "add-dir",
        ("add", _, true) => "add-symlink",
        ("add", _, _) => "add-file",
        ("remove", true, _) => "remove-dir",
        ("remove", _, true) => "remove-symlink",
        ("remove", _, _) => "remove-file",
        _ => prefix,
    }
}

#[derive(Debug, Default)]
struct ApplyAttemptMarker {
    phase: Option<String>,
    operations: Vec<String>,
    unstaged_operations: Vec<String>,
    staged_paths: Vec<String>,
    committed_operations: Vec<String>,
    committed_steps: Vec<String>,
}

#[derive(Debug)]
struct V2ApplyAttemptMarker {
    attempt_id: String,
    phase: ApplyAttemptPhase,
    stage_parent: Option<(PathBuf, DirectoryIdentity)>,
    stage: Option<(String, DirectoryIdentity)>,
    entries: HashMap<String, FileIdentity>,
}

fn parse_v2_apply_attempt(contents: &str) -> Result<V2ApplyAttemptMarker> {
    if !contents.starts_with("duet-apply-attempt-v2\n") {
        return Err(eyre!("apply recovery marker is not V2"));
    }
    let mut attempt_id = None;
    let mut phase = None;
    let mut stage_parent = None;
    let mut stage = None;
    let mut entries = HashMap::new();
    for line in contents.lines().skip(1) {
        if let Some(value) = line.strip_prefix("attempt-id: ") {
            if attempt_id.is_some() {
                return Err(eyre!("V2 apply marker has duplicate attempt IDs"));
            }
            attempt_id = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("phase: ") {
            if phase.is_some() {
                return Err(eyre!("V2 apply marker has duplicate phases"));
            }
            phase = ApplyAttemptPhase::parse(value);
            if phase.is_none() {
                return Err(eyre!("V2 apply marker has an invalid phase"));
            }
        } else if let Some(value) = line.strip_prefix("stage-parent: ") {
            if stage_parent.is_some() {
                return Err(eyre!("V2 apply marker has duplicate staging parents"));
            }
            let (path, identity) = parse_marker_path_identity(value)?;
            stage_parent = Some((PathBuf::from(path), identity));
        } else if let Some(value) = line.strip_prefix("stage: ") {
            if stage.is_some() {
                return Err(eyre!("V2 apply marker has duplicate staging directories"));
            }
            let (name, identity) = parse_marker_path_identity(value)?;
            stage = Some((name.to_string(), identity));
        } else if let Some(value) = line.strip_prefix("stage-entry: ") {
            let (name, identity) = parse_marker_path_identity(value)?;
            if entries
                .insert(
                    name.to_string(),
                    FileIdentity {
                        dev: identity.dev,
                        ino: identity.ino,
                    },
                )
                .is_some()
            {
                return Err(eyre!("V2 apply marker has duplicate staged entries"));
            }
        }
    }
    Ok(V2ApplyAttemptMarker {
        attempt_id: attempt_id.ok_or_else(|| eyre!("V2 apply marker is missing attempt ID"))?,
        phase: phase.ok_or_else(|| eyre!("V2 apply marker has an invalid or missing phase"))?,
        stage_parent,
        stage,
        entries,
    })
}

fn parse_marker_path_identity(value: &str) -> Result<(&str, DirectoryIdentity)> {
    let mut parts = value.rsplitn(3, ' ');
    let ino = parts.next().and_then(|value| value.parse().ok());
    let dev = parts.next().and_then(|value| value.parse().ok());
    let path = parts.next();
    match (path, dev, ino) {
        (Some(path), Some(dev), Some(ino)) if !path.is_empty() => {
            Ok((path, DirectoryIdentity { dev, ino }))
        }
        _ => Err(eyre!("malformed staged identity in V2 apply marker")),
    }
}

fn append_v2_marker_line_durable(state_path: &Path, attempt_id: &str, line: &str) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    let contents = fs::read_to_string(&marker_path)?;
    let marker = parse_v2_apply_attempt(&contents)?;
    if marker.attempt_id != attempt_id || marker.phase != ApplyAttemptPhase::Preparing {
        return Err(eyre!(
            "staged apply marker does not match active preparing attempt"
        ));
    }
    let mut file = fs::OpenOptions::new().append(true).open(&marker_path)?;
    file.write_all(line.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn append_v2_marker_line(state_path: &Path, line: &str) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    let mut file = fs::OpenOptions::new().append(true).open(&marker_path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn sync_v2_marker_entries(state_path: &Path, attempt_id: &str) -> Result<()> {
    let marker_path = apply_attempt_path(state_path)?;
    let contents = fs::read_to_string(&marker_path)?;
    let marker = parse_v2_apply_attempt(&contents)?;
    if marker.attempt_id != attempt_id || marker.phase != ApplyAttemptPhase::Preparing {
        return Err(eyre!(
            "staged apply marker does not match active preparing attempt"
        ));
    }
    fs::OpenOptions::new()
        .write(true)
        .open(&marker_path)?
        .sync_all()?;
    Ok(())
}

fn record_v2_stage(state_path: &Path, attempt_id: &str, staging: &StagingArea) -> Result<()> {
    let shared = &staging.shared;
    let metadata = shared.directory.metadata()?;
    append_v2_marker_line_durable(
        state_path,
        attempt_id,
        &format!(
            "stage-parent: {} {} {}\nstage: {} {} {}\n",
            shared.stage_parent_path.display(),
            shared.stage_parent_identity.dev,
            shared.stage_parent_identity.ino,
            shared.name.to_string_lossy(),
            metadata.dev(),
            metadata.ino()
        ),
    )
}

fn record_v2_stage_entry(state_path: &Path, output: &TempOutput) -> Result<()> {
    let metadata = output
        .file
        .as_ref()
        .ok_or_else(|| eyre!("new staged output is closed"))?
        .metadata()?;
    append_v2_marker_line(
        state_path,
        &format!(
            "stage-entry: {} {} {}\n",
            output.output_name.to_string_lossy(),
            metadata.dev(),
            metadata.ino()
        ),
    )
}

fn cleanup_v2_precommit_stage(contents: &str) -> Result<()> {
    let marker = parse_v2_apply_attempt(contents)?;
    if !matches!(
        marker.phase,
        ApplyAttemptPhase::Preparing | ApplyAttemptPhase::Prepared
    ) {
        return Err(eyre!(
            "refusing precommit cleanup for staged apply attempt {} in {} phase",
            marker.attempt_id,
            marker.phase.as_str()
        ));
    }
    let entries = marker.entries;
    let (Some((parent_path, parent_identity)), Some((stage_name, stage_identity))) =
        (marker.stage_parent, marker.stage)
    else {
        if entries.is_empty() {
            return Ok(());
        }
        return Err(eyre!(
            "V2 apply marker has staged entries without a recorded stage"
        ));
    };
    let parent = open_directory_for_access(&parent_path)?;
    verify_directory_handle_identity(
        &parent,
        parent_identity,
        &parent_path,
        "staging parent directory",
    )?;
    verify_path_identity(&parent_path, &parent, "staging parent directory")?;
    let stage_name = path_component_cstring(stage_name.as_ref(), "recorded stage name")?;
    let stage_stat = match fstatat_nofollow(parent.as_raw_fd(), &stage_name) {
        Ok(stage_stat) => stage_stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return sync_retained_directory(
                &parent_path,
                &parent,
                Some(parent_identity),
                "staging parent directory",
            );
        }
        Err(error) => return Err(error.into()),
    };
    if stage_stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        || stage_stat.st_dev as u64 != stage_identity.dev
        || stage_stat.st_ino as u64 != stage_identity.ino
    {
        return Err(eyre!(
            "refusing to remove substituted staging directory {}",
            parent_path
                .join(stage_name.to_string_lossy().as_ref())
                .display()
        ));
    }
    let stage = openat_file(
        parent.as_raw_fd(),
        &stage_name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    verify_directory_handle_identity(
        &stage,
        stage_identity,
        &parent_path.join(stage_name.to_string_lossy().as_ref()),
        "staging directory",
    )?;

    let names = directory_entry_names(&stage)?;
    if names.iter().any(|name| !entries.contains_key(name)) {
        return Err(eyre!(
            "refusing to clean staged apply attempt: staging directory contains unexpected entries"
        ));
    }
    for name in names {
        let expected = entries[&name];
        let name_c = path_component_cstring(name.as_ref(), "recorded staged entry")?;
        let actual = fstatat_nofollow(stage.as_raw_fd(), &name_c)?;
        if actual.st_mode & libc::S_IFMT != libc::S_IFREG
            || actual.st_uid != unsafe { libc::geteuid() }
            || actual.st_dev as u64 != expected.dev
            || actual.st_ino as u64 != expected.ino
        {
            return Err(eyre!(
                "refusing to remove substituted staged entry {}",
                name
            ));
        }
        unlinkat(stage.as_raw_fd(), &name_c, 0)?;
    }
    stage.sync_all()?;
    match unlinkat(parent.as_raw_fd(), &stage_name, libc::AT_REMOVEDIR) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_retained_directory(
        &parent_path,
        &parent,
        Some(parent_identity),
        "staging parent directory",
    )
}

fn directory_entry_names(directory: &fs::File) -> Result<Vec<String>> {
    let duplicated = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(duplicated) };
    if stream.is_null() {
        unsafe { libc::close(duplicated) };
        return Err(io::Error::last_os_error().into());
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let name = std::str::from_utf8(name.to_bytes())
            .map_err(|_| eyre!("staging directory contains a non-UTF-8 entry"))?;
        names.push(name.to_string());
    }
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(names)
}

fn parse_apply_attempt_marker(marker: &str) -> ApplyAttemptMarker {
    let mut parsed = ApplyAttemptMarker::default();
    for line in marker.lines() {
        if let Some(phase) = line.strip_prefix("phase: ") {
            parsed.phase = Some(phase.to_string());
        } else if let Some(operation) = line.strip_prefix("operation: ") {
            parsed.operations.push(operation.to_string());
        } else if let Some(operation) = line.strip_prefix("unstaged-operation: ") {
            parsed.unstaged_operations.push(operation.to_string());
        } else if let Some(path) = line.strip_prefix("staged-file: ") {
            parsed.staged_paths.push(path.to_string());
        } else if let Some(operation) = line.strip_prefix("committed-operation: ") {
            parsed.committed_operations.push(operation.to_string());
        } else if let Some(step) = line.strip_prefix("committed-step: ") {
            parsed.committed_steps.push(step.to_string());
        }
    }
    parsed
}

pub(crate) fn create_dir_all_durable(path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current
        .try_exists()
        .wrap_err_with(|| format!("unable to check directory {}", current.display()))?
    {
        missing.push(current.to_path_buf());
        current = current
            .parent()
            .ok_or_else(|| eyre!("directory {} has no existing ancestor", path.display()))?;
    }
    fs::create_dir_all(path)
        .wrap_err_with(|| format!("unable to create directory {}", path.display()))?;
    for directory in missing.iter().rev() {
        let parent = directory
            .parent()
            .ok_or_else(|| eyre!("directory {} has no parent", directory.display()))?;
        sync_directory(parent)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    dev: u64,
    ino: u64,
}

fn directory_identity(
    directory: &fs::File,
    path: &Path,
    description: &str,
) -> Result<DirectoryIdentity> {
    let metadata = directory
        .metadata()
        .wrap_err_with(|| format!("failed to inspect {} {}", description, path.display()))?;
    if !metadata.is_dir() {
        return Err(eyre!(
            "{} {} is not a directory",
            description,
            path.display()
        ));
    }
    Ok(DirectoryIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

fn directory_path_identity(path: &Path, description: &str) -> Result<DirectoryIdentity> {
    let metadata = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to inspect {} {}", description, path.display()))?;
    if !metadata.is_dir() {
        return Err(eyre!(
            "{} {} is not a directory",
            description,
            path.display()
        ));
    }
    Ok(DirectoryIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

fn verify_directory_handle_identity(
    directory: &fs::File,
    expected: DirectoryIdentity,
    path: &Path,
    description: &str,
) -> Result<()> {
    let actual = directory_identity(directory, path, description)?;
    if actual != expected {
        return Err(eyre!(
            "{} path {} no longer refers to the recorded directory",
            description,
            path.display()
        ));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    let access = open_directory_for_access(path)
        .wrap_err_with(|| format!("unable to open directory for syncing {}", path.display()))?;
    sync_retained_directory(path, &access, None, "directory being synced")
}

fn sync_recorded_directory(path: &Path, expected: DirectoryIdentity) -> Result<()> {
    let access = open_directory_for_access(path).wrap_err_with(|| {
        format!(
            "unable to reopen published destination parent {}",
            path.display()
        )
    })?;
    sync_retained_directory(
        path,
        &access,
        Some(expected),
        "published destination parent",
    )
}

fn verify_recorded_directory_path(path: &Path, expected: DirectoryIdentity) -> Result<()> {
    let actual = fs::symlink_metadata(path).wrap_err_with(|| {
        format!(
            "failed to inspect published destination parent path {}",
            path.display()
        )
    })?;
    if !actual.is_dir() || actual.dev() != expected.dev || actual.ino() != expected.ino {
        return Err(eyre!(
            "published destination parent path {} no longer refers to the recorded directory",
            path.display()
        ));
    }
    Ok(())
}

fn sync_retained_directory(
    path: &Path,
    access: &fs::File,
    expected: Option<DirectoryIdentity>,
    description: &str,
) -> Result<()> {
    verify_path_identity(path, access, description)?;
    if let Some(expected) = expected {
        verify_directory_handle_identity(access, expected, path, description)?;
    }
    match access.sync_all() {
        Ok(()) => {
            verify_path_identity(path, access, description)?;
            return Ok(());
        }
        Err(error) if access_descriptor_needs_readable_sync(&error) => {}
        Err(error) => {
            return Err(error)
                .wrap_err_with(|| format!("unable to sync directory {}", path.display()));
        }
    }

    let original_mode = access
        .metadata()
        .wrap_err_with(|| format!("unable to inspect directory for syncing {}", path.display()))?
        .permissions()
        .mode()
        & 0o7777;
    verify_path_identity(path, access, description)?;
    set_retained_directory_mode(&access, original_mode | 0o500, path).wrap_err_with(|| {
        format!(
            "unable to temporarily make directory readable for syncing {}",
            path.display()
        )
    })?;

    let readable = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path);
    let readable = match readable {
        Ok(readable) => readable,
        Err(error) => {
            let restore = set_retained_directory_mode(&access, original_mode, path);
            return match restore {
                Ok(()) => Err(error).wrap_err_with(|| {
                    format!("unable to reopen directory for syncing {}", path.display())
                }),
                Err(restore) => Err(eyre!(
                    "unable to reopen directory for syncing {}: {}; additionally failed to restore mode {:04o}: {}",
                    path.display(),
                    error,
                    original_mode,
                    restore
                )),
            };
        }
    };

    // Restore immediately through the readable inode handle. If the process exits
    // before this point, the still-durable apply marker blocks normal recovery.
    if let Err(error) = set_retained_directory_mode(&access, original_mode, path) {
        let _ = set_retained_directory_mode(&access, original_mode, path);
        return Err(error).wrap_err_with(|| {
            format!(
                "unable to restore directory mode after opening it for syncing {}",
                path.display()
            )
        });
    }
    verify_same_directory_handles(&access, &readable, path, "directory being synced")?;
    if let Some(expected) = expected {
        verify_directory_handle_identity(&readable, expected, path, description)?;
    }
    verify_path_identity(path, &readable, description)?;
    readable
        .sync_all()
        .wrap_err_with(|| format!("unable to sync directory {}", path.display()))?;
    verify_path_identity(path, &readable, description)
}

fn metadata_synced_directories(base: &Path, actions: &[Action]) -> HashSet<PathBuf> {
    actions
        .iter()
        .filter_map(applied_change)
        .filter_map(|change| match change {
            Change::Added(entry) | Change::Modified(_, entry) if entry.is_dir() => {
                Some(base.join(entry.path()))
            }
            _ => None,
        })
        .collect()
}

fn complete_apply_phase(
    base: &Path,
    actions: &[Action],
    attempt_state: Option<&Path>,
    already_synced: &HashSet<PathBuf>,
) -> Result<()> {
    // Destination directory durability is intentionally batched here, before the
    // caller saves state; until these barriers complete, the durable apply marker
    // remains authoritative.
    let metadata_synced_directories = metadata_synced_directories(base, actions);
    let mut directories = HashSet::new();
    for action in actions {
        let mut path = base.join(action.path());
        if path != base && !path.pop() {
            continue;
        }
        loop {
            if path == base {
                break;
            }
            if !already_synced.contains(&path)
                && !metadata_synced_directories.contains(&path)
                && path
                    .try_exists()
                    .wrap_err_with(|| format!("unable to check affected path {}", path.display()))?
                && fs::symlink_metadata(&path)
                    .wrap_err_with(|| {
                        format!("unable to inspect affected path {}", path.display())
                    })?
                    .is_dir()
            {
                directories.insert(path.clone());
            }
            if path == base || !path.pop() {
                break;
            }
        }
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_directory(&directory)?;
    }
    if !already_synced.contains(base) {
        sync_directory(&base.join("."))?;
    }
    if let Some(state_path) = attempt_state {
        let marker_path = apply_attempt_path(state_path)?;
        fs::OpenOptions::new()
            .read(true)
            .open(&marker_path)
            .and_then(|file| file.sync_all())
            .wrap_err_with(|| {
                format!(
                    "unable to sync accumulated apply recovery records {}",
                    marker_path.display()
                )
            })?;
    }
    Ok(())
}

fn apply_attempt_description(state_path: &Path, marker_path: &Path, marker: &str) -> String {
    format!(
        "previous Duet apply attempt did not finish: {}\n{}\nRecovery marker contents:\n{}",
        marker_path.display(),
        apply_attempt_recovery_advice(state_path, marker_path, marker),
        marker.trim_end()
    )
}

fn apply_attempt_recovery_advice(state_path: &Path, marker_path: &Path, marker: &str) -> String {
    let v2_marker = marker.starts_with("duet-apply-attempt-v2");
    let staged_precommit = v2_marker
        && parse_v2_apply_attempt(marker)
            .map(|marker| {
                matches!(
                    marker.phase,
                    ApplyAttemptPhase::Preparing | ApplyAttemptPhase::Prepared
                )
            })
            .unwrap_or(false);
    let marker = parse_apply_attempt_marker(marker);
    let mut advice = if staged_precommit {
        "Recovery: this staged apply did not begin committing, so synchronized target paths were not changed. `duet recover --clear` can identity-check and remove the recorded Duet staging directory before clearing the marker."
            .to_string()
    } else if marker.phase.as_deref() == Some("state-save") {
        "Recovery: filesystem changes were applied, but Duet state may not have been saved on this side. Inspect and reconcile both synchronized trees and snapshots before explicitly clearing the markers; do not rerun sync against stale snapshots."
            .to_string()
    } else {
        "Recovery: filesystem changes may have been partially applied on this side. Inspect and reconcile the listed paths and snapshots on both sides before explicitly clearing the markers."
            .to_string()
    };

    if staged_precommit {
        advice.push_str(&format!(
            " Inspect and safely clean this marker with `duet recover --clear {}`; do not remove the marker directly because it owns the recorded staging directory.",
            state_path.display()
        ));
    } else if v2_marker {
        advice.push_str(&format!(
            " Inspect this marker with `duet recover {}`. After the required inspection and reconciliation, use `duet recover --clear {}`; do not remove the marker directly because it may own recorded staging.",
            state_path.display(),
            state_path.display()
        ));
    } else {
        advice.push_str(&format!(
            " Inspect this marker with `duet recover {}`. After inspection and reconciliation, remove it with `duet recover --clear {}` or manually with `rm {}`.",
            state_path.display(),
            state_path.display(),
            marker_path.display()
        ));
    }
    advice.push_str(" Run recovery commands on the side where this state file exists; for remote-side markers, SSH to the remote host first.");

    if marker
        .operations
        .iter()
        .any(|operation| operation.starts_with("remove-") || operation.starts_with("replace "))
    {
        advice.push_str(" Removed or replaced paths may need to be restored or reconciled before removing the marker.");
    }
    if marker.operations.iter().any(|operation| {
        operation.starts_with("modify-metadata")
            || operation.starts_with("modify-dir-metadata")
            || operation.starts_with("modify-symlink")
    }) {
        advice.push_str(" Metadata operations may have changed modes, mtimes, or symlink targets without matching state.");
    }
    if marker
        .operations
        .iter()
        .any(|operation| operation.starts_with("add-file") || operation.starts_with("modify-file"))
    {
        advice.push_str(
            " File contents may have changed even if the matching state save did not finish.",
        );
    }
    if !marker.committed_operations.is_empty() {
        advice.push_str(&format!(
            " The marker records {} committed operation(s); inspect those paths first before removing the marker.",
            marker.committed_operations.len()
        ));
    }
    if !marker.committed_steps.is_empty() {
        advice.push_str(&format!(
            " The marker records {} committed apply step(s); inspect those step paths before removing the marker.",
            marker.committed_steps.len()
        ));
    }
    if !marker.staged_paths.is_empty() {
        let existing_staged_paths = marker
            .staged_paths
            .iter()
            .filter(|path| Path::new(path.as_str()).exists())
            .count();
        if existing_staged_paths == 0 {
            advice.push_str(&format!(
                " The marker lists {} staged temporary path(s), but none still exist; they were likely published or already cleaned up.",
                marker.staged_paths.len()
            ));
        } else {
            advice.push_str(&format!(
                " The marker lists {} staged temporary path(s), and {} still exist; inspect them before removing leftover temporary paths.",
                marker.staged_paths.len(),
                existing_staged_paths
            ));
        }
    }
    if !marker.unstaged_operations.is_empty() {
        advice.push_str(&format!(
            " The marker lists {} unstaged operation(s) that commit directly; inspect those paths for partial changes.",
            marker.unstaged_operations.len()
        ));
    }

    advice
}

fn preflight_directory_writable_or_creatable(path: &Path, description: &str) -> Result<()> {
    if path
        .try_exists()
        .wrap_err_with(|| format!("unable to preflight {} {}", description, path.display()))?
    {
        return preflight_existing_writable_directory(path, description);
    }

    let ancestor = nearest_existing_ancestor(path).ok_or_else(|| {
        eyre!(
            "unable to find existing ancestor for {} {}",
            description,
            path.display()
        )
    })?;
    preflight_existing_writable_directory(&ancestor, "state directory ancestor")
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.try_exists().ok()? {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn preflight_existing_writable_directory(path: &Path, description: &str) -> Result<()> {
    let meta = fs::symlink_metadata(path).wrap_err_with(|| {
        format!(
            "unable to preflight {} metadata for {}",
            description,
            path.display()
        )
    })?;
    if !meta.is_dir() {
        return Err(eyre!(
            "{} {} is not a directory",
            description,
            path.display()
        ));
    }
    if !owner_write_execute(meta.permissions().mode()) {
        return Err(eyre!("{} {} is not writable", description, path.display()));
    }
    Ok(())
}

fn owner_writable(mode: u32) -> bool {
    mode & 0o200 != 0
}

fn owner_write_execute(mode: u32) -> bool {
    mode & 0o300 == 0o300
}

fn readonly_directory_metadata_changes(actions: &Vec<Action>) -> HashSet<PathBuf> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Remote(Change::Modified(old, new))
            | Action::ResolvedRemote((_, _), Change::Modified(old, new))
                if old.is_dir()
                    && new.is_dir()
                    && owner_writable(old.mode())
                    && !owner_writable(new.mode()) =>
            {
                Some(new.path().clone())
            }
            _ => None,
        })
        .collect()
}

fn planned_destination_directories(actions: &Vec<Action>) -> HashSet<PathBuf> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Local(Change::Added(entry))
            | Action::ResolvedLocal((_, _), Change::Added(entry))
                if entry.is_dir() =>
            {
                Some(entry.path().clone())
            }
            Action::Local(Change::Modified(old, new))
            | Action::ResolvedLocal((_, _), Change::Modified(old, new))
                if !old.is_dir() && new.is_dir() =>
            {
                Some(new.path().clone())
            }
            _ => None,
        })
        .collect()
}

struct ParentMutation {
    path: PathBuf,
    allow_writable_guard: bool,
    allow_missing_parent: bool,
}

fn apply_parent_mutations(actions: &Vec<Action>) -> Vec<ParentMutation> {
    let mut mutations = Vec::new();
    for action in actions {
        match action {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => match change {
                Change::Removed(e) => mutations.push(ParentMutation {
                    path: e.path().clone(),
                    allow_writable_guard: false,
                    allow_missing_parent: false,
                }),
                Change::Added(e) => mutations.push(ParentMutation {
                    path: e.path().clone(),
                    allow_writable_guard: e.is_file(),
                    allow_missing_parent: true,
                }),
                Change::Modified(old, new) if old.is_file() && new.is_file() => {
                    if !old.same_contents(new) {
                        mutations.push(ParentMutation {
                            path: new.path().clone(),
                            allow_writable_guard: true,
                            allow_missing_parent: false,
                        });
                    }
                }
                Change::Modified(old, new) if old.is_dir() && new.is_dir() => {}
                Change::Modified(old, new) if old.is_symlink() && new.is_symlink() => {
                    if old.target() != new.target() {
                        mutations.push(ParentMutation {
                            path: new.path().clone(),
                            allow_writable_guard: false,
                            allow_missing_parent: false,
                        });
                    }
                }
                Change::Modified(_, new) => mutations.push(ParentMutation {
                    path: new.path().clone(),
                    allow_writable_guard: new.is_file(),
                    allow_missing_parent: false,
                }),
            },
            _ => {}
        }
    }
    mutations
}

struct RemovalBlockerPolicy {
    locations: Locations,
    ignore: Vec<(String, regex::Regex)>,
    prune: Vec<(String, regex::Regex)>,
    prune_ignored: bool,
    excludes: Vec<PathBuf>,
}

enum RemovalBlockerKind<'a> {
    Ignored(&'a str),
    Prune(&'a str),
    Excluded,
    Unexpected,
}

impl<'a> RemovalBlockerKind<'a> {
    fn blocker_type(&self) -> RemovalBlockerType {
        match self {
            RemovalBlockerKind::Ignored(_) => RemovalBlockerType::Ignored,
            RemovalBlockerKind::Prune(_) => RemovalBlockerType::Prune,
            RemovalBlockerKind::Excluded => RemovalBlockerType::Excluded,
            RemovalBlockerKind::Unexpected => RemovalBlockerType::Unexpected,
        }
    }

    fn pattern(&self) -> Option<&'a str> {
        match self {
            RemovalBlockerKind::Ignored(pattern) | RemovalBlockerKind::Prune(pattern) => {
                Some(pattern)
            }
            RemovalBlockerKind::Excluded | RemovalBlockerKind::Unexpected => None,
        }
    }
}

impl RemovalBlockerPolicy {
    fn new(scan_policy: Option<&ScanPolicy>, apply_options: ApplyOptions) -> Result<Self> {
        let Some(scan_policy) = scan_policy else {
            return Ok(Self {
                locations: Vec::new(),
                ignore: Vec::new(),
                prune: Vec::new(),
                prune_ignored: apply_options.prune_ignored,
                excludes: Vec::new(),
            });
        };

        use fnmatch_regex::glob_to_regex;
        let compile_patterns =
            |patterns: &[String], kind: &str| -> Result<Vec<(String, regex::Regex)>> {
                let mut compiled = Vec::new();
                for pattern in patterns {
                    compiled.push((
                        pattern.clone(),
                        glob_to_regex(pattern)
                            .wrap_err_with(|| format!("invalid {kind} pattern {pattern}"))?,
                    ));
                }
                Ok(compiled)
            };
        let ignore = compile_patterns(&scan_policy.ignore, "ignore")?;
        let prune = compile_patterns(&scan_policy.prune, "prune")?;

        let locations = crate::scan::location::canonicalize(&scan_policy.locations);
        Ok(Self {
            locations,
            ignore,
            prune,
            prune_ignored: apply_options.prune_ignored,
            excludes: scan_policy.excludes.clone(),
        })
    }

    fn classify<'a>(&'a self, relative_path: &Path) -> RemovalBlockerKind<'a> {
        if self
            .excludes
            .iter()
            .any(|exclude| relative_path.starts_with(exclude))
        {
            return RemovalBlockerKind::Excluded;
        }
        if self.is_excluded(relative_path) {
            return RemovalBlockerKind::Excluded;
        }
        if let Some(pattern) = self.matching_prune_pattern(relative_path) {
            return RemovalBlockerKind::Prune(pattern);
        }
        if let Some(pattern) = self.matching_ignore_pattern(relative_path) {
            return RemovalBlockerKind::Ignored(pattern);
        }
        RemovalBlockerKind::Unexpected
    }

    fn should_prune(&self, kind: &RemovalBlockerKind<'_>) -> bool {
        matches!(kind, RemovalBlockerKind::Prune(_))
            || (matches!(kind, RemovalBlockerKind::Ignored(_)) && self.prune_ignored)
    }

    fn matching_prune_pattern(&self, relative_path: &Path) -> Option<&str> {
        Self::matching_pattern(&self.prune, relative_path)
    }

    fn matching_ignore_pattern(&self, relative_path: &Path) -> Option<&str> {
        Self::matching_pattern(&self.ignore, relative_path)
    }

    fn matching_pattern<'a>(
        patterns: &'a [(String, regex::Regex)],
        relative_path: &Path,
    ) -> Option<&'a str> {
        let filename = relative_path.file_name()?.to_str()?;
        patterns
            .iter()
            .find(|(_, regex)| regex.is_match(filename))
            .map(|(pattern, _)| pattern.as_str())
    }

    fn is_excluded(&self, relative_path: &Path) -> bool {
        if self
            .excludes
            .iter()
            .any(|exclude| relative_path.starts_with(exclude))
        {
            return true;
        }
        let mut best: Option<&Location> = None;
        for location in &self.locations {
            if location_applies(location.path(), relative_path) {
                let location_specificity = path_specificity(location.path());
                let replace = best
                    .map(|best| {
                        let best_specificity = path_specificity(best.path());
                        location_specificity > best_specificity
                    })
                    .unwrap_or(true);
                if replace {
                    best = Some(location);
                }
            }
        }
        best.map(Location::is_exclude).unwrap_or(false)
    }
}

fn location_applies(location: &Path, relative_path: &Path) -> bool {
    location.as_os_str().is_empty()
        || location == Path::new(".")
        || relative_path == location
        || relative_path.starts_with(location)
}

fn path_specificity(path: &Path) -> usize {
    if path.as_os_str().is_empty() || path == Path::new(".") {
        0
    } else {
        path.components().count()
    }
}

fn removal_blocker_error(
    dirname: &Path,
    child: &Path,
    kind: RemovalBlockerKind<'_>,
    prune_ignored: bool,
) -> color_eyre::eyre::Report {
    match kind {
        RemovalBlockerKind::Ignored(pattern) => {
            let action = if prune_ignored {
                "the ignored child appeared after pruning was checked; rerun sync to recheck and prune it"
            } else {
                "ignored content is not deleted by default; remove it manually or rerun with --prune-ignored if it is disposable"
            };
            eyre!(
                "destination directory {} is not empty; ignored child {} matched pattern {:?} and would prevent removal. {}",
                dirname.display(),
                child.display(),
                pattern,
                action
            )
        }
        RemovalBlockerKind::Prune(pattern) => eyre!(
            "destination directory {} is not empty; prunable child {} matched pattern {:?} and would prevent removal. Duet will prune this disposable content before removing the synced parent",
            dirname.display(),
            child.display(),
            pattern
        ),
        RemovalBlockerKind::Excluded => eyre!(
            "destination directory {} is not empty; excluded child {} would prevent removal. Excluded content is outside the sync selection and is not deleted automatically",
            dirname.display(),
            child.display()
        ),
        RemovalBlockerKind::Unexpected => eyre!(
            "destination directory {} is not empty; unexpected child {} would prevent removal",
            dirname.display(),
            child.display()
        ),
    }
}

fn preflight_removed_directories(
    base: &Path,
    actions: &Vec<Action>,
    policy: &RemovalBlockerPolicy,
) -> Result<()> {
    let removed_paths = removed_destination_paths(actions);
    for path in removed_paths.iter() {
        let dirname = safe_join(base, path)?;
        if dirname.is_dir() {
            preflight_removed_directory_contents(base, &dirname, &removed_paths, policy)?;
        }
    }
    Ok(())
}

fn removed_destination_paths(actions: &Vec<Action>) -> HashSet<PathBuf> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Local(Change::Removed(entry))
            | Action::ResolvedLocal((_, _), Change::Removed(entry)) => Some(entry.path().clone()),
            Action::Local(Change::Modified(old, new))
            | Action::ResolvedLocal((_, _), Change::Modified(old, new))
                if old.is_dir() && !new.is_dir() =>
            {
                Some(old.path().clone())
            }
            _ => None,
        })
        .collect()
}

fn removal_blocker_report(
    base: &Path,
    actions: &Vec<Action>,
    policy: &RemovalBlockerPolicy,
) -> Result<ApplyPreflightReport> {
    let removed_paths = removed_destination_paths(actions);
    let mut report = ApplyPreflightReport::default();
    for path in removed_paths.iter() {
        let dirname = safe_join(base, path)?;
        if dirname.is_dir() {
            collect_removed_directory_blockers(
                base,
                &dirname,
                &removed_paths,
                policy,
                &mut report,
            )?;
        }
    }
    Ok(report)
}

fn collect_removed_directory_blockers(
    base: &Path,
    dirname: &Path,
    removed_paths: &HashSet<PathBuf>,
    policy: &RemovalBlockerPolicy,
    report: &mut ApplyPreflightReport,
) -> Result<()> {
    for entry in fs::read_dir(dirname).wrap_err_with(|| {
        format!(
            "unable to preflight directory removal {}",
            dirname.display()
        )
    })? {
        let entry = entry.wrap_err_with(|| {
            format!(
                "unable to preflight directory removal entry in {}",
                dirname.display()
            )
        })?;
        let path = entry.path();
        let relative_path = path.strip_prefix(base).wrap_err_with(|| {
            format!(
                "unable to preflight directory removal path {}",
                path.display()
            )
        })?;
        if !removed_paths.contains(relative_path) {
            let kind = policy.classify(relative_path);
            if policy.should_prune(&kind) {
                let file_type = entry.file_type().wrap_err_with(|| {
                    format!("unable to preflight directory entry {}", path.display())
                })?;
                let base_dev = fs::symlink_metadata(base)
                    .wrap_err_with(|| {
                        format!("failed to read sync base metadata for {}", base.display())
                    })?
                    .dev();
                preflight_prunable_ignored_path(base, &path, file_type.is_dir(), base_dev, policy)?;
            }
            report.blockers.push(RemovalBlocker {
                parent: dirname.to_path_buf(),
                child: path.clone(),
                kind: kind.blocker_type(),
                pattern: kind.pattern().map(str::to_string),
                prunable: policy.should_prune(&kind),
            });
            continue;
        }
        if entry
            .file_type()
            .wrap_err_with(|| format!("unable to preflight directory entry {}", path.display()))?
            .is_dir()
        {
            collect_removed_directory_blockers(base, &path, removed_paths, policy, report)?;
        }
    }
    Ok(())
}

fn preflight_removed_directory_contents(
    base: &Path,
    dirname: &Path,
    removed_paths: &HashSet<PathBuf>,
    policy: &RemovalBlockerPolicy,
) -> Result<()> {
    for entry in fs::read_dir(dirname).wrap_err_with(|| {
        format!(
            "unable to preflight directory removal {}",
            dirname.display()
        )
    })? {
        let entry = entry.wrap_err_with(|| {
            format!(
                "unable to preflight directory removal entry in {}",
                dirname.display()
            )
        })?;
        let path = entry.path();
        let relative_path = path.strip_prefix(base).wrap_err_with(|| {
            format!(
                "unable to preflight directory removal path {}",
                path.display()
            )
        })?;
        if !removed_paths.contains(relative_path) {
            let kind = policy.classify(relative_path);
            if policy.should_prune(&kind) {
                let file_type = entry.file_type().wrap_err_with(|| {
                    format!("unable to preflight directory entry {}", path.display())
                })?;
                let base_dev = fs::symlink_metadata(base)
                    .wrap_err_with(|| {
                        format!("failed to read sync base metadata for {}", base.display())
                    })?
                    .dev();
                preflight_prunable_ignored_path(base, &path, file_type.is_dir(), base_dev, policy)?;
                continue;
            }
            return Err(removal_blocker_error(
                dirname,
                &path,
                kind,
                policy.prune_ignored,
            ));
        }
        if entry
            .file_type()
            .wrap_err_with(|| format!("unable to preflight directory entry {}", path.display()))?
            .is_dir()
        {
            preflight_removed_directory_contents(base, &path, removed_paths, policy)?;
        }
    }
    Ok(())
}

fn prune_ignored_removal_blockers(
    base: &Path,
    dirname: &Path,
    removed_paths: &HashSet<PathBuf>,
    policy: &RemovalBlockerPolicy,
    attempt_state: Option<&Path>,
) -> Result<()> {
    for entry in fs::read_dir(dirname).wrap_err_with(|| {
        format!(
            "unable to preflight directory removal {}",
            dirname.display()
        )
    })? {
        let entry = entry.wrap_err_with(|| {
            format!(
                "unable to preflight directory removal entry in {}",
                dirname.display()
            )
        })?;
        let path = entry.path();
        let relative_path = path.strip_prefix(base).wrap_err_with(|| {
            format!(
                "unable to preflight directory removal path {}",
                path.display()
            )
        })?;
        let file_type = entry
            .file_type()
            .wrap_err_with(|| format!("unable to preflight directory entry {}", path.display()))?;
        if removed_paths.contains(relative_path) {
            if file_type.is_dir() {
                prune_ignored_removal_blockers(base, &path, removed_paths, policy, attempt_state)?;
            }
            continue;
        }

        let kind = policy.classify(relative_path);
        if policy.should_prune(&kind) {
            if file_type.is_dir() {
                let base_dev = fs::symlink_metadata(base)
                    .wrap_err_with(|| {
                        format!("failed to read sync base metadata for {}", base.display())
                    })?
                    .dev();
                remove_ignored_dir_all_same_device(base, &path, base_dev, policy, attempt_state)
                    .wrap_err_with(|| {
                        format!("failed to prune ignored directory {}", path.display())
                    })?;
            } else {
                fs::remove_file(&path)
                    .wrap_err_with(|| format!("failed to prune file {}", path.display()))?;
            }
            record_committed_step(attempt_state, "prune-blocker", &relative_path.to_path_buf())?;
        } else {
            return Err(removal_blocker_error(
                dirname,
                &path,
                kind,
                policy.prune_ignored,
            ));
        }
    }
    Ok(())
}

fn preflight_prunable_ignored_path(
    base: &Path,
    path: &Path,
    is_dir: bool,
    base_dev: u64,
    policy: &RemovalBlockerPolicy,
) -> Result<()> {
    preflight_prune_unlink_parent(path)?;
    if is_dir {
        preflight_prunable_ignored_dir(base, path, base_dev, policy)?;
    }
    Ok(())
}

fn preflight_prune_unlink_parent(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        eyre!(
            "ignored path {} has no parent directory for pruning",
            path.display()
        )
    })?;
    let meta = fs::symlink_metadata(parent).wrap_err_with(|| {
        format!(
            "unable to preflight ignored prune parent {}",
            parent.display()
        )
    })?;
    if !owner_write_execute(meta.permissions().mode()) {
        return Err(eyre!(
            "ignored prune parent {} is not writable",
            parent.display()
        ));
    }
    Ok(())
}

fn preflight_prunable_ignored_dir(
    base: &Path,
    path: &Path,
    base_dev: u64,
    policy: &RemovalBlockerPolicy,
) -> Result<()> {
    let meta = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to read ignored directory {}", path.display()))?;
    if meta.dev() != base_dev {
        return Err(eyre!(
            "ignored directory {} is on another filesystem; refusing to prune it",
            path.display()
        ));
    }
    if !owner_write_execute(meta.permissions().mode()) {
        return Err(eyre!(
            "ignored directory {} is not writable for pruning",
            path.display()
        ));
    }

    for entry in fs::read_dir(path)
        .wrap_err_with(|| format!("unable to preflight ignored directory {}", path.display()))?
    {
        let entry = entry.wrap_err_with(|| {
            format!(
                "unable to preflight ignored directory entry in {}",
                path.display()
            )
        })?;
        let child = entry.path();
        let relative_child = child.strip_prefix(base).wrap_err_with(|| {
            format!(
                "unable to preflight ignored directory path {}",
                child.display()
            )
        })?;
        if policy.is_excluded(relative_child) {
            return Err(removal_blocker_error(
                path,
                &child,
                RemovalBlockerKind::Excluded,
                policy.prune_ignored,
            ));
        }
        let child_meta = fs::symlink_metadata(&child)
            .wrap_err_with(|| format!("unable to preflight ignored path {}", child.display()))?;
        if child_meta.is_dir() {
            preflight_prunable_ignored_dir(base, &child, base_dev, policy)?;
        }
    }

    Ok(())
}

fn remove_ignored_dir_all_same_device(
    base: &Path,
    path: &Path,
    base_dev: u64,
    policy: &RemovalBlockerPolicy,
    attempt_state: Option<&Path>,
) -> Result<()> {
    preflight_prunable_ignored_dir(base, path, base_dev, policy)?;

    let temp_path = ignored_prune_temp_path(path)?;
    record_staged_file(attempt_state, &temp_path)?;
    fs::rename(path, &temp_path).wrap_err_with(|| {
        format!(
            "failed to move ignored directory {} aside for pruning",
            path.display()
        )
    })?;
    remove_quarantined_ignored_path(&temp_path, base_dev)
}

fn ignored_prune_temp_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        eyre!(
            "ignored path {} has no parent directory for pruning",
            path.display()
        )
    })?;
    let filename = path
        .file_name()
        .ok_or_else(|| eyre!("ignored path {} has no file name", path.display()))?
        .to_string_lossy();
    for _ in 0..100 {
        let counter = TEMP_OUTPUT_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let temp_path = parent.join(format!(
            ".duet-prune-{}-{}-{}",
            std::process::id(),
            counter,
            filename
        ));
        if !temp_path.try_exists().wrap_err_with(|| {
            format!("failed to check ignored prune path {}", temp_path.display())
        })? {
            return Ok(temp_path);
        }
    }

    Err(eyre!(
        "failed to choose temporary prune path for {}",
        path.display()
    ))
}

fn remove_quarantined_ignored_path(path: &Path, base_dev: u64) -> Result<()> {
    let meta = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to read ignored path {}", path.display()))?;
    if !meta.is_dir() {
        fs::remove_file(path)
            .wrap_err_with(|| format!("failed to prune ignored file {}", path.display()))?;
        return Ok(());
    }
    if meta.dev() != base_dev {
        return Err(eyre!(
            "ignored directory {} is on another filesystem; refusing to prune it",
            path.display()
        ));
    }

    for entry in fs::read_dir(path)
        .wrap_err_with(|| format!("failed to read ignored directory {}", path.display()))?
    {
        let entry = entry.wrap_err_with(|| {
            format!(
                "failed to read ignored directory entry in {}",
                path.display()
            )
        })?;
        let child = entry.path();
        let child_meta = fs::symlink_metadata(&child)
            .wrap_err_with(|| format!("failed to read ignored path {}", child.display()))?;
        if child_meta.is_dir() {
            remove_quarantined_ignored_path(&child, base_dev)?;
        } else {
            fs::remove_file(&child)
                .wrap_err_with(|| format!("failed to prune ignored file {}", child.display()))?;
        }
    }

    fs::remove_dir(path)
        .wrap_err_with(|| format!("failed to prune ignored directory {}", path.display()))?;
    Ok(())
}

fn apply_metadata_targets(actions: &Vec<Action>) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    for action in actions {
        match action {
            Action::Local(Change::Modified(_, new))
            | Action::ResolvedLocal((_, _), Change::Modified(_, new)) => {
                targets.push(new.path().clone())
            }
            Action::Local(Change::Removed(e))
            | Action::ResolvedLocal((_, _), Change::Removed(e)) => targets.push(e.path().clone()),
            _ => {}
        }
    }
    targets
}

fn preflight_source_reads(base: &PathBuf, actions: &Vec<Action>) -> Result<()> {
    for action in actions {
        if let Some(kind) = source_detail_kind(action) {
            match kind {
                SourceDetailKind::File(path) | SourceDetailKind::Diff(path) => {
                    preflight_read_file(base, path, "source detail")?;
                }
            }
        }

        match action {
            Action::Local(Change::Modified(old, new))
            | Action::ResolvedLocal((_, _), Change::Modified(old, new))
                if old.is_file() && new.is_file() && !old.same_contents(new) =>
            {
                preflight_read_file(base, old.path(), "destination signature")?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn preflight_read_file(base: &Path, path: &Path, description: &str) -> Result<()> {
    let filename = safe_join(base, path)?;
    fs::File::open(&filename).wrap_err_with(|| {
        format!(
            "unable to preflight {} read for {}",
            description,
            filename.display()
        )
    })?;
    Ok(())
}

pub fn get_detailed_changes(
    base: &PathBuf,
    actions: &Vec<Action>,
    signatures: &Vec<SignatureWithPath>,
) -> Result<Vec<ChangeDetails>> {
    validate_actions(actions)?;
    let mut sig_iter = signatures.iter();
    let mut details: Vec<ChangeDetails> = Vec::new();

    for action in actions {
        match action {
            Action::Remote(change) | Action::ResolvedRemote((_, _), change) => {
                match change {
                    Change::Removed(_) => {}
                    Change::Added(e) => {
                        if e.is_file() {
                            log::debug!("Getting detail for adding {}", e.path().display());
                            let v = fs::read(safe_join(base, e.path())?)?;
                            details.push(ChangeDetails::Contents(v));
                        }
                    }
                    Change::Modified(e1, e2) => {
                        if e1.is_file() && e2.is_file() && !e1.same_contents(&e2) {
                            let f = fs::File::open(safe_join(base, e1.path())?)?;
                            let sig = next_signature(&mut sig_iter, e1.path())?;
                            let block = vec![0; sig.window];
                            let delta = compare(sig, f, block)?;
                            details.push(ChangeDetails::Diff(delta))
                        } else if !e1.is_file() && e2.is_file() {
                            let v = fs::read(safe_join(base, e2.path())?)?;
                            details.push(ChangeDetails::Contents(v));
                        } // else: permissions or target change
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(extra) = sig_iter.next() {
        return Err(eyre!("unexpected signature for {}", extra.0.display()));
    }

    Ok(details)
}

enum ProducerState {
    File {
        action_index: u32,
        file: fs::File,
        remaining: u64,
    },
    Diff {
        receiver: mpsc::Receiver<Result<DetailFrame>>,
        handle: thread::JoinHandle<()>,
    },
}

pub struct DetailProducer {
    base: PathBuf,
    actions: Vec<Action>,
    signatures: Vec<SignatureWithPath>,
    max_chunk_bytes: usize,
    action_index: usize,
    signature_index: usize,
    pending: VecDeque<DetailFrame>,
    state: Option<ProducerState>,
}

impl DetailProducer {
    pub fn new(
        base: PathBuf,
        actions: Vec<Action>,
        signatures: Vec<SignatureWithPath>,
        max_chunk_bytes: usize,
    ) -> Self {
        DetailProducer {
            base,
            actions,
            signatures,
            max_chunk_bytes: max_chunk_bytes.max(1),
            action_index: 0,
            signature_index: 0,
            pending: VecDeque::new(),
            state: None,
        }
    }

    pub fn next_frame(&mut self) -> Result<Option<DetailFrame>> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(Some(frame));
        }

        if let Some(state) = self.state.take() {
            match state {
                ProducerState::File {
                    action_index,
                    mut file,
                    mut remaining,
                } => {
                    if remaining == 0 {
                        return Ok(Some(DetailFrame {
                            action_index,
                            payload: DetailPayload::FileEnd,
                        }));
                    }

                    let chunk_bytes = remaining.min(self.max_chunk_bytes as u64) as usize;
                    let mut buf = vec![0; chunk_bytes];
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        return Ok(Some(DetailFrame {
                            action_index,
                            payload: DetailPayload::FileEnd,
                        }));
                    }

                    buf.truncate(n);
                    remaining = remaining.saturating_sub(n as u64);
                    self.state = Some(ProducerState::File {
                        action_index,
                        file,
                        remaining,
                    });
                    return Ok(Some(DetailFrame {
                        action_index,
                        payload: DetailPayload::FileBytes(buf),
                    }));
                }
                ProducerState::Diff { receiver, handle } => match receiver.recv() {
                    Ok(frame) => {
                        let frame = frame?;
                        let done = matches!(frame.payload, DetailPayload::DiffEnd);
                        if done {
                            let _ = handle.join();
                        } else {
                            self.state = Some(ProducerState::Diff { receiver, handle });
                        }
                        return Ok(Some(frame));
                    }
                    Err(_) => {
                        let _ = handle.join();
                        return Err(eyre!("detail diff stream ended without DiffEnd"));
                    }
                },
            }
        }

        while self.action_index < self.actions.len() {
            let index = self.action_index;
            self.action_index += 1;
            let action_index = index as u32;
            let Some(kind) = source_detail_kind(&self.actions[index]) else {
                continue;
            };

            match kind {
                SourceDetailKind::File(path) => {
                    let file = fs::File::open(safe_join(&self.base, path)?)?;
                    let remaining = file.metadata()?.len();
                    self.state = Some(ProducerState::File {
                        action_index,
                        file,
                        remaining,
                    });
                    return Ok(Some(DetailFrame {
                        action_index,
                        payload: DetailPayload::FileBegin,
                    }));
                }
                SourceDetailKind::Diff(path) => {
                    let signature_with_path = self
                        .signatures
                        .get(self.signature_index)
                        .ok_or_else(|| eyre!("missing signature for {}", path.display()))?;
                    if signature_with_path.0 != *path {
                        return Err(eyre!(
                            "signature path mismatch: expected {}, got {}",
                            path.display(),
                            signature_with_path.0.display()
                        ));
                    }
                    validate_signature_window(signature_with_path.1.window).wrap_err_with(
                        || {
                            format!(
                                "invalid signature window for {}",
                                signature_with_path.0.display()
                            )
                        },
                    )?;
                    let signature = signature_with_path.1.clone();
                    self.signature_index += 1;

                    let file_path = safe_join(&self.base, path)?;
                    let max_chunk_bytes = self.max_chunk_bytes;
                    let (sender, receiver) = mpsc::sync_channel(4);
                    let handle = thread::spawn(move || {
                        let result = stream_diff_frames(
                            file_path,
                            action_index,
                            signature,
                            max_chunk_bytes,
                            sender.clone(),
                        );
                        if let Err(error) = result {
                            let _ = sender.send(Err(error));
                        }
                    });
                    self.state = Some(ProducerState::Diff { receiver, handle });
                    return Ok(Some(DetailFrame {
                        action_index,
                        payload: DetailPayload::DiffBegin,
                    }));
                }
            }
        }

        if let Some(extra) = self.signatures.get(self.signature_index) {
            return Err(eyre!("unexpected signature for {}", extra.0.display()));
        }

        Ok(None)
    }

    pub fn next_frames(
        &mut self,
        max_frames: usize,
        max_payload_bytes: usize,
    ) -> Result<Vec<DetailFrame>> {
        let max_frames = max_frames.max(1);
        let max_payload_bytes = max_payload_bytes.max(1);
        let mut frames = Vec::new();
        let mut payload_bytes = 0;

        while frames.len() < max_frames {
            let Some(frame) = self.next_frame()? else {
                break;
            };

            let frame_payload_bytes = detail_payload_bytes(&frame.payload);
            if !frames.is_empty() && payload_bytes + frame_payload_bytes > max_payload_bytes {
                self.pending.push_front(frame);
                break;
            }

            payload_bytes += frame_payload_bytes;
            frames.push(frame);
        }

        Ok(frames)
    }
}

fn detail_payload_bytes(payload: &DetailPayload) -> usize {
    match payload {
        DetailPayload::FileBytes(bytes) | DetailPayload::DiffBytes(bytes) => bytes.len(),
        DetailPayload::FileBegin
        | DetailPayload::FileEnd
        | DetailPayload::DiffBegin
        | DetailPayload::DiffCopy { .. }
        | DetailPayload::DiffEnd => 0,
    }
}

enum SourceDetailKind<'a> {
    File(&'a PathBuf),
    Diff(&'a PathBuf),
}

fn source_detail_kind(action: &Action) -> Option<SourceDetailKind<'_>> {
    let change = match action {
        Action::Remote(change) | Action::ResolvedRemote((_, _), change) => change,
        _ => return None,
    };

    match change {
        Change::Removed(_) => None,
        Change::Added(e) => e.is_file().then(|| SourceDetailKind::File(e.path())),
        Change::Modified(e1, e2) => {
            if e1.is_file() && e2.is_file() && !e1.same_contents(e2) {
                Some(SourceDetailKind::Diff(e1.path()))
            } else if !e1.is_file() && e2.is_file() {
                Some(SourceDetailKind::File(e2.path()))
            } else {
                None
            }
        }
    }
}

fn stream_diff_frames(
    file_path: PathBuf,
    action_index: u32,
    signature: Signature,
    max_chunk_bytes: usize,
    sender: mpsc::SyncSender<Result<DetailFrame>>,
) -> Result<()> {
    validate_signature_window(signature.window)?;
    let file = fs::File::open(file_path)?;
    let block = vec![0; signature.window];
    let mut pending_copy: Option<(u64, u64)> = None;

    let send_frame = |payload| {
        sender
            .send(Ok(DetailFrame {
                action_index,
                payload,
            }))
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "detail stream closed")
            })
    };

    let flush_copy = |pending_copy: &mut Option<(u64, u64)>| {
        if let Some((offset, len)) = pending_copy.take() {
            send_frame(DetailPayload::DiffCopy { offset, len })?;
        }
        Ok::<(), std::io::Error>(())
    };

    compare_stream(&signature, file, block, max_chunk_bytes, |op| {
        match op {
            DeltaOp::FromSource(offset) => {
                let copy_len = signature.window as u64;
                match &mut pending_copy {
                    Some((start, len)) if *start + *len == offset => *len += copy_len,
                    Some(_) => {
                        flush_copy(&mut pending_copy)?;
                        pending_copy = Some((offset, copy_len));
                    }
                    None => pending_copy = Some((offset, copy_len)),
                }
            }
            DeltaOp::Literal(bytes) => {
                flush_copy(&mut pending_copy)?;
                send_frame(DetailPayload::DiffBytes(bytes))?;
            }
        }
        Ok(())
    })?;
    flush_copy(&mut pending_copy)?;
    send_frame(DetailPayload::DiffEnd)?;
    Ok(())
}

struct StagingState {
    stage_parent_path: PathBuf,
    stage_parent_identity: DirectoryIdentity,
    path: PathBuf,
    name: std::ffi::CString,
    stage_parent_directory: fs::File,
    directory: fs::File,
    published_parents: Mutex<HashMap<PathBuf, DirectoryIdentity>>,
}

enum CloneOutput {
    Cloned(PathBuf, std::ffi::CString, fs::File),
    Unsupported,
}

impl StagingState {
    fn verify_stage_parent_identity(&self) -> Result<()> {
        verify_path_identity(
            &self.stage_parent_path,
            &self.stage_parent_directory,
            "temporary directory parent",
        )?;
        verify_directory_handle_identity(
            &self.stage_parent_directory,
            self.stage_parent_identity,
            &self.stage_parent_path,
            "temporary directory parent",
        )
    }

    fn verify_stage_path_identity(&self) -> Result<()> {
        verify_directory_at_identity(
            &self.stage_parent_directory,
            &self.name,
            &self.directory,
            &self.path,
        )
    }

    fn verify_identity(&self) -> Result<()> {
        self.verify_stage_parent_identity()?;
        self.verify_stage_path_identity()
    }

    fn record_published_parent(&self, path: &Path, directory: &fs::File) -> Result<()> {
        verify_path_identity(path, directory, "published destination parent")?;
        let identity = directory_identity(directory, path, "published destination parent")?;
        let mut parents = self
            .published_parents
            .lock()
            .map_err(|_| eyre!("published destination parent identity lock was poisoned"))?;
        if let Some(existing) = parents.get(path) {
            if *existing != identity {
                return Err(eyre!(
                    "published destination parent path {} changed identity during apply",
                    path.display()
                ));
            }
        } else {
            parents.insert(path.to_path_buf(), identity);
        }
        Ok(())
    }

    fn create_output(&self) -> Result<(PathBuf, std::ffi::CString, fs::File)> {
        self.verify_identity()?;
        for _ in 0..128 {
            let component = format!(
                "o-{:x}",
                TEMP_OUTPUT_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
            );
            let name = path_component_cstring(component.as_ref(), "temporary output name")?;
            let path = self.path.join(&component);
            let file = match openat_file(
                self.directory.as_raw_fd(),
                &name,
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            ) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        format!("failed to create temporary file {}", path.display())
                    });
                }
            };
            let secure_result = (|| -> Result<()> {
                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .wrap_err_with(|| {
                        format!(
                            "failed to normalize temporary file permissions {}",
                            path.display()
                        )
                    })?;
                let metadata = file.metadata().wrap_err_with(|| {
                    format!("failed to inspect temporary file {}", path.display())
                })?;
                if !metadata.is_file() || metadata.mode() & 0o7777 != 0o600 {
                    return Err(eyre!(
                        "temporary file {} mode was not normalized to 0600",
                        path.display()
                    ));
                }
                Ok(())
            })();
            if let Err(error) = secure_result {
                let _ = unlinkat(self.directory.as_raw_fd(), &name, 0);
                return Err(error);
            }
            return Ok((path, name, file));
        }

        Err(eyre!(
            "failed to create a unique temporary file in {}",
            self.path.display()
        ))
    }

    #[cfg(target_os = "macos")]
    fn clone_output(&self, source: &fs::File) -> Result<CloneOutput> {
        self.verify_identity()?;
        for _ in 0..128 {
            let component = format!(
                "o-{:x}",
                TEMP_OUTPUT_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
            );
            let name = path_component_cstring(component.as_ref(), "temporary output name")?;
            let path = self.path.join(&component);
            match cvt(unsafe {
                libc::fclonefileat(
                    source.as_raw_fd(),
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    CLONE_NOOWNERCOPY,
                )
            }) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) if clone_error_is_unsupported(&error) => {
                    return Ok(CloneOutput::Unsupported);
                }
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        format!("failed to clone temporary file {}", path.display())
                    });
                }
            }

            let setup = (|| -> Result<fs::File> {
                let created =
                    fstatat_nofollow(self.directory.as_raw_fd(), &name).wrap_err_with(|| {
                        format!("failed to inspect cloned file {}", path.display())
                    })?;
                if created.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(eyre!(
                        "cloned output {} is not a regular file",
                        path.display()
                    ));
                }
                let file = openat_file(
                    self.directory.as_raw_fd(),
                    &name,
                    libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                )
                .wrap_err_with(|| format!("failed to open cloned file {}", path.display()))?;
                normalize_macos_cloned_file(&file, &path)?;
                let metadata = file.metadata().wrap_err_with(|| {
                    format!("failed to inspect cloned file {}", path.display())
                })?;
                if !metadata.is_file()
                    || metadata.dev() != created.st_dev as u64
                    || metadata.ino() != created.st_ino as u64
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o7777 != 0o600
                {
                    return Err(eyre!(
                        "cloned output {} changed identity or mode during creation",
                        path.display()
                    ));
                }
                Ok(file)
            })();
            match setup {
                Ok(file) => return Ok(CloneOutput::Cloned(path, name, file)),
                Err(error) => {
                    if let Err(cleanup_error) = unlinkat(self.directory.as_raw_fd(), &name, 0) {
                        return Err(error).wrap_err_with(|| {
                            format!(
                                "failed to initialize cloned file {}; additionally failed to remove it: {}",
                                path.display(), cleanup_error
                            )
                        });
                    }
                    return Err(error);
                }
            }
        }

        Err(eyre!(
            "failed to create a unique cloned file in {}",
            self.path.display()
        ))
    }

    #[cfg(target_os = "linux")]
    fn clone_output(&self, source: &fs::File) -> Result<CloneOutput> {
        let (path, name, file) = self.create_output()?;
        match cvt(unsafe { libc::ioctl(file.as_raw_fd(), libc::FICLONE as _, source.as_raw_fd()) })
        {
            Ok(()) => Ok(CloneOutput::Cloned(path, name, file)),
            Err(error) if clone_error_is_unsupported(&error) => {
                unlinkat(self.directory.as_raw_fd(), &name, 0).wrap_err_with(|| {
                    format!(
                        "failed to remove unsupported cloned output {}",
                        path.display()
                    )
                })?;
                Ok(CloneOutput::Unsupported)
            }
            Err(error) => {
                if let Err(cleanup_error) = unlinkat(self.directory.as_raw_fd(), &name, 0) {
                    return Err(error).wrap_err_with(|| {
                        format!(
                            "failed to clone temporary file {}; additionally failed to remove it: {}",
                            path.display(), cleanup_error
                        )
                    });
                }
                Err(error)
                    .wrap_err_with(|| format!("failed to clone temporary file {}", path.display()))
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn clone_output(&self, _source: &fs::File) -> Result<CloneOutput> {
        Ok(CloneOutput::Unsupported)
    }
}

fn clone_error_is_unsupported(error: &io::Error) -> bool {
    let Some(code) = error.raw_os_error() else {
        return false;
    };
    if code == libc::EXDEV || code == libc::ENOSYS || code == libc::EOPNOTSUPP {
        return true;
    }
    #[cfg(target_os = "linux")]
    if code == libc::ENOTTY || code == libc::EINVAL || code == libc::EBADF {
        return true;
    }
    false
}

#[cfg(target_os = "macos")]
fn normalize_macos_cloned_file(file: &fs::File, path: &Path) -> Result<()> {
    cvt(unsafe { libc::fchflags(file.as_raw_fd(), 0) })
        .wrap_err_with(|| format!("failed to clear cloned file flags {}", path.display()))?;
    clear_macos_acl(file, path)?;
    remove_macos_xattrs(file, path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .wrap_err_with(|| {
            format!(
                "failed to normalize cloned file permissions {}",
                path.display()
            )
        })?;

    let metadata = file
        .metadata()
        .wrap_err_with(|| format!("failed to verify cloned file metadata {}", path.display()))?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(eyre!(
            "cloned output {} is not owned by the current user",
            path.display()
        ));
    }
    let retained_xattrs = macos_xattr_names(file, path)?.collect::<Vec<_>>();
    if retained_xattrs
        .iter()
        .any(|name| name.as_slice() != b"com.apple.provenance")
    {
        return Err(eyre!(
            "cloned output {} retained extended attributes {:?}",
            path.display(),
            retained_xattrs
        ));
    }
    if !macos_acl_is_empty(file, path)? {
        return Err(eyre!(
            "cloned output {} retained an extended ACL",
            path.display()
        ));
    }
    let mut stat = std::mem::MaybeUninit::uninit();
    cvt(unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) })
        .wrap_err_with(|| format!("failed to inspect cloned file flags {}", path.display()))?;
    if unsafe { stat.assume_init() }.st_flags != 0 {
        return Err(eyre!(
            "cloned output {} retained file flags",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_xattr_names(file: &fs::File, path: &Path) -> Result<std::vec::IntoIter<Vec<u8>>> {
    let size = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    if size == -1 {
        return Err(io::Error::last_os_error())
            .wrap_err_with(|| format!("failed to list cloned file attributes {}", path.display()));
    }
    if size == 0 {
        return Ok(Vec::new().into_iter());
    }

    let mut names = vec![0_u8; size as usize];
    let read =
        unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len(), 0) };
    if read == -1 {
        return Err(io::Error::last_os_error())
            .wrap_err_with(|| format!("failed to read cloned file attributes {}", path.display()));
    }
    names.truncate(read as usize);
    let parsed = names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    Ok(parsed.into_iter())
}

#[cfg(target_os = "macos")]
fn remove_macos_xattrs(file: &fs::File, path: &Path) -> Result<()> {
    for name in macos_xattr_names(file, path)? {
        let name = std::ffi::CString::new(name)
            .map_err(|_| eyre!("cloned file attribute name contains an interior NUL byte"))?;
        cvt(unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr(), 0) }).wrap_err_with(
            || {
                format!(
                    "failed to remove cloned file attribute {:?} from {}",
                    name,
                    path.display()
                )
            },
        )?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clear_macos_acl(file: &fs::File, path: &Path) -> Result<()> {
    let acl = unsafe { acl_init(0) };
    if acl.is_null() {
        return Err(io::Error::last_os_error())
            .wrap_err_with(|| format!("failed to create an empty ACL for {}", path.display()));
    }
    let set_result = cvt(unsafe { acl_set_fd_np(file.as_raw_fd(), acl, ACL_TYPE_EXTENDED) })
        .wrap_err_with(|| format!("failed to clear cloned file ACL {}", path.display()));
    let free_result = cvt(unsafe { acl_free(acl) })
        .wrap_err_with(|| format!("failed to release cloned file ACL {}", path.display()));
    set_result?;
    free_result
}

#[cfg(target_os = "macos")]
fn macos_acl_is_empty(file: &fs::File, path: &Path) -> Result<bool> {
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(true);
        }
        return Err(error)
            .wrap_err_with(|| format!("failed to inspect cloned file ACL {}", path.display()));
    }
    let mut entry = std::ptr::null_mut();
    let get_result = unsafe { acl_get_entry(acl, 0, &mut entry) };
    let free_result = cvt(unsafe { acl_free(acl) })
        .wrap_err_with(|| format!("failed to release cloned file ACL {}", path.display()));
    let empty = match get_result {
        0 => true,
        1 => false,
        _ => {
            return Err(io::Error::last_os_error())
                .wrap_err_with(|| format!("failed to read cloned file ACL {}", path.display()));
        }
    };
    free_result?;
    Ok(empty)
}

struct StagingArea {
    shared: Arc<StagingState>,
    finished: bool,
    cleanup_on_drop: bool,
}

impl StagingArea {
    fn new(stage_parent: &Path) -> Result<Self> {
        let (stage_parent_directory, stage_parent_guard) = WritableDirGuard::new(stage_parent)?;
        let stage_parent_identity = directory_identity(
            &stage_parent_directory,
            stage_parent,
            "temporary directory parent",
        )?;
        let (path, name, directory) =
            create_staging_directory(stage_parent, &stage_parent_directory)?;
        let area = Self {
            shared: Arc::new(StagingState {
                stage_parent_path: stage_parent.to_path_buf(),
                stage_parent_identity,
                path,
                name,
                stage_parent_directory,
                directory,
                published_parents: Mutex::new(HashMap::new()),
            }),
            finished: false,
            cleanup_on_drop: true,
        };
        if let Some(guard) = stage_parent_guard {
            guard.restore()?;
        }
        Ok(area)
    }

    fn path(&self) -> &Path {
        &self.shared.path
    }

    fn shared(&self) -> Arc<StagingState> {
        Arc::clone(&self.shared)
    }

    fn retain_for_recovery(&mut self) {
        self.cleanup_on_drop = false;
    }

    fn seal(&self) -> Result<()> {
        self.shared.verify_identity()?;
        self.shared.directory.sync_all().wrap_err_with(|| {
            format!(
                "failed to sync temporary directory {}",
                self.shared.path.display()
            )
        })?;
        self.shared.verify_identity()
    }

    fn finish(mut self, metadata_synced: &HashSet<PathBuf>) -> Result<HashSet<PathBuf>> {
        self.shared.verify_stage_parent_identity()?;
        let stage_parent_guard = WritableDirGuard::from_retained(
            &self.shared.stage_parent_path,
            &self.shared.stage_parent_directory,
        )?;
        self.shared.verify_stage_path_identity()?;
        self.shared.directory.sync_all().wrap_err_with(|| {
            format!(
                "failed to sync temporary directory {}",
                self.shared.path.display()
            )
        })?;
        self.shared.verify_stage_parent_identity()?;
        self.shared.verify_stage_path_identity()?;
        unlinkat(
            self.shared.stage_parent_directory.as_raw_fd(),
            &self.shared.name,
            libc::AT_REMOVEDIR,
        )
        .wrap_err_with(|| {
            format!(
                "failed to remove temporary directory {}",
                self.shared.path.display()
            )
        })?;
        self.finished = true;
        if let Some(guard) = stage_parent_guard {
            guard.restore()?;
        }

        let published_parents = self
            .shared
            .published_parents
            .lock()
            .map_err(|_| eyre!("published destination parent identity lock was poisoned"))?
            .clone();
        let mut synced = HashSet::new();
        sync_retained_directory(
            &self.shared.stage_parent_path,
            &self.shared.stage_parent_directory,
            Some(self.shared.stage_parent_identity),
            "temporary directory parent",
        )?;
        synced.insert(self.shared.stage_parent_path.clone());
        for (path, identity) in published_parents {
            if path == self.shared.stage_parent_path {
                continue;
            }
            if metadata_synced.contains(&path) {
                // Directory metadata is applied and fsynced after child publication.
                verify_recorded_directory_path(&path, identity)?;
            } else {
                sync_recorded_directory(&path, identity)?;
            }
            synced.insert(path);
        }
        Ok(synced)
    }
}

impl Drop for StagingArea {
    fn drop(&mut self) {
        if !self.finished && self.cleanup_on_drop {
            let guard = WritableDirGuard::from_retained(
                &self.shared.stage_parent_path,
                &self.shared.stage_parent_directory,
            )
            .ok()
            .flatten();
            if self.shared.verify_identity().is_ok() {
                let _ = unlinkat(
                    self.shared.stage_parent_directory.as_raw_fd(),
                    &self.shared.name,
                    libc::AT_REMOVEDIR,
                );
            }
            if let Some(guard) = guard {
                let _ = guard.restore();
            }
        }
    }
}

struct TempOutput {
    final_path: PathBuf,
    parent_path: PathBuf,
    parent_identity: Option<DirectoryIdentity>,
    temp_path: PathBuf,
    final_name: std::ffi::CString,
    output_name: std::ffi::CString,
    staging: Arc<StagingState>,
    file: Option<fs::File>,
    identity: Option<FileIdentity>,
    cleanup_on_drop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

impl TempOutput {
    fn new(final_path: PathBuf, staging: Arc<StagingState>) -> Result<Self> {
        let created = staging.create_output()?;
        Self::from_created(final_path, staging, created)
    }

    fn clone_from(
        final_path: PathBuf,
        staging: Arc<StagingState>,
        source: &fs::File,
    ) -> Result<Option<Self>> {
        let created = match staging.clone_output(source)? {
            CloneOutput::Cloned(path, name, file) => (path, name, file),
            CloneOutput::Unsupported => return Ok(None),
        };
        let cleanup_name = created.1.clone();
        match Self::from_created(final_path, Arc::clone(&staging), created) {
            Ok(output) => Ok(Some(output)),
            Err(error) => {
                match unlinkat(staging.directory.as_raw_fd(), &cleanup_name, 0) {
                    Ok(()) => {}
                    Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {}
                    Err(cleanup_error) => {
                        return Err(error).wrap_err_with(|| {
                            format!(
                                "failed to initialize cloned temporary output; additionally failed to remove it: {}",
                                cleanup_error
                            )
                        });
                    }
                }
                Err(error)
            }
        }
    }

    fn from_created(
        final_path: PathBuf,
        staging: Arc<StagingState>,
        (temp_path, output_name, file): (PathBuf, std::ffi::CString, fs::File),
    ) -> Result<Self> {
        let parent = output_parent(&final_path);
        let parent_path = parent.to_path_buf();
        let parent_identity = if parent.try_exists().wrap_err_with(|| {
            format!(
                "failed to inspect output parent directory {}",
                parent.display()
            )
        })? {
            Some(directory_path_identity(parent, "output parent directory")?)
        } else {
            None
        };
        let final_name = path_component_cstring(
            final_path
                .file_name()
                .ok_or_else(|| eyre!("output path {} has no file name", final_path.display()))?,
            "output file name",
        )?;
        let output = TempOutput {
            final_path,
            parent_path,
            parent_identity,
            temp_path,
            final_name,
            output_name,
            staging,
            file: Some(file),
            identity: None,
            cleanup_on_drop: true,
        };
        output.verify_at_identity(
            &output.staging.directory,
            &output.output_name,
            &output.temp_path,
        )?;
        Ok(output)
    }

    fn prepare_metadata(&mut self, entry: &Entry) -> Result<Entry> {
        self.flush()?;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| eyre!("temporary output is closed"))?;
        let meta = file.metadata().wrap_err_with(|| {
            format!(
                "failed to read temporary file metadata {}",
                self.temp_path.display()
            )
        })?;
        filetime::set_file_handle_times(
            file,
            Some(filetime::FileTime::from_unix_time(meta.atime(), 0)),
            Some(filetime::FileTime::from_unix_time(entry.mtime(), 0)),
        )
        .wrap_err_with(|| {
            format!(
                "failed to set temporary file time {}",
                self.temp_path.display()
            )
        })?;
        file.set_permissions(fs::Permissions::from_mode(synced_mode(entry.mode())))
            .wrap_err_with(|| {
                format!(
                    "failed to set temporary file permissions {}",
                    self.temp_path.display()
                )
            })?;

        let final_meta = file.metadata().wrap_err_with(|| {
            format!(
                "failed to verify temporary file metadata {}",
                self.temp_path.display()
            )
        })?;
        if synced_mode(final_meta.mode()) != synced_mode(entry.mode())
            || final_meta.mtime() != entry.mtime()
        {
            return Err(eyre!(
                "temporary file {} metadata did not match the requested mode and mtime",
                self.temp_path.display()
            ));
        }

        let mut final_entry = entry.clone();
        final_entry.set_ino(final_meta.ino());
        Ok(final_entry)
    }

    fn verify_contents(&mut self, entry: &Entry, description: &str) -> Result<()> {
        self.flush()?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| eyre!("temporary output is closed"))?;
        verify_open_file_matches_entry(file, &self.temp_path, entry, description)
    }

    fn verify_at_identity(
        &self,
        directory: &fs::File,
        name: &std::ffi::CStr,
        path: &Path,
    ) -> Result<()> {
        let identity = match self.file.as_ref() {
            Some(file) => {
                let metadata = file
                    .metadata()
                    .wrap_err("failed to read open temporary file metadata")?;
                FileIdentity {
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                }
            }
            None => self
                .identity
                .ok_or_else(|| eyre!("temporary output has no recorded identity"))?,
        };
        let path_stat = fstatat_nofollow(directory.as_raw_fd(), name)
            .wrap_err_with(|| format!("failed to read temporary path {}", path.display()))?;
        if path_stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || path_stat.st_dev as u64 != identity.dev
            || path_stat.st_ino as u64 != identity.ino
        {
            return Err(eyre!(
                "temporary path {} no longer refers to the open output file",
                path.display()
            ));
        }
        Ok(())
    }

    fn verify_prepared_contents(&self, entry: &Entry) -> Result<()> {
        let mut file = openat_file(
            self.staging.directory.as_raw_fd(),
            &self.output_name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        self.verify_at_identity(&self.staging.directory, &self.output_name, &self.temp_path)?;
        verify_open_file_matches_entry(&mut file, &self.temp_path, entry, "staged output")
    }

    fn prepare(mut self, entry: &Entry, publication: OutputPublication) -> Result<PreparedOutput> {
        let final_entry = self.prepare_metadata(entry)?;
        self.file
            .as_ref()
            .ok_or_else(|| eyre!("temporary output is closed"))?
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(PreparedOutput {
            output: self,
            final_entry,
            publication,
        })
    }

    fn publish_replacing(
        mut self,
        final_entry: Entry,
        expected: &Entry,
        on_commit: impl FnOnce(&Entry) -> Result<()>,
    ) -> Result<Entry> {
        self.reopen()?;
        self.verify_contents(&final_entry, "staged output")?;
        let final_entry = self.prepare_metadata(&final_entry)?;
        self.prepare_publication_parent()?;
        verify_current_matches_entry(&self.final_path, expected, "rename target")?;
        self.with_publication_parent(|parent_directory| {
            self.verify_at_identity(&self.staging.directory, &self.output_name, &self.temp_path)?;
            self.staging.verify_identity()?;
            cvt(unsafe {
                libc::renameat(
                    self.staging.directory.as_raw_fd(),
                    self.output_name.as_ptr(),
                    parent_directory.as_raw_fd(),
                    self.final_name.as_ptr(),
                )
            })
            .wrap_err_with(|| {
                format!(
                    "failed to rename temporary file {} to {}",
                    self.temp_path.display(),
                    self.final_path.display()
                )
            })?;
            on_commit(&final_entry)?;
            verify_path_identity(
                &self.parent_path,
                parent_directory,
                "output parent directory",
            )?;
            self.verify_at_identity(parent_directory, &self.final_name, &self.final_path)?;
            self.staging
                .record_published_parent(&self.parent_path, parent_directory)
        })?;
        Ok(final_entry)
    }

    fn publish_without_replacing(
        mut self,
        final_entry: Entry,
        description: &str,
        on_commit: impl FnOnce(&Entry) -> Result<()>,
    ) -> Result<Entry> {
        self.reopen()?;
        self.verify_contents(&final_entry, "staged output")?;
        let final_entry = self.prepare_metadata(&final_entry)?;
        self.prepare_publication_parent()?;
        self.with_publication_parent(|parent_directory| {
            self.verify_at_identity(&self.staging.directory, &self.output_name, &self.temp_path)?;
            self.staging.verify_identity()?;
            match cvt(unsafe {
                libc::linkat(
                    self.staging.directory.as_raw_fd(),
                    self.output_name.as_ptr(),
                    parent_directory.as_raw_fd(),
                    self.final_name.as_ptr(),
                    0,
                )
            }) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(eyre!(
                        "{} {} already exists",
                        description,
                        self.final_path.display()
                    ));
                }
                Err(err) => {
                    return Err(err).wrap_err_with(|| {
                        format!(
                            "failed to link temporary file {} to {}",
                            self.temp_path.display(),
                            self.final_path.display()
                        )
                    });
                }
            }
            on_commit(&final_entry)?;
            verify_path_identity(
                &self.parent_path,
                parent_directory,
                "output parent directory",
            )?;
            self.verify_at_identity(parent_directory, &self.final_name, &self.final_path)?;
            self.staging
                .record_published_parent(&self.parent_path, parent_directory)?;
            unlinkat(self.staging.directory.as_raw_fd(), &self.output_name, 0).wrap_err_with(|| {
                format!(
                    "failed to remove temporary file {}",
                    self.temp_path.display()
                )
            })
        })?;
        Ok(final_entry)
    }

    #[cfg(test)]
    fn finish(mut self, entry: &Entry) -> Result<Entry> {
        let final_entry = self.prepare_metadata(entry)?;
        self.sync_all()?;
        self.publish_replacing_without_target_check(final_entry)
    }

    #[cfg(test)]
    fn finish_without_replacing(self, description: &str, entry: &Entry) -> Result<Entry> {
        let prepared = self.prepare(
            entry,
            OutputPublication::NoReplace {
                description: description.to_string(),
            },
        )?;
        prepared.output.sync_all()?;
        prepared
            .output
            .publish_without_replacing(prepared.final_entry, description, |_| Ok(()))
    }

    #[cfg(test)]
    fn publish_replacing_without_target_check(mut self, final_entry: Entry) -> Result<Entry> {
        self.prepare_publication_parent()?;
        self.with_publication_parent(|parent_directory| {
            self.verify_at_identity(&self.staging.directory, &self.output_name, &self.temp_path)?;
            self.staging.verify_identity()?;
            cvt(unsafe {
                libc::renameat(
                    self.staging.directory.as_raw_fd(),
                    self.output_name.as_ptr(),
                    parent_directory.as_raw_fd(),
                    self.final_name.as_ptr(),
                )
            })
            .wrap_err_with(|| {
                format!(
                    "failed to rename temporary file {} to {}",
                    self.temp_path.display(),
                    self.final_path.display()
                )
            })?;
            verify_path_identity(
                &self.parent_path,
                parent_directory,
                "output parent directory",
            )?;
            self.verify_at_identity(parent_directory, &self.final_name, &self.final_path)?;
            self.staging
                .record_published_parent(&self.parent_path, parent_directory)
        })?;
        Ok(final_entry)
    }

    #[cfg(test)]
    fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    #[cfg(test)]
    fn stage_path(&self) -> &Path {
        &self.staging.path
    }

    fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| eyre!("temporary output is closed"))?
            .flush()
            .wrap_err_with(|| {
                format!(
                    "failed to flush temporary file {}",
                    self.temp_path.display()
                )
            })
    }

    fn close_after_sync(&mut self) -> Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| eyre!("temporary output is closed"))?;
        let metadata = file.metadata().wrap_err_with(|| {
            format!(
                "failed to inspect temporary file {}",
                self.temp_path.display()
            )
        })?;
        self.identity = Some(FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        });
        self.verify_at_identity(&self.staging.directory, &self.output_name, &self.temp_path)?;
        self.file = None;
        Ok(())
    }

    fn reopen(&mut self) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let expected = self
            .identity
            .ok_or_else(|| eyre!("temporary output has no recorded identity"))?;
        let file = openat_file(
            self.staging.directory.as_raw_fd(),
            &self.output_name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .wrap_err_with(|| {
            format!(
                "failed to reopen temporary file {}",
                self.temp_path.display()
            )
        })?;
        let metadata = file.metadata().wrap_err_with(|| {
            format!(
                "failed to inspect reopened temporary file {}",
                self.temp_path.display()
            )
        })?;
        if !metadata.is_file() || metadata.dev() != expected.dev || metadata.ino() != expected.ino {
            return Err(eyre!(
                "temporary path {} no longer refers to the prepared output file",
                self.temp_path.display()
            ));
        }
        self.file = Some(file);
        self.verify_at_identity(&self.staging.directory, &self.output_name, &self.temp_path)
    }

    #[cfg(test)]
    fn sync_all(&self) -> Result<()> {
        self.file
            .as_ref()
            .ok_or_else(|| eyre!("temporary output is closed"))?
            .sync_all()
            .wrap_err_with(|| format!("failed to sync temporary file {}", self.temp_path.display()))
    }

    fn prepare_publication_parent(&mut self) -> Result<()> {
        if self.parent_identity.is_none() {
            self.parent_identity = Some(directory_path_identity(
                &self.parent_path,
                "output parent directory",
            )?);
        }
        Ok(())
    }

    fn with_publication_parent<T>(
        &self,
        operation: impl FnOnce(&fs::File) -> Result<T>,
    ) -> Result<T> {
        let parent_identity = self
            .parent_identity
            .expect("parent identity is initialized");
        let (parent_directory, parent_guard) =
            WritableDirGuard::new_with_expected(&self.parent_path, Some(parent_identity))?;
        verify_directory_handle_identity(
            &parent_directory,
            parent_identity,
            &self.parent_path,
            "output parent directory",
        )?;
        let result = operation(&parent_directory);
        let restore = restore_parent_guard(parent_guard);
        match (result, restore) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }
}

fn restore_parent_guard(parent_guard: Option<WritableDirGuard>) -> Result<()> {
    if let Some(guard) = parent_guard {
        guard.restore()?;
    }
    Ok(())
}

enum OutputPublication {
    Replace { expected: Entry },
    NoReplace { description: String },
}

struct PreparedOutput {
    output: TempOutput,
    final_entry: Entry,
    publication: OutputPublication,
}

struct PendingOutput {
    action_index: usize,
    prepared: PreparedOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputBatchConfig {
    max_files: usize,
    max_bytes: u64,
    workers: usize,
}

impl OutputBatchConfig {
    fn default_for_host() -> Self {
        let parallelism = thread::available_parallelism()
            .map(|parallelism| parallelism.get())
            .unwrap_or(1);
        Self {
            max_files: DEFAULT_OUTPUT_BATCH_FILES,
            max_bytes: DEFAULT_OUTPUT_BATCH_BYTES,
            workers: parallelism.min(DEFAULT_OUTPUT_SYNC_WORKERS_MAX),
        }
    }

    fn from_env() -> Self {
        Self::default_for_host().with_env_overrides_from(|name| std::env::var(name).ok())
    }

    fn with_env_overrides_from(mut self, mut get: impl FnMut(&str) -> Option<String>) -> Self {
        if let Some(value) = get(ENV_OUTPUT_BATCH_FILES).and_then(|value| value.parse().ok()) {
            self.max_files = value;
        }
        if let Some(value) = get(ENV_OUTPUT_BATCH_BYTES).and_then(|value| value.parse().ok()) {
            self.max_bytes = value;
        }
        if let Some(value) = get(ENV_OUTPUT_SYNC_WORKERS).and_then(|value| value.parse().ok()) {
            self.workers = value;
        }
        self.normalized()
    }

    fn normalized(self) -> Self {
        self.normalized_with_file_limit(output_batch_file_limit())
    }

    fn normalized_with_file_limit(self, file_limit: usize) -> Self {
        Self {
            max_files: self
                .max_files
                .clamp(1, MAX_OUTPUT_BATCH_FILES.min(file_limit.max(1))),
            max_bytes: self.max_bytes.clamp(1, MAX_OUTPUT_BATCH_BYTES),
            workers: self.workers.clamp(1, MAX_OUTPUT_SYNC_WORKERS),
        }
    }
}

fn output_batch_file_limit() -> usize {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return MAX_OUTPUT_BATCH_FILES;
    }
    let soft = unsafe { limit.assume_init() }.rlim_cur;
    if soft == libc::RLIM_INFINITY {
        return MAX_OUTPUT_BATCH_FILES;
    }
    let soft = soft.min(usize::MAX as libc::rlim_t) as usize;
    soft.saturating_sub(OUTPUT_BATCH_FD_HEADROOM)
        .clamp(1, MAX_OUTPUT_BATCH_FILES)
}

trait OutputSyncWorker: Send + Sync {
    fn sync(&self, batch_index: usize, output: &TempOutput) -> io::Result<()>;
}

struct FileOutputSyncWorker;

impl OutputSyncWorker for FileOutputSyncWorker {
    fn sync(&self, _batch_index: usize, output: &TempOutput) -> io::Result<()> {
        output
            .file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "temporary output is closed"))?
            .sync_all()
    }
}

struct FilePublicationBatch {
    config: OutputBatchConfig,
    sync_worker: Arc<dyn OutputSyncWorker>,
    pending: Vec<PendingOutput>,
    pending_bytes: u64,
    #[cfg(test)]
    post_commit_hook: Option<Arc<dyn Fn(usize) -> Result<()> + Send + Sync>>,
}

impl FilePublicationBatch {
    fn new() -> Self {
        Self::with_worker(
            OutputBatchConfig::from_env(),
            Arc::new(FileOutputSyncWorker),
        )
    }

    fn with_worker(config: OutputBatchConfig, sync_worker: Arc<dyn OutputSyncWorker>) -> Self {
        Self {
            config: config.normalized(),
            sync_worker,
            pending: Vec::new(),
            pending_bytes: 0,
            #[cfg(test)]
            post_commit_hook: None,
        }
    }

    fn should_flush_before(&self, bytes: u64) -> bool {
        !self.pending.is_empty()
            && (self.pending.len() >= self.config.max_files
                || self.pending_bytes.saturating_add(bytes) > self.config.max_bytes)
    }

    fn push(&mut self, action_index: usize, prepared: PreparedOutput) -> bool {
        self.pending_bytes = self
            .pending_bytes
            .saturating_add(prepared.final_entry.size());
        self.pending.push(PendingOutput {
            action_index,
            prepared,
        });
        self.pending.len() >= self.config.max_files || self.pending_bytes >= self.config.max_bytes
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn is_full(&self) -> bool {
        self.pending.len() >= self.config.max_files || self.pending_bytes >= self.config.max_bytes
    }

    fn flush(
        &mut self,
        actions: &[Action],
        recorder: &mut ApplyRecorder,
        new_entries: &mut Vec<Entry>,
    ) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }

        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let next = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::channel();
        let worker_count = self.config.workers.min(pending.len()).max(1);
        let spawn_result = thread::scope(|scope| -> io::Result<()> {
            for _ in 0..worker_count {
                let sender = sender.clone();
                let pending = &pending;
                let sync_worker = Arc::clone(&self.sync_worker);
                let next = &next;
                thread::Builder::new().spawn_scoped(scope, move || loop {
                    let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if index >= pending.len() {
                        break;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        sync_worker.sync(index, &pending[index].prepared.output)
                    }))
                    .unwrap_or_else(|panic| {
                        let message = panic
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("unknown panic");
                        Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("sync worker panicked: {}", message),
                        ))
                    });
                    if sender.send((index, result)).is_err() {
                        break;
                    }
                })?;
            }
            Ok(())
        });
        drop(sender);
        spawn_result.wrap_err("failed to start temporary file sync worker")?;

        let mut results: Vec<Option<io::Result<()>>> = (0..pending.len()).map(|_| None).collect();
        for (index, result) in receiver {
            results[index] = Some(result);
        }
        for (index, result) in results.into_iter().enumerate() {
            match result {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    return Err(error).wrap_err_with(|| {
                        format!(
                            "failed to sync output for {} at batch item {}",
                            pending[index].prepared.final_entry.path().display(),
                            index
                        )
                    });
                }
                None => {
                    return Err(eyre!(
                        "temporary file sync worker stopped before syncing batch item {}",
                        index
                    ));
                }
            }
        }

        for item in pending {
            let action_index = item.action_index;
            let PreparedOutput {
                output,
                final_entry,
                publication,
            } = item.prepared;
            let on_commit = |entry: &Entry| -> Result<()> {
                recorder.record_committed_step("rename-file", entry.path())?;
                new_entries.push(entry.clone());
                recorder.record_committed_step("update-metadata", entry.path())?;
                recorder.record_committed_action(&actions[action_index])?;
                #[cfg(test)]
                if let Some(hook) = &self.post_commit_hook {
                    hook(action_index)?;
                }
                Ok(())
            };
            match publication {
                OutputPublication::Replace { expected } => {
                    output.publish_replacing(final_entry, &expected, on_commit)?;
                }
                OutputPublication::NoReplace { description } => {
                    output.publish_without_replacing(final_entry, &description, on_commit)?;
                }
            }
        }
        Ok(())
    }

    fn seal_into(
        &mut self,
        prepared_outputs: &mut [Option<PreparedOutput>],
        recovery_attempt: Option<(&Path, &str)>,
    ) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        self.pending_bytes = 0;
        let next = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::channel();
        let worker_count = self.config.workers.min(pending.len()).max(1);
        thread::scope(|scope| -> io::Result<()> {
            for _ in 0..worker_count {
                let sender = sender.clone();
                let pending = &pending;
                let sync_worker = Arc::clone(&self.sync_worker);
                let next = &next;
                thread::Builder::new().spawn_scoped(scope, move || loop {
                    let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if index >= pending.len() {
                        break;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        sync_worker.sync(index, &pending[index].prepared.output)
                    }))
                    .unwrap_or_else(|panic| {
                        let message = panic
                            .downcast_ref::<&str>()
                            .copied()
                            .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                            .unwrap_or("unknown panic");
                        Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("sync worker panicked: {}", message),
                        ))
                    });
                    if sender.send((index, result)).is_err() {
                        break;
                    }
                })?;
            }
            Ok(())
        })
        .wrap_err("failed to start temporary file sync worker")?;
        drop(sender);
        let mut results: Vec<Option<io::Result<()>>> = (0..pending.len()).map(|_| None).collect();
        for (index, result) in receiver {
            results[index] = Some(result);
        }
        for (index, result) in results.into_iter().enumerate() {
            match result {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    return Err(error).wrap_err_with(|| {
                        format!(
                            "failed to sync output for {} at batch item {}",
                            pending[index].prepared.final_entry.path().display(),
                            index
                        )
                    });
                }
                None => {
                    return Err(eyre!(
                        "temporary file sync worker stopped before syncing batch item {}",
                        index
                    ));
                }
            }
        }
        if let Some((state_path, attempt_id)) = recovery_attempt {
            sync_v2_marker_entries(state_path, attempt_id)?;
        }
        for mut item in pending {
            if recovery_attempt.is_some() {
                item.prepared.output.cleanup_on_drop = false;
            }
            item.prepared.output.close_after_sync()?;
            if prepared_outputs[item.action_index]
                .replace(item.prepared)
                .is_some()
            {
                return Err(eyre!(
                    "duplicate prepared output for action {}",
                    item.action_index
                ));
            }
        }
        Ok(())
    }
}

fn create_staging_directory(
    stage_parent: &Path,
    stage_parent_directory: &fs::File,
) -> Result<(PathBuf, std::ffi::CString, fs::File)> {
    for _ in 0..128 {
        let stage_component = format!(
            ".duet-stage-{}-{:016x}-{}",
            std::process::id(),
            temp_nonce(),
            TEMP_OUTPUT_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
        );
        let stage_name = path_component_cstring(stage_component.as_ref(), "stage directory name")?;
        let stage_dir = stage_parent.join(&stage_component);
        match cvt(unsafe {
            libc::mkdirat(
                stage_parent_directory.as_raw_fd(),
                stage_name.as_ptr(),
                0o700,
            )
        }) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).wrap_err_with(|| {
                    format!(
                        "failed to create temporary directory {}",
                        stage_dir.display()
                    )
                });
            }
        }
        let created_stat = match fstatat_nofollow(stage_parent_directory.as_raw_fd(), &stage_name) {
            Ok(stat) => stat,
            Err(error) => {
                if let Ok(directory) = openat_file(
                    stage_parent_directory.as_raw_fd(),
                    &stage_name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                ) {
                    cleanup_stage_at(stage_parent_directory, &stage_name, &directory);
                }
                return Err(error).wrap_err_with(|| {
                    format!(
                        "failed to inspect new temporary directory {}",
                        stage_dir.display()
                    )
                });
            }
        };
        if created_stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || created_stat.st_uid != unsafe { libc::geteuid() }
        {
            cleanup_unopened_stage_at(stage_parent_directory, &stage_name, &created_stat);
            return Err(eyre!(
                "new temporary directory path {} was replaced before it could be opened",
                stage_dir.display()
            ));
        }
        let access_directory = match open_new_stage_for_access(
            stage_parent_directory,
            &stage_name,
            &created_stat,
            &stage_dir,
        ) {
            Ok(directory) => directory,
            Err(error) => {
                cleanup_unopened_stage_at(stage_parent_directory, &stage_name, &created_stat);
                return Err(error).wrap_err_with(|| {
                    format!(
                        "failed to retain temporary directory {}",
                        stage_dir.display()
                    )
                });
            }
        };
        if let Err(error) = verify_retained_directory_at_identity(
            stage_parent_directory,
            &stage_name,
            &access_directory,
            &created_stat,
            &stage_dir,
            "new temporary directory",
        ) {
            cleanup_stage_at(stage_parent_directory, &stage_name, &access_directory);
            return Err(error);
        }
        if let Err(error) = normalize_stage_directory_mode(
            stage_parent_directory,
            &stage_name,
            &access_directory,
            &created_stat,
            &stage_dir,
        ) {
            cleanup_stage_at(stage_parent_directory, &stage_name, &access_directory);
            return Err(error);
        }
        let directory = match openat_file(
            stage_parent_directory.as_raw_fd(),
            &stage_name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(directory) => directory,
            Err(e) => {
                cleanup_unopened_stage_at(stage_parent_directory, &stage_name, &created_stat);
                return Err(e).wrap_err_with(|| {
                    format!("failed to open temporary directory {}", stage_dir.display())
                });
            }
        };
        let secure_result = (|| -> Result<()> {
            verify_directory_at_identity(
                stage_parent_directory,
                &stage_name,
                &directory,
                &stage_dir,
            )?;
            let opened_meta = directory.metadata().wrap_err_with(|| {
                format!(
                    "failed to inspect temporary directory {}",
                    stage_dir.display()
                )
            })?;
            if created_stat.st_dev as u64 != opened_meta.dev()
                || created_stat.st_ino as u64 != opened_meta.ino()
            {
                return Err(eyre!(
                    "new temporary directory path {} was replaced before it could be opened",
                    stage_dir.display()
                ));
            }
            if opened_meta.mode() & 0o7777 != 0o700 {
                return Err(eyre!(
                    "temporary directory {} mode was not normalized to 0700",
                    stage_dir.display()
                ));
            }
            Ok(())
        })();
        if let Err(error) = secure_result {
            cleanup_stage_at(stage_parent_directory, &stage_name, &directory);
            return Err(error);
        }
        return Ok((stage_dir, stage_name, directory));
    }

    Err(eyre!(
        "failed to create a unique temporary directory in {}",
        stage_parent.display()
    ))
}

fn path_component_cstring(
    component: &std::ffi::OsStr,
    description: &str,
) -> Result<std::ffi::CString> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.as_bytes().contains(&b'/')
    {
        return Err(eyre!("{} is not an exact path component", description));
    }
    std::ffi::CString::new(component.as_bytes())
        .map_err(|_| eyre!("{} contains an interior NUL byte", description))
}

fn cvt(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn openat_file(
    directory: RawFd,
    name: &std::ffi::CStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<fs::File> {
    let fd = unsafe { libc::openat(directory, name.as_ptr(), flags, mode as libc::c_uint) };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

fn fstatat_nofollow(directory: RawFd, name: &std::ffi::CStr) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::uninit();
    cvt(unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(unsafe { stat.assume_init() })
}

fn normalize_stage_directory_mode(
    parent: &fs::File,
    name: &std::ffi::CStr,
    retained: &fs::File,
    created: &libc::stat,
    path: &Path,
) -> Result<()> {
    verify_retained_directory_at_identity(
        parent,
        name,
        retained,
        created,
        path,
        "new temporary directory",
    )?;
    set_retained_directory_mode(retained, 0o700, path)
        .wrap_err_with(|| format!("failed to secure temporary directory {}", path.display()))?;
    let retained_after = retained.metadata().wrap_err_with(|| {
        format!(
            "failed to verify retained temporary directory {}",
            path.display()
        )
    })?;
    let after = fstatat_nofollow(parent.as_raw_fd(), name)
        .wrap_err_with(|| format!("failed to verify temporary directory {}", path.display()))?;
    verify_stat_identity(
        created,
        &after,
        libc::S_IFDIR,
        path,
        "new temporary directory",
    )?;
    if retained_after.dev() != created.st_dev as u64
        || retained_after.ino() != created.st_ino as u64
        || !retained_after.is_dir()
        || retained_after.mode() & 0o7777 != 0o700
        || after.st_mode & 0o7777 != 0o700
    {
        return Err(eyre!(
            "temporary directory {} mode was not normalized to 0700",
            path.display()
        ));
    }
    Ok(())
}

fn verify_stat_identity(
    expected: &libc::stat,
    actual: &libc::stat,
    expected_type: libc::mode_t,
    path: &Path,
    description: &str,
) -> Result<()> {
    if actual.st_mode & libc::S_IFMT != expected_type
        || actual.st_dev != expected.st_dev
        || actual.st_ino != expected.st_ino
    {
        return Err(eyre!(
            "{} path {} no longer refers to the retained object",
            description,
            path.display()
        ));
    }
    Ok(())
}

fn verify_retained_directory_at_identity(
    parent: &fs::File,
    name: &std::ffi::CStr,
    retained: &fs::File,
    created: &libc::stat,
    path: &Path,
    description: &str,
) -> Result<()> {
    let retained_meta = retained.metadata().wrap_err_with(|| {
        format!(
            "failed to inspect retained {} {}",
            description,
            path.display()
        )
    })?;
    if !retained_meta.is_dir()
        || retained_meta.dev() != created.st_dev as u64
        || retained_meta.ino() != created.st_ino as u64
    {
        return Err(eyre!(
            "{} handle {} does not refer to the newly created directory",
            description,
            path.display()
        ));
    }
    let current = fstatat_nofollow(parent.as_raw_fd(), name)
        .wrap_err_with(|| format!("failed to inspect {} path {}", description, path.display()))?;
    verify_stat_identity(created, &current, libc::S_IFDIR, path, description)
}

fn unlinkat(directory: RawFd, name: &std::ffi::CStr, flags: libc::c_int) -> io::Result<()> {
    cvt(unsafe { libc::unlinkat(directory, name.as_ptr(), flags) })
}

fn verify_directory_at_identity(
    parent: &fs::File,
    name: &std::ffi::CStr,
    directory: &fs::File,
    path: &Path,
) -> Result<()> {
    let expected = directory
        .metadata()
        .wrap_err_with(|| format!("failed to inspect temporary directory {}", path.display()))?;
    let actual = fstatat_nofollow(parent.as_raw_fd(), name).wrap_err_with(|| {
        format!(
            "failed to inspect temporary directory path {}",
            path.display()
        )
    })?;
    if actual.st_mode & libc::S_IFMT != libc::S_IFDIR
        || actual.st_dev as u64 != expected.dev()
        || actual.st_ino as u64 != expected.ino()
    {
        return Err(eyre!(
            "temporary directory path {} no longer refers to the retained directory",
            path.display()
        ));
    }
    Ok(())
}

fn verify_path_identity(path: &Path, retained: &fs::File, description: &str) -> Result<()> {
    let expected = retained.metadata().wrap_err_with(|| {
        format!(
            "failed to inspect retained {} {}",
            description,
            path.display()
        )
    })?;
    let actual = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to inspect {} path {}", description, path.display()))?;
    if !actual.is_dir() || actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        return Err(eyre!(
            "{} path {} no longer refers to the retained directory",
            description,
            path.display()
        ));
    }
    Ok(())
}

fn verify_same_directory_handles(
    expected: &fs::File,
    actual: &fs::File,
    path: &Path,
    description: &str,
) -> Result<()> {
    let expected = expected.metadata().wrap_err_with(|| {
        format!(
            "failed to inspect retained {} {}",
            description,
            path.display()
        )
    })?;
    let actual = actual.metadata().wrap_err_with(|| {
        format!(
            "failed to inspect reopened {} {}",
            description,
            path.display()
        )
    })?;
    if !actual.is_dir() || actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        return Err(eyre!(
            "{} path {} no longer refers to the retained directory",
            description,
            path.display()
        ));
    }
    Ok(())
}

fn cleanup_stage_at(parent: &fs::File, stage_name: &std::ffi::CStr, directory: &fs::File) {
    if directory
        .metadata()
        .map(|meta| meta.uid() != unsafe { libc::geteuid() })
        .unwrap_or(true)
    {
        return;
    }
    if verify_directory_at_identity(parent, stage_name, directory, Path::new("stage directory"))
        .is_err()
    {
        return;
    }
    let _ = unlinkat(parent.as_raw_fd(), stage_name, libc::AT_REMOVEDIR);
}

fn cleanup_unopened_stage_at(
    parent: &fs::File,
    stage_name: &std::ffi::CStr,
    created_stat: &libc::stat,
) {
    let Ok(current_stat) = fstatat_nofollow(parent.as_raw_fd(), stage_name) else {
        return;
    };
    if current_stat.st_mode & libc::S_IFMT == libc::S_IFDIR
        && current_stat.st_dev == created_stat.st_dev
        && current_stat.st_ino == created_stat.st_ino
    {
        let _ = unlinkat(parent.as_raw_fd(), stage_name, libc::AT_REMOVEDIR);
    }
}

fn temp_nonce() -> u64 {
    let mut bytes = [0u8; 8];
    if fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
    {
        return u64::from_ne_bytes(bytes);
    }

    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ TEMP_OUTPUT_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
}

struct WritableDirGuard {
    path: PathBuf,
    directory: fs::File,
    original_mode: u32,
}

impl WritableDirGuard {
    fn new(path: &Path) -> Result<(fs::File, Option<Self>)> {
        Self::new_with_expected(path, None)
    }

    fn new_with_expected(
        path: &Path,
        expected: Option<DirectoryIdentity>,
    ) -> Result<(fs::File, Option<Self>)> {
        let path_meta = fs::symlink_metadata(path).wrap_err_with(|| {
            format!("failed to read directory metadata for {}", path.display())
        })?;
        if !path_meta.is_dir() {
            return Err(eyre!("output parent {} is not a directory", path.display()));
        }
        if let Some(expected) = expected {
            if path_meta.dev() != expected.dev || path_meta.ino() != expected.ino {
                return Err(eyre!(
                    "output parent directory path {} no longer refers to the recorded directory",
                    path.display()
                ));
            }
        }
        let original_mode = path_meta.permissions().mode();
        if owner_write_execute(original_mode) {
            let directory = open_directory_for_access(path)?;
            verify_path_identity(path, &directory, "output parent directory")?;
            if let Some(expected) = expected {
                verify_directory_handle_identity(
                    &directory,
                    expected,
                    path,
                    "output parent directory",
                )?;
            }
            return Ok((directory, None));
        }
        let directory = match open_directory_for_access(path) {
            Ok(directory) => directory,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied) =>
            {
                open_directory_after_bootstrap_widening(path, &path_meta, expected, original_mode)?
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = verify_path_identity(path, &directory, "output parent directory") {
            let _ = set_retained_directory_mode(&directory, original_mode, path);
            return Err(error);
        }
        if let Some(expected) = expected {
            if let Err(error) = verify_directory_handle_identity(
                &directory,
                expected,
                path,
                "output parent directory",
            ) {
                let _ = set_retained_directory_mode(&directory, original_mode, path);
                return Err(error);
            }
        }
        if let Err(error) = set_retained_directory_mode(&directory, original_mode | 0o700, path) {
            let _ = set_retained_directory_mode(&directory, original_mode, path);
            return Err(error).wrap_err_with(|| {
                format!(
                    "failed to make directory writable for sync {}",
                    path.display()
                )
            });
        }
        if let Err(error) = verify_path_identity(path, &directory, "output parent directory") {
            let _ = set_retained_directory_mode(&directory, original_mode, path);
            return Err(error);
        }
        let guard_directory = match directory.try_clone() {
            Ok(directory) => directory,
            Err(error) => {
                let _ = set_retained_directory_mode(&directory, original_mode, path);
                return Err(error).wrap_err_with(|| {
                    format!("failed to retain directory handle for {}", path.display())
                });
            }
        };
        Ok((
            directory,
            Some(Self {
                path: path.to_path_buf(),
                directory: guard_directory,
                original_mode,
            }),
        ))
    }

    fn from_retained(path: &Path, directory: &fs::File) -> Result<Option<Self>> {
        verify_path_identity(path, directory, "temporary directory parent")?;
        let metadata = directory.metadata().wrap_err_with(|| {
            format!(
                "failed to inspect directory permissions for {}",
                path.display()
            )
        })?;
        let original_mode = metadata.permissions().mode() & 0o7777;
        if owner_write_execute(original_mode) {
            return Ok(None);
        }
        let guard_directory = directory.try_clone().wrap_err_with(|| {
            format!("failed to retain directory handle for {}", path.display())
        })?;
        set_retained_directory_mode(directory, original_mode | 0o700, path).wrap_err_with(
            || {
                format!(
                    "failed to make directory writable for sync {}",
                    path.display()
                )
            },
        )?;
        if let Err(error) = verify_path_identity(path, directory, "temporary directory parent") {
            let _ = set_retained_directory_mode(directory, original_mode, path);
            return Err(error);
        }
        Ok(Some(Self {
            path: path.to_path_buf(),
            directory: guard_directory,
            original_mode,
        }))
    }

    fn restore(mut self) -> Result<()> {
        set_retained_directory_mode(&self.directory, self.original_mode, &self.path)
            .wrap_err_with(|| {
                format!(
                    "failed to restore directory permissions after sync {}",
                    self.path.display()
                )
            })?;
        verify_path_identity(&self.path, &self.directory, "output parent directory")?;
        self.path.clear();
        Ok(())
    }
}

fn open_directory_after_bootstrap_widening(
    path: &Path,
    initial: &fs::Metadata,
    expected: Option<DirectoryIdentity>,
    original_mode: u32,
) -> Result<fs::File> {
    let parent_path = output_parent(path);
    let name = path_component_cstring(
        path.file_name()
            .ok_or_else(|| eyre!("output parent {} has no final component", path.display()))?,
        "output parent directory name",
    )?;
    let parent = open_directory_for_access(parent_path)?;
    verify_path_identity(parent_path, &parent, "output parent ancestor")?;
    let expected = expected.unwrap_or(DirectoryIdentity {
        dev: initial.dev(),
        ino: initial.ino(),
    });
    let retained =
        open_permission_independent_directory_at(&parent, &name).wrap_err_with(|| {
            format!(
                "failed to retain mode-independent access to output parent {}",
                path.display()
            )
        })?;
    verify_directory_handle_identity(&retained, expected, path, "output parent directory")?;
    verify_path_identity(path, &retained, "output parent directory")?;
    if let Err(error) = set_retained_directory_mode(&retained, original_mode | 0o700, path) {
        let restore = set_retained_directory_mode(&retained, original_mode, path);
        return match restore {
            Ok(()) => Err(error).wrap_err_with(|| {
                format!(
                    "failed to bootstrap publication access to output parent {}",
                    path.display()
                )
            }),
            Err(restore) => Err(eyre!(
                "failed to bootstrap publication access to output parent {}: {}; additionally failed to restore mode {:04o}: {}",
                path.display(),
                error,
                original_mode,
                restore
            )),
        };
    }

    let directory = match open_directory_at_for_access(&parent, &name) {
        Ok(directory) => directory,
        Err(error) => {
            let restore = set_retained_directory_mode(&retained, original_mode, path);
            return match restore {
                Ok(()) => Err(error).wrap_err_with(|| {
                    format!("failed to open widened output parent {}", path.display())
                }),
                Err(restore) => Err(eyre!(
                    "failed to open widened output parent {}: {}; additionally failed to restore mode {:04o}: {}",
                    path.display(),
                    error,
                    original_mode,
                    restore
                )),
            };
        }
    };
    let verify =
        verify_directory_handle_identity(&directory, expected, path, "output parent directory")
            .and_then(|()| {
                verify_same_directory_handles(
                    &retained,
                    &directory,
                    path,
                    "output parent directory",
                )
            })
            .and_then(|()| verify_path_identity(path, &directory, "output parent directory"));
    if let Err(error) = verify {
        let restore = set_retained_directory_mode(&retained, original_mode, path);
        return match restore {
            Ok(()) => Err(error),
            Err(restore) => Err(eyre!(
                "{}; additionally failed to restore output parent {} mode {:04o}: {}",
                error,
                path.display(),
                original_mode,
                restore
            )),
        };
    }
    Ok(directory)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_permission_independent_directory_at(
    parent: &fs::File,
    name: &std::ffi::CStr,
) -> io::Result<fs::File> {
    openat_file(
        parent.as_raw_fd(),
        name,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

#[cfg(target_vendor = "apple")]
fn open_permission_independent_directory_at(
    _parent: &fs::File,
    _name: &std::ffi::CStr,
) -> io::Result<fs::File> {
    // O_EVTONLY still requests FREAD unless the process has Apple's private
    // disallow-rw-for-o-evtonly entitlement. Normal processes therefore
    // cannot use it to retain a mode-000 directory safely.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "safe mode-independent directory descriptors require a private O_EVTONLY entitlement on Apple platforms",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn open_permission_independent_directory_at(
    _parent: &fs::File,
    _name: &std::ffi::CStr,
) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "safe mode-independent directory descriptors are unsupported on this platform",
    ))
}

fn open_directory_for_access(path: &Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            directory_access_flag() | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
        .open(path)
        .wrap_err_with(|| format!("failed to retain access to directory {}", path.display()))
}

fn open_directory_at_for_access(parent: &fs::File, name: &std::ffi::CStr) -> io::Result<fs::File> {
    openat_file(
        parent.as_raw_fd(),
        name,
        directory_access_flag() | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_new_stage_for_access(
    parent: &fs::File,
    name: &std::ffi::CStr,
    _created: &libc::stat,
    _path: &Path,
) -> Result<fs::File> {
    open_directory_at_for_access(parent, name).map_err(Into::into)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn open_new_stage_for_access(
    parent: &fs::File,
    name: &std::ffi::CStr,
    created: &libc::stat,
    path: &Path,
) -> Result<fs::File> {
    match open_directory_at_for_access(parent, name) {
        Ok(directory) => return Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        Err(error) => return Err(error.into()),
    }

    let before = fstatat_nofollow(parent.as_raw_fd(), name).wrap_err_with(|| {
        format!(
            "failed to inspect new temporary directory {}",
            path.display()
        )
    })?;
    verify_stat_identity(
        created,
        &before,
        libc::S_IFDIR,
        path,
        "new temporary directory",
    )?;
    cvt(unsafe {
        libc::fchmodat(
            parent.as_raw_fd(),
            name.as_ptr(),
            0o700,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })
    .wrap_err_with(|| {
        format!(
            "failed to bootstrap search access to new temporary directory {} without following symlinks",
            path.display()
        )
    })?;
    let after = fstatat_nofollow(parent.as_raw_fd(), name).wrap_err_with(|| {
        format!(
            "failed to verify new temporary directory {}",
            path.display()
        )
    })?;
    verify_stat_identity(
        created,
        &after,
        libc::S_IFDIR,
        path,
        "new temporary directory",
    )?;
    open_directory_at_for_access(parent, name).wrap_err_with(|| {
        format!(
            "failed to retain new temporary directory {} after securing it",
            path.display()
        )
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn directory_access_flag() -> libc::c_int {
    libc::O_PATH
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos"
))]
fn directory_access_flag() -> libc::c_int {
    libc::O_SEARCH
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "solaris",
    target_os = "illumos"
)))]
fn directory_access_flag() -> libc::c_int {
    // Other Unix targets retain the portable O_RDONLY behavior; mode-0300
    // directories are not guaranteed to be supported there.
    libc::O_RDONLY
}

fn access_descriptor_needs_readable_sync(error: &io::Error) -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos"
    )) && matches!(
        error.raw_os_error(),
        Some(libc::EBADF) | Some(libc::EINVAL) | Some(libc::ENOTSUP)
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_retained_directory_mode(directory: &fs::File, mode: u32, path: &Path) -> Result<()> {
    match directory.set_permissions(fs::Permissions::from_mode(mode)) {
        Ok(()) => return Ok(()),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EBADF) | Some(libc::EINVAL) | Some(libc::ENOTSUP)
            ) => {}
        Err(error) => return Err(error.into()),
    }

    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    fs::set_permissions(&proc_path, fs::Permissions::from_mode(mode)).wrap_err_with(|| {
        format!(
            "direct descriptor chmod is unsupported and /proc/self/fd fallback is unavailable for retained directory {}",
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_retained_directory_mode(directory: &fs::File, mode: u32, _path: &Path) -> Result<()> {
    directory
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(Into::into)
}

impl Drop for WritableDirGuard {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = set_retained_directory_mode(&self.directory, self.original_mode, &self.path);
        }
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        if self.cleanup_on_drop
            && self
                .verify_at_identity(&self.staging.directory, &self.output_name, &self.temp_path)
                .is_ok()
        {
            let _ = unlinkat(self.staging.directory.as_raw_fd(), &self.output_name, 0);
        }
    }
}

fn probe_cow_clone(base: &Path) -> Result<bool> {
    let staging = StagingArea::new(base)?;
    let shared = staging.shared();
    let source_path = base.join(".duet-cow-probe-source");
    let mut source = TempOutput::new(source_path, Arc::clone(&shared))?;
    let source_file = source
        .file
        .as_mut()
        .ok_or_else(|| eyre!("COW probe source is closed"))?;
    source_file.write_all(&[0])?;
    source_file.sync_all()?;

    let clone_path = base.join(".duet-cow-probe-clone");
    let cloned = TempOutput::clone_from(clone_path, shared, source_file)?;
    Ok(cloned.is_some())
}

struct ApplyRecorder {
    state_path: Option<PathBuf>,
    marker_path: Option<PathBuf>,
    file: Option<fs::File>,
}

impl ApplyRecorder {
    fn new(state_path: Option<PathBuf>) -> Self {
        Self {
            state_path,
            marker_path: None,
            file: None,
        }
    }

    fn record_staged_file(&mut self, path: &Path) -> Result<()> {
        self.write_line(
            "record staged path",
            format!("staged-file: {}\n", path.display()),
        )
    }

    fn record_committed_step(&mut self, operation: &str, path: &Path) -> Result<()> {
        self.write_line(
            "record committed step",
            format!("committed-step: {} {}\n", operation, path.display()),
        )
    }

    fn record_committed_action(&mut self, action: &Action) -> Result<()> {
        let Some(change) = applied_change(action) else {
            return Ok(());
        };
        self.write_line(
            "record committed operation",
            format!(
                "committed-operation: {} {}\n",
                change_operation(change),
                action.path().display()
            ),
        )
    }

    fn write_line(&mut self, operation: &str, line: String) -> Result<()> {
        let Some(state_path) = &self.state_path else {
            return Ok(());
        };

        if self.file.is_none() {
            let marker_path = apply_attempt_path(state_path)?;
            let file = fs::OpenOptions::new()
                .append(true)
                .open(&marker_path)
                .wrap_err_with(|| {
                    format!(
                        "unable to {} in apply recovery marker {}",
                        operation,
                        marker_path.display()
                    )
                })?;
            self.marker_path = Some(marker_path);
            self.file = Some(file);
        }

        let marker_path = self
            .marker_path
            .as_ref()
            .expect("marker path is initialized");
        self.file
            .as_mut()
            .expect("marker file is initialized")
            .write_all(line.as_bytes())
            .wrap_err_with(|| {
                format!(
                    "unable to {} in apply recovery marker {}",
                    operation,
                    marker_path.display()
                )
            })
    }
}

fn ensure_parent_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        let mut missing = Vec::new();
        let mut current = parent;
        while !current.try_exists().wrap_err_with(|| {
            format!(
                "failed to check destination parent directory {}",
                current.display()
            )
        })? {
            missing.push(current);
            current = current.parent().ok_or_else(|| {
                eyre!(
                    "destination parent directory {} has no existing ancestor",
                    parent.display()
                )
            })?;
        }
        for directory in missing.into_iter().rev() {
            create_private_directory(directory)?;
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .wrap_err_with(|| format!("failed to create directory {}", path.display()))?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .wrap_err_with(|| format!("failed to open new directory {}", path.display()))?;
    let meta = directory
        .metadata()
        .wrap_err_with(|| format!("failed to inspect new directory {}", path.display()))?;
    if !meta.is_dir() {
        return Err(eyre!(
            "new directory path {} is not a directory",
            path.display()
        ));
    }
    let private_mode = 0o700 | (meta.mode() & 0o2000);
    directory
        .set_permissions(fs::Permissions::from_mode(private_mode))
        .wrap_err_with(|| {
            format!(
                "failed to normalize directory permissions {}",
                path.display()
            )
        })
}

enum ApplyState {
    File {
        action_index: usize,
        output: TempOutput,
        verifier: StreamedOutputVerifier,
    },
    Diff {
        action_index: usize,
        source: fs::File,
        output: TempOutput,
        verifier: StreamedOutputVerifier,
        output_position: u64,
        clone_backed: bool,
    },
}

struct StreamedOutputVerifier {
    bytes: u64,
    checksum: Option<adler32::RollingAdler32>,
    digest: Option<blake2_rfc::blake2b::Blake2b>,
}

impl StreamedOutputVerifier {
    fn new(entry: &Entry) -> Self {
        let strong = entry.digest().is_some();
        Self {
            bytes: 0,
            checksum: (!strong).then(adler32::RollingAdler32::new),
            digest: strong.then(|| blake2_rfc::blake2b::Blake2b::new(32)),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if let Some(checksum) = &mut self.checksum {
            checksum.update_buffer(bytes);
        }
        if let Some(digest) = &mut self.digest {
            digest.update(bytes);
        }
    }

    fn verify(self, entry: &Entry) -> Result<()> {
        if self.bytes != entry.size() {
            return Err(eyre!(
                "file output {} size mismatch: expected {}, got {}",
                entry.path().display(),
                entry.size(),
                self.bytes
            ));
        }
        if let Some(expected) = entry.digest() {
            let digest = self
                .digest
                .ok_or_else(|| eyre!("file output verifier did not compute a strong digest"))?
                .finalize();
            let mut bytes = [0; 32];
            bytes.copy_from_slice(digest.as_bytes());
            let actual = ContentDigest(bytes);
            if actual != expected {
                return Err(eyre!(
                    "file output {} strong digest mismatch: expected {}, got {}",
                    entry.path().display(),
                    expected,
                    actual
                ));
            }
        } else {
            let actual = self
                .checksum
                .ok_or_else(|| eyre!("file output verifier did not compute a legacy checksum"))?
                .hash();
            if actual != entry.checksum() {
                return Err(eyre!(
                    "file output {} legacy checksum mismatch: expected {}, got {}",
                    entry.path().display(),
                    entry.checksum(),
                    actual
                ));
            }
        }
        Ok(())
    }
}

pub struct DetailApplier {
    base: PathBuf,
    actions: Vec<Action>,
    all_old: Vec<Entry>,
    attempt_state: Option<PathBuf>,
    attempt_id: Option<String>,
    recorder: ApplyRecorder,
    scan_policy: Option<ScanPolicy>,
    apply_options: ApplyOptions,
    old_index: usize,
    action_index: usize,
    new_entries: Vec<Entry>,
    state: Option<ApplyState>,
    output_batch: FilePublicationBatch,
    prepared_outputs: Vec<Option<PreparedOutput>>,
    // Drop pending outputs before removing their shared staging directory.
    staging: Option<StagingArea>,
    staging_space_monitor: Option<StagingSpaceMonitor>,
    failed: Option<String>,
}

pub struct PreparedApply {
    inner: DetailApplier,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedApplyReport {
    pub action_count: usize,
    pub prepared_file_count: usize,
    pub prepared_file_bytes: u64,
}

impl DetailApplier {
    #[allow(dead_code)]
    pub fn new_with_attempt(
        base: PathBuf,
        actions: Vec<Action>,
        all_old: Vec<Entry>,
        attempt_state: Option<PathBuf>,
    ) -> Self {
        Self::new_with_attempt_and_policy(
            base,
            actions,
            all_old,
            attempt_state,
            None,
            ApplyOptions::default(),
        )
    }

    #[allow(dead_code)]
    pub(crate) fn new_staged_with_attempt_and_policy(
        base: PathBuf,
        actions: Vec<Action>,
        all_old: Vec<Entry>,
        attempt_state: PathBuf,
        attempt_id: String,
        scan_policy: Option<ScanPolicy>,
        apply_options: ApplyOptions,
    ) -> Self {
        let mut applier = Self::new_with_attempt_and_policy(
            base,
            actions,
            all_old,
            Some(attempt_state),
            scan_policy,
            apply_options,
        );
        applier.attempt_id = Some(attempt_id);
        applier
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_capacity_aware_staged_with_attempt_and_policy(
        base: PathBuf,
        actions: Vec<Action>,
        all_old: Vec<Entry>,
        attempt_state: PathBuf,
        attempt_id: String,
        scan_policy: Option<ScanPolicy>,
        apply_options: ApplyOptions,
        staging_policy: StagingPolicy,
    ) -> Self {
        let mut applier = Self::new_with_attempt_and_policy(
            base.clone(),
            actions,
            all_old,
            Some(attempt_state),
            scan_policy,
            apply_options,
        );
        applier.attempt_id = Some(attempt_id);
        applier.staging_space_monitor = Some(StagingSpaceMonitor {
            base,
            policy: staging_policy,
            state: Arc::new(Mutex::new(StagingSpaceMonitorState::default())),
        });
        applier
    }

    pub fn new_with_attempt_and_policy(
        base: PathBuf,
        actions: Vec<Action>,
        all_old: Vec<Entry>,
        attempt_state: Option<PathBuf>,
        scan_policy: Option<ScanPolicy>,
        apply_options: ApplyOptions,
    ) -> Self {
        let mut prepared_outputs = Vec::with_capacity(actions.len());
        prepared_outputs.resize_with(actions.len(), || None);
        DetailApplier {
            base,
            actions,
            all_old,
            recorder: ApplyRecorder::new(attempt_state.clone()),
            attempt_state,
            attempt_id: None,
            scan_policy,
            apply_options,
            old_index: 0,
            action_index: 0,
            new_entries: Vec::new(),
            state: None,
            staging: None,
            staging_space_monitor: None,
            output_batch: FilePublicationBatch::new(),
            prepared_outputs,
            failed: None,
        }
    }

    pub fn apply_frame(&mut self, frame: DetailFrame) -> Result<()> {
        if let Some(failed) = &self.failed {
            return Err(eyre!("detail apply stream already failed: {}", failed));
        }
        let result = self.apply_frame_inner(frame);
        if let Err(error) = &result {
            self.failed = Some(format!("{:#}", error));
        }
        result
    }

    fn apply_frame_inner(&mut self, frame: DetailFrame) -> Result<()> {
        let frame_index = frame.action_index as usize;
        if frame_index >= self.actions.len() {
            return Err(eyre!(
                "detail frame references missing action {}",
                frame_index
            ));
        }

        let staging_space_monitor = self.staging_space_monitor.clone();
        match &mut self.state {
            Some(ApplyState::File {
                action_index,
                output,
                verifier,
            }) => {
                if *action_index != frame_index {
                    return Err(eyre!(
                        "detail frame for action {} arrived while applying action {}",
                        frame_index,
                        action_index
                    ));
                }
                match frame.payload {
                    DetailPayload::FileBytes(bytes) => {
                        if let Some(monitor) = &staging_space_monitor {
                            monitor.check(&output.final_path, bytes.len() as u64)?;
                        }
                        output
                            .file
                            .as_mut()
                            .ok_or_else(|| eyre!("temporary output is closed"))?
                            .write_all(&bytes)?;
                        verifier.update(&bytes);
                    }
                    DetailPayload::FileEnd => self.finish_file_detail()?,
                    _ => return Err(eyre!("unexpected file detail frame")),
                }
                return Ok(());
            }
            Some(ApplyState::Diff {
                action_index,
                source,
                output,
                verifier,
                output_position,
                clone_backed,
            }) => {
                if *action_index != frame_index {
                    return Err(eyre!(
                        "detail frame for action {} arrived while applying action {}",
                        frame_index,
                        action_index
                    ));
                }
                match frame.payload {
                    DetailPayload::DiffCopy { offset, len } => {
                        if !(*clone_backed && offset == *output_position) {
                            if let Some(monitor) = &staging_space_monitor {
                                monitor.check(&output.final_path, len)?;
                            }
                        }
                        let output_file = output
                            .file
                            .as_mut()
                            .ok_or_else(|| eyre!("temporary output is closed"))?;
                        apply_diff_copy(
                            source,
                            output_file,
                            verifier,
                            output_position,
                            *clone_backed,
                            offset,
                            len,
                        )?;
                    }
                    DetailPayload::DiffBytes(bytes) => {
                        if let Some(monitor) = &staging_space_monitor {
                            monitor.check(&output.final_path, bytes.len() as u64)?;
                        }
                        let output_file = output
                            .file
                            .as_mut()
                            .ok_or_else(|| eyre!("temporary output is closed"))?;
                        apply_diff_bytes(output_file, verifier, output_position, &bytes)?;
                    }
                    DetailPayload::DiffEnd => {
                        output
                            .file
                            .as_ref()
                            .ok_or_else(|| eyre!("temporary output is closed"))?
                            .set_len(*output_position)?;
                        self.finish_file_detail()?;
                    }
                    _ => return Err(eyre!("unexpected diff detail frame")),
                }
                return Ok(());
            }
            None => {}
        }

        if frame_index < self.action_index {
            return Err(eyre!(
                "detail frame for action {} arrived after action {} was already processed",
                frame_index,
                self.action_index.saturating_sub(1)
            ));
        }

        self.advance_to_action(frame_index)?;
        let expected_detail = apply_detail_kind(&self.actions[frame_index]);
        match frame.payload {
            DetailPayload::FileBegin if expected_detail == Some(ApplyDetailKind::File) => {
                self.begin_file_detail(frame_index)
            }
            DetailPayload::DiffBegin if expected_detail == Some(ApplyDetailKind::Diff) => {
                self.begin_diff_detail(frame_index)
            }
            DetailPayload::FileBegin | DetailPayload::DiffBegin => {
                Err(eyre!("unexpected detail kind for action {}", frame_index))
            }
            _ => Err(eyre!(
                "detail stream for action {} did not begin with a begin frame",
                frame_index
            )),
        }
    }

    pub fn apply_file_byte_chunk(&mut self, chunk: FileByteChunk) -> Result<()> {
        self.apply_frame(DetailFrame {
            action_index: chunk.action_index,
            payload: DetailPayload::FileBytes(chunk.into_bytes()),
        })
    }

    pub fn finish(self) -> Result<Vec<Entry>> {
        self.finish_preparation()?.commit()
    }

    #[allow(dead_code)]
    pub fn prepare(self) -> Result<PreparedApply> {
        self.finish_preparation()
    }

    pub fn finish_preparation(mut self) -> Result<PreparedApply> {
        if let Some(failed) = &self.failed {
            return Err(eyre!("detail apply stream already failed: {}", failed));
        }
        if self.state.is_some() {
            return Err(eyre!("detail stream ended with an unfinished file"));
        }
        self.advance_to_action(self.actions.len())?;
        self.seal_outputs()?;
        if let Some(staging) = &self.staging {
            staging.seal()?;
        }
        if let Some(monitor) = &self.staging_space_monitor {
            monitor.recheck(&self.base)?;
        }
        validate_actions(&self.actions)?;
        if let (Some(state_path), Some(attempt_id)) =
            (self.attempt_state.as_deref(), self.attempt_id.as_deref())
        {
            transition_staged_apply_attempt(
                state_path,
                attempt_id,
                &[ApplyAttemptPhase::Preparing],
                ApplyAttemptPhase::Prepared,
            )?;
        }
        Ok(PreparedApply { inner: self })
    }

    fn advance_to_action(&mut self, target_index: usize) -> Result<()> {
        while self.action_index < target_index {
            if apply_detail_kind(&self.actions[self.action_index]).is_some() {
                return Err(eyre!(
                    "missing detail frames for action {}",
                    self.action_index
                ));
            }
            self.action_index += 1;
        }
        Ok(())
    }

    fn prepare_action(&mut self, action_index: usize) {
        let path = self.actions[action_index].path();
        loop {
            let oe = self.all_old.get(self.old_index);
            if let Some(e) = oe {
                match e.path().cmp(path) {
                    Ordering::Less => {
                        self.new_entries.push(e.clone());
                        self.old_index += 1;
                    }
                    Ordering::Equal => {
                        let e = e.clone();
                        self.old_index += 1;
                        if self.actions[action_index].is_unresolved_conflict() {
                            self.new_entries.push(e);
                        }
                        continue;
                    }
                    Ordering::Greater => break,
                }
            } else {
                break;
            }
        }
    }

    fn apply_action_without_detail(&mut self, action_index: usize) -> Result<()> {
        self.prepare_action(action_index);
        match &self.actions[action_index] {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => match change {
                Change::Removed(e) => {
                    let filename = safe_join(&self.base, e.path())?;
                    if !e.is_dir() {
                        verify_current_matches_entry(&filename, e, "remove target")?;
                        fs::remove_file(&filename)?;
                        record_committed_step(
                            self.attempt_state.as_deref(),
                            "remove-file",
                            e.path(),
                        )?;
                    }
                }
                Change::Added(e) => {
                    let filename = safe_join(&self.base, e.path())?;
                    ensure_parent_directory(&filename)?;
                    if let Some(p) = e.target() {
                        std::os::unix::fs::symlink(p, &filename)?;
                        record_committed_step(
                            self.attempt_state.as_deref(),
                            "create-symlink",
                            e.path(),
                        )?;
                        self.new_entries.push(update_meta(&filename, e)?);
                        record_committed_step(
                            self.attempt_state.as_deref(),
                            "update-metadata",
                            e.path(),
                        )?;
                    } else if e.is_dir() {
                        create_private_directory(&filename)?;
                        record_committed_step(
                            self.attempt_state.as_deref(),
                            "create-dir",
                            e.path(),
                        )?;
                    } else {
                        return Err(eyre!("missing file detail for {}", e.path().display()));
                    }
                }
                Change::Modified(e1, e2) => {
                    let filename = safe_join(&self.base, e2.path())?;
                    if e1.is_file() {
                        if e2.is_file() {
                            if !e1.same_contents(e2) {
                                return Err(eyre!(
                                    "missing diff detail for {}",
                                    e2.path().display()
                                ));
                            } else {
                                verify_current_matches_entry(&filename, e1, "metadata target")?;
                            }
                            self.new_entries.push(update_meta(&filename, e2)?);
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "update-metadata",
                                e2.path(),
                            )?;
                        } else {
                            verify_current_matches_entry(&filename, e1, "replace target")?;
                            fs::remove_file(&filename)?;
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "remove-file",
                                e1.path(),
                            )?;
                            if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &filename)?;
                                record_committed_step(
                                    self.attempt_state.as_deref(),
                                    "create-symlink",
                                    e2.path(),
                                )?;
                                self.new_entries.push(update_meta(&filename, e2)?);
                                record_committed_step(
                                    self.attempt_state.as_deref(),
                                    "update-metadata",
                                    e2.path(),
                                )?;
                            } else if e2.is_dir() {
                                create_private_directory(&filename)?;
                                record_committed_step(
                                    self.attempt_state.as_deref(),
                                    "create-dir",
                                    e2.path(),
                                )?;
                            } else {
                                return Err(eyre!(
                                    "unsupported new entry for {}",
                                    e2.path().display()
                                ));
                            }
                        }
                    } else if e1.is_symlink() {
                        if e2.is_file() {
                            return Err(eyre!("missing file detail for {}", e2.path().display()));
                        }
                        verify_current_matches_entry(&filename, e1, "replace target")?;
                        fs::remove_file(&filename)?;
                        record_committed_step(
                            self.attempt_state.as_deref(),
                            "remove-symlink",
                            e1.path(),
                        )?;
                        if let Some(p) = e2.target() {
                            std::os::unix::fs::symlink(p, &filename)?;
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "create-symlink",
                                e2.path(),
                            )?;
                            self.new_entries.push(update_meta(&filename, e2)?);
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "update-metadata",
                                e2.path(),
                            )?;
                        } else if e2.is_dir() {
                            create_private_directory(&filename)?;
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "create-dir",
                                e2.path(),
                            )?;
                        }
                    } else if e1.is_dir() {
                        if e2.is_file() {
                            return Err(eyre!(
                                "streaming directory-to-file changes is not supported"
                            ));
                        }
                    } else {
                        return Err(eyre!("unsupported old entry for {}", e1.path().display()));
                    }
                }
            },
            Action::Remote(change) | Action::ResolvedRemote((_, _), change) => match change {
                Change::Removed(_) => {}
                Change::Added(e) | Change::Modified(_, e) => self.new_entries.push(e.clone()),
            },
            Action::Identical(change, _) => match change {
                Change::Removed(_) => {}
                Change::Added(e) | Change::Modified(_, e) => self.new_entries.push(e.clone()),
            },
            Action::Conflict(_, _) => {}
        }
        if let Some(change) = applied_change(&self.actions[action_index]) {
            if !change.is_dir() {
                record_committed_action(
                    self.attempt_state.as_deref(),
                    &self.actions[action_index],
                )?;
            }
        }
        Ok(())
    }

    fn begin_file_detail(&mut self, action_index: usize) -> Result<()> {
        let entry = action_output_entry(&self.actions[action_index])?;
        let output_bytes = entry.size();
        let verifier = StreamedOutputVerifier::new(entry);
        self.flush_before_output(output_bytes)?;
        let filename = detail_filename(&self.base, &self.actions[action_index])?;
        let output = self.new_output(filename)?;
        self.state = Some(ApplyState::File {
            action_index,
            output,
            verifier,
        });
        Ok(())
    }

    fn begin_diff_detail(&mut self, action_index: usize) -> Result<()> {
        let entry = action_output_entry(&self.actions[action_index])?;
        let output_bytes = entry.size();
        let verifier = StreamedOutputVerifier::new(entry);
        self.flush_before_output(output_bytes)?;
        let filename = detail_filename(&self.base, &self.actions[action_index])?;
        let old_entry = match &self.actions[action_index] {
            Action::Local(Change::Modified(e, _))
            | Action::ResolvedLocal((_, _), Change::Modified(e, _)) => e,
            _ => return Err(eyre!("diff detail began for non-diff action")),
        };
        let mut source = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&filename)
            .wrap_err_with(|| format!("failed to open diff source {}", filename.display()))?;
        verify_open_file_matches_entry(&mut source, &filename, old_entry, "diff source")?;
        let (output, clone_backed) = self.new_diff_output(filename, &source)?;
        self.state = Some(ApplyState::Diff {
            action_index,
            source,
            output,
            verifier,
            output_position: 0,
            clone_backed,
        });
        Ok(())
    }

    fn new_diff_output(
        &mut self,
        final_path: PathBuf,
        source: &fs::File,
    ) -> Result<(TempOutput, bool)> {
        if let Some(monitor) = &self.staging_space_monitor {
            monitor.check(&final_path, 0)?;
        }
        self.ensure_staging()?;
        let staging = self
            .staging
            .as_ref()
            .expect("staging area is initialized")
            .shared();
        let (output, clone_backed) =
            match TempOutput::clone_from(final_path.clone(), Arc::clone(&staging), source)? {
                Some(output) => (output, true),
                None => (TempOutput::new(final_path, staging)?, false),
            };
        self.record_new_output(&output)?;
        Ok((output, clone_backed))
    }

    fn new_output(&mut self, final_path: PathBuf) -> Result<TempOutput> {
        if let Some(monitor) = &self.staging_space_monitor {
            monitor.check(&final_path, 0)?;
        }
        self.ensure_staging()?;
        let output = TempOutput::new(
            final_path,
            self.staging
                .as_ref()
                .expect("staging area is initialized")
                .shared(),
        )?;
        self.record_new_output(&output)?;
        Ok(output)
    }

    fn ensure_staging(&mut self) -> Result<()> {
        if self.staging.is_none() {
            let mut staging = StagingArea::new(&self.base)?;
            if let (Some(state_path), Some(attempt_id)) =
                (self.attempt_state.as_deref(), self.attempt_id.as_deref())
            {
                record_v2_stage(state_path, attempt_id, &staging)?;
                staging.retain_for_recovery();
            } else {
                self.recorder.record_staged_file(staging.path())?;
            }
            self.staging = Some(staging);
        }
        Ok(())
    }

    fn record_new_output(&self, output: &TempOutput) -> Result<()> {
        if let (Some(state_path), Some(_)) =
            (self.attempt_state.as_deref(), self.attempt_id.as_deref())
        {
            record_v2_stage_entry(state_path, output)?;
        }
        Ok(())
    }

    fn finish_file_detail(&mut self) -> Result<()> {
        let state = self
            .state
            .take()
            .ok_or_else(|| eyre!("no file detail in progress"))?;
        let (action_index, output, verifier) = match state {
            ApplyState::File {
                action_index,
                output,
                verifier,
            } => (action_index, output, verifier),
            ApplyState::Diff {
                action_index,
                output,
                verifier,
                ..
            } => (action_index, output, verifier),
        };
        let entry = match &self.actions[action_index] {
            Action::Local(Change::Added(e))
            | Action::ResolvedLocal((_, _), Change::Added(e))
            | Action::Local(Change::Modified(_, e))
            | Action::ResolvedLocal((_, _), Change::Modified(_, e)) => e,
            _ => return Err(eyre!("file detail finished for non-file action")),
        };
        verifier.verify(entry)?;
        let publication =
            if let Some(old_entry) = replacement_old_entry(&self.actions[action_index]) {
                OutputPublication::Replace {
                    expected: old_entry.clone(),
                }
            } else {
                OutputPublication::NoReplace {
                    description: "rename target".to_string(),
                }
            };
        let prepared = output.prepare(entry, publication)?;
        let flush = self.output_batch.push(action_index, prepared);
        self.action_index = action_index + 1;
        if flush {
            self.seal_outputs()?;
        }
        Ok(())
    }

    fn flush_before_output(&mut self, bytes: u64) -> Result<()> {
        if self.output_batch.should_flush_before(bytes) {
            self.seal_outputs()?;
        }
        Ok(())
    }

    fn seal_outputs(&mut self) -> Result<()> {
        self.output_batch.seal_into(
            &mut self.prepared_outputs,
            self.attempt_state
                .as_deref()
                .zip(self.attempt_id.as_deref()),
        )?;
        if let Some(monitor) = &self.staging_space_monitor {
            monitor.recheck(&self.base)?;
        }
        Ok(())
    }

    fn publish_prepared_output(&mut self, action_index: usize) -> Result<()> {
        let PreparedOutput {
            output,
            final_entry,
            publication,
        } = self.prepared_outputs[action_index]
            .take()
            .ok_or_else(|| eyre!("missing prepared output for action {}", action_index))?;
        ensure_parent_directory(&output.final_path)?;
        let action = &self.actions[action_index];
        let recorder = &mut self.recorder;
        let new_entries = &mut self.new_entries;
        #[cfg(test)]
        let post_commit_hook = self.output_batch.post_commit_hook.clone();
        let on_commit = |entry: &Entry| -> Result<()> {
            recorder.record_committed_step("rename-file", entry.path())?;
            new_entries.push(entry.clone());
            recorder.record_committed_step("update-metadata", entry.path())?;
            recorder.record_committed_action(action)?;
            #[cfg(test)]
            if let Some(hook) = &post_commit_hook {
                hook(action_index)?;
            }
            Ok(())
        };
        match publication {
            OutputPublication::Replace { expected } => {
                output.publish_replacing(final_entry, &expected, on_commit)?;
            }
            OutputPublication::NoReplace { description } => {
                output.publish_without_replacing(final_entry, &description, on_commit)?;
            }
        }
        Ok(())
    }

    fn apply_directory_second_pass(&mut self) -> Result<()> {
        let removal_policy =
            RemovalBlockerPolicy::new(self.scan_policy.as_ref(), self.apply_options)?;
        let removed_paths = removed_destination_paths(&self.actions);
        for action in self.actions.iter().rev() {
            match action {
                Action::Local(change) | Action::ResolvedLocal((_, _), change) => {
                    if !change.is_dir() {
                        continue;
                    }
                    match change {
                        Change::Removed(e) => {
                            let dirname = safe_join(&self.base, e.path())?;
                            verify_current_matches_entry(&dirname, e, "remove target")?;
                            prune_ignored_removal_blockers(
                                &self.base,
                                &dirname,
                                &removed_paths,
                                &removal_policy,
                                self.attempt_state.as_deref(),
                            )?;
                            fs::remove_dir(&dirname)?;
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "remove-dir",
                                e.path(),
                            )?;
                        }
                        Change::Added(e) => {
                            let dirname = safe_join(&self.base, e.path())?;
                            self.new_entries.push(update_meta(&dirname, e)?);
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "update-metadata",
                                e.path(),
                            )?;
                        }
                        Change::Modified(e1, e2) => {
                            let dirname = safe_join(&self.base, e2.path())?;
                            if e1.is_dir() && !e2.is_dir() {
                                return Err(eyre!(
                                    "streaming directory-to-file changes is not supported"
                                ));
                            }
                            if e1.is_dir() && e2.is_dir() {
                                verify_current_matches_entry(&dirname, e1, "metadata target")?;
                            }
                            self.new_entries.push(update_meta(&dirname, e2)?);
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "update-metadata",
                                e2.path(),
                            )?;
                        }
                    }
                    record_committed_action(self.attempt_state.as_deref(), action)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl PreparedApply {
    #[allow(dead_code)]
    pub fn report(&self) -> PreparedApplyReport {
        let mut prepared_file_count = 0;
        let mut prepared_file_bytes = 0u64;
        for output in self.inner.prepared_outputs.iter().flatten() {
            prepared_file_count += 1;
            prepared_file_bytes = prepared_file_bytes.saturating_add(output.final_entry.size());
        }
        PreparedApplyReport {
            action_count: self.inner.actions.len(),
            prepared_file_count,
            prepared_file_bytes,
        }
    }

    pub fn validate_commit(&self) -> Result<()> {
        preflight_apply_with_policy(
            &self.inner.base,
            &self.inner.actions,
            self.inner.scan_policy.as_ref(),
            self.inner.apply_options,
        )?;
        for output in self.inner.prepared_outputs.iter().flatten() {
            output.output.verify_at_identity(
                &output.output.staging.directory,
                &output.output.output_name,
                &output.output.temp_path,
            )?;
            output
                .output
                .verify_prepared_contents(&output.final_entry)?;
        }
        for action in &self.inner.actions {
            let change = match action {
                Action::Local(change) | Action::ResolvedLocal((_, _), change) => change,
                _ => continue,
            };
            let path = safe_join(&self.inner.base, change.path())?;
            match change {
                Change::Added(_) => match path.symlink_metadata() {
                    Ok(_) => {
                        return Err(eyre!(
                            "destination {} appeared after staged preparation",
                            path.display()
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).wrap_err_with(|| {
                            format!("unable to validate destination {}", path.display())
                        });
                    }
                },
                Change::Removed(old) | Change::Modified(old, _) => {
                    verify_current_matches_entry(&path, old, "staged commit target")?;
                }
            }
        }
        if let Some(monitor) = &self.inner.staging_space_monitor {
            monitor.recheck(&self.inner.base)?;
        }
        Ok(())
    }

    pub fn commit(mut self) -> Result<Vec<Entry>> {
        self.validate_commit()?;
        if let (Some(state_path), Some(attempt_id)) = (
            self.inner.attempt_state.as_deref(),
            self.inner.attempt_id.as_deref(),
        ) {
            transition_staged_apply_attempt(
                state_path,
                attempt_id,
                &[ApplyAttemptPhase::Prepared],
                ApplyAttemptPhase::Committing,
            )?;
        }

        self.inner.old_index = 0;
        self.inner.action_index = 0;
        self.inner.new_entries.clear();
        for action_index in 0..self.inner.actions.len() {
            if self.inner.prepared_outputs[action_index].is_some() {
                self.inner.prepare_action(action_index);
                self.inner.publish_prepared_output(action_index)?;
            } else {
                self.inner.apply_action_without_detail(action_index)?;
            }
            self.inner.action_index = action_index + 1;
        }
        self.inner.apply_directory_second_pass()?;

        for entry in self.inner.all_old.iter().skip(self.inner.old_index) {
            self.inner.new_entries.push(entry.clone());
        }
        self.inner.new_entries.sort();
        let metadata_synced = metadata_synced_directories(&self.inner.base, &self.inner.actions);
        let already_synced = match self.inner.staging.take() {
            Some(staging) => staging.finish(&metadata_synced)?,
            None => HashSet::new(),
        };
        complete_apply_phase(
            &self.inner.base,
            &self.inner.actions,
            self.inner.attempt_state.as_deref(),
            &already_synced,
        )?;
        if let (Some(state_path), Some(attempt_id)) = (
            self.inner.attempt_state.as_deref(),
            self.inner.attempt_id.as_deref(),
        ) {
            transition_staged_apply_attempt(
                state_path,
                attempt_id,
                &[ApplyAttemptPhase::Committing],
                ApplyAttemptPhase::Committed,
            )?;
        }
        Ok(std::mem::take(&mut self.inner.new_entries))
    }

    #[allow(dead_code)]
    pub fn abort(mut self) -> Result<()> {
        if let (Some(state_path), Some(attempt_id)) = (
            self.inner.attempt_state.as_deref(),
            self.inner.attempt_id.as_deref(),
        ) {
            abort_staged_apply_attempt(state_path, attempt_id)?;
        }
        self.inner.output_batch.pending.clear();
        for output in &mut self.inner.prepared_outputs {
            *output = None;
        }
        self.inner.staging = None;
        if self.inner.attempt_id.is_none() {
            if let Some(state_path) = self.inner.attempt_state.as_deref() {
                finish_apply_attempt(state_path)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApplyDetailKind {
    File,
    Diff,
}

fn apply_detail_kind(action: &Action) -> Option<ApplyDetailKind> {
    let change = match action {
        Action::Local(change) | Action::ResolvedLocal((_, _), change) => change,
        _ => return None,
    };

    apply_detail_kind_for_change(change)
}

fn apply_detail_kind_for_change(change: &Change) -> Option<ApplyDetailKind> {
    match change {
        Change::Removed(_) => None,
        Change::Added(e) => e.is_file().then_some(ApplyDetailKind::File),
        Change::Modified(e1, e2) => {
            if e1.is_file() && e2.is_file() && !e1.same_contents(e2) {
                Some(ApplyDetailKind::Diff)
            } else if !e1.is_file() && e2.is_file() {
                Some(ApplyDetailKind::File)
            } else {
                None
            }
        }
    }
}

fn detail_filename(base: &Path, action: &Action) -> Result<PathBuf> {
    match action {
        Action::Local(Change::Added(e))
        | Action::ResolvedLocal((_, _), Change::Added(e))
        | Action::Local(Change::Modified(_, e))
        | Action::ResolvedLocal((_, _), Change::Modified(_, e)) => safe_join(base, e.path()),
        _ => Err(eyre!("action has no detail filename")),
    }
}

fn action_output_entry(action: &Action) -> Result<&Entry> {
    let change = match action {
        Action::Local(change) | Action::ResolvedLocal((_, _), change) => change,
        _ => return Err(eyre!("action has no regular file output")),
    };
    change_output_entry(change)
}

fn change_output_entry(change: &Change) -> Result<&Entry> {
    let entry = match change {
        Change::Added(entry) | Change::Modified(_, entry) => entry,
        Change::Removed(_) => return Err(eyre!("action has no regular file output")),
    };
    if !entry.is_file() {
        return Err(eyre!("action has no regular file output"));
    }
    Ok(entry)
}

fn replacement_old_entry(action: &Action) -> Option<&Entry> {
    match action {
        Action::Local(Change::Modified(e, _))
        | Action::ResolvedLocal((_, _), Change::Modified(e, _)) => Some(e),
        _ => None,
    }
}

fn next_detail<'a, I>(details_iter: &mut I, path: &Path) -> Result<&'a ChangeDetails>
where
    I: Iterator<Item = &'a ChangeDetails>,
{
    details_iter
        .next()
        .ok_or_else(|| eyre!("missing detail for {}", path.display()))
}

fn fallback_action_can_follow_pending(action: &Action) -> bool {
    matches!(
        action,
        Action::Local(Change::Added(entry))
            | Action::ResolvedLocal((_, _), Change::Added(entry))
            if entry.is_file()
    ) || matches!(
        action,
        Action::Local(Change::Modified(old, new))
            | Action::ResolvedLocal((_, _), Change::Modified(old, new))
            if old.is_file() && new.is_file() && !old.same_contents(new)
    )
}

fn apply_diff_copy(
    source: &mut fs::File,
    output: &mut fs::File,
    verifier: &mut StreamedOutputVerifier,
    output_position: &mut u64,
    clone_backed: bool,
    offset: u64,
    len: u64,
) -> Result<()> {
    if clone_backed && offset == *output_position {
        output.seek(SeekFrom::Start(*output_position))?;
        let copied = hash_file_range(output, verifier, len)?;
        *output_position = output_position
            .checked_add(copied)
            .ok_or_else(|| eyre!("diff output position overflow"))?;
        return Ok(());
    }

    source.seek(SeekFrom::Start(offset))?;
    output.seek(SeekFrom::Start(*output_position))?;
    let copied = copy_and_hash(source, output, verifier, len)?;
    *output_position = output_position
        .checked_add(copied)
        .ok_or_else(|| eyre!("diff output position overflow"))?;
    Ok(())
}

fn apply_diff_bytes(
    output: &mut fs::File,
    verifier: &mut StreamedOutputVerifier,
    output_position: &mut u64,
    bytes: &[u8],
) -> Result<()> {
    output.seek(SeekFrom::Start(*output_position))?;
    output.write_all(bytes)?;
    verifier.update(bytes);
    *output_position = output_position
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| eyre!("diff output position overflow"))?;
    Ok(())
}

fn copy_and_hash(
    source: &mut fs::File,
    output: &mut fs::File,
    verifier: &mut StreamedOutputVerifier,
    len: u64,
) -> Result<u64> {
    let mut remaining = len;
    let mut copied = 0u64;
    let mut buf = vec![0; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let want = std::cmp::min(remaining as usize, buf.len());
        let n = source.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        verifier.update(&buf[..n]);
        remaining -= n as u64;
        copied += n as u64;
    }
    Ok(copied)
}

fn hash_file_range(
    file: &mut fs::File,
    verifier: &mut StreamedOutputVerifier,
    len: u64,
) -> Result<u64> {
    let mut remaining = len;
    let mut hashed = 0u64;
    let mut buf = vec![0; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let want = std::cmp::min(remaining as usize, buf.len());
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        verifier.update(&buf[..n]);
        remaining -= n as u64;
        hashed += n as u64;
    }
    Ok(hashed)
}

#[allow(dead_code)]
pub fn apply_detailed_changes(
    base: &PathBuf,
    actions: &Vec<Action>,
    details: &Vec<ChangeDetails>,
    all_old: &mut Vec<Entry>,
    attempt_state: Option<&Path>,
) -> Result<()> {
    apply_detailed_changes_with_policy(
        base,
        actions,
        details,
        all_old,
        attempt_state,
        None,
        ApplyOptions::default(),
    )
}

pub fn apply_detailed_changes_with_policy(
    base: &PathBuf,
    actions: &Vec<Action>,
    details: &Vec<ChangeDetails>,
    all_old: &mut Vec<Entry>,
    attempt_state: Option<&Path>,
    scan_policy: Option<&ScanPolicy>,
    apply_options: ApplyOptions,
) -> Result<()> {
    apply_detailed_changes_with_output_batch(
        base,
        actions,
        details,
        all_old,
        attempt_state,
        scan_policy,
        apply_options,
        FilePublicationBatch::new(),
    )
}

fn apply_detailed_changes_with_output_batch(
    base: &PathBuf,
    actions: &Vec<Action>,
    details: &Vec<ChangeDetails>,
    all_old: &mut Vec<Entry>,
    attempt_state: Option<&Path>,
    scan_policy: Option<&ScanPolicy>,
    apply_options: ApplyOptions,
    output_batch: FilePublicationBatch,
) -> Result<()> {
    validate_actions(actions)?;
    let removal_policy = RemovalBlockerPolicy::new(scan_policy, apply_options)?;
    log::debug!("details.len() = {}", details.len());
    let mut details_iter = details.iter();
    let mut new_entries: Vec<Entry> = Vec::new();
    let mut old_iter = all_old.iter().peekable();
    let mut leftover_details: Vec<&ChangeDetails> = Vec::new();
    let mut staging = None;
    // Drop pending outputs before their shared staging directory on early return.
    let mut output_batch = output_batch;
    let mut recorder = ApplyRecorder::new(attempt_state.map(Path::to_path_buf));

    for (action_index, action) in actions.iter().enumerate() {
        if !fallback_action_can_follow_pending(action) {
            output_batch.flush(actions, &mut recorder, &mut new_entries)?;
        }
        let path = action.path();
        loop {
            let oe = old_iter.peek();
            if let Some(e) = oe {
                match e.path().cmp(path) {
                    Ordering::Less => {
                        new_entries.push(old_iter.next().unwrap().clone());
                    }
                    Ordering::Equal => {
                        let e = old_iter.next().unwrap();
                        if action.is_unresolved_conflict() {
                            new_entries.push(e.clone()); // preserve the original
                        }
                        continue;
                    } // action will deal with this
                    Ordering::Greater => {
                        break;
                    }
                }
            } else {
                break;
            }
        }

        match action {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => {
                log::debug!("applying detailed change to {}", action.path().display());
                let mut queued_file_output = false;
                match change {
                    Change::Removed(e) => {
                        let filename = safe_join(base, e.path())?;
                        log::debug!("Removing {:?}", filename);
                        if !e.is_dir() {
                            verify_current_matches_entry(&filename, e, "remove target")?;
                            fs::remove_file(&filename).wrap_err_with(|| {
                                format!("failed to remove file {}", filename.display())
                            })?;
                            record_committed_step(attempt_state, "remove-file", e.path())?;
                        } // else: removing directory;
                          //   must happen after all the files have been removed, which will happen
                          //   in the second pass
                          // nothing gets copied into new_entries
                    }
                    Change::Added(e) => {
                        let filename = safe_join(base, e.path())?;
                        ensure_parent_directory(&filename)?;
                        if let Some(p) = e.target() {
                            std::os::unix::fs::symlink(p, &filename).wrap_err_with(|| {
                                format!(
                                    "failed to create symlink {} -> {}",
                                    filename.display(),
                                    p.display()
                                )
                            })?;
                            record_committed_step(attempt_state, "create-symlink", e.path())?;
                            new_entries.push(update_meta(&filename, e)?);
                            record_committed_step(attempt_state, "update-metadata", e.path())?;
                        } else if e.is_dir() {
                            create_private_directory(&filename)?;
                            record_committed_step(attempt_state, "create-dir", e.path())?;
                            // new entry gets updated in the second pass, after all the updates in
                            // the directory are finished
                        } else {
                            log::debug!("Adding {}", e.path().display());
                            let detail = next_detail(&mut details_iter, e.path())?;
                            if output_batch.should_flush_before(e.size()) {
                                output_batch.flush(actions, &mut recorder, &mut new_entries)?;
                            }
                            create_file(
                                &filename,
                                detail,
                                e,
                                &mut staging,
                                &mut recorder,
                                action_index,
                                &mut output_batch,
                            )?;
                            queued_file_output = true;
                            if output_batch.is_full() {
                                output_batch.flush(actions, &mut recorder, &mut new_entries)?;
                            }
                        }
                    }
                    Change::Modified(e1, e2) => {
                        let filename = safe_join(base, e2.path())?;
                        if e1.is_file() {
                            if e2.is_file() {
                                if !e1.same_contents(&e2) {
                                    let detail = next_detail(&mut details_iter, e2.path())?;
                                    match detail {
                                        ChangeDetails::Diff(delta) => {
                                            if output_batch.should_flush_before(e2.size()) {
                                                output_batch.flush(
                                                    actions,
                                                    &mut recorder,
                                                    &mut new_entries,
                                                )?;
                                            }
                                            update_file_with_diff(
                                                &filename,
                                                e1,
                                                e2,
                                                delta,
                                                &mut staging,
                                                &mut recorder,
                                                action_index,
                                                &mut output_batch,
                                            )?;
                                            queued_file_output = true;
                                            if output_batch.is_full() {
                                                output_batch.flush(
                                                    actions,
                                                    &mut recorder,
                                                    &mut new_entries,
                                                )?;
                                            }
                                        }
                                        _ => {
                                            return Err(eyre!(
                                            "mismatch when adding {}, expected Diff, but not found",
                                            e1.path().display()
                                        ))
                                        }
                                    }
                                } else {
                                    verify_current_matches_entry(&filename, e1, "metadata target")?;
                                }
                                if e1.same_contents(e2) {
                                    new_entries.push(update_meta(&filename, e2)?);
                                }
                                if !queued_file_output {
                                    record_committed_step(
                                        attempt_state,
                                        "update-metadata",
                                        e2.path(),
                                    )?;
                                }
                            } else {
                                // e2 not a file
                                // remove the file
                                verify_current_matches_entry(&filename, e1, "replace target")?;
                                fs::remove_file(&filename).wrap_err_with(|| {
                                    format!("failed to remove file {}", filename.display())
                                })?;
                                record_committed_step(attempt_state, "remove-file", e1.path())?;
                                if let Some(p) = e2.target() {
                                    std::os::unix::fs::symlink(p, &filename).wrap_err_with(
                                        || {
                                            format!(
                                                "failed to create symlink {} -> {}",
                                                filename.display(),
                                                p.display()
                                            )
                                        },
                                    )?;
                                    record_committed_step(
                                        attempt_state,
                                        "create-symlink",
                                        e2.path(),
                                    )?;
                                    new_entries.push(update_meta(&filename, e2)?);
                                    record_committed_step(
                                        attempt_state,
                                        "update-metadata",
                                        e2.path(),
                                    )?;
                                } else if e2.is_dir() {
                                    create_private_directory(&filename)?;
                                    record_committed_step(attempt_state, "create-dir", e2.path())?;
                                } else {
                                    return Err(eyre!(
                                        "unsupported new entry for {}",
                                        e2.path().display()
                                    ));
                                }
                            }
                        } else if e1.is_symlink() {
                            // remove the symlink
                            verify_current_matches_entry(&filename, e1, "replace target")?;
                            fs::remove_file(&filename).wrap_err_with(|| {
                                format!("failed to remove file {}", filename.display())
                            })?;
                            record_committed_step(attempt_state, "remove-symlink", e1.path())?;
                            if e2.is_file() {
                                let detail = next_detail(&mut details_iter, e2.path())?;
                                if output_batch.should_flush_before(e2.size()) {
                                    output_batch.flush(actions, &mut recorder, &mut new_entries)?;
                                }
                                create_file(
                                    &filename,
                                    detail,
                                    e2,
                                    &mut staging,
                                    &mut recorder,
                                    action_index,
                                    &mut output_batch,
                                )?;
                                queued_file_output = true;
                                if output_batch.is_full() {
                                    output_batch.flush(actions, &mut recorder, &mut new_entries)?;
                                }
                            } else if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &filename).wrap_err_with(|| {
                                    format!(
                                        "failed to create symlink {} -> {}",
                                        filename.display(),
                                        p.display()
                                    )
                                })?;
                                record_committed_step(attempt_state, "create-symlink", e2.path())?;
                                new_entries.push(update_meta(&filename, e2)?);
                                record_committed_step(attempt_state, "update-metadata", e2.path())?;
                            } else if e2.is_dir() {
                                create_private_directory(&filename)?;
                                record_committed_step(attempt_state, "create-dir", e2.path())?;
                                // new entry gets updated in the second pass, after all the updates in
                                // the directory are finished
                            }
                        } else if e1.is_dir() {
                            if e2.is_file() {
                                // need to save the file contents for after we remove the directory
                                let detail = next_detail(&mut details_iter, e2.path())?;
                                leftover_details.push(detail);
                            }
                        } else {
                            return Err(eyre!("unsupported old entry for {}", e1.path().display()));
                        }
                    }
                }
                if !change.is_dir() && !queued_file_output {
                    record_committed_action(attempt_state, action)?;
                }
            }
            Action::Remote(change) | Action::ResolvedRemote((_, _), change) => match change {
                Change::Removed(_) => {}
                Change::Added(e) => {
                    new_entries.push(e.clone());
                }
                Change::Modified(_, e) => {
                    new_entries.push(e.clone());
                }
            },
            Action::Identical(change, _) => match change {
                Change::Removed(_) => {}
                Change::Added(e) => {
                    new_entries.push(e.clone());
                }
                Change::Modified(_, e) => {
                    new_entries.push(e.clone());
                }
            },
            Action::Conflict(_, _) => {} // skip conflicts; only way we get here with them, if we are in the batch force mode
        }
    }

    if details_iter.next().is_some() {
        return Err(eyre!("unexpected extra file detail"));
    }
    output_batch.flush(actions, &mut recorder, &mut new_entries)?;

    // second pass, in reverse order, to remove directories and update their metadata
    let mut details_iter = leftover_details.iter().rev();
    let removed_paths = removed_destination_paths(actions);
    for (action_index, action) in actions.iter().enumerate().rev() {
        output_batch.flush(actions, &mut recorder, &mut new_entries)?;
        match action {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => {
                if !change.is_dir() {
                    continue;
                }
                match change {
                    Change::Removed(e) => {
                        let dirname = safe_join(base, e.path())?;
                        verify_current_matches_entry(&dirname, e, "remove target")?;
                        prune_ignored_removal_blockers(
                            base,
                            &dirname,
                            &removed_paths,
                            &removal_policy,
                            attempt_state,
                        )?;
                        fs::remove_dir(&dirname).wrap_err_with(|| {
                            format!("failed to remove directory {}", dirname.display())
                        })?;
                        record_committed_step(attempt_state, "remove-dir", e.path())?;
                    }
                    Change::Added(e) => {
                        let dirname = safe_join(base, e.path())?;
                        new_entries.push(update_meta(&dirname, e)?);
                        record_committed_step(attempt_state, "update-metadata", e.path())?;
                    }
                    Change::Modified(e1, e2) => {
                        let dirname = safe_join(base, e2.path())?;
                        let mut queued_file_output = false;
                        if e1.is_dir() && !e2.is_dir() {
                            verify_current_matches_entry(&dirname, e1, "replace target")?;
                            prune_ignored_removal_blockers(
                                base,
                                &dirname,
                                &removed_paths,
                                &removal_policy,
                                attempt_state,
                            )?;
                            fs::remove_dir(&dirname).wrap_err_with(|| {
                                format!("failed to remove directory {}", dirname.display())
                            })?;
                            record_committed_step(attempt_state, "remove-dir", e1.path())?;
                            if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &dirname).wrap_err_with(|| {
                                    format!(
                                        "failed to create symlink {} -> {}",
                                        dirname.display(),
                                        p.display()
                                    )
                                })?;
                                record_committed_step(attempt_state, "create-symlink", e2.path())?;
                            } else if e2.is_file() {
                                let detail = details_iter.next().ok_or_else(|| {
                                    eyre!("missing detail for {}", e2.path().display())
                                })?;
                                create_file(
                                    &dirname,
                                    detail,
                                    e2,
                                    &mut staging,
                                    &mut recorder,
                                    action_index,
                                    &mut output_batch,
                                )?;
                                queued_file_output = true;
                                output_batch.flush(actions, &mut recorder, &mut new_entries)?;
                            }
                        }
                        if e1.is_dir() && e2.is_dir() {
                            verify_current_matches_entry(&dirname, e1, "metadata target")?;
                        }
                        if !queued_file_output {
                            new_entries.push(update_meta(&dirname, e2)?);
                            record_committed_step(attempt_state, "update-metadata", e2.path())?;
                        }
                    }
                }
                if output_batch.is_empty() {
                    let is_directory_to_file = matches!(
                        change,
                        Change::Modified(old, new) if old.is_dir() && new.is_file()
                    );
                    if !is_directory_to_file {
                        record_committed_action(attempt_state, action)?;
                    }
                }
            }
            _ => {}
        }
    }

    // copy remaining entries from all_old
    for e in old_iter {
        new_entries.push(e.clone());
    }
    new_entries.sort(); // directory -> file or symlink will be out of order, so need to sort them

    let metadata_synced = metadata_synced_directories(base, actions);
    let already_synced = match staging {
        Some(staging) => staging.finish(&metadata_synced)?,
        None => HashSet::new(),
    };
    complete_apply_phase(base, actions, attempt_state, &already_synced)?;
    std::mem::swap(all_old, &mut new_entries);

    Ok(())
}

fn fallback_output(
    staging: &mut Option<StagingArea>,
    final_path: PathBuf,
    recorder: &mut ApplyRecorder,
) -> Result<TempOutput> {
    if staging.is_none() {
        let area = StagingArea::new(output_parent(&final_path))?;
        recorder.record_staged_file(area.path())?;
        *staging = Some(area);
    }
    TempOutput::new(
        final_path,
        staging
            .as_ref()
            .expect("staging area is initialized")
            .shared(),
    )
}

fn create_file(
    filename: &Path,
    detail: &ChangeDetails,
    entry: &Entry,
    staging: &mut Option<StagingArea>,
    recorder: &mut ApplyRecorder,
    action_index: usize,
    output_batch: &mut FilePublicationBatch,
) -> Result<()> {
    match detail {
        ChangeDetails::Contents(v) => create_file_with_contents(
            filename,
            v,
            entry,
            staging,
            recorder,
            action_index,
            output_batch,
        ),
        _ => Err(eyre!(
            "mismatch when adding {}, expected Contents, but not found",
            filename.display()
        )),
    }
}

fn create_file_with_contents(
    filename: &Path,
    data: &[u8],
    entry: &Entry,
    staging: &mut Option<StagingArea>,
    recorder: &mut ApplyRecorder,
    action_index: usize,
    output_batch: &mut FilePublicationBatch,
) -> Result<()> {
    if data.len() as u64 != entry.size() {
        return Err(eyre!(
            "file detail for {} size mismatch: expected {}, got {}",
            entry.path().display(),
            entry.size(),
            data.len()
        ));
    }
    if let Some(expected) = entry.digest() {
        let actual = content_digest(data);
        if actual != expected {
            return Err(eyre!(
                "file detail for {} strong digest mismatch: expected {}, got {}",
                entry.path().display(),
                expected,
                actual
            ));
        }
    } else {
        let checksum = adler32::adler32(data).wrap_err_with(|| {
            format!(
                "failed to checksum legacy detail for {}",
                entry.path().display()
            )
        })?;
        if checksum != entry.checksum() {
            return Err(eyre!(
                "file detail for {} legacy checksum mismatch: expected {}, got {}",
                entry.path().display(),
                entry.checksum(),
                checksum
            ));
        }
    }

    ensure_parent_directory(filename)?;
    let mut output = fallback_output(staging, filename.to_path_buf(), recorder)?;
    output
        .file
        .as_mut()
        .ok_or_else(|| eyre!("temporary output is closed"))?
        .write_all(data)
        .wrap_err_with(|| format!("failed to write temporary file for {}", filename.display()))?;
    output.verify_contents(entry, "file output")?;
    let prepared = output.prepare(
        entry,
        OutputPublication::NoReplace {
            description: "rename target".to_string(),
        },
    )?;
    output_batch.push(action_index, prepared);
    Ok(())
}

fn verify_file_matches_entry(filename: &Path, entry: &Entry, description: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(filename)
        .wrap_err_with(|| format!("failed to open file {}", filename.display()))?;
    verify_open_file_matches_entry(&mut file, filename, entry, description)
}

fn verify_open_file_matches_entry(
    file: &mut fs::File,
    filename: &Path,
    entry: &Entry,
    description: &str,
) -> Result<()> {
    let meta = file
        .metadata()
        .wrap_err_with(|| format!("failed to read metadata for {}", filename.display()))?;
    if !meta.is_file() {
        return Err(eyre!(
            "{} {} is not a regular file",
            description,
            entry.path().display()
        ));
    }
    if meta.size() != entry.size() {
        return Err(eyre!(
            "{} {} size mismatch: expected {}, got {}",
            description,
            entry.path().display(),
            entry.size(),
            meta.size()
        ));
    }

    file.seek(SeekFrom::Start(0))
        .wrap_err_with(|| format!("failed to seek file {}", filename.display()))?;
    if let Some(expected) = entry.digest() {
        let actual = content_digest_reader(file)
            .wrap_err_with(|| format!("failed to hash {}", filename.display()))?;
        if actual != expected {
            return Err(eyre!(
                "{} {} strong digest mismatch: expected {}, got {}",
                description,
                entry.path().display(),
                expected,
                actual
            ));
        }
    } else {
        let checksum = adler32::adler32(file)
            .wrap_err_with(|| format!("failed to checksum legacy file {}", filename.display()))?;
        if checksum != entry.checksum() {
            return Err(eyre!(
                "{} {} legacy checksum mismatch: expected {}, got {}",
                description,
                entry.path().display(),
                entry.checksum(),
                checksum
            ));
        }
    }

    Ok(())
}

pub(crate) fn content_digest(data: &[u8]) -> ContentDigest {
    let digest = blake2_rfc::blake2b::blake2b(32, &[], data);
    let mut bytes = [0; 32];
    bytes.copy_from_slice(digest.as_bytes());
    ContentDigest(bytes)
}

fn content_digest_reader(reader: &mut impl Read) -> io::Result<ContentDigest> {
    let mut state = blake2_rfc::blake2b::Blake2b::new(32);
    let mut buffer = [0; COPY_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        state.update(&buffer[..read]);
    }
    let digest = state.finalize();
    let mut bytes = [0; 32];
    bytes.copy_from_slice(digest.as_bytes());
    Ok(ContentDigest(bytes))
}

fn verify_current_matches_entry(filename: &Path, entry: &Entry, description: &str) -> Result<()> {
    let meta = fs::symlink_metadata(filename)
        .wrap_err_with(|| format!("failed to read metadata for {}", filename.display()))?;

    if entry.is_file() {
        verify_file_matches_entry(filename, entry, description)?;
    } else if entry.is_dir() {
        if !meta.is_dir() {
            return Err(eyre!(
                "{} {} is not a directory",
                description,
                entry.path().display()
            ));
        }
    } else if entry.is_symlink() {
        if !meta.file_type().is_symlink() {
            return Err(eyre!(
                "{} {} is not a symlink",
                description,
                entry.path().display()
            ));
        }
        let target = fs::read_link(filename)
            .wrap_err_with(|| format!("failed to read symlink {}", filename.display()))?;
        if Some(&target) != entry.target().as_ref() {
            return Err(eyre!(
                "{} {} symlink target mismatch: expected {}, got {}",
                description,
                entry.path().display(),
                entry
                    .target()
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string()),
                target.display()
            ));
        }
    } else {
        return Err(eyre!("unsupported entry for {}", entry.path().display()));
    }

    if !entry.is_symlink() && synced_mode(meta.mode()) != synced_mode(entry.mode()) {
        return Err(eyre!(
            "{} {} mode mismatch: expected {:o}, got {:o}",
            description,
            entry.path().display(),
            synced_mode(entry.mode()),
            synced_mode(meta.mode())
        ));
    }
    if !entry.is_dir() && meta.mtime() != entry.mtime() {
        return Err(eyre!(
            "{} {} mtime mismatch: expected {}, got {}",
            description,
            entry.path().display(),
            entry.mtime(),
            meta.mtime()
        ));
    }

    Ok(())
}

fn update_file_with_diff(
    filename: &Path,
    old_entry: &Entry,
    new_entry: &Entry,
    delta: &Delta,
    staging: &mut Option<StagingArea>,
    recorder: &mut ApplyRecorder,
    action_index: usize,
    output_batch: &mut FilePublicationBatch,
) -> Result<()> {
    validate_delta(delta)?;
    verify_file_matches_entry(filename, old_entry, "diff source")?;
    let source = fs::File::open(filename)
        .wrap_err_with(|| format!("failed to open file {}", filename.display()))?;
    let mut output = fallback_output(staging, filename.to_path_buf(), recorder)?;
    let output_file = output
        .file
        .as_mut()
        .ok_or_else(|| eyre!("temporary output is closed"))?;
    restore_seek(output_file, source, vec![0; delta.window], delta)
        .wrap_err_with(|| format!("failed to restore diff for {}", filename.display()))?;
    output.verify_contents(new_entry, "diff output")?;
    let prepared = output.prepare(
        new_entry,
        OutputPublication::Replace {
            expected: old_entry.clone(),
        },
    )?;
    output_batch.push(action_index, prepared);
    Ok(())
}

fn update_meta(path: &PathBuf, e: &Entry) -> Result<Entry> {
    let meta = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to read metadata for {}", path.display()))?;
    if e.is_symlink() {
        filetime::set_symlink_file_times(
            path,
            filetime::FileTime::from_unix_time(meta.atime(), 0),
            filetime::FileTime::from_unix_time(e.mtime(), 0),
        )
        .wrap_err_with(|| format!("failed to set time for {}", path.display()))?;
    } else {
        let desired_mode = synced_mode(e.mode());
        let file = match open_metadata_target(path, e.is_dir()) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && desired_mode & if e.is_dir() { 0o400 } else { 0o600 } != 0 =>
            {
                fs::set_permissions(path, fs::Permissions::from_mode(desired_mode)).wrap_err_with(
                    || format!("failed to set permissions for {}", path.display()),
                )?;
                open_metadata_target(path, e.is_dir()).wrap_err_with(|| {
                    format!("failed to open metadata target {}", path.display())
                })?
            }
            Err(error) => {
                return Err(error).wrap_err_with(|| {
                    format!("failed to open metadata target {}", path.display())
                });
            }
        };
        file.set_permissions(fs::Permissions::from_mode(desired_mode))
            .wrap_err_with(|| format!("failed to set permissions for {}", path.display()))?;
        filetime::set_file_handle_times(
            &file,
            Some(filetime::FileTime::from_unix_time(meta.atime(), 0)),
            Some(filetime::FileTime::from_unix_time(e.mtime(), 0)),
        )
        .wrap_err_with(|| format!("failed to set time for {}", path.display()))?;
        file.sync_all()
            .wrap_err_with(|| format!("failed to sync metadata for {}", path.display()))?;
    }
    let mut new_entry = e.clone();
    let final_meta = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to verify metadata for {}", path.display()))?;
    new_entry.set_ino(final_meta.ino());
    Ok(new_entry)
}

fn open_metadata_target(path: &Path, is_dir: bool) -> io::Result<fs::File> {
    let flags = libc::O_CLOEXEC | libc::O_NOFOLLOW;
    if is_dir {
        return fs::OpenOptions::new()
            .read(true)
            .custom_flags(flags | libc::O_DIRECTORY)
            .open(path);
    }
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => fs::OpenOptions::new()
            .write(true)
            .custom_flags(flags)
            .open(path),
        Err(error) => Err(error),
    }
}

fn synced_mode(mode: u32) -> u32 {
    mode & SYNCED_MODE_MASK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rustsync::Block;
    use rand::{RngCore, SeedableRng};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Condvar;
    use std::time::{Duration, Instant};

    fn filesystem_info(total_bytes: u64, available_bytes: u64) -> StagingFilesystemInfo {
        StagingFilesystemInfo {
            total_bytes,
            available_bytes,
            available_inodes: 0,
            block_size: 4096,
            cow_clone_supported: false,
        }
    }

    #[test]
    fn staging_budget_applies_reserve_and_limit() {
        assert_eq!(
            StagingPolicy::default().budget(filesystem_info(1_000, 800)),
            StagingBudget {
                reserve_bytes: 50,
                usable_bytes: 750,
                budget_bytes: 750,
                cow_clone_supported: false,
            }
        );
        assert_eq!(
            StagingPolicy {
                limit_bytes: Some(200),
                reserve: StagingReserve::Bytes(300),
            }
            .budget(filesystem_info(2_000, 1_000)),
            StagingBudget {
                reserve_bytes: 300,
                usable_bytes: 700,
                budget_bytes: 200,
                cow_clone_supported: false,
            }
        );
    }

    #[test]
    fn staging_budget_saturates_at_boundaries() {
        assert_eq!(
            StagingPolicy {
                limit_bytes: Some(u64::MAX),
                reserve: StagingReserve::Bytes(u64::MAX),
            }
            .budget(filesystem_info(u64::MAX, u64::MAX - 1)),
            StagingBudget {
                reserve_bytes: u64::MAX,
                usable_bytes: 0,
                budget_bytes: 0,
                cow_clone_supported: false,
            }
        );
        assert_eq!(
            StagingPolicy {
                limit_bytes: None,
                reserve: StagingReserve::BasisPoints(9_999),
            }
            .budget(filesystem_info(u64::MAX, u64::MAX)),
            StagingBudget {
                reserve_bytes: 18_444_899_399_302_180_659,
                usable_bytes: 1_844_674_407_370_956,
                budget_bytes: 1_844_674_407_370_956,
                cow_clone_supported: false,
            }
        );
    }

    #[test]
    fn staging_write_capacity_rounds_and_preserves_exact_boundary() {
        let policy = StagingPolicy {
            limit_bytes: None,
            reserve: StagingReserve::Bytes(4_096),
        };
        let exact = staging_write_capacity(policy, filesystem_info(40_000, 12_288), 4_096);
        assert_eq!(exact.reserve_bytes, 4_096);
        assert_eq!(exact.required_bytes, 8_192);
        assert_eq!(exact.available_above_reserve, exact.required_bytes);

        let rounded = staging_write_capacity(policy, filesystem_info(40_000, 12_289), 4_097);
        assert_eq!(rounded.required_bytes, 12_288);
        assert_eq!(rounded.available_above_reserve, 8_193);
    }

    #[test]
    fn staging_write_capacity_handles_reserve_exhaustion_and_overflow() {
        let exhausted = staging_write_capacity(
            StagingPolicy {
                limit_bytes: None,
                reserve: StagingReserve::Bytes(10_000),
            },
            filesystem_info(20_000, 9_999),
            0,
        );
        assert_eq!(exhausted.available_above_reserve, 0);
        assert_eq!(exhausted.required_bytes, 4_096);

        let overflow = staging_write_capacity(
            StagingPolicy {
                limit_bytes: None,
                reserve: StagingReserve::Bytes(0),
            },
            filesystem_info(u64::MAX, u64::MAX),
            u64::MAX,
        );
        assert_eq!(overflow.required_bytes, u64::MAX);
    }

    #[test]
    fn staging_monitor_reserves_metadata_headroom_and_bounds_credit() {
        let monitor = StagingSpaceMonitor {
            base: PathBuf::from("unused"),
            policy: StagingPolicy {
                limit_bytes: None,
                reserve: StagingReserve::Bytes(4_096),
            },
            state: Arc::new(Mutex::new(StagingSpaceMonitorState::default())),
        };
        let path = Path::new("output");
        let mut state = StagingSpaceMonitorState::default();

        monitor
            .refresh_with_filesystem_locked(path, 0, filesystem_info(100_000, 12_288), &mut state)
            .unwrap();
        assert_eq!(state.block_size, 4_096);
        assert_eq!(state.remaining_credit, 0);

        let error = monitor
            .refresh_with_filesystem_locked(path, 0, filesystem_info(100_000, 8_191), &mut state)
            .unwrap_err();
        assert!(error.to_string().contains("aborted before commit"));

        monitor
            .refresh_with_filesystem_locked(
                path,
                1,
                filesystem_info(200_000_000, 100_000_000),
                &mut state,
            )
            .unwrap();
        assert_eq!(
            state.remaining_credit,
            STAGING_CAPACITY_WINDOW_BYTES - 4_096
        );
    }

    fn staging_budget(target: u64, usable: u64) -> StagingBudget {
        StagingBudget {
            reserve_bytes: 7,
            usable_bytes: usable,
            budget_bytes: target,
            cow_clone_supported: true,
        }
    }

    fn staging_budget_without_cow(target: u64, usable: u64) -> StagingBudget {
        StagingBudget {
            cow_clone_supported: false,
            ..staging_budget(target, usable)
        }
    }

    fn local_file(path: &str, size: u64) -> Action {
        Action::Local(Change::Added(Entry::test_file_with_size(
            PathBuf::from(path),
            size,
            size as u32,
        )))
    }

    fn remote_file(path: &str, size: u64) -> Action {
        Action::Remote(Change::Added(Entry::test_file_with_size(
            PathBuf::from(path),
            size,
            size as u32,
        )))
    }

    fn local_modified_file(path: &str, size: u64) -> Action {
        Action::Local(Change::Modified(
            Entry::test_file_with_size(PathBuf::from(path), size.saturating_sub(1), 1),
            Entry::test_file_with_size(PathBuf::from(path), size, 2),
        ))
    }

    fn remote_modified_file(path: &str, size: u64) -> Action {
        Action::Remote(Change::Modified(
            Entry::test_file_with_size(PathBuf::from(path), size.saturating_sub(1), 1),
            Entry::test_file_with_size(PathBuf::from(path), size, 2),
        ))
    }

    fn removed_file(path: &str, size: u64) -> Action {
        Action::Local(Change::Removed(Entry::test_file_with_size(
            PathBuf::from(path),
            size,
            size as u32,
        )))
    }

    #[test]
    fn staging_waves_pack_asymmetric_directions_and_exact_boundaries() {
        let actions = vec![
            local_file("a", 4),
            remote_file("b", 6),
            remote_file("c", 4),
            local_file("d", 7),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(10, 20), staging_budget(10, 20)).unwrap();

        assert_eq!(plan.waves.len(), 2);
        assert_eq!(plan.waves[0].action_indices, vec![0, 1, 2]);
        assert_eq!(plan.waves[0].local_reconstructed_bytes, 4);
        assert_eq!(plan.waves[0].remote_reconstructed_bytes, 10);
        assert_eq!(plan.waves[1].action_indices, vec![3]);
        assert_eq!(plan.local_reconstructed_bytes, 11);
        assert_eq!(plan.remote_reconstructed_bytes, 10);
    }

    #[test]
    fn staging_waves_allow_one_isolated_oversized_file_within_usable_space() {
        let actions = vec![local_file("a", 4), remote_file("b", 11), local_file("c", 4)];
        let plan =
            plan_staging_waves(&actions, staging_budget(10, 20), staging_budget(10, 12)).unwrap();

        assert_eq!(plan.waves.len(), 3);
        assert_eq!(plan.waves[1].action_indices, vec![1]);
        assert!(!plan.waves[1].local_exceeds_budget);
        assert!(plan.waves[1].remote_exceeds_budget);
        assert_eq!(plan.waves[1].remote_staged_regular_outputs, 1);
    }

    #[test]
    fn staging_waves_reject_oversized_file_beyond_usable_space() {
        let error = plan_staging_waves(
            &[local_file("large", 13)],
            staging_budget(10, 12),
            staging_budget(10, 12),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("local"), "{}", message);
        assert!(message.contains("requires 13"), "{}", message);
        assert!(message.contains("usable after reserving 7"), "{}", message);
    }

    #[test]
    fn staging_waves_allow_oversized_cow_modification_as_an_isolated_wave() {
        let actions = vec![local_file("a", 1), local_modified_file("large", 13)];
        let plan =
            plan_staging_waves(&actions, staging_budget(10, 12), staging_budget(10, 12)).unwrap();

        assert_eq!(plan.waves.len(), 2);
        assert_eq!(plan.waves[1].action_indices, vec![1]);
        assert!(plan.waves[1].local_requires_cow_capacity);
        assert!(!plan.waves[1].remote_requires_cow_capacity);
    }

    #[test]
    fn staging_waves_reject_oversized_modification_without_clone_support() {
        let error = plan_staging_waves(
            &[local_modified_file("large", 13)],
            staging_budget_without_cow(10, 12),
            staging_budget(10, 12),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("only an isolated single COW diff"));
    }

    #[test]
    fn staging_waves_reject_oversized_addition_without_cow_source() {
        let error = plan_staging_waves(
            &[local_file("large", 13)],
            staging_budget(10, 12),
            staging_budget(10, 12),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("only an isolated single COW diff"));
    }

    #[test]
    fn staging_waves_reject_multi_output_subtree_with_one_cow_candidate() {
        let actions = vec![
            Action::Local(Change::Added(Entry::test_dir(PathBuf::from("dir")))),
            local_modified_file("dir/a", 13),
            local_file("dir/b", 1),
        ];
        let error = plan_staging_waves(&actions, staging_budget(10, 12), staging_budget(10, 12))
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("only an isolated single COW diff"));
    }

    #[test]
    fn staging_waves_track_cow_capacity_per_side() {
        let local_actions = vec![
            Action::Local(Change::Added(Entry::test_dir(PathBuf::from("dir")))),
            local_modified_file("dir/a", 13),
            remote_file("dir/b", 2),
        ];
        let local_plan = plan_staging_waves(
            &local_actions,
            staging_budget(10, 12),
            staging_budget(10, 12),
        )
        .unwrap();
        assert!(local_plan.waves[0].local_requires_cow_capacity);
        assert!(!local_plan.waves[0].remote_requires_cow_capacity);

        let remote_actions = vec![
            Action::Remote(Change::Added(Entry::test_dir(PathBuf::from("dir")))),
            local_file("dir/a", 2),
            remote_modified_file("dir/b", 13),
        ];
        let remote_plan = plan_staging_waves(
            &remote_actions,
            staging_budget(10, 12),
            staging_budget(10, 12),
        )
        .unwrap();
        assert!(!remote_plan.waves[0].local_requires_cow_capacity);
        assert!(remote_plan.waves[0].remote_requires_cow_capacity);
    }

    #[test]
    fn staging_waves_reject_oversized_multifile_directory_group() {
        let actions = vec![
            Action::Local(Change::Added(Entry::test_dir(PathBuf::from("dir")))),
            local_file("dir/a", 6),
            local_file("dir/b", 6),
        ];
        let error = plan_staging_waves(&actions, staging_budget(10, 20), staging_budget(10, 20))
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("unsafe local staging dependency group"));
    }

    #[test]
    fn staging_waves_group_directory_add_remove_and_metadata_with_descendants() {
        let old_meta = Entry::test_dir(PathBuf::from("b"));
        let mut new_meta = old_meta.clone();
        new_meta.set_mode(0o40700);
        let actions = vec![
            Action::Local(Change::Added(Entry::test_dir(PathBuf::from("a")))),
            local_file("a/file", 5),
            Action::Local(Change::Modified(old_meta, new_meta)),
            local_file("b/file", 5),
            Action::Local(Change::Removed(Entry::test_dir(PathBuf::from("c")))),
            removed_file("c/file", 5),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(5, 20), staging_budget(5, 20)).unwrap();

        assert_eq!(
            plan.waves
                .iter()
                .map(|wave| wave.action_indices.clone())
                .collect::<Vec<_>>(),
            vec![vec![0, 1], vec![2, 3, 4, 5]]
        );
    }

    #[test]
    fn staging_waves_group_file_to_directory_and_nested_subtrees() {
        let actions = vec![
            Action::Local(Change::Modified(
                Entry::test_file(PathBuf::from("a"), 1),
                Entry::test_dir(PathBuf::from("a")),
            )),
            Action::Local(Change::Added(Entry::test_dir(PathBuf::from("a/b")))),
            local_file("a/b/file", 3),
            local_file("a/sibling", 2),
            local_file("z", 2),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(5, 20), staging_budget(5, 20)).unwrap();
        assert_eq!(plan.waves[0].action_indices, vec![0, 1, 2, 3]);
        assert_eq!(plan.waves[1].action_indices, vec![4]);
    }

    #[test]
    fn staging_waves_inspect_both_identical_directory_forms() {
        let actions = vec![
            Action::Identical(
                Change::Removed(Entry::test_file(PathBuf::from("a"), 1)),
                Change::Removed(Entry::test_dir(PathBuf::from("a"))),
            ),
            removed_file("a/child", 0),
            local_file("z", 2),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(1, 10), staging_budget(1, 10)).unwrap();
        assert_eq!(plan.waves[0].action_indices, vec![0, 1]);
        assert_eq!(plan.waves[1].action_indices, vec![2]);

        let replacement = Action::Identical(
            Change::Removed(Entry::test_file(PathBuf::from("a"), 1)),
            Change::Modified(
                Entry::test_dir(PathBuf::from("a")),
                Entry::test_file(PathBuf::from("a"), 2),
            ),
        );
        assert!(plan_staging_waves(
            &[replacement],
            staging_budget(10, 10),
            staging_budget(10, 10),
        )
        .unwrap_err()
        .to_string()
        .contains("directory-to-nondirectory"));
    }

    #[test]
    fn staging_waves_split_independent_files_in_existing_directory() {
        let actions = vec![local_file("dir/a", 6), local_file("dir/b", 6)];
        let plan =
            plan_staging_waves(&actions, staging_budget(6, 20), staging_budget(6, 20)).unwrap();
        assert_eq!(plan.waves[0].action_indices, vec![0]);
        assert_eq!(plan.waves[1].action_indices, vec![1]);
    }

    #[test]
    fn staging_waves_keep_bidirectional_directory_subtree_together() {
        let actions = vec![
            Action::Remote(Change::Added(Entry::test_dir(PathBuf::from("dir")))),
            local_file("dir/from-remote", 5),
            remote_file("dir/to-remote", 6),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(5, 10), staging_budget(6, 10)).unwrap();
        assert_eq!(plan.waves.len(), 1);
        assert_eq!(plan.waves[0].action_indices, vec![0, 1, 2]);
        assert_eq!(plan.waves[0].local_reconstructed_bytes, 5);
        assert_eq!(plan.waves[0].remote_reconstructed_bytes, 6);
    }

    #[test]
    fn staging_waves_count_only_reconstructed_regular_outputs() {
        let old_file = Entry::test_file_with_size(PathBuf::from("a"), 9, 1);
        let mut metadata_file = old_file.clone();
        metadata_file.set_mode(0o100600);
        let actions = vec![
            Action::Local(Change::Modified(old_file, metadata_file)),
            Action::Remote(Change::Removed(Entry::test_file_with_size(
                PathBuf::from("b"),
                20,
                2,
            ))),
            Action::Identical(
                Change::Added(Entry::test_file_with_size(PathBuf::from("c"), 30, 3)),
                Change::Added(Entry::test_file_with_size(PathBuf::from("c"), 30, 3)),
            ),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(0, 0), staging_budget(0, 0)).unwrap();
        assert_eq!(plan.waves.len(), 1);
        assert_eq!(plan.waves[0].action_indices, vec![0, 1, 2]);
        assert_eq!(plan.local_reconstructed_bytes, 0);
        assert_eq!(plan.remote_reconstructed_bytes, 0);
        assert_eq!(plan.local_staged_regular_outputs, 0);
        assert_eq!(plan.remote_staged_regular_outputs, 0);
    }

    #[test]
    fn staging_waves_use_full_reconstructed_sizes_for_modified_files() {
        let actions = vec![
            Action::Local(Change::Modified(
                Entry::test_file_with_size(PathBuf::from("a"), 4, 1),
                Entry::test_file_with_size(PathBuf::from("a"), 9, 2),
            )),
            Action::Remote(Change::Modified(
                Entry::test_symlink(PathBuf::from("b"), PathBuf::from("target")),
                Entry::test_file_with_size(PathBuf::from("b"), 11, 3),
            )),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(20, 20), staging_budget(20, 20)).unwrap();

        assert_eq!(plan.local_reconstructed_bytes, 9);
        assert_eq!(plan.remote_reconstructed_bytes, 11);
        assert_eq!(plan.local_staged_regular_outputs, 1);
        assert_eq!(plan.remote_staged_regular_outputs, 1);
    }

    #[test]
    fn staging_waves_classify_resolved_directions_and_empty_outputs() {
        let local_change = Change::Added(Entry::test_file_with_size(PathBuf::from("a"), 0, 0));
        let remote_change = Change::Added(Entry::test_file_with_size(PathBuf::from("b"), 7, 7));
        let actions = vec![
            Action::ResolvedLocal((local_change.clone(), local_change.clone()), local_change),
            Action::ResolvedRemote(
                (remote_change.clone(), remote_change.clone()),
                remote_change,
            ),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(7, 7), staging_budget(7, 7)).unwrap();
        assert_eq!(plan.local_staged_regular_outputs, 1);
        assert_eq!(plan.local_reconstructed_bytes, 0);
        assert_eq!(plan.remote_staged_regular_outputs, 1);
        assert_eq!(plan.remote_reconstructed_bytes, 7);
    }

    #[test]
    fn staging_waves_keep_resolved_directory_history_with_descendants() {
        let removed_dir = Change::Removed(Entry::test_dir(PathBuf::from("a")));
        let resolved_file = Change::Added(Entry::test_file_with_size(PathBuf::from("a"), 11, 11));
        let child_removed = Change::Removed(Entry::test_file(PathBuf::from("a/child"), 1));
        let actions = vec![
            Action::ResolvedLocal((removed_dir.clone(), removed_dir), resolved_file),
            Action::Identical(child_removed.clone(), child_removed),
            local_file("z", 1),
        ];
        let plan =
            plan_staging_waves(&actions, staging_budget(10, 20), staging_budget(10, 20)).unwrap();
        assert_eq!(plan.waves[0].action_indices, vec![0, 1]);
        assert!(plan.waves[0].local_exceeds_budget);
        assert_eq!(plan.waves[1].action_indices, vec![2]);

        let directory_to_file = Action::ResolvedRemote(
            (
                Change::Modified(
                    Entry::test_dir(PathBuf::from("a")),
                    Entry::test_file(PathBuf::from("a"), 2),
                ),
                Change::Removed(Entry::test_dir(PathBuf::from("a"))),
            ),
            Change::Added(Entry::test_file(PathBuf::from("a"), 2)),
        );
        assert!(plan_staging_waves(
            &[directory_to_file],
            staging_budget(10, 10),
            staging_budget(10, 10),
        )
        .unwrap_err()
        .to_string()
        .contains("directory-to-nondirectory"));
    }

    #[test]
    fn staging_waves_reject_invalid_action_sequences() {
        let cases = [
            vec![local_file("b", 1), local_file("a", 1)],
            vec![local_file("a", 1), remote_file("a", 1)],
            vec![Action::Conflict(
                Change::Added(Entry::test_file(PathBuf::from("a"), 1)),
                Change::Added(Entry::test_file(PathBuf::from("a"), 2)),
            )],
            vec![Action::Local(Change::Modified(
                Entry::test_dir(PathBuf::from("a")),
                Entry::test_symlink(PathBuf::from("a"), PathBuf::from("target")),
            ))],
        ];
        let expected = [
            "strictly increasing",
            "strictly increasing",
            "unresolved conflict",
            "directory-to-nondirectory",
        ];
        for (actions, expected) in IntoIterator::into_iter(cases).zip(expected) {
            let error = plan_staging_waves(
                &actions,
                staging_budget(u64::MAX, u64::MAX),
                staging_budget(u64::MAX, u64::MAX),
            )
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{}", error);
        }
    }

    #[test]
    fn staging_waves_report_dependency_group_byte_overflow() {
        let actions = vec![
            Action::Local(Change::Added(Entry::test_dir(PathBuf::from("dir")))),
            local_file("dir/a", u64::MAX),
            local_file("dir/b", 1),
        ];
        let error = plan_staging_waves(
            &actions,
            staging_budget(u64::MAX, u64::MAX),
            staging_budget(u64::MAX, u64::MAX),
        )
        .unwrap_err();
        assert!(error.to_string().contains("overflow"), "{}", error);
    }

    #[test]
    fn staging_reserve_rejects_invalid_serialized_percentage() {
        let error = serde_json::from_str::<StagingReserve>(r#"{"BasisPoints":10000}"#).unwrap_err();
        assert!(error.to_string().contains("less than 100%"));

        let reserve = StagingReserve::BasisPoints(725);
        let encoded = bincode::serde::encode_to_vec(reserve, bincode::config::standard()).unwrap();
        let (decoded, consumed): (StagingReserve, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(decoded, reserve);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn staging_filesystem_info_preserves_v1_wire_layout() {
        #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
        struct WireV1 {
            total_bytes: u64,
            available_bytes: u64,
            available_inodes: u64,
            block_size: u64,
            cow_clone_supported: bool,
        }

        let info = StagingFilesystemInfo {
            total_bytes: 100,
            available_bytes: 80,
            available_inodes: u64::MAX,
            block_size: 4096,
            cow_clone_supported: true,
        };
        let encoded = bincode::serde::encode_to_vec(info, bincode::config::standard()).unwrap();
        let (legacy, consumed): (WireV1, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::standard()).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(
            legacy,
            WireV1 {
                total_bytes: 100,
                available_bytes: 80,
                available_inodes: u64::MAX,
                block_size: 4096,
                cow_clone_supported: true,
            }
        );

        let legacy_encoded =
            bincode::serde::encode_to_vec(legacy, bincode::config::standard()).unwrap();
        let (decoded, consumed): (StagingFilesystemInfo, usize) =
            bincode::serde::decode_from_slice(&legacy_encoded, bincode::config::standard())
                .unwrap();
        assert_eq!(consumed, legacy_encoded.len());
        assert_eq!(decoded, info);
    }

    #[test]
    fn staging_filesystem_count_conversion_falls_back_and_saturates() {
        let info = staging_filesystem_info_from_counts(
            u64::MAX,
            u64::MAX,
            100,
            17,
            0,
            4096,
            Path::new("base"),
        )
        .unwrap();
        assert_eq!(info.block_size, 4096);
        assert_eq!(info.total_bytes, u64::MAX);
        assert_eq!(info.available_bytes, u64::MAX);
        assert_eq!(info.available_inodes, 17);
    }

    #[test]
    fn staging_filesystem_count_conversion_marks_inode_less_filesystems_unknown() {
        let info =
            staging_filesystem_info_from_counts(100, 50, 0, 0, 4096, 4096, Path::new("base"))
                .unwrap();

        assert_eq!(info.available_inodes, u64::MAX);
    }

    #[test]
    fn reports_staging_filesystem_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let info = staging_filesystem_info(dir.path()).unwrap();

        assert!(info.block_size > 0);
        assert!(info.total_bytes > 0);
        assert!(info.available_bytes <= info.total_bytes);
        assert!(!info.cow_clone_supported);
    }

    #[test]
    fn reports_staging_capacity_through_symlink_base() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let target_info = staging_filesystem_info(&target).unwrap();
        let link_info = staging_filesystem_info(&link).unwrap();
        assert_eq!(link_info.block_size, target_info.block_size);
        assert_eq!(link_info.total_bytes, target_info.total_bytes);
    }

    fn test_file_entry(path: &str, contents: &[u8]) -> Entry {
        Entry::test_file_with_size(
            PathBuf::from(path),
            contents.len() as u64,
            adler32::adler32(contents).unwrap(),
        )
    }

    fn test_file_entry_with_mode(path: &str, contents: &[u8], mode: u32) -> Entry {
        let mut entry = test_file_entry(path, contents);
        entry.set_mode(mode);
        entry
    }

    #[test]
    fn strong_verification_rejects_adler_collision() {
        let dir = tempfile::tempdir().unwrap();
        let expected = [10, 10, 10, 10];
        let collision = [11, 9, 9, 11];
        assert_eq!(
            adler32::adler32(&expected[..]).unwrap(),
            adler32::adler32(&collision[..]).unwrap()
        );
        std::fs::write(dir.path().join("file"), collision).unwrap();
        let mut entry = test_file_entry("file", &expected);
        entry.set_digest(Some(content_digest(&expected)));

        let error =
            verify_file_matches_entry(&dir.path().join("file"), &entry, "target").unwrap_err();

        assert!(
            error.to_string().contains("strong digest mismatch"),
            "{}",
            error
        );
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & SYNCED_MODE_MASK
    }

    fn marker_staged_paths(state: &Path) -> Vec<PathBuf> {
        fs::read_to_string(apply_attempt_path(state).unwrap())
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("staged-file: "))
            .map(PathBuf::from)
            .collect()
    }

    fn stage_directories(base: &Path) -> Vec<PathBuf> {
        fs::read_dir(base)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".duet-stage-")
            })
            .collect()
    }

    fn synced_existing_file_entry(base: &Path, path: &str, contents: &[u8]) -> Entry {
        let filename = base.join(path);
        fs::write(&filename, contents).unwrap();
        update_meta(&filename, &test_file_entry(path, contents)).unwrap()
    }

    fn synced_existing_dir_entry(base: &Path, path: &str) -> Entry {
        update_meta(&base.join(path), &Entry::test_dir(PathBuf::from(path))).unwrap()
    }

    fn stream_file(
        applier: &mut DetailApplier,
        action_index: usize,
        contents: &[u8],
    ) -> Result<()> {
        applier.apply_frame(DetailFrame {
            action_index: action_index as u32,
            payload: DetailPayload::FileBegin,
        })?;
        applier.apply_frame(DetailFrame {
            action_index: action_index as u32,
            payload: DetailPayload::FileBytes(contents.to_vec()),
        })?;
        applier.apply_frame(DetailFrame {
            action_index: action_index as u32,
            payload: DetailPayload::FileEnd,
        })
    }

    fn prepared_test_output(
        base: &Path,
        staging: &StagingArea,
        entry: &Entry,
        contents: &[u8],
    ) -> PreparedOutput {
        let mut output = TempOutput::new(base.join(entry.path()), staging.shared()).unwrap();
        output.file.as_mut().unwrap().write_all(contents).unwrap();
        output.verify_contents(entry, "test output").unwrap();
        output
            .prepare(
                entry,
                OutputPublication::NoReplace {
                    description: "test target".to_string(),
                },
            )
            .unwrap()
    }

    struct NoopSyncWorker;

    impl OutputSyncWorker for NoopSyncWorker {
        fn sync(&self, _batch_index: usize, _output: &TempOutput) -> io::Result<()> {
            Ok(())
        }
    }

    struct OverlapSyncWorker {
        expected_overlap: usize,
        state: Mutex<(usize, usize)>,
        changed: Condvar,
    }

    impl OutputSyncWorker for OverlapSyncWorker {
        fn sync(&self, _batch_index: usize, _output: &TempOutput) -> io::Result<()> {
            let mut state = self.state.lock().unwrap();
            state.0 += 1;
            state.1 = state.1.max(state.0);
            self.changed.notify_all();
            let deadline = Instant::now() + Duration::from_secs(2);
            while state.1 < self.expected_overlap {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "sync workers did not overlap",
                    ));
                }
                state = self.changed.wait_timeout(state, remaining).unwrap().0;
            }
            state.0 -= 1;
            self.changed.notify_all();
            Ok(())
        }
    }

    struct ReverseCompletionSyncWorker {
        state: Mutex<(bool, Vec<usize>)>,
        changed: Condvar,
    }

    impl OutputSyncWorker for ReverseCompletionSyncWorker {
        fn sync(&self, batch_index: usize, _output: &TempOutput) -> io::Result<()> {
            let mut state = self.state.lock().unwrap();
            if batch_index == 0 {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !state.0 {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "later sync worker did not complete",
                        ));
                    }
                    state = self.changed.wait_timeout(state, remaining).unwrap().0;
                }
            } else if batch_index == 1 {
                state.0 = true;
                self.changed.notify_all();
            }
            state.1.push(batch_index);
            Ok(())
        }
    }

    struct PathFailureSyncWorker;

    impl OutputSyncWorker for PathFailureSyncWorker {
        fn sync(&self, _batch_index: usize, output: &TempOutput) -> io::Result<()> {
            match output.final_path.file_name().and_then(|name| name.to_str()) {
                Some("a.txt") | Some("c.txt") => Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("sync failed for {}", output.final_path.display()),
                )),
                Some("b.txt") | Some("d.txt") => Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("later sync failed for {}", output.final_path.display()),
                )),
                _ => Ok(()),
            }
        }
    }

    struct BatchCountingSyncWorker {
        batches: AtomicUsize,
        files: AtomicUsize,
    }

    struct ParentModeSyncWorker {
        parent: PathBuf,
        expected_mode: u32,
    }

    struct PanicAndErrorSyncWorker;

    impl OutputSyncWorker for PanicAndErrorSyncWorker {
        fn sync(&self, batch_index: usize, _output: &TempOutput) -> io::Result<()> {
            if batch_index == 0 {
                panic!("injected first worker panic");
            }
            Err(io::Error::new(
                io::ErrorKind::Other,
                "injected later worker error",
            ))
        }
    }

    impl OutputSyncWorker for ParentModeSyncWorker {
        fn sync(&self, _batch_index: usize, _output: &TempOutput) -> io::Result<()> {
            let actual = fs::symlink_metadata(&self.parent)?.mode() & SYNCED_MODE_MASK;
            if actual != self.expected_mode {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "destination parent mode changed during sync: expected {:04o}, got {:04o}",
                        self.expected_mode, actual
                    ),
                ));
            }
            Ok(())
        }
    }

    impl OutputSyncWorker for BatchCountingSyncWorker {
        fn sync(&self, batch_index: usize, _output: &TempOutput) -> io::Result<()> {
            if batch_index == 0 {
                self.batches.fetch_add(1, AtomicOrdering::Relaxed);
            }
            self.files.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn output_batch_config_validates_env_overrides_and_caps_resources() {
        let baseline = OutputBatchConfig {
            max_files: 12,
            max_bytes: 34,
            workers: 5,
        };
        let invalid = baseline.with_env_overrides_from(|_| Some("invalid".to_string()));
        assert_eq!(invalid, baseline);

        let capped = baseline.with_env_overrides_from(|name| match name {
            ENV_OUTPUT_BATCH_FILES => Some(usize::MAX.to_string()),
            ENV_OUTPUT_BATCH_BYTES => Some(u64::MAX.to_string()),
            ENV_OUTPUT_SYNC_WORKERS => Some("0".to_string()),
            _ => None,
        });
        assert_eq!(capped.max_files, output_batch_file_limit());
        assert_eq!(capped.max_bytes, MAX_OUTPUT_BATCH_BYTES);
        assert_eq!(capped.workers, 1);

        let low_fd_limit = OutputBatchConfig {
            max_files: usize::MAX,
            max_bytes: 1,
            workers: 1,
        }
        .normalized_with_file_limit(17);
        assert_eq!(low_fd_limit.max_files, 17);
        assert_eq!(
            OutputBatchConfig {
                max_files: usize::MAX,
                max_bytes: 1,
                workers: 1,
            }
            .normalized_with_file_limit(usize::MAX)
            .max_files,
            MAX_OUTPUT_BATCH_FILES
        );

        let host = OutputBatchConfig::default_for_host().normalized();
        assert!((1..=MAX_OUTPUT_SYNC_WORKERS).contains(&host.workers));
        assert!(host.max_files <= output_batch_file_limit());
    }

    #[test]
    fn publication_failure_records_each_prior_publication_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state_path = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        fs::write(&state_path, b"old snapshot").unwrap();
        let entries = [
            test_file_entry("a.txt", b"a"),
            test_file_entry("b.txt", b"b"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        start_apply_attempt("local", &state_path, &base, &actions, None).unwrap();
        let staging = StagingArea::new(&base).unwrap();
        let mut batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 2,
                max_bytes: 1024,
                workers: 2,
            },
            Arc::new(NoopSyncWorker),
        );
        batch.push(0, prepared_test_output(&base, &staging, &entries[0], b"a"));
        batch.push(1, prepared_test_output(&base, &staging, &entries[1], b"b"));
        fs::write(base.join("b.txt"), b"racing destination").unwrap();
        let mut recorder = ApplyRecorder::new(Some(state_path.clone()));
        let mut new_entries = Vec::new();

        let error = batch
            .flush(&actions, &mut recorder, &mut new_entries)
            .unwrap_err();

        assert!(error.to_string().contains("already exists"), "{}", error);
        assert_eq!(fs::read(base.join("a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(base.join("b.txt")).unwrap(), b"racing destination");
        assert_eq!(new_entries.len(), 1);
        assert_eq!(new_entries[0].path(), Path::new("a.txt"));
        assert_eq!(fs::read(&state_path).unwrap(), b"old snapshot");
        let marker = fs::read_to_string(apply_attempt_path(&state_path).unwrap()).unwrap();
        assert!(marker.contains("committed-step: rename-file a.txt"));
        assert!(marker.contains("committed-step: update-metadata a.txt"));
        assert!(marker.contains("committed-operation: add-file a.txt"));
        assert!(!marker.contains("committed-operation: add-file b.txt"));
        staging.finish(&HashSet::new()).unwrap();
    }

    #[test]
    fn streamed_validation_failure_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state_path = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        fs::write(&state_path, b"old snapshot").unwrap();
        fs::write(base.join("b.txt"), b"racing destination").unwrap();
        let entries = [
            test_file_entry("a.txt", b"a"),
            test_file_entry("b.txt", b"b"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        start_apply_attempt("local", &state_path, &base, &actions, None).unwrap();
        let mut applier = DetailApplier::new_with_attempt(
            base.clone(),
            actions,
            Vec::new(),
            Some(state_path.clone()),
        );
        applier.output_batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 2,
                max_bytes: 1024,
                workers: 2,
            },
            Arc::new(NoopSyncWorker),
        );

        stream_file(&mut applier, 0, b"a").unwrap();
        stream_file(&mut applier, 1, b"b").unwrap();
        let publication_error = applier.finish().unwrap_err();
        assert!(
            publication_error
                .to_string()
                .contains("appeared after staged preparation"),
            "{}",
            publication_error
        );
        assert!(!base.join("a.txt").exists());
        assert_eq!(fs::read(base.join("b.txt")).unwrap(), b"racing destination");
        assert_eq!(fs::read(&state_path).unwrap(), b"old snapshot");
        let marker = fs::read_to_string(apply_attempt_path(&state_path).unwrap()).unwrap();
        assert!(!marker.contains("committed-operation: add-file a.txt"));
        assert!(!marker.contains("committed-operation: add-file b.txt"));
    }

    #[test]
    fn post_publication_failure_records_commit_and_poisons_stream() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state_path = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        fs::write(&state_path, b"old snapshot").unwrap();
        let entry = test_file_entry("a.txt", b"a");
        let actions = vec![Action::Local(Change::Added(entry))];
        start_apply_attempt("local", &state_path, &base, &actions, None).unwrap();
        let mut applier = DetailApplier::new_with_attempt(
            base.clone(),
            actions,
            Vec::new(),
            Some(state_path.clone()),
        );
        let mut batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 1,
                max_bytes: 1024,
                workers: 1,
            },
            Arc::new(NoopSyncWorker),
        );
        batch.post_commit_hook = Some(Arc::new(|_| {
            Err(eyre!("injected post-publication failure"))
        }));
        applier.output_batch = batch;

        stream_file(&mut applier, 0, b"a").unwrap();
        let error = applier.finish().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("injected post-publication failure"),
            "{}",
            error
        );
        assert_eq!(fs::read(base.join("a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(&state_path).unwrap(), b"old snapshot");
        let marker = fs::read_to_string(apply_attempt_path(&state_path).unwrap()).unwrap();
        assert!(marker.contains("committed-step: rename-file a.txt"));
        assert!(marker.contains("committed-step: update-metadata a.txt"));
        assert!(marker.contains("committed-operation: add-file a.txt"));
    }

    #[test]
    fn replacement_parent_is_rejected_before_publication_widening() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let parent = base.join("parent");
        let moved_parent = base.join("moved-parent");
        fs::create_dir_all(&parent).unwrap();
        let entry = test_file_entry("parent/a.txt", b"a");
        let actions = vec![Action::Local(Change::Added(entry.clone()))];
        let staging = StagingArea::new(&base).unwrap();
        let mut batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 1,
                max_bytes: 1024,
                workers: 1,
            },
            Arc::new(NoopSyncWorker),
        );
        batch.push(0, prepared_test_output(&base, &staging, &entry, b"a"));
        fs::rename(&parent, &moved_parent).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();

        let error = batch
            .flush(&actions, &mut ApplyRecorder::new(None), &mut Vec::new())
            .unwrap_err();

        assert!(
            error.to_string().contains("recorded directory"),
            "{}",
            error
        );
        assert_eq!(mode(&parent), 0o500);
        assert!(!parent.join("a.txt").exists());
        assert!(!moved_parent.join("a.txt").exists());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        staging.finish(&HashSet::new()).unwrap();
    }

    #[test]
    fn worker_panics_are_indexed_errors_and_publish_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingArea::new(dir.path()).unwrap();
        let entries = [
            test_file_entry("a.txt", b"a"),
            test_file_entry("b.txt", b"b"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        let mut batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 2,
                max_bytes: 1024,
                workers: 2,
            },
            Arc::new(PanicAndErrorSyncWorker),
        );
        batch.push(
            0,
            prepared_test_output(dir.path(), &staging, &entries[0], b"a"),
        );
        batch.push(
            1,
            prepared_test_output(dir.path(), &staging, &entries[1], b"b"),
        );

        let error = batch
            .flush(&actions, &mut ApplyRecorder::new(None), &mut Vec::new())
            .unwrap_err();
        let error_chain = format!("{:#}", error);

        assert!(error.to_string().contains("a.txt"), "{}", error);
        assert!(
            error_chain.contains("sync worker panicked"),
            "{}",
            error_chain
        );
        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("b.txt").exists());
        staging.finish(&HashSet::new()).unwrap();
    }

    #[test]
    fn pending_batches_do_not_widen_restrictive_destination_parents() {
        for requested_mode in [0o000, 0o300] {
            for fail_second_publication in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let base = dir.path().join("base");
                let parent = base.join("parent");
                fs::create_dir_all(&parent).unwrap();
                if fail_second_publication {
                    fs::write(parent.join("b.txt"), b"racing destination").unwrap();
                }
                fs::set_permissions(&parent, fs::Permissions::from_mode(requested_mode)).unwrap();
                let entries = [
                    test_file_entry("parent/a.txt", b"a"),
                    test_file_entry("parent/b.txt", b"b"),
                ];
                let actions = entries
                    .iter()
                    .cloned()
                    .map(|entry| Action::Local(Change::Added(entry)))
                    .collect::<Vec<_>>();
                let staging = StagingArea::new(&base).unwrap();
                let mut batch = FilePublicationBatch::with_worker(
                    OutputBatchConfig {
                        max_files: 2,
                        max_bytes: 1024,
                        workers: 2,
                    },
                    Arc::new(ParentModeSyncWorker {
                        parent: parent.clone(),
                        expected_mode: requested_mode,
                    }),
                );
                batch.push(0, prepared_test_output(&base, &staging, &entries[0], b"a"));
                batch.push(1, prepared_test_output(&base, &staging, &entries[1], b"b"));

                assert_eq!(mode(&parent), requested_mode);
                let result = batch.flush(&actions, &mut ApplyRecorder::new(None), &mut Vec::new());
                let bootstrap_unsupported = requested_mode == 0o000
                    && !cfg!(any(target_os = "linux", target_os = "android"));
                assert_eq!(
                    result.is_err(),
                    fail_second_publication || bootstrap_unsupported,
                    "mode {:04o}, fail_second_publication {}: {:?}",
                    requested_mode,
                    fail_second_publication,
                    result
                );
                assert_eq!(mode(&parent), requested_mode);

                fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
                if bootstrap_unsupported {
                    assert!(!parent.join("a.txt").exists());
                } else {
                    assert_eq!(fs::read(parent.join("a.txt")).unwrap(), b"a");
                }
                if fail_second_publication {
                    assert_eq!(
                        fs::read(parent.join("b.txt")).unwrap(),
                        b"racing destination"
                    );
                } else if bootstrap_unsupported {
                    assert!(!parent.join("b.txt").exists());
                } else {
                    assert_eq!(fs::read(parent.join("b.txt")).unwrap(), b"b");
                }
                staging.finish(&HashSet::new()).unwrap();
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn mode_independent_directory_handle_tracks_inode_across_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let parent_path = dir.path().join("parent");
        let moved_path = dir.path().join("moved-parent");
        fs::create_dir(&parent_path).unwrap();
        fs::set_permissions(&parent_path, fs::Permissions::from_mode(0o000)).unwrap();
        let containing = open_directory_for_access(dir.path()).unwrap();
        let name = path_component_cstring(parent_path.file_name().unwrap(), "test parent").unwrap();
        let retained = open_permission_independent_directory_at(&containing, &name).unwrap();
        let expected = directory_identity(&retained, &parent_path, "test parent").unwrap();
        fs::rename(&parent_path, &moved_path).unwrap();
        fs::create_dir(&parent_path).unwrap();
        fs::set_permissions(&parent_path, fs::Permissions::from_mode(0o555)).unwrap();

        set_retained_directory_mode(&retained, 0o700, &moved_path).unwrap();

        assert_eq!(mode(&moved_path), 0o700);
        assert_eq!(mode(&parent_path), 0o555);
        assert_eq!(
            directory_identity(&retained, &moved_path, "test parent").unwrap(),
            expected
        );
        set_retained_directory_mode(&retained, 0o000, &moved_path).unwrap();
        assert_eq!(mode(&moved_path), 0o000);
        assert_eq!(mode(&parent_path), 0o555);
        fs::set_permissions(&moved_path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn fallback_many_file_outputs_use_multiple_bounded_batches() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let file_count = 70;
        let entries = (0..file_count)
            .map(|index| test_file_entry(&format!("file-{:03}.txt", index), b"x"))
            .collect::<Vec<_>>();
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        let details = (0..file_count)
            .map(|_| ChangeDetails::Contents(b"x".to_vec()))
            .collect::<Vec<_>>();
        let worker = Arc::new(BatchCountingSyncWorker {
            batches: AtomicUsize::new(0),
            files: AtomicUsize::new(0),
        });
        let batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 16,
                max_bytes: 1024,
                workers: 4,
            },
            worker.clone(),
        );
        let mut all_old = Vec::new();

        apply_detailed_changes_with_output_batch(
            &base,
            &actions,
            &details,
            &mut all_old,
            None,
            None,
            ApplyOptions::default(),
            batch,
        )
        .unwrap();

        assert_eq!(worker.files.load(AtomicOrdering::Relaxed), file_count);
        assert_eq!(worker.batches.load(AtomicOrdering::Relaxed), 5);
        assert_eq!(all_old.len(), file_count);
        for entry in entries {
            assert_eq!(fs::read(base.join(entry.path())).unwrap(), b"x");
        }
    }

    #[test]
    fn fallback_error_drops_pending_outputs_before_staging_area() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let entries = [
            test_file_entry("a.txt", b"a"),
            test_file_entry("b.txt", b"b"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        let details = vec![
            ChangeDetails::Contents(b"a".to_vec()),
            ChangeDetails::Contents(b"wrong size".to_vec()),
        ];
        let batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 10,
                max_bytes: 1024,
                workers: 2,
            },
            Arc::new(NoopSyncWorker),
        );

        let error = apply_detailed_changes_with_output_batch(
            &base,
            &actions,
            &details,
            &mut Vec::new(),
            None,
            None,
            ApplyOptions::default(),
            batch,
        )
        .unwrap_err();

        assert!(error.to_string().contains("size mismatch"), "{}", error);
        assert!(!base.join("a.txt").exists());
        assert!(!base.join("b.txt").exists());
        assert!(fs::read_dir(&base).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".duet-stage-")));
    }

    #[test]
    fn fallback_publication_race_records_prior_file_without_advancing_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state_path = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        fs::write(&state_path, b"old snapshot").unwrap();
        fs::write(base.join("b.txt"), b"racing destination").unwrap();
        let entries = [
            test_file_entry("a.txt", b"a"),
            test_file_entry("b.txt", b"b"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        let details = vec![
            ChangeDetails::Contents(b"a".to_vec()),
            ChangeDetails::Contents(b"b".to_vec()),
        ];
        start_apply_attempt("local", &state_path, &base, &actions, None).unwrap();
        let batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 2,
                max_bytes: 1024,
                workers: 2,
            },
            Arc::new(NoopSyncWorker),
        );
        let mut all_old = Vec::new();

        let error = apply_detailed_changes_with_output_batch(
            &base,
            &actions,
            &details,
            &mut all_old,
            Some(&state_path),
            None,
            ApplyOptions::default(),
            batch,
        )
        .unwrap_err();

        assert!(error.to_string().contains("already exists"), "{}", error);
        assert_eq!(fs::read(base.join("a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(base.join("b.txt")).unwrap(), b"racing destination");
        assert!(all_old.is_empty());
        assert_eq!(fs::read(&state_path).unwrap(), b"old snapshot");
        let marker = fs::read_to_string(apply_attempt_path(&state_path).unwrap()).unwrap();
        assert!(marker.contains("committed-operation: add-file a.txt"));
        assert!(!marker.contains("committed-operation: add-file b.txt"));
    }

    #[test]
    fn output_sync_workers_overlap_and_respect_the_worker_bound() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingArea::new(dir.path()).unwrap();
        let entries = (0..4)
            .map(|index| test_file_entry(&format!("{}.txt", index), b"x"))
            .collect::<Vec<_>>();
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        let worker = Arc::new(OverlapSyncWorker {
            expected_overlap: 2,
            state: Mutex::new((0, 0)),
            changed: Condvar::new(),
        });
        let mut batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 4,
                max_bytes: 1024,
                workers: 2,
            },
            worker.clone(),
        );
        for (index, entry) in entries.iter().enumerate() {
            batch.push(
                index,
                prepared_test_output(dir.path(), &staging, entry, b"x"),
            );
        }

        batch
            .flush(&actions, &mut ApplyRecorder::new(None), &mut Vec::new())
            .unwrap();

        assert_eq!(worker.state.lock().unwrap().1, 2);
        staging.finish(&HashSet::new()).unwrap();
    }

    #[test]
    fn output_batches_flush_at_count_and_byte_thresholds() {
        let count_dir = tempfile::tempdir().unwrap();
        let count_entries = [
            test_file_entry("a.txt", b"aaa"),
            test_file_entry("b.txt", b"bbb"),
        ];
        let count_actions = count_entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect();
        let mut count_applier = DetailApplier::new_with_attempt(
            count_dir.path().to_path_buf(),
            count_actions,
            Vec::new(),
            None,
        );
        count_applier.output_batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 2,
                max_bytes: 1024,
                workers: 1,
            },
            Arc::new(NoopSyncWorker),
        );
        stream_file(&mut count_applier, 0, b"aaa").unwrap();
        assert!(!count_dir.path().join("a.txt").exists());
        stream_file(&mut count_applier, 1, b"bbb").unwrap();
        assert!(!count_dir.path().join("a.txt").exists());
        assert!(!count_dir.path().join("b.txt").exists());
        count_applier.finish().unwrap();
        assert!(count_dir.path().join("a.txt").exists());
        assert!(count_dir.path().join("b.txt").exists());

        let byte_dir = tempfile::tempdir().unwrap();
        let byte_actions = count_entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect();
        let mut byte_applier = DetailApplier::new_with_attempt(
            byte_dir.path().to_path_buf(),
            byte_actions,
            Vec::new(),
            None,
        );
        byte_applier.output_batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 10,
                max_bytes: 5,
                workers: 1,
            },
            Arc::new(NoopSyncWorker),
        );
        stream_file(&mut byte_applier, 0, b"aaa").unwrap();
        byte_applier
            .apply_frame(DetailFrame {
                action_index: 1,
                payload: DetailPayload::FileBegin,
            })
            .unwrap();
        assert!(!byte_dir.path().join("a.txt").exists());
        assert!(!byte_dir.path().join("b.txt").exists());
        byte_applier
            .apply_frame(DetailFrame {
                action_index: 1,
                payload: DetailPayload::FileBytes(b"bbb".to_vec()),
            })
            .unwrap();
        byte_applier
            .apply_frame(DetailFrame {
                action_index: 1,
                payload: DetailPayload::FileEnd,
            })
            .unwrap();
        assert!(!byte_dir.path().join("b.txt").exists());
        byte_applier.finish().unwrap();
        assert!(byte_dir.path().join("b.txt").exists());
    }

    #[test]
    fn output_publication_and_recovery_records_follow_action_order() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state_path = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let entries = [
            test_file_entry("a.txt", b"a"),
            test_file_entry("b.txt", b"b"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        start_apply_attempt("local", &state_path, &base, &actions, None).unwrap();
        let worker = Arc::new(ReverseCompletionSyncWorker {
            state: Mutex::new((false, Vec::new())),
            changed: Condvar::new(),
        });
        let mut applier = DetailApplier::new_with_attempt(
            base.clone(),
            actions,
            Vec::new(),
            Some(state_path.clone()),
        );
        applier.output_batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 2,
                max_bytes: 1024,
                workers: 2,
            },
            worker.clone(),
        );

        stream_file(&mut applier, 0, b"a").unwrap();
        stream_file(&mut applier, 1, b"b").unwrap();

        assert_eq!(worker.state.lock().unwrap().1, vec![1, 0]);
        let marker = fs::read_to_string(apply_attempt_path(&state_path).unwrap()).unwrap();
        let committed = marker
            .lines()
            .filter(|line| line.starts_with("committed-operation: "))
            .collect::<Vec<_>>();
        assert!(committed.is_empty());
        assert!(applier.new_entries.is_empty());
        let entries = applier.finish().unwrap();
        assert_eq!(entries[0].path(), Path::new("a.txt"));
        assert_eq!(entries[1].path(), Path::new("b.txt"));
        let marker = fs::read_to_string(apply_attempt_path(&state_path).unwrap()).unwrap();
        let committed = marker
            .lines()
            .filter(|line| line.starts_with("committed-operation: "))
            .collect::<Vec<_>>();
        assert_eq!(
            committed,
            vec![
                "committed-operation: add-file a.txt",
                "committed-operation: add-file b.txt",
            ]
        );
    }

    #[test]
    fn sync_failure_publishes_nothing_and_reports_earliest_batch_item() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state_path = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        fs::write(&state_path, b"old snapshot").unwrap();
        fs::write(base.join("a.txt"), b"unchanged-a").unwrap();
        fs::write(base.join("b.txt"), b"unchanged-b").unwrap();
        let entries = [
            test_file_entry("a.txt", b"new-a"),
            test_file_entry("b.txt", b"new-b"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        start_apply_attempt("local", &state_path, &base, &actions, None).unwrap();
        let mut applier = DetailApplier::new_with_attempt(
            base.clone(),
            actions,
            Vec::new(),
            Some(state_path.clone()),
        );
        applier.output_batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 2,
                max_bytes: 1024,
                workers: 2,
            },
            Arc::new(PathFailureSyncWorker),
        );

        stream_file(&mut applier, 0, b"new-a").unwrap();
        let error = stream_file(&mut applier, 1, b"new-b").unwrap_err();

        assert!(error.to_string().contains("a.txt"), "{}", error);
        assert_eq!(fs::read(base.join("a.txt")).unwrap(), b"unchanged-a");
        assert_eq!(fs::read(base.join("b.txt")).unwrap(), b"unchanged-b");
        assert!(applier.new_entries.is_empty());
        assert_eq!(fs::read(&state_path).unwrap(), b"old snapshot");
        let marker_path = apply_attempt_path(&state_path).unwrap();
        assert!(marker_path.exists());
        let marker = fs::read_to_string(marker_path).unwrap();
        assert!(!marker.contains("committed-operation:"), "{}", marker);
    }

    #[test]
    fn later_batch_sync_failure_keeps_prior_publications_without_advancing_state() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state_path = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        fs::write(&state_path, b"old snapshot").unwrap();
        let entries = [
            test_file_entry("first.txt", b"first"),
            test_file_entry("second.txt", b"second"),
            test_file_entry("c.txt", b"third"),
            test_file_entry("d.txt", b"fourth"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        start_apply_attempt("local", &state_path, &base, &actions, None).unwrap();
        let mut applier = DetailApplier::new_with_attempt(
            base.clone(),
            actions,
            Vec::new(),
            Some(state_path.clone()),
        );
        applier.output_batch = FilePublicationBatch::with_worker(
            OutputBatchConfig {
                max_files: 2,
                max_bytes: 1024,
                workers: 2,
            },
            Arc::new(PathFailureSyncWorker),
        );

        stream_file(&mut applier, 0, b"first").unwrap();
        stream_file(&mut applier, 1, b"second").unwrap();
        assert!(!base.join("first.txt").exists());
        assert!(!base.join("second.txt").exists());
        stream_file(&mut applier, 2, b"third").unwrap();
        let error = stream_file(&mut applier, 3, b"fourth").unwrap_err();

        assert!(error.to_string().contains("c.txt"), "{}", error);
        assert!(!base.join("c.txt").exists());
        assert!(!base.join("d.txt").exists());
        assert_eq!(fs::read(&state_path).unwrap(), b"old snapshot");
        let marker_path = apply_attempt_path(&state_path).unwrap();
        assert!(marker_path.exists());
        let marker = fs::read_to_string(marker_path).unwrap();
        assert!(!marker.contains("committed-operation: add-file first.txt"));
        assert!(!marker.contains("committed-operation: add-file second.txt"));
        assert!(!marker.contains("committed-operation: add-file c.txt"));
    }

    #[test]
    fn metadata_update_syncs_targets_with_restrictive_modes() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file");
        fs::write(&file_path, b"contents").unwrap();
        let file_entry = test_file_entry_with_mode("file", b"contents", 0o000);
        update_meta(&file_path, &file_entry).unwrap();
        assert_eq!(mode(&file_path), 0o000);

        let directory_path = dir.path().join("directory");
        fs::create_dir(&directory_path).unwrap();
        let mut directory_entry = Entry::test_dir(PathBuf::from("directory"));
        directory_entry.set_mode(0o000);
        update_meta(&directory_path, &directory_entry).unwrap();
        assert_eq!(mode(&directory_path), 0o000);

        let unreadable_path = dir.path().join("unreadable");
        fs::write(&unreadable_path, b"contents").unwrap();
        fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable_entry = test_file_entry_with_mode("unreadable", b"contents", 0o400);
        update_meta(&unreadable_path, &unreadable_entry).unwrap();
        assert_eq!(mode(&unreadable_path), 0o400);
    }

    #[test]
    fn apply_phase_does_not_reopen_metadata_synced_restrictive_directory() {
        let base = tempfile::tempdir().unwrap();
        let directory_path = base.path().join("directory");
        fs::create_dir(&directory_path).unwrap();
        fs::write(directory_path.join("child"), b"contents").unwrap();
        let mut old_directory = Entry::test_dir(PathBuf::from("directory"));
        old_directory.set_mode(0o700);
        let mut new_directory = old_directory.clone();
        new_directory.set_mode(0o300);
        update_meta(&directory_path, &new_directory).unwrap();
        let child = test_file_entry("directory/child", b"contents");
        let actions = vec![
            Action::Local(Change::Modified(old_directory, new_directory)),
            Action::Local(Change::Added(child)),
        ];

        complete_apply_phase(base.path(), &actions, None, &HashSet::new()).unwrap();

        assert_eq!(mode(&directory_path), 0o300);
        fs::set_permissions(&directory_path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn temp_output_is_private_and_readable_through_its_handle() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingArea::new(dir.path()).unwrap();
        let mut output = TempOutput::new(dir.path().join("out.txt"), staging.shared()).unwrap();

        assert_eq!(mode(output.temp_path().parent().unwrap()) & 0o777, 0o700);
        assert_eq!(mode(output.temp_path()), 0o600);
        let file = output.file.as_mut().unwrap();
        file.write_all(b"private").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();

        assert_eq!(contents, b"private");
    }

    #[test]
    fn temp_output_stays_hidden_after_final_mode_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        let staging = StagingArea::new(dir.path()).unwrap();
        let mut output = TempOutput::new(final_path.clone(), staging.shared()).unwrap();
        let stage_dir = output.temp_path().parent().unwrap().to_path_buf();
        output.file.as_mut().unwrap().write_all(b"public").unwrap();
        let entry = test_file_entry_with_mode("out.txt", b"public", 0o644);

        output.prepare_metadata(&entry).unwrap();

        assert_eq!(mode(output.temp_path()), 0o644);
        assert_eq!(mode(&stage_dir) & 0o777, 0o700);
        output.finish(&entry).unwrap();
        assert_eq!(mode(&final_path), 0o644);
        assert!(stage_dir.exists());
        staging.finish(&HashSet::new()).unwrap();
        assert!(!stage_dir.exists());
    }

    #[test]
    fn temp_output_drop_removes_only_its_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let staging = StagingArea::new(dir.path()).unwrap();
        let stage_dir = staging.path().to_path_buf();
        let first = TempOutput::new(dir.path().join("first.txt"), staging.shared()).unwrap();
        let first_path = first.temp_path().to_path_buf();
        let second = TempOutput::new(dir.path().join("second.txt"), staging.shared()).unwrap();
        let second_path = second.temp_path().to_path_buf();
        assert_ne!(first_path, second_path);

        drop(first);

        assert!(stage_dir.exists());
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second);
        assert!(stage_dir.exists());
        staging.finish(&HashSet::new()).unwrap();
        assert!(!stage_dir.exists());
    }

    #[test]
    fn temp_output_create_new_does_not_clobber_destination() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        fs::write(&final_path, b"existing").unwrap();
        let staging = StagingArea::new(dir.path()).unwrap();
        let mut output = TempOutput::new(final_path.clone(), staging.shared()).unwrap();
        let stage_dir = output.stage_path().to_path_buf();
        output.file.as_mut().unwrap().write_all(b"new").unwrap();
        let entry = test_file_entry("out.txt", b"new");

        let error = output
            .finish_without_replacing("rename target", &entry)
            .unwrap_err();

        assert!(error.to_string().contains("already exists"), "{}", error);
        assert_eq!(fs::read(&final_path).unwrap(), b"existing");
        assert!(stage_dir.exists());
        staging.finish(&HashSet::new()).unwrap();
        assert!(!stage_dir.exists());
    }

    #[test]
    fn temp_output_verifies_mode_zero_final_inode_relative_to_parent() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        let staging = StagingArea::new(dir.path()).unwrap();
        let mut output = TempOutput::new(final_path.clone(), staging.shared()).unwrap();
        output.file.as_mut().unwrap().write_all(b"private").unwrap();
        let entry = test_file_entry_with_mode("out.txt", b"private", 0o000);

        output.finish(&entry).unwrap();
        staging.finish(&HashSet::new()).unwrap();

        assert_eq!(mode(&final_path), 0o000);
    }

    #[test]
    fn temp_output_supports_writable_unreadable_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).unwrap();

        let result = (|| -> Result<()> {
            let staging = StagingArea::new(dir.path())?;
            let final_path = parent.join("out.txt");
            let mut output = TempOutput::new(final_path.clone(), staging.shared())?;
            output.file.as_mut().unwrap().write_all(b"contents")?;
            output.finish(&test_file_entry("out.txt", b"contents"))?;
            staging.finish(&HashSet::new())?;
            assert_eq!(fs::read(final_path)?, b"contents");
            Ok(())
        })();

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
    }

    #[test]
    fn stage_mode_normalization_is_descriptor_relative() {
        let dir = tempfile::tempdir().unwrap();
        let parent = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.path())
            .unwrap();
        let name = std::ffi::CString::new("stage").unwrap();
        cvt(unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o000) }).unwrap();
        let created = fstatat_nofollow(parent.as_raw_fd(), &name).unwrap();
        let retained =
            open_new_stage_for_access(&parent, &name, &created, &dir.path().join("stage")).unwrap();

        normalize_stage_directory_mode(
            &parent,
            &name,
            &retained,
            &created,
            &dir.path().join("stage"),
        )
        .unwrap();

        assert_eq!(mode(&dir.path().join("stage")) & 0o7777, 0o700);
    }

    #[test]
    fn stage_parent_permissions_are_restored_immediately_after_creation() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();

        let staging = StagingArea::new(&parent).unwrap();

        assert_eq!(mode(&parent), 0o500);
        assert!(staging.path().exists());
        staging.finish(&HashSet::new()).unwrap();
        assert_eq!(mode(&parent), 0o500);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn stage_drop_cleans_up_under_restrictive_parent_mode() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        fs::create_dir(&parent).unwrap();
        let staging = StagingArea::new(&parent).unwrap();
        let stage_path = staging.path().to_path_buf();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();

        drop(staging);

        assert_eq!(mode(&parent), 0o000);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!stage_path.exists());
    }

    #[test]
    fn complete_apply_phase_syncs_existing_writable_unreadable_parent() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("child"), b"contents").unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).unwrap();
        let actions = vec![Action::Local(Change::Added(test_file_entry(
            "parent/child",
            b"contents",
        )))];

        let result = complete_apply_phase(base.path(), &actions, None, &HashSet::new());

        assert_eq!(mode(&parent), 0o300);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
    }

    #[test]
    fn complete_apply_phase_skips_already_synced_destination_parent() {
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
        let actions = vec![Action::Local(Change::Added(test_file_entry(
            "parent/child",
            b"contents",
        )))];
        let already_synced = HashSet::from([parent.clone()]);

        let result = complete_apply_phase(base.path(), &actions, None, &already_synced);

        assert_eq!(mode(&parent), 0o000);
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
    }

    #[test]
    fn parent_path_swap_after_staging_cannot_report_success_and_keeps_marker() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let parent = base.join("parent");
        let moved_parent = base.join("moved-parent");
        let state = dir.path().join("profile.snp");
        fs::create_dir_all(&parent).unwrap();
        let entry = test_file_entry("parent/out.txt", b"contents");
        let actions = vec![Action::Local(Change::Added(entry.clone()))];
        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        let mut applier =
            DetailApplier::new_with_attempt(base.clone(), actions, Vec::new(), Some(state.clone()));
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBytes(b"contents".to_vec()),
            })
            .unwrap();
        fs::rename(&parent, &moved_parent).unwrap();
        fs::create_dir(&parent).unwrap();

        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileEnd,
            })
            .unwrap();
        let error = applier.finish().unwrap_err();

        assert!(
            error.to_string().contains("output parent directory path"),
            "{}",
            error
        );
        assert!(!parent.join("out.txt").exists());
        assert!(!moved_parent.join("out.txt").exists());
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        assert!(marker.contains("staged-file: "), "{}", marker);
        assert!(
            !marker.contains("committed-step: rename-file"),
            "{}",
            marker
        );
    }

    #[test]
    fn published_parent_swap_before_phase_finish_keeps_marker() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let stable_parent = base.join("stable");
        let swapped_parent = base.join("swapped");
        let moved_parent = base.join("moved-swapped");
        let state = dir.path().join("profile.snp");
        fs::create_dir_all(&stable_parent).unwrap();
        fs::create_dir(&swapped_parent).unwrap();
        let entries = [
            test_file_entry("stable/first.txt", b"first"),
            test_file_entry("swapped/second.txt", b"second"),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        let mut applier =
            DetailApplier::new_with_attempt(base.clone(), actions, Vec::new(), Some(state.clone()));
        applier.output_batch.config.max_files = 1;
        for (index, contents) in [b"first".as_slice(), b"second".as_slice()]
            .iter()
            .copied()
            .enumerate()
        {
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileBegin,
                })
                .unwrap();
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileBytes(contents.to_vec()),
                })
                .unwrap();
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileEnd,
                })
                .unwrap();
        }
        fs::rename(&swapped_parent, &moved_parent).unwrap();
        fs::create_dir(&swapped_parent).unwrap();

        let error = applier.finish().unwrap_err();

        assert!(
            error.to_string().contains("output parent directory path"),
            "{}",
            error
        );
        assert!(!moved_parent.join("second.txt").exists());
        assert!(!swapped_parent.join("second.txt").exists());
        assert!(apply_attempt_path(&state).unwrap().exists());
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        assert!(marker.contains("staged-file: "), "{}", marker);
    }

    #[test]
    fn temp_output_does_not_follow_swapped_stage_path() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        let staging = StagingArea::new(dir.path()).unwrap();
        let mut output = TempOutput::new(final_path.clone(), staging.shared()).unwrap();
        let stage_dir = output.stage_path().to_path_buf();
        let output_name = output.temp_path().file_name().unwrap().to_owned();
        let moved_stage = dir.path().join("moved-stage");
        output
            .file
            .as_mut()
            .unwrap()
            .write_all(b"retained")
            .unwrap();
        fs::rename(&stage_dir, &moved_stage).unwrap();
        fs::create_dir(&stage_dir).unwrap();
        fs::write(stage_dir.join(&output_name), b"substitute").unwrap();
        let entry = test_file_entry("out.txt", b"retained");

        let error = output.finish(&entry).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no longer refers to the retained directory"),
            "{}",
            error
        );
        assert!(!final_path.exists());
        assert_eq!(
            fs::read(stage_dir.join(&output_name)).unwrap(),
            b"substitute"
        );
        drop(staging);
        fs::remove_dir(&moved_stage).unwrap();
        fs::remove_file(stage_dir.join(output_name)).unwrap();
        fs::remove_dir(stage_dir).unwrap();
    }

    #[test]
    fn streamed_stage_path_swap_rejects_publication_and_keeps_phase_marker() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let entry = test_file_entry("out.txt", b"contents");
        let actions = vec![Action::Local(Change::Added(entry))];
        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        let mut applier =
            DetailApplier::new_with_attempt(base.clone(), actions, Vec::new(), Some(state.clone()));
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBytes(b"contents".to_vec()),
            })
            .unwrap();
        let stage_path = applier.staging.as_ref().unwrap().path().to_path_buf();
        let moved_stage = base.join("moved-stage");
        fs::rename(&stage_path, &moved_stage).unwrap();
        fs::create_dir(&stage_path).unwrap();

        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileEnd,
            })
            .unwrap();
        let error = applier.finish().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no longer refers to the retained directory"),
            "{}",
            error
        );
        assert!(!base.join("out.txt").exists());
        assert_eq!(marker_staged_paths(&state), vec![stage_path.clone()]);
        let recovery = describe_apply_attempt(&state).unwrap().unwrap();
        assert!(
            recovery.contains(&stage_path.display().to_string()),
            "{}",
            recovery
        );
        assert!(apply_attempt_path(&state).unwrap().exists());
        fs::remove_dir(moved_stage).unwrap();
        fs::remove_dir(stage_path).unwrap();
    }

    #[test]
    fn descriptor_component_helper_rejects_non_components() {
        use std::ffi::OsStr;

        assert!(path_component_cstring(OsStr::new("output"), "test").is_ok());
        assert!(path_component_cstring(OsStr::new("../output"), "test").is_err());
        assert!(path_component_cstring(OsStr::new("."), "test").is_err());
        assert!(path_component_cstring(OsStr::from_bytes(b"bad\0name"), "test").is_err());
    }

    #[test]
    fn strong_actions_reject_regular_files_without_digests() {
        let mut entry = test_file_entry("file.txt", b"contents");
        entry.set_digest(None);
        let actions = vec![Action::Local(Change::Added(entry.clone()))];
        assert!(validate_strong_actions(&actions).is_err());

        entry.set_digest(Some(content_digest(b"contents")));
        let actions = vec![Action::Local(Change::Added(entry))];
        validate_strong_actions(&actions).unwrap();
    }

    #[test]
    fn file_verification_rejects_symlink_to_matching_contents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("target.txt"), b"contents").unwrap();
        std::os::unix::fs::symlink("target.txt", dir.path().join("link.txt")).unwrap();
        let entry = test_file_entry("link.txt", b"contents");

        let error =
            verify_file_matches_entry(&dir.path().join("link.txt"), &entry, "verification target")
                .unwrap_err();

        assert!(error.to_string().contains("failed to open file"));
    }

    #[test]
    fn streamed_files_publish_requested_modes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let requested_modes = [0o600, 0o400, 0o000, 0o644];
        let entries = requested_modes
            .iter()
            .enumerate()
            .map(|(index, requested_mode)| {
                test_file_entry_with_mode(
                    &format!("file-{}.txt", index),
                    b"contents",
                    *requested_mode,
                )
            })
            .collect::<Vec<_>>();
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        let mut applier = DetailApplier::new_with_attempt(base.clone(), actions, Vec::new(), None);

        for (index, entry) in entries.iter().enumerate() {
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileBegin,
                })
                .unwrap();
            let output = match applier.state.as_ref().unwrap() {
                ApplyState::File { output, .. } => output,
                _ => panic!("expected streamed file state"),
            };
            assert_eq!(mode(output.temp_path()), 0o600);
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileBytes(b"contents".to_vec()),
                })
                .unwrap();
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileEnd,
                })
                .unwrap();
            assert!(!base.join(entry.path()).exists());
            let pending = &applier.output_batch.pending[index].prepared.output;
            assert_eq!(mode(pending.temp_path()), 0o600);
        }

        let final_entries = applier.finish().unwrap();
        assert_eq!(final_entries.len(), entries.len());
        for final_entry in final_entries {
            let metadata = fs::metadata(base.join(final_entry.path())).unwrap();
            assert_eq!(
                metadata.permissions().mode() & 0o777,
                final_entry.mode() & 0o777
            );
            assert_eq!(metadata.mtime(), final_entry.mtime());
            assert_eq!(final_entry.ino(), metadata.ino());
        }
    }

    #[test]
    fn streamed_files_share_one_stage_until_phase_finish() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let entries = vec![
            test_file_entry_with_mode("first.txt", b"first", 0o644),
            test_file_entry_with_mode("second.txt", b"second", 0o644),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        let mut applier =
            DetailApplier::new_with_attempt(base.clone(), actions, Vec::new(), Some(state.clone()));
        let mut stage_path = None;

        for (index, contents) in [b"first".as_slice(), b"second".as_slice()]
            .iter()
            .copied()
            .enumerate()
        {
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileBegin,
                })
                .unwrap();
            let output = match applier.state.as_ref().unwrap() {
                ApplyState::File { output, .. } => output,
                _ => panic!("expected streamed file state"),
            };
            assert_eq!(mode(output.temp_path()), 0o600);
            assert_eq!(mode(output.stage_path()), 0o700);
            match &stage_path {
                Some(stage_path) => assert_eq!(output.stage_path(), stage_path),
                None => stage_path = Some(output.stage_path().to_path_buf()),
            }
            assert_eq!(
                marker_staged_paths(&state),
                vec![stage_path.clone().unwrap()]
            );
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileBytes(contents.to_vec()),
                })
                .unwrap();
            applier
                .apply_frame(DetailFrame {
                    action_index: index as u32,
                    payload: DetailPayload::FileEnd,
                })
                .unwrap();
            assert!(stage_path.as_ref().unwrap().exists());
            assert!(!base.join(entries[index].path()).exists());
        }

        let stage_path = stage_path.unwrap();
        let final_entries = applier.finish().unwrap();

        assert!(!stage_path.exists());
        assert_eq!(marker_staged_paths(&state), vec![stage_path]);
        for entry in final_entries {
            assert_eq!(
                entry.ino(),
                fs::symlink_metadata(base.join(entry.path())).unwrap().ino()
            );
        }
    }

    #[test]
    fn streamed_stage_cleanup_preserves_restrictive_parent_modes() {
        for requested_mode in [0o000, 0o300] {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().join("base");
            fs::create_dir(&base).unwrap();
            let mut parent_entry = Entry::test_dir(PathBuf::from("parent"));
            parent_entry.set_mode(requested_mode);
            let file_entry = test_file_entry("parent/file.txt", b"contents");
            let actions = vec![
                Action::Local(Change::Added(parent_entry)),
                Action::Local(Change::Added(file_entry)),
            ];
            let mut applier =
                DetailApplier::new_with_attempt(base.clone(), actions, Vec::new(), None);
            applier
                .apply_frame(DetailFrame {
                    action_index: 1,
                    payload: DetailPayload::FileBegin,
                })
                .unwrap();
            let stage_path = applier.staging.as_ref().unwrap().path().to_path_buf();
            applier
                .apply_frame(DetailFrame {
                    action_index: 1,
                    payload: DetailPayload::FileBytes(b"contents".to_vec()),
                })
                .unwrap();
            applier
                .apply_frame(DetailFrame {
                    action_index: 1,
                    payload: DetailPayload::FileEnd,
                })
                .unwrap();

            applier.finish().unwrap();

            let parent = base.join("parent");
            assert_eq!(mode(&parent), requested_mode);
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(!stage_path.exists());
            assert_eq!(fs::read(parent.join("file.txt")).unwrap(), b"contents");
        }
    }

    #[test]
    fn streamed_diff_stays_private_until_final_metadata_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let old = synced_existing_file_entry(&base, "file.txt", b"old");
        let new = test_file_entry_with_mode("file.txt", b"new!", 0o400);
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let mut applier = DetailApplier::new_with_attempt(base.clone(), actions, vec![old], None);

        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffBegin,
            })
            .unwrap();
        let output = match applier.state.as_ref().unwrap() {
            ApplyState::Diff { output, .. } => output,
            _ => panic!("expected streamed diff state"),
        };
        assert_eq!(mode(output.temp_path()), 0o600);
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffBytes(b"new!".to_vec()),
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffEnd,
            })
            .unwrap();

        assert_eq!(mode(&base.join("file.txt")), 0o644);
        assert_eq!(
            mode(applier.output_batch.pending[0].prepared.output.temp_path()),
            0o600
        );
        applier.finish().unwrap();
        assert_eq!(mode(&base.join("file.txt")), 0o400);
        assert_eq!(fs::metadata(base.join("file.txt")).unwrap().mtime(), 0);
    }

    fn run_test_streamed_delta(
        old: &[u8],
        expected: &[u8],
        clone_backed: bool,
        apply: impl FnOnce(&mut fs::File, &mut fs::File, &mut StreamedOutputVerifier, &mut u64, bool),
    ) -> (Vec<u8>, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        fs::write(&target, old).unwrap();
        let mut source = fs::File::open(&target).unwrap();
        let staging = StagingArea::new(dir.path()).unwrap();
        let mut output = TempOutput::new(target.clone(), staging.shared()).unwrap();
        if clone_backed {
            output.file.as_mut().unwrap().write_all(old).unwrap();
        }
        let mut entry = test_file_entry("file.txt", expected);
        entry.set_digest(Some(content_digest(expected)));
        let mut verifier = StreamedOutputVerifier::new(&entry);
        let mut output_position = 0;

        apply(
            &mut source,
            output.file.as_mut().unwrap(),
            &mut verifier,
            &mut output_position,
            clone_backed,
        );
        output
            .file
            .as_ref()
            .unwrap()
            .set_len(output_position)
            .unwrap();
        verifier.verify(&entry).unwrap();

        (
            fs::read(output.temp_path()).unwrap(),
            fs::read(target).unwrap(),
        )
    }

    #[test]
    fn clone_backed_localized_replacement_remains_private() {
        let old = b"abcDEFghi";
        let expected = b"abcxyzghi";
        let (staged, target) = run_test_streamed_delta(
            old,
            expected,
            true,
            |source, output, verifier, position, clone_backed| {
                apply_diff_copy(source, output, verifier, position, clone_backed, 0, 3).unwrap();
                apply_diff_bytes(output, verifier, position, b"xyz").unwrap();
                apply_diff_copy(source, output, verifier, position, clone_backed, 6, 3).unwrap();
            },
        );

        assert_eq!(staged, expected);
        assert_eq!(target, old);
    }

    #[test]
    fn clone_backed_delta_materializes_moved_copy_ranges() {
        let (staged, target) = run_test_streamed_delta(
            b"abcdef",
            b"defabc",
            true,
            |source, output, verifier, position, clone_backed| {
                apply_diff_copy(source, output, verifier, position, clone_backed, 3, 3).unwrap();
                apply_diff_copy(source, output, verifier, position, clone_backed, 0, 3).unwrap();
            },
        );

        assert_eq!(staged, b"defabc");
        assert_eq!(target, b"abcdef");
    }

    #[test]
    fn clone_backed_delta_grows_with_literal_bytes() {
        let (staged, target) = run_test_streamed_delta(
            b"abc",
            b"abcdef",
            true,
            |source, output, verifier, position, clone_backed| {
                apply_diff_copy(source, output, verifier, position, clone_backed, 0, 3).unwrap();
                apply_diff_bytes(output, verifier, position, b"def").unwrap();
            },
        );

        assert_eq!(staged, b"abcdef");
        assert_eq!(target, b"abc");
    }

    #[test]
    fn clone_backed_delta_truncates_after_shrink() {
        let (staged, target) = run_test_streamed_delta(
            b"abcdef",
            b"abc",
            true,
            |source, output, verifier, position, clone_backed| {
                apply_diff_copy(source, output, verifier, position, clone_backed, 0, 3).unwrap();
            },
        );

        assert_eq!(staged, b"abc");
        assert_eq!(target, b"abcdef");
    }

    #[test]
    fn unsupported_clone_uses_byte_identical_materialized_delta() {
        assert!(clone_error_is_unsupported(&io::Error::from_raw_os_error(
            libc::EXDEV
        )));
        assert!(clone_error_is_unsupported(&io::Error::from_raw_os_error(
            libc::EOPNOTSUPP
        )));
        assert!(!clone_error_is_unsupported(&io::Error::from_raw_os_error(
            libc::EACCES
        )));
        assert!(!clone_error_is_unsupported(&io::Error::from_raw_os_error(
            libc::EIO
        )));
        #[cfg(target_os = "linux")]
        assert!(clone_error_is_unsupported(&io::Error::from_raw_os_error(
            libc::EBADF
        )));

        let (staged, target) = run_test_streamed_delta(
            b"abcdef",
            b"abXYZf",
            false,
            |source, output, verifier, position, clone_backed| {
                apply_diff_copy(source, output, verifier, position, clone_backed, 0, 2).unwrap();
                apply_diff_bytes(output, verifier, position, b"XYZ").unwrap();
                apply_diff_copy(source, output, verifier, position, clone_backed, 5, 1).unwrap();
            },
        );

        assert_eq!(staged, b"abXYZf");
        assert_eq!(target, b"abcdef");
    }

    #[test]
    fn platform_clone_creates_private_normalized_output_when_supported() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source");
        fs::write(&source_path, b"clone contents").unwrap();
        fs::set_permissions(&source_path, fs::Permissions::from_mode(0o744)).unwrap();
        let source = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&source_path)
            .unwrap();

        #[cfg(target_os = "macos")]
        {
            let name = std::ffi::CString::new("user.duet-clone-test").unwrap();
            let value = b"source-only";
            assert_eq!(
                unsafe {
                    libc::fsetxattr(
                        source.as_raw_fd(),
                        name.as_ptr(),
                        value.as_ptr().cast(),
                        value.len(),
                        0,
                        0,
                    )
                },
                0
            );
        }

        let staging = StagingArea::new(dir.path()).unwrap();
        let (path, name, mut file) = match staging.shared.clone_output(&source).unwrap() {
            CloneOutput::Unsupported => return,
            CloneOutput::Cloned(path, name, file) => (path, name, file),
        };

        assert_eq!(fs::read(&path).unwrap(), b"clone contents");
        assert_eq!(file.metadata().unwrap().mode() & 0o7777, 0o600);
        assert_eq!(file.metadata().unwrap().uid(), unsafe { libc::geteuid() });
        #[cfg(target_os = "macos")]
        {
            let names = macos_xattr_names(&file, &path).unwrap().collect::<Vec<_>>();
            assert!(!names
                .iter()
                .any(|name| name.as_slice() == b"user.duet-clone-test"));
            assert!(macos_acl_is_empty(&file, &path).unwrap());
        }

        file.seek(SeekFrom::Start(6)).unwrap();
        file.write_all(b"COW").unwrap();
        file.set_len(9).unwrap();
        file.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"clone COW");
        assert_eq!(fs::read(&source_path).unwrap(), b"clone contents");
        assert_eq!(fs::metadata(&source_path).unwrap().mode() & 0o7777, 0o744);

        drop(file);
        unlinkat(staging.shared.directory.as_raw_fd(), &name, 0).unwrap();
    }

    #[test]
    fn cow_clone_probe_cleans_private_staging() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let _ = staging_filesystem_info_with_clone_probe(base).unwrap();

        assert!(stage_directories(base).is_empty());
        assert!(!base.join(".duet-cow-probe-source").exists());
        assert!(!base.join(".duet-cow-probe-clone").exists());
    }

    #[test]
    fn fallback_contents_publish_requested_modes() {
        let requested_modes = [0o600, 0o400, 0o000, 0o644];
        for requested_mode in requested_modes {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().to_path_buf();
            let entry = test_file_entry_with_mode("file.txt", b"contents", requested_mode);
            let actions = vec![Action::Local(Change::Added(entry))];
            let details = vec![ChangeDetails::Contents(b"contents".to_vec())];
            let mut all_old = Vec::new();

            apply_detailed_changes(&base, &actions, &details, &mut all_old, None).unwrap();

            assert_eq!(mode(&base.join("file.txt")), requested_mode);
            assert_eq!(fs::metadata(base.join("file.txt")).unwrap().mtime(), 0);
            assert_ne!(fs::metadata(base.join("file.txt")).unwrap().ino(), 0);
        }
    }

    #[test]
    fn nested_writable_destination_does_not_broaden_nonwritable_root() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let nested = base.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o500)).unwrap();
        let entry = test_file_entry("nested/file.txt", b"contents");
        let actions = vec![Action::Local(Change::Added(entry))];
        let details = vec![ChangeDetails::Contents(b"contents".to_vec())];
        let mut all_old = Vec::new();

        let result = apply_detailed_changes(&base, &actions, &details, &mut all_old, None);

        assert_eq!(mode(&base), 0o500);
        assert_eq!(fs::read(nested.join("file.txt")).unwrap(), b"contents");
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
    }

    #[test]
    fn nested_destination_under_symlink_root_still_applies() {
        let dir = tempfile::tempdir().unwrap();
        let real_base = dir.path().join("real-base");
        let base = dir.path().join("base-link");
        fs::create_dir_all(real_base.join("nested")).unwrap();
        std::os::unix::fs::symlink(&real_base, &base).unwrap();
        let entry = test_file_entry("nested/file.txt", b"contents");
        let actions = vec![Action::Local(Change::Added(entry))];
        let details = vec![ChangeDetails::Contents(b"contents".to_vec())];
        let mut all_old = Vec::new();

        apply_detailed_changes(&base, &actions, &details, &mut all_old, None).unwrap();

        assert_eq!(
            fs::read(real_base.join("nested/file.txt")).unwrap(),
            b"contents"
        );
        assert!(fs::symlink_metadata(&base)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn fallback_files_share_one_phase_stage_marker() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let entries = vec![
            test_file_entry_with_mode("first.txt", b"first", 0o644),
            test_file_entry_with_mode("second.txt", b"second", 0o644),
        ];
        let actions = entries
            .iter()
            .cloned()
            .map(|entry| Action::Local(Change::Added(entry)))
            .collect::<Vec<_>>();
        let details = vec![
            ChangeDetails::Contents(b"first".to_vec()),
            ChangeDetails::Contents(b"second".to_vec()),
        ];
        let mut all_old = Vec::new();
        start_apply_attempt("local", &state, &base, &actions, None).unwrap();

        apply_detailed_changes(&base, &actions, &details, &mut all_old, Some(&state)).unwrap();

        let staged_paths = marker_staged_paths(&state);
        assert_eq!(staged_paths.len(), 1);
        assert!(!staged_paths[0].exists());
        for entry in all_old {
            let metadata = fs::symlink_metadata(base.join(entry.path())).unwrap();
            assert_eq!(entry.ino(), metadata.ino());
            assert_eq!(metadata.mode() & SYNCED_MODE_MASK, 0o644);
        }
    }

    #[test]
    fn content_free_apply_paths_create_no_stage() {
        let streamed_dir = tempfile::tempdir().unwrap();
        let streamed_base = streamed_dir.path().join("base");
        let streamed_state = streamed_dir.path().join("profile.snp");
        fs::create_dir(&streamed_base).unwrap();
        let old = synced_existing_file_entry(&streamed_base, "removed.txt", b"old");
        let actions = vec![Action::Local(Change::Removed(old.clone()))];
        start_apply_attempt("local", &streamed_state, &streamed_base, &actions, None).unwrap();

        DetailApplier::new_with_attempt(
            streamed_base.clone(),
            actions,
            vec![old],
            Some(streamed_state.clone()),
        )
        .finish()
        .unwrap();

        assert!(marker_staged_paths(&streamed_state).is_empty());
        assert!(stage_directories(&streamed_base).is_empty());

        let fallback_dir = tempfile::tempdir().unwrap();
        let fallback_base = fallback_dir.path().join("base");
        let fallback_state = fallback_dir.path().join("profile.snp");
        fs::create_dir(&fallback_base).unwrap();
        let old = synced_existing_file_entry(&fallback_base, "metadata.txt", b"old");
        let mut new = old.clone();
        new.set_mode(0o600);
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let mut all_old = vec![old];
        start_apply_attempt("local", &fallback_state, &fallback_base, &actions, None).unwrap();

        apply_detailed_changes(
            &fallback_base,
            &actions,
            &Vec::new(),
            &mut all_old,
            Some(&fallback_state),
        )
        .unwrap();

        assert!(marker_staged_paths(&fallback_state).is_empty());
        assert!(stage_directories(&fallback_base).is_empty());
    }

    #[test]
    fn fallback_diff_and_directory_replacement_apply_metadata_before_publication() {
        let diff_dir = tempfile::tempdir().unwrap();
        let diff_base = diff_dir.path().to_path_buf();
        let old = synced_existing_file_entry(&diff_base, "file.txt", b"old");
        let new = test_file_entry_with_mode("file.txt", b"new!", 0o000);
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let details = vec![ChangeDetails::Diff(Delta {
            blocks: vec![Block::Literal(b"new!".to_vec())],
            window: 1,
        })];
        let mut all_old = vec![old];

        apply_detailed_changes(&diff_base, &actions, &details, &mut all_old, None).unwrap();
        assert_eq!(mode(&diff_base.join("file.txt")), 0o000);
        assert_eq!(fs::metadata(diff_base.join("file.txt")).unwrap().mtime(), 0);

        let replacement_dir = tempfile::tempdir().unwrap();
        let replacement_base = replacement_dir.path().to_path_buf();
        fs::create_dir(replacement_base.join("path")).unwrap();
        let old = synced_existing_dir_entry(&replacement_base, "path");
        let new = test_file_entry_with_mode("path", b"replacement", 0o400);
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let details = vec![ChangeDetails::Contents(b"replacement".to_vec())];
        let mut all_old = vec![old];

        apply_detailed_changes(&replacement_base, &actions, &details, &mut all_old, None).unwrap();
        assert_eq!(mode(&replacement_base.join("path")), 0o400);
        assert_eq!(
            fs::metadata(replacement_base.join("path")).unwrap().mtime(),
            0
        );
    }

    #[test]
    fn apply_parent_directories_are_created_private_without_broadening_existing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        fs::set_permissions(base, fs::Permissions::from_mode(0o711)).unwrap();

        ensure_parent_directory(&base.join("one/two/file.txt")).unwrap();

        assert_eq!(mode(base), 0o711);
        assert_eq!(mode(&base.join("one")), 0o700);
        assert_eq!(mode(&base.join("one/two")), 0o700);
    }

    #[test]
    fn stream_diff_frames_coalesces_adjacent_copy_ops() {
        const WINDOW: usize = LEGACY_SIGNATURE_WINDOW;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let mut contents = vec![0; WINDOW * 8];
        rand::rngs::StdRng::seed_from_u64(1).fill_bytes(&mut contents);
        fs::write(&path, &contents).unwrap();

        let sig = signature(fs::File::open(&path).unwrap(), [0; WINDOW]).unwrap();
        let (sender, receiver) = mpsc::sync_channel(16);

        stream_diff_frames(path, 0, sig, 1024 * 1024, sender).unwrap();

        let frames = receiver
            .into_iter()
            .map(|frame| frame.unwrap())
            .collect::<Vec<_>>();

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].action_index, 0);
        assert!(matches!(
            frames[0].payload,
            DetailPayload::DiffCopy { offset: 0, len }
                if len >= contents.len() as u64 && len <= (contents.len() + WINDOW) as u64
        ));
        assert!(matches!(frames[1].payload, DetailPayload::DiffEnd));
    }

    #[test]
    fn get_detailed_changes_rejects_signature_path_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::write(base.join("file.txt"), b"new contents").unwrap();
        let old = test_file_entry("file.txt", b"old");
        let new = test_file_entry("file.txt", b"new contents");
        let actions = vec![Action::Remote(Change::Modified(old, new))];
        let sig = signature(&b"old"[..], [0; 4]).unwrap();
        let signatures = vec![SignatureWithPath(PathBuf::from("other.txt"), sig)];

        let error = get_detailed_changes(&base, &actions, &signatures)
            .unwrap_err()
            .to_string();

        assert!(error.contains("signature path mismatch"), "{}", error);
    }

    #[test]
    fn apply_rejects_invalid_delta_window() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::write(base.join("file.txt"), b"old").unwrap();
        let old = test_file_entry("file.txt", b"old");
        let new = test_file_entry("file.txt", b"new!");
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let details = vec![ChangeDetails::Diff(Delta {
            blocks: Vec::new(),
            window: 0,
        })];
        let mut all_old = vec![old];

        let error = apply_detailed_changes(&base, &actions, &details, &mut all_old, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid diff window"), "{}", error);
    }

    #[test]
    fn apply_verifies_diff_source_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::write(base.join("file.txt"), b"bad").unwrap();
        let old = test_file_entry("file.txt", b"old");
        let new = test_file_entry("file.txt", b"new!");
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let details = vec![ChangeDetails::Diff(Delta {
            blocks: vec![Block::Literal(b"new!".to_vec())],
            window: 1,
        })];
        let mut all_old = vec![old];

        let error = apply_detailed_changes(&base, &actions, &details, &mut all_old, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("diff source"), "{}", error);
        assert_eq!(fs::read(base.join("file.txt")).unwrap(), b"bad");
    }

    #[test]
    fn apply_verifies_diff_output_before_rename() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::write(base.join("file.txt"), b"old").unwrap();
        let old = test_file_entry("file.txt", b"old");
        let new = test_file_entry("file.txt", b"new!");
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let details = vec![ChangeDetails::Diff(Delta {
            blocks: vec![Block::Literal(b"bad!".to_vec())],
            window: 1,
        })];
        let mut all_old = vec![old];

        let error = apply_detailed_changes(&base, &actions, &details, &mut all_old, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("diff output"), "{}", error);
        assert_eq!(fs::read(base.join("file.txt")).unwrap(), b"old");
    }

    #[test]
    fn apply_rechecks_removed_file_before_delete() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let old = synced_existing_file_entry(&base, "file.txt", b"old");
        fs::write(base.join("file.txt"), b"bad").unwrap();
        let actions = vec![Action::Local(Change::Removed(old.clone()))];
        let mut all_old = vec![old];

        let error = apply_detailed_changes(&base, &actions, &Vec::new(), &mut all_old, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("remove target"), "{}", error);
        assert_eq!(fs::read(base.join("file.txt")).unwrap(), b"bad");
    }

    #[test]
    fn apply_ignores_symlink_mode_when_rechecking_removed_target() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        std::os::unix::fs::symlink("target", base.join("link")).unwrap();
        let meta = fs::symlink_metadata(base.join("link")).unwrap();
        let mismatched_mode = meta.mode() ^ 0o022;
        let old = Entry::test_symlink_with_mode_and_mtime(
            PathBuf::from("link"),
            PathBuf::from("target"),
            mismatched_mode,
            meta.mtime(),
        );
        let actions = vec![Action::Local(Change::Removed(old.clone()))];
        let mut all_old = vec![old];

        apply_detailed_changes(&base, &actions, &Vec::new(), &mut all_old, None).unwrap();

        assert!(fs::symlink_metadata(base.join("link")).is_err());
        assert!(all_old.is_empty());
    }

    #[test]
    fn apply_rechecks_added_file_destination_before_rename() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::write(base.join("file.txt"), b"race").unwrap();
        let entry = test_file_entry("file.txt", b"new");
        let actions = vec![Action::Local(Change::Added(entry))];
        let details = vec![ChangeDetails::Contents(b"new".to_vec())];
        let mut all_old = Vec::new();

        let error = apply_detailed_changes(&base, &actions, &details, &mut all_old, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("rename target"), "{}", error);
        assert_eq!(fs::read(base.join("file.txt")).unwrap(), b"race");
    }

    #[test]
    fn apply_rechecks_directory_metadata_target_before_update() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let dirname = base.join("dir");
        fs::create_dir(&dirname).unwrap();
        let old = update_meta(&dirname, &Entry::test_dir(PathBuf::from("dir"))).unwrap();
        let new = old.clone();
        let mut perms = fs::metadata(&dirname).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&dirname, perms).unwrap();
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let mut all_old = vec![old];

        let error = apply_detailed_changes(&base, &actions, &Vec::new(), &mut all_old, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("metadata target"), "{}", error);
        assert_eq!(
            fs::metadata(&dirname).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn streaming_diff_rechecks_destination_before_rename() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let old = synced_existing_file_entry(&base, "file.txt", b"old");
        let new = test_file_entry("file.txt", b"new!");
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let mut applier = DetailApplier::new_with_attempt(base.clone(), actions, vec![old], None);

        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffBegin,
            })
            .unwrap();
        fs::write(base.join("file.txt"), b"race").unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffBytes(b"new!".to_vec()),
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffEnd,
            })
            .unwrap();
        let error = applier.finish().unwrap_err().to_string();

        assert!(error.contains("staged commit target"), "{}", error);
        assert_eq!(fs::read(base.join("file.txt")).unwrap(), b"race");
    }

    #[test]
    fn streaming_diff_allows_empty_source_copy_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let old = synced_existing_file_entry(&base, "file.txt", b"");
        let new = test_file_entry("file.txt", b"new");
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let mut applier = DetailApplier::new_with_attempt(base.clone(), actions, vec![old], None);

        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffBegin,
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffBytes(b"new".to_vec()),
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffCopy { offset: 0, len: 4 },
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffEnd,
            })
            .unwrap();

        let new_entries = applier.finish().unwrap();
        assert_eq!(fs::read(base.join("file.txt")).unwrap(), b"new");
        assert_eq!(new_entries.len(), 1);
    }

    #[test]
    fn streaming_file_rechecks_added_destination_before_rename() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::write(base.join("file.txt"), b"race").unwrap();
        let entry = test_file_entry("file.txt", b"new");
        let actions = vec![Action::Local(Change::Added(entry))];
        let mut applier = DetailApplier::new_with_attempt(base.clone(), actions, Vec::new(), None);

        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBytes(b"new".to_vec()),
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileEnd,
            })
            .unwrap();
        let error = applier.finish().unwrap_err().to_string();

        assert!(
            error.contains("appeared after staged preparation"),
            "{}",
            error
        );
        assert_eq!(fs::read(base.join("file.txt")).unwrap(), b"race");
    }

    #[test]
    fn detail_applier_rejects_backward_action_indices() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let first = test_file_entry("a.txt", b"a");
        let second = test_file_entry("b.txt", b"b");
        let actions = vec![
            Action::Local(Change::Added(first)),
            Action::Local(Change::Added(second)),
        ];
        let mut applier = DetailApplier::new_with_attempt(base, actions, Vec::new(), None);

        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBytes(b"a".to_vec()),
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileEnd,
            })
            .unwrap();
        let stage_path = applier
            .staging
            .as_ref()
            .expect("staging area exists while output is pending")
            .path()
            .to_path_buf();
        let error = applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            })
            .unwrap_err()
            .to_string();

        assert!(error.contains("already processed"), "{}", error);
        drop(applier);
        assert!(!stage_path.exists());
    }

    #[test]
    fn detail_applier_rejects_file_frames_for_diff_actions() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::write(base.join("a.txt"), b"old").unwrap();
        let old = test_file_entry("a.txt", b"old");
        let new = test_file_entry("a.txt", b"new!");
        let actions = vec![Action::Local(Change::Modified(old.clone(), new))];
        let mut applier = DetailApplier::new_with_attempt(base, actions, vec![old], None);

        let error = applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            })
            .unwrap_err()
            .to_string();

        assert!(error.contains("unexpected detail kind"), "{}", error);
    }

    #[test]
    fn can_stream_details_rejects_directory_to_symlink_replacements() {
        let actions = vec![Action::Local(Change::Modified(
            Entry::test_dir(PathBuf::from("path")),
            Entry::test_symlink(PathBuf::from("path"), PathBuf::from("target")),
        ))];

        assert!(!can_stream_details(&actions));
    }

    #[test]
    fn can_stream_details_accepts_directory_metadata_changes() {
        let actions = vec![Action::Local(Change::Modified(
            Entry::test_dir(PathBuf::from("path")),
            Entry::test_dir(PathBuf::from("path")),
        ))];

        assert!(can_stream_details(&actions));
    }

    #[test]
    fn signature_window_config_uses_sqrt_clamped_to_limits() {
        let config = SignatureWindowConfig { min: 8, max: 64 };

        assert_eq!(config.window_for_size(0), 8);
        assert_eq!(config.window_for_size(1), 8);
        assert_eq!(config.window_for_size(256), 16);
        assert_eq!(config.window_for_size(10_000), 64);
    }

    #[test]
    fn signature_window_config_normalizes_invalid_limits() {
        let config = SignatureWindowConfig { min: 0, max: 0 };

        assert_eq!(config.window_for_size(1), 1);
    }

    #[test]
    fn sync_tuning_applies_env_overrides_and_ignores_invalid_values() {
        let tuning = SyncTuning::preferred().with_env_overrides_from(|name| match name {
            ENV_SIGNATURE_WINDOW_MIN => Some("4096".to_string()),
            ENV_SIGNATURE_WINDOW_MAX => Some("33554432".to_string()),
            ENV_DETAIL_CHUNK_BYTES => Some("8388608".to_string()),
            ENV_DETAIL_BATCH_FRAMES => Some("invalid".to_string()),
            ENV_DETAIL_BATCH_PAYLOAD_BYTES => Some("536870912".to_string()),
            _ => None,
        });

        assert_eq!(tuning.signature_window_min, 4096);
        assert_eq!(tuning.signature_window_max, MAX_SIGNATURE_WINDOW);
        assert_eq!(tuning.detail_chunk_bytes, 8 * 1024 * 1024);
        assert_eq!(
            tuning.detail_batch_frames,
            DEFAULT_DETAIL_BATCH_FRAMES as u32
        );
        assert_eq!(
            tuning.detail_batch_payload_bytes,
            MAX_DETAIL_BATCH_PAYLOAD_BYTES
        );
    }

    #[test]
    fn detail_frames_transfer_bytes_counts_reconstructed_bytes() {
        let frames = vec![
            DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            },
            DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBytes(vec![0; 7]),
            },
            DetailFrame {
                action_index: 1,
                payload: DetailPayload::DiffCopy { offset: 0, len: 11 },
            },
            DetailFrame {
                action_index: 1,
                payload: DetailPayload::DiffBytes(vec![0; 13]),
            },
            DetailFrame {
                action_index: 1,
                payload: DetailPayload::DiffEnd,
            },
        ];

        assert_eq!(detail_frames_transfer_bytes(&frames), 31);
    }

    #[test]
    fn temp_output_name_stays_short_for_long_destination_names() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join(format!("{}.txt", "a".repeat(250)));
        let staging = StagingArea::new(dir.path()).unwrap();

        let output = TempOutput::new(final_path.clone(), staging.shared()).unwrap();
        let temp_name = output.temp_path.file_name().unwrap().to_string_lossy();

        assert!(temp_name.len() < 64, "temp name was {}", temp_name);
        assert!(output.temp_path.exists());

        output
            .finish(&test_file_entry(
                final_path.file_name().unwrap().to_str().unwrap(),
                b"",
            ))
            .unwrap();
        staging.finish(&HashSet::new()).unwrap();
        assert!(final_path.exists());
    }

    #[test]
    fn temp_output_does_not_clobber_predictable_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        let old_predictable = dir
            .path()
            .join(format!(".duet-part-{}-0", std::process::id()));
        fs::write(&old_predictable, b"do not touch").unwrap();
        let staging = StagingArea::new(dir.path()).unwrap();

        let output = TempOutput::new(final_path, staging.shared()).unwrap();

        assert_ne!(output.temp_path, old_predictable);
        assert_eq!(fs::read(&old_predictable).unwrap(), b"do not touch");
    }

    #[test]
    fn safe_join_rejects_paths_that_escape_base() {
        let base = Path::new("/tmp/base");

        assert!(safe_join(base, Path::new("file.txt")).is_ok());
        assert!(safe_join(base, Path::new("dir/file.txt")).is_ok());
        assert!(safe_join(base, Path::new("../file.txt")).is_err());
        assert!(safe_join(base, Path::new("dir/../file.txt")).is_err());
        assert!(safe_join(base, Path::new("/tmp/file.txt")).is_err());
    }

    #[test]
    fn preflight_rejects_action_paths_that_escape_base() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let actions = vec![Action::Local(Change::Added(Entry::test_file(
            PathBuf::from("../escape.txt"),
            0,
        )))];

        let error = preflight_apply(&base, &actions).unwrap_err().to_string();

        assert!(error.contains("invalid action entry path"), "{}", error);
    }

    #[test]
    fn synced_mode_masks_file_type_bits() {
        assert_eq!(synced_mode(0o100644), 0o644);
        assert_eq!(synced_mode(0o40755), 0o755);
        assert_eq!(synced_mode(0o104755), 0o4755);
    }

    #[test]
    fn preflight_allows_creatable_parent_for_added_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let actions = vec![Action::Local(Change::Added(Entry::test_file(
            PathBuf::from(".git/refs/remotes/origin/main"),
            0,
        )))];

        preflight_apply(&base, &actions).unwrap();
    }

    #[test]
    fn apply_added_file_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let path = PathBuf::from(".git/refs/remotes/origin/main");
        let contents = b"commit-id\n".to_vec();
        let checksum = adler32::adler32(&contents[..]).unwrap();
        let actions = vec![Action::Local(Change::Added(Entry::test_file_with_size(
            path.clone(),
            contents.len() as u64,
            checksum,
        )))];
        let details = vec![ChangeDetails::Contents(contents.clone())];
        let mut all_old = Vec::new();

        preflight_apply(&base, &actions).unwrap();
        apply_detailed_changes(&base, &actions, &details, &mut all_old, None).unwrap();

        assert_eq!(fs::read(base.join(path)).unwrap(), contents);
    }

    #[test]
    fn preflight_rejects_removed_directory_with_untracked_child() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir(base.join("removed")).unwrap();
        fs::write(base.join("removed/untracked.txt"), b"still here").unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];

        let error = preflight_apply(&base, &actions).unwrap_err().to_string();

        assert!(error.contains("destination directory"), "{}", error);
        assert!(error.contains("unexpected child"), "{}", error);
        assert!(error.contains("untracked.txt"), "{}", error);
    }

    #[test]
    fn preflight_classifies_ignored_removed_directory_blocker() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir(base.join("removed")).unwrap();
        fs::create_dir(base.join("removed/__pycache__")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            vec!["__pycache__".to_string()],
        );

        let error =
            preflight_apply_with_policy(&base, &actions, Some(&policy), ApplyOptions::default())
                .unwrap_err()
                .to_string();

        assert!(error.contains("ignored child"), "{}", error);
        assert!(error.contains("__pycache__"), "{}", error);
        assert!(error.contains("--prune-ignored"), "{}", error);

        let report =
            preflight_apply_report(&base, &actions, Some(&policy), ApplyOptions::default())
                .unwrap();
        assert_eq!(report.blockers.len(), 1);
        assert_eq!(report.blockers[0].kind, RemovalBlockerType::Ignored);
        assert_eq!(report.blockers[0].pattern.as_deref(), Some("__pycache__"));
        assert!(!report.blockers[0].prunable);
        assert!(report.has_unprunable_blockers());
    }

    #[test]
    fn preflight_report_marks_ignored_blocker_prunable_with_option() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            vec!["__pycache__".to_string()],
        );

        let report = preflight_apply_report(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap();

        assert_eq!(report.blockers.len(), 1);
        assert_eq!(report.blockers[0].kind, RemovalBlockerType::Ignored);
        assert_eq!(report.blockers[0].pattern.as_deref(), Some("__pycache__"));
        assert!(report.blockers[0].prunable);
        assert!(!report.has_unprunable_blockers());
    }

    #[test]
    fn preflight_prunes_profile_prune_blocker_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::with_prune(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            Vec::new(),
            vec!["__pycache__".to_string()],
        );

        let report =
            preflight_apply_report(&base, &actions, Some(&policy), ApplyOptions::default())
                .unwrap();
        assert_eq!(report.blockers.len(), 1);
        assert_eq!(report.blockers[0].kind, RemovalBlockerType::Prune);
        assert_eq!(report.blockers[0].pattern.as_deref(), Some("__pycache__"));
        assert!(report.blockers[0].prunable);
        assert!(!report.has_unprunable_blockers());
        preflight_apply_with_policy(&base, &actions, Some(&policy), ApplyOptions::default())
            .unwrap();
    }

    #[test]
    fn preflight_classifies_excluded_removed_directory_blocker() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/excluded")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
                Location::Exclude(PathBuf::from("removed/excluded")),
            ],
            Vec::new(),
        );

        let error =
            preflight_apply_with_policy(&base, &actions, Some(&policy), ApplyOptions::default())
                .unwrap_err()
                .to_string();

        assert!(error.contains("excluded child"), "{}", error);
        assert!(error.contains("outside the sync selection"), "{}", error);

        let report =
            preflight_apply_report(&base, &actions, Some(&policy), ApplyOptions::default())
                .unwrap();
        assert_eq!(report.blockers.len(), 1);
        assert_eq!(report.blockers[0].kind, RemovalBlockerType::Excluded);
        assert!(!report.blockers[0].prunable);
        assert!(report.has_unprunable_blockers());
    }

    #[test]
    fn preflight_report_is_clear_without_directory_blockers() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let actions = vec![Action::Local(Change::Added(Entry::test_file(
            PathBuf::from("new.txt"),
            0,
        )))];

        let report =
            preflight_apply_report(&base, &actions, None, ApplyOptions::default()).unwrap();

        assert!(report.blockers.is_empty());
        assert!(!report.has_unprunable_blockers());
    }

    #[test]
    fn preflight_report_rejects_invalid_prune_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let actions = Vec::new();
        let policy = ScanPolicy::with_prune(Vec::new(), Vec::new(), vec!["[".to_string()]);

        let error = preflight_apply_report(&base, &actions, Some(&policy), ApplyOptions::default())
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid prune pattern"), "{}", error);
    }

    #[test]
    fn preflight_allows_ignored_blocker_with_prune_option() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            vec!["__pycache__".to_string()],
        );

        preflight_apply_with_policy(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap();
    }

    #[test]
    fn preflight_treats_root_include_as_selected_for_ignored_pruning() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from(".")),
            ],
            vec!["__pycache__".to_string()],
        );

        preflight_apply_with_policy(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap();
    }

    #[test]
    fn removal_policy_uses_canonical_location_precedence() {
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::new()),
                Location::Include(PathBuf::from("tree")),
                Location::Exclude(PathBuf::from("tree/./private")),
                Location::Include(PathBuf::from("tree/private/keep")),
                Location::Exclude(PathBuf::from("duplicate")),
                Location::Include(PathBuf::from("./duplicate")),
            ],
            Vec::new(),
        );
        let policy = RemovalBlockerPolicy::new(Some(&policy), ApplyOptions::default()).unwrap();

        assert!(!policy.is_excluded(Path::new("root.txt")));
        assert!(policy.is_excluded(Path::new("tree/private/hidden.txt")));
        assert!(!policy.is_excluded(Path::new("tree/private/keep/file.txt")));
        assert!(!policy.is_excluded(Path::new("duplicate/file.txt")));
    }

    #[test]
    fn preflight_does_not_prune_explicitly_excluded_ignored_blocker() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
                Location::Exclude(PathBuf::from("removed/__pycache__")),
            ],
            vec!["__pycache__".to_string()],
        );

        let error = preflight_apply_with_policy(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("excluded child"), "{}", error);
        assert!(base.join("removed/__pycache__").exists());
    }

    #[test]
    fn preflight_does_not_prune_cli_excluded_ignored_blocker() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/cache")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![Location::Include(PathBuf::new())],
            vec!["cache".to_string()],
        )
        .with_excludes(vec![PathBuf::from("removed/cache")]);

        let report = preflight_apply_report(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap();

        assert_eq!(report.blockers[0].kind, RemovalBlockerType::Excluded);
        assert!(!report.blockers[0].prunable);
        assert!(base.join("removed/cache").exists());
    }

    #[test]
    fn preflight_does_not_prune_ignored_parent_of_cli_exclusion() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/cache/protected")).unwrap();
        fs::write(base.join("removed/cache/protected/data"), b"keep").unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![Location::Include(PathBuf::new())],
            vec!["cache".to_string()],
        )
        .with_excludes(vec![PathBuf::from("removed/cache/protected")]);

        let error = preflight_apply_with_policy(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("excluded child"), "{}", error);
        assert!(base.join("removed/cache/protected/data").exists());
    }

    #[test]
    fn preflight_does_not_prune_excluded_descendant_in_ignored_directory() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/cache")).unwrap();
        fs::write(base.join("removed/cache/keep.db"), b"keep").unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
                Location::Exclude(PathBuf::from("removed/cache/keep.db")),
            ],
            vec!["cache".to_string()],
        );

        let error = preflight_apply_with_policy(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("excluded child"), "{}", error);
        assert!(base.join("removed/cache/keep.db").exists());
    }

    #[test]
    fn preflight_rejects_ignored_prune_without_unlink_permission() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let removed = base.join("removed");
        fs::create_dir_all(removed.join("__pycache__")).unwrap();
        fs::write(removed.join("__pycache__/cache.pyc"), b"cache").unwrap();
        let mut perms = fs::metadata(&removed).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(&removed, perms).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            vec!["__pycache__".to_string()],
        );

        let error = preflight_apply_with_policy(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap_err()
        .to_string();

        let mut perms = fs::metadata(&removed).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&removed, perms).unwrap();
        assert!(error.contains("ignored prune parent"), "{}", error);
        assert!(error.contains("not writable"), "{}", error);
    }

    #[test]
    fn apply_prunes_ignored_blocker_before_directory_removal() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        fs::write(base.join("removed/__pycache__/cache.pyc"), b"cache").unwrap();
        let old = synced_existing_dir_entry(&base, "removed");
        let actions = vec![Action::Local(Change::Removed(old.clone()))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            vec!["__pycache__".to_string()],
        );
        let mut all_old = vec![old.clone()];

        apply_detailed_changes_with_policy(
            &base,
            &actions,
            &Vec::new(),
            &mut all_old,
            None,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap();

        assert!(!base.join("removed").exists());
        assert!(all_old.is_empty());
    }

    #[test]
    fn apply_does_not_prune_ignored_blocker_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        fs::write(base.join("removed/__pycache__/cache.pyc"), b"cache").unwrap();
        let old = synced_existing_dir_entry(&base, "removed");
        let actions = vec![Action::Local(Change::Removed(old.clone()))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            vec!["__pycache__".to_string()],
        );
        let mut all_old = vec![old.clone()];

        let error = apply_detailed_changes_with_policy(
            &base,
            &actions,
            &Vec::new(),
            &mut all_old,
            None,
            Some(&policy),
            ApplyOptions::default(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("ignored child"), "{}", error);
        assert!(error.contains("--prune-ignored"), "{}", error);
        assert!(base.join("removed/__pycache__/cache.pyc").exists());
        assert!(base.join("removed").exists());
        assert_eq!(all_old, vec![old]);
    }

    #[test]
    fn apply_prunes_profile_prune_blocker_without_option() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        fs::write(base.join("removed/__pycache__/cache.pyc"), b"cache").unwrap();
        let old = synced_existing_dir_entry(&base, "removed");
        let actions = vec![Action::Local(Change::Removed(old.clone()))];
        let policy = ScanPolicy::with_prune(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            Vec::new(),
            vec!["__pycache__".to_string()],
        );
        let mut all_old = vec![old];

        apply_detailed_changes_with_policy(
            &base,
            &actions,
            &Vec::new(),
            &mut all_old,
            None,
            Some(&policy),
            ApplyOptions::default(),
        )
        .unwrap();

        assert!(!base.join("removed").exists());
        assert!(all_old.is_empty());
    }

    #[test]
    fn streaming_apply_prunes_profile_prune_blocker_before_directory_removal() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        fs::create_dir_all(base.join("removed/__pycache__")).unwrap();
        fs::write(base.join("removed/__pycache__/cache.pyc"), b"cache").unwrap();
        let old = synced_existing_dir_entry(&base, "removed");
        let actions = vec![Action::Local(Change::Removed(old.clone()))];
        let policy = ScanPolicy::with_prune(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            Vec::new(),
            vec!["__pycache__".to_string()],
        );
        let applier = DetailApplier::new_with_attempt_and_policy(
            base.clone(),
            actions,
            vec![old],
            None,
            Some(policy),
            ApplyOptions::default(),
        );

        let new_entries = applier.finish().unwrap();

        assert!(!base.join("removed").exists());
        assert!(new_entries.is_empty());
    }

    #[test]
    fn apply_prunes_ignored_symlink_without_following_target() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let outside = dir.path().join("outside");
        fs::create_dir_all(base.join("removed")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("removed/__pycache__")).unwrap();
        let actions = vec![Action::Local(Change::Removed(Entry::test_dir(
            PathBuf::from("removed"),
        )))];
        let policy = ScanPolicy::new(
            vec![
                Location::Exclude(PathBuf::from(".")),
                Location::Include(PathBuf::from("removed")),
            ],
            vec!["__pycache__".to_string()],
        );
        let mut all_old = vec![Entry::test_dir(PathBuf::from("removed"))];

        apply_detailed_changes_with_policy(
            &base,
            &actions,
            &Vec::new(),
            &mut all_old,
            None,
            Some(&policy),
            ApplyOptions {
                prune_ignored: true,
            },
        )
        .unwrap();

        assert!(!base.join("removed").exists());
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn apply_attempt_marker_blocks_until_finished() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let base = dir.path().join("base");
        let actions = vec![Action::Local(Change::Added(Entry::test_file(
            PathBuf::from("a.txt"),
            0,
        )))];
        fs::create_dir(&base).unwrap();

        start_apply_attempt("local", &state, &base, &actions, Some("attempt-1")).unwrap();
        record_staged_file(Some(&state), &base.join(".duet-part-test")).unwrap();
        record_committed_step(Some(&state), "rename-file", &PathBuf::from("a.txt")).unwrap();
        record_committed_action(Some(&state), &actions[0]).unwrap();
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        assert!(marker.contains("attempt-id: attempt-1"), "{}", marker);
        assert!(marker.contains("operation: add-file a.txt"), "{}", marker);
        assert!(
            marker.contains("unstaged-operation: metadata a.txt"),
            "{}",
            marker
        );
        assert!(marker.contains("staged-file: "), "{}", marker);
        assert!(
            marker.contains("committed-step: rename-file a.txt"),
            "{}",
            marker
        );
        assert!(
            marker.contains("committed-operation: add-file a.txt"),
            "{}",
            marker
        );
        let error = check_apply_attempt_clear(&state).unwrap_err().to_string();

        assert!(error.contains("previous Duet apply attempt did not finish"));
        assert!(error.contains("Inspect this marker with `duet recover "));
        assert!(error.contains("duet recover --clear "));
        assert!(error.contains("Run recovery commands on the side"));
        assert!(error.contains("Recovery marker contents:"));
        assert!(error.contains("side: local"));
        assert!(error.contains("phase: apply"));
        assert!(error.contains("path: a.txt"));

        mark_apply_attempt_state_save("local", &state, &base, &actions, Some("attempt-1")).unwrap();
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        assert!(marker.contains("attempt-id: attempt-1"), "{}", marker);
        assert!(
            marker.contains("unstaged-operation: metadata a.txt"),
            "{}",
            marker
        );
        assert!(marker.contains("staged-file: "), "{}", marker);
        assert!(
            marker.contains("committed-operation: add-file a.txt"),
            "{}",
            marker
        );
        assert!(
            marker.contains("committed-step: rename-file a.txt"),
            "{}",
            marker
        );
        let error = check_apply_attempt_clear(&state).unwrap_err().to_string();
        assert!(error.contains("phase: state-save"));
        assert!(error.contains("state may not have been saved"));
        assert!(error.contains("committed operation(s)"));
        assert!(error.contains("committed apply step(s)"));

        finish_apply_attempt(&state).unwrap();
        check_apply_attempt_clear(&state).unwrap();
    }

    #[test]
    fn apply_attempt_marker_is_private_and_phase_transition_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let base = dir.path().join("base");
        fs::create_dir(&base).unwrap();
        let actions = vec![Action::Local(Change::Added(Entry::test_file(
            PathBuf::from("a.txt"),
            0,
        )))];

        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        let marker_path = apply_attempt_path(&state).unwrap();
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o644)).unwrap();
        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        assert_eq!(
            fs::metadata(&marker_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        record_staged_file(Some(&state), Path::new(".duet-part-test")).unwrap();
        record_committed_step(Some(&state), "rename-file", Path::new("a.txt")).unwrap();
        mark_apply_attempt_state_save("local", &state, &base, &actions, None).unwrap();

        let marker = fs::read_to_string(&marker_path).unwrap();
        assert!(marker.contains("phase: state-save"), "{}", marker);
        assert!(
            marker.contains("staged-file: .duet-part-test"),
            "{}",
            marker
        );
        assert!(
            marker.contains("committed-step: rename-file a.txt"),
            "{}",
            marker
        );
        assert_eq!(
            fs::metadata(&marker_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn apply_attempt_recovery_detects_leftover_empty_stage_directory() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let base = dir.path().join("base");
        let stage_dir = base.join(".duet-stage-leftover");
        fs::create_dir(&base).unwrap();
        fs::create_dir(&stage_dir).unwrap();
        let actions = vec![Action::Local(Change::Added(Entry::test_file(
            PathBuf::from("a.txt"),
            0,
        )))];
        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        record_staged_file(Some(&state), &stage_dir).unwrap();

        let description = describe_apply_attempt(&state).unwrap().unwrap();

        assert!(
            description.contains("1 staged temporary path(s), and 1 still exist"),
            "{}",
            description
        );
    }

    #[test]
    fn apply_attempt_recovery_advice_uses_operation_summaries() {
        let marker = "duet-apply-attempt-v1\nphase: apply\noperation: remove-file old.txt\noperation: modify-metadata mode.txt\noperation: modify-file contents.txt\nunstaged-operation: remove-file old.txt\nstaged-file: /tmp/.duet-part-test\ncommitted-step: rename-file contents.txt\ncommitted-operation: modify-file contents.txt\n";

        let advice = apply_attempt_recovery_advice(
            Path::new("/tmp/profile.snp"),
            Path::new("/tmp/.profile.snp.duet-apply"),
            marker,
        );

        assert!(
            advice.contains("Inspect this marker with `duet recover /tmp/profile.snp`"),
            "{}",
            advice
        );
        assert!(
            advice.contains("duet recover --clear /tmp/profile.snp"),
            "{}",
            advice
        );

        assert!(advice.contains("rm /tmp/.profile.snp.duet-apply"));
        assert!(advice.contains("SSH to the remote host first"));
        assert!(advice.contains("Removed or replaced paths"), "{}", advice);
        assert!(advice.contains("Metadata operations"), "{}", advice);
        assert!(
            advice.contains("File contents may have changed"),
            "{}",
            advice
        );
        assert!(advice.contains("committed operation(s)"), "{}", advice);
        assert!(advice.contains("committed apply step(s)"), "{}", advice);
        assert!(advice.contains("staged temporary path(s)"), "{}", advice);
        assert!(advice.contains("unstaged operation(s)"), "{}", advice);
    }

    #[test]
    fn staged_precommit_recovery_advice_does_not_suggest_raw_marker_removal() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let base = dir.path().join("base");
        fs::create_dir(&base).unwrap();
        start_staged_apply_attempt("local", &state, &base, &[], "attempt-1").unwrap();
        let marker_path = apply_attempt_path(&state).unwrap();
        let marker = fs::read_to_string(&marker_path).unwrap();

        let advice = apply_attempt_recovery_advice(&state, &marker_path, &marker);

        assert!(advice.contains("duet recover --clear"), "{}", advice);
        assert!(
            advice.contains("do not remove the marker directly"),
            "{}",
            advice
        );
        assert!(!advice.contains("manually with `rm"), "{}", advice);
    }

    #[test]
    fn malformed_v2_recovery_advice_does_not_suggest_raw_marker_removal() {
        let marker = "duet-apply-attempt-v2";
        let advice = apply_attempt_recovery_advice(
            Path::new("/tmp/profile.snp"),
            Path::new("/tmp/.profile.snp.duet-apply"),
            marker,
        );

        assert!(
            advice.contains("do not remove the marker directly"),
            "{}",
            advice
        );
        assert!(!advice.contains("manually with `rm"), "{}", advice);
    }

    #[test]
    fn staged_marker_rejects_line_breaking_action_paths() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let base = dir.path().join("base");
        fs::create_dir(&base).unwrap();
        let actions = vec![Action::Local(Change::Added(Entry::test_file(
            PathBuf::from("bad\nphase: committed"),
            0,
        )))];

        let error =
            start_staged_apply_attempt("local", &state, &base, &actions, "attempt-1").unwrap_err();

        assert!(error.to_string().contains("single-line"), "{}", error);
        assert!(!apply_attempt_path(&state).unwrap().exists());
    }

    #[test]
    fn conflicting_staged_marker_advice_uses_state_path() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let base = dir.path().join("base");
        fs::create_dir(&base).unwrap();
        start_staged_apply_attempt("local", &state, &base, &[], "attempt-1").unwrap();

        let error =
            start_staged_apply_attempt("local", &state, &base, &[], "attempt-2").unwrap_err();
        let rendered = format!("{:#}", error);

        assert!(
            rendered.contains(&format!("duet recover --clear {}", state.display())),
            "{}",
            rendered
        );
        let marker_path = apply_attempt_path(&state).unwrap();
        assert!(
            !rendered.contains(&format!("duet recover {}", marker_path.display())),
            "{}",
            rendered
        );
    }

    #[test]
    fn staged_prepare_mutates_no_targets_and_commit_applies_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let removed = synced_existing_file_entry(&base, "z-remove.txt", b"old");
        let added_dir = Entry::test_dir(PathBuf::from("a-dir"));
        let added_file = test_file_entry("b-file.txt", b"new");
        let added_link = Entry::test_symlink(PathBuf::from("c-link"), PathBuf::from("b-file.txt"));
        let actions = vec![
            Action::Local(Change::Added(added_dir)),
            Action::Local(Change::Added(added_file)),
            Action::Local(Change::Added(added_link)),
            Action::Local(Change::Removed(removed.clone())),
        ];
        start_staged_apply_attempt("local", &state, &base, &actions, "attempt-1").unwrap();
        let mut applier = DetailApplier::new_staged_with_attempt_and_policy(
            base.clone(),
            actions,
            vec![removed],
            state.clone(),
            "attempt-1".to_string(),
            None,
            ApplyOptions::default(),
        );

        stream_file(&mut applier, 1, b"new").unwrap();
        let prepared = applier.finish_preparation().unwrap();

        assert_eq!(prepared.report().prepared_file_count, 1);
        assert!(!base.join("a-dir").exists());
        assert!(!base.join("b-file.txt").exists());
        assert!(!base.join("c-link").exists());
        assert_eq!(fs::read(base.join("z-remove.txt")).unwrap(), b"old");
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        assert!(marker.contains("phase: prepared"), "{}", marker);

        let entries = prepared.commit().unwrap();
        assert!(base.join("a-dir").is_dir());
        assert_eq!(fs::read(base.join("b-file.txt")).unwrap(), b"new");
        assert_eq!(
            fs::read_link(base.join("c-link")).unwrap(),
            Path::new("b-file.txt")
        );
        assert!(!base.join("z-remove.txt").exists());
        assert_eq!(entries.len(), 3);
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        assert!(marker.contains("phase: committed"), "{}", marker);
        mark_staged_apply_attempt_state_save(&state, "attempt-1").unwrap();
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        assert!(marker.contains("phase: state-save"), "{}", marker);
        finish_staged_apply_attempt(&state, "attempt-1").unwrap();
        assert!(!apply_attempt_path(&state).unwrap().exists());
    }

    #[test]
    fn legacy_staged_constructor_does_not_enable_reserve_enforcement() {
        let legacy = DetailApplier::new_staged_with_attempt_and_policy(
            PathBuf::from("base"),
            Vec::new(),
            Vec::new(),
            PathBuf::from("state"),
            "attempt".to_string(),
            None,
            ApplyOptions::default(),
        );
        assert!(legacy.staging_space_monitor.is_none());

        let negotiated = DetailApplier::new_capacity_aware_staged_with_attempt_and_policy(
            PathBuf::from("base"),
            Vec::new(),
            Vec::new(),
            PathBuf::from("state"),
            "attempt".to_string(),
            None,
            ApplyOptions::default(),
            StagingPolicy::default(),
        );
        assert!(negotiated.staging_space_monitor.is_some());
    }

    #[test]
    fn staged_commit_validation_rechecks_reserve_after_preparation() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let actions = vec![Action::Local(Change::Added(test_file_entry(
            "file.txt",
            b"contents",
        )))];
        start_staged_apply_attempt("local", &state, &base, &actions, "attempt-1").unwrap();
        let mut applier = DetailApplier::new_capacity_aware_staged_with_attempt_and_policy(
            base,
            actions,
            Vec::new(),
            state.clone(),
            "attempt-1".to_string(),
            None,
            ApplyOptions::default(),
            StagingPolicy {
                limit_bytes: None,
                reserve: StagingReserve::Bytes(0),
            },
        );
        stream_file(&mut applier, 0, b"contents").unwrap();
        let mut prepared = applier.finish_preparation().unwrap();
        prepared
            .inner
            .staging_space_monitor
            .as_mut()
            .unwrap()
            .policy = StagingPolicy {
            limit_bytes: None,
            reserve: StagingReserve::Bytes(u64::MAX),
        };

        let error = prepared.validate_commit().unwrap_err();
        assert!(
            error.to_string().contains("aborted before commit"),
            "{}",
            error
        );
        prepared.abort().unwrap();
        assert!(!apply_attempt_path(&state).unwrap().exists());
    }

    #[test]
    fn staged_abort_removes_stage_and_marker_and_checks_attempt_id() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let entry = test_file_entry("file.txt", b"contents");
        let actions = vec![Action::Local(Change::Added(entry))];
        start_staged_apply_attempt("local", &state, &base, &actions, "attempt-1").unwrap();
        let mut applier = DetailApplier::new_staged_with_attempt_and_policy(
            base.clone(),
            actions,
            Vec::new(),
            state.clone(),
            "attempt-1".to_string(),
            None,
            ApplyOptions::default(),
        );
        stream_file(&mut applier, 0, b"contents").unwrap();
        let prepared = applier.finish_preparation().unwrap();
        let stage = prepared
            .inner
            .staging
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();

        assert!(abort_staged_apply_attempt(&state, "wrong-attempt").is_err());
        assert!(stage.exists());
        prepared.abort().unwrap();
        assert!(!stage.exists());
        assert!(!apply_attempt_path(&state).unwrap().exists());
        assert!(!base.join("file.txt").exists());
    }

    #[test]
    fn staged_validation_rejects_in_place_output_modification() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let entry = test_file_entry("file.txt", b"contents");
        let actions = vec![Action::Local(Change::Added(entry))];
        start_staged_apply_attempt("local", &state, &base, &actions, "attempt-1").unwrap();
        let mut applier = DetailApplier::new_staged_with_attempt_and_policy(
            base,
            actions,
            Vec::new(),
            state.clone(),
            "attempt-1".to_string(),
            None,
            ApplyOptions::default(),
        );
        stream_file(&mut applier, 0, b"contents").unwrap();
        let prepared = applier.finish_preparation().unwrap();
        let output = prepared.inner.prepared_outputs[0]
            .as_ref()
            .unwrap()
            .output
            .temp_path
            .clone();

        fs::write(&output, b"mutated!").unwrap();
        let error = prepared.validate_commit().unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{}", error);
        prepared.abort().unwrap();
    }

    #[test]
    fn staged_abort_is_idempotent_after_partial_stage_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let actions = vec![
            Action::Local(Change::Added(test_file_entry("a.txt", b"a"))),
            Action::Local(Change::Added(test_file_entry("b.txt", b"b"))),
        ];
        start_staged_apply_attempt("local", &state, &base, &actions, "attempt-1").unwrap();
        let mut applier = DetailApplier::new_staged_with_attempt_and_policy(
            base.clone(),
            actions,
            Vec::new(),
            state.clone(),
            "attempt-1".to_string(),
            None,
            ApplyOptions::default(),
        );
        stream_file(&mut applier, 0, b"a").unwrap();
        stream_file(&mut applier, 1, b"b").unwrap();
        let prepared = applier.finish_preparation().unwrap();
        let first_output = prepared.inner.prepared_outputs[0]
            .as_ref()
            .unwrap()
            .output
            .temp_path
            .clone();
        let stage = prepared
            .inner
            .staging
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();

        fs::remove_file(first_output).unwrap();
        prepared.abort().unwrap();
        assert!(!stage.exists());
        assert!(!apply_attempt_path(&state).unwrap().exists());

        let state2 = dir.path().join("profile2.snp");
        let actions = vec![Action::Local(Change::Added(test_file_entry("c.txt", b"c")))];
        start_staged_apply_attempt("local", &state2, &base, &actions, "attempt-2").unwrap();
        let mut applier = DetailApplier::new_staged_with_attempt_and_policy(
            base,
            actions,
            Vec::new(),
            state2.clone(),
            "attempt-2".to_string(),
            None,
            ApplyOptions::default(),
        );
        stream_file(&mut applier, 0, b"c").unwrap();
        let prepared = applier.finish_preparation().unwrap();
        let output = prepared.inner.prepared_outputs[0]
            .as_ref()
            .unwrap()
            .output
            .temp_path
            .clone();
        let stage = prepared
            .inner
            .staging
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        fs::remove_file(output).unwrap();
        fs::remove_dir(stage).unwrap();

        prepared.abort().unwrap();
        assert!(!apply_attempt_path(&state2).unwrap().exists());
    }

    #[test]
    fn staged_cleanup_fails_closed_for_substitution_and_committing_phase() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let entry = test_file_entry("file.txt", b"contents");
        let actions = vec![Action::Local(Change::Added(entry))];
        start_staged_apply_attempt("local", &state, &base, &actions, "attempt-1").unwrap();
        let mut applier = DetailApplier::new_staged_with_attempt_and_policy(
            base.clone(),
            actions,
            Vec::new(),
            state.clone(),
            "attempt-1".to_string(),
            None,
            ApplyOptions::default(),
        );
        stream_file(&mut applier, 0, b"contents").unwrap();
        let prepared = applier.finish_preparation().unwrap();
        let output = prepared.inner.prepared_outputs[0]
            .as_ref()
            .unwrap()
            .output
            .temp_path
            .clone();
        fs::remove_file(&output).unwrap();
        fs::write(&output, b"substitute").unwrap();
        assert!(abort_staged_apply_attempt(&state, "attempt-1").is_err());
        assert!(apply_attempt_path(&state).unwrap().exists());
        drop(prepared);

        let state2 = dir.path().join("profile2.snp");
        start_staged_apply_attempt("local", &state2, &base, &[], "attempt-2").unwrap();
        transition_staged_apply_attempt(
            &state2,
            "attempt-2",
            &[ApplyAttemptPhase::Preparing],
            ApplyAttemptPhase::Prepared,
        )
        .unwrap();
        transition_staged_apply_attempt(
            &state2,
            "attempt-2",
            &[ApplyAttemptPhase::Prepared],
            ApplyAttemptPhase::Committing,
        )
        .unwrap();
        assert!(abort_staged_apply_attempt(&state2, "attempt-2").is_err());
        clear_apply_attempt(&state2).unwrap();
        assert!(!apply_attempt_path(&state2).unwrap().exists());
    }

    #[test]
    fn dropped_preparing_applier_leaves_marker_owned_stage_for_abort() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let state = dir.path().join("profile.snp");
        fs::create_dir(&base).unwrap();
        let entry = test_file_entry("file.txt", b"contents");
        let actions = vec![Action::Local(Change::Added(entry))];
        start_staged_apply_attempt("local", &state, &base, &actions, "attempt-1").unwrap();
        let mut applier = DetailApplier::new_staged_with_attempt_and_policy(
            base.clone(),
            actions,
            Vec::new(),
            state.clone(),
            "attempt-1".to_string(),
            None,
            ApplyOptions::default(),
        );
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            })
            .unwrap();
        applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBytes(b"partial".to_vec()),
            })
            .unwrap();
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        let parsed = parse_v2_apply_attempt(&marker).unwrap();
        let (stage_parent, _) = parsed.stage_parent.unwrap();
        let (stage_name, _) = parsed.stage.unwrap();

        drop(applier);

        assert!(stage_parent.join(stage_name).exists());
        abort_staged_apply_attempt(&state, "attempt-1").unwrap();
        assert!(!apply_attempt_path(&state).unwrap().exists());
        assert!(fs::read_dir(&base).unwrap().next().is_none());
    }
}
