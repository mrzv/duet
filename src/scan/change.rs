use std::cmp::Ordering;
use std::fmt;
use std::path::{PathBuf};
use colored::*;

use serde::{Serialize,Deserialize};

use crate::utils::{match_sorted,MatchSorted};

use super::DirEntryWithMeta;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Change {
    Added(DirEntryWithMeta),
    Removed(DirEntryWithMeta),
    Modified(DirEntryWithMeta, DirEntryWithMeta),
}

impl Change {
    pub fn path(&self) -> &PathBuf {
        match self {
            Change::Added(dir)    => &dir.path,
            Change::Removed(dir)  => &dir.path,
            Change::Modified(dir1,dir2) => { assert_eq!(dir1.path, dir2.path); &dir2.path},
        }
    }

    pub fn is_dir(&self) -> bool {
        match self {
            Change::Added(e)    => e.is_dir(),
            Change::Removed(e)  => e.is_dir(),
            Change::Modified(e1,e2) => e1.is_dir() || e2.is_dir(),
        }
    }
}

pub fn same(x: &Change, y: &Change) -> bool {
    match (x,y) {
        (Change::Removed(_), Change::Removed(_)) => true,
        (Change::Added(d1), Change::Added(d2)) => {
            assert_eq!(d1.path, d2.path);
            d1.size == d2.size && d1.mode == d2.mode && d1.target == d2.target && d1.is_dir == d2.is_dir
                && (d1.is_dir || d1.mtime == d2.mtime)
        },
        (Change::Modified(_,d1), Change::Modified(_,d2)) => {
            assert_eq!(d1.path, d2.path);
            d1.size == d2.size && d1.mode == d2.mode && d1.target == d2.target && d1.is_dir == d2.is_dir
                && (d1.is_dir || d1.mtime == d2.mtime)
        },
        _ => false,
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
            Change::Added(_)      => write!(f, "{}", "+".green()),
            Change::Removed(_)    => write!(f, "{}", "-".red()),
            Change::Modified(_,_) => write!(f, "{}", "M".yellow()),
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
