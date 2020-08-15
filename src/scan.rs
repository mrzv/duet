use std::path::{PathBuf};
use std::fs;
use std::io;
use itertools::{MultiPeek,multipeek};

use std::os::unix::fs::MetadataExt;

//use glob;
// TODO: incorporate ignore

use crate::profile::{Profile,Locations,Location};

struct Directory {
    entries: <Vec<fs::DirEntry> as IntoIterator>::IntoIter,
}

struct DirIterator {
    stack: Vec<Directory>,
    dev:   u64,
    base:  PathBuf,
    locations: MultiPeek<<Locations as IntoIterator>::IntoIter>,
}

impl DirIterator {
    fn push(&mut self, path: &PathBuf) {
        let mut paths: Vec<_> = fs::read_dir(path).unwrap()
                                                  .map(|r| r.unwrap())
                                                  .collect();
        paths.sort_by_key(|dir| dir.path());

        self.stack.push(Directory { entries: paths.into_iter() });
    }

    fn from_with_dev(path: &PathBuf, dev_path: &PathBuf, prf: &Profile) -> Self {
        let dev = dev_path.symlink_metadata().ok().unwrap().dev();

        let mut it = DirIterator {
            stack: vec![],
            dev,
            base: PathBuf::from(&prf.local),
            locations: multipeek(prf.paths.clone().into_iter()),
        };

        it.push(path);

        it.advance(path);

        it
    }

    // find the last parent location
    fn advance(&mut self, path: &PathBuf) {
        let locations = &mut self.locations;

        // assumes that locations start with something that's <= path
        while let Some(l1) = locations.peek() {
            if let Some(l2) = locations.peek() {
                let l = self.base.join(l2.path());
                if &l <= path {
                    locations.next();
                } else {
                    break;
                }
            } else {
                locations.next();
                break;
            }
        }
        locations.reset_peek();
    }
}

impl Iterator for DirIterator {
    type Item = (PathBuf,fs::Metadata);

    fn next(&mut self) -> Option<(PathBuf,fs::Metadata)> {
        let entry = loop {
            let dir = self.stack.last_mut();

            // nothing left on the stack
            if let None = dir {
                return None;
            }

            let dir = dir.unwrap();
            let entry = dir.entries.next();

            // entries exhausted
            if let None = entry {
                self.stack.pop();
            } else {
                break entry;
            }
        };

        let path = entry.unwrap().path();
        let meta = path.symlink_metadata().ok()?;

        if meta.is_dir() && self.dev == meta.dev() {
            self.push(&path);
        }

        Some((path,meta))
    }
}

pub fn scan(prf: &Profile, path: &Option<&str>) -> Result<(), io::Error> {
    let mut to_scan = PathBuf::from(&prf.local);
    let device_path = PathBuf::from(&prf.local);
    if let Some(path) = path {
        to_scan.push(path);
    }

    println!("Going to scan: {}", to_scan.display());

    let mut count = 0;

    for (path,_meta) in DirIterator::from_with_dev(&to_scan, &device_path, &prf) {
        //println!("{}", path.display());
        count += 1;
    }

    println!("Count: {}", count);

    Ok(())
}
