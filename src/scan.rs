use std::path::{PathBuf};

use std::os::unix::fs::MetadataExt;
use jwalk::{WalkDir};
pub use jwalk::Error;

use colored::*;

//use glob;

use crate::profile::Profile;

// TODO: restrict to same file system

pub fn scan(prf: &Profile, path: &Option<&str>) -> Result<(), Error> {
    let mut to_scan = PathBuf::from(&prf.local);
    if let Some(path) = path {
        to_scan.push(path);
    }

    println!("Going to scan: {}", to_scan.display());

    for entry in WalkDir::new(&to_scan).skip_hidden(false).sort(true) {
      let entry = entry?;
      let meta = entry.metadata()?;
      println!("{} {} {:?}", meta.ino().to_string().magenta(), entry.path().display().to_string().green(), meta);
    }

    Ok(())
}
