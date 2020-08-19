use std::path::{PathBuf,Path};
use std::fs;
use std::io;

use std::os::unix::fs::MetadataExt;

use savefile::{save_file,load_file};
use savefile_derive::Savefile;

//use glob;
// TODO: incorporate ignore

use crate::profile::{Profile,Locations,Location};

use log;

struct Directory {
    entries: <Vec<fs::DirEntry> as IntoIterator>::IntoIter,
    parent: usize,
    descendants: (usize, usize),
}

pub struct DirIterator {
    stack:     Vec<Directory>,
    dev:       u64,
    base:      PathBuf,
    restrict:  PathBuf,
    locations: Locations,
}

#[derive(Debug,Savefile)]
pub struct DirEntryWithMeta {
    path:   String,
    size:   u64,
    mtime:  i64,
    ino:    u64,
    mode:   u32,
    target: Option<String>,
}

impl<'a> DirIterator {
    pub fn create(path: &PathBuf, dev_path: &PathBuf, prf: &Profile) -> Self {
        let dev = dev_path.symlink_metadata().ok().unwrap().dev();

        let base = PathBuf::from(&prf.local);

        // prefix locations with base
        let locations = prf.locations.iter().map(|l| l.prefix(&base)).collect();

        let mut it = DirIterator {
            stack:     vec![],
            dev,
            base,
            restrict:  path.clone(),
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

pub fn scan(prf: &Profile, path: &Option<PathBuf>) -> Result<(), io::Error> {
    let mut to_scan = PathBuf::from(&prf.local);
    let device_path = PathBuf::from(&prf.local);
    if let Some(path) = path {
        to_scan.push(path);
    }

    log::info!("Going to scan: {}", to_scan.display());

    //let entries: Vec<_> = DirIterator::create(&to_scan, &device_path, &prf).collect();
    //let count = entries.len();
    //save_file("save.bin", 0, &entries).unwrap();

    let mut count = 0;
    for entry in DirIterator::create(&to_scan, &device_path, &prf) {
        println!("{:?}", entry);
        count += 1;
    }

    //let entries: Vec<DirEntryWithMeta> = load_file("save.bin", 0).unwrap();
    //let count = entries.len();

    println!("Count: {}", count);

    Ok(())
}
