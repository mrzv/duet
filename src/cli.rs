use std::path::PathBuf;

use color_eyre::eyre::Result;

use crate::profile::ProfileSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOptions {
    pub interactive: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub batch: bool,
    pub force: bool,
    pub verbose: bool,
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
        return Ok(Command::Help);
    }

    if pargs.contains("--version") {
        return Ok(Command::Version);
    }

    if pargs.contains("--license") {
        return Ok(Command::License);
    }

    if pargs.contains("--server") {
        return Ok(Command::Server);
    }

    let profile_file = pargs.opt_value_from_os_str("--profile-file", parse_path)?;

    let options = SyncOptions {
        interactive: pargs.contains(["-i", "--interactive"]),
        yes: pargs.contains(["-y", "--yes"]),
        dry_run: pargs.contains(["-n", "--dry-run"]),
        batch: pargs.contains(["-b", "--batch"]),
        force: pargs.contains(["-f", "--force"]),
        verbose: pargs.contains(["-v", "--verbose"]),
    };

    if let Some(profile_file) = profile_file {
        return Ok(Command::Sync {
            profile: ProfileSource::File(profile_file),
            path: pargs.opt_free_from_os_str(parse_path)?,
            options,
        });
    }

    let profile = match pargs.free_from_str::<String>() {
        Ok(profile) => profile,
        Err(_) => return Ok(Command::Help),
    };

    match profile.as_str() {
        "_snapshot" => Ok(Command::Snapshot {
            profile: pargs.free_from_str()?,
            statefile: pargs.opt_free_from_os_str(parse_path)?,
        }),
        "_inspect" => Ok(Command::Inspect {
            statefile: pargs.free_from_os_str(parse_path)?,
        }),
        "_changes" => Ok(Command::Changes {
            profile: pargs.free_from_str()?,
            statefile: pargs.opt_free_from_os_str(parse_path)?,
        }),
        "_info" => Ok(Command::Info {
            profile: pargs.free_from_str()?,
        }),
        "_walk" => Ok(Command::Walk {
            path: pargs.free_from_os_str(parse_path)?,
        }),
        _ => Ok(Command::Sync {
            profile: ProfileSource::Named(profile),
            path: pargs.opt_free_from_os_str(parse_path)?,
            options,
        }),
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
    }
}
