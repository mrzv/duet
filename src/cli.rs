use std::path::PathBuf;

use color_eyre::eyre::{eyre, Result};

use crate::profile::ProfileSource;
use crate::sync::{StagingPolicy, StagingReserve};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOptions {
    pub interactive: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub batch: bool,
    pub force: bool,
    pub verbose: bool,
    pub debug_info: bool,
    pub prune_ignored: bool,
    pub excludes: Vec<PathBuf>,
    pub profile_performance: bool,
    pub profile_performance_json: Option<PathBuf>,
    pub staging_policy: StagingPolicy,
    pub staging_policy_explicit: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version {
        verbose: bool,
    },
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
        target: PathBuf,
        remote: bool,
        clear: bool,
        yes: bool,
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
        ensure_help_args(pargs)?;
        return Ok(Command::Help);
    }

    if pargs.contains("--version") {
        let verbose = pargs.contains(["-v", "--verbose"]);
        ensure_no_args(pargs)?;
        return Ok(Command::Version { verbose });
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
    let staging_limit: Option<StagingLimit> = pargs.opt_value_from_str("--staging-limit")?;
    let staging_reserve = pargs.opt_value_from_str("--staging-reserve")?;
    let excludes = pargs.values_from_os_str("--exclude", parse_path)?;

    let staging_policy_explicit = staging_limit.is_some() || staging_reserve.is_some();
    let options = SyncOptions {
        interactive: pargs.contains(["-i", "--interactive"]),
        yes: pargs.contains(["-y", "--yes"]),
        dry_run: pargs.contains(["-n", "--dry-run"]),
        batch: pargs.contains(["-b", "--batch"]),
        force: pargs.contains(["-f", "--force"]),
        verbose: pargs.contains(["-v", "--verbose"]),
        debug_info: pargs.contains("--debug-info"),
        prune_ignored: pargs.contains("--prune-ignored"),
        excludes,
        profile_performance: pargs.contains("--profile-performance"),
        profile_performance_json,
        staging_policy: StagingPolicy {
            limit_bytes: staging_limit.map(|limit| limit.0),
            reserve: staging_reserve.unwrap_or(StagingReserve::BasisPoints(500)),
        },
        staging_policy_explicit,
    };

    if let Some(profile_file) = profile_file {
        let path = pargs.opt_free_from_os_str(parse_path)?;
        let path_is_recover = path
            .as_deref()
            .map(|path| {
                path == std::path::Path::new("recover") || path == std::path::Path::new("_recover")
            })
            .unwrap_or(false);
        if path_is_recover {
            return Err(eyre!("recover is a subcommand, not a profile-file path"));
        }
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
        "recover" | "_recover" => {
            let clear = pargs.contains("--clear");
            let remote = pargs.contains("--remote");
            reject_recover_options(&options, clear)?;
            Command::Recover {
                target: pargs.free_from_os_str(parse_path)?,
                remote,
                clear,
                yes: options.yes,
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
        || options.prune_ignored
        || !options.excludes.is_empty()
        || options.profile_performance
        || options.profile_performance_json.is_some()
        || options.staging_policy_explicit
    {
        Err(eyre!("sync options are not supported for this command"))
    } else {
        Ok(())
    }
}

fn reject_recover_options(options: &SyncOptions, clear: bool) -> Result<()> {
    if options.interactive
        || options.dry_run
        || options.batch
        || options.force
        || options.verbose
        || options.debug_info
        || options.prune_ignored
        || !options.excludes.is_empty()
        || options.profile_performance
        || options.profile_performance_json.is_some()
        || options.staging_policy_explicit
        || (options.yes && !clear)
    {
        Err(eyre!(
            "only --clear and --yes are supported for recover; --yes requires --clear"
        ))
    } else {
        Ok(())
    }
}

fn ensure_no_args(pargs: pico_args::Arguments) -> Result<()> {
    let remaining = pargs.finish();
    if remaining.is_empty() {
        Ok(())
    } else {
        Err(eyre!(
            "unexpected argument: {}",
            remaining[0].to_string_lossy()
        ))
    }
}

fn ensure_help_args(pargs: pico_args::Arguments) -> Result<()> {
    let remaining = pargs.finish();
    if remaining.is_empty() {
        return Ok(());
    }

    if remaining.len() == 1 {
        let arg = remaining[0].to_string_lossy();
        if matches!(arg.as_ref(), "recover" | "_recover") {
            return Ok(());
        }
    }

    Err(eyre!(
        "unexpected argument: {}",
        remaining[0].to_string_lossy()
    ))
}

fn parse_path(s: &std::ffi::OsStr) -> Result<PathBuf, &'static str> {
    Ok(s.into())
}

fn parse_size(value: &str) -> Result<u64, String> {
    if value.is_empty() || value.trim() != value {
        return Err("size must be a nonempty value without surrounding whitespace".to_string());
    }
    byte_unit::Byte::parse_str(value, true)
        .map_err(|error| format!("invalid size: {error}"))?
        .as_u64_checked()
        .ok_or_else(|| "size exceeds the maximum supported value".to_string())
}

impl std::str::FromStr for StagingReserve {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let Some(percent) = value.strip_suffix('%') else {
            return parse_size(value).map(StagingReserve::Bytes);
        };
        if percent.is_empty() || percent.trim() != percent {
            return Err("percentage must be a number followed by %".to_string());
        }
        let split = percent.split_once('.');
        let (whole, fraction) = split.unwrap_or((percent, ""));
        if whole.is_empty()
            || (split.is_some() && fraction.is_empty())
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.len() > 2
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err("percentage must have at most two decimal places".to_string());
        }
        let whole: u16 = whole
            .parse()
            .map_err(|_| "percentage is out of range".to_string())?;
        let fraction: u16 = match fraction.len() {
            0 => 0,
            1 => fraction.parse::<u16>().unwrap() * 10,
            _ => fraction.parse().unwrap(),
        };
        let basis_points = whole
            .checked_mul(100)
            .and_then(|value| value.checked_add(fraction))
            .ok_or_else(|| "percentage is out of range".to_string())?;
        if basis_points >= 10_000 {
            return Err("percentage must be less than 100%".to_string());
        }
        Ok(StagingReserve::BasisPoints(basis_points))
    }
}

struct StagingLimit(u64);

impl std::str::FromStr for StagingLimit {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let bytes = parse_size(value)?;
        if bytes == 0 {
            return Err("size must be greater than zero".to_string());
        }
        Ok(Self(bytes))
    }
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

    fn default_options() -> SyncOptions {
        SyncOptions {
            interactive: false,
            yes: false,
            dry_run: false,
            batch: false,
            force: false,
            verbose: false,
            debug_info: false,
            prune_ignored: false,
            excludes: Vec::new(),
            profile_performance: false,
            profile_performance_json: None,
            staging_policy: StagingPolicy::default(),
            staging_policy_explicit: false,
        }
    }

    #[test]
    fn parses_global_commands() {
        assert_eq!(parse_args(&["--help"]), Command::Help);
        assert_eq!(
            parse_args(&["--version"]),
            Command::Version { verbose: false }
        );
        assert_eq!(
            parse_args(&["--version", "--verbose"]),
            Command::Version { verbose: true }
        );
        assert_eq!(
            parse_args(&["-v", "--version"]),
            Command::Version { verbose: true }
        );
        assert_eq!(parse_args(&["--license"]), Command::License);
        assert_eq!(parse_args(&["--server"]), Command::Server);
        assert_eq!(parse_args(&["--help", "recover"]), Command::Help);
        assert_eq!(parse_args(&["recover", "-h"]), Command::Help);
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
                "--prune-ignored",
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
                    prune_ignored: true,
                    excludes: Vec::new(),
                    profile_performance: false,
                    profile_performance_json: None,
                    staging_policy: StagingPolicy::default(),
                    staging_policy_explicit: false,
                },
            }
        );
    }

    #[test]
    fn parses_repeated_sync_excludes_without_normalizing_them() {
        let Command::Sync { options, path, .. } = parse_args(&[
            "--exclude",
            "cache",
            "--exclude",
            "../shared/tmp",
            "work",
            "docs",
        ]) else {
            panic!("expected sync command");
        };

        assert_eq!(path, Some(PathBuf::from("docs")));
        assert_eq!(
            options.excludes,
            vec![PathBuf::from("cache"), PathBuf::from("../shared/tmp")]
        );
    }

    #[test]
    fn rejects_exclude_for_non_sync_commands() {
        assert!(parse_args_error(&["--exclude", "cache", "_info", "work"])
            .contains("sync options are not supported"));
        assert!(parse_args_error(&["--exclude", "cache", "recover", "work"])
            .contains("only --clear and --yes"));
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
                    prune_ignored: false,
                    excludes: Vec::new(),
                    profile_performance: false,
                    profile_performance_json: None,
                    staging_policy: StagingPolicy::default(),
                    staging_policy_explicit: false,
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
                    prune_ignored: false,
                    excludes: Vec::new(),
                    profile_performance: true,
                    profile_performance_json: Some(PathBuf::from("profile.json")),
                    staging_policy: StagingPolicy::default(),
                    staging_policy_explicit: false,
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
                    prune_ignored: false,
                    excludes: Vec::new(),
                    profile_performance: false,
                    profile_performance_json: None,
                    staging_policy: StagingPolicy::default(),
                    staging_policy_explicit: false,
                },
            }
        );
        assert_eq!(
            parse_args(&["--profile-file", "profile.prf", "preflight"]),
            Command::Sync {
                profile: ProfileSource::File(PathBuf::from("profile.prf")),
                path: Some(PathBuf::from("preflight")),
                options: default_options(),
            }
        );
    }

    #[test]
    fn parses_staging_policy() {
        let command = parse_args(&[
            "--staging-limit",
            "1.5GiB",
            "--staging-reserve",
            "7.25%",
            "work",
        ]);
        let Command::Sync { options, .. } = command else {
            panic!("expected sync command");
        };
        assert_eq!(
            options.staging_policy,
            StagingPolicy {
                limit_bytes: Some(1_610_612_736),
                reserve: StagingReserve::BasisPoints(725),
            }
        );

        let Command::Sync { options, .. } = parse_args(&["--staging-reserve", "250 MB", "work"])
        else {
            panic!("expected sync command");
        };
        assert_eq!(
            options.staging_policy,
            StagingPolicy {
                limit_bytes: None,
                reserve: StagingReserve::Bytes(250_000_000),
            }
        );
        let Command::Sync { options, .. } = parse_args(&["work"]) else {
            panic!("expected sync command");
        };
        assert_eq!(options.staging_policy, StagingPolicy::default());
        assert!(!options.staging_policy_explicit);
    }

    #[test]
    fn rejects_invalid_staging_policy() {
        for value in ["0", "bogus", "auto", "unlimited", "18446744073709551616B"] {
            let error = parse_args_error(&["--staging-limit", value, "work"]);
            assert!(!error.is_empty(), "{} should be rejected", value);
        }
        for value in ["", "5.%", ".5%", "1.234%", "NaN%", "inf%", "100%", "101%"] {
            let error = parse_args_error(&["--staging-reserve", value, "work"]);
            assert!(!error.is_empty(), "{} should be rejected", value);
        }
        assert!(
            parse_args_error(&["--staging-limit", "1GiB", "--staging-limit", "2GiB", "work",])
                .contains("staging-limit")
        );
        assert!(
            parse_args_error(&["--staging-reserve", "5%", "--staging-reserve", "6%", "work",])
                .contains("staging-reserve")
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
                target: PathBuf::from("state.bin"),
                remote: false,
                clear: false,
                yes: false,
            }
        );
        assert_eq!(
            parse_args(&["recover", "state.bin"]),
            Command::Recover {
                target: PathBuf::from("state.bin"),
                remote: false,
                clear: false,
                yes: false,
            }
        );
        assert_eq!(
            parse_args(&["recover", "--clear", "--yes", "state.bin"]),
            Command::Recover {
                target: PathBuf::from("state.bin"),
                remote: false,
                clear: true,
                yes: true,
            }
        );
        assert_eq!(
            parse_args(&["recover", "--remote", "cole"]),
            Command::Recover {
                target: PathBuf::from("cole"),
                remote: true,
                clear: false,
                yes: false,
            }
        );
    }

    #[test]
    fn rejects_unknown_flags_and_extra_arguments() {
        assert!(parse_args_error(&["--dryrun", "work"]).contains("unexpected argument"));
        assert!(parse_args_error(&["work", "path1", "path2"]).contains("unexpected argument"));
        assert!(
            parse_args_error(&["--profile-file", "profile.prf", "path1", "path2"])
                .contains("unexpected argument")
        );
        assert!(parse_args_error(&["--help", "work"]).contains("unexpected argument"));
        assert!(parse_args_error(&["--dry-run", "_inspect", "state.bin"]).contains("sync options"));
        assert!(
            parse_args_error(&["--staging-limit", "1GiB", "_inspect", "state.bin"])
                .contains("sync options")
        );
        assert!(
            parse_args_error(&["--staging-reserve", "5GiB", "recover", "state.bin"])
                .contains("only --clear")
        );
        assert!(
            parse_args_error(&["--staging-reserve", "5%", "_inspect", "state.bin"])
                .contains("sync options")
        );
        assert!(
            parse_args_error(&["--staging-reserve", "5.00%", "recover", "state.bin"])
                .contains("only --clear")
        );
        assert!(
            parse_args_error(&["--yes", "recover", "state.bin"]).contains("--yes requires --clear")
        );
        assert!(
            parse_args_error(&["recover", "--profile-file", "profile.prf"])
                .contains("recover is a subcommand")
        );
        assert!(
            parse_args_error(&["recover", "--profile-file", "profile.prf", "state.bin"])
                .contains("recover is a subcommand")
        );
        assert!(
            parse_args_error(&["_recover", "--profile-file", "profile.prf"])
                .contains("recover is a subcommand")
        );
        assert!(
            parse_args_error(&["--profile-file", "profile.prf", "_recover"])
                .contains("recover is a subcommand")
        );
        assert!(parse_args_error(&["preflight", "work", "path1", "path2"])
            .contains("unexpected argument"));
    }
}
