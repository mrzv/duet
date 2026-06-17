use super::scan::{Change, DirEntryWithMeta as Entry};
use color_eyre::eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc;
use std::thread;

use crate::actions::Action;

use crate::rustsync::{compare, compare_stream, restore_seek, signature, DeltaOp};
pub use crate::rustsync::{Delta, Signature};

const WINDOW: usize = 1024; // TODO: figure out appropriate window size
const SYNCED_MODE_MASK: u32 = 0o7777;
static TEMP_OUTPUT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureWithPath(PathBuf, Signature);

pub fn get_signatures(base: &PathBuf, actions: &Vec<Action>) -> Result<Vec<SignatureWithPath>> {
    let mut signatures: Vec<SignatureWithPath> = Vec::new();
    for action in actions {
        match action {
            Action::Local(Change::Modified(e1, e2))
            | Action::ResolvedLocal((_, _), Change::Modified(e1, e2)) => {
                if e1.is_file() && e2.is_file() && !e1.same_contents(&e2) {
                    let f = fs::File::open(base.join(e1.path()))?;
                    let block = [0; WINDOW];
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

        !matches!(change, Change::Modified(old, new) if old.is_dir() && new.is_file())
    })
}

pub fn preflight_apply(base: &PathBuf, actions: &Vec<Action>) -> Result<()> {
    preflight_source_reads(base, actions)?;
    preflight_removed_directories(base, actions)?;

    let readonly_metadata_changes = readonly_directory_metadata_changes(actions);
    let planned_directories = planned_destination_directories(actions);
    for target in apply_metadata_targets(actions) {
        let target_path = base.join(&target);
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
        let parent_path = base.join(parent);
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
    let recovery_advice = apply_attempt_recovery_advice(&marker);
    Ok(Some(format!(
        "previous Duet apply attempt did not finish: {}\n{}\n{}",
        marker_path.display(),
        marker.trim_end(),
        recovery_advice
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
    if let Some(parent) = marker_path.parent() {
        fs::create_dir_all(parent).wrap_err_with(|| {
            format!(
                "unable to create apply recovery marker directory {}",
                parent.display()
            )
        })?;
    }
    let contents = apply_attempt_contents(side, state_path, base, "apply", actions, attempt_id);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_path)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(&marker_path).wrap_err_with(|| {
                format!(
                    "unable to read apply recovery marker {}",
                    marker_path.display()
                )
            })?;
            if existing == contents {
                Ok(())
            } else {
                Err(eyre!(
                    "previous Duet apply attempt did not finish: {}\n{}\n{}",
                    marker_path.display(),
                    existing.trim_end(),
                    apply_attempt_recovery_advice(&existing)
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
    let committed_operations = committed_operations_from_marker(&existing);
    let committed_steps = committed_steps_from_marker(&existing);
    let mut contents =
        apply_attempt_contents(side, state_path, base, "state-save", actions, attempt_id);
    for operation in committed_operations {
        contents.push_str("committed-operation: ");
        contents.push_str(&operation);
        contents.push('\n');
    }
    for step in committed_steps {
        contents.push_str("committed-step: ");
        contents.push_str(&step);
        contents.push('\n');
    }
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&marker_path)
        .and_then(|mut file| file.write_all(contents.as_bytes()))
        .wrap_err_with(|| {
            format!(
                "unable to update apply recovery marker {}",
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
                "unable to record staged file in apply recovery marker {}",
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
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).wrap_err_with(|| {
            format!(
                "unable to remove apply recovery marker {}",
                marker_path.display()
            )
        }),
    }
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
    staged_files: Vec<String>,
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
            parsed.staged_files.push(path.to_string());
        } else if let Some(operation) = line.strip_prefix("committed-operation: ") {
            parsed.committed_operations.push(operation.to_string());
        } else if let Some(step) = line.strip_prefix("committed-step: ") {
            parsed.committed_steps.push(step.to_string());
        }
    }
    parsed
}

fn committed_operations_from_marker(marker: &str) -> Vec<String> {
    marker
        .lines()
        .filter_map(|line| line.strip_prefix("committed-operation: "))
        .map(ToString::to_string)
        .collect()
}

fn committed_steps_from_marker(marker: &str) -> Vec<String> {
    marker
        .lines()
        .filter_map(|line| line.strip_prefix("committed-step: "))
        .map(ToString::to_string)
        .collect()
}

fn apply_attempt_recovery_advice(marker: &str) -> String {
    let marker = parse_apply_attempt_marker(marker);
    let mut advice = if marker.phase.as_deref() == Some("state-save") {
        "Recovery: filesystem changes were applied, but Duet state may not have been saved on this side. Fix state-storage permissions if needed, inspect the listed paths if needed, then remove this marker and rerun Duet before making unrelated changes."
            .to_string()
    } else {
        "Recovery: filesystem changes may have been partially applied on this side. Inspect the listed paths on both sides, fix any permission or filesystem problem, then remove this marker and rerun Duet."
            .to_string()
    };

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
        advice.push_str(
            " The marker records committed operations; inspect those paths first before removing the marker.",
        );
    }
    if !marker.committed_steps.is_empty() {
        advice.push_str(
            " The marker records committed apply steps; inspect those step paths before removing the marker.",
        );
    }
    if !marker.staged_files.is_empty() {
        advice.push_str(
            " The marker lists staged temporary files that may be safe to remove after inspection if they were not renamed into place.",
        );
    }
    if !marker.unstaged_operations.is_empty() {
        advice.push_str(
            " The marker lists unstaged operations that commit directly; inspect those paths for partial changes.",
        );
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

fn preflight_removed_directories(base: &Path, actions: &Vec<Action>) -> Result<()> {
    let removed_paths = removed_destination_paths(actions);
    for path in removed_paths.iter() {
        let dirname = base.join(path);
        if dirname.is_dir() {
            preflight_removed_directory_contents(base, &dirname, &removed_paths)?;
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

fn preflight_removed_directory_contents(
    base: &Path,
    dirname: &Path,
    removed_paths: &HashSet<PathBuf>,
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
            return Err(eyre!(
                "destination directory {} is not empty; unexpected child {} would prevent removal",
                dirname.display(),
                path.display()
            ));
        }
        if entry
            .file_type()
            .wrap_err_with(|| format!("unable to preflight directory entry {}", path.display()))?
            .is_dir()
        {
            preflight_removed_directory_contents(base, &path, removed_paths)?;
        }
    }
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
    let filename = base.join(path);
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
                            let v = fs::read(base.join(e.path()))?;
                            details.push(ChangeDetails::Contents(v));
                        }
                    }
                    Change::Modified(e1, e2) => {
                        if e1.is_file() && e2.is_file() && !e1.same_contents(&e2) {
                            let block = [0; WINDOW];
                            let f = fs::File::open(base.join(e1.path()))?;
                            let sig = &sig_iter
                                .next()
                                .ok_or_else(|| {
                                    eyre!("missing signature for {}", e1.path().display())
                                })?
                                .1;
                            let delta = compare(sig, f, block)?;
                            details.push(ChangeDetails::Diff(delta))
                        } else if !e1.is_file() && e2.is_file() {
                            let v = fs::read(base.join(e2.path()))?;
                            details.push(ChangeDetails::Contents(v));
                        } // else: permissions or target change
                    }
                }
            }
            _ => {}
        }
    }
    Ok(details)
}

enum ProducerState {
    File {
        action_index: u32,
        file: fs::File,
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
                } => {
                    let mut buf = vec![0; self.max_chunk_bytes];
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        return Ok(Some(DetailFrame {
                            action_index,
                            payload: DetailPayload::FileEnd,
                        }));
                    }

                    buf.truncate(n);
                    self.state = Some(ProducerState::File { action_index, file });
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
                    let file = fs::File::open(self.base.join(path))?;
                    self.state = Some(ProducerState::File { action_index, file });
                    return Ok(Some(DetailFrame {
                        action_index,
                        payload: DetailPayload::FileBegin,
                    }));
                }
                SourceDetailKind::Diff(path) => {
                    let signature = self
                        .signatures
                        .get(self.signature_index)
                        .ok_or_else(|| eyre!("missing signature for {}", path.display()))?
                        .1
                        .clone();
                    self.signature_index += 1;

                    let file_path = self.base.join(path);
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
    temp_path: PathBuf,
    file: Option<fs::File>,
    _parent_guard: Option<WritableDirGuard>,
}

impl TempOutput {
    fn new(final_path: PathBuf) -> Result<Self> {
        let mut temp_path = final_path.clone();
        let temp_id = TEMP_OUTPUT_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        temp_path.set_file_name(format!(
            ".duet-part-{}-{}",
            std::process::id(),
            temp_id
        ));
        let parent_guard = match final_path.parent() {
            Some(parent) => WritableDirGuard::new(parent)?,
            None => None,
        };
        let file = fs::File::create(&temp_path)
            .wrap_err_with(|| format!("failed to create temporary file {}", temp_path.display()))?;
        Ok(TempOutput {
            final_path,
            temp_path,
            file: Some(file),
            _parent_guard: parent_guard,
        })
    }

    fn finish(mut self) -> Result<()> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| eyre!("temporary output is closed"))?;
        file.flush().wrap_err_with(|| {
            format!("failed to flush temporary file {}", self.temp_path.display())
        })?;
        drop(file);
        fs::rename(&self.temp_path, &self.final_path).wrap_err_with(|| {
            format!(
                "failed to rename temporary file {} to {}",
                self.temp_path.display(),
                self.final_path.display()
            )
        })?;
        Ok(())
    }

    fn temp_path(&self) -> &Path {
        &self.temp_path
    }
}

struct WritableDirGuard {
    path: PathBuf,
    original_mode: u32,
}

impl WritableDirGuard {
    fn new(path: &Path) -> Result<Option<Self>> {
        let meta = fs::symlink_metadata(path).wrap_err_with(|| {
            format!("failed to read directory metadata for {}", path.display())
        })?;
        let original_mode = meta.permissions().mode();
        if owner_writable(original_mode) {
            return Ok(None);
        }
        let mut perms = meta.permissions();
        perms.set_mode(original_mode | 0o700);
        fs::set_permissions(path, perms).wrap_err_with(|| {
            format!(
                "failed to make directory writable for sync {}",
                path.display()
            )
        })?;
        Ok(Some(Self {
            path: path.to_path_buf(),
            original_mode,
        }))
    }
}

impl Drop for WritableDirGuard {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.original_mode));
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp_path);
    }
}

fn ensure_parent_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).wrap_err_with(|| {
            format!(
                "failed to create destination parent directory {}",
                parent.display()
            )
        })?;
    }
    Ok(())
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
    old_index: usize,
    action_index: usize,
    new_entries: Vec<Entry>,
    state: Option<ApplyState>,
}

impl DetailApplier {
    pub fn new_with_attempt(
        base: PathBuf,
        actions: Vec<Action>,
        all_old: Vec<Entry>,
        attempt_state: Option<PathBuf>,
    ) -> Self {
        DetailApplier {
            base,
            actions,
            all_old,
            attempt_state,
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

        self.advance_to_action(frame_index)?;
        match frame.payload {
            DetailPayload::FileBegin => self.begin_file_detail(frame_index),
            DetailPayload::DiffBegin => self.begin_diff_detail(frame_index),
            _ => Err(eyre!(
                "detail stream for action {} did not begin with a begin frame",
                frame_index
            )),
        }
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
                    let filename = self.base.join(e.path());
                    if !e.is_dir() {
                        fs::remove_file(&filename)?;
                        record_committed_step(
                            self.attempt_state.as_deref(),
                            "remove-file",
                            e.path(),
                        )?;
                    }
                }
                Change::Added(e) => {
                    let filename = self.base.join(e.path());
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
                        fs::create_dir(&filename)?;
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
                    let filename = self.base.join(e2.path());
                    if e1.is_file() {
                        if e2.is_file() {
                            if !e1.same_contents(e2) {
                                return Err(eyre!(
                                    "missing diff detail for {}",
                                    e2.path().display()
                                ));
                            }
                            self.new_entries.push(update_meta(&filename, e2)?);
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "update-metadata",
                                e2.path(),
                            )?;
                        } else {
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
                                fs::create_dir(&filename)?;
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
                            fs::create_dir(&filename)?;
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
        record_staged_file(self.attempt_state.as_deref(), output.temp_path())?;
        self.state = Some(ApplyState::File {
            action_index,
            output,
        });
        Ok(())
    }

    fn begin_diff_detail(&mut self, action_index: usize) -> Result<()> {
        self.prepare_action(action_index);
        let filename = detail_filename(&self.base, &self.actions[action_index])?;
        let source = fs::File::open(&filename)?;
        let output = TempOutput::new(filename)?;
        record_staged_file(self.attempt_state.as_deref(), output.temp_path())?;
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
        let (action_index, output) = match state {
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
        let filename = self.base.join(entry.path());
        output.finish()?;
        record_committed_step(self.attempt_state.as_deref(), "rename-file", entry.path())?;
        self.new_entries.push(update_meta(&filename, entry)?);
        record_committed_step(self.attempt_state.as_deref(), "update-metadata", entry.path())?;
        record_committed_action(self.attempt_state.as_deref(), &self.actions[action_index])?;
        self.action_index = action_index + 1;
        Ok(())
    }

    fn apply_directory_second_pass(&mut self) -> Result<()> {
        for action in self.actions.iter().rev() {
            match action {
                Action::Local(change) | Action::ResolvedLocal((_, _), change) => {
                    if !change.is_dir() {
                        continue;
                    }
                    match change {
                        Change::Removed(e) => {
                            let dirname = self.base.join(e.path());
                            fs::remove_dir(&dirname)?;
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "remove-dir",
                                e.path(),
                            )?;
                        }
                        Change::Added(e) => {
                            let dirname = self.base.join(e.path());
                            self.new_entries.push(update_meta(&dirname, e)?);
                            record_committed_step(
                                self.attempt_state.as_deref(),
                                "update-metadata",
                                e.path(),
                            )?;
                        }
                        Change::Modified(e1, e2) => {
                            let dirname = self.base.join(e2.path());
                            if e1.is_dir() && !e2.is_dir() {
                                return Err(eyre!(
                                    "streaming directory-to-file changes is not supported"
                                ));
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
        | Action::ResolvedLocal((_, _), Change::Modified(_, e)) => Ok(base.join(e.path())),
        _ => Err(eyre!("action has no detail filename")),
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
    let mut buf = [0; WINDOW];
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

pub fn apply_detailed_changes(
    base: &PathBuf,
    actions: &Vec<Action>,
    details: &Vec<ChangeDetails>,
    all_old: &mut Vec<Entry>,
    attempt_state: Option<&Path>,
) -> Result<()> {
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
                        let filename = base.join(e.path());
                        log::debug!("Removing {:?}", filename);
                        if !e.is_dir() {
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
                        let filename = base.join(e.path());
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
                            fs::create_dir(&filename).wrap_err_with(|| {
                                format!("failed to create directory {}", filename.display())
                            })?;
                            record_committed_step(attempt_state, "create-dir", e.path())?;
                            // new entry gets updated in the second pass, after all the updates in
                            // the directory are finished
                        } else {
                            log::debug!("Adding {}", e.path().display());
                            let detail = next_detail(&mut details_iter, e.path())?;
                            create_file(&filename, detail, attempt_state)?;
                            new_entries.push(update_meta(&filename, e)?);
                            record_committed_step(attempt_state, "update-metadata", e.path())?;
                        }
                    }
                    Change::Modified(e1, e2) => {
                        let filename = base.join(e2.path());
                        if e1.is_file() {
                            if e2.is_file() {
                                if !e1.same_contents(&e2) {
                                    let detail = next_detail(&mut details_iter, e2.path())?;
                                    match detail {
                                        ChangeDetails::Diff(delta) => {
                                            update_file_with_diff(&filename, delta, attempt_state)?;
                                        }
                                        _ => {
                                            return Err(eyre!(
                                            "mismatch when adding {}, expected Diff, but not found",
                                            e1.path().display()
                                        ))
                                        }
                                    }
                                }
                                new_entries.push(update_meta(&filename, e2)?);
                                record_committed_step(
                                    attempt_state,
                                    "update-metadata",
                                    e2.path(),
                                )?;
                            } else {
                                // e2 not a file
                                // remove the file
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
                                    fs::create_dir(&filename).wrap_err_with(|| {
                                        format!("failed to create directory {}", filename.display())
                                    })?;
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
                            fs::remove_file(&filename).wrap_err_with(|| {
                                format!("failed to remove file {}", filename.display())
                            })?;
                            record_committed_step(attempt_state, "remove-symlink", e1.path())?;
                            if e2.is_file() {
                                let detail = next_detail(&mut details_iter, e2.path())?;
                                create_file(&filename, detail, attempt_state)?;
                                new_entries.push(update_meta(&filename, e2)?);
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
                                fs::create_dir(&filename).wrap_err_with(|| {
                                    format!("failed to create directory {}", filename.display())
                                })?;
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

    // TODO: think how directory removal interacts with "ignore", if we ever implement it

    // second pass, in reverse order, to remove directories and update their metadata
    let mut details_iter = leftover_details.iter().rev();
    for action in actions.iter().rev() {
        match action {
            Action::Local(change) | Action::ResolvedLocal((_, _), change) => {
                if !change.is_dir() {
                    continue;
                }
                match change {
                    Change::Removed(e) => {
                        let dirname = base.join(e.path());
                        fs::remove_dir(&dirname).wrap_err_with(|| {
                            format!("failed to remove directory {}", dirname.display())
                        })?;
                        record_committed_step(attempt_state, "remove-dir", e.path())?;
                    }
                    Change::Added(e) => {
                        let dirname = base.join(e.path());
                        new_entries.push(update_meta(&dirname, e)?);
                        record_committed_step(attempt_state, "update-metadata", e.path())?;
                    }
                    Change::Modified(e1, e2) => {
                        let dirname = base.join(e2.path());
                        if e1.is_dir() && !e2.is_dir() {
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
                                create_file(&dirname, detail, attempt_state)?;
                            }
                        }
                        new_entries.push(update_meta(&dirname, e2)?);
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

    Ok(())
}

fn create_file(
    filename: &Path,
    detail: &ChangeDetails,
    attempt_state: Option<&Path>,
) -> Result<()> {
    match detail {
        ChangeDetails::Contents(v) => create_file_with_contents(filename, v, attempt_state),
        _ => Err(eyre!(
            "mismatch when adding {}, expected Contents, but not found",
            filename.display()
        )),
    }
}

fn create_file_with_contents(
    filename: &Path,
    data: &[u8],
    attempt_state: Option<&Path>,
) -> Result<()> {
    ensure_parent_directory(filename)?;
    let mut output = TempOutput::new(filename.to_path_buf())?;
    record_staged_file(attempt_state, output.temp_path())?;
    output
        .file
        .as_mut()
        .ok_or_else(|| eyre!("temporary output is closed"))?
        .write_all(data)
        .wrap_err_with(|| format!("failed to write temporary file for {}", filename.display()))?;
    output.finish()?;
    record_committed_step(attempt_state, "rename-file", filename)
}

fn update_file_with_diff(
    filename: &Path,
    delta: &Delta,
    attempt_state: Option<&Path>,
) -> Result<()> {
    let source = fs::File::open(filename)
        .wrap_err_with(|| format!("failed to open file {}", filename.display()))?;
    let mut output = TempOutput::new(filename.to_path_buf())?;
    record_staged_file(attempt_state, output.temp_path())?;
    let output_file = output
        .file
        .as_mut()
        .ok_or_else(|| eyre!("temporary output is closed"))?;
    restore_seek(output_file, source, [0; WINDOW], delta)
        .wrap_err_with(|| format!("failed to restore diff for {}", filename.display()))?;
    output.finish()?;
    record_committed_step(attempt_state, "rename-file", filename)
}

fn update_meta(path: &PathBuf, e: &Entry) -> Result<Entry> {
    let meta = fs::symlink_metadata(path)
        .wrap_err_with(|| format!("failed to read metadata for {}", path.display()))?;
    if !e.is_symlink() {
        let mut perms = meta.permissions();
        perms.set_mode(synced_mode(e.mode()));
        fs::set_permissions(path, perms)
            .wrap_err_with(|| format!("failed to set permissions for {}", path.display()))?;
    }
    filetime::set_symlink_file_times(
        path,
        filetime::FileTime::from_unix_time(meta.atime(), 0),
        filetime::FileTime::from_unix_time(e.mtime(), 0),
    )
    .wrap_err_with(|| format!("failed to set time for {}", path.display()))?;
    let mut new_entry = e.clone();
    new_entry.set_ino(meta.ino());
    Ok(new_entry)
}

fn synced_mode(mode: u32) -> u32 {
    mode & SYNCED_MODE_MASK
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngCore, SeedableRng};

    #[test]
    fn stream_diff_frames_coalesces_adjacent_copy_ops() {
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

        output.finish().unwrap();
        assert!(final_path.exists());
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
        let actions = vec![Action::Local(Change::Added(Entry::test_file(
            path.clone(),
            0,
        )))];
        let details = vec![ChangeDetails::Contents(b"commit-id\n".to_vec())];
        let mut all_old = Vec::new();

        preflight_apply(&base, &actions).unwrap();
        apply_detailed_changes(&base, &actions, &details, &mut all_old, None).unwrap();

        assert_eq!(fs::read(base.join(path)).unwrap(), b"commit-id\n");
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
        assert!(!marker.contains("staged-file: "), "{}", marker);
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
        assert!(error.contains("committed operations"));
        assert!(error.contains("committed apply steps"));

        finish_apply_attempt(&state).unwrap();
        check_apply_attempt_clear(&state).unwrap();
    }

    #[test]
    fn apply_attempt_recovery_advice_uses_operation_summaries() {
        let marker = "duet-apply-attempt-v1\nphase: apply\noperation: remove-file old.txt\noperation: modify-metadata mode.txt\noperation: modify-file contents.txt\nunstaged-operation: remove-file old.txt\nstaged-file: /tmp/.duet-part-test\ncommitted-step: rename-file contents.txt\ncommitted-operation: modify-file contents.txt\n";

        let advice = apply_attempt_recovery_advice(marker);

        assert!(advice.contains("Removed or replaced paths"), "{}", advice);
        assert!(advice.contains("Metadata operations"), "{}", advice);
        assert!(
            advice.contains("File contents may have changed"),
            "{}",
            advice
        );
        assert!(advice.contains("committed operations"), "{}", advice);
        assert!(advice.contains("committed apply steps"), "{}", advice);
        assert!(advice.contains("staged temporary files"), "{}", advice);
        assert!(advice.contains("unstaged operations"), "{}", advice);
    }
}
