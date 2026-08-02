use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Serialize;
use tempfile::TempDir;

#[derive(Serialize)]
struct LegacyEntryFixture {
    path: PathBuf,
    size: u64,
    mtime: i64,
    ino: u64,
    mode: u32,
    target: Option<PathBuf>,
    is_dir: bool,
    checksum: u32,
}

fn write_legacy_file_state(state: &Path, root: &Path, relative: &str, checksum: u32) {
    let metadata = fs::symlink_metadata(root.join(relative)).unwrap();
    let entries = vec![LegacyEntryFixture {
        path: PathBuf::from(relative),
        size: metadata.size(),
        mtime: metadata.mtime(),
        ino: metadata.ino(),
        mode: metadata.mode(),
        target: None,
        is_dir: false,
        checksum,
    }];
    let mut file = fs::File::create(state).unwrap();
    bincode::serde::encode_into_std_write(&entries, &mut file, bincode::config::legacy()).unwrap();
}

struct SyncCase {
    _temp: TempDir,
    local: PathBuf,
    remote: PathBuf,
    profile: PathBuf,
}

impl SyncCase {
    fn new() -> Self {
        Self::new_with_rules("+a.txt\n")
    }

    fn new_with_rules(rules: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local");
        let remote = temp.path().join("remote");
        let profile = temp.path().join("profile.prf");

        fs::create_dir(&local).unwrap();
        fs::create_dir(&remote).unwrap();
        fs::write(
            &profile,
            format!(
                "{}\n{} {}\n{}",
                local.display(),
                duet_bin().display(),
                remote.display(),
                rules
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

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
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
fn excluded_change_is_discovered_when_rerun_without_exclusion() {
    let case = SyncCase::new_with_rules("+.\n");
    fs::create_dir(case.local.join("held")).unwrap();
    write(&case.local.join("held/file.txt"), "baseline");
    assert_success(case.sync());

    write(&case.local.join("held/file.txt"), "accumulated");
    assert_success(case.sync_with_args(&["--exclude", "held"]));
    assert_eq!(read(&case.remote.join("held/file.txt")), "baseline");

    assert_success(case.sync());
    assert_eq!(read(&case.remote.join("held/file.txt")), "accumulated");
}

#[test]
fn exclusion_composes_with_restricted_synchronization() {
    let case = SyncCase::new_with_rules("+.\n");
    fs::create_dir(case.local.join("scope")).unwrap();
    write(&case.local.join("scope/included.txt"), "included baseline");
    write(&case.local.join("scope/excluded.txt"), "excluded baseline");
    write(&case.local.join("outside.txt"), "outside baseline");
    assert_success(case.sync());

    write(&case.local.join("scope/included.txt"), "included updated");
    write(&case.local.join("scope/excluded.txt"), "excluded updated");
    write(&case.local.join("outside.txt"), "outside updated");

    assert_success(case.sync_with_args(&["--exclude", "scope/excluded.txt", "scope"]));

    assert_eq!(
        read(&case.remote.join("scope/included.txt")),
        "included updated"
    );
    assert_eq!(
        read(&case.remote.join("scope/excluded.txt")),
        "excluded baseline"
    );
    assert_eq!(read(&case.remote.join("outside.txt")), "outside baseline");

    assert_success(case.sync());
    assert_eq!(
        read(&case.remote.join("scope/excluded.txt")),
        "excluded updated"
    );
    assert_eq!(read(&case.remote.join("outside.txt")), "outside updated");
}

#[test]
fn checkpointed_staging_runs_multiple_bidirectional_waves() {
    let case = SyncCase::new_with_rules("+.\n");
    let local_a = vec![b'a'; 3 * 1024];
    let local_c = vec![b'c'; 3 * 1024];
    let remote_b = vec![b'b'; 3 * 1024];
    let remote_d = vec![b'd'; 3 * 1024];
    write_bytes(&case.local.join("a-local.bin"), &local_a);
    write_bytes(&case.local.join("c-local.bin"), &local_c);
    write_bytes(&case.remote.join("b-remote.bin"), &remote_b);
    write_bytes(&case.remote.join("d-remote.bin"), &remote_d);
    let profile_json = case.local.parent().unwrap().join("wave-performance.json");

    assert_success(case.sync_with_args(&[
        "--staging-limit",
        "4KiB",
        "--staging-reserve",
        "0%",
        "--profile-performance-json",
        profile_json.to_str().unwrap(),
    ]));

    let profile: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&profile_json).unwrap()).unwrap();
    let staging = &profile["counters"]["staging"];
    assert_eq!(staging["wave_count"], 2);
    assert_eq!(staging["local_reconstructed_bytes"], 6 * 1024);
    assert_eq!(staging["remote_reconstructed_bytes"], 6 * 1024);
    assert_eq!(staging["local_staged_regular_outputs"], 2);
    assert_eq!(staging["remote_staged_regular_outputs"], 2);
    assert_eq!(staging["local_budget_bytes"], 4 * 1024);
    assert_eq!(staging["remote_budget_bytes"], 4 * 1024);

    for (name, expected) in [
        ("a-local.bin", &local_a),
        ("b-remote.bin", &remote_b),
        ("c-local.bin", &local_c),
        ("d-remote.bin", &remote_d),
    ] {
        assert_eq!(fs::read(case.local.join(name)).unwrap(), *expected);
        assert_eq!(fs::read(case.remote.join(name)).unwrap(), *expected);
    }

    assert_success(case.sync_with_args(&["--staging-limit", "4KiB", "--staging-reserve", "0%"]));
}

#[test]
fn legacy_migration_reports_metadata_hidden_adler_collision() {
    let case = SyncCase::new();
    let original = [10, 10, 10, 10];
    let collision = [11, 9, 9, 11];
    let checksum = adler32::adler32(&original[..]).unwrap();
    assert_eq!(checksum, adler32::adler32(&collision[..]).unwrap());
    write_bytes(&case.local.join("a.txt"), &original);
    assert_success(case.sync());

    let local_state = case.profile.with_extension("snp");
    let remote_state_dir = case.profile.with_extension("remotes");
    let remote_state = fs::read_dir(&remote_state_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_file())
        .expect("remote state file");
    write_legacy_file_state(&local_state, &case.local, "a.txt", checksum);
    write_legacy_file_state(&remote_state, &case.remote, "a.txt", checksum);
    let local_state_before = fs::read(&local_state).unwrap();
    let remote_state_before = fs::read(&remote_state).unwrap();

    let metadata = fs::metadata(case.local.join("a.txt")).unwrap();
    write_bytes(&case.local.join("a.txt"), &collision);
    filetime::set_file_mtime(
        case.local.join("a.txt"),
        filetime::FileTime::from_unix_time(metadata.mtime(), 0),
    )
    .unwrap();

    let output = case.sync();
    let text = combined_output(&output);
    assert!(!output.status.success(), "{}", text);
    assert!(text.contains("conflict"), "{}", text);
    assert_eq!(fs::read(case.local.join("a.txt")).unwrap(), collision);
    assert_eq!(fs::read(case.remote.join("a.txt")).unwrap(), original);
    assert_eq!(fs::read(local_state).unwrap(), local_state_before);
    assert_eq!(fs::read(remote_state).unwrap(), remote_state_before);
}

#[test]
fn dot_root_include_synchronizes_the_whole_root() {
    let case = SyncCase::new_with_rules("+.\n");
    fs::create_dir(case.local.join("dir")).unwrap();
    write(&case.local.join("root.txt"), "root");
    write(&case.local.join("dir/nested.txt"), "nested");

    assert_success(case.sync());

    assert_eq!(read(&case.remote.join("root.txt")), "root");
    assert_eq!(read(&case.remote.join("dir/nested.txt")), "nested");
}

#[test]
fn bare_root_include_synchronizes_the_whole_root() {
    let case = SyncCase::new_with_rules("+\n");
    fs::create_dir(case.local.join("dir")).unwrap();
    write(&case.local.join("root.txt"), "root");
    write(&case.local.join("dir/nested.txt"), "nested");

    assert_success(case.sync());

    assert_eq!(read(&case.remote.join("root.txt")), "root");
    assert_eq!(read(&case.remote.join("dir/nested.txt")), "nested");
}

#[test]
fn later_equivalent_location_rule_wins() {
    let case = SyncCase::new_with_rules("+selected.txt\n-./selected.txt\n");
    write(&case.local.join("selected.txt"), "excluded");

    assert_success(case.sync());

    assert!(!case.remote.join("selected.txt").exists());
}

#[test]
fn nested_specific_location_rule_wins() {
    let case = SyncCase::new_with_rules("+tree\n-tree/private\n+tree/private/keep\n");
    fs::create_dir_all(case.local.join("tree/private/keep")).unwrap();
    write(&case.local.join("tree/public.txt"), "public");
    write(&case.local.join("tree/private/hidden.txt"), "hidden");
    write(
        &case.local.join("tree/private/keep/included.txt"),
        "included",
    );

    assert_success(case.sync());

    assert_eq!(read(&case.remote.join("tree/public.txt")), "public");
    assert!(!case.remote.join("tree/private/hidden.txt").exists());
    assert_eq!(
        read(&case.remote.join("tree/private/keep/included.txt")),
        "included"
    );
}

#[test]
fn dry_run_does_not_apply_changes() {
    let case = SyncCase::new();
    write(&case.local.join("a.txt"), "from local");

    let output = case.sync_with_args(&["--dry-run"]);

    assert_success(output);
    assert!(!case.remote.join("a.txt").exists());
}

#[test]
fn dry_run_reports_ignored_directory_removal_blockers() {
    let case = SyncCase::new_with_rules("+dir\n\n[ignore]\n__pycache__\n");
    fs::create_dir_all(case.local.join("dir")).unwrap();
    write(&case.local.join("dir/tracked.txt"), "tracked");
    assert_success(case.sync());

    fs::remove_dir_all(case.local.join("dir")).unwrap();
    fs::create_dir_all(case.remote.join("dir/__pycache__")).unwrap();
    write(&case.remote.join("dir/__pycache__/cache.pyc"), "cache");

    let output = case.sync_with_args(&["--dry-run"]);
    let output_text = combined_output(&output);

    assert!(!output.status.success(), "{}", output_text);
    assert!(output_text.contains("ignored"), "{}", output_text);
    assert!(output_text.contains("__pycache__"), "{}", output_text);
    assert!(output_text.contains("--prune-ignored"), "{}", output_text);
    assert!(case.remote.join("dir/tracked.txt").exists());
    assert!(case.remote.join("dir/__pycache__/cache.pyc").exists());
}

#[test]
fn dry_run_validates_remote_apply_preflight() {
    let case = SyncCase::new();
    write(&case.local.join("a.txt"), "from local");
    assert_success(case.sync());

    fs::remove_file(case.local.join("a.txt")).unwrap();
    let mut permissions = fs::metadata(&case.remote).unwrap().permissions();
    permissions.set_mode(0o555);
    fs::set_permissions(&case.remote, permissions).unwrap();

    let output = case.sync_with_args(&["--dry-run"]);

    let mut permissions = fs::metadata(&case.remote).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&case.remote, permissions).unwrap();

    let output_text = combined_output(&output);
    assert!(!output.status.success(), "{}", output_text);
    assert!(output_text.contains("preflight apply"), "{}", output_text);
    assert!(output_text.contains("not writable"), "{}", output_text);
    assert!(case.remote.join("a.txt").exists());
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
        stdout.contains(
            "agreed capabilities: profile-file-state-dir, streamed-details-v1, streamed-detail-batches-v1"
        ),
        "{}",
        stdout
    );
    assert!(stdout.contains("sync-tuning-v1"), "{}", stdout);
    assert!(stdout.contains("stream-performance-v1"), "{}", stdout);
    assert!(stdout.contains("file-byte-chunks-v1"), "{}", stdout);
    assert!(stdout.contains("sync tuning:"), "{}", stdout);
    assert!(stdout.contains("detail-batch-frames=256"), "{}", stdout);
}

#[test]
fn named_profile_debug_info_reports_negotiated_capabilities() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let config = home.join(".config").join("duet");
    let local = temp.path().join("local");
    let remote = temp.path().join("remote");

    fs::create_dir_all(&config).unwrap();
    fs::create_dir(&local).unwrap();
    fs::create_dir(&remote).unwrap();
    fs::write(
        config.join("work.prf"),
        format!(
            "{}\n{} {}\n+a.txt\n",
            local.display(),
            duet_bin().display(),
            remote.display()
        ),
    )
    .unwrap();
    write(&local.join("a.txt"), "from local");

    let output = Command::new(duet_bin())
        .arg("--debug-info")
        .arg("work")
        .arg("-b")
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    assert_success(output);
    assert!(stdout.contains("Debug information:"), "{}", stdout);
    assert!(stdout.contains("server protocol:"), "{}", stdout);
    assert!(
        stdout.contains(
            "agreed capabilities: profile-file-state-dir, streamed-details-v1, streamed-detail-batches-v1"
        ),
        "{}",
        stdout
    );
    assert!(stdout.contains("sync-tuning-v1"), "{}", stdout);
    assert!(stdout.contains("file-byte-chunks-v1"), "{}", stdout);
    assert!(stdout.contains("sync tuning:"), "{}", stdout);
    assert!(stdout.contains("detail-batch-frames=256"), "{}", stdout);
    assert_eq!(read(&remote.join("a.txt")), "from local");
}

#[test]
fn performance_profile_reports_human_and_json_output() {
    let case = SyncCase::new();
    write(&case.local.join("a.txt"), "from local");
    let profile_json = case.local.parent().unwrap().join("performance.json");
    let profile_json_arg = profile_json.to_str().unwrap();

    let output = case.sync_with_args(&[
        "--profile-performance",
        "--profile-performance-json",
        profile_json_arg,
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    assert_success(output);
    assert!(stdout.contains("Performance profile:"), "{}", stdout);
    assert!(stdout.contains("phases:"), "{}", stdout);
    assert!(stdout.contains("local_scan"), "{}", stdout);
    assert!(stdout.contains("remote_scan_rpc"), "{}", stdout);
    assert!(stdout.contains("staged_validation"), "{}", stdout);
    assert!(stdout.contains("signatures:"), "{}", stdout);
    assert!(stdout.contains("staging: waves="), "{}", stdout);
    assert!(stdout.contains("stream remote->local"), "{}", stdout);
    assert!(stdout.contains("stream local->remote"), "{}", stdout);
    assert!(stdout.contains("remote server stream:"), "{}", stdout);

    let json = fs::read_to_string(profile_json).unwrap();
    let profile: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(profile["total_ms"].is_u64());
    assert!(profile["phases"].is_array());
    assert!(profile["phases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|phase| phase["name"] == "staged_validation"));
    assert!(profile["sync_tuning"].is_object());
    let counters = &profile["counters"];
    assert_eq!(counters["streamed_details"], true);
    assert_eq!(counters["staging"]["wave_count"], 1);
    assert_eq!(counters["staging"]["local_staged_regular_outputs"], 0);
    assert_eq!(counters["staging"]["remote_staged_regular_outputs"], 1);
    assert!(counters["staging"]["remote_budget_bytes"].as_u64().unwrap() > 0);
    assert!(counters["streaming"]["local_to_remote"].is_object());
    assert!(counters["streaming"]["remote_server"].is_object());
    assert!(counters["streaming"]["remote_server"]["apply_frames_ms"].is_u64());
    assert_eq!(read(&case.remote.join("a.txt")), "from local");
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
