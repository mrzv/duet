use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
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

fn canonical_relative_path(path: &Path) -> PathBuf {
    let mut canonical = PathBuf::new();
    for component in path.components() {
        if !matches!(component, Component::CurDir) {
            canonical.push(component.as_os_str());
        }
    }
    canonical
}

/// Canonicalize relative rule paths and retain the last rule for each path.
pub fn canonicalize(locations: &Locations) -> Locations {
    let mut winners = BTreeMap::new();
    for location in locations {
        let path = canonical_relative_path(location.path());
        let canonical = if location.is_include() {
            Location::Include(path.clone())
        } else {
            Location::Exclude(path.clone())
        };
        winners.insert(path, canonical);
    }
    winners.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_treats_root_paths_as_equivalent_and_keeps_later_rule() {
        let locations = canonicalize(&vec![
            Location::Exclude(PathBuf::from(".")),
            Location::Exclude(PathBuf::new()),
            Location::Include(PathBuf::from("./.")),
        ]);

        assert_eq!(locations.len(), 1);
        assert!(locations[0].is_include());
        assert_eq!(locations[0].path(), &PathBuf::new());
    }

    #[test]
    fn canonicalize_keeps_one_later_winner_per_equivalent_path() {
        let locations = canonicalize(&vec![
            Location::Include(PathBuf::from("dir/./nested")),
            Location::Exclude(PathBuf::from("other")),
            Location::Exclude(PathBuf::from("dir/nested")),
        ]);

        assert_eq!(locations.len(), 2);
        assert!(locations[0].is_exclude());
        assert_eq!(locations[0].path(), &PathBuf::from("dir/nested"));
        assert_eq!(locations[1].path(), &PathBuf::from("other"));
    }
}
