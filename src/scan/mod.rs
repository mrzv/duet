use std::path::{PathBuf,Path};
use std::cmp::Ordering;

use std::os::unix::fs::MetadataExt;

use serde::{Serialize,Deserialize};

use color_eyre::eyre::Result;

use tokio::sync::{mpsc,Semaphore};
use std::sync::Arc;

use log;

//use glob;
// TODO: incorporate ignore

pub mod location;
pub mod change;

use location::{Locations,Location};
pub use change::{changes,Change};

#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct DirEntryWithMeta {
    path:   String,
    size:   u64,
    mtime:  i64,
    ino:    u64,
    mode:   u32,
    target: Option<String>,
    is_dir: bool,
    // TODO: uid and gid
}

impl DirEntryWithMeta {
    fn same(&self, other: &Self) -> bool {
        assert_eq!(self.path, other.path);
        self.same_contents(other) && self.mode == other.mode && self.target == other.target && self.is_dir == other.is_dir
    }

    pub fn starts_with(&self, path: &str) -> bool {
        self.path.starts_with(path)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn target(&self) -> &Option<String> {
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

    pub fn set_ino(&mut self, ino: u64) {
        self.ino = ino;
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }
}

impl PartialEq for DirEntryWithMeta {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for DirEntryWithMeta { }

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

#[derive(Debug,Clone)]
struct ParentFromTo {
    parent: usize,
    from: usize,
    to: usize
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
        while to < parent_to && locations[to+1].path().starts_with(path) {
            to += 1;
        }
    }

    // update parent
    if from <= to && locations[from].path() == path {
        parent = from;
    }

    ParentFromTo { parent, from, to }
}

fn relative<'a>(base: &PathBuf, path: &'a PathBuf) -> &'a Path {
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

async fn scan_dir(path: PathBuf, locations: Arc<Locations>, restrict: Arc<PathBuf>, base: Arc<PathBuf>, pft: ParentFromTo, dev: u64, tx: mpsc::Sender<DirEntryWithMeta>, s: Arc<Semaphore>) -> Result<()> {
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

    let _sp = s.acquire().await;
    let mut dir = fs::read_dir(path).await.expect("Couldn't read the directory");
    while let Some(child) = dir.next_entry().await.expect("Couldn't read the next directory entry") {
        let path = child.path();
        let meta = fs::symlink_metadata(&path).await.expect("Couldn't get metadata");

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
            tx.send(DirEntryWithMeta {
                    path: relative(&*base, &path).to_str().unwrap().to_string(),
                    target: fs::read_link(path).await.map_or(None, |p| Some(p.to_str().unwrap().to_string())),
                    size: meta.size(),
                    mtime: meta.mtime(),
                    ino: meta.ino(),
                    mode: meta.mode(),
                    is_dir: meta.is_dir(),
            }).await.expect("Couldn't send result through the channel")
        }
    }

    scan_children(child_dirs, locations, restrict, base, pft, dev, tx, s.clone())?;

    Ok(())
}

fn scan_children(children: Vec<PathBuf>, locations: Arc<Locations>, restrict: Arc<PathBuf>, base: Arc<PathBuf>, pft: ParentFromTo, dev: u64, tx: mpsc::Sender<DirEntryWithMeta>, s: Arc<Semaphore>) -> Result<()> {
    for path in children {
        let locations = locations.clone();
        let restrict = restrict.clone();
        let base = base.clone();
        let pft = pft.clone();
        let tx = tx.clone();
        let s = s.clone();
        tokio::spawn(async move {
            scan_dir(path, locations, restrict, base, pft, dev, tx, s).await
        });
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
pub async fn scan<P: AsRef<Path>, Q: AsRef<Path>>(base: P, path: Q, locations: &Locations, tx: mpsc::Sender<DirEntryWithMeta>) -> Result<()> {
    let base = PathBuf::from(base.as_ref());
    let mut restrict = Arc::new(PathBuf::from(&base));
    (*Arc::get_mut(&mut restrict).unwrap()).push(path);
    let base = Arc::new(base);

    log::info!("Going to scan: {}", restrict.display());

    let dev = base.symlink_metadata().ok().unwrap().dev();
    let mut locations: Arc<Locations> = Arc::new(locations.iter().map(|l| l.prefix(&base)).collect());
    (*Arc::get_mut(&mut locations).unwrap()).sort();

    let s = Arc::new(Semaphore::new(64));

    let path = (*base).clone();
    let to = locations.len() - 1;
    scan_dir(path, locations, restrict, base, ParentFromTo { parent: 0, from: 0, to: to }, dev, tx, s).await?;

    Ok(())
}
