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
- `duet --dry-run <profile> [path]`: scan both sides, report actions and apply
  blockers, and exit without applying filesystem changes or saving state.
- `duet recover <profile-or-statefile>`: print any unfinished local
  apply-attempt marker and optionally clear it after manual inspection.
- `duet recover --remote <profile>`: inspect or clear a remote-side marker using
  the profile's remote endpoint and selected remote state id.
- `duet --server`: run the RPC server used by another Duet process.
- `duet --version`, `--license`, `--help`: informational commands.

Hidden maintenance commands:

- `_snapshot <profile> [statefile]`: scan the local side and save a snapshot.
- `_inspect <statefile>`: print entries from a snapshot.
- `_changes <profile> [statefile]`: print local changes against a snapshot.
- `_info <profile>`: print the profile file location.
- `_walk <path>`: print paths discovered by the scanner.
- `_recover <profile-or-statefile>`: hidden alias for `recover`.

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

All snapshot writes use centralized atomic helpers in `state.rs`. Current V2
snapshots have a magic/version envelope. The decoder also accepts the exact
headerless V1 field order used by older releases. Strong-digest peers write V2;
when either peer lacks strong-digest support, both sides deliberately use the
legacy RPC methods and write headerless V1 snapshots.

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
  dual content-hash computation for changed regular files.

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
- legacy Adler-32 checksum and optional BLAKE2b-256 content digest for regular files

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

Location rules are canonicalized before local scanning and the remote scan RPC.
Equivalent relative paths are collapsed to their last source rule, while the
most-specific matching path controls descendants. This also makes `.` and the
bare root path equivalent without changing the serialized `Location` shape.

The scanner:

1. Prefixes canonical, sorted, unique location rules with the absolute base path.
2. Converts ignore globs to regexes.
3. Walks the base directory while honoring include/exclude rules.
4. Skips ignored entries, special files, and filesystem boundary crossings.
5. Reads symlink targets as metadata instead of following symlinks.
6. Sends reported entries through the channel as `DirEntryWithMeta`.

`state::scan_entries()` directly owns and polls the scanner future while receiving
entries from the channel. Cancellation drops the scan instead of detaching it;
failures discard partial entries, and successful results are sorted by path.

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

For added and modified regular files, `old_and_changes()` computes legacy
Adler-32 and BLAKE2b-256 in one streaming pass. Metadata/inode identity remains
the cheap scan comparison; cross-side content equality and strong apply
verification use BLAKE2b-256. Adler is trusted only in negotiated legacy mode.
Hashing uses ordered buffered futures with at most eight files active, so errors
and resulting changes retain path order.

When a requested scope contains legacy or hybrid entries without digests, both
strong-capable peers hash their complete current manifests in that scope once.
The old metadata change streams preserve which side changed, while actions are
built from the current strong entries. Divergent content with no metadata change
becomes a synthetic conflict. The in-memory baseline is replaced only for the
requested scope before apply; abort and dry-run do not save it, and a successful
run writes V2 while preserving out-of-scope hybrid entries.

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
- `--dry-run`: print actions, run non-mutating local/remote preflight checks,
  and exit without applying changes or saving state.

Resolution converts a `Conflict` into a directed action:

- update local side -> apply the remote state locally
- update remote side -> apply the local state remotely

Unresolved conflicts are filtered out before the transfer/apply phase when
`--force` is used.

## RPC Boundary

The RPC API is declared in `src/rpc.rs` as the `DuetServer` trait using
`essrpc`. The trait is append-only. Existing methods use explicit
`LegacyEntry`/`LegacyChange`/`LegacyAction` mirror types so their V1 encodings do
not change as internal types evolve. V2 variants of every method carrying
changes or actions are appended and selected by the
`content-digest-blake2b256-v1` capability.

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
stream performance, file byte chunks, remote state id selection, and
BLAKE2b-256 content digests.
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

Regular file output in both apply paths uses a lazy, side-local `StagingArea`.
The first output creates one mode-0700 `.duet-stage-*` directory in that output's
destination parent and records the phase path once in the recovery marker. Any
temporary permission change needed for stage creation is restored immediately;
the phase retains only the parent and stage descriptors and their identities.
Each `TempOutput` uses a unique mode-0600 component inside the shared directory,
then flushes, verifies the expected BLAKE2b-256 digest (or Adler only in legacy
mode), and applies final metadata while the output remains hidden. Completed
outputs collect into bounded pre-publication batches. Each batch syncs output
file handles concurrently with a bounded blocking worker set; no output in the
batch is published unless every file sync succeeds. Successful batches publish
descriptor-relatively in original action order, revalidating the target, stage,
and destination-parent identities immediately before each publication. Added
files use no-clobber linking and replacements use descriptor-relative rename.
Immediately after each successful `renameat`/`linkat`, Duet appends that output's
committed-step/action recovery records and updates phase-local reconstructed
state before post-publication checks, cleanup, parent-mode restoration, or the
next output. These completion records are best effort across the unavoidable
syscall-to-userspace instruction boundary: if execution continues they are
appended immediately, while a signal or process loss in that boundary still
leaves the initial durable apply marker authoritative. Successful publication
also records the destination parent's path, device, and inode.
Pending outputs retain only their private file handle and the expected parent
identity; they do not retain a destination-parent descriptor or widen its mode.
The saved parent identity is checked before publication-time widening and again
against the opened descriptor.

Batches flush before adding an output that would exceed the count or aggregate
content-byte limit, when either limit is reached, before a later conflicting
non-file operation, and at phase end. Defaults are 256 files, 64 MiB, and one
sync worker per available CPU capped at 64. Hidden benchmark overrides are
`DUET_SYNC_OUTPUT_BATCH_FILES`, `DUET_SYNC_OUTPUT_BATCH_BYTES`, and
`DUET_SYNC_OUTPUT_SYNC_WORKERS`; hard caps are 512 files, 1 GiB, and 64 workers,
with every setting clamped to at least one. The effective file cap is reduced
further when `RLIMIT_NOFILE` minus 64 reserved descriptors is lower. A single
file may exceed the byte limit, but it is isolated from other pending outputs.

After all outputs have been published or removed, the apply phase syncs the shared
source stage directory once and removes it descriptor-relatively. A short retained-
descriptor guard permits removal when final parent metadata is restrictive and is
restored before barriers continue. The phase then verifies and syncs the recorded
destination parents, including the stage parent after stage removal. The existing
completion barrier skips those paths while syncing unsynced ancestors, direct-
mutation parents, and the sync base before state save. Metadata/removal-only phases
never create a staging directory. On apply failure, best-effort cleanup performs no
additional directory barrier and the durable recovery marker remains authoritative.
Earlier complete batches may remain published if a later batch fails, but no
snapshot is advanced and the marker records only publications completed in action
order.
Any streamed apply error permanently poisons that `DetailApplier`; later frames,
byte chunks, and finish requests fail closed rather than returning partial state.
`WritableDirGuard` can temporarily add owner write permission to an already-synced
read-only destination directory during one publication and restore the original
mode before the next publication. Metadata updates use Unix permission bits and
symlink-aware file times.
Permission bootstrap is descriptor-bound: Linux/Android retain the expected
inode with `O_PATH`, verify it, chmod that descriptor, then open and compare the
publication descriptor. Apple documents `O_EVTONLY`, but making it independent
of read permission requires a private entitlement unavailable to normal Duet
processes, so Duet fails closed for an already mode-000 destination parent there
rather than chmod a pathname. Newly added directories can still finish as mode
000 on Apple because child publication precedes the directory metadata pass.

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
- each apply side syncs one bounded batch of private output file handles through
  scoped blocking workers, then publishes the successful batch serially
- local and remote snapshot saves run concurrently

Blocking filesystem work that can take time, such as signature generation,
detail generation, apply operations, and local state save, is moved to
`tokio::task::spawn_blocking()` from the orchestrator.

The scanner uses a flat global `VecDeque`/`FuturesUnordered` scheduler with at
most 64 one-directory scans active. It streams entries through a bounded `mpsc`
channel; the owning collector polls that scanner future directly so scan errors
cannot silently turn into partial snapshots. File hashing runs each whole-file
Adler-32/BLAKE2b-256 pass in a blocking worker, using up to one worker per
available CPU and at most 64 workers by default. Workers complete out of order
so a slow file does not block new work; results and errors are indexed and
committed in deterministic path order. `DUET_HASH_WORKERS` can override the
worker count up to 64 for storage-specific tuning.
Each worker reads in 1 MiB chunks, validates that the no-follow file handle still
matches the scan, and checks that its identity remains stable through hashing.
`DUET_HASH_BUFFER_BYTES` can override the per-worker read size up to 4 MiB, for
a maximum aggregate hash-buffer allocation of 256 MiB.
Output sync worker panics are caught and returned as indexed batch errors, so the
whole file-sync gate finishes deterministically without publishing that batch.

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
