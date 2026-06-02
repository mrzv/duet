use std::path::PathBuf;

use color_eyre::eyre::Result;

#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub interactive: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub batch: bool,
    pub force: bool,
    pub verbose: bool,
}

#[derive(Debug)]
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
        profile: String,
        path: Option<PathBuf>,
        options: SyncOptions,
    },
}

pub fn parse_from_env() -> Result<Command> {
    let mut pargs = pico_args::Arguments::from_env();

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

    let options = SyncOptions {
        interactive: pargs.contains(["-i", "--interactive"]),
        yes: pargs.contains(["-y", "--yes"]),
        dry_run: pargs.contains(["-n", "--dry-run"]),
        batch: pargs.contains(["-b", "--batch"]),
        force: pargs.contains(["-f", "--force"]),
        verbose: pargs.contains(["-v", "--verbose"]),
    };

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
            profile,
            path: pargs.opt_free_from_os_str(parse_path)?,
            options,
        }),
    }
}

fn parse_path(s: &std::ffi::OsStr) -> Result<PathBuf, &'static str> {
    Ok(s.into())
}
