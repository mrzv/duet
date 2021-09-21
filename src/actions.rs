use std::fmt;
use std::path::{PathBuf};
use colored::*;
use serde::{Serialize,Deserialize};
use super::scan::change::{Change,same};
use super::scan::{DirEntryWithMeta as Entry};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Local(Change),
    Remote(Change),
    Conflict(Change,Change),
    ResolvedLocal((Change,Change), Change),
    ResolvedRemote((Change,Change), Change),
    Identical(Change,Change),       // need for bookkeeping
}

pub type Actions = Vec<Action>;

impl Action {
    pub fn create(loc: Option<&Change>, roc: Option<&Change>) -> Option<Action> {
        match (loc,roc) {
            (Some(lc), None) => Some(Action::Remote(lc.clone())),
            (None, Some(rc)) => Some(Action::Local(rc.clone())),
            (Some(lc), Some(rc)) => {
                if same(lc,rc) {
                    Some(Action::Identical(lc.clone(),rc.clone()))
                } else {
                    Some(Action::Conflict(lc.clone(),rc.clone()))
                }
            }
            (None,None) => None,
        }
    }

    pub fn is_conflict(&self) -> bool {
        match self {
            Action::Conflict(_,_) | Action::ResolvedLocal((_,_),_) | Action::ResolvedRemote((_,_),_) => true,
            _ => false,
        }
    }

    pub fn is_unresolved_conflict(&self) -> bool {
        match self {
            Action::Conflict(_,_) => true,
            _ => false,
        }
    }


    pub fn is_identical(&self) -> bool {
        if let Action::Identical(_,_) = self {
            true
        } else {
            false
        }
    }

    pub fn path(&self) -> &PathBuf {
        match self {
            Action::Local(l) => l.path(),
            Action::Remote(r) => r.path(),
            Action::Conflict(l,_r) => l.path(),
            Action::ResolvedLocal((_,_),l) => l.path(),
            Action::ResolvedRemote((_,_),r) => r.path(),
            Action::Identical(l,_r) => l.path(),
        }
    }
}

pub fn num_unresolved_conflicts<'a,I>(actions: I) -> usize
where
    I: Iterator<Item = &'a Action>,
{
    actions
        .filter(|a| a.is_unresolved_conflict())
        .count()
}

pub fn num_identical<'a,I>(actions: I) -> usize
where
    I: Iterator<Item = &'a Action>,
{
    actions
        .filter(|a| a.is_identical())
        .count()
}

pub fn reverse(actions: &Vec<Action>) -> Vec<Action> {
    actions.iter()
        .map(|a| match a {
            Action::Local(l) => Action::Remote(l.clone()),
            Action::Remote(r) => Action::Local(r.clone()),
            Action::Conflict(l,r) => Action::Conflict(r.clone(),l.clone()),
            Action::ResolvedLocal((o,n),l) => Action::ResolvedRemote((o.clone(), n.clone()), l.clone()),
            Action::ResolvedRemote((o,n),r) => Action::ResolvedLocal((o.clone(), n.clone()), r.clone()),
            Action::Identical(l,r) => Action::Identical(r.clone(),l.clone()),
        })
        .collect()
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            // actions are reversed:
            // local action means remote change, and remote action means local change
            Action::Local(l)                => write!(f, "  <---- {} {}", l, l.path().display()),
            Action::Remote(r)               => write!(f, "{} ---->   {}", r, r.path().display()),
            Action::Conflict(l,r)           => write!(f, "{} {} {} {}", l, "<===>".bright_red(), r, l.path().display()),
            Action::ResolvedLocal((_,_),l)  => write!(f, "  <==== {} {}", l, l.path().display()),
            Action::ResolvedRemote((_,_),r) => write!(f, "{} ====>   {}", r, r.path().display()),
            Action::Identical(l,r)          => write!(f, "{} --I-- {} {}", l, r, l.path().display()),
        }
    }
}

pub fn details(action: &Action) -> String {
    match action {
            Action::Local(c) => {
                match c {
                    Change::Added(d) | Change::Removed(d) => {
                        format!("{}", show_meta(d, d))
                    },
                    Change::Modified(o,n) => {
                        format!("{}      {}", show_meta(o, o), show_meta(n, o))           // remote is new, local is old
                    },
                }
            },
            Action::Remote(c) => {
                match c {
                    Change::Added(d) | Change::Removed(d) => {
                        format!("{}", show_meta(d, d))
                    },
                    Change::Modified(o,n) => {
                        format!("{}      {}", show_meta(n,o), show_meta(o,o))           // local is new, remote is old
                    },
                }
            },
            Action::Conflict(l,r)
            | Action::ResolvedLocal((l,r),_)
            | Action::ResolvedRemote((l,r),_) => {
                format!("{}      {}", show_meta(change_entry(l),change_entry(r)), show_meta(change_entry(r),change_entry(l)))
            },
            Action::Identical(l,_) => format!("{}", show_meta(change_entry(l),change_entry(l))),
    }
}

fn change_entry(change: &Change) -> &Entry {
    match change {
        Change::Added(d) | Change::Removed(d) | Change::Modified(_,d) => {
            d
        }
    }
}

fn highlight(s: String, status: bool) -> ColoredString {
    if status {
        s.yellow()
    } else {
        s.normal()
    }
}

fn show_meta(e: &Entry, before: &Entry) -> String {
    if e.is_symlink() {
        // permissions don't matter
        format!("{} -> {}", highlight(show_mtime(e), e.mtime() != before.mtime()),
                            highlight(e.target().as_ref().unwrap().to_string(), e.target() != before.target()))
    } else if e.is_dir() {
        format!("{} {}", highlight(show_permissions(e), e.mode() != before.mode()),
                         highlight(show_mtime(e), e.mtime() != before.mtime()))
    } else {
        format!("{} {} {} {}", highlight(show_permissions(e), e.mode() != before.mode()),
                               highlight(show_mtime(e), e.mtime() != before.mtime()),
                               highlight(show_size(e), e.size() != before.size()),
                               highlight(show_checksum(e), e.checksum() != before.checksum()))
    }
}

fn show_mtime(e: &Entry) -> String {
    let mtime = e.mtime();
    use chrono::prelude::*;
    let date_time = NaiveDateTime::from_timestamp(mtime, 0);
    let date_time = DateTime::<Utc>::from_utc(date_time, Utc).with_timezone(&Local);
    date_time.format("%a %Y-%m-%d %H:%M:%S").to_string()
}

fn show_permissions(e: &Entry) -> String {
    let mode = e.mode();
    unix_mode::to_string(mode)
}

fn show_size(e: &Entry) -> String {
    let size = e.size();
    format!("{:>10}", byte_unit::Byte::from_bytes(size.into()).get_appropriate_unit(true).to_string())
}

fn show_checksum(e: &Entry) -> String {
    let checksum = e.checksum();
    format!("{:08x}", checksum)
}
