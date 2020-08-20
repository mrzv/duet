use std::cmp::Ordering;
use std::fmt;

use super::DirEntryWithMeta;

pub enum Change {
    Added(DirEntryWithMeta),
    Removed(DirEntryWithMeta),
    Modified(DirEntryWithMeta),
}

impl Change {
    pub fn path(&self) -> &String {
        match self {
            Change::Added(dir)    => &dir.path,
            Change::Removed(dir)  => &dir.path,
            Change::Modified(dir) => &dir.path,
        }
    }
}

impl Ord for Change {
    fn cmp(&self, other: &Self) -> Ordering {
        self.path().cmp(other.path())
    }
}

impl PartialOrd for Change {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Change {
    fn eq(&self, other: &Self) -> bool {
        self.path() == other.path()
    }
}

impl Eq for Change { }

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            Change::Added(_)    => write!(f, "A {}", self.path()),
            Change::Removed(_)  => write!(f, "R {}", self.path()),
            Change::Modified(_) => write!(f, "M {}", self.path()),
        }
    }
}
