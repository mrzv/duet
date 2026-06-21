use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr};
use colored::*;
use tokio::sync::mpsc;

use crate::{full, profile, scan, state, sync};

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

pub(crate) fn show_help() {
    print!(
        "\
duet {}
bi-directional synchronization

USAGE:
    duet [FLAGS] <profile> [path]
    duet [FLAGS] --profile-file <file> [path]
    duet [FLAGS] preflight <profile> [path]
    duet [FLAGS] --profile-file <file> preflight [path]
    duet recover [--clear] [--yes] [--remote] <profile-or-statefile>

FLAGS:
    -i, --interactive   interactive conflict resolution
    -y, --yes           assume yes (i.e., synchronize, if there are no conflicts)
    -b, --batch         run as a batch (abort on conflict)
    -f, --force         in batch mode, apply what's possible, even if there are conflicts
    -v, --verbose       verbose output
    -n, --dry-run       don't apply changes
        --debug-info    print protocol and capability negotiation details
        --prune-ignored delete ignored files/directories that block removing a synced parent
        --profile-performance
                         print sync phase timings and transfer counters
        --profile-performance-json <file>
                         write sync phase timings and transfer counters as JSON

        --profile-file <file>
                         read profile from a local file and keep state next to it

        --version       prints version information
        --license       prints license information (including dependencies)
    -h, --help          prints help information

RECOVERY:
    recover <profile-or-statefile>
        inspect an unfinished local apply marker for a profile or state file
    recover --remote <profile>
        inspect an unfinished remote apply marker for a named profile
    recover --clear <profile-or-statefile>
        inspect and then interactively remove the marker after manual recovery
    recover --clear --yes <profile-or-statefile>
        remove the marker without prompting after manual recovery

    Local recovery accepts a profile name, such as `duet recover cole`, and falls
    back to treating the argument as an explicit state file path when no named
    profile exists. Remote recovery uses the profile's remote server and selected
    remote state id.

ARGS:
    <profile>    profile to synchronize
    <path>       path to synchronize

PREFLIGHT:
    preflight checks what a sync would do, reports directory removal blockers on
    both sides, and exits without applying changes or saving state.
",
        built_info::PKG_VERSION
    );
}

pub(crate) fn version(verbose: bool) {
    println!(
        "duet {}{}",
        built_info::PKG_VERSION,
        option_env!("DUET_VERSION_SUFFIX").unwrap_or("")
    );
    if verbose {
        for (name, version) in built_info::DEPENDENCIES {
            println!("  {} {}", name, version);
        }
    }
}

pub(crate) fn license() {
    println!("{}\n", include_str!("../LICENSE"));
    println!("{}", include_str!("../licenses/deps.txt"));
    println!("{}", include_str!("../licenses/included.txt"));
}

pub(crate) fn inspect(statefile: PathBuf) -> Result<()> {
    let entries = state::load_entries(&statefile)?;
    for e in entries {
        println!("{:?}", e);
    }
    Ok(())
}

pub(crate) async fn snapshot(name: String, statefile: Option<PathBuf>) -> Result<()> {
    let prf = profile::parse(&name)
        .wrap_err_with(|| format!("Failed to read profile {}", name.yellow()))?;
    println!("Using profile: {}", name.cyan());

    let local_base = full(&prf.local)?;
    let scan_ignore = prf.scan_ignore();

    let current_entries =
        state::scan_entries(&local_base, &PathBuf::from(""), &prf.locations, &scan_ignore).await?;

    let statefile = match statefile {
        Some(statefile) => statefile,
        None => profile::local_state(&name)?,
    };
    state::save_entries(&statefile, &current_entries)?;
    Ok(())
}

pub(crate) async fn changes(name: String, statefile: Option<PathBuf>) -> Result<()> {
    let prf = profile::parse(&name)
        .wrap_err_with(|| format!("Failed to read profile {}", name.yellow()))?;
    println!("Using profile: {}", name.cyan());

    let local_base = full(&prf.local)?;
    let scan_ignore = prf.scan_ignore();

    let statefile = match statefile {
        Some(statefile) => statefile,
        None => profile::local_state(&name)?,
    };

    let (_, changes) = state::old_and_changes(
        &local_base,
        &PathBuf::from(""),
        &prf.locations,
        &scan_ignore,
        Some(&statefile),
    )
    .await?;

    for c in changes {
        println!("{} {}", c, c.path().display());
    }

    Ok(())
}

pub(crate) fn info(name: String) -> Result<()> {
    println!(
        "Profile {} located at {}",
        name.cyan(),
        profile::location(&name)?.display().to_string().yellow()
    );
    Ok(())
}

pub(crate) async fn walk(path: PathBuf) -> Result<()> {
    let locations = vec![scan::location::Location::Include(PathBuf::from("."))];

    let (tx, mut rx) = mpsc::channel(1024);
    let handle = tokio::spawn(async move { scan::scan(path, "", &locations, &Vec::new(), tx).await });

    while let Some(e) = rx.recv().await {
        println!("{}", e.path().display());
    }
    handle.await.wrap_err("scanner task failed")??;
    Ok(())
}

pub(crate) fn recover(target: PathBuf, clear: bool, yes: bool) -> Result<()> {
    let statefile = recovery_statefile(&target)?;
    match sync::describe_apply_attempt(&statefile)? {
        Some(description) => {
            println!("{}", description);
            if clear && confirm_clear_recovery_marker(yes)? {
                sync::clear_apply_attempt(&statefile)?;
                println!("Removed recovery marker for {}", statefile.display());
            }
        }
        None => println!(
            "No unfinished Duet apply attempt for {}",
            statefile.display()
        ),
    }
    Ok(())
}

fn recovery_statefile(target: &PathBuf) -> Result<PathBuf> {
    if let Some(name) = profile_name_recovery_target(target) {
        if profile::location(name)?.try_exists()? {
            return Ok(profile::local_state(name)?);
        }
    }

    Ok(target.clone())
}

fn profile_name_recovery_target(target: &PathBuf) -> Option<&str> {
    if target.components().count() != 1 {
        return None;
    }
    target
        .to_str()
        .filter(|name| !name.is_empty() && *name != "." && *name != ".." && !name.contains('\\'))
}

pub(crate) fn confirm_clear_recovery_marker(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }

    Ok(dialoguer::Confirm::new()
        .with_prompt("Remove this recovery marker now? Only do this after inspecting both sides")
        .default(false)
        .interact()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn walk_reports_scan_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");

        assert!(walk(missing).await.is_err());
    }

    #[test]
    fn recover_clear_yes_removes_apply_marker() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let marker = dir.path().join(".profile.snp.duet-apply");
        std::fs::write(
            &marker,
            "duet-apply-attempt-v1\nside: local\nphase: apply\npath-count: 0\noperation-count: 0\nunstaged-operation-count: 0\n",
        )
        .unwrap();

        recover(state, true, true).unwrap();

        assert!(!marker.exists());
    }

    #[test]
    fn recover_treats_explicit_path_as_statefile() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let marker = dir.path().join(".profile.snp.duet-apply");
        std::fs::write(
            &marker,
            "duet-apply-attempt-v1\nside: local\nphase: apply\npath-count: 0\noperation-count: 0\nunstaged-operation-count: 0\n",
        )
        .unwrap();

        recover(state, true, true).unwrap();

        assert!(!marker.exists());
    }

    #[test]
    fn recovery_profile_targets_must_be_plain_names() {
        assert_eq!(
            profile_name_recovery_target(&PathBuf::from("cole")),
            Some("cole")
        );
        assert_eq!(profile_name_recovery_target(&PathBuf::from("./cole")), None);
        assert_eq!(
            profile_name_recovery_target(&PathBuf::from("/tmp/cole.snp")),
            None
        );
        assert_eq!(profile_name_recovery_target(&PathBuf::from("work\\old")), None);
    }

    #[test]
    fn recover_clear_yes_rejects_malformed_marker() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("profile.snp");
        let marker = dir.path().join(".profile.snp.duet-apply");
        std::fs::write(&marker, "not a duet marker\n").unwrap();

        let error = recover(state, true, true).unwrap_err().to_string();

        assert!(error.contains("refusing to remove malformed"), "{}", error);
        assert!(marker.exists());
    }
}
