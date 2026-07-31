use std::cmp::Ordering;
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use std::os::unix::fs::{FileTypeExt, MetadataExt};

use serde::{Deserialize, Serialize};

use color_eyre::eyre::{eyre, Result, WrapErr};

use std::sync::Arc;
use tokio::sync::mpsc;

use futures::stream::{FuturesUnordered, StreamExt};

use log;

use crate::profile::Ignore;
use regex::Regex;
pub type Regexes = Vec<Regex>;
fn is_match(regexes: &Regexes, p: &Path) -> bool {
    if let Some(s) = p.file_name() {
        if let Some(s) = s.to_str() {
            for r in regexes {
                if r.is_match(s) {
                    return true;
                }
            }
        }
    }
    false
}

pub mod change;
pub mod location;

pub use change::{changes, Change};
use location::{Location, Locations};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentDigest(pub [u8; 32]);

impl std::fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntryWithMeta {
    path: PathBuf,
    size: u64,
    mtime: i64,
    ino: u64,
    mode: u32,
    target: Option<PathBuf>,
    is_dir: bool,
    checksum: u32,
    digest: Option<ContentDigest>,
    // TODO: uid and gid
}

/// Exact pre-digest entry layout used by headerless V1 snapshots and legacy RPCs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyEntry {
    path: PathBuf,
    size: u64,
    mtime: i64,
    ino: u64,
    mode: u32,
    target: Option<PathBuf>,
    is_dir: bool,
    checksum: u32,
}

impl From<LegacyEntry> for DirEntryWithMeta {
    fn from(entry: LegacyEntry) -> Self {
        Self {
            path: entry.path,
            size: entry.size,
            mtime: entry.mtime,
            ino: entry.ino,
            mode: entry.mode,
            target: entry.target,
            is_dir: entry.is_dir,
            checksum: entry.checksum,
            digest: None,
        }
    }
}

impl From<DirEntryWithMeta> for LegacyEntry {
    fn from(entry: DirEntryWithMeta) -> Self {
        Self {
            path: entry.path,
            size: entry.size,
            mtime: entry.mtime,
            ino: entry.ino,
            mode: entry.mode,
            target: entry.target,
            is_dir: entry.is_dir,
            checksum: entry.checksum,
        }
    }
}

impl From<&DirEntryWithMeta> for LegacyEntry {
    fn from(entry: &DirEntryWithMeta) -> Self {
        entry.clone().into()
    }
}

impl DirEntryWithMeta {
    #[cfg(test)]
    pub(crate) fn test_file(path: PathBuf, checksum: u32) -> Self {
        Self {
            path,
            size: 0,
            mtime: 0,
            ino: 0,
            mode: 0o100644,
            target: None,
            is_dir: false,
            checksum,
            digest: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_file_with_size(path: PathBuf, size: u64, checksum: u32) -> Self {
        Self {
            path,
            size,
            mtime: 0,
            ino: 0,
            mode: 0o100644,
            target: None,
            is_dir: false,
            checksum,
            digest: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_file_from_path(path: PathBuf, absolute_path: &Path) -> Self {
        let metadata = absolute_path.metadata().unwrap();
        Self {
            path,
            size: metadata.size(),
            mtime: metadata.mtime(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            target: None,
            is_dir: false,
            checksum: 0,
            digest: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_dir(path: PathBuf) -> Self {
        Self {
            path,
            size: 0,
            mtime: 0,
            ino: 0,
            mode: 0o40755,
            target: None,
            is_dir: true,
            checksum: 0,
            digest: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_symlink(path: PathBuf, target: PathBuf) -> Self {
        Self::test_symlink_with_mode(path, target, 0o120777)
    }

    #[cfg(test)]
    pub(crate) fn test_symlink_with_mode(path: PathBuf, target: PathBuf, mode: u32) -> Self {
        Self::test_symlink_with_mode_and_mtime(path, target, mode, 0)
    }

    #[cfg(test)]
    pub(crate) fn test_symlink_with_mode_and_mtime(
        path: PathBuf,
        target: PathBuf,
        mode: u32,
        mtime: i64,
    ) -> Self {
        Self {
            path,
            size: 0,
            mtime,
            ino: 0,
            mode,
            target: Some(target),
            is_dir: false,
            checksum: 0,
            digest: None,
        }
    }

    fn same(&self, other: &Self) -> bool {
        assert_eq!(self.path, other.path);
        (self.is_symlink() || self.mode == other.mode)
            && self.target == other.target
            && self.is_dir == other.is_dir
            && (self.is_dir || self.same_scan_identity(other))
    }

    pub fn starts_with<P: AsRef<Path>>(&self, path: P) -> bool {
        self.path.starts_with(path)
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn target(&self) -> &Option<PathBuf> {
        &self.target
    }

    pub fn same_contents(&self, other: &Self) -> bool {
        if self.is_file() && other.is_file() {
            if let (Some(left), Some(right)) = (self.digest, other.digest) {
                return self.size == other.size && left == right;
            }
        }
        self.same_scan_identity(other)
    }

    fn same_scan_identity(&self, other: &Self) -> bool {
        self.size == other.size && self.mtime == other.mtime && self.ino == other.ino
    }

    pub fn is_symlink(&self) -> bool {
        self.target.is_some()
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn mtime(&self) -> i64 {
        self.mtime
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn checksum(&self) -> u32 {
        self.checksum
    }

    pub fn digest(&self) -> Option<ContentDigest> {
        self.digest
    }

    pub(crate) fn inherit_content_hashes(&mut self, old: &Self) {
        self.checksum = old.checksum;
        self.digest = old.digest;
    }

    #[cfg(test)]
    pub(crate) fn set_digest(&mut self, digest: Option<ContentDigest>) {
        self.digest = digest;
    }

    pub fn set_ino(&mut self, ino: u64) {
        self.ino = ino;
    }

    #[cfg(test)]
    pub(crate) fn ino(&self) -> u64 {
        self.ino
    }

    #[cfg(test)]
    pub(crate) fn set_mode(&mut self, mode: u32) {
        self.mode = mode;
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        !(self.is_dir || self.is_symlink())
    }

    pub(crate) fn compute_content_hashes<F>(
        &mut self,
        base: &Path,
        buffer_size: usize,
        cancelled: F,
    ) -> Result<()>
    where
        F: Fn() -> bool,
    {
        if !self.is_file() {
            return Ok(());
        }
        assert!(buffer_size > 0, "hash buffer size must be nonzero");

        let filename = base.join(&self.path);
        log::trace!("Computing checksum for {}", filename.display());

        use adler32::RollingAdler32;
        let mut file = open_hash_source(base, &self.path)
            .wrap_err_with(|| format!("unable to open {} for checksum", filename.display()))?;
        let before = file
            .metadata()
            .wrap_err_with(|| format!("unable to read metadata for {}", filename.display()))?;
        self.validate_hash_source(&before, &filename)?;

        let mut hash = RollingAdler32::new();
        let mut strong = blake2_rfc::blake2b::Blake2b::new(32);
        let mut buffer = vec![0; buffer_size];
        loop {
            if cancelled() {
                return Err(eyre!("content hashing cancelled"));
            }
            let read = file
                .read(&mut buffer)
                .wrap_err_with(|| format!("unable to read {} for checksum", filename.display()))?;
            if read == 0 {
                break;
            }
            hash.update_buffer(&buffer[..read]);
            strong.update(&buffer[..read]);
        }
        let after = file
            .metadata()
            .wrap_err_with(|| format!("unable to re-read metadata for {}", filename.display()))?;
        if HashSourceIdentity::from(&before) != HashSourceIdentity::from(&after) {
            return Err(eyre!(
                "file changed while computing checksum: {}",
                filename.display()
            ));
        }
        self.checksum = hash.hash();
        let digest = strong.finalize();
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest.as_bytes());
        self.digest = Some(ContentDigest(bytes));

        Ok(())
    }

    fn validate_hash_source(&self, metadata: &std::fs::Metadata, filename: &Path) -> Result<()> {
        if !metadata.is_file()
            || metadata.ino() != self.ino
            || metadata.size() != self.size
            || metadata.mtime() != self.mtime
            || metadata.mode() != self.mode
        {
            return Err(eyre!(
                "file changed between scan and checksum: {}",
                filename.display()
            ));
        }
        Ok(())
    }
}

fn open_hash_source(base: &Path, relative: &Path) -> Result<std::fs::File> {
    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY)
        .open(base)
        .wrap_err_with(|| format!("unable to open checksum root {}", base.display()))?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(eyre!("invalid checksum path: {}", relative.display()));
        };
        let name = CString::new(name.as_bytes())?;
        let last = components.peek().is_none();
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if last {
                libc::O_RDONLY
            } else {
                libc::O_RDONLY | libc::O_DIRECTORY
            };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).wrap_err_with(|| {
                format!(
                    "unable to open checksum path component in {}",
                    relative.display()
                )
            });
        }
        let opened = unsafe { std::fs::File::from_raw_fd(fd) };
        if last {
            return Ok(opened);
        }
        directory = opened;
    }
    Err(eyre!("invalid empty checksum path"))
}

#[derive(PartialEq, Eq)]
struct HashSourceIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl From<&std::fs::Metadata> for HashSourceIdentity {
    fn from(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mode: metadata.mode(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

impl PartialEq for DirEntryWithMeta {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for DirEntryWithMeta {}

impl PartialOrd for DirEntryWithMeta {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DirEntryWithMeta {
    fn cmp(&self, other: &Self) -> Ordering {
        self.path.cmp(&other.path)
    }
}

#[derive(Debug, Clone)]
struct ParentFromTo {
    parent: usize,
    from: usize,
    to: usize,
}

#[derive(Debug)]
struct ScanContext {
    locations: Arc<Locations>,
    restrict: Arc<PathBuf>,
    base: Arc<PathBuf>,
    ignore: Arc<Regexes>,
    dev: u64,
    tx: mpsc::Sender<DirEntryWithMeta>,
}

#[derive(Debug)]
struct ScanJob {
    path: PathBuf,
    pft: ParentFromTo,
}

fn narrow_parent_from_to(pft: ParentFromTo, path: &PathBuf, locations: &Locations) -> ParentFromTo {
    let mut parent = pft.parent;
    let mut from = pft.from;
    let mut to = pft.to;

    // update descendants
    while from <= to && !locations[from].path().starts_with(path) {
        from += 1;
    }
    if from <= to {
        let parent_to = to;
        to = from;
        while to < parent_to && locations[to + 1].path().starts_with(path) {
            to += 1;
        }
    }

    // update parent
    if from <= to && locations[from].path() == path {
        parent = from;
    }

    ParentFromTo { parent, from, to }
}

pub fn relative<'a>(base: &PathBuf, path: &'a PathBuf) -> &'a Path {
    path.strip_prefix(&base).unwrap()
}

fn find_parent<'a>(path: &PathBuf, locations: &'a Locations, pft: &ParentFromTo) -> &'a Location {
    let parent = pft.parent;
    let mut from = pft.from;
    let to = pft.to;

    while from <= to && from < locations.len() {
        if locations[from].path() == path {
            return &locations[from];
        }
        from += 1;
    }
    &locations[parent]
}

fn is_relevant_to_restrict(path: &Path, restrict: &Path) -> bool {
    path.starts_with(restrict) || restrict.starts_with(path)
}

async fn scan_one_directory(context: Arc<ScanContext>, job: ScanJob) -> Result<Vec<ScanJob>> {
    let ScanJob { path, pft } = job;
    log::trace!("Scanning: {}", path.display());

    // check the restriction
    if !path.starts_with(&*context.restrict) && !context.restrict.starts_with(&path) {
        log::trace!(
            "Skipping (restriction): {:?} vs {:?}",
            path,
            context.restrict
        );
        return Ok(Vec::new());
    }

    let pft = narrow_parent_from_to(pft, &path, &context.locations);

    // no need to descend if we are in the exclude regime and there are no descendants
    if context.locations[pft.parent].is_exclude() && pft.from > pft.to {
        log::trace!("Skipping excluded: {:?}", path);
        return Ok(Vec::new());
    }

    // read the directory
    use tokio::fs;
    let mut child_jobs = Vec::new();

    let mut dir = fs::read_dir(&path)
        .await
        .wrap_err_with(|| format!("unable to read directory {}", path.display()))?;
    while let Some(child) = dir
        .next_entry()
        .await
        .wrap_err_with(|| format!("unable to read next directory entry in {}", path.display()))?
    {
        let path = child.path();

        if is_match(&context.ignore, &path) {
            log::trace!("Skipping (ignored): {:?}", path);
            continue;
        }

        let meta = fs::symlink_metadata(&path)
            .await
            .wrap_err_with(|| format!("unable to read metadata for {}", path.display()))?;

        let file_type = meta.file_type();
        let location = find_parent(&path, &context.locations, &pft);
        let child_pft = narrow_parent_from_to(pft.clone(), &path, &context.locations);
        let has_descendant_includes = child_pft.from <= child_pft.to
            && (child_pft.from..=child_pft.to)
                .any(|i| context.locations[i].is_include() && context.locations[i].path() != &path);

        if location.is_exclude() && !has_descendant_includes {
            log::trace!("Not reporting (excluded): {:?}", path);
            continue;
        }

        if file_type.is_block_device()
            || file_type.is_char_device()
            || file_type.is_fifo()
            || file_type.is_socket()
        {
            if path.starts_with(&*context.restrict) {
                return Err(eyre!(
                    "unsupported special file in sync tree: {}",
                    path.display()
                ));
            }
            log::trace!("Skipping special file outside restriction: {:?}", path);
            continue;
        }

        if meta.is_dir()
            && context.dev != meta.dev()
            && is_relevant_to_restrict(&path, &context.restrict)
        {
            return Err(eyre!(
                "refusing to cross filesystem boundary at {}",
                path.display()
            ));
        }

        if meta.is_dir() && context.dev == meta.dev() {
            child_jobs.push(ScanJob {
                path: path.clone(),
                pft: pft.clone(),
            });
        }

        if location.is_exclude() {
            log::trace!("Not reporting (excluded): {:?}", path);
            continue;
        }

        // check restriction and crossing the filesystem boundary
        if path.starts_with(&*context.restrict) && context.dev == meta.dev() {
            log::trace!("Reporting: {:?}", path);
            let target = if file_type.is_symlink() {
                Some(fs::read_link(&path).await.wrap_err_with(|| {
                    format!("unable to read symlink target for {}", path.display())
                })?)
            } else {
                None
            };

            context
                .tx
                .send(DirEntryWithMeta {
                    path: relative(&context.base, &path).to_path_buf(),
                    target,
                    size: meta.size(),
                    mtime: meta.mtime(),
                    ino: meta.ino(),
                    mode: meta.mode(),
                    is_dir: meta.is_dir(),
                    checksum: 0,
                    digest: None,
                })
                .await
                .map_err(|_| eyre!("unable to send scan result for {}", path.display()))?
        }
    }
    Ok(child_jobs)
}

async fn run_scan_scheduler<F, Fut>(initial: ScanJob, limit: usize, mut worker: F) -> Result<()>
where
    F: FnMut(ScanJob) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<ScanJob>>>,
{
    assert!(limit > 0, "scan concurrency limit must be nonzero");
    let mut pending = VecDeque::from([initial]);
    let mut active = FuturesUnordered::new();

    while !pending.is_empty() || !active.is_empty() {
        while active.len() < limit {
            let Some(job) = pending.pop_front() else {
                break;
            };
            active.push(worker(job));
        }

        if let Some(result) = active.next().await {
            let mut children = result?;
            children.sort_by(|a, b| a.path.cmp(&b.path));
            pending.extend(children);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn test_job(path: impl Into<PathBuf>) -> ScanJob {
        ScanJob {
            path: path.into(),
            pft: ParentFromTo {
                parent: 0,
                from: 0,
                to: 0,
            },
        }
    }

    #[tokio::test]
    async fn scheduler_runs_multiple_jobs_within_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(std::sync::Mutex::new(Vec::new()));
        let active_for_worker = active.clone();
        let maximum_for_worker = maximum.clone();
        let started_for_worker = started.clone();

        run_scan_scheduler(test_job("root"), 4, move |job| {
            started_for_worker.lock().unwrap().push(job.path.clone());
            let active = active_for_worker.clone();
            let maximum = maximum_for_worker.clone();
            async move {
                let now = active.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                maximum.fetch_max(now, AtomicOrdering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                active.fetch_sub(1, AtomicOrdering::SeqCst);
                if job.path == Path::new("root") {
                    Ok((0..12)
                        .rev()
                        .map(|i| test_job(format!("dir{i:02}")))
                        .collect())
                } else {
                    Ok(Vec::new())
                }
            }
        })
        .await
        .unwrap();

        assert!(maximum.load(AtomicOrdering::SeqCst) > 1);
        assert!(maximum.load(AtomicOrdering::SeqCst) <= 4);
        assert_eq!(
            *started.lock().unwrap(),
            std::iter::once(PathBuf::from("root"))
                .chain((0..12).map(|i| PathBuf::from(format!("dir{i:02}"))))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn scan_rejects_included_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        let err = scan(
            temp.path(),
            "",
            &vec![Location::Include(PathBuf::new())],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("unsupported special file"));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn scan_ignores_excluded_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        scan(
            temp.path(),
            "",
            &vec![
                Location::Include(PathBuf::new()),
                Location::Exclude(PathBuf::from("socket")),
            ],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap();

        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn scan_ignores_excluded_special_files_with_descendant_excludes() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        scan(
            temp.path(),
            "",
            &vec![
                Location::Include(PathBuf::new()),
                Location::Exclude(PathBuf::from("socket")),
                Location::Exclude(PathBuf::from("socket/child")),
            ],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap();

        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn restricted_scan_ignores_special_file_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("socket");
        let _listener = UnixListener::bind(&socket_path).unwrap();
        tokio::fs::create_dir_all(temp.path().join("wanted"))
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("wanted/file.txt"), b"data")
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        scan(
            temp.path(),
            "wanted",
            &vec![Location::Include(PathBuf::new())],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap();

        let mut paths = Vec::new();
        while let Some(entry) = rx.recv().await {
            paths.push(entry.path().clone());
        }

        assert!(paths.contains(&PathBuf::from("wanted")));
        assert!(paths.contains(&PathBuf::from("wanted/file.txt")));
        assert!(!paths.contains(&PathBuf::from("socket")));
    }

    #[tokio::test]
    async fn scan_still_descends_into_excluded_dir_for_included_child() {
        let temp = tempfile::tempdir().unwrap();
        let nested_dir = temp.path().join("dir").join("nested");
        tokio::fs::create_dir_all(&nested_dir).await.unwrap();
        tokio::fs::write(nested_dir.join("file.txt"), b"data")
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        scan(
            temp.path(),
            "",
            &vec![
                Location::Include(PathBuf::new()),
                Location::Exclude(PathBuf::from("dir")),
                Location::Include(PathBuf::from("dir/nested")),
            ],
            &Vec::new(),
            tx,
        )
        .await
        .unwrap();

        let mut paths = Vec::new();
        while let Some(entry) = rx.recv().await {
            paths.push(entry.path().clone());
        }

        assert!(paths.contains(&PathBuf::from("dir/nested")));
        assert!(paths.contains(&PathBuf::from("dir/nested/file.txt")));
        assert!(!paths.contains(&PathBuf::from("dir")));
    }

    #[tokio::test]
    async fn scan_handles_deep_directory_trees() {
        let temp = tempfile::tempdir().unwrap();
        let mut nested_dir = temp.path().to_path_buf();
        let mut deepest_relative = PathBuf::new();

        for i in 0..70 {
            let component = format!("dir{i}");
            nested_dir.push(&component);
            deepest_relative.push(component);
        }

        tokio::fs::create_dir_all(&nested_dir).await.unwrap();
        tokio::fs::write(nested_dir.join("file.txt"), b"data")
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::channel(128);

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            scan(
                temp.path(),
                "",
                &vec![Location::Include(PathBuf::new())],
                &Vec::new(),
                tx,
            ),
        )
        .await
        .unwrap()
        .unwrap();

        let mut paths = Vec::new();
        while let Some(entry) = rx.recv().await {
            paths.push(entry.path().clone());
        }

        assert!(paths.contains(&deepest_relative.join("file.txt")));
    }

    #[test]
    fn compute_checksum_streams_file_contents() {
        let temp = tempfile::tempdir().unwrap();
        let contents = (0..100_000).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let path = temp.path().join("file.bin");
        std::fs::write(&path, &contents).unwrap();
        let metadata = path.metadata().unwrap();
        let mut entry = DirEntryWithMeta {
            path: PathBuf::from("file.bin"),
            size: metadata.size(),
            mtime: metadata.mtime(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            target: None,
            is_dir: false,
            checksum: 0,
            digest: None,
        };

        entry
            .compute_content_hashes(temp.path(), 64 * 1024, || false)
            .unwrap();

        assert_eq!(entry.checksum(), adler32::adler32(&contents[..]).unwrap());
        assert_eq!(entry.digest(), Some(crate::sync::content_digest(&contents)));
    }

    #[test]
    fn compute_checksum_rejects_changed_or_symlinked_source() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.bin");
        std::fs::write(&path, b"before").unwrap();
        let entry = DirEntryWithMeta::test_file_from_path(PathBuf::from("file.bin"), &path);

        std::fs::write(&path, b"longer after").unwrap();
        let mut changed = entry.clone();
        let error = changed
            .compute_content_hashes(temp.path(), 1024, || false)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("changed between scan and checksum"));
        assert_eq!(changed.digest(), None);

        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("target.bin", &path).unwrap();
        std::fs::write(temp.path().join("target.bin"), b"before").unwrap();
        let mut symlinked = entry;
        assert!(symlinked
            .compute_content_hashes(temp.path(), 1024, || false)
            .is_err());
        assert_eq!(symlinked.digest(), None);
    }

    #[test]
    fn compute_checksum_does_not_follow_symlinked_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("directory");
        let external = temp.path().join("external");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&external).unwrap();
        let path = directory.join("file.bin");
        std::fs::write(&path, b"contents").unwrap();
        let mut entry =
            DirEntryWithMeta::test_file_from_path(PathBuf::from("directory/file.bin"), &path);

        std::fs::hard_link(&path, external.join("file.bin")).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        std::os::unix::fs::symlink(&external, &directory).unwrap();

        assert!(entry
            .compute_content_hashes(temp.path(), 1024, || false)
            .is_err());
        assert_eq!(entry.digest(), None);
    }

    #[test]
    fn blake2b_256_known_vector_and_adler_collision() {
        assert_eq!(
            crate::sync::content_digest(b"").to_string(),
            "0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8"
        );
        let a = [10, 10, 10, 10];
        let b = [11, 9, 9, 11];
        assert_eq!(
            adler32::adler32(&a[..]).unwrap(),
            adler32::adler32(&b[..]).unwrap()
        );
        assert_ne!(
            crate::sync::content_digest(&a),
            crate::sync::content_digest(&b)
        );
    }

    #[test]
    fn filesystem_boundary_relevance_includes_restrict_ancestors() {
        let restrict = Path::new("/base/mount/wanted");

        assert!(is_relevant_to_restrict(Path::new("/base/mount"), restrict));
        assert!(is_relevant_to_restrict(
            Path::new("/base/mount/wanted"),
            restrict
        ));
        assert!(is_relevant_to_restrict(
            Path::new("/base/mount/wanted/child"),
            restrict
        ));
        assert!(!is_relevant_to_restrict(Path::new("/base/other"), restrict));
    }
}

/// Send all [directory entries](DirEntryWithMeta) into the channel, given via its [Sender](mpsc::Sender) `tx`.
///
/// # Arguments
///
/// * `base` - root path of the scan, `locations` are specified relative to this path
/// * `path` - restriction under base, which should be scanned
/// * `locations` - [locations](Locations) to scan
/// * `tx` - [Sender](mpsc::Sender) of the channel, where to send the [directory entries](DirEntryWithMeta)
pub async fn scan<P: AsRef<Path>, Q: AsRef<Path>>(
    base: P,
    path: Q,
    locations: &Locations,
    ignore: &Ignore,
    tx: mpsc::Sender<DirEntryWithMeta>,
) -> Result<()> {
    scan_with_limit(base, path, locations, ignore, tx, 64).await
}

pub(crate) async fn scan_with_limit<P: AsRef<Path>, Q: AsRef<Path>>(
    base: P,
    path: Q,
    locations: &Locations,
    ignore: &Ignore,
    tx: mpsc::Sender<DirEntryWithMeta>,
    limit: usize,
) -> Result<()> {
    assert!(limit > 0, "scan concurrency limit must be nonzero");
    let base = PathBuf::from(base.as_ref());
    let mut restrict = Arc::new(PathBuf::from(&base));
    (*Arc::get_mut(&mut restrict).unwrap()).push(path);
    let base = Arc::new(base);

    log::info!("Going to scan: {}", restrict.display());

    let dev = tokio::fs::symlink_metadata(&*base)
        .await
        .wrap_err_with(|| format!("unable to read metadata for scan base {}", base.display()))?
        .dev();
    let locations = location::canonicalize(locations);
    let locations: Arc<Locations> = Arc::new(locations.iter().map(|l| l.prefix(&base)).collect());

    // build ignore regex
    use fnmatch_regex::glob_to_regex;
    let mut ignore_regex: Regexes = Vec::new();
    for p in ignore {
        ignore_regex
            .push(glob_to_regex(p).wrap_err_with(|| format!("invalid ignore pattern {p}"))?);
    }
    let ignore = Arc::new(ignore_regex);

    let path = (*base).clone();
    let to = locations.len() - 1;
    let context = Arc::new(ScanContext {
        locations,
        restrict,
        base,
        ignore,
        dev,
        tx,
    });
    let initial = ScanJob {
        path,
        pft: ParentFromTo {
            parent: 0,
            from: 0,
            to: to,
        },
    };
    run_scan_scheduler(initial, limit, |job| {
        scan_one_directory(context.clone(), job)
    })
    .await
}
