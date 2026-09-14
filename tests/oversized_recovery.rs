use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use blake2_rfc::blake2b::Blake2b;
use tempfile::TempDir;

const COMMITTED_OPERATIONS: usize = 8_300;
const COMMITTED_STEPS: usize = 3;
const OUTPUT_LIMIT: usize = 128 * 1024;

struct RecoveryCase {
    temp: TempDir,
    home: PathBuf,
    local: PathBuf,
    remote: PathBuf,
    state: PathBuf,
    remote_state: PathBuf,
}

impl RecoveryCase {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = home.join(".config/duet");
        let local = temp.path().join("local");
        let remote = temp.path().join("remote");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir(&local).unwrap();
        fs::create_dir(&remote).unwrap();
        fs::write(local.join("kept.txt"), b"synchronized contents\n").unwrap();
        fs::write(
            config.join("oversized.prf"),
            format!(
                "{}\n{} {}\n+kept.txt\n[staging]\nreserve = 0%\n",
                local.display(),
                env!("CARGO_BIN_EXE_duet"),
                remote.display(),
            ),
        )
        .unwrap();
        let mut case = Self {
            temp,
            home,
            local,
            remote,
            state: config.join("oversized.snp"),
            remote_state: PathBuf::new(),
        };
        assert_success(&case.command(&["oversized", "-b"]));
        let remote_states: Vec<_> = fs::read_dir(config.join("remotes"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_file())
            .collect();
        assert_eq!(remote_states.len(), 1);
        case.remote_state = remote_states.into_iter().next().unwrap();
        case
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_duet"))
            .args(args)
            .current_dir(self.temp.path())
            .env("HOME", &self.home)
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn recover(&self, remote: bool, clear: bool) -> Output {
        let mut args = vec!["recover"];
        if remote {
            args.push("--remote");
        }
        if clear {
            args.extend(["--clear", "--yes"]);
        }
        args.push("oversized");
        self.command(&args)
    }
}

fn marker_path(state: &Path) -> PathBuf {
    state.with_file_name(format!(
        ".{}.duet-apply",
        state.file_name().unwrap().to_str().unwrap()
    ))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_summary(output: &Output, phase: &str) {
    assert_success(output);
    assert!(output.stdout.len() + output.stderr.len() < OUTPUT_LIMIT);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.lines()
            .any(|line| line.strip_prefix("Effective phase: ") == Some(phase)),
        "missing effective phase in recovery summary",
    );
    let counts = text
        .lines()
        .find_map(|line| line.strip_prefix("Journal records: "))
        .expect("missing journal record totals");
    let fields: Vec<_> = counts.split(", ").collect();
    assert!(fields.contains(&format!("committed-operation={COMMITTED_OPERATIONS}").as_str()));
    assert!(fields.contains(&format!("committed-step={COMMITTED_STEPS}").as_str()));
}

fn write_records(writer: &mut impl Write) {
    // Complete, individually bounded records; the final step records are beyond
    // both the preview budget and the old whole-marker 16 MiB ceiling.
    let parent = "segment/".repeat(256);
    for index in 0..COMMITTED_OPERATIONS {
        writeln!(
            writer,
            "committed-operation: add-file {parent}file-{index:05}"
        )
        .unwrap();
    }
    for index in 0..COMMITTED_STEPS {
        writeln!(writer, "committed-step: publish-file kept-{index}.txt").unwrap();
    }
}

fn write_marker(path: &Path, base: &Path, state: &Path, side: &str, v3: bool) {
    let mut writer = BufWriter::new(File::create(path).unwrap());
    writeln!(
        writer,
        "{}\nside: {side}\nbase: {}\nstate: {}\nattempt-id: oversized-fixture\nphase: {}\npath-count: {COMMITTED_OPERATIONS}\noperation-count: {COMMITTED_OPERATIONS}\nunstaged-operation-count: 0\npaths-truncated: true\noperations-truncated: true",
        if v3 {
            "duet-apply-attempt-v2-journal-v3"
        } else {
            "duet-apply-attempt-v1"
        },
        base.display(),
        state.display(),
        if v3 { "preparing" } else { "state-save" },
    )
    .unwrap();
    if v3 {
        let mut previous = [0; 32];
        for (sequence, phase) in [
            "prepared",
            "committing",
            "committed",
            "state-save",
            "finished",
        ]
        .iter()
        .copied()
        .enumerate()
        {
            let (status, digest) = if phase == "finished" {
                ("pending", "0".repeat(64))
            } else {
                let mut hash = Blake2b::new(32);
                hash.update(b"duet staged marker fixed phase slot v3\0");
                hash.update(&(sequence as u64).to_be_bytes());
                hash.update(&previous);
                hash.update(b"oversized-fixture");
                hash.update(phase.as_bytes());
                let digest = hash.finalize();
                previous.copy_from_slice(digest.as_bytes());
                (
                    "applied",
                    digest
                        .as_bytes()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>(),
                )
            };
            writeln!(
                writer,
                "phase-slot-v3: {sequence:016x} {status} {digest} {phase}"
            )
            .unwrap();
        }
    }
    write_records(&mut writer);
    writer.flush().unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(fs::metadata(path).unwrap().len() > 16 * 1024 * 1024);
}

fn marker_snapshot(path: &Path) -> (u64, u64, u32, u64, Vec<u8>) {
    let mut file = File::open(path).unwrap();
    let metadata = file.metadata().unwrap();
    let mut hash = Blake2b::new(32);
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = file.read(&mut buffer).unwrap();
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.len(),
        hash.finalize().as_bytes().to_vec(),
    )
}

fn inspect_and_clear_v1(remote: bool) {
    let case = RecoveryCase::new();
    let (state, base) = if remote {
        (&case.remote_state, &case.remote)
    } else {
        (&case.state, &case.local)
    };
    let marker = marker_path(state);
    write_marker(
        &marker,
        base,
        state,
        if remote { "remote" } else { "local" },
        false,
    );
    let original_marker = marker_snapshot(&marker);
    let local_state = fs::read(&case.state).unwrap();
    let remote_state = fs::read(&case.remote_state).unwrap();

    assert_summary(&case.recover(remote, false), "state-save");
    assert_eq!(marker_snapshot(&marker), original_marker);
    assert_summary(&case.recover(remote, true), "state-save");
    assert!(!marker.exists());
    assert_eq!(fs::read(&case.state).unwrap(), local_state);
    assert_eq!(fs::read(&case.remote_state).unwrap(), remote_state);
    for root in [&case.local, &case.remote] {
        assert_eq!(
            fs::read(root.join("kept.txt")).unwrap(),
            b"synchronized contents\n",
        );
    }
}

#[test]
fn oversized_v1_local_recovery_counts_all_records_and_clears_only_marker() {
    inspect_and_clear_v1(false);
}

#[test]
fn oversized_v1_remote_recovery_counts_all_records_and_clears_only_marker() {
    inspect_and_clear_v1(true);
}

#[test]
fn oversized_v3_uses_effective_phase_and_rejects_corruption_after_preview() {
    let case = RecoveryCase::new();
    let marker = marker_path(&case.state);
    write_marker(&marker, &case.local, &case.state, "local", true);
    assert_summary(&case.recover(false, false), "state-save");

    OpenOptions::new()
        .append(true)
        .open(&marker)
        .unwrap()
        .write_all(b"stage-entry: invalid-identity\n")
        .unwrap();
    let original_marker = marker_snapshot(&marker);
    for clear in [false, true] {
        let output = case.recover(false, clear);
        assert!(
            !output.status.success(),
            "late malformed identity must prevent inspect and clear",
        );
        assert!(output.stdout.len() + output.stderr.len() < OUTPUT_LIMIT);
        assert_eq!(marker_snapshot(&marker), original_marker);
    }
}
