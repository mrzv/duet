# Duet

Bi-directional synchronization, similar to [unison](https://www.cis.upenn.edu/~bcpierce/unison/).
Maintains the last known state of two directories, identifies and synchronizes
changes from that state.

## Usage

```
USAGE:
    duet [FLAGS] <profile> [path]
    duet [FLAGS] --profile-file <file> [path]
    duet recover [--clear] [--yes] [--remote] <profile-or-statefile>

FLAGS:
    -i, --interactive   interactive conflict resolution
    -y, --yes           assume yes (i.e., synchronize, if there are no conflicts)
    -b, --batch         run as a batch (abort on conflict)
    -f, --force         in batch mode, apply what's possible, even if there are conflicts
    -v, --verbose       verbose output
    -n, --dry-run       check what sync would do without applying changes
        --debug-info    print protocol and capability negotiation details
        --exclude <path>
                         exclude a subtree from this sync; may be repeated
        --prune-ignored delete ignored files/directories that block removing a synced parent
        --staging-limit <size>
                         target maximum reconstructed bytes per staging wave
        --staging-reserve <size|percent>
                         preserve free space on each staging filesystem; defaults to 5%
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

DRY RUN:
    --dry-run checks what sync would do, reports directory removal blockers on
    both sides, reports the staging wave and capacity plan when supported by the
    peer, validates preflight checks, and exits without applying changes or
    saving state.

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
paths containing spaces are not supported in remote profile entries.

The most-specific matching path rule wins. For equivalent paths, the later rule
in the profile wins. `+.` and a bare `+` are equivalent ways to include the
entire synchronization root.

Subsequently, `duet my_profile` will synchronize the two directories.

## Ignore and Prune

An optional `[ignore]` section specifies glob patterns to ignore. Ignore globs
match entry basenames, not full relative paths, so `*.tmp` matches
`dir/file.tmp` but `dir/*.tmp` does not.

Ignored paths are not synchronized or tracked. They are also not deleted by
default if they physically block removal of a synced parent directory. Use
`--prune-ignored` only for disposable ignored content, such as generated caches,
when those ignored children should be deleted to allow the parent removal.

Use `[prune]` for generated, disposable basename globs that should be ignored and
automatically deleted when they are the only reason a synced parent directory
cannot be removed. Excluded paths (`-path`) are never pruned automatically.
Run `duet --dry-run <profile> [path]` to inspect blockers before applying a sync.

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

## Staging Capacity

Supported peers prepare changes in dependency-safe bilateral waves. Each wave is
fully prepared and validated on both sides, committed, and saved as a canonical
checkpoint before the next wave starts. This bounds private staging without
making the complete invocation atomic: if a later wave is interrupted or fails,
earlier checkpoints remain synchronized and a normal rerun discovers the
remaining changes.

`--staging-limit` sets the target reconstructed bytes in one wave on each host.
One regular file larger than that target is isolated in its own wave, but it may
never consume the configured reserve. `--staging-reserve` accepts a byte size or
percentage of total filesystem capacity and defaults to 5% independently on
each host. Duet checks capacity before every wave, monitors materialized writes,
and refreshes free space after output durability barriers and immediately before
commit. A no-space or quota error during preparation aborts the current wave
without changing synchronized targets. A concurrent writer can race the final
check, and a later commit or state-save failure can leave a recovery marker and a
partially applied wave.

For modified regular files, Duet uses APFS clones on macOS or reflinks on Linux
when supported, applies the delta to the private clone, and publishes it
atomically. This can make physical staging proportional to changed blocks rather
than full logical file size. Additions and clone-unavailable modifications still
require materialized staging. Explicit staging controls require a peer that can
enforce them; default settings retain legacy fallback for older peers.

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

`--exclude <path>` may be repeated to hard-exclude subtrees from one
synchronization. Excludes use the same path rules as the restricted positional
path. They are normalized, deduplicated, and collapsed; an exclude outside the
current restriction is a no-op, while excluding the restriction itself or one
of its ancestors selects nothing. Excluded subtrees are not scanned or treated
as removed, profile includes cannot re-enter them, and their saved baseline is
preserved so a later run without the exclusion discovers accumulated changes.
Absolute excludes must remain under the local synchronization base.

For example,
```
duet my_profile ~/Path1/...

duet my_profile .

duet --exclude build --exclude ./private/cache my_profile src
```

## Recovery

For peers supporting staged apply, the first Ctrl+C before a wave's commit safely
removes that wave's private staging and exits with code 6 without changing its
targets. If an intermediate wave commit has started, Duet finishes and saves that
checkpoint, then exits with code 6 before the next wave. An interrupt after the
final commit fence is deferred through finalization and a successful completed
sync exits normally. Press Ctrl+C a second time to force an immediate code-6 exit;
this can leave recovery markers or staging behind.

If Duet stops after applying filesystem changes but before saving state, it
leaves an apply recovery marker next to the affected state file and blocks the
next sync. Run `duet recover <profile>` or `duet recover <statefile>` to inspect
the local marker, including the side, phase, affected paths, staged temporary
files, and committed operations. Run `duet recover --remote <profile>` to inspect
the remote-side marker for a named profile. After you have inspected both sides
and reconciled any partial changes, add `--clear` to remove the marker and allow
syncs to resume. For a V2 `preparing` or `prepared` marker, `--clear` first
identity-checks and removes only Duet-owned private staging; commit-or-later
markers still require manual inspection. Use `--yes` with `--clear` only for
non-interactive cleanup after that inspection.
