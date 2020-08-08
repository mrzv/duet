use std::fs::File;
use std::io::{self, prelude::*, BufReader};

use shellexpand;

pub struct Profile {
    pub local:   String,
    pub remote:  String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub ignore:  Vec<String>,
}

pub fn parse(name: &str) -> Result<Profile, io::Error> {
    let f      = File::open(name)?;
    let reader = BufReader::new(f);

    let mut p = Profile {
        local:   String::new(),
        remote:  String::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        ignore:  Vec::new(),
    };

    let mut locations = 0;
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if locations == 0 {
            if let Ok(line) = shellexpand::full(&line) {
                p.local = line.to_string();
                locations += 1;
                continue;
            } else {
                return parse_error(&line);
            }
        } else if locations == 1 {
            p.remote = line;
            locations += 1;
            continue;
        }

        // includes/excludes
        if locations == 2 {
            if line.trim().starts_with('+') {
                p.include.push(line);
            } else if line.trim().starts_with('-') {
                p.exclude.push(line);
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
