use std::path::{PathBuf,Path};
use std::fs;
use std::cmp::Ordering;

use std::os::unix::fs::MetadataExt;

use savefile_derive::Savefile;

use color_eyre::eyre::Result;

use tokio::sync::mpsc;

use log;

//use glob;
// TODO: incorporate ignore

pub mod location;
pub mod change;

use location::{Locations,Location};
pub use change::{changes,Change};

#[derive(Debug,Clone,Savefile)]
pub struct DirEntryWithMeta {
    path:   String,
    size:   u64,
    mtime:  i64,
    ino:    u64,
    mode:   u32,
    target: Option<String>,
}

impl DirEntryWithMeta {
    fn same(&self, other: &Self) -> bool {
        assert_eq!(self.path, other.path);
        self.size == other.size && self.mtime == other.mtime && self.ino == other.ino && self.mode == other.mode && self.target == other.target
    }

    pub fn starts_with(&self, path: &str) -> bool {
        self.path.starts_with(path)
    }

    pub fn path(&self) -> &str {
        &self.path
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


pub struct DirIterator {
    stack:     Vec<Directory>,
    dev:       u64,
    base:      PathBuf,
    restrict:  PathBuf,
    locations: Locations,
}

struct Directory {
    entries: <Vec<fs::DirEntry> as IntoIterator>::IntoIter,
    parent: usize,
    descendants: (usize, usize),
}

impl<'a> DirIterator {
    pub fn create(base: PathBuf, restrict: PathBuf, locations: &Locations) -> Self {
        let dev = base.symlink_metadata().ok().unwrap().dev();

        // prefix locations with base
        let mut locations: Locations = locations.iter().map(|l| l.prefix(&base)).collect();
        locations.sort();

        let mut it = DirIterator {
            stack:     vec![],
            dev,
            base,
            restrict,
            locations,
        };
        for x in &it.locations {
            log::debug!("Location: {:?}", x);
        }

        it.push(&it.base.clone());

        it
    }

    fn push(&mut self, path: &PathBuf) {
        // check the restriction
        if !path.starts_with(&self.restrict) && !self.restrict.starts_with(path) {
            log::trace!("Skipping (restriction): {:?} vs {:?}", path, self.restrict);
            return;
        }

        let (parent, from, to) = self.find_parent_descendants(path);

        // no need to descend if we are in the exclude regime and there are no descendants
        if self.locations[parent].is_exclude() && from > to {
            log::trace!("Skipping excluded: {:?}", path);
            return;
        }

        // read the directory
        let mut paths: Vec<_> = fs::read_dir(path).unwrap()
                                                  .map(|r| r.unwrap())
                                                  .collect();
        paths.sort_by_key(|dir| dir.path());

        self.stack.push(Directory { entries: paths.into_iter(), parent, descendants: (from,to) });
    }

    // narrow the last parent/descendants on the stack for the path
    fn find_parent_descendants(&self, path: &PathBuf) -> (usize, usize, usize) {
        // read old parent and descendants
        let (mut parent, mut from, mut to) = if self.stack.is_empty() {
            (0, 0, self.locations.len() - 1)
        } else {
            let dir       = self.stack.last().unwrap();
            let parent    = dir.parent;
            let (from,to) = dir.descendants;
            (parent, from, to)
        };

        // update descendants
        while from <= to && !self.locations[from].path().starts_with(path) {
            from += 1;
        }
        if from <= to {
            let parent_to = to;
            to = from;
            while to < parent_to && self.locations[to+1].path().starts_with(path) {
                to += 1;
            }
        }

        // update parent
        if from <= to && self.locations[from].path() == path {
            parent = from;
        }

        (parent, from, to)
    }

    fn relative(&self, path: &'a PathBuf) -> &'a Path {
        path.strip_prefix(&self.base).unwrap()
    }

    // find closest parent among self.locations
    fn find_parent(&self, path: &PathBuf, parent: usize, descendants: (usize, usize)) -> &Location {
        let (mut from, to) = descendants;
        while from <= to && from < self.locations.len() {
            if self.locations[from].path() == path {
                return &self.locations[from];
            }
            from += 1;
        }
        &self.locations[parent]
    }
}

impl Iterator for DirIterator {
    type Item = DirEntryWithMeta;

    fn next(&mut self) -> Option<DirEntryWithMeta> {
        let (path, meta) = loop {
            let (path, parent, descendants) = loop {
                if self.stack.is_empty() {
                    return None;
                }

                let dir = self.stack.last_mut();

                let dir = dir.unwrap();
                let entry = dir.entries.next();

                // entries exhausted
                if let None = entry {
                    self.stack.pop();
                } else {
                    let path = entry.unwrap().path();
                    break (path, dir.parent, dir.descendants);
                }
            };

            // don't cross the filesystem boundary
            let meta = path.symlink_metadata().ok()?;
            if meta.is_dir() && self.dev == meta.dev() {
                self.push(&path);
            }

            if self.find_parent(&path, parent, descendants).is_exclude() {
                log::trace!("Not reporting (excluded): {:?}", path);
                continue;
            }

            // check restriction and crossing the filesystem boundary
            if path.starts_with(&self.restrict) && self.dev == meta.dev() {
                break (path,meta);
            }
        };

        Some(DirEntryWithMeta {
                path: self.relative(&path).to_str().unwrap().to_string(),
                target: path.read_link().map_or(None, |p| Some(p.to_str().unwrap().to_string())),
                size: meta.size(),
                mtime: meta.mtime(),
                ino: meta.ino(),
                mode: meta.mode(), })
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
pub async fn scan<P: AsRef<Path>, Q: AsRef<Path>>(base: P, path: Q, locations: &Locations, tx: mpsc::Sender<DirEntryWithMeta>) -> Result<()> {
    let base = PathBuf::from(base.as_ref());
    let mut restrict = PathBuf::from(&base);
    restrict.push(path);

    log::info!("Going to scan: {}", restrict.display());

    for e in DirIterator::create(base, restrict, locations) {
        tx.send(e).await?;
    }

    Ok(())
}
