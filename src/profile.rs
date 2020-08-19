use std::fs::File;
use std::io::{self, prelude::*, BufReader};
use std::path::{PathBuf};
use std::cmp::Ordering;
use std::fmt;

use shellexpand;

#[derive(Debug, Clone)]
pub enum Location {
    Include(PathBuf),
    Exclude(PathBuf),
}

impl Location {
    pub fn path(&self) -> &PathBuf {
        match self {
            Location::Include(path) => path,
            Location::Exclude(path) => path,
        }
    }

    pub fn is_include(&self) -> bool {
        match self {
            Location::Include(_) => true,
            Location::Exclude(_) => false,
        }
    }

    pub fn is_exclude(&self) -> bool {
        return !self.is_include();
    }

    pub fn prefix(&self, p: &PathBuf) -> Self {
        match self {
            Location::Include(path) => Location::Include(p.join(path)),
            Location::Exclude(path) => Location::Exclude(p.join(path)),
        }
    }
}

impl Ord for Location {
    fn cmp(&self, other: &Self) -> Ordering {
        self.path().cmp(other.path())
    }
}

impl PartialOrd for Location {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Location {
    fn eq(&self, other: &Self) -> bool {
        self.path() == other.path()
    }
}

impl Eq for Location { }

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            Location::Include(path) => write!(f, "+ {}", path.to_str().unwrap()),
            Location::Exclude(path) => write!(f, "- {}", path.to_str().unwrap()),
        }
    }
}

pub type Locations = Vec<Location>;

#[derive(Debug)]
pub struct Profile {
    pub local:      String,
    pub remote:     String,
    pub locations:  Locations,
    pub ignore:     Vec<String>,
}

pub fn parse(name: &str) -> Result<Profile, io::Error> {
    let f      = File::open(name)?;
    let reader = BufReader::new(f);

    let mut p = Profile {
        local:      String::new(),
        remote:     String::new(),
        locations:  Vec::new(),
        ignore:     Vec::new(),
    };

    let mut locations = 0;
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if locations == 0 {
            if let Ok(line) = shellexpand::full(&line) {
                p.local = line.to_string();
                locations += 1;
                continue;
            } else {
                return parse_error(&line);
            }
        } else if locations == 1 {
            p.remote = line;
            locations += 1;
            p.locations.push(Location::Exclude(PathBuf::from(".")));    // implicitly exclude .
            continue;
        }

        // includes/excludes
        if locations == 2 {
            if line.trim().starts_with('+') {
                p.locations.push(Location::Include(PathBuf::from(&line[1..])));
            } else if line.trim().starts_with('-') {
                p.locations.push(Location::Exclude(PathBuf::from(&line[1..])));
            } else if line == "[ignore]" {
                locations += 1;
            } else {
                return parse_error(&line);
            }
        } else {
            p.ignore.push(line);
        }
    }

    p.locations.sort();

    Ok(p)
}

fn parse_error(line: &str) -> Result<Profile, io::Error> {
    return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("can't parse line: {}", line)));
}
