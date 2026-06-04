# Permission Handling Status

This document tracks the remaining permission-handling work in Duet. Older
audit documents have been consolidated here so there is a single source of
truth.

Duet has two separate permission concerns:

- It synchronizes Unix mode bits for files and directories as metadata.
- It depends on OS permissions to scan, read, write, remove, chmod, set mtimes,
  launch the server, and save state.

The highest-risk bugs around permission failures being mistaken for deletions
have been fixed. The remaining work is mostly transactionality, recovery policy,
and fuller diagnostics for setup and platform-specific permission models.

## Fixed And Tested

These issues are covered by active tests in `tests/permission_failures.rs`.

- Scanner failures now propagate instead of being accepted as partial scans.
  Unreadable local or remote subdirectories no longer look like deletions.
- Scanner filesystem operations now return path-aware errors for directory
  reads, directory-entry reads, metadata reads, and symlink target reads.
- Checksum, signature, and detail-collection read failures now abort before
  remote mutation in the tested permission-denied cases.
- State loading uses `try_exists` and reports state open/read/decode errors
  instead of treating inaccessible state as missing.
- Local state save errors are no longer ignored, and remote state save errors
  retain path/error context.
- Apply-side filesystem panics for remove/create/symlink/chmod/mtime paths were
  converted to `Result` errors with path context.
- Local and remote apply both run a preflight check before applying detailed
  changes. Preflight now checks source reads, destination metadata targets,
  destination parent availability/writability for common mutations, and local and
  remote state-save paths before filesystem mutation starts.
- Already-synced read-only destination directories can be temporarily made
  writable for child file updates and then restored.
- Remote RPC handlers wrap sync failures in a structured `RemoteSyncError`
  envelope with side, operation, optional path, classified error kind, and source
  message.
- Streamed apply temp files use bounded-length names, so long destination file
  names no longer create overlong temporary path components.
- Setup/orchestration paths that previously used permission-triggerable
  `expect`/`unwrap` calls now return contextual errors.
- SSH/session setup now adds targeted hints for common private-key and SSH config
  permission failures, and server launch errors include command/log context.
- Applied Unix modes are masked to permission/special bits before `chmod`, so
  file-type bits from `symlink_metadata` are not passed back to the OS.
- `README.md` documents the user-facing metadata model: file contents,
  directories, symlink targets, Unix mode bits, and mtimes are synchronized;
  ownership, ACLs, xattrs, platform-specific permission models, and symlink
  permissions are not.
- Local and remote apply now create a side-local recovery marker before applying
  changes and remove it after state save succeeds. A later sync refuses to run if
  the marker remains, with recovery instructions instead of silently continuing
  from an unknown partial-apply state.

## Remaining Work

### 1. Apply Needs A Prepare/Commit Protocol

Current preflight catches common permission failures before mutation, and apply
now records coarse recovery markers while filesystem changes and state saves are
in progress. This prevents a later run from silently continuing after an
interrupted apply. Sync is still not a true transaction: local and remote apply
can still mutate files before a later non-preflighted error, crash, or race is
detected.

Target design:

- `prepare`: both sides validate the selected actions again, create a per-sync
  apply attempt id, and stage all content writes into side-local temporary files.
- `prepared`: both sides report staged paths, metadata operations, removals, and
  any operation that cannot be safely staged.
- `commit`: both sides perform the shortest possible rename/remove/chmod/utime
  window. File content replacement should be rename-based wherever the platform
  allows it.
- `finish`: state is saved only after both sides report commit success.
- `recover`: if a process dies after `prepare` or during `commit`, the next run
  detects the attempt id, reports which phase may have partially completed, and
  either cleans abandoned staged files or asks the user to inspect committed
  paths before continuing.

Remaining work:

- Add RPC methods for `prepare`, `commit`, `finish`, and `recover`, advertised by
  a protocol capability.
- Move streamed and non-streamed apply through the same staged apply engine.
- Stage or explicitly classify every operation that cannot be staged, especially
  directory removals, type replacements, chmod, and utime.
- Persist richer phase/path attempt metadata to resume or provide deterministic
  recovery instructions after a crash. The current marker only identifies that a
  side may have applied changes or failed during state save.
- Keep state saving after both sides have committed successfully.

### 2. Preflight And Apply Recovery Are Still Best-Effort

Current preflight catches common permission failures before mutation, including
source reads, destination parent/metadata checks, and local/remote state-save
paths. It is still a snapshot: the filesystem can change after preflight, and
some capabilities cannot be proven without attempting the operation.

When a post-preflight apply or state-save error occurs, Duet now reports recovery
advice explaining that filesystem changes may have been partially applied and
state may not have been saved. If a process exits before cleanup, the recovery
marker blocks the next sync until the user inspects the state and removes the
marker. This is still not a resumable apply protocol.

Remaining work:

- Improve chmod and utime preflight where ownership/platform support makes that
  possible.
- Expand preflight coverage for less common replacement/remove combinations as
  they are found.
- Add tests for representative races between preflight and apply.
- Replace generic marker guidance with phase/path-specific recovery advice once
  apply attempts record staged paths and committed operations.

### 3. Remote Errors Are Only Partly Structured

Remote sync errors now use a structured `RemoteSyncError` envelope inside
`RPCErrorKind::Other`. This is a practical improvement over generic strings, but
it is not a complete end-to-end error model yet.

The client parses this envelope for concise remote error rendering. The original
RPC payload remains line-oriented and machine-readable, but source chains are
still carried as formatted debug text.

Remaining work:

- Promote the envelope into a shared sync error type instead of an RPC-only
  wrapper.
- Preserve structured source chains without relying on formatted debug strings.
- Extend client-side parsing/rendering to all sync/setup errors, not only remote
  RPC errors.
- Extend structured setup errors for server launch, SSH, profile, and log/state
  setup paths that fail before the normal RPC server is running.

### 4. Permission Model Is Still Unix Mode Bits Only

Duet records and syncs mode bits, but it does not sync ownership, ACLs, xattrs,
or platform-specific permission models. Symlink permissions are intentionally
ignored. When applying metadata, Duet masks the recorded mode to Unix
permission/special bits (`0o7777`) before calling `chmod`.

Remaining work:

- Decide whether uid/gid support is in scope. If it is, gate it behind explicit
  capability checks and clear behavior for non-root users.

### 5. Server, Profile, And SSH Setup Diagnostics Are Still Evolving

Most permission-triggerable setup `expect`/`unwrap` paths in profile loading,
remote command expansion, RPC setup, and orchestration have been converted to
contextual errors. SSH setup adds hints for common key/config permission
failures, and server launch errors include the command and server log path where
available. Some setup failures still happen before normal sync error handling is
established and can produce weak diagnostics.

Known examples:

- Server startup failures before RPC initialization still depend on child
  process stderr/log context.

Remaining work:

- Preserve remote server startup failures with enough context to identify remote
  log, config, state, temp, and base-path permission problems before RPC starts.

### 6. Restricted Sync Can Still Enumerate Readable Ancestors Broadly

Restricted sync now benefits from propagated scanner errors, so inaccessible
ancestors fail safely. It has not been optimized to walk only the direct path
components needed for the restricted subtree.

Remaining work:

- Optimize restricted scans to avoid broad ancestor enumeration where possible.
- Keep path-aware failure messages for any ancestor that must still be read or
  statted.

### 7. Permission-Denied Skip Is Out Of Scope For Default Sync

The current policy is fail-fast. That is safer than silently skipping unreadable
paths, but it means one inaccessible path aborts the sync.

Decision: keep fail-fast as the default and do not add implicit skip behavior.
Skipping permission-denied paths is only acceptable as a future explicit opt-in
mode, because both sides must know exactly which paths were skipped so they are
not interpreted as deletions or legitimate updates.

Remaining work:

- If an opt-in skip mode is implemented, communicate skipped permission-denied
  paths to both sides so they are not interpreted as removals or legitimate
  updates.
- Make any skipped-path behavior visible in the action plan and final summary.

## Current Priority Order

1. Complete the transactional or resumable apply protocol described above.
2. Add tests and phase/path-specific recovery advice for races and
   post-preflight failures.
3. Promote `RemoteSyncError` into an end-to-end structured error model with
   client-side rendering.
