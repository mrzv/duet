use std::{env, fs, path::Path, process::Command};

fn main() {
    // Dirty-version detection depends on source contents, not only VCS metadata.
    println!("cargo:rerun-if-changed=.");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    println!("cargo:rerun-if-changed=.jj/repo");
    println!("cargo:rerun-if-changed=.jj/working_copy/checkout");
    println!("cargo:rerun-if-changed=.jj/working_copy/tree_state");
    emit_vcs_rerun_paths();
    println!(
        "cargo:rustc-env=DUET_VERSION_SUFFIX={}",
        version_suffix().unwrap_or_default()
    );

    built::write_built_file().expect("Failed to acquire build-time information");
}

fn version_suffix() -> Option<String> {
    let version = env::var("CARGO_PKG_VERSION").ok()?;
    // Prefer plain Git checkouts; fall back to jj checkouts when Git metadata is
    // not directly available.
    match git_version_suffix(&version) {
        Ok(suffix) => suffix,
        Err(()) => jj_version_suffix(&version),
    }
}

fn git_version_suffix(version: &str) -> Result<Option<String>, ()> {
    let tag = format!("v{}", version);
    let count = command_output("git", &["rev-list", "--count", &format!("{}..HEAD", tag)])?
        .parse::<u32>()
        .map_err(|_| ())?;
    let dirty =
        !command_output("git", &["status", "--porcelain", "--untracked-files=all"])?.is_empty();

    if count == 0 && !dirty {
        return Ok(None);
    }

    let commit = command_output("git", &["rev-parse", "--short=12", "HEAD"])?;
    let suffix = if dirty {
        format!("+{}.{}.dirty", count, commit)
    } else {
        format!("+{}.{}", count, commit)
    };
    Ok(Some(suffix))
}

fn jj_version_suffix(version: &str) -> Option<String> {
    let tag = format!("v{}", version);
    let revset = format!("({}..@-) | (@ & ~empty())", tag);
    let commits = command_output(
        "jj",
        &[
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            "commit_id.short(12) ++ \"\\n\"",
        ],
    )
    .ok()?;
    let count = commits.lines().count();

    if count == 0 {
        return None;
    }

    let commit = commits.lines().next()?;
    Some(format!("+{}.{}", count, commit))
}

fn emit_vcs_rerun_paths() {
    if let Ok(git_dir) = command_output("git", &["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={}/HEAD", git_dir);
        println!("cargo:rerun-if-changed={}/refs/heads", git_dir);
        println!("cargo:rerun-if-changed={}/refs/tags", git_dir);
        println!("cargo:rerun-if-changed={}/packed-refs", git_dir);
    }

    if let Ok(repo) = fs::read_to_string(".jj/repo") {
        let path = Path::new(".jj").join(repo.trim());
        if let Ok(path) = fs::canonicalize(path) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn command_output(command: &str, args: &[&str]) -> Result<String, ()> {
    let output = Command::new(command).args(args).output().map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
