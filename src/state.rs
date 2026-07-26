use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bincode::serde::{decode_from_slice, encode_into_std_write};
use color_eyre::eyre::{eyre, Result, WrapErr};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::profile;
use crate::scan::change::LegacyChange;
use crate::scan::location::Locations;
use crate::scan::{self, Change, DirEntryWithMeta, LegacyEntry};
use crate::sync;

pub type Entries = Vec<DirEntryWithMeta>;
pub type Changes = Vec<Change>;
pub type LegacyChanges = Vec<LegacyChange>;

const SNAPSHOT_MAGIC: &[u8; 8] = b"DUETSNP\0";
const SNAPSHOT_VERSION_V2: u8 = 2;
const HASH_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_HASH_BUFFER_SIZE: usize = 4 * 1024 * 1024;
const MAX_HASH_WORKERS: usize = 64;
const HASH_WORKERS_ENV: &str = "DUET_HASH_WORKERS";
const HASH_BUFFER_BYTES_ENV: &str = "DUET_HASH_BUFFER_BYTES";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotFormat {
    LegacyV1,
    V2,
}

#[derive(Debug, Clone)]
pub struct LoadedEntries {
    pub entries: Entries,
    pub format: SnapshotFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesV2 {
    pub changes: Changes,
    pub current: Entries,
    pub migration_needed: bool,
}

#[derive(Debug)]
pub struct ScanContext {
    pub all_old: Entries,
    pub changes: Changes,
    pub current: Entries,
    pub migration_needed: bool,
}

pub fn decode_entries(contents: &[u8]) -> Result<LoadedEntries> {
    let config = bincode::config::legacy();
    if contents.starts_with(SNAPSHOT_MAGIC) {
        let version = contents
            .get(SNAPSHOT_MAGIC.len())
            .copied()
            .ok_or_else(|| eyre!("truncated snapshot header"))?;
        if version != SNAPSHOT_VERSION_V2 {
            return Err(eyre!("unsupported snapshot version {version}"));
        }
        let payload = &contents[SNAPSHOT_MAGIC.len() + 1..];
        let (entries, consumed): (Entries, usize) = decode_from_slice(payload, config)?;
        if consumed != payload.len() {
            return Err(eyre!("trailing bytes in V2 snapshot"));
        }
        Ok(LoadedEntries { entries, format: SnapshotFormat::V2 })
    } else {
        let (legacy, consumed): (Vec<LegacyEntry>, usize) = decode_from_slice(contents, config)?;
        if consumed != contents.len() {
            return Err(eyre!("trailing bytes in legacy snapshot"));
        }
        Ok(LoadedEntries {
            entries: legacy.into_iter().map(Into::into).collect(),
            format: SnapshotFormat::LegacyV1,
        })
    }
}

pub fn load_entries_with_format(statefile: &Path) -> Result<LoadedEntries> {
    if !statefile
        .try_exists()
        .wrap_err_with(|| format!("unable to check state file {}", statefile.display()))?
    {
        return Ok(LoadedEntries { entries: Vec::new(), format: SnapshotFormat::V2 });
    }
    log::debug!("Loading: {}", statefile.display());
    let contents = std::fs::read(statefile)
        .wrap_err_with(|| format!("unable to read state file {}", statefile.display()))?;
    let loaded = decode_entries(&contents)
        .wrap_err_with(|| format!("unable to decode state file {}", statefile.display()))?;
    sync::validate_entries("state file", &loaded.entries)?;
    Ok(loaded)
}

pub fn load_entries(statefile: &PathBuf) -> Result<Entries> {
    Ok(load_entries_with_format(statefile)?.entries)
}

fn write_entries(writer: &mut impl Write, entries: &Entries, format: SnapshotFormat) -> Result<()> {
    match format {
        SnapshotFormat::LegacyV1 => {
            let legacy: Vec<LegacyEntry> = entries.iter().map(Into::into).collect();
            encode_into_std_write(&legacy, writer, bincode::config::legacy())?;
        }
        SnapshotFormat::V2 => {
            writer.write_all(SNAPSHOT_MAGIC)?;
            writer.write_all(&[SNAPSHOT_VERSION_V2])?;
            encode_into_std_write(entries, writer, bincode::config::legacy())?;
        }
    }
    Ok(())
}

pub fn save_entries_as(statefile: &Path, entries: &Entries, format: SnapshotFormat) -> Result<()> {
    sync::validate_entries("state file", entries)?;
    if let Some(parent) = statefile.parent() {
        sync::create_dir_all_durable(parent)
            .wrap_err_with(|| format!("unable to create state directory {}", parent.display()))?;
    }
    use atomicwrites::{AllowOverwrite, AtomicFile};
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    AtomicFile::new(statefile, AllowOverwrite)
        .write_with_options(|file| {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            let mut writer = BufWriter::new(file);
            write_entries(&mut writer, entries, format).map_err(std::io::Error::other)?;
            writer.flush()
        }, options)
        .wrap_err_with(|| format!("unable to atomically save state file {}", statefile.display()))?;
    Ok(())
}

pub fn save_entries(statefile: &PathBuf, entries: &Entries) -> Result<()> {
    save_entries_as(statefile, entries, SnapshotFormat::V2)
}

async fn collect_scan<F>(scanner: F, mut rx: mpsc::Receiver<DirEntryWithMeta>) -> Result<Entries>
where
    F: std::future::Future<Output = Result<()>>,
{
    tokio::pin!(scanner);
    let pb = indicatif::ProgressBar::new(1);
    pb.set_style(
        indicatif::ProgressStyle::default_spinner().template("[{elapsed_precise}] {wide_msg}")?,
    );
    let mut entries = Entries::new();

    loop {
        tokio::select! {
            result = &mut scanner => {
                if let Err(error) = result {
                    pb.finish_and_clear();
                    return Err(error).wrap_err("scanner failed");
                }
                while let Some(entry) = rx.recv().await {
                    pb.set_message(entry.path().display().to_string());
                    entries.push(entry);
                }
                break;
            }
            entry = rx.recv() => match entry {
                Some(entry) => {
                    pb.set_message(entry.path().display().to_string());
                    entries.push(entry);
                }
                None => {
                    if let Err(error) = scanner.await {
                        pb.finish_and_clear();
                        return Err(error).wrap_err("scanner failed");
                    }
                    break;
                }
            }
        }
    }

    pb.finish_and_clear();
    entries.sort();
    Ok(entries)
}

pub async fn scan_entries(
    base: &PathBuf,
    path: &PathBuf,
    locations: &Locations,
    ignore: &profile::Ignore,
) -> Result<Entries> {
    let base = base.clone();
    let path = path.clone();
    let locations = locations.clone();
    let ignore = ignore.clone();
    let (tx, rx) = mpsc::channel(32);
    collect_scan(scan::scan(&base, &path, &locations, &ignore, tx), rx).await
}

pub async fn hash_manifest(base: &PathBuf, entries: &mut Entries) -> Result<()> {
    hash_manifest_with_limit(base, entries, hash_worker_limit()).await
}

struct BlockingCancellationState {
    all: AtomicBool,
    cutoff: AtomicUsize,
}

struct BlockingCancellation {
    state: Arc<BlockingCancellationState>,
    position: usize,
}

impl BlockingCancellation {
    fn is_cancelled(&self) -> bool {
        self.state.all.load(Ordering::Relaxed)
            || self.position > self.state.cutoff.load(Ordering::Relaxed)
    }
}

struct CancelOnDrop(Arc<BlockingCancellationState>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.all.store(true, Ordering::Relaxed);
    }
}

async fn run_blocking_with_limit<T, U, F>(
    items: Vec<T>,
    limit: usize,
    worker: F,
) -> Result<Vec<U>>
where
    T: Send + 'static,
    U: Send + 'static,
    F: Fn(T, &BlockingCancellation) -> Result<U> + Send + Sync + 'static,
{
    assert!(limit > 0, "concurrency limit must be nonzero");
    let item_count = items.len();
    let mut pending: VecDeque<_> = items.into_iter().enumerate().collect();
    let mut active: FuturesUnordered<
        BoxFuture<'static, (usize, std::result::Result<Result<U>, tokio::task::JoinError>)>,
    > = FuturesUnordered::new();
    let mut results: Vec<Option<U>> = std::iter::repeat_with(|| None).take(item_count).collect();
    let mut active_positions = std::collections::BTreeSet::new();
    let mut first_error = None;
    let worker = Arc::new(worker);
    let cancellation = Arc::new(BlockingCancellationState {
        all: AtomicBool::new(false),
        cutoff: AtomicUsize::new(usize::MAX),
    });
    let _cancel_on_drop = CancelOnDrop(cancellation.clone());

    while !pending.is_empty() || !active.is_empty() {
        while active.len() < limit {
            let Some((position, item)) = pending.pop_front() else { break };
            let worker = worker.clone();
            let task_cancellation = BlockingCancellation {
                state: cancellation.clone(),
                position,
            };
            active_positions.insert(position);
            active.push(
                tokio::task::spawn_blocking(move || worker(item, &task_cancellation))
                    .map(move |joined| (position, joined))
                    .boxed(),
            );
        }

        let Some((position, joined)) = active.next().await else { break };
        active_positions.remove(&position);
        match joined {
            Ok(Ok(result)) => results[position] = Some(result),
            Ok(Err(error)) => {
                cancellation.cutoff.fetch_min(position, Ordering::Relaxed);
                record_first_error(&mut first_error, position, error);
            }
            Err(error) => {
                cancellation.cutoff.fetch_min(position, Ordering::Relaxed);
                record_first_error(
                    &mut first_error,
                    position,
                    eyre!("hash worker failed: {error}"),
                );
            }
        }
        if let Some((error_position, _)) = &first_error {
            pending.retain(|(position, _)| position < error_position);
            if !active_positions.iter().any(|position| position < error_position) {
                cancellation.all.store(true, Ordering::Relaxed);
                return Err(first_error.take().unwrap().1);
            }
        }
    }

    debug_assert!(first_error.is_none());
    Ok(results
        .into_iter()
        .map(|result| result.expect("successful hash worker did not return a result"))
        .collect())
}

fn record_first_error(
    first_error: &mut Option<(usize, color_eyre::Report)>,
    position: usize,
    error: color_eyre::Report,
) {
    if first_error.as_ref().map(|(first, _)| position < *first).unwrap_or(true) {
        *first_error = Some((position, error));
    }
}

fn hash_worker_limit() -> usize {
    let available = std::thread::available_parallelism().map(usize::from).unwrap_or(1);
    hash_worker_limit_from(std::env::var(HASH_WORKERS_ENV).ok().as_deref(), available)
}

fn hash_worker_limit_from(configured: Option<&str>, available: usize) -> usize {
    let requested = match configured {
        Some(value) => match value.parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => {
                log::warn!("ignoring invalid {HASH_WORKERS_ENV}={value:?}; using {available}");
                available.max(1)
            }
        },
        None => available.max(1),
    };
    if requested > MAX_HASH_WORKERS {
        log::warn!("limiting content hash workers from {requested} to {MAX_HASH_WORKERS}");
    }
    requested.min(MAX_HASH_WORKERS)
}

fn hash_buffer_size() -> usize {
    let requested = positive_env_usize(HASH_BUFFER_BYTES_ENV, HASH_BUFFER_SIZE);
    if requested > MAX_HASH_BUFFER_SIZE {
        log::warn!(
            "limiting content hash buffer from {requested} to {MAX_HASH_BUFFER_SIZE} bytes"
        );
    }
    requested.min(MAX_HASH_BUFFER_SIZE)
}

fn positive_env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name).ok().as_deref() {
        Some(value) => match value.parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => {
                log::warn!("ignoring invalid {name}={value:?}; using {default}");
                default
            }
        },
        None => default,
    }
}

async fn hash_work_with_limit(
    base: &PathBuf,
    mut work: Vec<(usize, DirEntryWithMeta)>,
    limit: usize,
) -> Result<Vec<(usize, DirEntryWithMeta)>> {
    assert!(limit > 0, "checksum concurrency limit must be nonzero");
    work.sort_by(|(_, a), (_, b)| a.path().cmp(b.path()));
    let pb = indicatif::ProgressBar::new(work.len() as u64);
    let base = Arc::new(base.clone());
    let buffer_size = hash_buffer_size();
    let worker_pb = pb.clone();
    let result = run_blocking_with_limit(work, limit, move |(index, mut entry), cancelled| {
        let base = base.clone();
        entry.compute_content_hashes(&base, buffer_size, || cancelled.is_cancelled())?;
        worker_pb.inc(1);
        Ok((index, entry))
    })
    .await;
    pb.finish_and_clear();
    result
}

pub(crate) async fn hash_manifest_with_limit(
    base: &PathBuf,
    entries: &mut Entries,
    limit: usize,
) -> Result<()> {
    let work = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.is_file())
        .map(|(index, entry)| (index, entry.clone()))
        .collect();
    for (index, entry) in hash_work_with_limit(base, work, limit).await? {
        entries[index] = entry;
    }
    Ok(())
}

pub fn replace_scope(entries: &mut Entries, restrict: &Path, current: &Entries) {
    entries.retain(|entry| !entry.starts_with(restrict));
    entries.extend(current.iter().cloned());
    entries.sort();
}

pub async fn old_and_changes(
    base: &PathBuf,
    restrict: &PathBuf,
    locations: &Locations,
    ignore: &profile::Ignore,
    statefile: Option<&PathBuf>,
    strong: bool,
) -> Result<ScanContext> {
    let restricted_current_scan = scan_entries(base, restrict, locations, ignore);
    let loaded = async {
        match statefile {
            Some(path) => load_entries_with_format(path),
            None => Ok(LoadedEntries { entries: Vec::new(), format: SnapshotFormat::V2 }),
        }
    };
    let (loaded, current) = tokio::join!(loaded, restricted_current_scan);
    let loaded = loaded?;
    let mut current = current?;
    let restricted_old: Vec<_> = loaded.entries.iter().filter(|e| e.starts_with(restrict)).collect();
    let mut changes: Changes = scan::changes(restricted_old.iter().copied(), current.iter()).collect();
    for entry in &mut current {
        if changes.binary_search_by(|change| change.path().cmp(entry.path())).is_err() {
            if let Ok(index) = restricted_old.binary_search_by(|old| old.path().cmp(entry.path())) {
                entry.inherit_content_hashes(restricted_old[index]);
            }
        }
    }
    let legacy_snapshot = loaded.format == SnapshotFormat::LegacyV1;
    if legacy_snapshot {
        log::debug!("loaded headerless V1 snapshot");
    }
    let migration_needed = strong
        && (loaded.format == SnapshotFormat::LegacyV1
            || restricted_old
                .iter()
                .any(|old| old.is_file() && old.digest().is_none()));

    if migration_needed {
        hash_manifest(base, &mut current).await?;
    } else {
        let work = changes
            .iter()
            .enumerate()
            .filter_map(|(index, change)| match change {
                Change::Added(new) | Change::Modified(_, new) if new.is_file() => {
                    Some((index, new.clone()))
                }
                _ => None,
            })
            .collect();
        for (index, entry) in hash_work_with_limit(base, work, hash_worker_limit()).await? {
            match &mut changes[index] {
                Change::Added(new) | Change::Modified(_, new) => *new = entry,
                Change::Removed(_) => unreachable!("removed entries are not hashed"),
            }
        }
        // Keep the retained current manifest useful to the V2 migration/action flow.
        for change in &changes {
            let new = match change { Change::Added(e) | Change::Modified(_, e) => Some(e), _ => None };
            if let Some(new) = new {
                if let Ok(index) = current.binary_search_by(|entry| entry.path().cmp(new.path())) {
                    current[index] = new.clone();
                }
            }
        }
    }

    Ok(ScanContext { all_old: loaded.entries, changes, current, migration_needed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ContentDigest;
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test]
    async fn scan_collection_is_cancellation_safe_and_never_returns_partial_entries() {
        struct DropFlag(std::sync::Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = std::sync::Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(1);
        let dropped_by_scanner = dropped.clone();
        let scanner = async move {
            let _drop_flag = DropFlag(dropped_by_scanner);
            tx.send(DirEntryWithMeta::test_file(PathBuf::from("partial"), 0))
                .await
                .unwrap();
            futures::future::pending::<()>().await;
            Ok(())
        };
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            collect_scan(scanner, rx)
        )
        .await
        .is_err());
        assert!(dropped.load(Ordering::SeqCst));

        let (tx, rx) = mpsc::channel(1);
        let scanner = async move {
            tx.send(DirEntryWithMeta::test_file(PathBuf::from("partial"), 0))
                .await
                .unwrap();
            Err(eyre!("injected scan failure"))
        };
        assert!(collect_scan(scanner, rx).await.is_err());
    }

    #[tokio::test]
    async fn blocking_worker_is_parallel_bounded_and_reports_first_input_error() {
        let active = std::sync::Arc::new(AtomicUsize::new(0));
        let maximum = std::sync::Arc::new(AtomicUsize::new(0));
        let threads = std::sync::Arc::new(std::sync::Mutex::new(HashSet::new()));
        let active_for_worker = active.clone();
        let maximum_for_worker = maximum.clone();
        let threads_for_worker = threads.clone();
        let output = run_blocking_with_limit((0..12).collect(), 3, move |item, _| {
            let active = active_for_worker.clone();
            let maximum = maximum_for_worker.clone();
            threads_for_worker.lock().unwrap().insert(std::thread::current().id());
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(10));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(item)
        })
        .await
        .unwrap();
        assert_eq!(output, (0..12).collect::<Vec<_>>());
        assert!(maximum.load(Ordering::SeqCst) > 1);
        assert!(maximum.load(Ordering::SeqCst) <= 3);
        assert!(threads.lock().unwrap().len() > 1);

        let error = run_blocking_with_limit(vec![0, 1], 2, |item, _| {
            if item == 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                Err::<(), _>(eyre!("first"))
            } else {
                Err::<(), _>(eyre!("second"))
            }
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "first");
    }

    #[tokio::test]
    async fn blocking_worker_admits_new_work_behind_slow_first_item() {
        let first_finished = Arc::new(AtomicBool::new(false));
        let admitted_while_first_running = Arc::new(AtomicBool::new(false));
        let first_finished_for_worker = first_finished.clone();
        let admitted_for_worker = admitted_while_first_running.clone();
        run_blocking_with_limit((0..8).collect(), 3, move |item, _| {
            if item == 0 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                first_finished_for_worker.store(true, Ordering::SeqCst);
            } else {
                if item >= 3 && !first_finished_for_worker.load(Ordering::SeqCst) {
                    admitted_for_worker.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(item)
        })
        .await
        .unwrap();
        assert!(admitted_while_first_running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropping_blocking_worker_run_requests_cancellation() {
        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let started_for_worker = started.clone();
        let stopped_for_worker = stopped.clone();
        let task = tokio::spawn(run_blocking_with_limit(vec![()], 1, move |(), cancelled| {
            started_for_worker.store(true, Ordering::SeqCst);
            while !cancelled.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            stopped_for_worker.store(true, Ordering::SeqCst);
            Err::<(), _>(eyre!("cancelled"))
        }));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !stopped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn hash_worker_default_uses_available_parallelism_and_override() {
        assert_eq!(hash_worker_limit_from(None, 12), 12);
        assert_eq!(hash_worker_limit_from(Some("24"), 12), 24);
        assert_eq!(hash_worker_limit_from(Some("0"), 12), 12);
        assert_eq!(hash_worker_limit_from(Some("invalid"), 12), 12);
        assert_eq!(hash_worker_limit_from(Some("1000"), 12), MAX_HASH_WORKERS);
    }

    #[tokio::test]
    async fn blocking_worker_stops_admitting_paths_after_first_error() {
        let largest_started = Arc::new(AtomicUsize::new(0));
        let largest_started_by_worker = largest_started.clone();
        let error = run_blocking_with_limit((0..100).collect(), 2, move |item, _| {
            largest_started_by_worker.fetch_max(item, Ordering::SeqCst);
            if item == 0 {
                Err::<(), _>(eyre!("first"))
            } else {
                std::thread::sleep(std::time::Duration::from_millis(50));
                Ok(())
            }
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "first");
        assert!(largest_started.load(Ordering::SeqCst) <= 1);
    }

    #[tokio::test]
    async fn blocking_worker_cancels_active_paths_after_first_error() {
        let later_stopped = Arc::new(AtomicBool::new(false));
        let later_stopped_by_worker = later_stopped.clone();
        let error = run_blocking_with_limit(vec![0, 1, 2], 3, move |item, cancelled| match item {
            0 => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                Ok(())
            }
            1 => Err(eyre!("first")),
            2 => {
                while !cancelled.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                later_stopped_by_worker.store(true, Ordering::SeqCst);
                Err(eyre!("cancelled later work"))
            }
            _ => unreachable!(),
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "first");
        assert!(later_stopped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn blocking_worker_panic_cancels_later_paths() {
        let later_stopped = Arc::new(AtomicBool::new(false));
        let later_stopped_by_worker = later_stopped.clone();
        let error = run_blocking_with_limit(vec![0, 1, 2], 3, move |item, cancelled| match item {
            0 => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                Ok(())
            }
            1 => panic!("injected worker panic"),
            2 => {
                while !cancelled.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                later_stopped_by_worker.store(true, Ordering::SeqCst);
                Err(eyre!("cancelled later work"))
            }
            _ => unreachable!(),
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("hash worker failed"));
        assert!(later_stopped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn checksum_limit_one_matches_parallel_and_errors_do_not_partially_update() {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in [("b", b"second".as_slice()), ("a", b"first".as_slice())] {
            tokio::fs::write(dir.path().join(path), contents)
                .await
                .unwrap();
        }
        let original = vec![
            DirEntryWithMeta::test_file_from_path(PathBuf::from("b"), &dir.path().join("b")),
            DirEntryWithMeta::test_file_from_path(PathBuf::from("a"), &dir.path().join("a")),
        ];
        let mut serial = original.clone();
        let mut parallel = original;
        hash_manifest_with_limit(&dir.path().to_path_buf(), &mut serial, 1)
            .await
            .unwrap();
        hash_manifest_with_limit(&dir.path().to_path_buf(), &mut parallel, 8)
            .await
            .unwrap();
        let hashes = |entries: &Entries| {
            entries
                .iter()
                .map(|entry| (entry.path().clone(), entry.checksum(), entry.digest()))
                .collect::<Vec<_>>()
        };
        assert_eq!(hashes(&serial), hashes(&parallel));

        let mut with_missing = vec![
            DirEntryWithMeta::test_file_from_path(PathBuf::from("a"), &dir.path().join("a")),
            DirEntryWithMeta::test_file(PathBuf::from("missing"), 0),
        ];
        assert!(
            hash_manifest_with_limit(&dir.path().to_path_buf(), &mut with_missing, 2)
                .await
                .is_err()
        );
        assert_eq!(with_missing[0].checksum(), 0);
        assert_eq!(with_missing[0].digest(), None);
    }

    #[tokio::test]
    async fn concurrent_scan_results_are_sorted_and_repeatable() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["z/file", "a/file", "m/file"] {
            tokio::fs::create_dir_all(dir.path().join(name).parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(dir.path().join(name), name.as_bytes())
                .await
                .unwrap();
        }
        let locations = vec![crate::scan::location::Location::Include(PathBuf::new())];
        let first = scan_entries(
            &dir.path().to_path_buf(),
            &PathBuf::new(),
            &locations,
            &Vec::new(),
        )
        .await
        .unwrap();
        let second = scan_entries(
            &dir.path().to_path_buf(),
            &PathBuf::new(),
            &locations,
            &Vec::new(),
        )
        .await
        .unwrap();
        let paths = |entries: Entries| {
            entries
                .into_iter()
                .map(|entry| entry.path().clone())
                .collect::<Vec<_>>()
        };
        let first = paths(first);
        let second = paths(second);
        assert!(first.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(first, second);
    }

    #[test]
    fn legacy_snapshot_is_headerless_and_round_trips_exactly() {
        let entries = vec![DirEntryWithMeta::test_file(PathBuf::from("a"), 0x12345678)];
        let mut bytes = Vec::new();
        write_entries(&mut bytes, &entries, SnapshotFormat::LegacyV1).unwrap();
        assert!(!bytes.starts_with(SNAPSHOT_MAGIC));
        let loaded = decode_entries(&bytes).unwrap();
        assert_eq!(loaded.format, SnapshotFormat::LegacyV1);
        assert_eq!(loaded.entries[0].checksum(), 0x12345678);
        assert_eq!(loaded.entries[0].digest(), None);
    }

    #[test]
    fn legacy_snapshot_matches_immutable_v1_bytes() {
        const V1_BYTES: &[u8] = &[
            1, 0, 0, 0, 0, 0, 0, 0, // entry count
            1, 0, 0, 0, 0, 0, 0, 0, b'a', // path
            0, 0, 0, 0, 0, 0, 0, 0, // size
            0, 0, 0, 0, 0, 0, 0, 0, // mtime
            0, 0, 0, 0, 0, 0, 0, 0, // inode
            0xa4, 0x81, 0, 0, // mode 0100644
            0, // no symlink target
            0, // not a directory
            0x78, 0x56, 0x34, 0x12, // Adler-32
        ];
        let entries = vec![DirEntryWithMeta::test_file(PathBuf::from("a"), 0x12345678)];
        let mut encoded = Vec::new();
        write_entries(&mut encoded, &entries, SnapshotFormat::LegacyV1).unwrap();

        assert_eq!(encoded, V1_BYTES);
        let decoded = decode_entries(V1_BYTES).unwrap();
        assert_eq!(decoded.format, SnapshotFormat::LegacyV1);
        assert_eq!(decoded.entries[0].path(), Path::new("a"));
    }

    #[tokio::test]
    async fn empty_legacy_snapshot_still_requires_v2_migration() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state");
        save_entries_as(&state_path, &Vec::new(), SnapshotFormat::LegacyV1).unwrap();
        let locations = vec![crate::scan::location::Location::Include(PathBuf::new())];

        let context = old_and_changes(
            &dir.path().to_path_buf(),
            &PathBuf::new(),
            &locations,
            &Vec::new(),
            Some(&state_path),
            true,
        )
        .await
        .unwrap();

        assert!(context.migration_needed);
    }

    #[test]
    fn v2_snapshot_preserves_digest() {
        let mut entry = DirEntryWithMeta::test_file(PathBuf::from("a"), 7);
        entry.set_digest(Some(ContentDigest([9; 32])));
        let mut bytes = Vec::new();
        write_entries(&mut bytes, &vec![entry], SnapshotFormat::V2).unwrap();
        assert!(bytes.starts_with(SNAPSHOT_MAGIC));
        let loaded = decode_entries(&bytes).unwrap();
        assert_eq!(loaded.format, SnapshotFormat::V2);
        assert_eq!(loaded.entries[0].digest(), Some(ContentDigest([9; 32])));
    }

    #[test]
    fn snapshot_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("nested/state");
        save_entries(&state_path, &Vec::new()).unwrap();
        assert_eq!(std::fs::metadata(state_path).unwrap().permissions().mode() & 0o777, 0o600);
    }


    #[tokio::test]
    async fn restricted_migration_hashes_scope_and_preserves_outside_hybrid_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("scope")).unwrap();
        std::fs::write(dir.path().join("scope/in"), b"inside").unwrap();
        std::fs::write(dir.path().join("outside"), b"outside").unwrap();
        let state_path = dir.path().join("state");
        let old = vec![
            DirEntryWithMeta::test_file(PathBuf::from("outside"), 1),
            DirEntryWithMeta::test_file(PathBuf::from("scope/in"), 2),
        ];
        save_entries_as(&state_path, &old, SnapshotFormat::LegacyV1).unwrap();
        let locations = vec![crate::scan::location::Location::Include(PathBuf::new())];

        let context = old_and_changes(
            &dir.path().to_path_buf(),
            &PathBuf::from("scope"),
            &locations,
            &Vec::new(),
            Some(&state_path),
            true,
        ).await.unwrap();

        assert!(context.migration_needed);
        assert!(context.current.iter().filter(|e| e.is_file()).all(|e| e.digest().is_some()));
        let mut migrated = context.all_old;
        replace_scope(&mut migrated, Path::new("scope"), &context.current);
        assert_eq!(migrated.iter().find(|e| e.path() == Path::new("outside")).unwrap().digest(), None);
        assert!(migrated.iter().find(|e| e.path() == Path::new("scope/in")).unwrap().digest().is_some());
    }
}
