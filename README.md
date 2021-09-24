# Duet

Bi-directional synchronization, similar to [unison](https://www.cis.upenn.edu/~bcpierce/unison/).
Maintains the last known state of two directories, identifies and synchronizes
changes from that state.

## Usage

```
USAGE:
    duet [FLAGS] <profile> [path]

FLAGS:
    -i, --interactive   interactive conflict resolution
    -y, --yes           assume yes (i.e., synchronize, if there are no conflicts)
    -b, --batch         run as a batch (abort on conflict)
    -f, --force         in batch mode, apply what's possible, even if there are conflicts
    -v, --verbose       verbose output
    -n, --dry-run       don't apply changes

        --version       prints version information
        --license       prints license information (including dependencies)
    -h, --help          prints help information

ARGS:
    <profile>    profile to synchronize
    <path>       path to synchronize

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
```
The first two lines specify the directories to synchronize. Either both are
local, or the second one can have the form `ssh server-name path/to/duet
directory-to-synchronize`. After a blank line, there is a list of
inclusion-exclusion of paths under `directory-to-synchronize` (by default
nothing is included). An optional `[ignore]` section specifies glob patterns to
ignore.

Subsequently, `duet my_profile` will synchronize the two directories.

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
