use super::scan::{Change, ContentDigest, DirEntryWithMeta as Entry};
use color_eyre::eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc;
use std::thread;

use crate::actions::Action;
use crate::profile::{Ignore, Prune};
use crate::scan::location::{Location, Locations};

use crate::rustsync::{compare, compare_stream, restore_seek, signature, DeltaOp};
pub use crate::rustsync::{Delta, Signature};

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
static TEMP_OUTPUT_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        }
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
        if let Some(value) = get(ENV_DETAIL_BATCH_PAYLOAD_BYTES)
            .and_then(|value| value.parse().ok())
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

    if !path.components().any(|component| matches!(component, Component::Normal(_))) {
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
    validate_signature_window(signature.1.window).wrap_err_with(|| {
        format!(
            "invalid signature window for {}",
            signature.0.display()
        )
    })?;
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
    validate_relative_path(entry.path()).wrap_err_with(|| {
        format!(
            "invalid action entry path {}",
            entry.path().display()
        )
    })
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
    let parent = marker_path.parent().ok_or_else(|| eyre!(
        "apply recovery marker {} has no parent directory", marker_path.display()
    ))?;
    create_dir_all_durable(parent).wrap_err_with(|| format!(
        "unable to create apply recovery marker directory {}", parent.display()
    ))?;
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
        })
    {
        Ok(()) => sync_directory(parent).wrap_err_with(|| format!(
            "unable to sync apply recovery marker directory {}", parent.display()
        )),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(&marker_path).wrap_err_with(|| {
                format!(
                    "unable to read apply recovery marker {}",
                    marker_path.display()
                )
            })?;
            if existing == contents {
                let file = fs::OpenOptions::new().read(true).write(true).open(&marker_path)
                    .wrap_err_with(|| format!(
                        "unable to open existing apply recovery marker {}", marker_path.display()
                    ))?;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                file.sync_all().wrap_err_with(|| format!(
                    "unable to sync existing apply recovery marker {}", marker_path.display()
                ))?;
                sync_directory(parent).wrap_err_with(|| format!(
                    "unable to sync apply recovery marker directory {}", parent.display()
                ))
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
        .write_with_options(|file| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(contents.as_bytes())
        }, options)
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

fn record_committed_step(
    attempt_state: Option<&Path>,
    operation: &str,
    path: &Path,
) -> Result<()> {
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
            let parent = marker_path.parent().ok_or_else(|| eyre!(
                "apply recovery marker {} has no parent directory", marker_path.display()
            ))?;
            sync_directory(parent).wrap_err_with(|| format!(
                "unable to sync cleared apply recovery marker directory {}", parent.display()
            ))
        },
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
    while !current.try_exists().wrap_err_with(|| format!(
        "unable to check directory {}", current.display()
    ))? {
        missing.push(current.to_path_buf());
        current = current.parent().ok_or_else(|| eyre!(
            "directory {} has no existing ancestor", path.display()
        ))?;
    }
    fs::create_dir_all(path)
        .wrap_err_with(|| format!("unable to create directory {}", path.display()))?;
    for directory in missing.iter().rev() {
        let parent = directory.parent().ok_or_else(|| eyre!(
            "directory {} has no parent", directory.display()
        ))?;
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    let access = open_directory_for_access(path)
        .wrap_err_with(|| format!("unable to open directory for syncing {}", path.display()))?;
    verify_path_identity(path, &access, "directory being synced")?;
    match access.sync_all() {
        Ok(()) => return Ok(()),
        Err(error) if access_descriptor_needs_readable_sync(&error) => {}
        Err(error) => {
            return Err(error).wrap_err_with(|| format!("unable to sync directory {}", path.display()));
        }
    }

    let original_mode = access
        .metadata()
        .wrap_err_with(|| format!("unable to inspect directory for syncing {}", path.display()))?
        .permissions()
        .mode()
        & 0o7777;
    verify_path_identity(path, &access, "directory being synced")?;
    set_retained_directory_mode(&access, original_mode | 0o400, path).wrap_err_with(|| {
        format!("unable to temporarily make directory readable for syncing {}", path.display())
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
            format!("unable to restore directory mode after opening it for syncing {}", path.display())
        });
    }
    verify_same_directory_handles(&access, &readable, path, "directory being synced")?;
    verify_path_identity(path, &readable, "directory being synced")?;
    readable
        .sync_all()
        .wrap_err_with(|| format!("unable to sync directory {}", path.display()))
}

fn complete_apply_phase(base: &Path, actions: &[Action], attempt_state: Option<&Path>) -> Result<()> {
    // File publication syncs each private stage directory first. Parent directory
    // durability is intentionally batched here, before the caller saves state;
    // until these barriers complete, the durable apply marker remains authoritative.
    let metadata_synced_directories: HashSet<_> = actions
        .iter()
        .filter_map(applied_change)
        .filter_map(|change| match change {
            Change::Added(entry) | Change::Modified(_, entry) if entry.is_dir() => {
                Some(base.join(entry.path()))
            }
            _ => None,
        })
        .collect();
    let mut directories = HashSet::new();
    directories.insert(base.to_path_buf());
    for action in actions {
        let mut path = base.join(action.path());
        if path != base && !path.pop() {
            continue;
        }
        loop {
            if !metadata_synced_directories.contains(&path)
                && path.try_exists().wrap_err_with(|| format!(
                "unable to check affected path {}", path.display()
            ))?
                && fs::symlink_metadata(&path).wrap_err_with(|| format!(
                "unable to inspect affected path {}", path.display()
            ))?.is_dir() {
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
    if let Some(state_path) = attempt_state {
        let marker_path = apply_attempt_path(state_path)?;
        fs::OpenOptions::new().read(true).open(&marker_path)
            .and_then(|file| file.sync_all())
            .wrap_err_with(|| format!(
                "unable to sync accumulated apply recovery records {}", marker_path.display()
            ))?;
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
    let marker = parse_apply_attempt_marker(marker);
    let mut advice = if marker.phase.as_deref() == Some("state-save") {
        "Recovery: filesystem changes were applied, but Duet state may not have been saved on this side. Fix state-storage permissions if needed, inspect the listed paths if needed, then remove only this marker and rerun Duet before making unrelated changes."
            .to_string()
    } else {
        "Recovery: filesystem changes may have been partially applied on this side. Inspect the listed paths on both sides, fix any permission or filesystem problem, then remove only this marker and rerun Duet."
            .to_string()
    };

    advice.push_str(&format!(
        " Inspect this marker with `duet recover {}`. After inspection, remove it with `duet recover --clear {}` or manually with `rm {}`.",
        state_path.display(),
        state_path.display(),
        marker_path.display()
    ));
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
            });
        };

        use fnmatch_regex::glob_to_regex;
        let compile_patterns = |patterns: &[String], kind: &str| -> Result<Vec<(String, regex::Regex)>> {
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
        })
    }

    fn classify<'a>(&'a self, relative_path: &Path) -> RemovalBlockerKind<'a> {
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
    for entry in fs::read_dir(dirname)
        .wrap_err_with(|| format!("unable to preflight directory removal {}", dirname.display()))?
    {
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
    for entry in fs::read_dir(dirname)
        .wrap_err_with(|| format!("unable to preflight directory removal {}", dirname.display()))?
    {
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
    for entry in fs::read_dir(dirname)
        .wrap_err_with(|| format!("unable to preflight directory removal {}", dirname.display()))?
    {
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
                    remove_ignored_dir_all_same_device(
                        base,
                        &path,
                        base_dev,
                        policy,
                        attempt_state,
                    )
                        .wrap_err_with(|| {
                            format!("failed to prune ignored directory {}", path.display())
                        })?;
                } else {
                    fs::remove_file(&path).wrap_err_with(|| {
                        format!("failed to prune file {}", path.display())
                    })?;
                }
                record_committed_step(
                    attempt_state,
                    "prune-blocker",
                    &relative_path.to_path_buf(),
                )?;
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
            format!(
                "failed to check ignored prune path {}",
                temp_path.display()
            )
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
        return Err(eyre!(
            "unexpected signature for {}",
            extra.0.display()
        ));
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
                    validate_signature_window(signature_with_path.1.window).wrap_err_with(|| {
                        format!(
                            "invalid signature window for {}",
                            signature_with_path.0.display()
                        )
                    })?;
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
            return Err(eyre!(
                "unexpected signature for {}",
                extra.0.display()
            ));
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

struct TempOutput {
    final_path: PathBuf,
    parent_path: PathBuf,
    stage_dir: PathBuf,
    temp_path: PathBuf,
    final_name: std::ffi::CString,
    stage_name: std::ffi::CString,
    parent_directory: fs::File,
    stage_directory: fs::File,
    file: Option<fs::File>,
    parent_guard: Option<WritableDirGuard>,
}

impl TempOutput {
    fn new(final_path: PathBuf) -> Result<Self> {
        let parent = final_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent_path = parent.to_path_buf();
        let (parent_directory, parent_guard) = WritableDirGuard::new(parent)?;
        let final_name = path_component_cstring(
            final_path
                .file_name()
                .ok_or_else(|| eyre!("output path {} has no file name", final_path.display()))?,
            "output file name",
        )?;
        let (stage_dir, temp_path, stage_name, stage_directory, file) =
            create_temp_output_file(&final_path, &parent_directory)?;
        Ok(TempOutput {
            final_path,
            parent_path,
            stage_dir,
            temp_path,
            final_name,
            stage_name,
            parent_directory,
            stage_directory,
            file: Some(file),
            parent_guard,
        })
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
        let file_meta = self
            .file
            .as_ref()
            .ok_or_else(|| eyre!("temporary output is closed"))?
            .metadata()
            .wrap_err("failed to read open temporary file metadata")?;
        let path_stat = fstatat_nofollow(directory.as_raw_fd(), name)
            .wrap_err_with(|| format!("failed to read temporary path {}", path.display()))?;
        if path_stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || path_stat.st_dev as u64 != file_meta.dev()
            || path_stat.st_ino as u64 != file_meta.ino()
        {
            return Err(eyre!(
                "temporary path {} no longer refers to the open output file",
                path.display()
            ));
        }
        Ok(())
    }

    fn finish(mut self, entry: &Entry) -> Result<Entry> {
        let final_entry = self.prepare_metadata(entry)?;
        self.sync_all()?;
        self.verify_at_identity(&self.stage_directory, output_name(), &self.temp_path)?;
        // The stage entry must be durable before it is published. The affected
        // parent chain is synced once at apply-phase completion.
        self.sync_stage_directory()?;
        self.verify_parent_path_identity()?;
        cvt(unsafe {
            libc::renameat(
                self.stage_directory.as_raw_fd(),
                output_name().as_ptr(),
                self.parent_directory.as_raw_fd(),
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
        self.verify_parent_path_identity()?;
        self.verify_at_identity(&self.parent_directory, &self.final_name, &self.final_path)?;
        self.sync_stage_directory()?;
        self.remove_stage_dir()?;
        self.restore_parent()?;
        Ok(final_entry)
    }

    fn finish_without_replacing(mut self, description: &str, entry: &Entry) -> Result<Entry> {
        let final_entry = self.prepare_metadata(entry)?;
        self.sync_all()?;
        self.verify_at_identity(&self.stage_directory, output_name(), &self.temp_path)?;
        // The stage entry must be durable before it is published. The affected
        // parent chain is synced once at apply-phase completion.
        self.sync_stage_directory()?;
        self.verify_parent_path_identity()?;
        match cvt(unsafe {
            libc::linkat(
                self.stage_directory.as_raw_fd(),
                output_name().as_ptr(),
                self.parent_directory.as_raw_fd(),
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
        self.verify_parent_path_identity()?;
        self.verify_at_identity(&self.parent_directory, &self.final_name, &self.final_path)?;
        unlinkat(self.stage_directory.as_raw_fd(), output_name(), 0).wrap_err_with(|| {
            format!("failed to remove temporary file {}", self.temp_path.display())
        })?;
        self.sync_stage_directory()?;
        self.remove_stage_dir()?;
        self.restore_parent()?;
        Ok(final_entry)
    }

    #[cfg(test)]
    fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    fn stage_path(&self) -> &Path {
        &self.stage_dir
    }

    fn flush(&mut self) -> Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| eyre!("temporary output is closed"))?
            .flush()
            .wrap_err_with(|| {
                format!("failed to flush temporary file {}", self.temp_path.display())
            })
    }

    fn sync_all(&self) -> Result<()> {
        self.file.as_ref()
            .ok_or_else(|| eyre!("temporary output is closed"))?
            .sync_all()
            .wrap_err_with(|| format!(
                "failed to sync temporary file {}", self.temp_path.display()
            ))
    }

    fn sync_stage_directory(&self) -> Result<()> {
        self.stage_directory.sync_all().wrap_err_with(|| {
            format!(
                "failed to sync temporary directory {}",
                self.stage_dir.display()
            )
        })
    }

    fn verify_parent_path_identity(&self) -> Result<()> {
        verify_path_identity(
            &self.parent_path,
            &self.parent_directory,
            "output parent directory",
        )
    }

    fn restore_parent(&mut self) -> Result<()> {
        if let Some(guard) = self.parent_guard.take() {
            guard.restore()?;
        }
        Ok(())
    }

    fn remove_stage_dir(&mut self) -> Result<()> {
        verify_directory_at_identity(
            &self.parent_directory,
            &self.stage_name,
            &self.stage_directory,
            &self.stage_dir,
        )?;
        unlinkat(
            self.parent_directory.as_raw_fd(),
            &self.stage_name,
            libc::AT_REMOVEDIR,
        )
        .wrap_err_with(|| {
            format!(
                "failed to remove temporary directory {}",
                self.stage_dir.display()
            )
        })?;
        self.stage_dir.clear();
        Ok(())
    }
}

fn create_temp_output_file(
    final_path: &Path,
    parent_directory: &fs::File,
) -> Result<(PathBuf, PathBuf, std::ffi::CString, fs::File, fs::File)> {
    let parent = final_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for _ in 0..128 {
        let stage_component = format!(
            ".duet-stage-{}-{:016x}-{}",
            std::process::id(),
            temp_nonce(),
            TEMP_OUTPUT_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
        );
        let stage_name = path_component_cstring(stage_component.as_ref(), "stage directory name")?;
        let stage_dir = parent.join(&stage_component);
        match cvt(unsafe {
            libc::mkdirat(parent_directory.as_raw_fd(), stage_name.as_ptr(), 0o700)
        }) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).wrap_err_with(|| {
                    format!("failed to create temporary directory {}", stage_dir.display())
                });
            }
        }
        let created_stat = match fstatat_nofollow(parent_directory.as_raw_fd(), &stage_name) {
            Ok(stat) => stat,
            Err(error) => {
                if let Ok(directory) = openat_file(
                    parent_directory.as_raw_fd(),
                    &stage_name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                ) {
                    cleanup_stage_at(parent_directory, &stage_name, &directory);
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
            cleanup_unopened_stage_at(parent_directory, &stage_name, &created_stat);
            return Err(eyre!(
                "new temporary directory path {} was replaced before it could be opened",
                stage_dir.display()
            ));
        }
        let access_directory = match open_new_stage_for_access(
            parent_directory,
            &stage_name,
            &created_stat,
            &stage_dir,
        ) {
            Ok(directory) => directory,
            Err(error) => {
                cleanup_unopened_stage_at(parent_directory, &stage_name, &created_stat);
                return Err(error).wrap_err_with(|| {
                    format!("failed to retain temporary directory {}", stage_dir.display())
                });
            }
        };
        if let Err(error) = verify_retained_directory_at_identity(
            parent_directory,
            &stage_name,
            &access_directory,
            &created_stat,
            &stage_dir,
            "new temporary directory",
        ) {
            cleanup_stage_at(parent_directory, &stage_name, &access_directory);
            return Err(error);
        }
        if let Err(error) = normalize_stage_directory_mode(
            parent_directory,
            &stage_name,
            &access_directory,
            &created_stat,
            &stage_dir,
        ) {
            cleanup_stage_at(parent_directory, &stage_name, &access_directory);
            return Err(error);
        }
        let directory = match openat_file(
            parent_directory.as_raw_fd(),
            &stage_name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ) {
            Ok(directory) => directory,
            Err(e) => {
                cleanup_unopened_stage_at(parent_directory, &stage_name, &created_stat);
                return Err(e).wrap_err_with(|| {
                    format!("failed to open temporary directory {}", stage_dir.display())
                });
            }
        };
        let secure_result = (|| -> Result<()> {
            verify_directory_at_identity(
                parent_directory,
                &stage_name,
                &directory,
                &stage_dir,
            )?;
            let opened_meta = directory.metadata().wrap_err_with(|| {
                format!("failed to inspect temporary directory {}", stage_dir.display())
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
            cleanup_stage_at(parent_directory, &stage_name, &directory);
            return Err(error);
        }
        let temp_path = stage_dir.join("output");
        match openat_file(
            directory.as_raw_fd(),
            output_name(),
            libc::O_RDWR
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0o600,
        ) {
            Ok(file) => {
                if let Err(error) = file
                    .set_permissions(fs::Permissions::from_mode(0o600))
                    .wrap_err_with(|| {
                        format!(
                            "failed to normalize temporary file permissions {}",
                            temp_path.display()
                        )
                    })
                {
                    cleanup_stage_at(parent_directory, &stage_name, &directory);
                    return Err(error);
                }
                return Ok((stage_dir, temp_path, stage_name, directory, file));
            }
            Err(e) => {
                cleanup_stage_at(parent_directory, &stage_name, &directory);
                return Err(e).wrap_err_with(|| {
                    format!("failed to create temporary file {}", temp_path.display())
                });
            }
        }
    }

    Err(eyre!(
        "failed to create a unique temporary file next to {}",
        final_path.display()
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

fn output_name() -> &'static std::ffi::CStr {
    unsafe { std::ffi::CStr::from_bytes_with_nul_unchecked(b"output\0") }
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
        format!("failed to verify retained temporary directory {}", path.display())
    })?;
    let after = fstatat_nofollow(parent.as_raw_fd(), name)
        .wrap_err_with(|| format!("failed to verify temporary directory {}", path.display()))?;
    verify_stat_identity(created, &after, libc::S_IFDIR, path, "new temporary directory")?;
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
    let retained_meta = retained
        .metadata()
        .wrap_err_with(|| format!("failed to inspect retained {} {}", description, path.display()))?;
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
    let expected = directory.metadata().wrap_err_with(|| {
        format!("failed to inspect temporary directory {}", path.display())
    })?;
    let actual = fstatat_nofollow(parent.as_raw_fd(), name).wrap_err_with(|| {
        format!("failed to inspect temporary directory path {}", path.display())
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
    let expected = retained
        .metadata()
        .wrap_err_with(|| format!("failed to inspect retained {} {}", description, path.display()))?;
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
    let expected = expected
        .metadata()
        .wrap_err_with(|| format!("failed to inspect retained {} {}", description, path.display()))?;
    let actual = actual
        .metadata()
        .wrap_err_with(|| format!("failed to inspect reopened {} {}", description, path.display()))?;
    if !actual.is_dir() || actual.dev() != expected.dev() || actual.ino() != expected.ino() {
        return Err(eyre!(
            "{} path {} no longer refers to the retained directory",
            description,
            path.display()
        ));
    }
    Ok(())
}

fn cleanup_stage_at(
    parent: &fs::File,
    stage_name: &std::ffi::CStr,
    directory: &fs::File,
) {
    if directory
        .metadata()
        .map(|meta| meta.uid() != unsafe { libc::geteuid() })
        .unwrap_or(true)
    {
        return;
    }
    let _ = unlinkat(directory.as_raw_fd(), output_name(), 0);
    let _ = directory.sync_all();
    if verify_directory_at_identity(
        parent,
        stage_name,
        directory,
        Path::new("stage directory"),
    )
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
        let path_meta = fs::symlink_metadata(path).wrap_err_with(|| {
            format!("failed to read directory metadata for {}", path.display())
        })?;
        if !path_meta.is_dir() {
            return Err(eyre!("output parent {} is not a directory", path.display()));
        }
        let original_mode = path_meta.permissions().mode();
        if owner_write_execute(original_mode) {
            let directory = open_directory_for_access(path)?;
            verify_path_identity(path, &directory, "output parent directory")?;
            return Ok((directory, None));
        }
        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .wrap_err_with(|| format!("failed to open directory {}", path.display()))?;
        verify_path_identity(path, &directory, "output parent directory")?;
        let mut perms = path_meta.permissions();
        perms.set_mode(original_mode | 0o700);
        directory.set_permissions(perms).wrap_err_with(|| {
            format!(
                "failed to make directory writable for sync {}",
                path.display()
            )
        })?;
        if let Err(error) = verify_path_identity(path, &directory, "output parent directory") {
            let _ = directory.set_permissions(fs::Permissions::from_mode(original_mode));
            return Err(error);
        }
        let guard_directory = match directory.try_clone() {
            Ok(directory) => directory,
            Err(error) => {
                let _ = directory.set_permissions(fs::Permissions::from_mode(original_mode));
                return Err(error).wrap_err_with(|| {
                    format!("failed to retain directory handle for {}", path.display())
                });
            }
        };
        Ok((directory, Some(Self {
            path: path.to_path_buf(),
            directory: guard_directory,
            original_mode,
        })))
    }

    fn restore(mut self) -> Result<()> {
        self.directory.set_permissions(fs::Permissions::from_mode(self.original_mode))
            .wrap_err_with(|| format!(
                "failed to restore directory permissions after sync {}", self.path.display()
            ))?;
        verify_path_identity(&self.path, &self.directory, "output parent directory")?;
        self.path.clear();
        Ok(())
    }
}

fn open_directory_for_access(path: &Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(directory_access_flag() | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
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

fn open_new_stage_for_access(
    parent: &fs::File,
    name: &std::ffi::CStr,
    created: &libc::stat,
    path: &Path,
) -> Result<fs::File> {
    match open_directory_at_for_access(parent, name) {
        Ok(directory) => return Ok(directory),
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        Err(error) => return Err(error.into()),
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let before = fstatat_nofollow(parent.as_raw_fd(), name)
            .wrap_err_with(|| format!("failed to inspect new temporary directory {}", path.display()))?;
        verify_stat_identity(created, &before, libc::S_IFDIR, path, "new temporary directory")?;
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
        let after = fstatat_nofollow(parent.as_raw_fd(), name)
            .wrap_err_with(|| format!("failed to verify new temporary directory {}", path.display()))?;
        verify_stat_identity(created, &after, libc::S_IFDIR, path, "new temporary directory")?;
        return open_directory_at_for_access(parent, name).wrap_err_with(|| {
            format!("failed to retain new temporary directory {} after securing it", path.display())
        });
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unreachable!("O_PATH stage open either succeeds or returns directly")
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
            let _ = self
                .directory
                .set_permissions(fs::Permissions::from_mode(self.original_mode));
        }
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        let _ = unlinkat(self.stage_directory.as_raw_fd(), output_name(), 0);
        let _ = self.stage_directory.sync_all();
        if !self.stage_dir.as_os_str().is_empty() {
            if verify_directory_at_identity(
                &self.parent_directory,
                &self.stage_name,
                &self.stage_directory,
                &self.stage_dir,
            )
            .is_ok()
            {
                let _ = unlinkat(
                    self.parent_directory.as_raw_fd(),
                    &self.stage_name,
                    libc::AT_REMOVEDIR,
                );
            }
        }
    }
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
        return Err(eyre!("new directory path {} is not a directory", path.display()));
    }
    let private_mode = 0o700 | (meta.mode() & 0o2000);
    directory
        .set_permissions(fs::Permissions::from_mode(private_mode))
        .wrap_err_with(|| format!("failed to normalize directory permissions {}", path.display()))
}

enum ApplyState {
    File {
        action_index: usize,
        output: TempOutput,
    },
    Diff {
        action_index: usize,
        source: fs::File,
        output: TempOutput,
    },
}

pub struct DetailApplier {
    base: PathBuf,
    actions: Vec<Action>,
    all_old: Vec<Entry>,
    attempt_state: Option<PathBuf>,
    recorder: ApplyRecorder,
    scan_policy: Option<ScanPolicy>,
    apply_options: ApplyOptions,
    old_index: usize,
    action_index: usize,
    new_entries: Vec<Entry>,
    state: Option<ApplyState>,
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

    pub fn new_with_attempt_and_policy(
        base: PathBuf,
        actions: Vec<Action>,
        all_old: Vec<Entry>,
        attempt_state: Option<PathBuf>,
        scan_policy: Option<ScanPolicy>,
        apply_options: ApplyOptions,
    ) -> Self {
        DetailApplier {
            base,
            actions,
            all_old,
            recorder: ApplyRecorder::new(attempt_state.clone()),
            attempt_state,
            scan_policy,
            apply_options,
            old_index: 0,
            action_index: 0,
            new_entries: Vec::new(),
            state: None,
        }
    }

    pub fn apply_frame(&mut self, frame: DetailFrame) -> Result<()> {
        let frame_index = frame.action_index as usize;
        if frame_index >= self.actions.len() {
            return Err(eyre!(
                "detail frame references missing action {}",
                frame_index
            ));
        }

        match &mut self.state {
            Some(ApplyState::File {
                action_index,
                output,
            }) => {
                if *action_index != frame_index {
                    return Err(eyre!(
                        "detail frame for action {} arrived while applying action {}",
                        frame_index,
                        action_index
                    ));
                }
                match frame.payload {
                    DetailPayload::FileBytes(bytes) => output
                        .file
                        .as_mut()
                        .ok_or_else(|| eyre!("temporary output is closed"))?
                        .write_all(&bytes)?,
                    DetailPayload::FileEnd => self.finish_file_detail()?,
                    _ => return Err(eyre!("unexpected file detail frame")),
                }
                return Ok(());
            }
            Some(ApplyState::Diff {
                action_index,
                source,
                output,
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
                        let output_file = output
                            .file
                            .as_mut()
                            .ok_or_else(|| eyre!("temporary output is closed"))?;
                        copy_from_source(source, output_file, offset, len)?;
                    }
                    DetailPayload::DiffBytes(bytes) => output
                        .file
                        .as_mut()
                        .ok_or_else(|| eyre!("temporary output is closed"))?
                        .write_all(&bytes)?,
                    DetailPayload::DiffEnd => self.finish_file_detail()?,
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
            DetailPayload::FileBegin | DetailPayload::DiffBegin => Err(eyre!(
                "unexpected detail kind for action {}",
                frame_index
            )),
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

    pub fn finish(mut self) -> Result<Vec<Entry>> {
        if self.state.is_some() {
            return Err(eyre!("detail stream ended with an unfinished file"));
        }
        self.advance_to_action(self.actions.len())?;
        self.apply_directory_second_pass()?;

        for e in self.all_old.iter().skip(self.old_index) {
            self.new_entries.push(e.clone());
        }
        self.new_entries.sort();
        complete_apply_phase(&self.base, &self.actions, self.attempt_state.as_deref())?;
        Ok(self.new_entries)
    }

    fn advance_to_action(&mut self, target_index: usize) -> Result<()> {
        while self.action_index < target_index {
            if apply_detail_kind(&self.actions[self.action_index]).is_some() {
                return Err(eyre!(
                    "missing detail frames for action {}",
                    self.action_index
                ));
            }
            self.apply_action_without_detail(self.action_index)?;
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
        self.prepare_action(action_index);
        let filename = detail_filename(&self.base, &self.actions[action_index])?;
        ensure_parent_directory(&filename)?;
        let output = TempOutput::new(filename)?;
        self.recorder.record_staged_file(output.stage_path())?;
        self.state = Some(ApplyState::File {
            action_index,
            output,
        });
        Ok(())
    }

    fn begin_diff_detail(&mut self, action_index: usize) -> Result<()> {
        self.prepare_action(action_index);
        let filename = detail_filename(&self.base, &self.actions[action_index])?;
        let old_entry = match &self.actions[action_index] {
            Action::Local(Change::Modified(e, _))
            | Action::ResolvedLocal((_, _), Change::Modified(e, _)) => e,
            _ => return Err(eyre!("diff detail began for non-diff action")),
        };
        verify_file_matches_entry(&filename, old_entry, "diff source")?;
        let source = fs::File::open(&filename)?;
        let output = TempOutput::new(filename)?;
        self.recorder.record_staged_file(output.stage_path())?;
        self.state = Some(ApplyState::Diff {
            action_index,
            source,
            output,
        });
        Ok(())
    }

    fn finish_file_detail(&mut self) -> Result<()> {
        let state = self
            .state
            .take()
            .ok_or_else(|| eyre!("no file detail in progress"))?;
        let (action_index, mut output) = match state {
            ApplyState::File {
                action_index,
                output,
            } => (action_index, output),
            ApplyState::Diff {
                action_index,
                output,
                ..
            } => (action_index, output),
        };
        let entry = match &self.actions[action_index] {
            Action::Local(Change::Added(e))
            | Action::ResolvedLocal((_, _), Change::Added(e))
            | Action::Local(Change::Modified(_, e))
            | Action::ResolvedLocal((_, _), Change::Modified(_, e)) => e,
            _ => return Err(eyre!("file detail finished for non-file action")),
        };
        let filename = safe_join(&self.base, entry.path())?;
        output.verify_contents(entry, "file output")?;
        let final_entry = if let Some(old_entry) = replacement_old_entry(&self.actions[action_index]) {
            verify_current_matches_entry(&filename, old_entry, "rename target")?;
            output.finish(entry)?
        } else {
            output.finish_without_replacing("rename target", entry)?
        };
        self.recorder
            .record_committed_step("rename-file", entry.path())?;
        self.new_entries.push(final_entry);
        self.recorder
            .record_committed_step("update-metadata", entry.path())?;
        self.recorder
            .record_committed_action(&self.actions[action_index])?;
        self.action_index = action_index + 1;
        Ok(())
    }

    fn apply_directory_second_pass(&mut self) -> Result<()> {
        let removal_policy = RemovalBlockerPolicy::new(self.scan_policy.as_ref(), self.apply_options)?;
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

fn copy_from_source(
    source: &mut fs::File,
    output: &mut fs::File,
    offset: u64,
    len: u64,
) -> Result<()> {
    source.seek(SeekFrom::Start(offset))?;
    let mut remaining = len;
    let mut buf = vec![0; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let want = std::cmp::min(remaining as usize, buf.len());
        let n = source.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
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
    validate_actions(actions)?;
    let removal_policy = RemovalBlockerPolicy::new(scan_policy, apply_options)?;
    log::debug!("details.len() = {}", details.len());
    let mut details_iter = details.iter();
    let mut new_entries: Vec<Entry> = Vec::new();
    let mut old_iter = all_old.iter().peekable();
    let mut leftover_details: Vec<&ChangeDetails> = Vec::new();

    for action in actions {
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
                            new_entries.push(create_file(&filename, detail, e, attempt_state)?);
                            record_committed_step(attempt_state, "update-metadata", e.path())?;
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
                                            new_entries.push(update_file_with_diff(
                                                &filename,
                                                e1,
                                                e2,
                                                delta,
                                                attempt_state,
                                            )?);
                                        }
                                        _ => {
                                            return Err(eyre!(
                                            "mismatch when adding {}, expected Diff, but not found",
                                            e1.path().display()
                                        ))
                                        }
                                    }
                                } else {
                                    verify_current_matches_entry(
                                        &filename,
                                        e1,
                                        "metadata target",
                                    )?;
                                }
                                if e1.same_contents(e2) {
                                    new_entries.push(update_meta(&filename, e2)?);
                                }
                                record_committed_step(
                                    attempt_state,
                                    "update-metadata",
                                    e2.path(),
                                )?;
                            } else {
                                // e2 not a file
                                // remove the file
                                verify_current_matches_entry(&filename, e1, "replace target")?;
                                fs::remove_file(&filename).wrap_err_with(|| {
                                    format!("failed to remove file {}", filename.display())
                                })?;
                                record_committed_step(attempt_state, "remove-file", e1.path())?;
                                if let Some(p) = e2.target() {
                                    std::os::unix::fs::symlink(p, &filename).wrap_err_with(|| {
                                        format!(
                                            "failed to create symlink {} -> {}",
                                            filename.display(),
                                            p.display()
                                        )
                                    })?;
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
                                new_entries.push(create_file(
                                    &filename,
                                    detail,
                                    e2,
                                    attempt_state,
                                )?);
                                record_committed_step(
                                    attempt_state,
                                    "update-metadata",
                                    e2.path(),
                                )?;
                            } else if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &filename).wrap_err_with(|| {
                                    format!(
                                        "failed to create symlink {} -> {}",
                                        filename.display(),
                                        p.display()
                                    )
                                })?;
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
                            return Err(eyre!(
                                "unsupported old entry for {}",
                                e1.path().display()
                            ));
                        }
                    }
                }
                if !change.is_dir() {
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

    // second pass, in reverse order, to remove directories and update their metadata
    let mut details_iter = leftover_details.iter().rev();
    let removed_paths = removed_destination_paths(actions);
    for action in actions.iter().rev() {
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
                        let mut prepared_entry = None;
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
                                record_committed_step(
                                    attempt_state,
                                    "create-symlink",
                                    e2.path(),
                                )?;
                            } else if e2.is_file() {
                                let detail = details_iter.next().ok_or_else(|| {
                                    eyre!("missing detail for {}", e2.path().display())
                                })?;
                                prepared_entry =
                                    Some(create_file(&dirname, detail, e2, attempt_state)?);
                            }
                        }
                        if e1.is_dir() && e2.is_dir() {
                            verify_current_matches_entry(&dirname, e1, "metadata target")?;
                        }
                        new_entries.push(match prepared_entry {
                            Some(entry) => entry,
                            None => update_meta(&dirname, e2)?,
                        });
                        record_committed_step(attempt_state, "update-metadata", e2.path())?;
                    }
                }
                record_committed_action(attempt_state, action)?;
            }
            _ => {}
        }
    }

    // copy remaining entries from all_old
    for e in old_iter {
        new_entries.push(e.clone());
    }
    new_entries.sort(); // directory -> file or symlink will be out of order, so need to sort them

    std::mem::swap(all_old, &mut new_entries);

    complete_apply_phase(base, actions, attempt_state)?;

    Ok(())
}

fn create_file(
    filename: &Path,
    detail: &ChangeDetails,
    entry: &Entry,
    attempt_state: Option<&Path>,
) -> Result<Entry> {
    match detail {
        ChangeDetails::Contents(v) => create_file_with_contents(filename, v, entry, attempt_state),
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
    attempt_state: Option<&Path>,
) -> Result<Entry> {
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
            return Err(eyre!("file detail for {} strong digest mismatch: expected {}, got {}", entry.path().display(), expected, actual));
        }
    } else {
        let checksum = adler32::adler32(data)
            .wrap_err_with(|| format!("failed to checksum legacy detail for {}", entry.path().display()))?;
        if checksum != entry.checksum() {
            return Err(eyre!("file detail for {} legacy checksum mismatch: expected {}, got {}", entry.path().display(), entry.checksum(), checksum));
        }
    }

    ensure_parent_directory(filename)?;
    let mut output = TempOutput::new(filename.to_path_buf())?;
    record_staged_file(attempt_state, output.stage_path())?;
    output
        .file
        .as_mut()
        .ok_or_else(|| eyre!("temporary output is closed"))?
        .write_all(data)
        .wrap_err_with(|| format!("failed to write temporary file for {}", filename.display()))?;
    output.verify_contents(entry, "file output")?;
    let final_entry = output.finish_without_replacing("rename target", entry)?;
    record_committed_step(attempt_state, "rename-file", filename)?;
    Ok(final_entry)
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
            return Err(eyre!("{} {} strong digest mismatch: expected {}, got {}", description, entry.path().display(), expected, actual));
        }
    } else {
        let checksum = adler32::adler32(file)
            .wrap_err_with(|| format!("failed to checksum legacy file {}", filename.display()))?;
        if checksum != entry.checksum() {
            return Err(eyre!("{} {} legacy checksum mismatch: expected {}, got {}", description, entry.path().display(), entry.checksum(), checksum));
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
    attempt_state: Option<&Path>,
) -> Result<Entry> {
    validate_delta(delta)?;
    verify_file_matches_entry(filename, old_entry, "diff source")?;
    let source = fs::File::open(filename)
        .wrap_err_with(|| format!("failed to open file {}", filename.display()))?;
    let mut output = TempOutput::new(filename.to_path_buf())?;
    record_staged_file(attempt_state, output.stage_path())?;
    let output_file = output
        .file
        .as_mut()
        .ok_or_else(|| eyre!("temporary output is closed"))?;
    restore_seek(output_file, source, vec![0; delta.window], delta)
        .wrap_err_with(|| format!("failed to restore diff for {}", filename.display()))?;
    output.verify_contents(new_entry, "diff output")?;
    verify_current_matches_entry(filename, old_entry, "rename target")?;
    let final_entry = output.finish(new_entry)?;
    record_committed_step(attempt_state, "rename-file", filename)?;
    Ok(final_entry)
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
                return Err(error)
                    .wrap_err_with(|| format!("failed to open metadata target {}", path.display()));
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
        assert_eq!(adler32::adler32(&expected[..]).unwrap(), adler32::adler32(&collision[..]).unwrap());
        std::fs::write(dir.path().join("file"), collision).unwrap();
        let mut entry = test_file_entry("file", &expected);
        entry.set_digest(Some(content_digest(&expected)));

        let error = verify_file_matches_entry(&dir.path().join("file"), &entry, "target").unwrap_err();

        assert!(error.to_string().contains("strong digest mismatch"), "{}", error);
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & SYNCED_MODE_MASK
    }

    fn synced_existing_file_entry(base: &Path, path: &str, contents: &[u8]) -> Entry {
        let filename = base.join(path);
        fs::write(&filename, contents).unwrap();
        update_meta(&filename, &test_file_entry(path, contents)).unwrap()
    }

    fn synced_existing_dir_entry(base: &Path, path: &str) -> Entry {
        update_meta(&base.join(path), &Entry::test_dir(PathBuf::from(path))).unwrap()
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

        complete_apply_phase(base.path(), &actions, None).unwrap();

        assert_eq!(mode(&directory_path), 0o300);
        fs::set_permissions(&directory_path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn temp_output_is_private_and_readable_through_its_handle() {
        let dir = tempfile::tempdir().unwrap();
        let mut output = TempOutput::new(dir.path().join("out.txt")).unwrap();

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
        let mut output = TempOutput::new(final_path.clone()).unwrap();
        let stage_dir = output.temp_path().parent().unwrap().to_path_buf();
        output.file.as_mut().unwrap().write_all(b"public").unwrap();
        let entry = test_file_entry_with_mode("out.txt", b"public", 0o644);

        output.prepare_metadata(&entry).unwrap();

        assert_eq!(mode(output.temp_path()), 0o644);
        assert_eq!(mode(&stage_dir) & 0o777, 0o700);
        output.finish(&entry).unwrap();
        assert_eq!(mode(&final_path), 0o644);
        assert!(!stage_dir.exists());
    }

    #[test]
    fn temp_output_drop_removes_stage_directory() {
        let dir = tempfile::tempdir().unwrap();
        let stage_dir = {
            let output = TempOutput::new(dir.path().join("out.txt")).unwrap();
            output.stage_path().to_path_buf()
        };

        assert!(!stage_dir.exists());
    }

    #[test]
    fn temp_output_create_new_does_not_clobber_destination() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        fs::write(&final_path, b"existing").unwrap();
        let mut output = TempOutput::new(final_path.clone()).unwrap();
        let stage_dir = output.stage_path().to_path_buf();
        output.file.as_mut().unwrap().write_all(b"new").unwrap();
        let entry = test_file_entry("out.txt", b"new");

        let error = output
            .finish_without_replacing("rename target", &entry)
            .unwrap_err();

        assert!(error.to_string().contains("already exists"), "{}", error);
        assert_eq!(fs::read(&final_path).unwrap(), b"existing");
        assert!(!stage_dir.exists());
    }

    #[test]
    fn temp_output_verifies_mode_zero_final_inode_relative_to_parent() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        let mut output = TempOutput::new(final_path.clone()).unwrap();
        output.file.as_mut().unwrap().write_all(b"private").unwrap();
        let entry = test_file_entry_with_mode("out.txt", b"private", 0o000);

        output.finish(&entry).unwrap();

        assert_eq!(mode(&final_path), 0o000);
    }

    #[test]
    fn temp_output_supports_writable_unreadable_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).unwrap();

        let result = (|| -> Result<()> {
            let final_path = parent.join("out.txt");
            let mut output = TempOutput::new(final_path.clone())?;
            output.file.as_mut().unwrap().write_all(b"contents")?;
            output.finish(&test_file_entry("out.txt", b"contents"))?;
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
        let retained = open_new_stage_for_access(
            &parent,
            &name,
            &created,
            &dir.path().join("stage"),
        )
        .unwrap();

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

        let result = complete_apply_phase(base.path(), &actions, None);

        assert_eq!(mode(&parent), 0o300);
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
        let mut applier = DetailApplier::new_with_attempt(
            base.clone(),
            actions,
            Vec::new(),
            Some(state.clone()),
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
                payload: DetailPayload::FileBytes(b"contents".to_vec()),
            })
            .unwrap();
        fs::rename(&parent, &moved_parent).unwrap();
        fs::create_dir(&parent).unwrap();

        let error = applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileEnd,
            })
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("output parent directory path"),
            "{}",
            error
        );
        assert!(!parent.join("out.txt").exists());
        assert!(!moved_parent.join("out.txt").exists());
        let marker = fs::read_to_string(apply_attempt_path(&state).unwrap()).unwrap();
        assert!(marker.contains("staged-file: "), "{}", marker);
        assert!(!marker.contains("committed-step: rename-file"), "{}", marker);
    }

    #[test]
    fn temp_output_does_not_follow_swapped_stage_path() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        let mut output = TempOutput::new(final_path.clone()).unwrap();
        let stage_dir = output.stage_path().to_path_buf();
        let moved_stage = dir.path().join("moved-stage");
        output.file.as_mut().unwrap().write_all(b"retained").unwrap();
        fs::rename(&stage_dir, &moved_stage).unwrap();
        fs::create_dir(&stage_dir).unwrap();
        fs::write(stage_dir.join("output"), b"substitute").unwrap();
        let entry = test_file_entry("out.txt", b"retained");

        let error = output.finish(&entry).unwrap_err();

        assert!(
            error.to_string().contains("no longer refers to the retained directory"),
            "{}",
            error
        );
        assert_eq!(fs::read(&final_path).unwrap(), b"retained");
        assert_eq!(fs::read(stage_dir.join("output")).unwrap(), b"substitute");
        fs::remove_dir(&moved_stage).unwrap();
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

        let error = verify_file_matches_entry(
            &dir.path().join("link.txt"),
            &entry,
            "verification target",
        )
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
            assert_eq!(mode(&base.join(entry.path())), requested_modes[index]);
            assert_eq!(fs::metadata(base.join(entry.path())).unwrap().mtime(), entry.mtime());
        }

        let final_entries = applier.finish().unwrap();
        assert_eq!(final_entries.len(), entries.len());
        for final_entry in final_entries {
            let metadata = fs::metadata(base.join(final_entry.path())).unwrap();
            assert_eq!(final_entry.ino(), metadata.ino());
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

        assert_eq!(mode(&base.join("file.txt")), 0o400);
        assert_eq!(fs::metadata(base.join("file.txt")).unwrap().mtime(), 0);
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

        apply_detailed_changes(
            &replacement_base,
            &actions,
            &details,
            &mut all_old,
            None,
        )
        .unwrap();
        assert_eq!(mode(&replacement_base.join("path")), 0o400);
        assert_eq!(fs::metadata(replacement_base.join("path")).unwrap().mtime(), 0);
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

        apply_detailed_changes(
            &base,
            &actions,
            &Vec::new(),
            &mut all_old,
            None,
        )
        .unwrap();

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
        assert_eq!(fs::metadata(&dirname).unwrap().permissions().mode() & 0o777, 0o700);
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
        let error = applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::DiffEnd,
            })
            .unwrap_err()
            .to_string();

        assert!(error.contains("rename target"), "{}", error);
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
        let error = applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileEnd,
            })
            .unwrap_err()
            .to_string();

        assert!(error.contains("rename target"), "{}", error);
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
        let error = applier
            .apply_frame(DetailFrame {
                action_index: 0,
                payload: DetailPayload::FileBegin,
            })
            .unwrap_err()
            .to_string();

        assert!(error.contains("already processed"), "{}", error);
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
        assert_eq!(tuning.detail_batch_payload_bytes, MAX_DETAIL_BATCH_PAYLOAD_BYTES);
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

        let output = TempOutput::new(final_path.clone()).unwrap();
        let temp_name = output.temp_path.file_name().unwrap().to_string_lossy();

        assert!(temp_name.len() < 64, "temp name was {}", temp_name);
        assert!(output.temp_path.exists());

        output
            .finish(&test_file_entry(
                final_path.file_name().unwrap().to_str().unwrap(),
                b"",
            ))
            .unwrap();
        assert!(final_path.exists());
    }

    #[test]
    fn temp_output_does_not_clobber_predictable_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("out.txt");
        let old_predictable = dir.path().join(format!(
            ".duet-part-{}-0",
            std::process::id()
        ));
        fs::write(&old_predictable, b"do not touch").unwrap();

        let output = TempOutput::new(final_path).unwrap();

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

        let error = preflight_apply_with_policy(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions::default(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("ignored child"), "{}", error);
        assert!(error.contains("__pycache__"), "{}", error);
        assert!(error.contains("--prune-ignored"), "{}", error);

        let report = preflight_apply_report(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions::default(),
        )
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

        let report = preflight_apply_report(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions::default(),
        )
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

        let error = preflight_apply_with_policy(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions::default(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("excluded child"), "{}", error);
        assert!(error.contains("outside the sync selection"), "{}", error);

        let report = preflight_apply_report(
            &base,
            &actions,
            Some(&policy),
            ApplyOptions::default(),
        )
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

        let report = preflight_apply_report(&base, &actions, None, ApplyOptions::default())
            .unwrap();

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
            PathBuf::from("a.txt"), 0,
        )))];

        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        let marker_path = apply_attempt_path(&state).unwrap();
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o644)).unwrap();
        start_apply_attempt("local", &state, &base, &actions, None).unwrap();
        assert_eq!(fs::metadata(&marker_path).unwrap().permissions().mode() & 0o777, 0o600);
        record_staged_file(Some(&state), Path::new(".duet-part-test")).unwrap();
        record_committed_step(Some(&state), "rename-file", Path::new("a.txt")).unwrap();
        mark_apply_attempt_state_save("local", &state, &base, &actions, None).unwrap();

        let marker = fs::read_to_string(&marker_path).unwrap();
        assert!(marker.contains("phase: state-save"), "{}", marker);
        assert!(marker.contains("staged-file: .duet-part-test"), "{}", marker);
        assert!(marker.contains("committed-step: rename-file a.txt"), "{}", marker);
        assert_eq!(fs::metadata(&marker_path).unwrap().permissions().mode() & 0o777, 0o600);
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
}
