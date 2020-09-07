use std::fs::File;
use std::io::{self, prelude::*, BufReader};
use std::path::{PathBuf};

use shellexpand;

use crate::scan::location::{Location,Locations};

#[derive(Debug)]
pub struct Profile {
    pub local:      String,
    pub remote:     String,
    pub locations:  Locations,
    pub ignore:     Vec<String>,
}

pub fn location(name: &str) -> PathBuf {
    let mut base = PathBuf::from(shellexpand::full("~/.config/duet/").unwrap().to_string());
    base.push(name);
    base
}

pub fn local_state(name: &str) -> PathBuf {
    let mut profile_location = location(name);
    profile_location.push("local_state");
    profile_location
}

pub fn remote_state(id: &str) -> PathBuf {
    let mut base = PathBuf::from(shellexpand::full("~/.config/duet/").unwrap().to_string());
    base.push("remotes");
    base.push(id);
    base
}

pub fn parse(name: &str) -> Result<Profile, io::Error> {
    let mut profile_location = location(name);
    profile_location.push("profile");
    log::debug!("Loading {:?}", profile_location);

    let f      = File::open(profile_location)?;
    let reader = BufReader::new(f);

    let mut p = Profile {
        local:      String::new(),
        remote:     String::new(),
        locations:  vec![Location::Exclude(PathBuf::from("."))],    // implicitly exclude .
        ignore:     Vec::new(),
    };

    let mut locations = 0;
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if locations == 0 {
            p.local = line.to_string();
            locations += 1;
            continue;
        } else if locations == 1 {
            p.remote = line;
            locations += 1;
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

    Ok(p)
}

fn parse_error(line: &str) -> Result<Profile, io::Error> {
    return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("can't parse line: {}", line)));
}
