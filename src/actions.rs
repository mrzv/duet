use std::fmt;
use super::scan::change::{Change,same};

pub enum Action {
    Local(Change),
    Remote(Change),
    Conflict(Change,Change),
}

impl Action {
    pub fn create(loc: Option<&Change>, roc: Option<&Change>) -> Option<Action> {
        match (loc,roc) {
            (Some(lc), None) => Some(Action::Remote(lc.clone())),
            (None, Some(rc)) => Some(Action::Local(rc.clone())),
            (Some(lc), Some(rc)) => {
                if same(lc,rc) {
                    None
                } else {
                    Some(Action::Conflict(lc.clone(),rc.clone()))
                }
            }
            (None,None) => None,
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            Action::Local(l)        => write!(f, "<--- {}", l),
            Action::Remote(r)       => write!(f, "---> {}", r),
            Action::Conflict(l,r)  => write!(f, "{} <--> {}", l, r),
        }
    }
}
