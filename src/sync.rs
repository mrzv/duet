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
pub struct SignatureWithPath(PathBuf, Signature);

pub fn get_signatures(base: &str, actions: &Vec<Action>) -> Result<Vec<SignatureWithPath>> {
    let base_path = Path::new(base);
    let mut signatures: Vec<SignatureWithPath> = Vec::new();
    for action in actions {
        match action {
            Action::Local(Change::Modified(e1, e2)) | Action::ResolvedLocal((_,_), Change::Modified(e1,e2)) =>
            {
                if e1.is_file() && e2.is_file() && !e1.same_contents(&e2) {
                    let f = fs::File::open(base_path.join(e1.path()))?;
                    let block = [0; WINDOW];
                    let sig = signature(f, block)?;
                    signatures.push(SignatureWithPath(e1.path().clone(), sig));
                }
            },
            _ => {}
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
        match action {
            Action::Remote(change) | Action::ResolvedRemote((_,_),change) => {
                match change {
                    Change::Removed(_) => {},
                    Change::Added(e) => {
                        if e.is_file() {
                            log::debug!("Getting detail for adding {}", e.path().display());
                            let v = fs::read(base_path.join(e.path()))?;
                            details.push(ChangeDetails::Contents(v));
                        }
                    },
                    Change::Modified(e1,e2) => {
                        if e1.is_file() && e2.is_file() && !e1.same_contents(&e2) {
                            let block = [0; WINDOW];
                            let f = fs::File::open(base_path.join(e1.path()))?;
                            let sig = &sig_iter.next().unwrap().1;
                            let delta = compare(sig, f, block)?;
                            details.push(ChangeDetails::Diff(delta))
                        } else if !e1.is_file() && e2.is_file() {
                            let v = fs::read(base_path.join(e2.path()))?;
                            details.push(ChangeDetails::Contents(v));
                        } // else: permissions or target change
                    }
                }
            },
            _ => {}
        }
    }
    Ok(details)
}

pub fn apply_detailed_changes(base: &str, actions: &Vec<Action>, details: &Vec<ChangeDetails>, all_old: &mut Vec<Entry>) -> Result<()> {
    let base_path = Path::new(base);
    log::debug!("details.len() = {}", details.len());
    let mut details_iter = details.iter();
    let mut new_entries: Vec<Entry> = Vec::new();
    let mut old_iter = all_old.iter().peekable();
    let mut leftover_details: Vec<&ChangeDetails> = Vec::new();

    for action in actions {
        let path = action.path();
        loop {
            let oe = old_iter.peek();
            if let Some(e) = oe {
                match e.path().cmp(path) {
                    Ordering::Less => { new_entries.push(old_iter.next().unwrap().clone()); },
                    Ordering::Equal => {
                        let e = old_iter.next().unwrap();
                        if action.is_unresolved_conflict() {
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

        match action {
            Action::Local(change) | Action::ResolvedLocal((_,_),change) => {
            log::debug!("applying detailed change to {}", action.path().display());
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
                        if let Some(p) = e.target() {
                            std::os::unix::fs::symlink(p, &filename).expect(format!("failed to create symlink {:?} {:?}", p, filename).as_str());
                            new_entries.push(update_meta(&filename, e).expect(format!("failed to update metadata for {:?}", filename).as_str()));
                        } else if e.is_dir() {
                            fs::create_dir(&filename).expect(format!("failed to create directory {:?}", filename).as_str());
                            // new entry gets updated in the second pass, after all the updates in
                            // the directory are finished
                        } else {
                            log::debug!("Adding {}", e.path().display());
                            let detail = &details_iter.next().unwrap();
                            create_file(&filename, &detail).expect(format!("failed to create file {:?}", filename).as_str());
                            new_entries.push(update_meta(&filename, e).expect(format!("failed to update metadata for {:?}", filename).as_str()));
                        }
                    },
                    Change::Modified(e1,e2) => {
                        let filename = base_path.join(e2.path());
                        if e1.is_file() {
                            if e2.is_file() {
                                if !e1.same_contents(&e2) {
                                    let detail = &details_iter.next().unwrap();
                                    match detail {
                                        ChangeDetails::Diff(delta) => {
                                            let block = [0; WINDOW];
                                            let mut updated = Vec::new();
                                            restore_seek(&mut updated, fs::File::open(&filename)?, block, &delta)?;
                                            create_file_with_contents(&filename, &updated)?;
                                        },
                                        _ => { return Err(eyre!("mismatch when adding {}, expected Diff, but not found", e1.path().display())) }
                                    }
                                }
                                new_entries.push(update_meta(&filename, e2)?);
                            } else {    // e2 not a file
                                // remove the file
                                fs::remove_file(&filename).expect(format!("failed to remove file {:?}", filename).as_str());
                                if let Some(p) = e2.target() {
                                    std::os::unix::fs::symlink(p, &filename).expect(format!("failed to create symlink {:?} {:?}", p, filename).as_str());
                                    new_entries.push(update_meta(&filename, e2)?);
                                } else if e2.is_dir() {
                                    fs::create_dir(&filename).expect(format!("failed to create directory {:?}", filename).as_str());
                                } else {
                                    panic!("Exhausted possibilities for the new entry");
                                }
                            }
                        } else if e1.is_symlink() {
                            // remove the symlink
                            fs::remove_file(&filename).expect(format!("failed to remove file {:?}", filename).as_str());
                            if e2.is_file() {
                                let detail = &details_iter.next().unwrap();
                                create_file(&filename, &detail).expect(format!("failed to create file {:?}", filename).as_str());
                                new_entries.push(update_meta(&filename, e2)?);
                            } else if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &filename).expect(format!("failed to create symlink {:?} {:?}", p, filename).as_str());
                                new_entries.push(update_meta(&filename, e2)?);
                            } else if e2.is_dir() {
                                fs::create_dir(&filename).expect(format!("failed to create directory {:?}", filename).as_str());
                                // new entry gets updated in the second pass, after all the updates in
                                // the directory are finished
                            }
                        } else if e1.is_dir() {
                            if e2.is_file() {
                                // need to save the file contents for after we remove the directory
                                let detail = &details_iter.next().unwrap();
                                leftover_details.push(detail);
                            }
                        } else {
                            panic!("Exhausted possibilities for the old entry");
                        }
                    }
                }
            },
            Action::Remote(change) | Action::ResolvedRemote((_,_),change) => {
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
    let mut details_iter = leftover_details.iter().rev();
    for action in actions.iter().rev() {
        match action {
            Action::Local(change) | Action::ResolvedLocal((_,_),change) => {
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
                        new_entries.push(update_meta(&dirname, e)?);
                    },
                    Change::Modified(e1,e2) => {
                        let dirname = base_path.join(e2.path());
                        if e1.is_dir() && !e2.is_dir() {
                            fs::remove_dir(&dirname).expect(format!("failed to remove directory {:?}", dirname).as_str());
                            if let Some(p) = e2.target() {
                                std::os::unix::fs::symlink(p, &dirname).expect(format!("failed to create symlink {:?} {:?}", p, dirname).as_str());
                            } else if e2.is_file() {
                                let detail = details_iter.next().unwrap();
                                create_file(&dirname, &detail).expect(format!("failed to create file {:?}", dirname).as_str());
                            }
                        }
                        new_entries.push(update_meta(&dirname, e2)?);
                    },
                }
            },
            _ => {}
        }
    }

    // copy remaining entries from all_old
    for e in old_iter {
        new_entries.push(e.clone());
    }
    new_entries.sort();     // directory -> file or symlink will be out of order, so need to sort them

    std::mem::swap(all_old, &mut new_entries);

    Ok(())
}

fn create_file(filename: &Path, detail: &ChangeDetails) -> Result<()> {
    match detail {
        ChangeDetails::Contents(v) => {
            create_file_with_contents(filename, v)
        },
        _ => { Err(eyre!("mismatch when adding {}, expected Contents, but not found", filename.display())) }
    }
}

fn create_file_with_contents(filename: &Path, data: &Vec<u8>) -> Result<()> {
    use atomicwrites::{AtomicFile,AllowOverwrite};
    let af = AtomicFile::new(filename, AllowOverwrite);
    let result = af.write(|f| {
        f.write_all(data)
    });
    match result {
        Ok(()) => Ok (()),
        Err(e) => Err(eyre!("unable to save {}: {}", filename.display(), e)),
    }
}

fn update_meta(path: &PathBuf, e: &Entry) -> Result<Entry> {
    let meta = fs::symlink_metadata(path).expect(format!("failed to acquire metadata for {:?}", path).as_str());
    if !e.is_symlink() {
        let mut perms = meta.permissions();
        perms.set_mode(e.mode());
        fs::set_permissions(path, perms).expect(format!("failed to set permissions for {:?}", path).as_str());
    }
    filetime::set_symlink_file_times(path, filetime::FileTime::from_unix_time(meta.atime(),0), filetime::FileTime::from_unix_time(e.mtime(),0))
        .expect(format!("failed to set time for {:?}", path).as_str());
    let mut new_entry = e.clone();
    new_entry.set_ino(meta.ino());
    Ok(new_entry)
}
