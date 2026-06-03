use std::fs;
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
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local");
        let remote = temp.path().join("remote");
        let profile = temp.path().join("profile.prf");

        fs::create_dir(&local).unwrap();
        fs::create_dir(&remote).unwrap();
        fs::write(
            &profile,
            format!(
                "{}\n{} {}\n+a.txt\n",
                local.display(),
                duet_bin().display(),
                remote.display()
            ),
        )
        .unwrap();

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

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
}

fn write_bytes(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn patterned_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn local_added_file_copies_to_remote() {
    let case = SyncCase::new();
    write(&case.local.join("a.txt"), "from local");

    assert_success(case.sync());

    assert_eq!(read(&case.remote.join("a.txt")), "from local");
}

#[test]
fn remote_added_file_copies_to_local() {
    let case = SyncCase::new();
    write(&case.remote.join("a.txt"), "from remote");

    assert_success(case.sync());

    assert_eq!(read(&case.local.join("a.txt")), "from remote");
}

#[test]
fn local_modified_file_copies_to_remote() {
    let case = SyncCase::new();
    write(&case.local.join("a.txt"), "initial");
    assert_success(case.sync());

    write(&case.local.join("a.txt"), "updated from local");
    assert_success(case.sync());

    assert_eq!(read(&case.remote.join("a.txt")), "updated from local");
}

#[test]
fn remote_modified_file_copies_to_local() {
    let case = SyncCase::new();
    write(&case.local.join("a.txt"), "initial");
    assert_success(case.sync());

    write(&case.remote.join("a.txt"), "updated from remote");
    assert_success(case.sync());

    assert_eq!(read(&case.local.join("a.txt")), "updated from remote");
}

#[test]
fn local_removed_file_removes_remote() {
    let case = SyncCase::new();
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    fs::remove_file(local_file).unwrap();
    assert_success(case.sync());

    assert!(!remote_file.exists());
}

#[test]
fn remote_removed_file_removes_local() {
    let case = SyncCase::new();
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    fs::remove_file(remote_file).unwrap();
    assert_success(case.sync());

    assert!(!local_file.exists());
}

#[test]
fn batch_conflict_aborts_without_changing_files() {
    let case = SyncCase::new();
    let local_file = case.local.join("a.txt");
    let remote_file = case.remote.join("a.txt");
    write(&local_file, "initial");
    assert_success(case.sync());

    write(&local_file, "local changed");
    write(&remote_file, "remote changed");

    let output = case.sync();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(read(&local_file), "local changed");
    assert_eq!(read(&remote_file), "remote changed");
}

#[test]
fn debug_info_reports_negotiated_capabilities() {
    let case = SyncCase::new();
    write(&case.local.join("a.txt"), "from local");

    let output = case.sync_with_args(&["--debug-info"]);

    assert_success(output);

    let output = case.sync_with_args(&["--debug-info"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_success(output);
    assert!(stdout.contains("Debug information:"), "{}", stdout);
    assert!(stdout.contains("client protocol:"), "{}", stdout);
    assert!(stdout.contains("server protocol:"), "{}", stdout);
    assert!(
        stdout.contains("agreed capabilities: profile-file-state-dir, streamed-details-v1"),
        "{}",
        stdout
    );
}

#[test]
fn large_local_added_file_streams_to_remote() {
    let case = SyncCase::new();
    let contents = patterned_bytes(3 * 1024 * 1024 + 17);
    write_bytes(&case.local.join("a.txt"), &contents);

    assert_success(case.sync());

    assert_eq!(fs::read(case.remote.join("a.txt")).unwrap(), contents);
}

#[test]
fn large_remote_modified_file_streams_to_local() {
    let case = SyncCase::new();
    let initial = patterned_bytes(3 * 1024 * 1024 + 17);
    write_bytes(&case.local.join("a.txt"), &initial);
    assert_success(case.sync());

    let mut updated = initial;
    for byte in &mut updated[1024 * 1024..1024 * 1024 + 64 * 1024] {
        *byte = byte.wrapping_add(17);
    }
    write_bytes(&case.remote.join("a.txt"), &updated);

    assert_success(case.sync());

    assert_eq!(fs::read(case.local.join("a.txt")).unwrap(), updated);
}

#[test]
fn large_local_modified_file_streams_to_remote() {
    let case = SyncCase::new();
    let initial = patterned_bytes(3 * 1024 * 1024 + 17);
    write_bytes(&case.local.join("a.txt"), &initial);
    assert_success(case.sync());

    let mut updated = initial;
    for byte in &mut updated[1024 * 1024..1024 * 1024 + 64 * 1024] {
        *byte = byte.wrapping_add(17);
    }
    write_bytes(&case.local.join("a.txt"), &updated);

    assert_success(case.sync());

    assert_eq!(fs::read(case.remote.join("a.txt")).unwrap(), updated);
}
