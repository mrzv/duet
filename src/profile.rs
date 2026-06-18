use std::fs::File;
use std::io::{self, prelude::*, BufReader};
use std::path::{Component, Path, PathBuf};

use shellexpand;

use crate::scan::location::{Location, Locations};

pub type Ignore = Vec<String>;

#[derive(Debug)]
pub struct Profile {
    pub local: String,
    pub remote: String,
    pub locations: Locations,
    pub ignore: Ignore,
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

fn config_dir() -> Result<PathBuf, io::Error> {
    let expanded = shellexpand::full("~/.config/duet/").map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unable to expand ~/.config/duet/: {}", e),
        )
    })?;
    Ok(PathBuf::from(expanded.into_owned()))
}

pub fn location(name: &str) -> Result<PathBuf, io::Error> {
    validate_profile_name(name)?;
    let mut base = config_dir()?;
    base.push(name.to_owned() + ".prf");
    Ok(base)
}

fn validate_profile_name(name: &str) -> Result<(), io::Error> {
    if name.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid profile name: {}", name),
        ));
    }

    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) if name != "." && name != ".." => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid profile name: {}", name),
        )),
    }
}

pub fn local_state(name: &str) -> Result<PathBuf, io::Error> {
    let mut profile_location = location(name)?;
    profile_location.set_extension("snp");
    Ok(profile_location)
}

pub fn remote_state_dir() -> Result<PathBuf, io::Error> {
    let mut base = config_dir()?;
    base.push("remotes");
    Ok(base)
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
            local_state: local_state(name)?,
            remote_state_dir: remote_state_dir()?,
            server_log: default_server_log()?,
        }),
        ProfileSource::File(path) => {
            let path = std::fs::canonicalize(path)?;
            let display_name = path.display().to_string();
            let mut local_state = path.clone();
            local_state.set_extension("snp");
            let mut remote_state_dir = path.clone();
            remote_state_dir.set_extension("remotes");
            let mut server_log = path.clone();
            server_log.set_extension("remote.log");

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

fn default_server_log() -> Result<PathBuf, io::Error> {
    let mut base = config_dir()?;
    base.push("remote.log");
    Ok(base)
}

pub fn parse(name: &str) -> Result<Profile, io::Error> {
    let profile_location = location(name)?;
    log::debug!("Loading {:?}", profile_location);

    parse_file(&profile_location)
}

pub fn parse_file(profile_location: &Path) -> Result<Profile, io::Error> {
    log::debug!("Loading {:?}", profile_location);

    let f = File::open(profile_location)?;
    let reader = BufReader::new(f);

    let mut p = Profile {
        local: String::new(),
        remote: String::new(),
        locations: vec![Location::Exclude(PathBuf::from("."))], // implicitly exclude .
        ignore: Vec::new(),
    };

    let mut locations = 0;
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
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
            if let Some(path) = trimmed.strip_prefix('+') {
                p.locations
                    .push(Location::Include(PathBuf::from(path.trim())));
            } else if let Some(path) = trimmed.strip_prefix('-') {
                p.locations
                    .push(Location::Exclude(PathBuf::from(path.trim())));
            } else if trimmed == "[ignore]" {
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
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("can't parse line: {}", line),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rejects_profile_names_with_path_components() {
        assert!(location("work").is_ok());
        assert!(location("../work").is_err());
        assert!(location("/tmp/work").is_err());
        assert!(location("work/other").is_err());
        assert!(location("work\\other").is_err());
        assert!(location(".").is_err());
        assert!(location("..").is_err());
    }

    #[test]
    fn parses_include_exclude_markers_after_leading_whitespace() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "/local").unwrap();
        writeln!(file, "remote /remote").unwrap();
        writeln!(file, "  +src").unwrap();
        writeln!(file, "  -target").unwrap();
        writeln!(file, "  [ignore]").unwrap();
        writeln!(file, "*.tmp").unwrap();

        let profile = parse_file(file.path()).unwrap();

        assert!(matches!(
            &profile.locations[1],
            Location::Include(path) if path == &PathBuf::from("src")
        ));
        assert!(matches!(
            &profile.locations[2],
            Location::Exclude(path) if path == &PathBuf::from("target")
        ));
        assert_eq!(profile.ignore, vec!["*.tmp".to_string()]);
    }
}
