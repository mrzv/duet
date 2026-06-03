use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use std::os::unix::fs::{FileTypeExt, MetadataExt};

use serde::{Deserialize, Serialize};

use color_eyre::eyre::{eyre, Result, WrapErr};

use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

use async_recursion::async_recursion;

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
    // TODO: uid and gid
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
        }
    }

    fn same(&self, other: &Self) -> bool {
        assert_eq!(self.path, other.path);
        (self.is_symlink() || self.mode == other.mode)
            && self.target == other.target
            && self.is_dir == other.is_dir
            && (self.is_dir || self.same_contents(other))
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

    pub fn set_ino(&mut self, ino: u64) {
        self.ino = ino;
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        !(self.is_dir || self.is_symlink())
    }

    pub async fn compute_checksum(&mut self, base: &PathBuf) -> Result<()> {
        if !self.is_file() {
            return Ok(());
        }

        let filename = base.join(&self.path);
        log::trace!("Computing checksum for {}", filename.display());

        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(&filename)
            .await
            .wrap_err_with(|| format!("unable to open {} for checksum", filename.display()))?;
        let mut contents = vec![];
        file.read_to_end(&mut contents)
            .await
            .wrap_err_with(|| format!("unable to read {} for checksum", filename.display()))?;
        self.checksum = adler32::adler32(&contents[..])
            .wrap_err_with(|| format!("unable to checksum {}", filename.display()))?;

        Ok(())
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

#[async_recursion]
async fn scan_dir(
    path: PathBuf,
    locations: Arc<Locations>,
    restrict: Arc<PathBuf>,
    base: Arc<PathBuf>,
    ignore: Arc<Regexes>,
    pft: ParentFromTo,
    dev: u64,
    tx: mpsc::Sender<DirEntryWithMeta>,
    s: Arc<Semaphore>,
) -> Result<()> {
    log::trace!("Scanning: {}", path.display());

    // check the restriction
    if !path.starts_with(&*restrict) && !restrict.starts_with(&path) {
        log::trace!("Skipping (restriction): {:?} vs {:?}", path, restrict);
        return Ok(());
    }

    let pft = narrow_parent_from_to(pft, &path, &locations);

    // no need to descend if we are in the exclude regime and there are no descendants
    if locations[pft.parent].is_exclude() && pft.from > pft.to {
        log::trace!("Skipping excluded: {:?}", path);
        return Ok(());
    }

    // read the directory
    use tokio::fs;
    let mut child_dirs = Vec::new();

    let _sp = s.acquire().await.wrap_err("scanner semaphore closed")?;
    let mut dir = fs::read_dir(&path)
        .await
        .wrap_err_with(|| format!("unable to read directory {}", path.display()))?;
    while let Some(child) = dir
        .next_entry()
        .await
        .wrap_err_with(|| format!("unable to read next directory entry in {}", path.display()))?
    {
        let path = child.path();

        if is_match(&ignore, &path) {
            log::trace!("Skipping (ignored): {:?}", path);
            continue;
        }

        let meta = fs::symlink_metadata(&path)
            .await
            .wrap_err_with(|| format!("unable to read metadata for {}", path.display()))?;

        let file_type = meta.file_type();
        if file_type.is_block_device()
            || file_type.is_char_device()
            || file_type.is_fifo()
            || file_type.is_socket()
        {
            log::trace!("Skipping (special): {:?}", path);
            continue;
        }

        if meta.is_dir() && dev == meta.dev() {
            let path = path.clone();
            child_dirs.push(path);
        }

        if find_parent(&path, &locations, &pft).is_exclude() {
            log::trace!("Not reporting (excluded): {:?}", path);
            continue;
        }

        // check restriction and crossing the filesystem boundary
        if path.starts_with(&*restrict) && dev == meta.dev() {
            log::trace!("Reporting: {:?}", path);
            let target = if file_type.is_symlink() {
                Some(fs::read_link(&path).await.wrap_err_with(|| {
                    format!("unable to read symlink target for {}", path.display())
                })?)
            } else {
                None
            };

            tx.send(DirEntryWithMeta {
                path: relative(&*base, &path).to_path_buf(),
                target,
                size: meta.size(),
                mtime: meta.mtime(),
                ino: meta.ino(),
                mode: meta.mode(),
                is_dir: meta.is_dir(),
                checksum: 0,
            })
            .await
            .map_err(|_| eyre!("unable to send scan result for {}", path.display()))?
        }
    }

    scan_children(
        child_dirs,
        locations,
        restrict,
        base,
        ignore,
        pft,
        dev,
        tx,
        s.clone(),
    )
    .await?;

    Ok(())
}

async fn scan_children(
    children: Vec<PathBuf>,
    locations: Arc<Locations>,
    restrict: Arc<PathBuf>,
    base: Arc<PathBuf>,
    ignore: Arc<Regexes>,
    pft: ParentFromTo,
    dev: u64,
    tx: mpsc::Sender<DirEntryWithMeta>,
    s: Arc<Semaphore>,
) -> Result<()> {
    for path in children {
        scan_dir(
            path,
            locations.clone(),
            restrict.clone(),
            base.clone(),
            ignore.clone(),
            pft.clone(),
            dev,
            tx.clone(),
            s.clone(),
        )
        .await?;
    }

    Ok(())
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
    let base = PathBuf::from(base.as_ref());
    let mut restrict = Arc::new(PathBuf::from(&base));
    (*Arc::get_mut(&mut restrict).unwrap()).push(path);
    let base = Arc::new(base);

    log::info!("Going to scan: {}", restrict.display());

    let dev = base
        .symlink_metadata()
        .wrap_err_with(|| format!("unable to read metadata for scan base {}", base.display()))?
        .dev();
    let mut locations: Arc<Locations> =
        Arc::new(locations.iter().map(|l| l.prefix(&base)).collect());
    (*Arc::get_mut(&mut locations).unwrap()).sort();

    // build ignore regex
    use fnmatch_regex::glob_to_regex;
    let mut ignore_regex: Regexes = Vec::new();
    for p in ignore {
        ignore_regex
            .push(glob_to_regex(p).wrap_err_with(|| format!("invalid ignore pattern {p}"))?);
    }
    let ignore = Arc::new(ignore_regex.clone());

    let s = Arc::new(Semaphore::new(64));

    let path = (*base).clone();
    let to = locations.len() - 1;
    scan_dir(
        path,
        locations,
        restrict,
        base,
        ignore,
        ParentFromTo {
            parent: 0,
            from: 0,
            to: to,
        },
        dev,
        tx,
        s,
    )
    .await?;

    Ok(())
}
