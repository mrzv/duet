use std::fs::File;
use std::io::{self, prelude::*, BufReader};
use std::path::{PathBuf};

use shellexpand;

use crate::scan::location::{Location,Locations};

pub type Ignore = Vec<String>;

#[derive(Debug)]
pub struct Profile {
    pub local:      String,
    pub remote:     String,
    pub locations:  Locations,
    pub ignore:     Ignore,
}

pub fn location(name: &str) -> PathBuf {
    let mut base = PathBuf::from(shellexpand::full("~/.config/duet/").unwrap().to_string());
    base.push(name.to_owned() + ".prf");
    base
}

pub fn local_state(name: &str) -> PathBuf {
    let mut profile_location = location(name);
    profile_location.set_extension("snp");
    profile_location
}

pub fn remote_state_dir() -> PathBuf {
    let mut base = PathBuf::from(shellexpand::full("~/.config/duet/").unwrap().to_string());
    base.push("remotes");
    base
}

pub fn remote_state(id: &str) -> PathBuf {
    let mut base = remote_state_dir();
    base.push(id);
    base
}

pub fn parse(name: &str) -> Result<Profile, io::Error> {
    let profile_location = location(name);
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
