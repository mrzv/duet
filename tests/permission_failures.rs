#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct SyncCase {
    _temp: TempDir,
    local: PathBuf,
    remote: PathBuf,
    profile: PathBuf,
}

impl SyncCase {
    fn new(locations: &[&str]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local");
        let remote = temp.path().join("remote");
        let profile = temp.path().join("profile.prf");

        fs::create_dir(&local).unwrap();
        fs::create_dir(&remote).unwrap();

        let mut profile_contents = format!(
            "{}\n{} {}\n",
            local.display(),
            duet_bin().display(),
            remote.display()
        );
        for location in locations {
            profile_contents.push_str(location);
            profile_contents.push('\n');
        }
        fs::write(&profile, profile_contents).unwrap();

        Self {
            _temp: temp,
            local,
            remote,
            profile,
        }
    }

    fn sync(&self) -> Output {
        Command::new(duet_bin())
            .arg("--profile-file")
            .arg(&self.profile)
            .arg("-b")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn local_state(&self) -> PathBuf {
        self.profile.with_extension("snp")
    }

    fn remote_state_dir(&self) -> PathBuf {
        self.profile.with_extension("remotes")
    }
}

fn duet_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_duet"))
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

struct PermissionGuard {
    path: PathBuf,
    mode: u32,
}

impl PermissionGuard {
    fn set(path: &Path, mode: u32) -> Self {
        let original = fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;
        chmod(path, mode);
        Self {
            path: path.to_path_buf(),
            mode: original,
        }
    }
}

impl Drop for PermissionGuard {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
    }
}

fn deny_read_dir(path: &Path) -> PermissionGuard {
    let guard = PermissionGuard::set(path, 0o000);
    assert_permission_denied(fs::read_dir(path).map(|_| ()), path, "read directory");
    guard
}

fn deny_read_file(path: &Path) -> PermissionGuard {
    let guard = PermissionGuard::set(path, 0o000);
    assert_permission_denied(fs::read(path).map(|_| ()), path, "read file");
    guard
}

fn deny_write_dir(path: &Path) -> PermissionGuard {
    let guard = PermissionGuard::set(path, 0o555);
    let probe = path.join(".duet-permission-probe");
    assert_permission_denied(fs::write(&probe, b"probe"), path, "write directory");
    let _ = fs::remove_file(probe);
    guard
}

fn assert_permission_denied(result: io::Result<()>, path: &Path, operation: &str) {
    match result {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {}
        Err(error) => panic!(
            "permission safety check for {} on {} returned {:?}, not PermissionDenied",
            operation,
            path.display(),
            error.kind()
        ),
        Ok(()) => panic!(
            "permission safety check for {} on {} unexpectedly succeeded",
            operation,
            path.display()
        ),
    }
}

#[test]
fn local_added_file_mode_propagates_to_remote() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "from local");
    chmod(&local_file, 0o600);

    assert_success(case.sync());

    assert_eq!(read(&remote_file), "from local");
    assert_eq!(mode(&remote_file), 0o600);
}

#[test]
fn metadata_only_chmod_propagates_to_remote() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "same contents");
    chmod(&local_file, 0o644);
    assert_success(case.sync());

    chmod(&local_file, 0o600);
    assert_success(case.sync());

    assert_eq!(read(&remote_file), "same contents");
    assert_eq!(mode(&remote_file), 0o600);
}

#[test]
fn local_added_directory_mode_propagates_to_remote() {
    let case = SyncCase::new(&["+dir", "+dir/a.txt"]);
    let local_dir = case.local.join("dir");
    let remote_dir = case.remote.join("dir");
    fs::create_dir(&local_dir).unwrap();
    write(&local_dir.join("a.txt"), "nested");
    chmod(&local_dir, 0o750);

    assert_success(case.sync());

    assert_eq!(read(&remote_dir.join("a.txt")), "nested");
    assert_eq!(mode(&remote_dir), 0o750);
}

#[test]
fn unreadable_local_subdir_does_not_look_like_deletion() {
    let case = SyncCase::new(&["+dir", "+dir/a.txt"]);
    let local_dir = case.local.join("dir");
    let remote_file = case.remote.join("dir/a.txt");
    fs::create_dir(&local_dir).unwrap();
    write(&local_dir.join("a.txt"), "tracked");
    assert_success(case.sync());

    let _guard = deny_read_dir(&local_dir);
    let output = case.sync();

    assert_failure(&output);
    assert_eq!(read(&remote_file), "tracked");
}

#[test]
fn unreadable_remote_subdir_does_not_look_like_deletion() {
    let case = SyncCase::new(&["+dir", "+dir/a.txt"]);
    let local_file = case.local.join("dir/a.txt");
    let remote_dir = case.remote.join("dir");
    fs::create_dir(case.local.join("dir")).unwrap();
    write(&local_file, "tracked");
    assert_success(case.sync());

    let _guard = deny_read_dir(&remote_dir);
    let output = case.sync();

    assert_failure(&output);
    assert_eq!(read(&local_file), "tracked");
}

#[test]
fn unreadable_changed_local_file_fails_before_remote_apply() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "secret");

    let _guard = deny_read_file(&local_file);
    let output = case.sync();

    assert_failure(&output);
    assert!(!remote_file.exists());
}

#[test]
fn unreadable_changed_remote_file_reports_remote_permission_error() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&remote_file, "secret");

    let _guard = deny_read_file(&remote_file);
    let output = case.sync();

    assert_failure(&output);
    assert!(!local_file.exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a.txt") && stderr.to_lowercase().contains("permission"),
        "expected path-aware permission error\nstderr:\n{}",
        stderr
    );
}

#[test]
fn unwritable_destination_parent_does_not_partially_apply() {
    let case = SyncCase::new(&["+a.txt", "+blocked", "+blocked/b.txt"]);
    let local_blocked = case.local.join("blocked");
    fs::create_dir(&local_blocked).unwrap();
    assert_success(case.sync());

    write(&case.remote.join("a.txt"), "should not be copied");
    write(&case.remote.join("blocked/b.txt"), "blocked");

    let _guard = deny_write_dir(&local_blocked);
    let output = case.sync();

    assert_failure(&output);
    assert!(!case.local.join("a.txt").exists());
    assert!(!case.local.join("blocked/b.txt").exists());
}

#[test]
fn concurrent_apply_does_not_mutate_remote_when_local_apply_fails() {
    let case = SyncCase::new(&["+upload.txt", "+blocked", "+blocked/download.txt"]);
    let local_blocked = case.local.join("blocked");
    fs::create_dir(&local_blocked).unwrap();
    assert_success(case.sync());

    write(&case.local.join("upload.txt"), "upload");
    write(&case.remote.join("blocked/download.txt"), "download");

    let _guard = deny_write_dir(&local_blocked);
    let output = case.sync();

    assert_failure(&output);
    assert!(!case.remote.join("upload.txt").exists());
    assert!(!case.local.join("blocked/download.txt").exists());
}

#[test]
fn readonly_synced_directory_does_not_block_future_child_sync() {
    let case = SyncCase::new(&["+dir", "+dir/a.txt"]);
    let local_dir = case.local.join("dir");
    let remote_dir = case.remote.join("dir");
    fs::create_dir(&local_dir).unwrap();
    write(&local_dir.join("a.txt"), "initial");
    chmod(&local_dir, 0o555);
    assert_success(case.sync());

    chmod(&local_dir, 0o755);
    write(&local_dir.join("a.txt"), "updated contents");
    chmod(&local_dir, 0o555);
    assert_eq!(mode(&remote_dir), 0o555);
    let output = case.sync();

    assert_success(output);
    assert_eq!(read(&remote_dir.join("a.txt")), "updated contents");
    assert_eq!(mode(&remote_dir), 0o555);
}

#[test]
fn unreadable_local_state_file_fails_without_remote_mutation() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    let _guard = deny_read_file(&case.local_state());
    write(&local_file, "updated contents");
    let output = case.sync();

    assert_failure(&output);
    assert_eq!(read(&remote_file), "initial");
}

#[test]
fn unreadable_remote_state_file_reports_path_aware_error() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());
    let remote_state_file = fs::read_dir(case.remote_state_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();

    let _guard = deny_read_file(&remote_state_file);
    write(&remote_file, "updated remotely");
    let output = case.sync();

    assert_failure(&output);
    assert_eq!(read(&local_file), "initial");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(remote_state_file.file_name().unwrap().to_str().unwrap())
            && stderr.to_lowercase().contains("permission"),
        "expected remote state permission error\nstderr:\n{}",
        stderr
    );
}

#[test]
fn unwritable_profile_directory_save_failure_is_reported_after_mutation() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    write(&local_file, "updated contents");
    let profile_dir = case.profile.parent().unwrap().to_path_buf();
    let _guard = deny_write_dir(&profile_dir);
    let output = case.sync();

    assert_failure(&output);
    assert_eq!(read(&remote_file), "updated contents");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("state") || stderr.to_lowercase().contains("save"),
        "expected state-save failure context\nstderr:\n{}",
        stderr
    );
}
