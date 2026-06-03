use std::fs::File;
use std::io::{self, prelude::*, BufReader};
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileSource {
    Named(String),
    File(PathBuf),
}

#[derive(Debug)]
pub struct ProfileConfig {
    pub display_name: String,
    pub identity: String,
    pub profile: Profile,
    pub local_state: PathBuf,
    pub remote_state_dir: PathBuf,
    pub server_log: PathBuf,
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

pub fn remote_state_in(dir: &Path, id: &str) -> PathBuf {
    dir.join(id)
}

pub fn load(source: &ProfileSource) -> Result<ProfileConfig, io::Error> {
    match source {
        ProfileSource::Named(name) => Ok(ProfileConfig {
            display_name: name.clone(),
            identity: name.clone(),
            profile: parse(name)?,
            local_state: local_state(name),
            remote_state_dir: remote_state_dir(),
            server_log: default_server_log(),
        }),
        ProfileSource::File(path) => {
            let path = std::fs::canonicalize(path)?;
            let display_name = path.display().to_string();
            let mut local_state = path.clone();
            local_state.set_extension("snp");
            let mut remote_state_dir = path.clone();
            remote_state_dir.set_extension("remotes");
            let server_log = remote_state_dir.join("remote.log");

            Ok(ProfileConfig {
                display_name,
                identity: path.display().to_string(),
                profile: parse_file(&path)?,
                local_state,
                remote_state_dir,
                server_log,
            })
        }
    }
}

fn default_server_log() -> PathBuf {
    let mut base = PathBuf::from(shellexpand::full("~/.config/duet/").unwrap().to_string());
    base.push("remote.log");
    base
}

pub fn parse(name: &str) -> Result<Profile, io::Error> {
    let profile_location = location(name);
    log::debug!("Loading {:?}", profile_location);

    parse_file(&profile_location)
}

pub fn parse_file(profile_location: &Path) -> Result<Profile, io::Error> {
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
