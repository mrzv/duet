# Architecture

Duet is a Rust command-line synchronizer for two directory trees. It keeps a
serialized snapshot of the last known shared state, scans both sides for
changes since that state, turns those changes into per-path actions, optionally
resolves conflicts, transfers only the data needed for selected actions, applies
the changes on both sides, and finally writes updated snapshots.

The binary is implemented as a single crate. There is no library target; module
boundaries are internal to the CLI.

## High-Level Flow

The default `duet <profile> [path]` flow is coordinated by `sync()` in
`src/main.rs`.

```text
profile file
    |
    v
parse profile and normalize optional restricted path
    |
    v
scan local side and ask remote server to scan remote side
    |
    v
compare current scans with saved snapshots
    |
    v
merge local and remote changes into actions
    |
    v
resolve conflicts or abort
    |
    v
exchange signatures and content/delta details
    |
    v
apply changes locally and remotely
    |
    v
atomically save updated snapshots
```

The local process launches a second `duet --server` process for the other side.
That server can run either as a local child process or over SSH. The two
processes communicate over stdin/stdout using `essrpc` with bincode transport.

## Entry Points

`src/main.rs` owns argument parsing and top-level command dispatch.

User-facing commands:

- `duet <profile> [path]`: synchronize a profile, optionally restricted to a
  path under the local base.
- `duet --server`: run the RPC server used by another Duet process.
- `duet --version`, `--license`, `--help`: informational commands.

Hidden maintenance commands:

- `_snapshot <profile> [statefile]`: scan the local side and save a snapshot.
- `_inspect <statefile>`: print entries from a snapshot.
- `_changes <profile> [statefile]`: print local changes against a snapshot.
- `_info <profile>`: print the profile file location.
- `_walk <path>`: print paths discovered by the scanner.

## Profiles And State

Profiles live under `~/.config/duet/<name>.prf` and are parsed by
`src/profile.rs`.

A profile contains:

- local base path
- remote endpoint
- include/exclude location rules
- optional ignore glob patterns under `[ignore]`

The remote endpoint is parsed by `parse_remote()` in `src/main.rs` and supports
two forms:

- `<duet-command> <remote-base>` for a local child server
- `ssh <server> <duet-command> <remote-base>` for an SSH server

State files are bincode-serialized `Vec<scan::DirEntryWithMeta>` snapshots:

- local snapshot: `~/.config/duet/<profile>.snp`
- remote snapshot directory: `~/.config/duet/remotes/`
- remote snapshot file: `~/.config/duet/remotes/<local-id>`

`local_id()` derives the remote snapshot key from the machine id and profile
name. This lets the remote side keep separate remembered states for different
clients and profiles.

Snapshot writes use `atomicwrites` so a failed write does not leave a partially
written state file.

## Module Map

```text
src/main.rs
  CLI, sync orchestration, RPC trait/server implementation, conflict UI

src/profile.rs
  Profile file locations, state file locations, profile parser

src/scan/mod.rs
  Async filesystem scanner and DirEntryWithMeta model

src/scan/location.rs
  Include/exclude location rules

src/scan/change.rs
  Change model and old-vs-current diff iterator

src/actions.rs
  Per-path action model, conflict/identical classification, display helpers

src/sync.rs
  Signature collection, detailed content/delta creation, filesystem mutation

src/rustsync.rs
  Embedded rsync-like signature, delta, and restore implementation

src/io_wrappers.rs
  AsyncRead/AsyncWrite adapters for local and SSH child process pipes

src/utils.rs
  Sorted iterator merge helper used by change and action construction

build.rs
  Generates build metadata consumed by --version
```

## Data Model

`scan::DirEntryWithMeta` is the core snapshot record. It stores the path relative
to the synchronization base plus metadata needed to detect and reproduce state:

- size
- modification time
- inode
- mode
- symlink target
- directory flag
- checksum for changed regular files

Entries are ordered only by relative path. This ordering is important because
change detection and action construction are implemented as sorted merges.

`scan::Change` represents one side's difference from its saved snapshot:

- `Added(new_entry)`
- `Removed(old_entry)`
- `Modified(old_entry, new_entry)`

`actions::Action` merges the local and remote change streams for the same path:

- `Remote(change)`: a local-only change that should be applied to the remote
  side
- `Local(change)`: a remote-only change that should be applied to the local side
- `Identical(local, remote)`: both sides changed to equivalent state
- `Conflict(local, remote)`: both sides changed differently
- `ResolvedLocal(...)`: conflict resolved by updating the local side
- `ResolvedRemote(...)`: conflict resolved by updating the remote side

The `Local` and `Remote` names describe where an action is applied, not where
the change originated. This is why `actions::reverse()` is sent to the server:
what is local from one process's point of view is remote from the other.

## Scanning

Scanning is asynchronous and implemented in `src/scan/mod.rs`.

`scan::scan()` receives:

- synchronization base path
- optional restricted path under the base
- include/exclude locations
- ignore globs
- a Tokio `mpsc::Sender` for discovered entries

The scanner:

1. Prefixes location rules with the absolute base path and sorts them.
2. Converts ignore globs to regexes.
3. Walks the base directory while honoring include/exclude rules.
4. Skips ignored entries, special files, and filesystem boundary crossings.
5. Sends reported entries through the channel as `DirEntryWithMeta`.

`scan_entries()` in `src/main.rs` collects the channel output, displays progress,
sorts the entries, and returns them.

Restricted synchronization is handled at scan time. A path is scanned only when
it is under the restriction or is an ancestor of the restriction, allowing Duet
to avoid walking unrelated parts of large trees.

## Change Detection

`old_and_changes()` in `src/main.rs` does two operations concurrently:

- scans the current restricted tree
- loads the saved snapshot, if one exists

It filters old snapshot entries to the restricted path and then calls
`scan::changes()`, which merges old and current sorted entries:

- old only -> removed
- current only -> added
- both paths present but metadata differs -> modified
- both paths equivalent -> no change

For added and modified regular files, `old_and_changes()` computes an Adler-32
checksum. The checksum is used to decide whether two sides changed to identical
content even when local mtimes differ.

## Conflict Resolution

After local and remote changes are available, `sync()` merges them with
`utils::match_sorted()` and `Action::create()`.

Conflict handling depends on flags:

- `--batch`: print actions and abort if conflicts exist.
- `--force`: in batch mode, apply non-conflicting actions and skip unresolved
  conflicts.
- `--interactive`: use a paged terminal UI for conflict navigation and
  resolution.
- default mode: ask about conflicts sequentially, then confirm before applying.
- `--yes`: proceed automatically only when there are no unresolved conflicts.
- `--dry-run`: print actions without applying anything.

Resolution converts a `Conflict` into a directed action:

- update local side -> apply the remote state locally
- update remote side -> apply the local state remotely

## RPC Boundary

The RPC API is declared in `src/main.rs` as `DuetServer` using `essrpc`.

The client calls:

- `set_base(base)`: configure the server's synchronization root.
- `changes(path, locations, ignore, remote_id)`: scan server side and return
  changes against the remembered remote snapshot.
- `set_actions(actions)`: store the server-side action plan.
- `get_signatures()`: return signatures for files that will be patched on this
  side.
- `get_detailed_changes(signatures)`: return file contents or rsync-like deltas
  needed by the other side.
- `apply_detailed_changes(details)`: mutate the server filesystem and update
  the server snapshot in memory.
- `save_state()`: atomically persist the server snapshot.

`io_wrappers.rs` hides the difference between local child pipes and SSH child
pipes so the same bincode RPC client can talk to either transport.

## Transfer And Apply

The content exchange is implemented by `src/sync.rs`.

For modified regular files, the destination side first creates signatures of its
old file content with `get_signatures()`. The source side receives those
signatures and runs `get_detailed_changes()`:

- added files are sent as full `Contents(Vec<u8>)`
- file-to-file modifications are sent as `Diff(Delta)` when content changed
- metadata-only changes require no content detail

`src/rustsync.rs` provides the rsync-like algorithm:

- `signature()` builds a block index using rolling Adler-32 and Blake2b hashes.
- `compare()` compares a new file with an old-file signature and emits a delta.
- `restore_seek()` reconstructs the new file from the old file plus delta.

`apply_detailed_changes()` mutates the filesystem and updates the in-memory
snapshot. It handles files, directories, symlinks, removals, replacements,
metadata updates, and directory cleanup in a second reverse-order pass so child
entries are processed before parent directories.

Regular file writes use `atomicwrites`. Metadata updates use Unix permissions
and symlink-aware file times.

## Concurrency Model

Duet uses Tokio for orchestration and asynchronous filesystem scanning.

Important concurrent phases:

- local scan/state load and remote scan run concurrently
- local and remote signatures are collected concurrently
- local and remote detailed changes are created concurrently
- local and remote apply phases run concurrently
- snapshot saves run concurrently

Blocking filesystem work that can take time, such as signature generation and
apply operations, is moved to `tokio::task::spawn_blocking()`.

The scanner uses an `mpsc` channel to stream entries from the walk to the
collector and a semaphore to bound concurrent directory reads.

## Platform Assumptions

The implementation is Unix-oriented:

- it uses Unix metadata extensions such as inode, mode, device id, and mtime
- it creates Unix symlinks
- it skips block devices, character devices, FIFOs, and sockets
- it avoids crossing filesystem device boundaries during scans

SSH support depends on the `openssh` crate and assumes passwordless
authentication.

## Failure Boundaries

The main synchronization flow only persists snapshots after both sides have
applied their changes. If a failure occurs before state save, a later run should
rescan and compare against the previous remembered state.

The apply phase performs real filesystem mutations on both sides concurrently.
File content writes and snapshot writes are atomic, but larger multi-entry
synchronizations are not transactional as a whole.
