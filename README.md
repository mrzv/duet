# Duet

Bi-directional synchronization, similar to [unison](https://www.cis.upenn.edu/~bcpierce/unison/).
Maintains the last known state of two directories, identifies and synchronizes
changes from that state.

## Usage

```
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

```

## Profiles

Profiles are defined in `~/.config/duet/my_profile.prf` and have the following structure:
```
~
ssh my_server duet ~

+Path1
+Path2
+Path3
-Path3/Path4
-Path3/Path5
+Path6

[ignore]
glob1*
glob2*

[prune]
__pycache__
target
```
The first two lines specify the directories to synchronize. Either both are
local, or the second one can have the form `ssh server-name path/to/duet
directory-to-synchronize`. After a blank line, there is a list of
inclusion-exclusion of paths under `directory-to-synchronize` (by default
nothing is included). Remote commands and base paths are split on whitespace;
paths containing spaces are not supported in remote profile entries. An optional
`[ignore]` section specifies glob patterns to ignore. Ignore globs match entry
basenames, not full relative paths, so `*.tmp` matches `dir/file.tmp` but
`dir/*.tmp` does not.

Ignored paths are not synchronized or tracked. They are also not deleted by
default if they physically block removal of a synced parent directory. Use
`--prune-ignored` only for disposable ignored content, such as generated caches,
when those ignored children should be deleted to allow the parent removal.

Use `[prune]` for generated, disposable basename globs that should be ignored and
automatically deleted when they are the only reason a synced parent directory
cannot be removed. Excluded paths (`-path`) are never pruned automatically.
Run `duet preflight <profile> [path]` to inspect blockers before applying a sync.

Subsequently, `duet my_profile` will synchronize the two directories.

## Metadata And Permissions

Duet synchronizes regular file contents, directory structure, symlink targets,
Unix mode bits, and modification times.

Duet does not synchronize file ownership, groups, ACLs, extended attributes, or
platform-specific permission models. Symlink permissions are ignored; the symlink
target is synchronized instead. When applying mode metadata, Duet applies only
Unix permission and special bits, not file-type bits.

Permission failures are treated as sync errors. Duet fails fast rather than
silently skipping unreadable or unwritable paths, because skipping a path can be
mistaken for a deletion or a legitimate update. Fix the reported permission
problem and rerun the sync.

## Recovery

If Duet stops after applying filesystem changes but before saving state, it
leaves an apply recovery marker next to the affected state file and blocks the
next sync. Run `duet recover <profile>` or `duet recover <statefile>` to inspect
the local marker, including the side, phase, affected paths, staged temporary
files, and committed operations. Run `duet recover --remote <profile>` to inspect
the remote-side marker for a named profile. After you have inspected both sides
and reconciled any partial changes, add `--clear` to remove the marker and allow
syncs to resume. Use `--yes` with `--clear` only for non-interactive cleanup after
that manual inspection.

## Caveat

Duet uses [openssh](https://docs.rs/openssh/) crate, which only supports
password-less authentication over SSH.

## Comparison to Unison

Advantages of Unison:
- much more mature and battle-tested
- supports Windows
- provides GUI

Advantages of Duet:
- **restricted synchronization**
- interactive TUI

Restricted synchronization is perhaps the biggest advantage of Duet. Briefly,
it's possible to restrict the directory scan to a specific path. Because the
the scan typically dominates the running time, this can speed up the
synchronization by two orders of magnitude, making this a major boost for
certain workflows. It is possible to achieve something similar in Unison by
creating several profiles that share the same state, but in practice it's much
more convenient to not have to set these up for every project one wants to
synchronize on demand.

The restricted path can be either absolute, or relative. In the former case,
the base is automatically stripped. In the latter case, if the path starts with
`.` or `..`, then it's relative to the current directory; otherwise it's
relative to the base directory.

For example,
```
duet my_profile ~/Path1/...

duet my_profile .
```
