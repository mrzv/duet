use std::cmp::Ordering;
use std::fmt;
use colored::*;

use crate::utils::{match_sorted,MatchSorted};

use super::DirEntryWithMeta;

#[derive(Debug, Clone)]
pub enum Change {
    Added(DirEntryWithMeta),
    Removed(DirEntryWithMeta),
    Modified(DirEntryWithMeta, DirEntryWithMeta),
}

impl Change {
    pub fn path(&self) -> &String {
        match self {
            Change::Added(dir)    => &dir.path,
            Change::Removed(dir)  => &dir.path,
            Change::Modified(dir1,dir2) => { assert_eq!(dir1.path, dir2.path); &dir2.path},
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
            Change::Added(_)    => write!(f, "+ {}", self.path().green()),
            Change::Removed(_)  => write!(f, "- {}", self.path().red()),
            Change::Modified(_,_) => write!(f, "M {}", self.path()),
        }
    }
}

pub struct ChangesIterator<'a, I1, I2>
    where
        I1: Iterator<Item = &'a DirEntryWithMeta>,
        I2: Iterator<Item = &'a DirEntryWithMeta>,
{
    it: MatchSorted<I1,I2,&'a DirEntryWithMeta>,
}

impl<'a, I1,I2> Iterator for ChangesIterator<'a, I1, I2>
    where
        I1: Iterator<Item = &'a DirEntryWithMeta>,
        I2: Iterator<Item = &'a DirEntryWithMeta>,
{
    type Item = Change;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let x = self.it.next();
            if let None = x {
                return None;
            }
            let (a,b) = x.unwrap();
            match (a,b) {
                (Some(a), None) => break Some(Change::Removed(a.clone())),
                (None, Some(b)) => break Some(Change::Added(b.clone())),
                (Some(a), Some(b)) => {
                    if !a.same(b) {
                        break Some(Change::Modified(a.clone(), b.clone()))
                    } else {
                        continue;
                    }
                },
                (None, None) => continue,
            }
        }
    }
}


pub fn changes<'a, I1,I2>(it1: I1, it2: I2) -> ChangesIterator<'a, I1, I2>
    where
        I1: Iterator<Item = &'a DirEntryWithMeta>,
        I2: Iterator<Item = &'a DirEntryWithMeta>,
{
    ChangesIterator { it: match_sorted(it1, it2) }
}
