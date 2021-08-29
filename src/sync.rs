use color_eyre::eyre::{Result,eyre};
use std::fs;
use std::io::Write;
use std::path::{Path,PathBuf};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::cmp::Ordering;
use serde::{Serialize,Deserialize};
use super::scan::{Change, DirEntryWithMeta as Entry};

use crate::actions::Action;

pub use rustsync::{Signature,Delta};
use rustsync::{signature,compare,restore_seek};

const WINDOW: usize = 1024;       // TODO: figure out appropriate window size

#[derive(Debug, Serialize, Deserialize)]
pub struct SignatureWithPath(String, Signature);

pub fn get_signatures(base: &str, actions: &Vec<Action>) -> Result<Vec<SignatureWithPath>> {
    let base_path = Path::new(base);
    let mut signatures: Vec<SignatureWithPath> = Vec::new();
    for action in actions {
        if let Action::Local(Change::Modified(e1, e2)) = action {
            if !e1.same_contents(&e2) && !e2.is_dir() && !e2.is_symlink() {
                let f = fs::File::open(base_path.join(e1.path()))?;
                let block = [0; WINDOW];
                let sig = signature(f, block)?;
                signatures.push(SignatureWithPath(e1.path().to_string(), sig));
            }
        }
    }
    Ok(signatures)
}



#[derive(Debug, Serialize, Deserialize)]
pub enum ChangeDetails {
    Contents(Vec<u8>),
    Diff(Delta),
}

pub fn get_detailed_changes(base: &str, actions: &Vec<Action>, signatures: &Vec<SignatureWithPath>) -> Result<Vec<ChangeDetails>> {
    let base_path = Path::new(base);
    let mut sig_iter = signatures.iter();
    let mut details: Vec<ChangeDetails> = Vec::new();

    for action in actions {
        if let Action::Remote(change) = action {
            match change {
                Change::Removed(_) => {},
                Change::Added(e) => {
                    if !e.is_dir() && !e.is_symlink() {
                        log::debug!("Getting detail for adding {}", e.path());
                        let v = fs::read(base_path.join(e.path()))?;
                        details.push(ChangeDetails::Contents(v));
                    }
                },
                Change::Modified(e1,e2) => {
                    if !e1.same_contents(&e2) && !e2.is_dir() && !e2.is_symlink() {
                        let block = [0; WINDOW];
                        let f = fs::File::open(base_path.join(e1.path()))?;
                        let sig = &sig_iter.next().unwrap().1;
                        let delta = compare(sig, f, block)?;
                        details.push(ChangeDetails::Diff(delta))
                    } // else: permissions or target change
                }
            }
        }
    }
    Ok(details)
}

// TODO:
//  - bi-directional changes to a directory:
//    - when files change in both directions, we get a conflict on the directory
//    - even when the user resolves the conflict, only one side gets updated
//    - as a result, the next sync, we get a conflict on the directory
//  - the correct solution is probably to auto-resolve a directory conflict that stems from
//    differing mtimes
//  - currently doesn't

pub fn apply_detailed_changes(base: &str, actions: &Vec<Action>, details: &Vec<ChangeDetails>, all_old: &mut Vec<Entry>) -> Result<()> {
    let base_path = Path::new(base);
    log::debug!("details.len() = {}", details.len());
    let mut details_iter = details.iter();
    let mut new_entries: Vec<Entry> = Vec::new();
    let mut old_iter = all_old.iter().peekable();

    for action in actions {
        let path = action.path();
        loop {
            let oe = old_iter.peek();
            if let Some(e) = oe {
                match e.path().cmp(path) {
                    Ordering::Less => { new_entries.push(old_iter.next().unwrap().clone()); },
                    Ordering::Equal => {
                        let e = old_iter.next().unwrap();
                        if let Action::Conflict(_,_) = action {
                            new_entries.push(e.clone());       // preserve the original
                        }
                        continue;
                    },       // action will deal with this
                    Ordering::Greater => { break; },
                }
            } else {
                break;
            }
        }

        log::debug!("applying detailed change to {}", action.path());
        match action {
            Action::Local(change) => {
                match change {
                    Change::Removed(e) => {
                        let filename = base_path.join(e.path());
                        log::debug!("Removing {:?}", filename);
                        if !e.is_dir() {
                            fs::remove_file(&filename).expect(format!("failed to remove file {:?}", filename).as_str());
                        } // else: removing directory;
                          //   must happen after all the files have been removed, which will happen
                          //   in the second pass
                        // nothing gets copied into new_entries
                    },
                    Change::Added(e) => {
                        let filename = base_path.join(e.path());
                        let new_entry =
                            if let Some(p) = e.target() {
                                std::os::unix::fs::symlink(p, &filename).expect(format!("failed to create symlink {:?} {:?}", p, filename).as_str());
                                update_meta(&filename, e).expect(format!("failed to update metadata for {:?}", filename).as_str())
                            } else if e.is_dir() {
                                fs::create_dir(&filename).expect(format!("failed to create directory {:?}", filename).as_str());
                                update_meta(&filename, e).expect(format!("failed to update metadata for {:?}", filename).as_str())
                            } else {
                                log::debug!("Adding {}", e.path());
                                let detail = &details_iter.next().unwrap();
                                let mut f = fs::File::create(&filename)?;
                                match detail {
                                    ChangeDetails::Contents(v) => {
                                        f.write_all(v)?;
                                    },
                                    _ => { return Err(eyre!("mismatch when adding {}, expected Contents, but not found", e.path())); }
                                }
                                update_meta(&filename, e).expect(format!("failed to update metadata for {:?}", filename).as_str())
                            };
                        new_entries.push(new_entry);
                    },
                    Change::Modified(e1,e2) => {
                        let filename = base_path.join(e2.path());
                        let new_entry =
                            if !e1.same_contents(&e2) && !e2.is_dir() && !e2.is_symlink() {
                                let detail = &details_iter.next().unwrap();
                                match detail {
                                    ChangeDetails::Diff(delta) => {
                                        let block = [0; WINDOW];
                                        let mut updated = Vec::new();
                                        restore_seek(&mut updated, fs::File::open(&filename)?, block, &delta)?;
                                        let mut f = fs::File::create(&filename)?;
                                        f.write_all(&updated)?;
                                        update_meta(&filename, e2)?
                                    },
                                    _ => { return Err(eyre!("mismatch when adding {}, expected Diff, but not found", e1.path())) }
                                }
                            } else {
                                // TODO: won't work if we go from a directory to a symlink
                                if let Some(p) = e2.target() {
                                    fs::remove_file(&filename).expect(format!("failed to remove file {:?} when updating symlink", filename).as_str());
                                    std::os::unix::fs::symlink(p, &filename).expect(format!("failed to create symlink {:?} {:?}", p, filename).as_str());
                                }
                                update_meta(&filename, e2).expect(format!("failed to update metadata for {:?}", filename).as_str())
                            };
                        new_entries.push(new_entry);
                    }
                }
            },
            Action::Remote(change) => {
                match change {
                    Change::Removed(_) => {},
                    Change::Added(e) => {
                        new_entries.push(e.clone());
                    },
                    Change::Modified(_,e) => {
                        new_entries.push(e.clone());
                    }
                }
            },
            Action::Identical(change, _) => {
                match change {
                    Change::Removed(_) => {},
                    Change::Added(e) => {
                        new_entries.push(e.clone());
                    },
                    Change::Modified(_,e) => {
                        new_entries.push(e.clone());
                    }
                }
            },
            Action::Conflict(_,_) => {},        // skip conflicts; only way we get here with them, if we are in the batch force mode
        }
    }

    // TODO: think how directory removal interacts with "ignore", if we ever implement it

    // second pass, in reverse order, to remove directories and update their metadata
    for action in actions.iter().rev() {
        if let Action::Local(change) = action {
            if !change.is_dir() {
                continue;
            }
            match change {
                Change::Removed(e) => {
                    let dirname = base_path.join(e.path());
                    fs::remove_dir(&dirname).expect(format!("failed to remove directory {:?}", dirname).as_str());
                },
                Change::Added(e) => {
                    let dirname = base_path.join(e.path());
                    update_meta(&dirname, e)?;
                },
                Change::Modified(_e1,e2) => {
                    let dirname = base_path.join(e2.path());
                    update_meta(&dirname, e2)?;
                },
            }
        }
    }

    // copy remaining entries from all_old
    for e in old_iter {
        new_entries.push(e.clone());
    }

    std::mem::swap(all_old, &mut new_entries);

    Ok(())
}

fn update_meta(path: &PathBuf, e: &Entry) -> Result<Entry> {
    let meta = fs::symlink_metadata(path).expect(format!("failed to acquire metadata for {:?}", path).as_str());
    meta.permissions().set_mode(e.mode());
    filetime::set_symlink_file_times(path, filetime::FileTime::from_unix_time(meta.atime(),0), filetime::FileTime::from_unix_time(e.mtime(),0))?;
    let mut new_entry = e.clone();
    new_entry.set_ino(meta.ino());
    Ok(new_entry)
}
