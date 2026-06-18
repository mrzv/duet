use std::path::PathBuf;

use color_eyre::eyre::{eyre, Result};

use crate::profile::ProfileSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOptions {
    pub interactive: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub batch: bool,
    pub force: bool,
    pub verbose: bool,
    pub debug_info: bool,
    pub profile_performance: bool,
    pub profile_performance_json: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    License,
    Server,
    Snapshot {
        profile: String,
        statefile: Option<PathBuf>,
    },
    Inspect {
        statefile: PathBuf,
    },
    Changes {
        profile: String,
        statefile: Option<PathBuf>,
    },
    Info {
        profile: String,
    },
    Walk {
        path: PathBuf,
    },
    Recover {
        statefile: PathBuf,
    },
    Sync {
        profile: ProfileSource,
        path: Option<PathBuf>,
        options: SyncOptions,
    },
}

pub fn parse_from_env() -> Result<Command> {
    parse(pico_args::Arguments::from_env())
}

fn parse(mut pargs: pico_args::Arguments) -> Result<Command> {
    if pargs.contains(["-h", "--help"]) {
        ensure_no_args(pargs)?;
        return Ok(Command::Help);
    }

    if pargs.contains("--version") {
        ensure_no_args(pargs)?;
        return Ok(Command::Version);
    }

    if pargs.contains("--license") {
        ensure_no_args(pargs)?;
        return Ok(Command::License);
    }

    if pargs.contains("--server") {
        ensure_no_args(pargs)?;
        return Ok(Command::Server);
    }

    let profile_file = pargs.opt_value_from_os_str("--profile-file", parse_path)?;
    let profile_performance_json =
        pargs.opt_value_from_os_str("--profile-performance-json", parse_path)?;

    let options = SyncOptions {
        interactive: pargs.contains(["-i", "--interactive"]),
        yes: pargs.contains(["-y", "--yes"]),
        dry_run: pargs.contains(["-n", "--dry-run"]),
        batch: pargs.contains(["-b", "--batch"]),
        force: pargs.contains(["-f", "--force"]),
        verbose: pargs.contains(["-v", "--verbose"]),
        debug_info: pargs.contains("--debug-info"),
        profile_performance: pargs.contains("--profile-performance"),
        profile_performance_json,
    };

    if let Some(profile_file) = profile_file {
        let path = pargs.opt_free_from_os_str(parse_path)?;
        ensure_no_args(pargs)?;
        return Ok(Command::Sync {
            profile: ProfileSource::File(profile_file),
            path,
            options,
        });
    }

    let profile = match pargs.free_from_str::<String>() {
        Ok(profile) => profile,
        Err(_) => return Ok(Command::Help),
    };
    if profile.starts_with('-') {
        return Err(eyre!("unexpected argument: {}", profile));
    }

    let command = match profile.as_str() {
        "_snapshot" => {
            reject_sync_options(&options)?;
            Command::Snapshot {
                profile: pargs.free_from_str()?,
                statefile: pargs.opt_free_from_os_str(parse_path)?,
            }
        }
        "_inspect" => {
            reject_sync_options(&options)?;
            Command::Inspect {
                statefile: pargs.free_from_os_str(parse_path)?,
            }
        }
        "_changes" => {
            reject_sync_options(&options)?;
            Command::Changes {
                profile: pargs.free_from_str()?,
                statefile: pargs.opt_free_from_os_str(parse_path)?,
            }
        }
        "_info" => {
            reject_sync_options(&options)?;
            Command::Info {
                profile: pargs.free_from_str()?,
            }
        }
        "_walk" => {
            reject_sync_options(&options)?;
            Command::Walk {
                path: pargs.free_from_os_str(parse_path)?,
            }
        }
        "_recover" => {
            reject_sync_options(&options)?;
            Command::Recover {
                statefile: pargs.free_from_os_str(parse_path)?,
            }
        }
        _ => Command::Sync {
            profile: ProfileSource::Named(profile),
            path: pargs.opt_free_from_os_str(parse_path)?,
            options,
        },
    };
    ensure_no_args(pargs)?;
    Ok(command)
}

fn reject_sync_options(options: &SyncOptions) -> Result<()> {
    if options.interactive
        || options.yes
        || options.dry_run
        || options.batch
        || options.force
        || options.verbose
        || options.debug_info
        || options.profile_performance
        || options.profile_performance_json.is_some()
    {
        Err(eyre!("sync options are not supported for this command"))
    } else {
        Ok(())
    }
}

fn ensure_no_args(pargs: pico_args::Arguments) -> Result<()> {
    let remaining = pargs.finish();
    if remaining.is_empty() {
        Ok(())
    } else {
        Err(eyre!("unexpected argument: {}", remaining[0].to_string_lossy()))
    }
}

fn parse_path(s: &std::ffi::OsStr) -> Result<PathBuf, &'static str> {
    Ok(s.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn parse_args(args: &[&str]) -> Command {
        parse(pico_args::Arguments::from_vec(
            args.iter().map(OsString::from).collect(),
        ))
        .unwrap()
    }

    fn parse_args_error(args: &[&str]) -> String {
        parse(pico_args::Arguments::from_vec(
            args.iter().map(OsString::from).collect(),
        ))
        .unwrap_err()
        .to_string()
    }

    #[test]
    fn parses_global_commands() {
        assert_eq!(parse_args(&["--help"]), Command::Help);
        assert_eq!(parse_args(&["--version"]), Command::Version);
        assert_eq!(parse_args(&["--license"]), Command::License);
        assert_eq!(parse_args(&["--server"]), Command::Server);
    }

    #[test]
    fn parses_sync_command_with_options() {
        assert_eq!(
            parse_args(&[
                "--interactive",
                "-y",
                "-n",
                "-b",
                "-f",
                "-v",
                "work",
                "docs",
            ]),
            Command::Sync {
                profile: ProfileSource::Named("work".to_string()),
                path: Some(PathBuf::from("docs")),
                options: SyncOptions {
                    interactive: true,
                    yes: true,
                    dry_run: true,
                    batch: true,
                    force: true,
                    verbose: true,
                    debug_info: false,
                    profile_performance: false,
                    profile_performance_json: None,
                },
            }
        );
    }

    #[test]
    fn parses_sync_command_with_debug_info() {
        assert_eq!(
            parse_args(&["--debug-info", "work"]),
            Command::Sync {
                profile: ProfileSource::Named("work".to_string()),
                path: None,
                options: SyncOptions {
                    interactive: false,
                    yes: false,
                    dry_run: false,
                    batch: false,
                    force: false,
                    verbose: false,
                    debug_info: true,
                    profile_performance: false,
                    profile_performance_json: None,
                },
            }
        );
    }

    #[test]
    fn parses_sync_command_with_performance_profile() {
        assert_eq!(
            parse_args(&[
                "--profile-performance",
                "--profile-performance-json",
                "profile.json",
                "work",
            ]),
            Command::Sync {
                profile: ProfileSource::Named("work".to_string()),
                path: None,
                options: SyncOptions {
                    interactive: false,
                    yes: false,
                    dry_run: false,
                    batch: false,
                    force: false,
                    verbose: false,
                    debug_info: false,
                    profile_performance: true,
                    profile_performance_json: Some(PathBuf::from("profile.json")),
                },
            }
        );
    }

    #[test]
    fn parses_sync_command_with_profile_file() {
        assert_eq!(
            parse_args(&["--profile-file", "profile.prf", "docs"]),
            Command::Sync {
                profile: ProfileSource::File(PathBuf::from("profile.prf")),
                path: Some(PathBuf::from("docs")),
                options: SyncOptions {
                    interactive: false,
                    yes: false,
                    dry_run: false,
                    batch: false,
                    force: false,
                    verbose: false,
                    debug_info: false,
                    profile_performance: false,
                    profile_performance_json: None,
                },
            }
        );
    }

    #[test]
    fn parses_hidden_commands() {
        assert_eq!(
            parse_args(&["_snapshot", "work", "state.bin"]),
            Command::Snapshot {
                profile: "work".to_string(),
                statefile: Some(PathBuf::from("state.bin")),
            }
        );
        assert_eq!(
            parse_args(&["_inspect", "state.bin"]),
            Command::Inspect {
                statefile: PathBuf::from("state.bin"),
            }
        );
        assert_eq!(
            parse_args(&["_changes", "work", "state.bin"]),
            Command::Changes {
                profile: "work".to_string(),
                statefile: Some(PathBuf::from("state.bin")),
            }
        );
        assert_eq!(
            parse_args(&["_info", "work"]),
            Command::Info {
                profile: "work".to_string(),
            }
        );
        assert_eq!(
            parse_args(&["_walk", "docs"]),
            Command::Walk {
                path: PathBuf::from("docs"),
            }
        );
        assert_eq!(
            parse_args(&["_recover", "state.bin"]),
            Command::Recover {
                statefile: PathBuf::from("state.bin"),
            }
        );
    }

    #[test]
    fn rejects_unknown_flags_and_extra_arguments() {
        assert!(parse_args_error(&["--dryrun", "work"]).contains("unexpected argument"));
        assert!(parse_args_error(&["work", "path1", "path2"]).contains("unexpected argument"));
        assert!(parse_args_error(&["--profile-file", "profile.prf", "path1", "path2"])
            .contains("unexpected argument"));
        assert!(parse_args_error(&["--help", "work"]).contains("unexpected argument"));
        assert!(parse_args_error(&["--dry-run", "_inspect", "state.bin"])
            .contains("sync options"));
    }
}
