#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
        self.sync_with_args(&[])
    }

    fn sync_with_args(&self, args: &[&str]) -> Output {
        Command::new(duet_bin())
            .arg("--profile-file")
            .arg(&self.profile)
            .args(args)
            .arg("-b")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn sync_child(&self) -> Child {
        self.sync_child_with_pause("DUET_TEST_PAUSE_AFTER_REMOTE_APPLY_PREPARE_MS")
    }

    fn sync_child_with_pause(&self, variable: &str) -> Child {
        self.sync_child_with_pause_and_args(variable, &[])
    }

    fn sync_child_with_pause_and_args(&self, variable: &str, args: &[&str]) -> Child {
        Command::new(duet_bin())
            .arg("--profile-file")
            .arg(&self.profile)
            .args(args)
            .arg("-b")
            .env("NO_COLOR", "1")
            .env(variable, "30000")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
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

fn apply_marker_for(state_path: &Path) -> PathBuf {
    let file_name = state_path.file_name().unwrap().to_string_lossy();
    state_path.with_file_name(format!(".{}.duet-apply", file_name))
}

fn remote_state_file(case: &SyncCase) -> PathBuf {
    fs::read_dir(case.remote_state_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

fn wait_for_path_while_child_runs(path: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "child exited with {} before {} appeared",
                status,
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}

fn wait_for_marker_phase_while_child_runs(path: &Path, phase: &str, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if fs::read_to_string(path)
            .map(|contents| {
                contents.lines().any(|line| {
                    line == format!("phase: {phase}")
                        || line
                            .strip_prefix("phase-slot-v3: ")
                            .map(|record| {
                                let mut fields = record.split(' ');
                                fields.next().is_some()
                                    && fields.next() == Some("applied")
                                    && fields.next().is_some()
                                    && fields.next() == Some(phase)
                                    && fields.next().is_none()
                            })
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
        {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "child exited with {} before {} reached phase {}",
                status,
                path.display(),
                phase
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {} to reach phase {}",
        path.display(),
        phase
    );
}

fn send_sigint(child: &Child) {
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(
        result,
        0,
        "failed to send SIGINT: {}",
        io::Error::last_os_error()
    );
}

fn assert_no_staging_directory(path: &Path) {
    let staged = fs::read_dir(path)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".duet-stage-")
        });
    assert!(
        !staged,
        "unexpected staging directory under {}",
        path.display()
    );
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
fn local_added_file_modes_propagate_to_remote() {
    let files = [
        ("private.txt", 0o600),
        ("readonly.txt", 0o400),
        ("normal.txt", 0o644),
    ];
    let case = SyncCase::new(&["+private.txt", "+readonly.txt", "+normal.txt"]);
    for (name, requested_mode) in files {
        let local_file = case.local.join(name);
        write(&local_file, name);
        chmod(&local_file, requested_mode);
    }

    assert_success(case.sync());

    for (name, requested_mode) in files {
        let remote_file = case.remote.join(name);
        assert_eq!(read(&remote_file), name);
        assert_eq!(mode(&remote_file), requested_mode);
    }
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
fn unfinished_local_apply_marker_blocks_next_sync() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    let marker = apply_marker_for(&case.local_state());
    write(
        &marker,
        "duet-apply-attempt-v1\nside: local\nphase: apply\npath-count: 1\npath: a.txt\n",
    );
    write(&local_file, "updated");

    let output = case.sync();

    assert_failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("previous Duet apply attempt did not finish"));
    assert_eq!(read(&remote_file), "initial");
}

#[test]
fn unfinished_remote_apply_marker_blocks_next_sync() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    let marker = apply_marker_for(&remote_state_file(&case));
    write(
        &marker,
        "duet-apply-attempt-v1\nside: remote\nphase: state-save\npath-count: 1\npath: a.txt\n",
    );
    write(&local_file, "updated");

    let output = case.sync();

    assert_failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("previous Duet apply attempt did not finish"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("state may not have been saved"));
    assert_eq!(read(&remote_file), "initial");
}

#[test]
fn staged_validation_catches_remote_permission_race_before_commit() {
    let case = SyncCase::new(&["+link"]);
    let local_link = case.local.join("link");
    let remote_link = case.remote.join("link");
    write(&local_link, "initial");
    assert_success(case.sync());

    fs::remove_file(&local_link).unwrap();
    std::os::unix::fs::symlink("target.txt", &local_link).unwrap();
    let remote_state = remote_state_file(&case);
    let marker = apply_marker_for(&remote_state);
    let mut child = case.sync_child();
    wait_for_path_while_child_runs(&marker, &mut child);
    let guard = PermissionGuard::set(&case.remote, 0o555);

    let output = child.wait_with_output().unwrap();
    drop(guard);

    assert_failure(&output);
    assert!(
        !marker.exists(),
        "unexpected recovery marker at {}",
        marker.display()
    );
    assert_eq!(read(&remote_link), "initial");
    assert_success(case.sync());
    assert_eq!(
        fs::read_link(&remote_link).unwrap(),
        PathBuf::from("target.txt")
    );
}

#[test]
fn sigint_after_staged_prepare_aborts_without_mutating_targets() {
    let case = SyncCase::new(&["+file.txt"]);
    let local_file = case.local.join("file.txt");
    let remote_file = case.remote.join("file.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    let local_state = case.local_state();
    let remote_state = remote_state_file(&case);
    let local_marker = apply_marker_for(&local_state);
    let remote_marker = apply_marker_for(&remote_state);
    let local_snapshot = fs::read(&local_state).unwrap();
    let remote_snapshot = fs::read(&remote_state).unwrap();
    write(&local_file, "updated");

    let mut child = case.sync_child_with_pause("DUET_TEST_PAUSE_AFTER_STAGED_PREPARE_MS");
    wait_for_marker_phase_while_child_runs(&remote_marker, "prepared", &mut child);
    send_sigint(&child);
    let output = child.wait_with_output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(6),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(read(&remote_file), "initial");
    assert_eq!(fs::read(&local_state).unwrap(), local_snapshot);
    assert_eq!(fs::read(&remote_state).unwrap(), remote_snapshot);
    assert!(!local_marker.exists());
    assert!(!remote_marker.exists());
    assert_no_staging_directory(&case.local);
    assert_no_staging_directory(&case.remote);
}

#[test]
fn sigint_after_staged_commit_finishes_state_and_exits_successfully() {
    let case = SyncCase::new(&["+file.txt"]);
    let local_file = case.local.join("file.txt");
    let remote_file = case.remote.join("file.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    let local_marker = apply_marker_for(&case.local_state());
    let remote_marker = apply_marker_for(&remote_state_file(&case));
    write(&local_file, "updated");

    let mut child = case.sync_child_with_pause("DUET_TEST_PAUSE_AFTER_STAGED_COMMIT_MS");
    wait_for_marker_phase_while_child_runs(&remote_marker, "committed", &mut child);
    send_sigint(&child);
    let output = child.wait_with_output().unwrap();

    assert_success(output);
    assert_eq!(read(&remote_file), "updated");
    assert!(!local_marker.exists());
    assert!(!remote_marker.exists());
    assert_no_staging_directory(&case.local);
    assert_no_staging_directory(&case.remote);
}

#[test]
fn sigint_after_intermediate_wave_checkpoint_stops_before_next_wave() {
    let case = SyncCase::new(&["+."]);
    write(&case.local.join("seed.txt"), "baseline");
    assert_success(case.sync());

    let local_a = "a".repeat(3 * 1024);
    let local_c = "c".repeat(3 * 1024);
    let remote_b = "b".repeat(3 * 1024);
    let remote_d = "d".repeat(3 * 1024);
    write(&case.local.join("a-local.bin"), &local_a);
    write(&case.local.join("c-local.bin"), &local_c);
    write(&case.remote.join("b-remote.bin"), &remote_b);
    write(&case.remote.join("d-remote.bin"), &remote_d);

    let args = ["--staging-limit", "4KiB", "--staging-reserve", "0%"];
    let remote_marker = apply_marker_for(&remote_state_file(&case));
    let mut child =
        case.sync_child_with_pause_and_args("DUET_TEST_PAUSE_AFTER_STAGED_COMMIT_MS", &args);
    wait_for_marker_phase_while_child_runs(&remote_marker, "committed", &mut child);
    send_sigint(&child);
    let output = child.wait_with_output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(6),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(read(&case.local.join("a-local.bin")), local_a);
    assert_eq!(read(&case.remote.join("a-local.bin")), local_a);
    assert_eq!(read(&case.local.join("b-remote.bin")), remote_b);
    assert_eq!(read(&case.remote.join("b-remote.bin")), remote_b);
    assert!(!case.remote.join("c-local.bin").exists());
    assert!(!case.local.join("d-remote.bin").exists());
    assert!(!apply_marker_for(&case.local_state()).exists());
    assert!(!remote_marker.exists());
    assert_no_staging_directory(&case.local);
    assert_no_staging_directory(&case.remote);

    assert_success(case.sync_with_args(&args));
    assert_eq!(read(&case.remote.join("c-local.bin")), local_c);
    assert_eq!(read(&case.local.join("d-remote.bin")), remote_d);
}

#[test]
fn unreadable_remote_state_file_reports_path_aware_error() {
    let case = SyncCase::new(&["+a.txt"]);
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());
    let remote_state_file = remote_state_file(&case);

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
fn unwritable_profile_directory_fails_before_remote_mutation() {
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
    assert_eq!(read(&remote_file), "initial");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("state") || stderr.to_lowercase().contains("save"),
        "expected state-save failure context\nstderr:\n{}",
        stderr
    );
}
