use std::fmt;
use std::path::{PathBuf};
use colored::*;
use serde::{Serialize,Deserialize};
use super::scan::change::{Change,same};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Local(Change),
    Remote(Change),
    Conflict(Change,Change),
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
        if let Action::Conflict(_,_) = self {
            true
        } else {
            false
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
            Action::Identical(l,_r) => l.path(),
        }
    }
}

pub fn num_conflicts(actions: &Vec<Action>) -> usize {
    actions.iter()
        .filter(|a| match a {
            Action::Conflict(_,_) => true,
            _ => false
        })
        .count()
}

pub fn num_identical(actions: &Vec<Action>) -> usize {
    actions.iter()
        .filter(|a| match a {
            Action::Identical(_,_) => true,
            _ => false
        })
        .count()
}

pub fn reverse(actions: &Vec<Action>) -> Vec<Action> {
    actions.iter()
        .map(|a| match a {
            Action::Local(l) => Action::Remote(l.clone()),
            Action::Remote(r) => Action::Local(r.clone()),
            Action::Conflict(l,r) => Action::Conflict(r.clone(),l.clone()),
            Action::Identical(l,r) => Action::Identical(r.clone(),l.clone()),
        })
        .collect()
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            // actions are reversed:
            // local action means remote change, and remote action means local change
            Action::Local(l)        => write!(f, "  <---- {} {}", l, l.path().display()),
            Action::Remote(r)       => write!(f, "{} ---->   {}", r, r.path().display()),
            Action::Conflict(l,r)   => write!(f, "{} {} {} {}", l, "<--->".bright_red(), r, l.path().display()),
            Action::Identical(l,r)  => write!(f, "{} --I-- {} {}", l, r, l.path().display()),
        }
    }
}
