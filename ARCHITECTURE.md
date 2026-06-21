# Architecture

Duet is a Rust command-line synchronizer for two directory trees. It keeps a
serialized snapshot of the last known shared state, scans both sides for changes
since that state, turns those changes into per-path actions, optionally resolves
conflicts, transfers only the data needed for selected actions, applies the
changes on both sides, and finally writes updated snapshots.

The binary is implemented as a single crate. There is no library target; module
boundaries are internal to the CLI.

## High-Level Flow

The default `duet <profile> [path]` and `duet --profile-file <file> [path]`
flows are dispatched by `src/main.rs` and coordinated by `orchestrator::sync()`
in `src/orchestrator.rs`.

```text
CLI arguments
    |
    v
load profile and normalize optional restricted path
    |
    v
start local scan/state load and remote scan/state load
    |
    v
merge local and remote changes into per-path actions
    |
    v
resolve conflicts, dry-run, or abort
    |
    v
preflight local and remote apply targets
    |
    v
exchange signatures and content/delta detail frames
    |
    v
apply changes locally and remotely
    |
    v
atomically save updated local and remote snapshots
```

The local process launches a second `duet --server` process for the other side.
That server can run either as a local child process or over SSH. The two
processes communicate over stdin/stdout using `essrpc` with bincode transport.

## Entry Points

`src/main.rs` installs error reporting, parses the command line through
`cli::parse_from_env()`, and dispatches to command-specific modules.

User-facing commands:

- `duet <profile> [path]`: synchronize a named profile, optionally restricted to
  a path under the local base.
- `duet --profile-file <file> [path]`: synchronize a profile file and keep state
  next to that file.
- `duet preflight <profile> [path]`: scan both sides, resolve actions, and
  report apply blockers without applying filesystem changes or saving state.
- `duet recover <statefile>`: print any unfinished apply-attempt marker for a
  state file and optionally clear it after manual inspection.
- `duet --server`: run the RPC server used by another Duet process.
- `duet --version`, `--license`, `--help`: informational commands.

Hidden maintenance commands:

- `_snapshot <profile> [statefile]`: scan the local side and save a snapshot.
- `_inspect <statefile>`: print entries from a snapshot.
- `_changes <profile> [statefile]`: print local changes against a snapshot.
- `_info <profile>`: print the profile file location.
- `_walk <path>`: print paths discovered by the scanner.
- `_recover <statefile>`: hidden alias for `recover`.

`src/commands.rs` implements the informational and maintenance commands. Normal
synchronization is implemented in `src/orchestrator.rs`.

## Profiles And State

Profiles are parsed by `src/profile.rs`. A profile contains:

- local base path
- remote endpoint
- include/exclude location rules
- optional ignore glob patterns under `[ignore]`
- optional disposable prune glob patterns under `[prune]`

Duet supports two profile sources:

- named profiles from `~/.config/duet/<name>.prf`
- explicit profile files passed with `--profile-file <file>`

Named profile state lives under `~/.config/duet`:

- local snapshot: `~/.config/duet/<profile>.snp`
- remote snapshot directory: `~/.config/duet/remotes/`
- remote snapshot file: `~/.config/duet/remotes/<local-id>`
- default server log: `~/.config/duet/remote.log`

Profile-file state lives next to the profile file:

- local snapshot: same path with extension `.snp`
- remote snapshot directory: same path with extension `.remotes`
- server log: same path with extension `.remote.log`

`orchestrator::local_id()` derives the remote snapshot key from the machine id
and profile identity. This lets the remote side keep separate remembered states
for different clients and profiles.

Snapshot writes use `atomicwrites` in the sync path so a failed state write does
not leave a partially written snapshot. Maintenance snapshot writes use
`state::save_entries()`.

Remote endpoints are parsed by `remote::parse_remote()` and support two forms:

- `<duet-command> <remote-base>` for a local child server
- `ssh <server> <duet-command> <remote-base>` for an SSH server

When `<duet-command>` is omitted, Duet uses `duet`.

## Module Map

```text
src/main.rs
  Crate module declarations, color_eyre setup, top-level command dispatch,
  and path expansion helper.

src/cli.rs
  pico_args parsing, SyncOptions, and Command enum.

src/commands.rs
  Help/version/license output and hidden maintenance commands.

src/orchestrator.rs
  Main sync coordinator: profile loading, SSH/session setup, remote server
  launch, capability negotiation, change/action flow, conflict resolution,
  streamed or non-streamed transfer/apply, and state saves.

src/profile.rs
  Profile sources, profile parser, named/profile-file state locations,
  remote state directory, and server log location.

src/remote.rs
  Remote endpoint parsing, local/SSH server launch, and RPC client transport
  construction.

src/rpc.rs
  essrpc wire protocol, server implementation, protocol version,
  capabilities, remote state handling, and streamed detail/apply state.

src/state.rs
  Snapshot load/save helpers, scan collection, old/current comparison, and
  checksum computation for changed regular files.

src/scan/mod.rs
  Async filesystem scanner and DirEntryWithMeta snapshot record.

src/scan/location.rs
  Include/exclude location rules.

src/scan/change.rs
  Change model and old-vs-current diff iterator.

src/actions.rs
  Per-path action model, conflict/identical classification, display helpers,
  and local/remote action reversal.

src/resolution.rs
  Conflict display, prompts, and interactive resolution UI.

src/sync.rs
  Apply preflight, signature collection, detailed content/delta creation,
  streaming detail producer/applier, and filesystem mutation.

src/rustsync.rs
  Embedded rsync-like signature, delta, and restore implementation.

src/io_wrappers.rs
  AsyncRead/AsyncWrite adapters for local and SSH child process pipes.

src/utils.rs
  Sorted iterator merge helper used by change and action construction.

build.rs
  Generates build metadata consumed by --version and RPC server_info.
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

Entries are ordered by relative path. This ordering is important because change
detection and action construction are implemented as sorted merges.

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

The `Local` and `Remote` names describe where an action is applied, not where the
change originated. This is why `actions::reverse()` is sent to the server: what
is local from one process's point of view is remote from the other.

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
5. Reads symlink targets as metadata instead of following symlinks.
6. Sends reported entries through the channel as `DirEntryWithMeta`.

`state::scan_entries()` owns the scanner task, displays progress while receiving
entries from the channel, awaits the scanner task, propagates scan failures, and
sorts the final entries.

Restricted synchronization is handled at scan time. A path is scanned only when
it is under the restriction or is an ancestor of the restriction, allowing Duet to
avoid walking unrelated parts of large trees.

## Change Detection

`state::old_and_changes()` runs two operations concurrently:

- scans the current restricted tree
- loads the saved snapshot, if one exists

State loading uses `try_exists()` and path-aware read/decode errors so permission
failures are not mistaken for missing state.

After both inputs are available, `old_and_changes()` filters old snapshot entries
to the restricted path and calls `scan::changes()`, which merges old and current
sorted entries:

- old only -> removed
- current only -> added
- both paths present but metadata differs -> modified
- both paths equivalent -> no change

For added and modified regular files, `old_and_changes()` computes an Adler-32
checksum. The checksum is used to decide whether two sides changed to identical
content even when mtimes differ.

## Conflict Resolution

After local and remote changes are available, `orchestrator::sync()` merges them
with `utils::match_sorted()` and `Action::create()`.

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

Unresolved conflicts are filtered out before the transfer/apply phase when
`--force` is used.

## RPC Boundary

The RPC API is declared in `src/rpc.rs` as the `DuetServer` trait using
`essrpc`. The trait is the wire protocol: methods are appended for compatibility
and existing method order/signatures should not be changed.

Core RPC methods:

- `set_base(base)`: configure the server's synchronization root.
- `server_info()`: return protocol version, Duet version, and capabilities.
- `set_remote_state_dir(path)`: override the server's remote state directory for
  profile-file syncs.
- `changes(path, locations, ignore, remote_id)`: scan server side and return
  changes against the remembered remote snapshot.
- `set_actions(actions)`: store the server-side action plan.
- `get_signatures()`: return signatures for files that will be patched on this
  side.
- `get_detailed_changes(signatures)`: return non-streamed file contents or
  rsync-like deltas needed by the other side.
- `apply_detailed_changes(details)`: mutate the server filesystem using the
  non-streamed detail vector and update the server snapshot in memory.
- `save_state()`: atomically persist the server snapshot.
- `prepare_apply_attempt()`: create the remote recovery marker before apply.
- `prepare_apply_attempt_with_id(attempt_id)`: create a remote recovery marker
  with a client-provided correlation id.
- `negotiate_sync_tuning(request)`: agree on streamed detail chunking and
  signature-window tuning.
- `stream_performance()`: return server-side streamed transfer/apply counters
  for performance profiling.
- `select_remote_state_id(stable_id, legacy_id)`: choose the stable remote state
  id for new state, or an existing legacy id when a legacy state file is already
  present.

Streaming RPC methods:

- `begin_detail_stream(signatures, max_chunk_bytes)`
- `next_detail_chunk(stream_id)`
- `end_detail_stream(stream_id)`
- `begin_apply_stream()`
- `apply_detail_chunk(stream_id, frame)`
- `finish_apply_stream(stream_id)`
- `next_detail_chunks(stream_id, max_frames, max_payload_bytes)`
- `apply_detail_chunks(stream_id, frames)`
- `apply_file_byte_chunk(stream_id, chunk)`

`ServerInfo` currently advertises protocol version `2` and capabilities for
profile-file remote state directories, streamed details, batched streamed detail
frames, apply-attempt preparation and ids, creatable added parents, sync tuning,
stream performance, file byte chunks, and remote state id selection.
`orchestrator::show_debug_info()` prints client, server, and agreed capabilities
when `--debug-info` is used.

`rpc::server()` uses `DUET_SERVER_LOG` (`rpc::SERVER_LOG_ENV`) when provided or
falls back to `~/.config/duet/remote.log`, initializes logging, and serves
`DuetServerSyncRPCServer` over bincode stdin/stdout transport.

## Transfer And Apply

The content exchange and filesystem mutation code is implemented by
`src/sync.rs`.

For modified regular files, the destination side first creates signatures of its
old file content with `get_signatures()`. The source side receives those
signatures and either sends full contents or an rsync-like delta:

- added files are sent as full contents
- file-to-file modifications are sent as a delta when content changed
- metadata-only changes require no content detail

`src/rustsync.rs` provides the rsync-like algorithm:

- `signature()` builds a block index using rolling Adler-32 and Blake2b hashes.
- `compare()` and `compare_stream()` compare new file content with an old-file
  signature and emit a delta.
- `restore_seek()` reconstructs the new file from the old file plus delta.

Duet has two detail/apply paths:

- streamed path: `DetailProducer` emits `DetailFrame` values containing file or
  diff payload chunks; `DetailApplier` consumes frames and mutates the
  destination incrementally.
- non-streamed fallback: `get_detailed_changes()` returns a vector of
  `ChangeDetails`, and `apply_detailed_changes()` applies that vector.

The streamed path is preferred when both sides advertise batched streaming and
`sync::can_stream_details()` says the selected actions are supported. The
orchestrator interleaves the two directions: it reads remote detail batches and
feeds the local applier, then produces local detail batches and sends them to the
remote applier.

When both sides advertise file-byte chunks, local-to-remote streamed apply routes
large `FileBytes` payloads through `apply_file_byte_chunk()`. Smaller file-byte
frames stay in normal `apply_detail_chunks()` batches so SSH transfers with many
small or medium files do not degrade into one apply RPC per file. The current
cutoff is 8 MiB per `FileBytes` payload: payloads below that size are batched;
payloads at or above it use the dedicated file-byte RPC.

`sync::preflight_apply()` checks selected destination write targets before
mutation. The RPC server also runs preflight before non-streamed apply and before
starting a streamed apply.

`DetailApplier` and `apply_detailed_changes()` handle files, directories,
symlinks, removals, replacements, metadata updates, and directory cleanup in a
second reverse-order pass so child entries are processed before parent
directories.

Regular streamed file output uses `TempOutput`: data is written to a bounded
`.duet-part-<pid>-<counter>` temporary basename in the destination directory and
renamed into place on finish. Before rename, staged file output is flushed and
verified against the expected file entry so mismatched content is rejected before
the synchronized snapshot is recorded. `WritableDirGuard` can temporarily add
owner write permission to an already-synced read-only destination directory and
restore the original mode afterward. Metadata updates use Unix permission bits
and symlink-aware file times.

## Concurrency Model

Duet uses Tokio for orchestration and asynchronous filesystem scanning.

Important concurrent phases:

- local scan/state load and remote scan/state load run concurrently
- local state load and local scan run concurrently inside `state::old_and_changes()`
- local and remote signatures are collected concurrently
- non-streamed local and remote detailed changes are created concurrently
- non-streamed local and remote apply phases run concurrently
- streamed apply interleaves remote-to-local and local-to-remote batches in one
  loop
- local and remote snapshot saves run concurrently

Blocking filesystem work that can take time, such as signature generation,
detail generation, apply operations, and local state save, is moved to
`tokio::task::spawn_blocking()` from the orchestrator.

The scanner uses an `mpsc` channel to stream entries from the walk to the
collector and a semaphore to bound concurrent directory reads. The collector
awaits the scanner task so scan errors cannot silently turn into partial
snapshots.

## Platform Assumptions

The implementation is Unix-oriented:

- it uses Unix metadata extensions such as inode, mode, device id, and mtime
- it syncs mode bits and mtimes, but not uid/gid, ACLs, or xattrs
- it creates Unix symlinks
- it skips block devices, character devices, FIFOs, and sockets
- it avoids crossing filesystem device boundaries during scans

SSH support depends on the `openssh` crate and assumes passwordless
authentication with strict known-hosts checking.

## Failure Boundaries

The main synchronization flow only persists snapshots after both sides have
applied their changes. If a failure occurs before state save, a later run should
rescan and compare against the previous remembered state.

Permission handling is fail-fast. Scanner errors are propagated through
`state::scan_entries()`, state file existence checks use `try_exists()`, local
and remote state save errors are reported, and apply operations return
path-aware errors instead of panicking for expected filesystem failures.

The apply phase performs real filesystem mutations on both sides. File content
writes and snapshot writes use temporary/atomic output where practical, and
preflight catches known unsafe destination cases, but larger multi-entry
synchronizations are not transactional as a whole.
