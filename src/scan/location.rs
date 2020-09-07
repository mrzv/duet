use std::path::{PathBuf};
use std::cmp::Ordering;
use std::fmt;
use savefile_derive::Savefile;

#[derive(Debug, Clone, Savefile)]
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
